use crate::agent_sessions::{AgentSession, SessionProvider};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Numeric IDs below this value remain owned by legacy per-window ChatStore
/// collections. Shared database rows use this prefix plus their integer key.
pub const SHARED_CHAT_ID_BASE: u64 = 1 << 48;

/// A durable source-qualified identity, never a sidebar row index.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ConversationId(pub String);

impl std::fmt::Display for ConversationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ConversationKind {
    Api,
    Cli,
    Task,
}

/// Current host knowledge. Historical provider statuses are deliberately not
/// used as evidence that a process is running or awaiting an approval.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum RuntimeState {
    #[default]
    Idle,
    Starting,
    Running,
    Attached,
    Stopping,
    Exited,
    Failed,
    Conflict,
}

impl RuntimeState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "Ready to resume",
            Self::Starting => "Starting",
            Self::Running => "Running · detached",
            Self::Attached => "Running · attached",
            Self::Stopping => "Stopping",
            Self::Exited => "Exited",
            Self::Failed => "Failed",
            Self::Conflict => "External owner",
        }
    }
    pub fn is_live(self) -> bool {
        matches!(
            self,
            Self::Starting | Self::Running | Self::Attached | Self::Stopping
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceRow {
    pub id: ConversationId,
    pub kind: ConversationKind,
    pub title: String,
    pub provider: String,
    pub cwd: String,
    pub preview: String,
    pub updated_at_ms: u64,
    pub archived: bool,
    pub pinned: bool,
    pub tags: Vec<String>,
    pub runtime: RuntimeState,
}

impl WorkspaceRow {
    pub fn from_agent(session: &AgentSession) -> Result<Self, String> {
        Ok(Self {
            id: session_id(session)?,
            kind: ConversationKind::Cli,
            title: session.title.clone(),
            provider: session.provider.label().to_owned(),
            cwd: session.cwd.to_string_lossy().into_owned(),
            preview: session.preview.clone(),
            updated_at_ms: session.updated_at_ms,
            archived: session.archived,
            pinned: false,
            tags: Vec::new(),
            runtime: RuntimeState::Idle,
        })
    }
    pub fn matches(&self, query: &str) -> bool {
        let haystack = format!(
            "{} {} {} {} {}",
            self.title,
            self.provider,
            self.cwd,
            self.preview,
            self.tags.join(" ")
        )
        .to_lowercase();
        query
            .split_whitespace()
            .all(|part| haystack.contains(&part.to_lowercase()))
    }
}

#[derive(Clone, Debug, Default)]
pub struct WorkspaceFilter {
    pub query: String,
    pub kind: Option<ConversationKind>,
    pub include_archived: bool,
    pub live_only: bool,
}

impl WorkspaceFilter {
    pub fn project(&self, rows: &[WorkspaceRow]) -> Vec<WorkspaceRow> {
        let mut result: Vec<_> = rows
            .iter()
            .filter(|row| {
                (self.include_archived || !row.archived)
                    && self.kind.is_none_or(|kind| row.kind == kind)
                    && (!self.live_only || row.runtime.is_live())
                    && row.matches(&self.query)
            })
            .cloned()
            .collect();
        result.sort_by(|a, b| {
            b.pinned
                .cmp(&a.pinned)
                .then(b.updated_at_ms.cmp(&a.updated_at_ms))
                .then(a.id.cmp(&b.id))
        });
        result
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoredChat {
    pub row: WorkspaceRow,
    pub revision: u64,
    pub draft_revision: u64,
    pub epoch: u64,
    /// A bounded, one-chat ConversationSnapshot, including its draft/context.
    /// Its numeric chat ID is a database-global ID suitable for ChatStore.
    pub snapshot_json: String,
    /// Older messages exist in the durable store; use History to page them.
    pub has_older: bool,
    pub read_position: Option<ReadPosition>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReadPosition {
    pub view: String,
    pub position: f64,
    pub updated_at_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LegacyChatMapping {
    pub legacy_id: u64,
    pub id: ConversationId,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkspaceOrganization {
    pub id: ConversationId,
    pub title: Option<String>,
    pub pinned: Option<bool>,
    pub archived: Option<bool>,
    pub tags: Option<Vec<String>>,
}

impl WorkspaceOrganization {
    pub fn new(id: ConversationId) -> Self {
        Self {
            id,
            title: None,
            pinned: None,
            archived: None,
            tags: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SubmissionReceipt {
    pub id: ConversationId,
    pub epoch: u64,
    pub nonce: String,
    pub draft_revision: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoredMessage {
    pub sequence: u64,
    pub role: String,
    pub text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RuntimeSnapshot {
    pub id: ConversationId,
    pub state: RuntimeState,
    pub pid: u32,
    pub process_start: String,
    pub epoch: String,
    pub event_sequence: u64,
    pub cols: u16,
    pub rows: u16,
    pub exit_code: Option<i32>,
    pub detail: String,
}

/// A validated direct exec descriptor. Environment overrides are restricted
/// to provider configuration locations; arbitrary commands are not accepted
/// by the public host protocol.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct ManagedLaunch {
    pub row: WorkspaceRow,
    pub native_id: String,
    pub provider: String,
    pub provider_home: PathBuf,
    pub source: PathBuf,
    pub cwd: PathBuf,
    pub cols: u16,
    pub rows: u16,
}

pub fn session_id(session: &AgentSession) -> Result<ConversationId, String> {
    let home = session
        .provider_home()
        .ok_or("Cannot identify the original provider home")?;
    source_id(
        session.provider.label(),
        &local_host_id(),
        &home,
        &session.id,
    )
}

/// Metadata identity for an application-owned engineering task. The
/// namespace must include the application and durable task-store location;
/// publishing this row does not transfer execution ownership to the host.
pub fn task_id(namespace: &str, native_id: &str) -> Result<ConversationId, String> {
    if namespace.is_empty()
        || namespace.len() > 8192
        || native_id.is_empty()
        || native_id.len() > 256
        || namespace.contains('\0')
        || native_id.contains('\0')
    {
        return Err("Invalid task source namespace".into());
    }
    Ok(ConversationId(format!(
        "jterm:task:v1:{}",
        serde_json::to_string(&(local_host_id(), namespace, native_id))
            .map_err(|e| e.to_string())?
    )))
}

pub fn source_id(
    provider: &str,
    host: &str,
    profile: &Path,
    native_id: &str,
) -> Result<ConversationId, String> {
    let profile = profile
        .canonicalize()
        .unwrap_or_else(|_| profile.to_path_buf());
    let profile = profile.to_str().ok_or("Provider home is not UTF-8")?;
    if [provider, host, profile, native_id]
        .iter()
        .any(|s| s.is_empty() || s.len() > 8192 || s.contains('\0'))
    {
        return Err("Invalid session namespace".into());
    }
    Ok(ConversationId(format!(
        "jterm:cli:v1:{}",
        serde_json::to_string(&(provider, host, profile, native_id)).map_err(|e| e.to_string())?
    )))
}

pub(super) fn descriptor(session: &AgentSession) -> Result<ManagedLaunch, String> {
    // Reuse the provider's exact-ID/cwd/source validation. Never run its
    // formatted display_command through a shell.
    session.resume_launch()?;
    Ok(ManagedLaunch {
        row: WorkspaceRow::from_agent(session)?,
        native_id: session.id.clone(),
        provider: match session.provider {
            SessionProvider::Codex => "codex",
            SessionProvider::Claude => "claude",
        }
        .into(),
        provider_home: session.provider_home().ok_or("Missing provider home")?,
        source: session.source.clone(),
        cwd: session.cwd.clone(),
        cols: 120,
        rows: 36,
    })
}

pub fn local_host_id() -> String {
    for path in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
        if let Ok(value) = std::fs::read_to_string(path) {
            let value = value.trim();
            if !value.is_empty() && value.len() <= 128 {
                return value.to_owned();
            }
        }
    }
    std::env::var("HOSTNAME").unwrap_or_else(|_| "local".into())
}

pub(super) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn namespaces_include_provider_home_host_and_native_id() {
        let a = source_id("Codex", "one", Path::new("/a"), "same").unwrap();
        for b in [
            source_id("Claude", "one", Path::new("/a"), "same"),
            source_id("Codex", "two", Path::new("/a"), "same"),
            source_id("Codex", "one", Path::new("/b"), "same"),
            source_id("Codex", "one", Path::new("/a"), "other"),
        ] {
            assert_ne!(a, b.unwrap());
        }
    }
}
