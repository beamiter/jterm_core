//! The host owns API workers and their control leases. Views only observe.
use super::api::*;
use super::model::*;
use super::ownership::{process_matches, process_start, Lease, Paths};
use super::protocol::{read_frame, write_frame, WorkspaceCommand, WorkspaceReply};
use super::store::Store;
use crate::supervised::SupervisedChild;
use crossbeam_channel::{bounded, Receiver, Sender};
use std::collections::HashMap;
use std::io::BufReader;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

type Owner = (u32, String);
pub(super) enum Disposition {
    Handled(Result<WorkspaceReply, String>),
    Other(WorkspaceCommand),
}
struct Controller {
    owner: Owner,
    view: String,
    lease: ApiControlLease,
}
struct Running {
    receipt: SubmissionReceipt,
    snapshot: ApiSnapshot,
    controller: Option<Controller>,
    cancel: Arc<AtomicBool>,
    commands: Sender<ApiWorkerCommand>,
    _lease: Arc<Lease>,
}
enum Event {
    Started(SubmissionReceipt, u32, String),
    Preview(SubmissionReceipt, String),
    Finished(SubmissionReceipt, Option<Result<String, String>>),
}
pub(super) struct ApiManager {
    paths: Paths,
    jobs: HashMap<ConversationId, Running>,
    events: Receiver<Event>,
    sender: Sender<Event>,
}
impl ApiManager {
    pub fn new(paths: Paths) -> Self {
        let (sender, events) = bounded(64);
        Self {
            paths,
            jobs: HashMap::new(),
            events,
            sender,
        }
    }
    pub fn handle(
        &mut self,
        command: WorkspaceCommand,
        owner: Owner,
        store: &mut Store,
    ) -> Disposition {
        let result = match command {
            WorkspaceCommand::StartApi { launch } => self.start(*launch, owner, store),
            WorkspaceCommand::InspectApi { id } => self.inspect(&id, store, None),
            WorkspaceCommand::ResolveApi { receipt } => {
                match store.api_matches(&receipt) {
                    Ok(true) => {
                        let control = self
                            .jobs
                            .get(&receipt.id)
                            .and_then(|j| j.controller.as_ref())
                            .filter(|c| c.owner == owner && c.view == receipt.nonce)
                            .map(|c| c.lease.clone());
                        self.inspect(&receipt.id, store, control)
                    }
                    Ok(false) => {
                        // No durable admission means no worker could have been
                        // released. Clear only this same peer's pending receipt.
                        store.resolve_unadmitted_api(&receipt, &owner)
                    }
                    Err(error) => Err(error),
                }
            }
            WorkspaceCommand::AttachApi { id, epoch, view } => {
                self.attach(&id, epoch, view, owner, store)
            }
            WorkspaceCommand::DetachApi { control } => self.detach(&control, &owner),
            WorkspaceCommand::CancelApi { control } => self.cancel(&control, &owner, store),
            WorkspaceCommand::CompleteSubmission { receipt, text } => {
                match store.api_matches(&receipt) {
                    Ok(true) => match store.api_snapshot(&receipt.id) {
                        Ok(Some(snapshot)) if snapshot.state == ApiState::Completed => store
                            .load(&receipt.id)
                            .map(|chat| WorkspaceReply::Chat { chat }),
                        Ok(_) => Err("The background host owns this API result".into()),
                        Err(e) => Err(e),
                    },
                    Ok(false) => {
                        return Disposition::Other(WorkspaceCommand::CompleteSubmission {
                            receipt,
                            text,
                        })
                    }
                    Err(e) => Err(e),
                }
            }
            WorkspaceCommand::CancelSubmission {
                receipt,
                restore_draft,
            } => {
                match store.api_matches(&receipt) {
                    Ok(true) => {
                        let cancel = (|| {
                            if let Some(job) = self.jobs.get(&receipt.id) {
                                let control = job
                                    .controller
                                    .as_ref()
                                    .filter(|c| c.owner == owner && c.view == receipt.nonce)
                                    .map(|c| c.lease.clone())
                                    .ok_or(
                                        "This view no longer controls the background API request",
                                    )?;
                                self.cancel(&control, &owner, store)?;
                            }
                            // A stop is not an acknowledgement of cleanup. The
                            // durable nonce stays active until the worker ends.
                            store
                                .load(&receipt.id)
                                .map(|chat| WorkspaceReply::Chat { chat })
                        })();
                        cancel
                    }
                    Ok(false) => {
                        return Disposition::Other(WorkspaceCommand::CancelSubmission {
                            receipt,
                            restore_draft,
                        })
                    }
                    Err(e) => Err(e),
                }
            }
            other => return Disposition::Other(other),
        };
        Disposition::Handled(result)
    }
    fn inspect(
        &self,
        id: &ConversationId,
        store: &Store,
        control: Option<ApiControlLease>,
    ) -> Result<WorkspaceReply, String> {
        let snapshot = store
            .api_snapshot(id)?
            .ok_or("No background API execution for this conversation")?;
        Ok(WorkspaceReply::Api { snapshot, control })
    }
    fn start(
        &mut self,
        launch: ApiLaunch,
        owner: Owner,
        store: &mut Store,
    ) -> Result<WorkspaceReply, String> {
        let receipt = launch.receipt.clone();
        launch.validate()?;
        if store.api_matches(&receipt)? {
            let control = self
                .jobs
                .get(&receipt.id)
                .and_then(|job| job.controller.as_ref())
                .filter(|c| c.owner == owner && c.view == receipt.nonce)
                .map(|c| c.lease.clone());
            return self.inspect(&receipt.id, store, control);
        }
        if self.jobs.len() >= 16 {
            return Err("The session host already owns 16 API executions".into());
        }
        let lease = Arc::new(Lease::acquire(&self.paths.lease(&receipt.id))?);
        let snapshot = ApiSnapshot {
            id: receipt.id.clone(),
            epoch: receipt.epoch,
            sequence: 0,
            state: ApiState::Starting,
            user_text: store.submission_text(&receipt)?,
            preview: String::new(),
            result: None,
            error: None,
        };
        let pid = std::process::id();
        let host = (
            pid,
            process_start(pid).ok_or("Cannot identify the session host")?,
        );
        store.admit_api(&receipt, &owner, &host, &snapshot)?;
        let control = ApiControlLease {
            id: receipt.id.clone(),
            epoch: receipt.epoch,
            generation: uuid::Uuid::new_v4().to_string(),
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let (commands, rx) = bounded(4);
        let sender = self.sender.clone();
        let child_lease = Arc::clone(&lease);
        let child_cancel = Arc::clone(&cancel);
        let child_receipt = receipt.clone();
        let spawn = std::thread::Builder::new()
            .name("jterm-hosted-api".into())
            .spawn(move || {
                supervise(launch, child_lease, child_cancel, rx, sender, child_receipt);
            });
        if let Err(error) = spawn {
            let snapshot = store.settle_api(
                &receipt,
                Err(format!("Could not prepare API worker: {error}")),
                false,
            )?;
            return Ok(WorkspaceReply::Api {
                snapshot,
                control: None,
            });
        }
        self.jobs.insert(
            receipt.id.clone(),
            Running {
                receipt: receipt.clone(),
                snapshot: snapshot.clone(),
                controller: Some(Controller {
                    owner,
                    view: receipt.nonce,
                    lease: control.clone(),
                }),
                cancel,
                commands,
                _lease: lease,
            },
        );
        Ok(WorkspaceReply::Api {
            snapshot,
            control: Some(control),
        })
    }
    fn attach(
        &mut self,
        id: &ConversationId,
        epoch: u64,
        view: String,
        owner: Owner,
        store: &Store,
    ) -> Result<WorkspaceReply, String> {
        if view.is_empty() || view.len() > 128 || view.chars().any(char::is_control) {
            return Err("Invalid API view identity".into());
        }
        if let Some(job) = self.jobs.get_mut(id) {
            if job.receipt.epoch != epoch {
                return Err("Stale API runtime incarnation".into());
            }
            if let Some(controller) = &job.controller {
                if controller.owner == owner && controller.view == view {
                    return Ok(WorkspaceReply::Api {
                        snapshot: job.snapshot.clone(),
                        control: Some(controller.lease.clone()),
                    });
                }
                if process_matches(controller.owner.0, &controller.owner.1) {
                    return Err(
                        "Another live view controls this API execution; detach that view first"
                            .into(),
                    );
                }
            }
            let lease = ApiControlLease {
                id: id.clone(),
                epoch,
                generation: uuid::Uuid::new_v4().to_string(),
            };
            job.controller = Some(Controller {
                owner,
                view,
                lease: lease.clone(),
            });
            return Ok(WorkspaceReply::Api {
                snapshot: job.snapshot.clone(),
                control: Some(lease),
            });
        }
        let reply = self.inspect(id, store, None)?;
        if matches!(&reply,WorkspaceReply::Api{snapshot,..} if snapshot.epoch!=epoch) {
            return Err("Stale API runtime incarnation".into());
        }
        Ok(reply)
    }
    fn owned(&mut self, control: &ApiControlLease, owner: &Owner) -> Result<&mut Running, String> {
        let job = self
            .jobs
            .get_mut(&control.id)
            .ok_or("API execution no longer has a live controller")?;
        if !job
            .controller
            .as_ref()
            .is_some_and(|c| c.lease == *control && c.owner == *owner)
        {
            return Err("Stale or foreign API control lease".into());
        }
        Ok(job)
    }
    fn detach(
        &mut self,
        control: &ApiControlLease,
        owner: &Owner,
    ) -> Result<WorkspaceReply, String> {
        self.owned(control, owner)?.controller = None;
        Ok(WorkspaceReply::Ack)
    }
    fn cancel(
        &mut self,
        control: &ApiControlLease,
        owner: &Owner,
        store: &Store,
    ) -> Result<WorkspaceReply, String> {
        let job = self.owned(control, owner)?;
        job.cancel.store(true, Ordering::SeqCst);
        let _ = job.commands.try_send(ApiWorkerCommand::Cancel);
        job.snapshot.state = ApiState::Stopping;
        job.snapshot.sequence += 1;
        store.save_api_snapshot(&job.receipt, &job.snapshot)?;
        Ok(WorkspaceReply::Ack)
    }
    pub fn poll(&mut self, store: &mut Store) {
        for _ in 0..8 {
            let Ok(event) = self.events.try_recv() else {
                break;
            };
            let receipt = match &event {
                Event::Started(r, _, _) | Event::Preview(r, _) | Event::Finished(r, _) => r,
            };
            let Some(job) = self
                .jobs
                .get_mut(&receipt.id)
                .filter(|j| j.receipt == *receipt)
            else {
                continue;
            };
            match event {
                Event::Started(_, pid, start) => {
                    let _ = store.assign_api_worker(&job.receipt, pid, &start);
                    if job.snapshot.state != ApiState::Stopping {
                        job.snapshot.state = ApiState::Running;
                    }
                    job.snapshot.sequence += 1;
                    let _ = store.save_api_snapshot(&job.receipt, &job.snapshot);
                }
                Event::Preview(_, text) => {
                    job.snapshot.preview = text;
                    job.snapshot.sequence += 1;
                    let _ = store.save_api_snapshot(&job.receipt, &job.snapshot);
                }
                Event::Finished(receipt, result) => {
                    if let Some(result) = result {
                        let cancelled = job.cancel.load(Ordering::SeqCst);
                        if store.settle_api(&receipt, result, cancelled).is_err() {
                            job.snapshot.state = ApiState::Interrupted;
                            job.snapshot.sequence += 1;
                            job.snapshot.error=Some("API execution ended but durable completion could not be confirmed. The request will not be repeated.".into());
                            let _ = store.save_api_snapshot(&receipt, &job.snapshot);
                        }
                    } else {
                        job.snapshot.state = ApiState::Interrupted;
                        job.snapshot.sequence += 1;
                        job.snapshot.error=Some("API worker ended without a verified result. The request will not be repeated.".into());
                        let _ = store.save_api_snapshot(&receipt, &job.snapshot);
                    }
                    // Release the supervisor's lease only after recording the
                    // result. Surviving descendants retain their inherited fd.
                    self.jobs.remove(&receipt.id);
                }
            }
        }
    }
}

fn supervise(
    launch: ApiLaunch,
    lease: Arc<Lease>,
    cancel: Arc<AtomicBool>,
    commands: Receiver<ApiWorkerCommand>,
    events: Sender<Event>,
    receipt: SubmissionReceipt,
) {
    let run = (|| -> Result<Option<Result<String, String>>, String> {
        if cancel.load(Ordering::SeqCst) {
            return Ok(Some(Err("Response cancelled before startup".into())));
        }
        let mut command = Command::new(std::env::current_exe().map_err(|e| e.to_string())?);
        command
            .arg(API_WORKER_FLAG)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        lease.configure_child(&mut command)?;
        let mut child = SupervisedChild::spawn(&mut command)
            .map_err(|e| format!("Could not start background API worker: {e}"))?;
        let worker_pid = child.process_group_id() as u32;
        let worker_start =
            process_start(worker_pid).ok_or("Cannot verify the background API worker identity")?;
        let mut input = child
            .take_stdin()
            .ok_or("API worker input is unavailable")?;
        let output = child
            .take_stdout()
            .ok_or("API worker output is unavailable")?;
        let (tx, rx) = bounded(32);
        let writer = std::thread::spawn(move || {
            if write_frame(&mut input, &launch).is_err() {
                return;
            }
            while let Ok(command) = commands.recv() {
                if write_frame(&mut input, &command).is_err() {
                    break;
                }
            }
        });
        let reader = std::thread::spawn(move || {
            let mut output = BufReader::new(output);
            while let Ok(Some(event)) = read_frame::<ApiWorkerEvent>(&mut output) {
                let done = matches!(event, ApiWorkerEvent::Complete(_));
                if tx.send(event).is_err() || done {
                    break;
                }
            }
        });
        let _ = events.send(Event::Started(receipt.clone(), worker_pid, worker_start));
        let started = Instant::now();
        let mut cancelled_at = None;
        let mut result = None;
        loop {
            if cancel.load(Ordering::SeqCst) && cancelled_at.is_none() {
                cancelled_at = Some(Instant::now());
            }
            match rx.recv_timeout(Duration::from_millis(10)) {
                Ok(ApiWorkerEvent::Preview(text)) => {
                    let _ = events.try_send(Event::Preview(receipt.clone(), text));
                }
                Ok(ApiWorkerEvent::Complete(value)) => {
                    result = Some(value);
                    break;
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            }
            if cancelled_at.is_some_and(|at| at.elapsed() > Duration::from_secs(3))
                || started.elapsed() > Duration::from_secs(15 * 60)
            {
                break;
            }
        }
        // The worker reports Complete only after curl has reaped. For an
        // interrupted worker, inherited leases quarantine surviving children.
        let _ = child.terminate_group();
        let _ = child.reap_after_group_kill();
        drop(rx);
        drop(writer); // Input is unblocked by child exit; no GUI lifetime owns it.
        let _ = reader.join();
        Ok(result)
    })();
    let result = match run {
        Ok(result) => result,
        Err(error) => Some(Err(error)),
    };
    let _ = events.send(Event::Finished(receipt, result));
}
