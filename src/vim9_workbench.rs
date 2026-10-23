//! A bounded view of the user's Simple plugin workbench. The editor owns the
//! providers and their caches; native hosts only display this state and send
//! finite, source-bound actions back to Vim.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Vim9WorkbenchAction {
    SimpleFiles,
    SimpleRecent,
    SimpleTreeToggle,
    SimpleTreeReveal,
    SimpleTerminalToggle,
    SimpleTerminalNew,
    SimpleTerminalNext,
    SimpleTerminalPrevious,
    SimpleBuffers,
    SimpleGrep,
    SimpleOutline,
    SimpleImplementation,
    SimpleTypeDefinition,
    SimpleWorkspaceSymbols,
    SimpleIncomingCalls,
    SimpleOutgoingCalls,
    SimpleSupertypes,
    SimpleSubtypes,
    SimpleSignatureHelp,
    SimpleRename,
    SimpleCodeAction,
    SimpleCodeLens,
    SimpleCodeLensRun,
    SimpleInlayHints,
    SimpleDiagnostics,
    SimpleNextDiagnostic,
    SimplePreviousDiagnostic,
    SimpleGitStatus,
    SimpleGitDiff,
    SimpleGitHunk,
    SimpleMarkdownPreview,
    SimpleKeys,
    SimpleWorkbench,
}

impl Vim9WorkbenchAction {
    pub const ALL: [Self; 33] = [
        Self::SimpleFiles,
        Self::SimpleRecent,
        Self::SimpleTreeToggle,
        Self::SimpleTreeReveal,
        Self::SimpleTerminalToggle,
        Self::SimpleTerminalNew,
        Self::SimpleTerminalNext,
        Self::SimpleTerminalPrevious,
        Self::SimpleBuffers,
        Self::SimpleGrep,
        Self::SimpleOutline,
        Self::SimpleImplementation,
        Self::SimpleTypeDefinition,
        Self::SimpleWorkspaceSymbols,
        Self::SimpleIncomingCalls,
        Self::SimpleOutgoingCalls,
        Self::SimpleSupertypes,
        Self::SimpleSubtypes,
        Self::SimpleSignatureHelp,
        Self::SimpleRename,
        Self::SimpleCodeAction,
        Self::SimpleCodeLens,
        Self::SimpleCodeLensRun,
        Self::SimpleInlayHints,
        Self::SimpleDiagnostics,
        Self::SimpleNextDiagnostic,
        Self::SimplePreviousDiagnostic,
        Self::SimpleGitStatus,
        Self::SimpleGitDiff,
        Self::SimpleGitHunk,
        Self::SimpleMarkdownPreview,
        Self::SimpleKeys,
        Self::SimpleWorkbench,
    ];

