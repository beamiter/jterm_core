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
struct ValidationExecution {
    preparing: Option<
        std::sync::mpsc::Receiver<Result<super::native_validation::PreparedValidation, String>>,
    >,
    runtime: Option<super::host::RuntimeHandle>,
    cancelled: bool,
    last_output_sequence: Option<u64>,
}
struct Job {
    snapshot: NativeSnapshot,
    tasks: TaskManager,
    runtime: AgentRuntimeManager,
    policy: NativePromptPolicy,
    policy_origin: Option<(Owner, String)>,
    controller: Option<Controller>,
    lease: Arc<NativeSessionLease>,
    context: NativeLaunchContext,
    view_revision: Option<u64>,
    historical: bool,
    handoff:
        Option<std::sync::mpsc::Receiver<Result<super::native_handoff::PreparedHandoff, String>>>,
    terminal_runtime: Option<super::host::RuntimeHandle>,
    validation: Option<ValidationExecution>,
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
        if let WorkspaceCommand::Archive { id, .. } | WorkspaceCommand::Delete { id } = &command {
            match store.native_snapshot(id) {
                Ok(Some(_)) => {
                    return Disposition::Handled(Err(
                        "Use the controlled task Hide/Restore action for this hosted task".into(),
                    ))
                }
                Err(error) => return Disposition::Handled(Err(error)),
                Ok(None) => {}
            }
        }
        if let WorkspaceCommand::Organize { changes } = &command {
            for change in changes.iter().filter(|change| change.archived.is_some()) {
                match store.native_snapshot(&change.id) {
                    Ok(Some(_)) => {
                        return Disposition::Handled(Err(
                            "A task archive batch requires controlled task actions".into(),
                        ))
                    }
                    Err(error) => return Disposition::Handled(Err(error)),
                    Ok(None) => {}
                }
            }
        }
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
            snapshot: Box::new(safe_snapshot_projection(&snapshot)),
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
            task: safe_task_projection(&launch.task, launch.policy),
            redact_secrets: launch.policy.redact_secrets,
            validation_evidence_redacted: launch.task.validation_command()
                != safe_task_projection(&launch.task, launch.policy).validation_command(),
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
            validation: None,
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
                policy_origin: Some((owner.clone(), launch.view.clone())),
                controller: Some(Controller {
                    owner,
                    view: launch.view,
                    lease: control.clone(),
                }),
                lease,
                context: launch.context,
                view_revision: None,
                historical: false,
                handoff: None,
                terminal_runtime: None,
                validation: None,
            },
        );
        Ok(WorkspaceReply::Native {
            snapshot: Box::new(safe_snapshot_projection(&snapshot)),
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
                    snapshot: Box::new(safe_snapshot_projection(&job.snapshot)),
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
            snapshot: Box::new(safe_snapshot_projection(&job.snapshot)),
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
        snapshot.can_resume = matches!(
            snapshot.task.status,
            TaskStatus::ReadyForReview | TaskStatus::Failed
        ) && snapshot.task.provider == AgentProvider::Codex
            && snapshot.native_home.is_some()
            && snapshot
                .view
                .as_ref()
                .is_some_and(|v| v.provider_thread_id.is_some());
        snapshot.can_continue_in_terminal = matches!(
            snapshot.task.status,
            TaskStatus::ReadyForReview | TaskStatus::Failed
        ) && snapshot
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
                policy_origin: None,
                controller: None,
                lease,
                context,
                view_revision: None,
                historical: true,
                handoff: None,
                terminal_runtime: None,
                validation: None,
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
        if matches!(action, NativeAction::Validate { .. })
            && self
                .jobs
                .values()
                .filter(|job| job.snapshot.ownership_locked)
                .count()
                >= 8
        {
            return Err(
                "The session host already owns eight active native/validation executions".into(),
            );
        }
        let paths = self.paths.clone();
        let job = self.owned(&control, owner)?;
        let before_sequence = job.snapshot.sequence;
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
            NativeAction::Cancel => {
                if let Some(validation) = &mut job.validation {
                    validation.cancelled = true;
                    if let Some(runtime) = &validation.runtime {
                        runtime.stop()
                    } else {
                        Ok(())
                    }
                } else {
                    job.runtime.cancel(task_id).map_err(|e| e.to_string())
                }
            }
            NativeAction::Approval { id, decision } => job
                .runtime
                .decide_approval(task_id, id, decision)
                .map_err(|e| e.to_string()),
            NativeAction::Prompt { text, policy, .. } => {
                job.policy = policy;
                job.policy_origin = job
                    .controller
                    .as_ref()
                    .map(|c| (c.owner.clone(), c.view.clone()));
                job.snapshot.redact_secrets |= policy.redact_secrets;
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
                // A passive observer's defaults cannot revoke a request that a
                // different window already authorized before detaching.
                let from_origin = job.controller.as_ref().is_some_and(|c| {
                    job.policy_origin.as_ref() == Some(&(c.owner.clone(), c.view.clone()))
                });
                if from_origin {
                    job.policy = policy;
                }
                // Privacy is monotonic for durable history. Never republish raw
                // in-memory source after another window enables redaction.
                job.snapshot.redact_secrets |= policy.redact_secrets;
                Ok(())
            }
            NativeAction::Resume { context, policy } => {
                if !matches!(
                    job.snapshot.task.status,
                    TaskStatus::ReadyForReview | TaskStatus::Failed | TaskStatus::Created
                ) {
                    return Err("This task lifecycle cannot be resumed".into());
                }
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
                job.policy_origin = job
                    .controller
                    .as_ref()
                    .map(|c| (c.owner.clone(), c.view.clone()));
                job.snapshot.redact_secrets |= policy.redact_secrets;
                job.runtime.configure_host(context, job.lease.clone());
                job.historical = false;
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
            NativeAction::Validate { expected_sequence } => {
                if job.snapshot.validation_evidence_redacted
                    && !original_validation_evidence(
                        job.tasks.get(task_id).ok_or("Task is unavailable")?,
                    )
                {
                    return Err("Provide the original validation recipe again; redacted evidence cannot be executed".into());
                }
                if expected_sequence != before_sequence {
                    return Err(
                        "Task changed before validation admission; refresh it before trying again"
                            .into(),
                    );
                }
                if job.snapshot.ownership_locked || job.validation.is_some() {
                    return Err("Finish the native/CLI execution before running validation".into());
                }
                job.tasks
                    .next_validation_attempt(task_id)
                    .map_err(|e| e.to_string())?;
                // Only raw evidence still held by the authoritative runtime may
                // become an executable recipe; persisted redacted evidence is
                // deliberately ineligible after host restart.
                let task = job.tasks.get(task_id).ok_or("Task is unavailable")?.clone();
                let run_id = format!("validation-{}", uuid::Uuid::new_v4());
                let (tx, rx) = std::sync::mpsc::sync_channel(1);
                std::thread::Builder::new()
                    .name("jterm-validation-prepare".into())
                    .spawn(move || {
                        let _ = tx.send(super::native_validation::prepare(&task));
                    })
                    .map_err(|e| e.to_string())?;
                // Preparation cannot execute the command. Bind only after the
                // worker exists so thread creation failure cannot leave a task
                // permanently recorded as validating.
                job.tasks
                    .bind_validation_session(task_id, run_id.clone())
                    .map_err(|e| e.to_string())?;
                job.validation = Some(ValidationExecution {
                    preparing: Some(rx),
                    runtime: None,
                    cancelled: false,
                    last_output_sequence: None,
                });
                job.snapshot.validation = Some(HostedValidationSnapshot {
                    run_id,
                    preparing: true,
                    terminal: None,
                    output: String::new(),
                    output_truncated: false,
                });
                job.snapshot.ownership_locked = true;
                Ok(())
            }
            NativeAction::MutateTask {
                expected_sequence,
                mutation,
            } => {
                if mutation == NativeTaskMutation::AcceptReviewed
                    && job.snapshot.validation_evidence_redacted
                {
                    return Err("Original validation evidence was redacted; provide its original recipe before accepting this task".into());
                }
                if expected_sequence != before_sequence {
                    return Err(
                        "Task changed before this action; refresh it before trying again".into(),
                    );
                }
                if job.snapshot.ownership_locked || job.validation.is_some() {
                    return Err("Stop native/CLI work and finish validation before changing the task lifecycle".into());
                }
                match mutation {
                    NativeTaskMutation::Complete => job.tasks.complete_after_validation(task_id),
                    NativeTaskMutation::AcceptReviewed => {
                        job.tasks.complete_after_manual_review(task_id)
                    }
                    NativeTaskMutation::Archive => job.tasks.archive(task_id),
                    NativeTaskMutation::Restore => job.tasks.restore(task_id),
                }
                .map_err(|e| e.to_string())
            }
            NativeAction::UpdateTask { .. } => Err(
                "Hosted task lifecycle is authoritative in the host; use a controlled task action"
                    .into(),
            ),
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
        if poll_validation(job, &self.paths, store, runtimes) {
            return;
        }
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
fn projection_policy(job: &Job) -> NativePromptPolicy {
    NativePromptPolicy {
        redact_secrets: job.snapshot.redact_secrets || job.policy.redact_secrets,
        ..job.policy
    }
}
fn refresh(job: &mut Job, report: AgentRuntimePollReport, store: &Store) -> Result<(), String> {
    let id = job.snapshot.task.id;
    if job.historical {
        let locked = job.handoff.is_some() || job.terminal_runtime.is_some();
        let can_resume = !locked
            && matches!(
                job.snapshot.task.status,
                TaskStatus::ReadyForReview | TaskStatus::Failed
            )
            && job.snapshot.task.provider == AgentProvider::Codex
            && job.snapshot.native_home.is_some()
            && job
                .snapshot
                .view
                .as_ref()
                .is_some_and(|view| view.provider_thread_id.is_some());
        let can_continue = !locked
            && matches!(
                job.snapshot.task.status,
                TaskStatus::ReadyForReview | TaskStatus::Failed
            )
            && job
                .snapshot
                .view
                .as_ref()
                .is_some_and(|view| view.provider_thread_id.is_some());
        let task = safe_task_projection(
            job.tasks.get(id).ok_or("Historical task is unavailable")?,
            projection_policy(job),
        );
        if job.snapshot.ownership_locked != locked
            || job.snapshot.can_resume != can_resume
            || job.snapshot.can_continue_in_terminal != can_continue
            || job.snapshot.task != task
        {
            job.snapshot.sequence += 1;
            job.snapshot.task = task;
            job.snapshot.ownership_locked = locked;
            job.snapshot.can_resume = can_resume;
            job.snapshot.can_continue_in_terminal = can_continue;
            job.snapshot.redact_secrets |= job.policy.redact_secrets;
            store.save_native(&job.snapshot)?;
        }
        return Ok(());
    }
    let task = job
        .tasks
        .get(id)
        .ok_or("Native task reducer is unavailable")?
        .clone();
    let raw_validation = task.validation_command().map(str::to_string);
    let task = safe_task_projection(&task, projection_policy(job));
    job.snapshot.validation_evidence_redacted |=
        raw_validation.as_deref() != task.validation_command();
    job.snapshot.redact_secrets |= job.policy.redact_secrets;
    let revision = job.runtime.snapshot_revision(id);
    let preparing = job.runtime.has_preparing(id);
    let live = job.runtime.has_running(id);
    let mut locked = job.runtime.has_any_activity()
        || job.handoff.is_some()
        || job.terminal_runtime.is_some()
        || job.validation.is_some();
    let exit = job.runtime.exit_report(id).cloned();
    if exit.as_ref().is_some_and(|exit| {
        exit.process.spawned && !(exit.process.reaped && exit.process.containment_verified_empty)
    }) {
        locked = true;
    }
    let can_prompt = job.validation.is_none() && job.runtime.can_prompt_native(id);
    let can_resume = !locked
        && ((matches!(task.status, TaskStatus::ReadyForReview | TaskStatus::Failed)
            && job.runtime.can_resume_codex(id))
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

#[cfg(test)]
mod historical_projection_tests {
    use super::*;
    #[test]
    fn historical_profile_survives_idle_polls_until_an_explicit_restore() {
        use std::os::unix::fs::DirBuilderExt;
        let root =
            std::env::temp_dir().join(format!("native-history-review-{}", uuid::Uuid::new_v4()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&root)
            .unwrap();
        let paths = Paths::at(root.join("state")).unwrap();
        let mut store = Store::open(&paths).unwrap();
        let mut tasks = TaskManager::new();
        let task_id = tasks
            .create(crate::agent_task::NewTask {
                title: "historical fixture".into(),
                provider: AgentProvider::Codex,
                repo_root: root.join("repo"),
                worktree_path: root.join("worktree"),
                branch: "fixture/history".into(),
                base_commit: "a".repeat(40),
                source_context: None,
            })
            .unwrap();
        let mut task = tasks.get(task_id).unwrap().clone();
        task.status = TaskStatus::ReadyForReview;
        task.runtime_kind = TaskRuntimeKind::Native;
        let id = super::super::model::task_id("fixture/history", &task_id.to_string()).unwrap();
        let home = root.join("native-profile");
        let view = crate::agent_task::CodexAppServerViewSnapshot {
            phase: crate::agent_task::CodexAppServerPhase::Ended,
            provider_thread_id: Some(uuid::Uuid::new_v4().to_string()),
            ..Default::default()
        };
        let snapshot = NativeSnapshot {
            id: id.clone(),
            namespace: "fixture/history".into(),
            incarnation: uuid::Uuid::new_v4().to_string(),
            sequence: 1,
            task,
            view: Some(view),
            preparing: false,
            live: false,
            ownership_locked: false,
            interrupted: true,
            can_prompt: false,
            can_resume: false,
            can_continue_in_terminal: false,
            exit: None,
            events: vec![],
            native_home: Some(home.clone()),
            provider_home: home.clone(),
            source_profile: root.join("source-profile"),
            handoff_pending: false,
            terminal: None,
            operations: vec![],
            redact_secrets: true,
            validation_evidence_redacted: false,
            validation: None,
        };
        store.save_native(&snapshot).unwrap();
        let mut manager = NativeManager::new(paths);
        let owner = (
            std::process::id(),
            super::super::ownership::process_start(std::process::id()).unwrap(),
        );
        manager
            .attach(
                id.clone(),
                snapshot.incarnation,
                "history-view".into(),
                owner,
                &store,
            )
            .unwrap();
        let mut runtimes = HashMap::new();
        for _ in 0..10 {
            manager.poll(&mut store, &mut runtimes);
        }
        let loaded = store.native_snapshot(&id).unwrap().unwrap();
        assert_eq!(loaded.native_home, Some(home));
        assert!(loaded.can_resume);
        assert!(loaded.can_continue_in_terminal);
        assert!(!loaded.live);
        drop(manager);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
}

/// Return true while validation owns the task, independently from any view.
fn poll_validation(
    job: &mut Job,
    paths: &Paths,
    store: &Store,
    runtimes: &mut HashMap<super::model::ConversationId, super::host::RuntimeHandle>,
) -> bool {
    let Some(mut validation) = job.validation.take() else {
        return false;
    };
    let task_id = job.snapshot.task.id;
    let Some(mut snapshot) = job.snapshot.validation.take() else {
        job.validation = Some(validation);
        return true;
    };
    let mut completed = false;
    let mut changed = false;
    if let Some(receiver) = &validation.preparing {
        match receiver.try_recv() {
            Ok(result) => {
                validation.preparing = None;
                snapshot.preparing = false;
                changed = true;
                let result = if validation.cancelled {
                    Err("Validation cancelled before command startup".into())
                } else {
                    result
                };
                match result.and_then(|prepared| {
                    if runtimes
                        .values()
                        .filter(|runtime| runtime.snapshot.lock().state.is_live())
                        .count()
                        >= 16
                    {
                        return Err("The session host already owns sixteen running PTYs".into());
                    }
                    if runtimes.len() >= 32 {
                        runtimes.retain(|_, runtime| runtime.snapshot.lock().state.is_live());
                    }
                    let cwd = prepared.prepared.cwd.clone();
                    let id = super::model::ConversationId(format!(
                        "jterm:validation:{}:{}",
                        task_id, snapshot.run_id
                    ));
                    let runtime = super::native_validation::spawn(
                        paths,
                        id.clone(),
                        prepared,
                        job.lease.clone(),
                        job.context.search_path.as_deref(),
                    )?;
                    let epoch = runtime.snapshot.lock().epoch.clone();
                    snapshot.terminal = Some(OwnedTerminalDescriptor {
                        id: id.clone(),
                        epoch,
                        cwd,
                        label: format!("Validation · {}", job.snapshot.task.title),
                    });
                    runtimes.insert(id, runtime.clone());
                    validation.runtime = Some(runtime);
                    Ok(())
                }) {
                    Ok(()) => {}
                    Err(error) => {
                        snapshot.output = crate::review_input::safe_inline_display(&error, 4096);
                        job.tasks
                            .handle_terminal_session_exit(&snapshot.run_id, None);
                        completed = true;
                    }
                }
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                snapshot.preparing = false;
                snapshot.output =
                    "Validation preparation ended without an outcome; command was not repeated"
                        .into();
                job.tasks
                    .handle_terminal_session_exit(&snapshot.run_id, None);
                completed = true;
                changed = true;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
    }
    if let Some(runtime) = &validation.runtime {
        let state = runtime.snapshot.lock().clone();
        if validation.last_output_sequence != Some(state.event_sequence) {
            let (output, truncated) =
                super::native_validation::output(runtime, job.snapshot.redact_secrets);
            validation.last_output_sequence = Some(state.event_sequence);
            if snapshot.output != output || snapshot.output_truncated != truncated {
                snapshot.output = output;
                snapshot.output_truncated = truncated;
                changed = true;
            }
        }
        if !state.state.is_live() {
            if state.cleanup_verified {
                job.tasks
                    .handle_terminal_session_exit(&snapshot.run_id, state.exit_code);
                completed = true;
                changed = true;
            } else if !job.snapshot.interrupted {
                job.snapshot.interrupted = true;
                job.snapshot.can_prompt = false;
                job.snapshot.can_resume = false;
                snapshot.output.push_str("\nValidation cleanup is unconfirmed; ownership remains locked and no command will be repeated.");
                changed = true;
            }
        }
    }
    job.snapshot.validation = Some(snapshot);
    if !completed {
        job.validation = Some(validation);
    }
    if changed {
        job.snapshot.sequence += 1;
        if let Some(task) = job.tasks.get(task_id) {
            job.snapshot.task = safe_task_projection(task, projection_policy(job));
        }
        job.snapshot.ownership_locked = !completed;
        job.snapshot.can_prompt = false;
        job.snapshot.can_resume = false;
        job.snapshot.can_continue_in_terminal = false;
        let _ = store.save_native(&job.snapshot);
    }
    !completed
}

fn original_validation_evidence(task: &crate::agent_task::AgentTask) -> bool {
    task.source_context.as_ref().is_some_and(|context| {
        context.command_exact
            && !context.command_truncated
            && context
                .command
                .as_ref()
                .is_some_and(|text| !text.is_empty())
    }) || task
        .brief
        .as_ref()
        .is_some_and(|brief| brief.validation.is_some())
}
