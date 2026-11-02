//! Detached API executions. Launch data lives only in the private socket and
//! worker's memory; durable snapshots contain redacted output and identities.
use super::model::{ConversationId, SubmissionReceipt};
use super::protocol::{read_frame, write_frame, WorkspaceCommand, WorkspaceReply};
use crate::ai::{AiCancellationToken, AiClient, Provider, Role, Turn};
use serde::{Deserialize, Serialize};
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub(super) const API_WORKER_FLAG: &str = "--jterm-workspace-api-worker";
pub(super) const API_PREVIEW_BYTES: usize = 64 * 1024;
pub(super) const API_RESULT_BYTES: usize = 256 * 1024;

#[derive(Clone, Serialize, Deserialize)]
pub struct ApiLaunch {
    pub receipt: SubmissionReceipt,
    provider: String,
    api_key: Option<String>,
    model: String,
    base_url: String,
    max_tokens: u32,
    temperature: Option<f32>,
    pub(super) redact_secrets: bool,
    system: Option<String>,
    turns: Vec<ApiTurn>,
    streaming: bool,
}

impl std::fmt::Debug for ApiLaunch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiLaunch")
            .field("conversation", &self.receipt.id)
            .field("epoch", &self.receipt.epoch)
            .field("provider", &self.provider)
            .field("credential_configured", &self.api_key.is_some())
            .field("turn_count", &self.turns.len())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct ApiTurn {
    role: Role,
    text: String,
}

impl ApiLaunch {
    pub fn new(
        receipt: SubmissionReceipt,
        client: &AiClient,
        system: Option<String>,
        history: &[Turn],
        streaming: bool,
    ) -> Result<Self, String> {
        let launch = Self {
            receipt,
            provider: client.provider.as_config_value().to_owned(),
            api_key: client.api_key.clone(),
            model: client.model.clone(),
            base_url: client.base_url.clone(),
            max_tokens: client.max_tokens,
            temperature: client.temperature,
            redact_secrets: client.redact_secrets,
            system,
            turns: history
                .iter()
                .map(|t| ApiTurn {
                    role: t.role,
                    text: t.text.clone(),
                })
                .collect(),
            streaming,
        };
        launch.validate()?;
        Ok(launch)
    }

