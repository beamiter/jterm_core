//! The host is the native runtime owner and sole lifecycle reducer.
use super::api_host::Disposition;
use super::native::*;
use super::ownership::{process_matches, NativeSessionLease, Paths};
use super::protocol::{WorkspaceCommand, WorkspaceReply};
use super::store::Store;
use crate::agent_task::{
    AgentProvider, AgentRuntimeManager, AgentRuntimePollReport, NativePromptPolicy, TaskManager,
    TaskRuntimeKind, TaskStatus,
};
use std::collections::HashMap;
use std::sync::Arc;

type Owner = (u32, String);
struct Controller {
    owner: Owner,
    view: String,
    lease: NativeControlLease,
}
struct Job {
    snapshot: NativeSnapshot,
    tasks: TaskManager,
    runtime: AgentRuntimeManager,
    policy: NativePromptPolicy,
    controller: Option<Controller>,
    lease: Arc<NativeSessionLease>,
    context: NativeLaunchContext,
    view_revision: Option<u64>,
    handoff:
        Option<std::sync::mpsc::Receiver<Result<super::native_handoff::PreparedHandoff, String>>>,
    terminal_runtime: Option<super::host::RuntimeHandle>,
}
pub(super) struct NativeManager {
    paths: Paths,
    jobs: HashMap<super::model::ConversationId, Job>,
    next_poll: usize,
}
impl NativeManager {
    pub fn new(paths: Paths) -> Self {
        Self {
            paths,
            jobs: HashMap::new(),
            next_poll: 0,
        }
    }
    pub fn handle(
        &mut self,
        command: WorkspaceCommand,
        owner: Owner,
        store: &mut Store,
    ) -> Disposition {
        let result = match command {
            WorkspaceCommand::StartNative { launch } => self.start(*launch, owner, store),
            WorkspaceCommand::NativeCatalog { after, limit } => {
                store.native_catalog(after.as_ref(), limit)
            }
            WorkspaceCommand::InspectNative { id } => Self::inspect(&id, store, None),
            WorkspaceCommand::AttachNative {
                id,
                incarnation,
                view,
            } => self.attach(id, incarnation, view, owner, store),
            WorkspaceCommand::DetachNative { control } => self.detach(control, &owner),
            WorkspaceCommand::DetachNativeView { view } => {
                for job in self.jobs.values_mut() {
                    if job
                        .controller
                        .as_ref()
                        .is_some_and(|c| c.owner == owner && c.view == view)
                    {
                        job.controller = None;
                    }
                }
                Ok(WorkspaceReply::Ack)
            }
            WorkspaceCommand::ControlNative {
                control,
                operation,
                action,
            } => self.control(control, operation, *action, &owner, store),
            other => return Disposition::Other(other),
        };
        Disposition::Handled(result)
    }
    fn inspect(
        id: &super::model::ConversationId,
        store: &Store,
        control: Option<NativeControlLease>,
    ) -> Result<WorkspaceReply, String> {
        let snapshot = store
            .native_snapshot(id)?
            .ok_or("No hosted native execution for this task")?;
        Ok(WorkspaceReply::Native {
            snapshot: Box::new(snapshot),
            control,
        })
    }
    fn start(
        &mut self,
        launch: NativeLaunch,
        owner: Owner,
        store: &mut Store,
    ) -> Result<WorkspaceReply, String> {
        launch.context.validate()?;
        validate_view(&launch.view)?;
        if self.jobs.len() >= 32 {
            self.jobs.retain(|_, job| job.snapshot.ownership_locked);
        }
        let id = super::model::task_id(&launch.namespace, &launch.task.id.to_string())?;
        if let Some(previous) = store.native_snapshot(&id)? {
            if !same_task_identity(&previous.task, &launch.task) {
                return Err("Native task identity changed".into());
            }
            let control = self
                .jobs
                .get(&id)
                .and_then(|j| j.controller.as_ref())
                .filter(|c| c.owner == owner && c.view == launch.view)
                .map(|c| c.lease.clone());
            return Self::inspect(&id, store, control);
        }
        if self
            .jobs
            .values()
            .filter(|j| j.snapshot.ownership_locked)
            .count()
            >= 8
        {
            return Err("The session host already owns eight active native executions".into());
        }
        if !launch.policy.share_command_context {
            return Err("Enable command-context sharing before starting a native provider".into());
        }
        if launch.task.status != TaskStatus::Created
            || launch.task.runtime_kind != TaskRuntimeKind::Unassigned
            || launch.task.terminal_session_id.is_some()
            || serde_json::to_vec(&launch.task)
                .map_err(|e| e.to_string())?
                .len()
                > 2 * 1024 * 1024
        {
            return Err("Native launch requires one bounded Created, unassigned task".into());
        }
        let lease = Arc::new(NativeSessionLease::acquire_task(&self.paths, id.clone())?);
        let mut tasks = TaskManager::new();
        tasks.project_hosted_task(launch.task.clone(), false)?;
        let mut runtime = AgentRuntimeManager::new();
        runtime.configure_host(launch.context.clone(), lease.clone());
        let source_profile = match launch.task.provider {
            AgentProvider::Codex => launch.context.codex_home.clone(),
            _ => launch.context.claude_home.clone(),
        };
        let mut snapshot = NativeSnapshot {
            id: id.clone(),
            namespace: launch.namespace,
            incarnation: uuid::Uuid::new_v4().to_string(),
            sequence: 1,
            task: launch.task,
            view: None,
            preparing: true,
            live: false,
            ownership_locked: true,
            interrupted: false,
            can_prompt: false,
            can_resume: false,
            can_continue_in_terminal: false,
            exit: None,
            events: vec![],
            native_home: None,
            provider_home: source_profile.clone(),
            source_profile,
            handoff_pending: false,
            terminal: None,
            operations: vec![],
        };
        // Durable intent precedes any preparation/provider worker release.
        store.save_native(&snapshot)?;
        let started = start_provider(
            &mut runtime,
            &mut tasks,
            snapshot.task.id,
            snapshot.task.provider,
            launch.policy,
        );
        if let Err(error) = started {
            snapshot.preparing = false;
            snapshot.ownership_locked = false;
            snapshot.task.status_detail = Some(error.clone());
            snapshot.can_resume = true;
            let view = crate::agent_task::CodexAppServerViewSnapshot {
                phase: crate::agent_task::CodexAppServerPhase::Failed,
                last_error: Some(error),
                ..Default::default()
            };
            snapshot.view = Some(view);
            snapshot.sequence += 1;
            store.save_native(&snapshot)?;
        }
        let control = NativeControlLease {
            id: id.clone(),
            incarnation: snapshot.incarnation.clone(),
            generation: uuid::Uuid::new_v4().to_string(),
        };
        self.jobs.insert(
            id,
            Job {
                snapshot: snapshot.clone(),
                tasks,
                runtime,
                policy: launch.policy,
                controller: Some(Controller {
                    owner,
                    view: launch.view,
                    lease: control.clone(),
                }),
                lease,
                context: launch.context,
                view_revision: None,
                handoff: None,
                terminal_runtime: None,
            },
        );
        Ok(WorkspaceReply::Native {
            snapshot: Box::new(snapshot),
            control: Some(control),
        })
    }
    fn attach(
        &mut self,
        id: super::model::ConversationId,
        incarnation: String,
        view: String,
        owner: Owner,
        store: &Store,
    ) -> Result<WorkspaceReply, String> {
        validate_view(&view)?;
        if !self.jobs.contains_key(&id) {
            self.restore_historical(&id, store)?;
        }
        let Some(job) = self.jobs.get_mut(&id) else {
            return Self::inspect(&id, store, None);
        };
        if job.snapshot.incarnation != incarnation {
            return Err("Stale native runtime incarnation".into());
        }
        if let Some(controller) = &job.controller {
            if controller.owner == owner && controller.view == view {
                return Ok(WorkspaceReply::Native {
                    snapshot: Box::new(job.snapshot.clone()),
                    control: Some(controller.lease.clone()),
                });
            }
            if process_matches(controller.owner.0, &controller.owner.1) {
                return Err(
                    "Another live view controls this native execution; detach it first".into(),
                );
            }
        }
        let control = NativeControlLease {
            id,
            incarnation,
            generation: uuid::Uuid::new_v4().to_string(),
        };
        job.controller = Some(Controller {
            owner,
            view,
            lease: control.clone(),
        });
        Ok(WorkspaceReply::Native {
            snapshot: Box::new(job.snapshot.clone()),
            control: Some(control),
        })
    }
    fn restore_historical(
        &mut self,
        id: &super::model::ConversationId,
        store: &Store,
    ) -> Result<(), String> {
        let mut snapshot = store
            .native_snapshot(id)?
            .ok_or("Native history is unavailable")?;
        // No time-based takeover: surviving descendants retain this same
        // open-file-description lease after a host crash.
        let lease=Arc::new(NativeSessionLease::acquire_task(&self.paths,id.clone()).map_err(|e|format!("The previous native process may still own this task: {e}. History remains readable."))?);
        snapshot.live = false;
        snapshot.preparing = false;
        snapshot.ownership_locked = false;
        snapshot.can_prompt = false;
        snapshot.can_resume = snapshot.task.provider == AgentProvider::Codex
            && snapshot.native_home.is_some()
            && snapshot
                .view
                .as_ref()
                .is_some_and(|v| v.provider_thread_id.is_some());
        snapshot.can_continue_in_terminal = snapshot
            .view
            .as_ref()
            .is_some_and(|v| v.provider_thread_id.is_some());
        snapshot.sequence += 1;
        if let Some(view) = &mut snapshot.view {
            view.pending_approvals.clear();
            view.history_loading = false;
        }
        let context = NativeLaunchContext {
            search_path: None,
            codex_home: snapshot.source_profile.clone(),
            claude_home: snapshot.source_profile.clone(),
            environment: vec![],
        };
        let mut tasks = TaskManager::new();
        tasks.project_hosted_task(snapshot.task.clone(), false)?;
        let mut runtime = AgentRuntimeManager::new();
        runtime.configure_host(context.clone(), lease.clone());
        store.save_native(&snapshot)?;
        self.jobs.insert(
            id.clone(),
            Job {
                snapshot,
                tasks,
                runtime,
                policy: NativePromptPolicy {
                    share_command_context: false,
                    redact_secrets: true,
                },
                controller: None,
                lease,
                context,
                view_revision: None,
                handoff: None,
                terminal_runtime: None,
            },
        );
        Ok(())
    }
    fn owned(&mut self, control: &NativeControlLease, owner: &Owner) -> Result<&mut Job, String> {
        let job = self
            .jobs
            .get_mut(&control.id)
            .ok_or("Native execution is historical; no live controller is available")?;
        if !job
            .controller
            .as_ref()
            .is_some_and(|c| c.owner == *owner && c.lease == *control)
        {
            return Err("Stale or foreign native control lease".into());
        }
        Ok(job)
    }
    fn detach(
        &mut self,
        control: NativeControlLease,
        owner: &Owner,
    ) -> Result<WorkspaceReply, String> {
        self.owned(&control, owner)?.controller = None;
        Ok(WorkspaceReply::Ack)
    }
    fn control(
        &mut self,
        control: NativeControlLease,
        operation: String,
        action: NativeAction,
        owner: &Owner,
        store: &mut Store,
    ) -> Result<WorkspaceReply, String> {
        if uuid::Uuid::parse_str(&operation).is_err() {
            return Err("Invalid native operation identity".into());
        }
        let paths = self.paths.clone();
        let job = self.owned(&control, owner)?;
        if !store.reserve_native_operation(
            &job.snapshot.id,
            &job.snapshot.incarnation,
            &operation,
        )? {
            return Self::inspect(&control.id.clone(), store, Some(control));
        }
        job.snapshot.operations.push(operation);
        if job.snapshot.operations.len() > 128 {
            job.snapshot.operations.remove(0);
        }
        job.snapshot.sequence += 1;
        store.save_native(&job.snapshot)?;
        let task_id = job.snapshot.task.id;
        let result = match action {
            NativeAction::Cancel => job.runtime.cancel(task_id).map_err(|e| e.to_string()),
            NativeAction::Approval { id, decision } => job
                .runtime
                .decide_approval(task_id, id, decision)
                .map_err(|e| e.to_string()),
            NativeAction::Prompt { text, policy } => {
                job.policy = policy;
                job.runtime
                    .prompt_native(&job.tasks, task_id, &text, policy)
                    .map_err(|e| e.to_string())
            }
            NativeAction::Finish => job
                .runtime
                .finish_codex(&job.tasks, task_id)
                .map_err(|e| e.to_string()),
            NativeAction::History { reset } => job
                .runtime
                .load_native_history(task_id, reset)
                .map_err(|e| e.to_string()),
            NativeAction::Policy { policy } => {
                job.policy = policy;
                Ok(())
            }
            NativeAction::Resume { context, policy } => {
                context.validate()?;
                let requested = if job.snapshot.task.provider == AgentProvider::Codex {
                    &context.codex_home
                } else {
                    &context.claude_home
                };
                if requested != &job.snapshot.source_profile {
                    return Err(
                        "Reopen the original provider profile before restoring this task".into(),
                    );
                }
                if job.snapshot.ownership_locked {
                    return Err("Stop the current native or terminal owner before restoring".into());
                }
                if !policy.share_command_context {
                    return Err("Enable command-context sharing before restoring".into());
                }
                job.context = context.clone();
                job.policy = policy;
                job.runtime.configure_host(context, job.lease.clone());
                if job.snapshot.task.status == TaskStatus::Created
                    && job.snapshot.task.runtime_kind == TaskRuntimeKind::Unassigned
                {
                    start_provider(
                        &mut job.runtime,
                        &mut job.tasks,
                        task_id,
                        job.snapshot.task.provider,
                        policy,
                    )
                } else if job.snapshot.task.provider == AgentProvider::Codex {
                    let home =
                        job.snapshot.native_home.clone().ok_or(
                            "No exact native profile is recorded; history remains readable",
                        )?;
                    let view = job
                        .snapshot
                        .view
                        .clone()
                        .filter(|v| v.provider_thread_id.is_some())
                        .ok_or("No exact provider thread is recorded")?;
                    job.runtime.restore_hosted_codex(task_id, home, view);
                    let mut task = job.snapshot.task.clone();
                    task.status = TaskStatus::ReadyForReview;
                    task.status_detail=Some("Explicitly restoring the exact provider thread; previous operations are not repeated".into());
                    job.tasks.project_hosted_task(task, false)?;
                    job.snapshot.interrupted = false;
                    start_provider(
                        &mut job.runtime,
                        &mut job.tasks,
                        task_id,
                        AgentProvider::Codex,
                        policy,
                    )
                } else {
                    Err("Open this stopped provider conversation in Terminal to continue its exact ID".into())
                }
            }
            NativeAction::Handoff { context } => {
                context.validate()?;
                let requested = if job.snapshot.task.provider == AgentProvider::Codex {
                    &context.codex_home
                } else {
                    &context.claude_home
                };
                if requested != &job.snapshot.source_profile {
                    return Err("Use the original provider profile for exact CLI handoff".into());
                }
                job.context = context;
                if job.snapshot.ownership_locked || job.handoff.is_some() {
                    return Err("Stop or Finish the native execution before handing its exact conversation to Terminal".into());
                }
                if job
                    .snapshot
                    .view
                    .as_ref()
                    .and_then(|v| v.provider_thread_id.as_ref())
                    .is_none()
                {
                    return Err("No exact provider conversation was recorded; native history remains readable".into());
                }
                let (tx, rx) = std::sync::mpsc::sync_channel(1);
                let snapshot = job.snapshot.clone();
                let context = job.context.clone();
                let lease = job.lease.clone();
                let paths = paths.clone();
                std::thread::Builder::new()
                    .name("jterm-native-cli-handoff".into())
                    .spawn(move || {
                        let _ = tx.send(super::native_handoff::prepare(
                            snapshot, context, lease, paths,
                        ));
                    })
                    .map_err(|e| e.to_string())?;
                job.handoff = Some(rx);
                job.snapshot.handoff_pending = true;
                job.snapshot.ownership_locked = true;
                Ok(())
            }
            NativeAction::UpdateTask { task } => {
                if job.snapshot.ownership_locked || !same_task_identity(&job.snapshot.task, &task) {
                    Err("Cannot change a running native task identity".into())
                } else {
                    job.tasks.project_hosted_task(*task, false)
                }
            }
        };
        result?;
        refresh(job, AgentRuntimePollReport::default(), store)?;
        Self::inspect(&control.id.clone(), store, Some(control))
    }
    pub fn poll(
        &mut self,
        store: &mut Store,
        runtimes: &mut HashMap<super::model::ConversationId, super::host::RuntimeHandle>,
    ) {
        let mut ids = self.jobs.keys().cloned().collect::<Vec<_>>();
        ids.sort();
        if ids.is_empty() {
            return;
        }
        let index = self.next_poll % ids.len();
        self.next_poll = index + 1;
        let Some(job) = self.jobs.get_mut(&ids[index]) else {
            return;
        };
        if let Some(receiver) = &job.handoff {
            match receiver.try_recv() {
                Ok(result) => {
                    job.handoff = None;
                    job.snapshot.handoff_pending = false;
                    let result = result.and_then(|prepared| {
                        store.upsert_runtime_row(&prepared.launch.row)?;
                        let runtime = super::host::spawn_runtime_with_native(
                            &self.paths,
                            &prepared.launch,
                            Some(prepared.options),
                        )?;
                        runtimes.insert(prepared.launch.row.id.clone(), runtime.clone());
                        job.terminal_runtime = Some(runtime);
                        job.snapshot.terminal = Some(prepared.session);
                        job.snapshot.can_continue_in_terminal = false;
                        let mut task = job.snapshot.task.clone();
                        task.status = TaskStatus::Working;
                        task.status_detail =
                            Some("Continuing the exact provider conversation in Terminal".into());
                        job.tasks.project_hosted_task(task.clone(), false)?;
                        job.snapshot.task = task;
                        Ok(())
                    });
                    if let Err(error) = result {
                        job.snapshot.task.status_detail = Some(error.clone());
                        if let Some(view) = &mut job.snapshot.view {
                            view.last_error = Some(error);
                        }
                    }
                    job.snapshot.sequence += 1;
                    let _ = store.save_native(&job.snapshot);
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    job.handoff = None;
                    job.snapshot.handoff_pending = false;
                    job.snapshot.sequence += 1;
                    let _ = store.save_native(&job.snapshot);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
        if let Some(terminal) = &job.terminal_runtime {
            let state = terminal.snapshot.lock().clone();
            if state.state.is_live() {
                return;
            }
            if !state.cleanup_verified {
                if !job.snapshot.interrupted {
                    job.snapshot.interrupted = true;
                    job.snapshot.can_prompt = false;
                    job.snapshot.can_resume = false;
                    job.snapshot.task.status_detail=Some("CLI cleanup is unconfirmed; ownership stays locked and no operation will be repeated".into());
                    job.snapshot.sequence += 1;
                    let _ = store.save_native(&job.snapshot);
                }
                return;
            }
            job.terminal_runtime = None;
            job.snapshot.terminal = None;
            job.snapshot.ownership_locked = false;
            let mut task = job.snapshot.task.clone();
            task.status = if state.exit_code == Some(0) {
                TaskStatus::ReadyForReview
            } else {
                TaskStatus::Failed
            };
            task.status_detail = Some(format!("Exact provider CLI exited: {}", state.detail));
            let _ = job.tasks.project_hosted_task(task, false);
        }
        let report = job.runtime.poll(&mut job.tasks, job.policy);
        let _ = refresh(job, report, store);
    }
}
fn refresh(job: &mut Job, report: AgentRuntimePollReport, store: &Store) -> Result<(), String> {
    let id = job.snapshot.task.id;
    let task = job
        .tasks
        .get(id)
        .ok_or("Native task reducer is unavailable")?
        .clone();
    let revision = job.runtime.snapshot_revision(id);
    let preparing = job.runtime.has_preparing(id);
    let live = job.runtime.has_running(id);
    let mut locked =
        job.runtime.has_any_activity() || job.handoff.is_some() || job.terminal_runtime.is_some();
    let exit = job.runtime.exit_report(id).cloned();
    if exit.as_ref().is_some_and(|exit| {
        exit.process.spawned && !(exit.process.reaped && exit.process.containment_verified_empty)
    }) {
        locked = true;
    }
    let can_prompt = job.runtime.can_prompt_native(id);
    let can_resume = !locked
        && (job.runtime.can_resume_codex(id)
            || (!locked
                && task.status == TaskStatus::Created
                && task.runtime_kind == TaskRuntimeKind::Unassigned));
    let home = job.runtime.native_home(id).map(|p| p.to_path_buf());
    let changed = job.snapshot.task != task
        || job.view_revision != revision
        || job.snapshot.preparing != preparing
        || job.snapshot.live != live
        || job.snapshot.ownership_locked != locked
        || job.snapshot.can_prompt != can_prompt
        || job.snapshot.can_resume != can_resume
        || job.snapshot.native_home != home
        || report.made_progress();
    if !changed {
        return Ok(());
    }
    job.snapshot.sequence += 1;
    let can_continue = !locked
        && job
            .snapshot
            .view
            .as_ref()
            .is_some_and(|v| v.provider_thread_id.is_some())
        && matches!(task.status, TaskStatus::Failed | TaskStatus::ReadyForReview);
    job.snapshot.task = task;
    job.snapshot.preparing = preparing;
    job.snapshot.live = live;
    job.snapshot.ownership_locked = locked;
    job.snapshot.can_prompt = can_prompt;
    job.snapshot.can_resume = can_resume;
    job.snapshot.exit = exit;
    job.snapshot.can_continue_in_terminal = can_continue;
    job.snapshot.native_home = home;
    if let Some(home) = &job.snapshot.native_home {
        job.snapshot.provider_home = home.clone();
    }
    if job.view_revision != revision {
        job.snapshot.view = job.runtime.snapshot(id).map(super::native_view::bounded);
        job.view_revision = revision;
    }
    if report.made_progress() {
        if job.snapshot.view.is_none() {
            if let Some(issue) = report.issues.last() {
                let view = crate::agent_task::CodexAppServerViewSnapshot {
                    phase: crate::agent_task::CodexAppServerPhase::Failed,
                    last_error: Some(issue.detail.clone()),
                    ..Default::default()
                };
                job.snapshot.view = Some(view);
            }
        }
        job.snapshot.events.push(NativeReportEvent {
            sequence: job.snapshot.sequence,
            report,
        });
        if job.snapshot.events.len() > 16 {
            job.snapshot.events.remove(0);
        }
    }
    store.save_native(&job.snapshot)
}
fn start_provider(
    runtime: &mut AgentRuntimeManager,
    tasks: &mut TaskManager,
    id: crate::agent_task::TaskId,
    provider: AgentProvider,
    policy: NativePromptPolicy,
) -> Result<(), String> {
    match provider {
        AgentProvider::Codex => runtime.start_codex(tasks, id, policy),
        AgentProvider::Claude => runtime.start_claude(tasks, id, policy),
        AgentProvider::Kimi => runtime.start_kimi(tasks, id, policy),
        _ => return Err("This provider has no structured native backend".into()),
    }
    .map_err(|e| e.to_string())
}
fn validate_view(view: &str) -> Result<(), String> {
    if view.is_empty() || view.len() > 128 || view.chars().any(char::is_control) {
        Err("Invalid native view identity".into())
    } else {
        Ok(())
    }
}
fn same_task_identity(a: &crate::agent_task::AgentTask, b: &crate::agent_task::AgentTask) -> bool {
    a.id == b.id
        && a.provider == b.provider
        && a.repo_root == b.repo_root
        && a.worktree_path == b.worktree_path
        && a.branch == b.branch
        && a.base_commit == b.base_commit
        && a.created_at_ms == b.created_at_ms
}