    pub fn capability(self) -> &'static str {
        match self {
            Self::SimpleFiles => "simple_files",
            Self::SimpleRecent => "simple_recent",
            Self::SimpleTreeToggle => "simple_tree_toggle",
            Self::SimpleTreeReveal => "simple_tree_reveal",
            Self::SimpleTerminalToggle => "simple_terminal_toggle",
            Self::SimpleTerminalNew => "simple_terminal_new",
            Self::SimpleTerminalNext => "simple_terminal_next",
            Self::SimpleTerminalPrevious => "simple_terminal_previous",
            Self::SimpleBuffers => "simple_buffers",
            Self::SimpleGrep => "simple_grep",
            Self::SimpleOutline => "simple_outline",
            Self::SimpleImplementation => "simple_implementation",
            Self::SimpleTypeDefinition => "simple_type_definition",
            Self::SimpleWorkspaceSymbols => "simple_workspace_symbols",
            Self::SimpleIncomingCalls => "simple_incoming_calls",
            Self::SimpleOutgoingCalls => "simple_outgoing_calls",
            Self::SimpleSupertypes => "simple_supertypes",
            Self::SimpleSubtypes => "simple_subtypes",
            Self::SimpleSignatureHelp => "simple_signature_help",
            Self::SimpleRename => "simple_rename",
            Self::SimpleCodeAction => "simple_code_action",
            Self::SimpleCodeLens => "simple_code_lens",
            Self::SimpleCodeLensRun => "simple_code_lens_run",
            Self::SimpleInlayHints => "simple_inlay_hints",
            Self::SimpleDiagnostics => "simple_diagnostics",
            Self::SimpleNextDiagnostic => "simple_next_diagnostic",
            Self::SimplePreviousDiagnostic => "simple_previous_diagnostic",
            Self::SimpleGitStatus => "simple_git_status",
            Self::SimpleGitDiff => "simple_git_diff",
            Self::SimpleGitHunk => "simple_git_hunk",
            Self::SimpleMarkdownPreview => "simple_markdown_preview",
            Self::SimpleKeys => "simple_keys",
            Self::SimpleWorkbench => "simple_workbench",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::SimpleFiles => "Simple: Find files",
            Self::SimpleRecent => "Simple: Find recent files",
            Self::SimpleTreeToggle => "Simple: Toggle project tree",
            Self::SimpleTreeReveal => "Simple: Reveal current file in tree",
            Self::SimpleTerminalToggle => "Simple: Toggle terminal",
            Self::SimpleTerminalNew => "Simple: Open new terminal",
            Self::SimpleTerminalNext => "Simple: Switch to next terminal",
            Self::SimpleTerminalPrevious => "Simple: Switch to previous terminal",
            Self::SimpleBuffers => "Simple: Find open buffers",
            Self::SimpleGrep => "Simple: Search project text",
            Self::SimpleOutline => "Simple: Show symbol outline",
            Self::SimpleImplementation => "Simple: Go to implementation",
            Self::SimpleTypeDefinition => "Simple: Go to type definition",
            Self::SimpleWorkspaceSymbols => "Simple: Search workspace symbols",
            Self::SimpleIncomingCalls => "Simple: Show incoming calls",
            Self::SimpleOutgoingCalls => "Simple: Show outgoing calls",
            Self::SimpleSupertypes => "Simple: Show supertypes",
            Self::SimpleSubtypes => "Simple: Show subtypes",
            Self::SimpleSignatureHelp => "Simple: Show signature help",
            Self::SimpleRename => "Simple: Rename symbol with Vim prompt",
            Self::SimpleCodeAction => "Simple: Choose code action",
            Self::SimpleCodeLens => "Simple: Refresh CodeLens",
            Self::SimpleCodeLensRun => "Simple: Run CodeLens at cursor",
            Self::SimpleInlayHints => "Simple: Toggle inlay hints",
            Self::SimpleDiagnostics => "Simple: Show buffer diagnostics",
            Self::SimpleNextDiagnostic => "Simple: Go to next buffer diagnostic",
            Self::SimplePreviousDiagnostic => "Simple: Go to previous buffer diagnostic",
            Self::SimpleGitStatus => "Simple: Show Git status",
            Self::SimpleGitDiff => "Simple: Show Git diff",
            Self::SimpleGitHunk => "Simple: Preview current Git hunk",
            Self::SimpleMarkdownPreview => "Simple: Toggle Markdown preview",
            Self::SimpleKeys => "Simple: Show key hints",
            Self::SimpleWorkbench => "Simple: Inspect workbench",
        }
    }

    pub(super) fn aliases(self) -> &'static [&'static str] {
        match self {
            Self::SimpleFiles => &["simplefinder files", "查找文件"],
            Self::SimpleRecent => &["simplefinder recent history", "最近文件"],
            Self::SimpleTreeToggle => &["simpletree simpleremote tree explorer", "项目目录树"],
            Self::SimpleTreeReveal => &["simpletree simpleremote reveal file", "定位当前文件"],
            Self::SimpleTerminalToggle => &["simpleterminal toggle shell", "切换终端"],
            Self::SimpleTerminalNew => &["simpleterminal new shell", "新建终端"],
            Self::SimpleTerminalNext => &["simpleterminal next shell", "下一个终端"],
            Self::SimpleTerminalPrevious => &["simpleterminal previous shell", "上一个终端"],
            Self::SimpleBuffers => &["simplefinder buffers", "查找缓冲区"],
            Self::SimpleGrep => &["simplefinder grep", "搜索项目"],
            Self::SimpleOutline => &["simplecc outline", "符号大纲"],
            Self::SimpleImplementation => &["simplecc implementation", "跳转实现"],
            Self::SimpleTypeDefinition => &["simplecc type definition", "类型定义"],
            Self::SimpleWorkspaceSymbols => {
                &["simplecc workspace symbols live search", "工作区符号"]
            }
            Self::SimpleIncomingCalls => &["simplecc incoming calls callers hierarchy", "调用者"],
            Self::SimpleOutgoingCalls => &["simplecc outgoing calls callees hierarchy", "被调用者"],
            Self::SimpleSupertypes => &["simplecc supertypes parent type hierarchy", "父类型"],
            Self::SimpleSubtypes => &["simplecc subtypes child type hierarchy", "子类型"],
            Self::SimpleSignatureHelp => &["simplecc signature help parameters", "签名帮助"],
            Self::SimpleRename => &["simplecc rename", "重命名符号"],
            Self::SimpleCodeAction => &["simplecc code action", "代码操作"],
            Self::SimpleCodeLens => &["simplecc code lens refresh", "刷新代码透镜"],
            Self::SimpleCodeLensRun => &["simplecc code lens run", "执行代码透镜"],
            Self::SimpleInlayHints => &["simplecc inlay hints toggle", "行内提示"],
            Self::SimpleDiagnostics => &["simplecc diagnostics problems", "缓冲区诊断"],
            Self::SimpleNextDiagnostic => &["simplecc next diagnostic error", "下一条诊断"],
            Self::SimplePreviousDiagnostic => &["simplecc previous diagnostic error", "上一条诊断"],
            Self::SimpleGitStatus => &["simplegit status", "Git 状态"],
            Self::SimpleGitDiff => &["simplegit diff", "Git 差异"],
            Self::SimpleGitHunk => &["simplegit hunk", "Git 修改块"],
            Self::SimpleMarkdownPreview => &["simplemarkdown preview", "Markdown 预览"],
            Self::SimpleKeys => &["simplewhichkey key hints", "按键提示"],
            Self::SimpleWorkbench => &["simple workbench health", "工作台状态"],
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Vim9WorkbenchGit {
    pub head: String,
    pub ahead: u64,
    pub behind: u64,
    pub added: u64,
    pub changed: u64,
    pub removed: u64,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Vim9WorkbenchDiagnostics {
    pub error: u64,
    pub warning: u64,
    pub info: u64,
    pub hint: u64,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Vim9WorkbenchRemote {
    pub connected: bool,
    pub label: String,
    pub root: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Vim9WorkbenchState {
    pub buffer: i64,
    pub window: i64,
    pub changedtick: u64,
    pub provider: String,
    pub root: String,
    pub native: bool,
    pub large_file: bool,
    pub git: Vim9WorkbenchGit,
    pub diagnostics: Vim9WorkbenchDiagnostics,
    pub remote: Vim9WorkbenchRemote,
}

impl Vim9WorkbenchState {
    pub(super) fn bound(&mut self, buffer: i64, window: i64, changedtick: u64) {
        // A provider cannot attach cached metadata to a different source.
        if self.buffer != buffer || self.window != window || self.changedtick != changedtick {
            *self = Self::default();
            return;
        }
        fn text(value: &mut String, limit: usize) {
            *value = value
                .chars()
                .filter(|c| !c.is_control())
                .take(limit)
                .collect();
        }
        text(&mut self.provider, 64);
        text(&mut self.root, 4096);
        text(&mut self.git.head, 128);
        text(&mut self.remote.label, 256);
        text(&mut self.remote.root, 4096);
        for value in [
            &mut self.git.ahead,
            &mut self.git.behind,
            &mut self.git.added,
            &mut self.git.changed,
            &mut self.git.removed,
            &mut self.diagnostics.error,
            &mut self.diagnostics.warning,
            &mut self.diagnostics.info,
            &mut self.diagnostics.hint,
        ] {
            *value = (*value).min(1_000_000);
        }
    }

    /// Shared compact context for the command palette and optional statusbar.
    /// This only formats already-published caches; it never queries Git/LSP.
    pub fn status_text(&self) -> String {
        if self.provider.is_empty() {
            return String::new();
        }
        let mut parts = vec![if self.provider == "simple" {
            "Simple".to_owned()
        } else {
            self.provider.clone()
        }];
        if !self.git.head.is_empty() {
            let mut branch = self.git.head.clone();
            if self.git.ahead > 0 {
                branch.push_str(&format!(" ↑{}", self.git.ahead));
            }
            if self.git.behind > 0 {
                branch.push_str(&format!(" ↓{}", self.git.behind));
            }
            parts.push(branch);
        }
        if self.git.added > 0 || self.git.changed > 0 || self.git.removed > 0 {
            parts.push(format!(
                "+{} ~{} -{}",
                self.git.added, self.git.changed, self.git.removed
            ));
        }
        let d = &self.diagnostics;
        if d.error > 0 || d.warning > 0 || d.info > 0 || d.hint > 0 {
            let mut counts = Vec::new();
            for (label, count) in [
                ("E", d.error),
                ("W", d.warning),
                ("I", d.info),
                ("H", d.hint),
            ] {
                if count > 0 {
                    counts.push(format!("{label}{count}"));
                }
            }
            parts.push(counts.join(" "));
        }
        if self.large_file {
            parts.push("large file".into());
        }
        if self.remote.connected {
            parts.push(if self.remote.label.is_empty() {
                "remote".into()
            } else {
                format!("remote: {}", self.remote.label)
            });
        }
        parts.join(" · ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_catalog_has_unique_wire_ids_matching_serde() {
        let mut ids = std::collections::HashSet::new();
        for action in Vim9WorkbenchAction::ALL {
            assert!(ids.insert(action.capability()));
            assert_eq!(serde_json::to_value(action).unwrap(), action.capability());
            assert!(!action.title().is_empty());
            assert!(!action.aliases().is_empty());
        }
    }

    #[test]
    fn receiver_bounds_context_and_expires_wrong_sources() {
        let mut state = Vim9WorkbenchState {
            buffer: 3,
            window: 1001,
            changedtick: 7,
            provider: "simple".into(),
            git: Vim9WorkbenchGit {
                head: "中\n".repeat(200),
                added: u64::MAX,
                ..Default::default()
            },
            ..Default::default()
        };
        state.bound(3, 1001, 7);
        assert_eq!(state.git.head.chars().count(), 128);
        assert!(!state.git.head.contains('\n'));
        assert_eq!(state.git.added, 1_000_000);
        state.bound(4, 1001, 7);
        assert_eq!(state, Vim9WorkbenchState::default());
    }

    #[test]
    fn context_is_compact_and_absent_providers_stay_quiet() {
        assert_eq!(Vim9WorkbenchState::default().status_text(), "");
        let state = Vim9WorkbenchState {
            provider: "simple".into(),
            git: Vim9WorkbenchGit {
                head: "main".into(),
                ahead: 2,
                changed: 1,
                ..Default::default()
            },
            diagnostics: Vim9WorkbenchDiagnostics {
                error: 1,
                warning: 2,
                ..Default::default()
            },
            remote: Vim9WorkbenchRemote {
                connected: true,
                label: "dev".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            state.status_text(),
            "Simple · main ↑2 · +0 ~1 -0 · E1 W2 · remote: dev"
        );
    }
}