    pub(super) fn validate(&self) -> Result<AiClient, String> {
        if self.receipt.id.0.len() > 4096
            || self.receipt.nonce.len() > 128
            || self.turns.is_empty()
            || self.turns.len() > 256
            || self
                .api_key
                .as_ref()
                .is_some_and(|key| key.len() > 64 * 1024)
            || self
                .system
                .as_ref()
                .is_some_and(|text| text.len() > 1024 * 1024)
            || self.turns.iter().map(|t| t.text.len()).sum::<usize>() > 4 * 1024 * 1024
        {
            return Err("Invalid or excessive background API launch".into());
        }
        AiClient::new(
            Provider::from_str(&self.provider).map_err(|e| e.to_string())?,
            self.api_key.clone(),
            self.model.clone(),
            self.base_url.clone(),
            self.max_tokens,
            self.temperature,
            self.redact_secrets,
        )
        .map_err(|e| e.to_string())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ApiState {
    Starting,
    Running,
    Stopping,
    Completed,
    Cancelled,
    Failed,
    Interrupted,
}
impl ApiState {
    pub fn is_live(self) -> bool {
        matches!(self, Self::Starting | Self::Running | Self::Stopping)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApiSnapshot {
    pub id: ConversationId,
    pub epoch: u64,
    pub sequence: u64,
    pub state: ApiState,
    /// Plain user text for rebuilding a view, never the provider prompt envelope.
    #[serde(default)]
    pub user_text: String,
    pub preview: String,
    pub result: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ApiControlLease {
    pub id: ConversationId,
    pub epoch: u64,
    pub generation: String,
}

/// UI ownership of an observation. Closing a view detaches; only an explicit
/// cancel asks the host to stop the provider transport.
#[derive(Debug)]
pub struct ApiObservation {
    token: ApiObservationToken,
}
#[derive(Clone, Debug)]
pub struct ApiObservationToken {
    cancellation: AiCancellationToken,
    detached: Arc<AtomicBool>,
}

#[derive(Clone, Debug)]
pub enum ApiRequest {
    Start(ApiLaunch),
    Observe {
        id: ConversationId,
        epoch: u64,
        view: String,
    },
}
impl Default for ApiObservation {
    fn default() -> Self {
        Self {
            token: ApiObservationToken {
                cancellation: AiCancellationToken::new(),
                detached: Arc::new(AtomicBool::new(false)),
            },
        }
    }
}
impl ApiObservation {
    pub fn token(&self) -> ApiObservationToken {
        self.token.clone()
    }
    pub fn cancel(&self) {
        self.token.cancellation.cancel();
    }
}
impl Drop for ApiObservation {
    fn drop(&mut self) {
        self.token.detached.store(true, Ordering::SeqCst);
    }
}

#[derive(Serialize, Deserialize)]
pub(super) enum ApiWorkerCommand {
    Cancel,
}
#[derive(Serialize, Deserialize)]
pub(super) enum ApiWorkerEvent {
    Preview(String),
    Complete(Result<String, String>),
}

/// A blocking observer for an existing frontend worker, never the UI thread.
/// Dropping an observer or closing its GUI does not cancel the host execution.
#[derive(Clone, Debug, Default)]
pub struct HostedApiClient {
    state: Option<PathBuf>,
}
struct ReleaseControl<'a> {
    client: &'a HostedApiClient,
    lease: Option<ApiControlLease>,
}
impl Drop for ReleaseControl<'_> {
    fn drop(&mut self) {
        if let Some(control) = self.lease.take() {
            let _ = self.client.request(WorkspaceCommand::DetachApi { control });
        }
    }
}
impl HostedApiClient {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn for_state_dir(state: impl AsRef<Path>) -> Self {
        Self {
            state: Some(state.as_ref().to_path_buf()),
        }
    }
    pub fn execute(
        &self,
        request: ApiRequest,
        observer: &ApiObservationToken,
        on_preview: &mut dyn FnMut(&str),
    ) -> Result<String, String> {
        match request {
            ApiRequest::Start(launch) => self.run(launch, observer, on_preview),
            ApiRequest::Observe { id, epoch, view } => {
                self.observe(id, epoch, view, observer, on_preview)
            }
        }
    }
    fn request(&self, command: WorkspaceCommand) -> Result<WorkspaceReply, String> {
        let paths = match &self.state {
            Some(path) => super::ownership::Paths::at(path.clone())?,
            None => super::ownership::Paths::discover()?,
        };
        let stream = super::client::connect(&paths, self.state.is_none(), None)?;
        super::client::command_request(stream, command)
    }
    pub fn run(
        &self,
        launch: ApiLaunch,
        observer: &ApiObservationToken,
        on_preview: &mut dyn FnMut(&str),
    ) -> Result<String, String> {
        let id = launch.receipt.id.clone();
        let epoch = launch.receipt.epoch;
        let receipt = launch.receipt.clone();
        let reply = self.request(WorkspaceCommand::StartApi {
            launch: Box::new(launch),
        }).or_else(|error| {
            match self.request(WorkspaceCommand::ResolveApi { receipt }) {
                Ok(reply @ WorkspaceReply::Api { .. }) => Ok(reply),
                Ok(_) => Err(error),
                Err(_) => Err(format!("API admission could not be confirmed: {error}. Reopen the conversation to inspect its state; it was not resubmitted.")),
            }
        })?;
        self.watch(id, epoch, reply, observer, on_preview)
    }
    /// Reattach to an existing request without resolving credentials or sending
    /// another HTTP request. A live controller permits read-only observation.
    pub fn observe(
        &self,
        id: ConversationId,
        epoch: u64,
        view: String,
        observer: &ApiObservationToken,
        on_preview: &mut dyn FnMut(&str),
    ) -> Result<String, String> {
        let reply = self
            .request(WorkspaceCommand::AttachApi {
                id: id.clone(),
                epoch,
                view,
            })
            .or_else(|_| self.request(WorkspaceCommand::InspectApi { id: id.clone() }))?;
        self.watch(id, epoch, reply, observer, on_preview)
    }
    fn watch(
        &self,
        id: ConversationId,
        epoch: u64,
        mut reply: WorkspaceReply,
        observer: &ApiObservationToken,
        on_preview: &mut dyn FnMut(&str),
    ) -> Result<String, String> {
        let mut control = ReleaseControl {
            client: self,
            lease: None,
        };
        let mut shown_sequence = None;
        let mut stopping = false;
        loop {
            let WorkspaceReply::Api {
                snapshot,
                control: assigned,
            } = reply
            else {
                return Err("The running host does not support background API execution".into());
            };
            if snapshot.id != id || snapshot.epoch != epoch {
                return Err("Background API incarnation changed; response was not adopted".into());
            }
            control.lease = assigned.or(control.lease.take());
            if observer.detached.load(Ordering::SeqCst) {
                // Cancel followed immediately by releasing the UI handle is
                // still an explicit stop; ordinary Drop is only detach.
                if let Some(control) = control.lease.take() {
                    let command = if observer.cancellation.is_cancelled() {
                        WorkspaceCommand::CancelApi { control }
                    } else {
                        WorkspaceCommand::DetachApi { control }
                    };
                    let _ = self.request(command);
                }
                return Err("Background API view detached; the host retains the request".into());
            }
            if shown_sequence != Some(snapshot.sequence) {
                on_preview(&snapshot.preview);
                shown_sequence = Some(snapshot.sequence);
            }
            match snapshot.state {
                ApiState::Completed => {
                    return snapshot
                        .result
                        .ok_or_else(|| "Completed API result is unavailable".into())
                }
                ApiState::Cancelled | ApiState::Failed | ApiState::Interrupted => {
                    return Err(snapshot
                        .error
                        .unwrap_or_else(|| "Background API execution stopped".into()));
                }
                _ => {}
            }
            if observer.cancellation.is_cancelled() && !stopping {
                let lease = control
                    .lease
                    .clone()
                    .ok_or("This view does not own background API control")?;
                self.request(WorkspaceCommand::CancelApi { control: lease })?;
                stopping = true;
            }
            // Socket errors are uncertainty, not permission to submit again.
            std::thread::sleep(Duration::from_millis(75));
            let reconnect = Instant::now();
            reply = loop {
                match self.request(WorkspaceCommand::InspectApi { id: id.clone() }) {
                    Ok(reply) => break reply,
                    Err(error) if reconnect.elapsed() >= Duration::from_secs(5) => {
                        return Err(error)
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(100)),
                }
            };
        }
    }
}

/// Internal worker entry point. Its inherited conversation lock also remains
/// open in curl descendants, so a crashed supervisor cannot admit duplicates.
pub(super) fn run_worker() -> Result<(), String> {
    use crossbeam_channel::{bounded, select};
    let stdin = std::io::stdin();
    let mut input = BufReader::new(stdin);
    let launch: ApiLaunch = read_frame(&mut input)?.ok_or("Missing background API launch")?;
    let client = launch.validate()?;
    let cancellation = AiCancellationToken::new();
    let command_token = cancellation.clone();
    std::thread::spawn(move || loop {
        match read_frame::<ApiWorkerCommand>(&mut input) {
            Ok(Some(ApiWorkerCommand::Cancel)) => command_token.cancel(),
            _ => {
                command_token.cancel();
                break;
            }
        }
    });
    let (tx, rx) = bounded(32);
    let token = cancellation.clone();
    let worker = std::thread::spawn(move || {
        let history: Vec<Turn> = launch
            .turns
            .into_iter()
            .map(|t| Turn {
                role: t.role,
                text: t.text,
            })
            .collect();
        let mut preview = String::new();
        let mut published_at = Instant::now() - Duration::from_secs(1);
        let result = if launch.streaming {
            client.send_turns_streaming_cancellable(
                launch.system.as_deref(),
                &history,
                &token,
                &mut |delta| {
                    if preview.len() < API_PREVIEW_BYTES {
                        let remaining = API_PREVIEW_BYTES - preview.len();
                        let mut end = delta.len().min(remaining);
                        while !delta.is_char_boundary(end) {
                            end -= 1;
                        }
                        preview.push_str(&delta[..end]);
                        if published_at.elapsed() >= Duration::from_millis(40) {
                            let safe = if launch.redact_secrets {
                                crate::redact::redact_secrets(&preview)
                            } else {
                                preview.clone()
                            };
                            let _ = tx.try_send(ApiWorkerEvent::Preview(safe));
                            published_at = Instant::now();
                        }
                    }
                },
            )
        } else {
            client.send_turns_blocking_cancellable(launch.system.as_deref(), &history, &token)
        }
        .map_err(|e| e.to_string())
        .and_then(|text| {
            if text.len() > API_RESULT_BYTES {
                return Err("API response exceeds the durable response limit".into());
            }
            Ok(if launch.redact_secrets {
                crate::redact::redact_secrets(&text)
            } else {
                text
            })
        });
        let _ = tx.send(ApiWorkerEvent::Complete(result));
    });
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    let mut write_error = None;
    loop {
        select! {
            recv(rx) -> event => match event {
                Ok(event) => {
                    let complete = matches!(event, ApiWorkerEvent::Complete(_));
                    if let Err(error) = write_frame(&mut output, &event) {
                        cancellation.cancel(); write_error = Some(error); break;
                    }
                    if complete { break; }
                }
                Err(_) => { write_error = Some("API worker exited without a result".into()); break; }
            },
            default(Duration::from_millis(20)) => {}
        }
    }
    cancellation.cancel();
    drop(rx);
    worker.join().map_err(|_| "API transport worker panicked")?;
    write_error.map_or(Ok(()), Err)
}
