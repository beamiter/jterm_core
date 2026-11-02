//! Shared command names and input scopes. This registry describes UI intents;
//! the current view and the host retain all execution and ownership checks.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceInputScope {
    Navigation,
    Composer,
    Terminal,
    Inspector,
    Modal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceAction {
    NewTerminal,
    NewApiChat,
    NewTask,
    OpenExisting,
    Search,
    ToggleWorkspace,
    ToggleNavigation,
    ToggleInspector,
    ShowChanges,
    ShowContext,
    ShowRun,
    NextAttention,
    PreviousAttention,
    Send,
    Detach,
    Stop,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkspaceActionDescriptor {
    pub id: &'static str,
    pub label: &'static str,
    /// Only established input contracts have defaults. Frontends keep existing
    /// user bindings for other actions and must check conflicts before adding
    /// global shortcuts.
    pub shortcut: Option<&'static str>,
    pub shortcut_scope: Option<WorkspaceInputScope>,
    pub navigates_only: bool,
}

impl WorkspaceAction {
    pub const ALL: [Self; 16] = [
        Self::NewTerminal,
        Self::NewApiChat,
        Self::NewTask,
        Self::OpenExisting,
        Self::Search,
        Self::ToggleWorkspace,
        Self::ToggleNavigation,
        Self::ToggleInspector,
        Self::ShowChanges,
        Self::ShowContext,
        Self::ShowRun,
        Self::NextAttention,
        Self::PreviousAttention,
        Self::Send,
        Self::Detach,
        Self::Stop,
    ];

    pub fn descriptor(self) -> WorkspaceActionDescriptor {
        let (id, label) = match self {
            Self::NewTerminal => ("workspace.new-terminal", "New terminal"),
            Self::NewApiChat => ("workspace.new-api-chat", "New API chat"),
            Self::NewTask => ("workspace.new-task", "New task"),
            Self::OpenExisting => ("workspace.open-existing", "Open existing conversation"),
            Self::Search => ("workspace.search", "Search conversations"),
            Self::ToggleWorkspace => ("workspace.toggle-group", "Agent Workspace"),
            Self::ToggleNavigation => ("workspace.toggle-navigation", "Navigation"),
            Self::ToggleInspector => ("workspace.toggle-inspector", "Conversation details"),
            Self::ShowChanges => ("workspace.show-changes", "Changes"),
            Self::ShowContext => ("workspace.show-context", "Context"),
            Self::ShowRun => ("workspace.show-run", "Run"),
            Self::NextAttention => ("workspace.next-attention", "Next needs you"),
            Self::PreviousAttention => ("workspace.previous-attention", "Previous needs you"),
            Self::Send => ("workspace.send", "Send"),
            Self::Detach => ("workspace.detach", "Detach view"),
            Self::Stop => ("workspace.stop", "Stop execution"),
        };
        let (shortcut, shortcut_scope) = match self {
            Self::Send => (Some("Ctrl+Enter"), Some(WorkspaceInputScope::Composer)),
            Self::Detach => (Some("Ctrl+]"), Some(WorkspaceInputScope::Terminal)),
            _ => (None, None),
        };
        WorkspaceActionDescriptor {
            id,
            label,
            shortcut,
            shortcut_scope,
            navigates_only: !matches!(
                self,
                Self::NewTerminal
                    | Self::NewApiChat
                    | Self::NewTask
                    | Self::Send
                    | Self::Detach
                    | Self::Stop
            ),
        }
    }

    pub fn label(self) -> &'static str {
        self.descriptor().label
    }

    /// IME composition always owns its confirmation keys. Terminal inputs are
    /// otherwise passed through untouched; navigation bindings never gain a
    /// terminal scope merely because the sidebar is visible.
    pub fn shortcut_allowed(self, scope: WorkspaceInputScope, ime_composing: bool) -> bool {
        !ime_composing && self.descriptor().shortcut_scope == Some(scope)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composer_send_never_steals_ime_or_terminal_input() {
        assert!(WorkspaceAction::Send.shortcut_allowed(WorkspaceInputScope::Composer, false));
        assert!(!WorkspaceAction::Send.shortcut_allowed(WorkspaceInputScope::Composer, true));
        assert!(!WorkspaceAction::Send.shortcut_allowed(WorkspaceInputScope::Terminal, false));
        assert!(!WorkspaceAction::Send.shortcut_allowed(WorkspaceInputScope::Modal, false));
        for action in WorkspaceAction::ALL {
            if action != WorkspaceAction::Detach {
                assert!(!action.shortcut_allowed(WorkspaceInputScope::Terminal, false));
            }
        }
    }

    #[test]
    fn command_ids_are_unique_and_layout_actions_are_distinct() {
        let mut ids: Vec<_> = WorkspaceAction::ALL
            .iter()
            .map(|action| action.descriptor().id)
            .collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), WorkspaceAction::ALL.len());
        assert_ne!(
            WorkspaceAction::ToggleWorkspace.descriptor().id,
            WorkspaceAction::ToggleNavigation.descriptor().id
        );
        assert_ne!(
            WorkspaceAction::Detach.label(),
            WorkspaceAction::Stop.label()
        );
    }
}
