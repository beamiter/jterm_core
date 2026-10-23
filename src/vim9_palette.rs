//! A finite command palette shared by every native Vim9 frontend. Display and
//! fuzzy matching are pure; activation rechecks the current editor state and
//! produces the same typed commands used by the existing workbench controls.

use fuzzy_matcher::{skim::SkimMatcherV2, FuzzyMatcher};

use super::{Vim9Command, Vim9Snapshot, Vim9WorkbenchAction};

/// `>` selects command mode. Prefix a literal filename with `./` when its
/// basename starts with `>`; the file picker then retains its ordinary policy.
pub fn vim9_palette_query(value: &str) -> Option<&str> {
    value.trim().strip_prefix('>').map(str::trim)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Vim9PaletteCommand {
    Save,
    SaveAll,
    PreviousBuffer,
    NextBuffer,
    CloseCurrentBuffer,
    ReopenBuffer,
    SplitHorizontal,
    SplitVertical,
    NavigateBack,
    NavigateForward,
    NextDiagnostic,
    PreviousDiagnostic,
    Definition,
    Hover,
    References,
    Format,
    CancelBuild,
    CancelSearch,
    Workbench(Vim9WorkbenchAction),
}

const COMMANDS: [Vim9PaletteCommand; 52] = {
    use Vim9PaletteCommand::*;
    [
        Save,
        SaveAll,
        SplitVertical,
        SplitHorizontal,
        PreviousBuffer,
        NextBuffer,
        CloseCurrentBuffer,
        ReopenBuffer,
        NavigateBack,
        NavigateForward,
        NextDiagnostic,
        PreviousDiagnostic,
        Definition,
        Hover,
        References,
        Format,
        CancelBuild,
        CancelSearch,
        Workbench(Vim9WorkbenchAction::SimpleFiles),
        Workbench(Vim9WorkbenchAction::SimpleRecent),
        Workbench(Vim9WorkbenchAction::SimpleTreeToggle),
        Workbench(Vim9WorkbenchAction::SimpleTreeReveal),
        Workbench(Vim9WorkbenchAction::SimpleTerminalToggle),
        Workbench(Vim9WorkbenchAction::SimpleTerminalNew),
        Workbench(Vim9WorkbenchAction::SimpleTerminalNext),
        Workbench(Vim9WorkbenchAction::SimpleTerminalPrevious),
        Workbench(Vim9WorkbenchAction::SimpleBuffers),
        Workbench(Vim9WorkbenchAction::SimpleGrep),
        Workbench(Vim9WorkbenchAction::SimpleOutline),
        Workbench(Vim9WorkbenchAction::SimpleImplementation),
        Workbench(Vim9WorkbenchAction::SimpleTypeDefinition),
        Workbench(Vim9WorkbenchAction::SimpleWorkspaceSymbols),
        Workbench(Vim9WorkbenchAction::SimpleIncomingCalls),
        Workbench(Vim9WorkbenchAction::SimpleOutgoingCalls),
        Workbench(Vim9WorkbenchAction::SimpleSupertypes),
        Workbench(Vim9WorkbenchAction::SimpleSubtypes),
        Workbench(Vim9WorkbenchAction::SimpleSignatureHelp),
        Workbench(Vim9WorkbenchAction::SimpleDocPin),
        Workbench(Vim9WorkbenchAction::SimpleRename),
        Workbench(Vim9WorkbenchAction::SimpleCodeAction),
        Workbench(Vim9WorkbenchAction::SimpleCodeLens),
        Workbench(Vim9WorkbenchAction::SimpleCodeLensRun),
        Workbench(Vim9WorkbenchAction::SimpleInlayHints),
        Workbench(Vim9WorkbenchAction::SimpleDiagnostics),
        Workbench(Vim9WorkbenchAction::SimpleNextDiagnostic),
        Workbench(Vim9WorkbenchAction::SimplePreviousDiagnostic),
        Workbench(Vim9WorkbenchAction::SimpleGitStatus),
        Workbench(Vim9WorkbenchAction::SimpleGitDiff),
        Workbench(Vim9WorkbenchAction::SimpleGitHunk),
        Workbench(Vim9WorkbenchAction::SimpleMarkdownPreview),
        Workbench(Vim9WorkbenchAction::SimpleKeys),
        Workbench(Vim9WorkbenchAction::SimpleWorkbench),
    ]
};

impl Vim9PaletteCommand {
    pub fn title(self) -> &'static str {
        match self {
            Self::Save => "Save current buffer",
            Self::SaveAll => "Save all buffers",
            Self::PreviousBuffer => "Switch to previous buffer",
            Self::NextBuffer => "Switch to next buffer",
            Self::CloseCurrentBuffer => "Close current buffer and keep splits",
            Self::ReopenBuffer => "Reopen recently closed file",
            Self::SplitHorizontal => "Split editor horizontally",
            Self::SplitVertical => "Split editor vertically",
            Self::NavigateBack => "Go back to previous position",
            Self::NavigateForward => "Go forward to next position",
            Self::NextDiagnostic => "Go to next quickfix problem",
            Self::PreviousDiagnostic => "Go to previous quickfix problem",
            Self::Definition => "Go to definition",
            Self::Hover => "Show symbol documentation",
            Self::References => "Find symbol references",
            Self::Format => "Format current buffer",
            Self::CancelBuild => "Stop running build",
            Self::CancelSearch => "Stop project search",
            Self::Workbench(command) => command.title(),
        }
    }

    pub fn shortcut(self) -> &'static str {
        match self {
            Self::Save => "Ctrl+S",
            Self::SaveAll => "Ctrl+Shift+S",
            Self::PreviousBuffer => "Ctrl+Shift+PageUp",
            Self::NextBuffer => "Ctrl+Shift+PageDown",
            Self::CloseCurrentBuffer => "Ctrl+Shift+W",
            Self::NavigateBack => "Alt+Left",
            Self::NavigateForward => "Alt+Right",
            Self::NextDiagnostic => "F8",
            Self::PreviousDiagnostic => "Shift+F8",
            _ => "",
        }
    }

    fn aliases(self) -> &'static [&'static str] {
        match self {
            Self::Save => &["write", "save file", "保存"],
            Self::SaveAll => &["write all", "save all", "全部保存"],
            Self::PreviousBuffer => &["previous file", "previous tab", "上一个文件"],
            Self::NextBuffer => &["next file", "next tab", "下一个文件"],
            Self::CloseCurrentBuffer => &["close file", "close buffer", "关闭文件"],
            Self::ReopenBuffer => &[
                "reopen closed file",
                "restore closed buffer",
                "恢复关闭文件",
            ],
            Self::SplitHorizontal => &["horizontal split", "split below", "水平分屏"],
            Self::SplitVertical => &["vertical split", "split right", "垂直分屏"],
            Self::NavigateBack => &["jump back", "jumplist back", "返回"],
            Self::NavigateForward => &["jump forward", "jumplist forward", "前进"],
            Self::NextDiagnostic => &["next diagnostic", "next error", "下一个问题"],
            Self::PreviousDiagnostic => &["previous diagnostic", "previous error", "上一个问题"],
            Self::Definition => &["definition", "定义"],
            Self::Hover => &["hover", "documentation", "悬浮", "文档"],
            Self::References => &["references", "引用"],
            Self::Format => &["format", "格式化"],
            Self::CancelBuild => &["cancel build", "停止构建"],
            Self::CancelSearch => &["cancel search", "停止搜索"],
            Self::Workbench(command) => command.aliases(),
        }
    }

    /// These hints avoid sending known unavailable actions. Vim remains the
    /// authority for modes, plugins, modified buffers and asynchronous changes.
    pub fn unavailable_reason(self, snapshot: &Vim9Snapshot) -> Option<&'static str> {
        if !snapshot.connected {
            return Some("Waiting for the Vim connection");
        }
        if !matches!(self, Self::CancelBuild | Self::CancelSearch)
            && (snapshot.waiting_for_input
                || snapshot.mode.is_empty()
                || snapshot.mode.starts_with("no")
                || snapshot.mode.starts_with(['c', 'r', '!', 't']))
        {
            return Some("Finish the current Vim command or input first");
        }
        match self {
            Self::CloseCurrentBuffer
                if snapshot.buffers.iter().any(|b| b.current && b.modified) =>
            {
                Some("Save this buffer before closing it")
            }
            Self::ReopenBuffer if !snapshot.capabilities.iter().any(|c| c == "reopen_buffer") => {
                Some("No recently closed file in this Vim session")
            }
            Self::PreviousBuffer | Self::NextBuffer
                if snapshot.buffers.len() < 2 && !snapshot.buffers_truncated =>
            {
                Some("No other open buffer")
            }
            Self::NavigateBack if !snapshot.navigation.can_go_back => Some("No earlier position"),
            Self::NavigateForward if !snapshot.navigation.can_go_forward => {
                Some("No later position")
            }
            Self::NextDiagnostic | Self::PreviousDiagnostic if snapshot.quickfix.count == 0 => {
                Some("The quickfix list has no problems")
            }
            Self::CancelBuild if !snapshot.build.running => Some("No build is running"),
            Self::CancelSearch if !snapshot.search.running => Some("No project search is running"),
            Self::Definition | Self::Hover | Self::References | Self::Format => {
                let capability = match self {
                    Self::Definition => "definition",
                    Self::Hover => "hover",
                    Self::References => "references",
                    _ => "format",
                };
                (!snapshot
                    .capabilities
                    .iter()
                    .any(|value| value == capability))
                .then_some("The current buffer's language service does not provide this action")
            }
            Self::Workbench(command) => {
                if snapshot.buffer <= 0
                    || snapshot.window <= 0
                    || snapshot.line == 0
                    || snapshot.column == 0
                    || snapshot.line > i64::MAX as u64
                    || snapshot.column > i64::MAX as u64
                {
                    Some("Waiting for the current Vim source identity")
                } else {
                    (!snapshot
                        .capabilities
                        .iter()
                        .any(|value| value == command.capability()))
                    .then_some("This Simple action is unavailable for the current buffer")
                }
            }
            _ => None,
        }
    }

    pub fn command(self, snapshot: &Vim9Snapshot) -> Result<Vim9Command, &'static str> {
        if let Some(reason) = self.unavailable_reason(snapshot) {
            return Err(reason);
        }
        Ok(match self {
            Self::Save => Vim9Command::Save,
            Self::SaveAll => Vim9Command::SaveAll,
            Self::PreviousBuffer => Vim9Command::PreviousBuffer,
            Self::NextBuffer => Vim9Command::NextBuffer,
            Self::CloseCurrentBuffer => Vim9Command::CloseCurrentBuffer,
            Self::ReopenBuffer => Vim9Command::ReopenBuffer,
            Self::SplitHorizontal => Vim9Command::Split { vertical: false },
            Self::SplitVertical => Vim9Command::Split { vertical: true },
            Self::NavigateBack => Vim9Command::NavigateBack,
            Self::NavigateForward => Vim9Command::NavigateForward,
            Self::NextDiagnostic => Vim9Command::NextDiagnostic,
            Self::PreviousDiagnostic => Vim9Command::PreviousDiagnostic,
            Self::Definition => Vim9Command::Definition,
            Self::Hover => Vim9Command::Hover,
            Self::References => Vim9Command::References,
            Self::Format => Vim9Command::Format,
            Self::CancelBuild => Vim9Command::CancelBuild,
            Self::CancelSearch => Vim9Command::CancelSearch,
            Self::Workbench(command) => Vim9Command::Workbench {
                command,
                window: snapshot.window,
                buffer: snapshot.buffer,
                changedtick: snapshot.changedtick,
                line: i64::try_from(snapshot.line).map_err(|_| "Invalid Vim source line")?,
                column: i64::try_from(snapshot.column).map_err(|_| "Invalid Vim source column")?,
            },
        })
    }
}

