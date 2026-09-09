//! Provider-neutral agent/task domain types.
//!
//! One copy of what used to live four times over, as `src/agent_task/` in
//! anvil, forge and frost and as `src/agent/` in ember. The four had stayed in
//! lockstep — at extraction the movable set differed by a single doc comment
//! between anvil and forge, and every other difference was a module path, an
//! `#[allow]`, or the app's own name — but each fix still had to be applied
//! four times and hand-renamed, which is how the copies drifted in the first
//! place.
//!
//! Diff *rendering* deliberately stays with each frontend: it is the one part
//! of the subsystem that touches a toolkit. Core owns the domain half.
//!
//! Anything here that names the running app reads it from
//! [`crate::identity`] through [`app_slug`] / [`app_display`] rather than
//! hardcoding one binary's name.

/// The running app's short name, for anything a machine reads back.
///
/// Temp directories a reaper matches by prefix, the client identity the Codex
/// app-server sees, thread names, git ref namespaces and the prompt tags a
/// test asserts verbatim all pass through here. [`crate::identity::get`] would
/// answer "jterm" before `init`, which is right for branding and wrong here —
/// but the fallback is at least *self-consistent*, since creation and matching
/// both read this one function. Warn once so a missing `identity::init` is
/// visible rather than silent.
pub fn app_slug() -> &'static str {
    match crate::identity::try_get() {
        Some(identity) => identity.app_name,
        None => {
            static WARNED: std::sync::Once = std::sync::Once::new();
            WARNED.call_once(|| {
                log::warn!(
                    "agent_task is naming worktrees, temp dirs and prompt tags before \
                     identity::init; falling back to the neutral \"jterm\""
                );
            });
            "jterm"
        }
    }
}

/// The running app's name as it appears in prose a human reads — an error
/// message, a git commit subject, a model-visible prompt.
pub fn app_display() -> String {
    let slug = app_slug();
    let mut chars = slug.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

pub mod context;
pub mod driver;
pub mod drivers;
pub mod event;
pub mod launcher;
pub mod native;
pub mod pinned_dir;
pub mod replay_text;
pub mod runtime;
pub mod task;
pub mod validation;
pub mod worktree;

// The re-exports below are the module's curated API surface, kept aligned
// with ember's so the frontends can share adapters and documentation. The
// panel consumes only part of it today, so items without a current caller
// are explicitly allowed to stay.
#[allow(unused_imports)]
pub use context::{ContextError, SemanticCommandContext};
#[allow(unused_imports)]
pub use driver::{
    AgentCancellation, AgentCommand, AgentDriver, AgentDriverError, AgentEventQueueLimits,
    AgentEventQueueStats, AgentEventReceiveError, AgentEventReceiver, AgentEventSendError,
    AgentEventSender, AgentEventSink, AgentPrompt, AgentStartRequest, ApprovalDecision,
};
#[allow(unused_imports)]
pub use drivers::{
    CodexAppServerApproval, CodexAppServerApprovalFileChange, CodexAppServerApprovalKind,
    CodexAppServerCommandView, CodexAppServerExitCause, CodexAppServerExitReport,
    CodexAppServerFileChange, CodexAppServerFileChangeView, CodexAppServerPhase,
    CodexAppServerProcessExit, CodexAppServerTurnCommandSummary, CodexAppServerTurnFileSummary,
    CodexAppServerTurnHistory, CodexAppServerViewSnapshot, CODEX_APP_SERVER_LIVE_TURN_MAX,
    CODEX_APP_SERVER_TURN_HISTORY_CAPACITY, CODEX_APP_SERVER_TURN_HISTORY_MAX_BYTES,
};
#[allow(unused_imports)]
pub use event::{
    AgentEvent, AgentEventEpoch, AgentEventError, AgentEventKind, AgentEventStream,
    AgentSessionOutcome, AgentTurnId, ApprovalId, InvalidNativeAgentSessionId,
    InvalidProviderSessionId, NativeAgentSessionId, ProviderSessionId,
};
#[allow(unused_imports)]
pub use launcher::{AgentLaunchError, AgentLaunchSpec};
#[allow(unused_imports)]
pub use native::{
    NativeCodexHomeError, NativePromptError, NativePromptPolicy, NativeWorkspaceError,
    NATIVE_AGENT_FOLLOW_UP_MAX_BYTES,
};
#[allow(unused_imports)]
pub use runtime::{
    AgentRuntimeCompletion, AgentRuntimeError, AgentRuntimeIssue, AgentRuntimeManager,
    AgentRuntimePollReport,
};
#[allow(unused_imports)]
pub use task::{
    AgentProvider, AgentTask, NewTask, TaskError, TaskId, TaskManager, TaskRuntimeKind, TaskSource,
    TaskStatus, TaskTerminalRole, TaskValidationState, TaskValidationStatus,
};
#[allow(unused_imports)]
pub use validation::{
    prepare_task_validation, PreparedTaskValidation, TaskValidationError, TaskValidationPath,
};
#[allow(unused_imports)]
pub use worktree::{
    CreateWorktreeRequest, ManagedWorktree, RetireOutcome, RetirePolicy, WorktreeError,
    WorktreeService,
};
