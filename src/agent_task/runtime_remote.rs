//! A view of host-owned native executions. No provider is spawned here.
use super::*;
use crate::agent_workspace::{self as ws, WorkspaceCommand as Command, WorkspaceReply as Reply};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

struct Cached {
    snapshot: ws::NativeSnapshot,
    view: Option<Arc<CodexAppServerViewSnapshot>>,
    control: Option<ws::NativeControlLease>,
}
enum Pending {
    Catalog,
    Inspect(TaskId),
    Start(TaskId),
    Attach(TaskId, Option<ws::NativeAction>),
    Action(TaskId, Option<String>),
    Detach,
}
pub(super) struct NativeRemote {
    client: Option<ws::WorkspaceClient>,
    namespace: String,
    view: String,
    cached: HashMap<TaskId, Cached>,
    versions: HashMap<TaskId, (ws::ConversationId, String, u64)>,
    selected: Option<TaskId>,
    validation_requests: HashSet<TaskId>,
    owned_launches: Vec<(TaskId, ws::OwnedTerminalDescriptor)>,
    pending: HashMap<u64, Pending>,
    inspections: HashSet<TaskId>,
    last_catalog: Instant,
    cursor: Option<ws::ConversationId>,
    catalog_pending: bool,
    retry_after: Instant,
    last_policy: Option<NativePromptPolicy>,
    attach_blocked: HashSet<TaskId>,
    pub(super) prompt_outcomes: Vec<NativePromptOutcome>,
    handoff_requests: HashSet<TaskId>,
    ready_handoffs: Vec<(TaskId, crate::agent_sessions::AgentSession)>,
}
impl NativeRemote {
    pub fn new(namespace: String) -> Self {
        Self::with_client(namespace, ws::WorkspaceClient::new())
    }
    pub fn with_client(namespace: String, client: ws::WorkspaceClient) -> Self {
        Self {
            client: Some(client),
            namespace,
            view: uuid::Uuid::new_v4().to_string(),
            cached: HashMap::new(),
            versions: HashMap::new(),
            selected: None,
            validation_requests: HashSet::new(),
            owned_launches: Vec::new(),
            pending: HashMap::new(),
            inspections: HashSet::new(),
            last_catalog: Instant::now() - Duration::from_secs(5),
            cursor: None,
            catalog_pending: false,
            retry_after: Instant::now(),
            last_policy: None,
            attach_blocked: HashSet::new(),
            prompt_outcomes: Vec::new(),
            handoff_requests: HashSet::new(),
            ready_handoffs: Vec::new(),
        }
    }
    fn send(&mut self, command: Command, pending: Pending) -> Result<(), AgentRuntimeError> {
        let id = self
            .client
            .as_ref()
            .expect("live native client")
            .send(command)
            .map_err(AgentRuntimeError::Preparation)?;
        self.pending.insert(id, pending);
        Ok(())
    }
    pub fn start(
        &mut self,
        tasks: &TaskManager,
        id: TaskId,
        policy: NativePromptPolicy,
    ) -> Result<(), AgentRuntimeError> {
        if self.cached.contains_key(&id) {
            let task = tasks.get(id).ok_or(AgentRuntimeError::UnknownTask(id))?;
            return self.action(
                id,
                ws::NativeAction::Resume {
                    context: ws::NativeLaunchContext::capture(task.provider)
                        .map_err(AgentRuntimeError::Preparation)?,
                    policy,
                },
            );
        }
        if self.is_preparing(id) {
            return Err(AgentRuntimeError::AlreadyRunning(id));
        }
        let task = tasks
            .get(id)
            .ok_or(AgentRuntimeError::UnknownTask(id))?
            .clone();
        let context = ws::NativeLaunchContext::capture(task.provider)
            .map_err(AgentRuntimeError::Preparation)?;
        self.send(
            Command::StartNative {
                launch: Box::new(ws::NativeLaunch {
                    namespace: self.namespace.clone(),
                    task,
                    context,
                    policy,
                    view: self.view.clone(),
                }),
            },
            Pending::Start(id),
        )
    }
    pub fn action(
        &mut self,
        id: TaskId,
        action: ws::NativeAction,
    ) -> Result<(), AgentRuntimeError> {
        let receipt = action_prompt_receipt(&action);
        if matches!(action, ws::NativeAction::Handoff { .. }) {
            self.handoff_requests.insert(id);
        }
        let cached = self
            .cached
            .get(&id)
            .ok_or(AgentRuntimeError::UnknownTask(id))?;
        if let Some(control) = cached.control.clone() {
            self.send(
                Command::ControlNative {
                    control,
                    operation: uuid::Uuid::new_v4().to_string(),
                    action: Box::new(action),
                },
                Pending::Action(id, receipt),
            )
        } else {
            self.send(
                Command::AttachNative {
                    id: cached.snapshot.id.clone(),
                    incarnation: cached.snapshot.incarnation.clone(),
                    view: self.view.clone(),
                },
                Pending::Attach(id, Some(action)),
            )
        }
    }
    pub fn start_validation(&mut self, id: TaskId) -> Result<(), AgentRuntimeError> {
        let expected_sequence = self
            .cached
            .get(&id)
            .ok_or(AgentRuntimeError::UnknownTask(id))?
            .snapshot
            .sequence;
        self.action(id, ws::NativeAction::Validate { expected_sequence })?;
        self.validation_requests.insert(id);
        Ok(())
    }
    pub fn mutate_task(
        &mut self,
        id: TaskId,
        mutation: ws::NativeTaskMutation,
    ) -> Result<(), AgentRuntimeError> {
        let expected_sequence = self
            .cached
            .get(&id)
            .ok_or(AgentRuntimeError::UnknownTask(id))?
            .snapshot
            .sequence;
        self.action(
            id,
            ws::NativeAction::MutateTask {
                expected_sequence,
                mutation,
            },
        )
    }
    pub fn take_owned_launches(&mut self) -> Vec<(TaskId, ws::OwnedTerminalDescriptor)> {
        std::mem::take(&mut self.owned_launches)
    }
    pub fn poll(
        &mut self,
        tasks: &mut TaskManager,
        policy: NativePromptPolicy,
    ) -> AgentRuntimePollReport {
        let mut report = AgentRuntimePollReport::default();
        for event in self.client.as_ref().expect("live native client").poll() {
            let ws::WorkspaceEvent::Reply { request_id, result } = event;
            let Some(pending) = self.pending.remove(&request_id) else {
                continue;
            };
            let task_id = match &pending {
                Pending::Inspect(id)
                | Pending::Start(id)
                | Pending::Attach(id, _)
                | Pending::Action(id, _) => Some(*id),
                _ => None,
            };
            if let Some(id) = task_id {
                self.inspections.remove(&id);
            }
            if matches!(pending, Pending::Catalog) {
                self.catalog_pending = false;
            }
            let reply = match result {
                Ok(reply) => reply,
                Err(detail) => {
                    // An uncertain operation is never replayed. Catalog/Inspect
                    // are safe to repeat after reconnect and determine the state.
                    if let Some(task_id) = task_id {
                        if let Some(cached) = self.cached.get_mut(&task_id) {
                            cached.control = None;
                        }
                        if matches!(&pending, Pending::Attach(_, None)) {
                            self.attach_blocked.insert(task_id);
                        }
                        if let Some(receipt) = pending_prompt_receipt(&pending) {
                            self.prompt_outcomes.push(NativePromptOutcome {
                                task_id,
                                receipt,
                                result: Err(detail.clone()),
                            });
                        }
                        self.validation_requests.remove(&task_id);
                        if !matches!(pending, Pending::Inspect(_)) {
                            report.issues.push(AgentRuntimeIssue { task_id, detail });
                        }
                    }
                    self.retry_after = Instant::now() + Duration::from_secs(2);
                    continue;
                }
            };
            match reply {
                Reply::NativeCatalog {
                    entries,
                    next_after,
                } => {
                    self.cursor = next_after;
                    self.last_catalog = Instant::now();
                    for entry in entries {
                        if self.inspections.len() >= 8 {
                            break;
                        }
                        let changed = self.versions.get(&entry.task_id).is_none_or(
                            |(_, incarnation, sequence)| {
                                incarnation != &entry.incarnation || *sequence != entry.sequence
                            },
                        );
                        if changed && !self.inspections.contains(&entry.task_id) {
                            if self
                                .send(
                                    Command::InspectNative { id: entry.id },
                                    Pending::Inspect(entry.task_id),
                                )
                                .is_ok()
                            {
                                self.inspections.insert(entry.task_id);
                            }
                        }
                    }
                }
                Reply::Native { snapshot, control } => {
                    let id = snapshot.task.id;
                    if self.validation_requests.contains(&id) {
                        if let Some(validation) = &snapshot.validation {
                            if let Some(terminal) = &validation.terminal {
                                self.owned_launches.push((id, terminal.clone()));
                                self.validation_requests.remove(&id);
                            } else if !validation.preparing {
                                self.validation_requests.remove(&id);
                            }
                        }
                    }

                    let previous = self.cached.get(&id);
                    let gained_control = control.is_some()
                        && matches!(&pending, Pending::Start(_) | Pending::Attach(..));
                    let same =
                        previous.is_some_and(|c| c.snapshot.incarnation == snapshot.incarnation);
                    if let Some(previous) = previous.filter(|_| same) {
                        for event in &snapshot.events {
                            if event.sequence > previous.snapshot.sequence {
                                merge_report(&mut report, event.report.clone());
                            }
                        }
                    }
                    if let Err(detail) =
                        tasks.project_hosted_task(snapshot.task.clone(), snapshot.ownership_locked)
                    {
                        report.issues.push(AgentRuntimeIssue {
                            task_id: id,
                            detail,
                        });
                        continue;
                    }
                    let retained_control = control
                        .or_else(|| previous.filter(|_| same).and_then(|c| c.control.clone()));
                    if self.handoff_requests.contains(&id) && !snapshot.handoff_pending {
                        if let Some(session) = &snapshot.terminal {
                            self.ready_handoffs.push((id, session.clone()));
                            self.handoff_requests.remove(&id);
                        } else if !snapshot.ownership_locked {
                            self.handoff_requests.remove(&id);
                        }
                    }
                    let view = snapshot.view.clone().map(Arc::new);
                    self.versions.insert(
                        id,
                        (
                            snapshot.id.clone(),
                            snapshot.incarnation.clone(),
                            snapshot.sequence,
                        ),
                    );
                    if self.cached.len() >= 128 && !self.cached.contains_key(&id) {
                        let victim = self
                            .cached
                            .iter()
                            .find(|(task, c)| {
                                Some(**task) != self.selected && !c.snapshot.ownership_locked
                            })
                            .map(|(task, _)| *task);
                        if let Some(victim) = victim {
                            self.detach(victim);
                            self.cached.remove(&victim);
                        }
                    }
                    self.cached.insert(
                        id,
                        Cached {
                            snapshot: *snapshot,
                            view,
                            control: retained_control,
                        },
                    );
                    if !same {
                        self.attach_blocked.remove(&id);
                    }
                    report.events_drained += 1;
                    if let Pending::Action(_, Some(receipt)) = &pending {
                        self.prompt_outcomes.push(NativePromptOutcome {
                            task_id: id,
                            receipt: receipt.clone(),
                            result: Ok(()),
                        });
                    }
                    if gained_control {
                        let _ = self.action(id, ws::NativeAction::Policy { policy });
                    }
                    if let Pending::Attach(_, Some(action)) = pending {
                        let stale_policy = match &action {
                            ws::NativeAction::Prompt {
                                policy: requested, ..
                            }
                            | ws::NativeAction::Resume {
                                policy: requested, ..
                            } => *requested != policy,
                            _ => false,
                        };
                        if stale_policy {
                            let detail="Sharing settings changed before the host accepted this action. Your draft was kept.".to_string();
                            if let Some(receipt) = action_prompt_receipt(&action) {
                                self.prompt_outcomes.push(NativePromptOutcome {
                                    task_id: id,
                                    receipt,
                                    result: Err(detail.clone()),
                                });
                            }
                            report.issues.push(AgentRuntimeIssue {
                                task_id: id,
                                detail,
                            });
                            continue;
                        }
                        let receipt = action_prompt_receipt(&action);
                        if let Err(error) = self.action(id, action) {
                            if let Some(receipt) = receipt {
                                self.prompt_outcomes.push(NativePromptOutcome {
                                    task_id: id,
                                    receipt,
                                    result: Err(error.to_string()),
                                });
                            }
                            report.issues.push(AgentRuntimeIssue {
                                task_id: id,
                                detail: error.to_string(),
                            });
                        }
                    }
                }
                _ => {}
            }
        }
        if Instant::now() >= self.retry_after
            && !self.catalog_pending
            && (self.cursor.is_some() || self.last_catalog.elapsed() >= Duration::from_millis(500))
            && self
                .send(
                    Command::NativeCatalog {
                        after: self.cursor.clone(),
                        limit: 64,
                    },
                    Pending::Catalog,
                )
                .is_ok()
        {
            self.catalog_pending = true;
        }
        // Consent changes are authoritative at the host even if the view is
        // subsequently detached. Send only when it changes (not every frame).
        if self.last_policy != Some(policy) {
            self.last_policy = Some(policy);
            let controlled = self
                .cached
                .iter()
                .filter(|(_, c)| c.control.is_some())
                .map(|(id, _)| *id)
                .collect::<Vec<_>>();
            for id in controlled {
                let _ = self.action(id, ws::NativeAction::Policy { policy });
            }
        }
        report
    }
    pub fn select_view(&mut self, selected: Option<TaskId>) {
        if self.selected != selected {
            if let Some(id) = selected {
                self.attach_blocked.remove(&id);
            }
        }
        self.selected = selected;
        if Instant::now() >= self.retry_after {
            if let Some(id) = selected.filter(|id| {
                !self.attach_blocked.contains(id)
                    && !self
                        .pending
                        .values()
                        .any(|p| matches!(p,Pending::Attach(task,_) if task==id))
            }) {
                if let Some(cached) = self
                    .cached
                    .get(&id)
                    .filter(|cached| cached.control.is_none())
                {
                    let _ = self.send(
                        Command::AttachNative {
                            id: cached.snapshot.id.clone(),
                            incarnation: cached.snapshot.incarnation.clone(),
                            view: self.view.clone(),
                        },
                        Pending::Attach(id, None),
                    );
                }
            }
        }
        if let Some(id) =
            selected.filter(|id| !self.cached.contains_key(id) && !self.inspections.contains(id))
        {
            if let Some((conversation, _, _)) = self.versions.get(&id) {
                if self
                    .send(
                        Command::InspectNative {
                            id: conversation.clone(),
                        },
                        Pending::Inspect(id),
                    )
                    .is_ok()
                {
                    self.inspections.insert(id);
                }
            }
        }
        let ids = self
            .cached
            .keys()
            .filter(|id| Some(**id) != selected)
            .copied()
            .collect::<Vec<_>>();
        for id in ids {
            self.detach(id);
        }
    }
    pub fn take_handoffs(&mut self) -> Vec<(TaskId, crate::agent_sessions::AgentSession)> {
        std::mem::take(&mut self.ready_handoffs)
    }
    pub fn fast_poll(&self) -> bool {
        self.any_preparing()
            || self.cached.values().any(|c| {
                c.snapshot.handoff_pending
                    || (c.snapshot.live
                        && c.view.as_ref().is_some_and(|v| {
                            !matches!(
                                v.phase,
                                CodexAppServerPhase::Ready
                                    | CodexAppServerPhase::WaitingForApproval
                            )
                        }))
            })
    }
    pub fn snapshot(&self, id: TaskId) -> Option<Arc<CodexAppServerViewSnapshot>> {
        self.cached.get(&id)?.view.clone()
    }
    pub fn revision(&self, id: TaskId) -> Option<u64> {
        self.cached.get(&id).map(|c| c.snapshot.sequence)
    }
    pub fn state(&self, id: TaskId) -> Option<&ws::NativeSnapshot> {
        self.cached.get(&id).map(|c| &c.snapshot)
    }
    pub fn exit(&self, id: TaskId) -> Option<&CodexAppServerExitReport> {
        self.state(id)?.exit.as_ref()
    }
    pub fn take_exit(&mut self, id: TaskId) -> Option<CodexAppServerExitReport> {
        self.cached.get_mut(&id)?.snapshot.exit.take()
    }
    pub fn is_preparing(&self, id: TaskId) -> bool {
        self.state(id).is_some_and(|s| s.preparing)
            || self
                .pending
                .values()
                .any(|p| matches!(p,Pending::Start(task) if *task==id))
    }
    pub fn any_preparing(&self) -> bool {
        self.cached.values().any(|c| c.snapshot.preparing)
            || self
                .pending
                .values()
                .any(|p| matches!(p, Pending::Start(_)))
    }
    pub fn any_running(&self) -> bool {
        self.cached.values().any(|c| c.snapshot.live)
    }
    pub fn active(&self) -> bool {
        self.any_preparing() || self.cached.values().any(|c| c.snapshot.ownership_locked)
    }
    pub fn detach(&mut self, id: TaskId) {
        if let Some(control) = self.cached.get_mut(&id).and_then(|c| c.control.take()) {
            let _ = self.send(Command::DetachNative { control }, Pending::Detach);
        }
    }
}
impl Drop for NativeRemote {
    fn drop(&mut self) {
        let Some(client) = self.client.take() else {
            return;
        };
        // FIFO on this client places view detach after every admitted Start /
        // Attach, including replies that arrived while the window was closing.
        // Keep its reply receiver alive until the final detach is acknowledged.
        let Ok(close) = client.send(Command::DetachNativeView {
            view: self.view.clone(),
        }) else {
            return;
        };
        let _ = std::thread::Builder::new()
            .name("jterm-native-detach".into())
            .spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(30);
                while Instant::now() < deadline {
                    for event in client.poll() {
                        let ws::WorkspaceEvent::Reply { request_id, .. } = event;
                        if request_id == close {
                            return;
                        }
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            });
    }
}
fn merge_report(into: &mut AgentRuntimePollReport, report: AgentRuntimePollReport) {
    into.preparations_finished += report.preparations_finished;
    into.preparations_started += report.preparations_started;
    into.events_drained += report.events_drained;
    into.events_applied += report.events_applied;
    into.workers_finished += report.workers_finished;
    into.budget_exhausted |= report.budget_exhausted;
    into.applied_turns.extend(report.applied_turns);
    into.issues.extend(report.issues);
    into.completions.extend(report.completions);
}

fn action_prompt_receipt(action: &ws::NativeAction) -> Option<String> {
    match action {
        ws::NativeAction::Prompt { receipt, .. } => receipt.clone(),
        _ => None,
    }
}
fn pending_prompt_receipt(pending: &Pending) -> Option<String> {
    match pending {
        Pending::Action(_, receipt) => receipt.clone(),
        Pending::Attach(_, Some(action)) => action_prompt_receipt(action),
        _ => None,
    }
}