impl Vim9PaletteCommand {
    pub(super) fn id(self) -> &'static str {
        match self {
            Self::Save => "save",
            Self::SaveAll => "save_all",
            Self::PreviousBuffer => "previous_buffer",
            Self::NextBuffer => "next_buffer",
            Self::CloseCurrentBuffer => "close_current_buffer",
            Self::ReopenBuffer => "reopen_buffer",
            Self::SplitHorizontal => "split_horizontal",
            Self::SplitVertical => "split_vertical",
            Self::NavigateBack => "navigate_back",
            Self::NavigateForward => "navigate_forward",
            Self::NextDiagnostic => "next_diagnostic",
            Self::PreviousDiagnostic => "previous_diagnostic",
            Self::Definition => "definition",
            Self::Hover => "hover",
            Self::References => "references",
            Self::Format => "format",
            Self::CancelBuild => "cancel_build",
            Self::CancelSearch => "cancel_search",
            Self::Workbench(command) => command.capability(),
        }
    }

    pub(super) fn from_request(request: &Vim9Command) -> Option<Self> {
        Some(match request {
            Vim9Command::Save => Self::Save,
            Vim9Command::SaveAll => Self::SaveAll,
            Vim9Command::PreviousBuffer => Self::PreviousBuffer,
            Vim9Command::NextBuffer => Self::NextBuffer,
            Vim9Command::CloseCurrentBuffer => Self::CloseCurrentBuffer,
            Vim9Command::ReopenBuffer => Self::ReopenBuffer,
            Vim9Command::Split { vertical: true } => Self::SplitVertical,
            Vim9Command::Split { vertical: false } => Self::SplitHorizontal,
            Vim9Command::NavigateBack => Self::NavigateBack,
            Vim9Command::NavigateForward => Self::NavigateForward,
            Vim9Command::NextDiagnostic => Self::NextDiagnostic,
            Vim9Command::PreviousDiagnostic => Self::PreviousDiagnostic,
            Vim9Command::Definition => Self::Definition,
            Vim9Command::Hover => Self::Hover,
            Vim9Command::References => Self::References,
            Vim9Command::Format => Self::Format,
            Vim9Command::CancelBuild => Self::CancelBuild,
            Vim9Command::CancelSearch => Self::CancelSearch,
            Vim9Command::Workbench { command, .. } => Self::Workbench(*command),
            _ => return None,
        })
    }
}

/// Case-insensitive fuzzy matching of names and aliases. Ordering is stable
/// across state updates so a language-server reply cannot move the selection.
/// Unavailable commands remain discoverable, with their current reason.
pub fn vim9_palette_matches(query: &str, limit: usize) -> Vec<Vim9PaletteCommand> {
    matches_with_recent(query, limit, &super::workspace::recent_commands())
}

fn matches_with_recent(query: &str, limit: usize, recent: &[String]) -> Vec<Vim9PaletteCommand> {
    let query = query.trim();
    if query.chars().count() > 256 || query.contains(['\r', '\n', '\0']) {
        return Vec::new();
    }
    let matcher = SkimMatcherV2::default().ignore_case().element_limit(4096);
    let mut ranked = COMMANDS
        .iter()
        .copied()
        .enumerate()
        .filter_map(|(index, command)| {
            let score = if query.is_empty() {
                Some(0)
            } else {
                std::iter::once(command.title())
                    .chain(command.aliases().iter().copied())
                    .filter_map(|label| matcher.fuzzy_match(label, query))
                    .max()
            }?;
            let recency = recent
                .iter()
                .position(|id| id == command.id())
                .unwrap_or(usize::MAX);
            Some((command, score, recency, index))
        })
        .collect::<Vec<_>>();
    ranked.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| a.2.cmp(&b.2))
            .then_with(|| a.3.cmp(&b.3))
    });
    ranked
        .into_iter()
        .take(limit.min(256))
        .map(|(command, _, _, _)| command)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connected() -> Vim9Snapshot {
        Vim9Snapshot {
            connected: true,
            mode: "n".into(),
            line: 1,
            column: 1,
            ..Default::default()
        }
    }

    #[test]
    fn command_mode_is_explicit_and_literal_names_remain_reachable() {
        assert_eq!(vim9_palette_query(" > vertical "), Some("vertical"));
        assert_eq!(vim9_palette_query(">"), Some(""));
        for value in ["./>notes", "src/>notes", ":42", "file:42", ""] {
            assert_eq!(vim9_palette_query(value), None);
        }
    }

    #[test]
    fn bounded_catalog_matches_aliases_and_keeps_stable_order() {
        assert_eq!(matches_with_recent("", 256, &[]), COMMANDS);
        assert_eq!(
            vim9_palette_matches("vertical split", 1),
            [Vim9PaletteCommand::SplitVertical]
        );
        assert_eq!(
            vim9_palette_matches("hover", 1),
            [Vim9PaletteCommand::Hover]
        );
        assert_eq!(
            vim9_palette_matches("格式化", 1),
            [Vim9PaletteCommand::Format]
        );
        for value in [
            "definitely no command",
            "save\nquit",
            "save\0",
            &"x".repeat(257),
        ] {
            assert!(vim9_palette_matches(value, 256).is_empty());
        }
        assert!(vim9_palette_matches("save", 0).is_empty());
        assert_eq!(vim9_palette_matches("save", 1).len(), 1);
    }

    #[test]
    fn recent_commands_rank_equal_matches_without_changing_the_catalog() {
        let recent = vec!["reopen_buffer".into(), "save".into()];
        let matches = matches_with_recent("", 256, &recent);
        assert_eq!(matches[0], Vim9PaletteCommand::ReopenBuffer);
        assert_eq!(matches[1], Vim9PaletteCommand::Save);
        assert_eq!(matches.len(), COMMANDS.len());
        assert!(matches_with_recent("close", 256, &recent)
            .contains(&Vim9PaletteCommand::CloseCurrentBuffer));
    }

    #[test]
    fn activation_rechecks_live_capabilities_and_connection() {
        let mut snapshot = connected();
        assert!(Vim9PaletteCommand::Format.command(&snapshot).is_err());
        snapshot.capabilities.push("format".into());
        assert!(matches!(
            Vim9PaletteCommand::Format.command(&snapshot),
            Ok(Vim9Command::Format)
        ));
        snapshot.capabilities.clear();
        assert!(Vim9PaletteCommand::Format.command(&snapshot).is_err());
        snapshot.connected = false;
        for command in COMMANDS {
            assert!(command.command(&snapshot).is_err());
        }
    }

    #[test]
    fn workbench_palette_uses_finite_capabilities_and_captures_source() {
        let mut snapshot = connected();
        snapshot.buffer = 4;
        snapshot.window = 1001;
        snapshot.changedtick = 9;
        let action = Vim9PaletteCommand::Workbench(Vim9WorkbenchAction::SimpleFiles);
        assert!(action.command(&snapshot).is_err());
        snapshot.capabilities.push("simple_files".into());
        let command = action.command(&snapshot).unwrap();
        let frame = serde_json::to_value(command).unwrap();
        assert_eq!(
            frame,
            serde_json::json!({
                "action": "workbench", "command": "simple_files", "window": 1001,
                "buffer": 4, "changedtick": 9, "line": snapshot.line, "column": snapshot.column
            })
        );
        snapshot.capabilities.clear();
        assert!(action.command(&snapshot).is_err());
        let simple = vim9_palette_matches("simple", 256);
        assert_eq!(simple.len(), Vim9WorkbenchAction::ALL.len());
        assert!(simple
            .iter()
            .all(|value| matches!(value, Vim9PaletteCommand::Workbench(_))));
        assert_eq!(
            vim9_palette_matches("simplewhichkey", 1),
            [Vim9PaletteCommand::Workbench(
                Vim9WorkbenchAction::SimpleKeys
            )]
        );
    }

    #[test]
    fn state_hints_follow_actual_quickfix_navigation_and_jobs() {
        let mut snapshot = connected();
        // LSP/location-list diagnostics do not make :cnext available.
        snapshot.diagnostics.push(Default::default());
        assert!(Vim9PaletteCommand::NextDiagnostic
            .command(&snapshot)
            .is_err());
        snapshot.quickfix.count = 1;
        assert!(Vim9PaletteCommand::NextDiagnostic
            .command(&snapshot)
            .is_ok());
        assert!(Vim9PaletteCommand::NavigateBack.command(&snapshot).is_err());
        snapshot.navigation.can_go_back = true;
        assert!(Vim9PaletteCommand::NavigateBack.command(&snapshot).is_ok());
        assert!(Vim9PaletteCommand::CancelBuild.command(&snapshot).is_err());
        snapshot.build.running = true;
        snapshot.mode = "no".into();
        assert!(Vim9PaletteCommand::Save.command(&snapshot).is_err());
        assert!(Vim9PaletteCommand::CancelBuild.command(&snapshot).is_ok());
        snapshot.search.running = true;
        snapshot.waiting_for_input = true;
        assert!(Vim9PaletteCommand::CancelSearch.command(&snapshot).is_ok());
        snapshot.mode = "i".into();
        snapshot.waiting_for_input = false;
        assert!(Vim9PaletteCommand::Save.command(&snapshot).is_ok());
    }

    #[test]
    fn simple_diagnostic_actions_keep_plugin_navigation_separate_from_quickfix() {
        let mut snapshot = connected();
        snapshot.buffer = 4;
        snapshot.window = 1001;
        snapshot.changedtick = 9;
        for action in [
            Vim9WorkbenchAction::SimpleDiagnostics,
            Vim9WorkbenchAction::SimpleNextDiagnostic,
            Vim9WorkbenchAction::SimplePreviousDiagnostic,
        ] {
            let command = Vim9PaletteCommand::Workbench(action);
            assert!(command.command(&snapshot).is_err());
            snapshot.capabilities.push(action.capability().into());
            let request = serde_json::to_value(command.command(&snapshot).unwrap()).unwrap();
            assert_eq!(request["command"], action.capability());
            assert_eq!(request["window"], 1001);
            assert_eq!(request["buffer"], 4);
            assert_eq!(request["changedtick"], 9);
            assert!(vim9_palette_matches("simplecc diagnostic", 256).contains(&command));
        }
        assert!(Vim9PaletteCommand::NextDiagnostic
            .command(&snapshot)
            .is_err());
        snapshot.waiting_for_input = true;
        assert!(
            Vim9PaletteCommand::Workbench(Vim9WorkbenchAction::SimpleNextDiagnostic)
                .command(&snapshot)
                .is_err()
        );
    }

    #[test]
    fn workspace_actions_capture_source_and_leave_terminal_job_input_to_vim() {
        let mut snapshot = connected();
        snapshot.buffer = 4;
        snapshot.window = 1001;
        snapshot.changedtick = 9;
        for action in [
            Vim9WorkbenchAction::SimpleRecent,
            Vim9WorkbenchAction::SimpleTreeToggle,
            Vim9WorkbenchAction::SimpleTreeReveal,
            Vim9WorkbenchAction::SimpleTerminalToggle,
            Vim9WorkbenchAction::SimpleTerminalNew,
            Vim9WorkbenchAction::SimpleTerminalNext,
            Vim9WorkbenchAction::SimpleTerminalPrevious,
        ] {
            let command = Vim9PaletteCommand::Workbench(action);
            assert!(command.command(&snapshot).is_err());
            snapshot.capabilities.push(action.capability().into());
            let request = serde_json::to_value(command.command(&snapshot).unwrap()).unwrap();
            assert_eq!(request["command"], action.capability());
            assert_eq!(request["window"], 1001);
            assert_eq!(request["buffer"], 4);
            assert_eq!(request["changedtick"], 9);
            snapshot.mode = "t".into();
            assert!(command.command(&snapshot).is_err());
            snapshot.mode = "n".into();
        }
        assert_eq!(vim9_palette_matches("simpleterminal", 256).len(), 4);
        let tree = vim9_palette_matches("simpletree", 2);
        assert!(tree.contains(&Vim9PaletteCommand::Workbench(
            Vim9WorkbenchAction::SimpleTreeToggle
        )));
        assert!(tree.contains(&Vim9PaletteCommand::Workbench(
            Vim9WorkbenchAction::SimpleTreeReveal
        )));
        assert!(
            vim9_palette_matches("最近文件", 256).contains(&Vim9PaletteCommand::Workbench(
                Vim9WorkbenchAction::SimpleRecent
            ))
        );
    }

    #[test]
    fn code_lens_and_hint_actions_recheck_the_current_source_and_capability() {
        let mut snapshot = connected();
        snapshot.buffer = 4;
        snapshot.window = 1001;
        snapshot.changedtick = 9;
        for (action, query) in [
            (Vim9WorkbenchAction::SimpleCodeLens, "刷新代码透镜"),
            (Vim9WorkbenchAction::SimpleCodeLensRun, "执行代码透镜"),
            (Vim9WorkbenchAction::SimpleInlayHints, "行内提示"),
        ] {
            let command = Vim9PaletteCommand::Workbench(action);
            assert!(vim9_palette_matches(query, 256).contains(&command));
            assert!(command.command(&snapshot).is_err());
            snapshot.capabilities.push(action.capability().into());
            let request = serde_json::to_value(command.command(&snapshot).unwrap()).unwrap();
            assert_eq!(request["action"], "workbench");
            assert_eq!(request["command"], action.capability());
            assert_eq!(request["window"], 1001);
            assert_eq!(request["buffer"], 4);
            assert_eq!(request["changedtick"], 9);

            // A palette can outlive a buffer switch or provider shutdown. It
            // must use the new snapshot, never an earlier advertised command.
            snapshot.capabilities.clear();
            snapshot.buffer += 1;
            snapshot.changedtick += 1;
            assert!(command.command(&snapshot).is_err());
            snapshot.capabilities.push(action.capability().into());
            snapshot.waiting_for_input = true;
            assert!(command.command(&snapshot).is_err());
            snapshot.waiting_for_input = false;
            snapshot.capabilities.clear();
            snapshot.buffer = 4;
            snapshot.changedtick = 9;
        }
    }

    #[test]
    fn semantic_navigation_actions_capture_cursor_and_recheck_live_state() {
        let mut snapshot = connected();
        snapshot.buffer = 4;
        snapshot.window = 1001;
        snapshot.changedtick = 9;
        snapshot.line = 7;
        snapshot.column = 12;
        for (action, query) in [
            (Vim9WorkbenchAction::SimpleImplementation, "跳转实现"),
            (Vim9WorkbenchAction::SimpleTypeDefinition, "类型定义"),
            (Vim9WorkbenchAction::SimpleWorkspaceSymbols, "工作区符号"),
            (Vim9WorkbenchAction::SimpleIncomingCalls, "调用者"),
            (Vim9WorkbenchAction::SimpleOutgoingCalls, "被调用者"),
            (Vim9WorkbenchAction::SimpleSupertypes, "父类型"),
            (Vim9WorkbenchAction::SimpleSubtypes, "子类型"),
            (Vim9WorkbenchAction::SimpleSignatureHelp, "签名帮助"),
        ] {
            let command = Vim9PaletteCommand::Workbench(action);
            assert!(vim9_palette_matches(query, 256).contains(&command));
            assert!(command.command(&snapshot).is_err());
            snapshot.capabilities.push(action.capability().into());
            let request = serde_json::to_value(command.command(&snapshot).unwrap()).unwrap();
            assert_eq!(request["command"], action.capability());
            assert_eq!(request["window"], 1001);
            assert_eq!(request["buffer"], 4);
            assert_eq!(request["changedtick"], 9);
            assert_eq!(request["line"], 7);
            assert_eq!(request["column"], 12);

            snapshot.column += 1;
            let moved = serde_json::to_value(command.command(&snapshot).unwrap()).unwrap();
            assert_eq!(moved["column"], 13);
            snapshot.column -= 1;
            snapshot.waiting_for_input = true;
            assert!(command.command(&snapshot).is_err());
            snapshot.waiting_for_input = false;
            snapshot.mode = "t".into();
            assert!(command.command(&snapshot).is_err());
            snapshot.mode = "n".into();
            snapshot.capabilities.clear();
            assert!(command.command(&snapshot).is_err());
        }
    }

    #[test]
    fn workbench_rejects_missing_or_unrepresentable_cursor_coordinates() {
        let mut snapshot = connected();
        snapshot.buffer = 4;
        snapshot.window = 1001;
        snapshot.capabilities.push("simple_files".into());
        let command = Vim9PaletteCommand::Workbench(Vim9WorkbenchAction::SimpleFiles);
        for (line, column) in [(0, 1), (1, 0), (u64::MAX, 1), (1, u64::MAX)] {
            snapshot.line = line;
            snapshot.column = column;
            assert!(command.unavailable_reason(&snapshot).is_some());
            assert!(command.command(&snapshot).is_err());
        }
    }

    #[test]
    fn palette_never_interprets_ex_or_build_text() {
        assert!(vim9_palette_matches(":!touch /tmp/palette", 256).is_empty());
        let snapshot = connected();
        assert!(matches!(
            Vim9PaletteCommand::SplitHorizontal.command(&snapshot),
            Ok(Vim9Command::Split { vertical: false })
        ));
        assert!(matches!(
            Vim9PaletteCommand::SplitVertical.command(&snapshot),
            Ok(Vim9Command::Split { vertical: true })
        ));
    }
}
