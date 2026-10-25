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
    SimpleDocPin,
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
    SimpleGitNextHunk,
    SimpleGitPreviousHunk,
    SimpleGitStageHunk,
    SimpleGitUndoHunk,
    SimpleGitBlame,
    SimpleGitBlameLine,
    SimpleGitHistory,
    SimplePluginStatus,
    SimplePluginCheck,
    SimplePluginInstall,
    SimplePluginUpdate,
    SimplePluginStop,
    SimplePluginDiff,
    SimplePluginProfile,
    SimplePluginHealth,
    SimpleLspStatus,
    SimpleLspRestart,
    SimpleLspLog,
    SimpleTreeRefresh,
    SimpleStatuslineToggle,
    SimpleStatuslineHealth,
    SimpleMarkdownPreview,
    SimpleKeys,
    SimpleDiagnosticsErrors,
    SimpleNextError,
    SimplePreviousError,
    SimpleRefreshDiagnostics,
    SimpleHighlightSymbol,
    SimpleClearHighlights,
    SimpleTerminalSendLine,
    SimpleTerminalList,
    SimpleTerminalHealth,
    SimpleWorkbench,
}

impl Vim9WorkbenchAction {
    pub const ALL: [Self; 64] = [
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
        Self::SimpleDocPin,
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
        Self::SimpleGitNextHunk,
        Self::SimpleGitPreviousHunk,
        Self::SimpleGitStageHunk,
        Self::SimpleGitUndoHunk,
        Self::SimpleGitBlame,
        Self::SimpleGitBlameLine,
        Self::SimpleGitHistory,
        Self::SimplePluginStatus,
        Self::SimplePluginCheck,
        Self::SimplePluginInstall,
        Self::SimplePluginUpdate,
        Self::SimplePluginStop,
        Self::SimplePluginDiff,
        Self::SimplePluginProfile,
        Self::SimplePluginHealth,
        Self::SimpleLspStatus,
        Self::SimpleLspRestart,
        Self::SimpleLspLog,
        Self::SimpleTreeRefresh,
        Self::SimpleStatuslineToggle,
        Self::SimpleStatuslineHealth,
        Self::SimpleMarkdownPreview,
        Self::SimpleKeys,
        Self::SimpleDiagnosticsErrors,
        Self::SimpleNextError,
        Self::SimplePreviousError,
        Self::SimpleRefreshDiagnostics,
        Self::SimpleHighlightSymbol,
        Self::SimpleClearHighlights,
        Self::SimpleTerminalSendLine,
        Self::SimpleTerminalList,
        Self::SimpleTerminalHealth,
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
            Self::SimpleDocPin => "simple_doc_pin",
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
            Self::SimpleGitNextHunk => "simple_git_next_hunk",
            Self::SimpleGitPreviousHunk => "simple_git_previous_hunk",
            Self::SimpleGitStageHunk => "simple_git_stage_hunk",
            Self::SimpleGitUndoHunk => "simple_git_undo_hunk",
            Self::SimpleGitBlame => "simple_git_blame",
            Self::SimpleGitBlameLine => "simple_git_blame_line",
            Self::SimpleGitHistory => "simple_git_history",
            Self::SimplePluginStatus => "simple_plugin_status",
            Self::SimplePluginCheck => "simple_plugin_check",
            Self::SimplePluginInstall => "simple_plugin_install",
            Self::SimplePluginUpdate => "simple_plugin_update",
            Self::SimplePluginStop => "simple_plugin_stop",
            Self::SimplePluginDiff => "simple_plugin_diff",
            Self::SimplePluginProfile => "simple_plugin_profile",
            Self::SimplePluginHealth => "simple_plugin_health",
            Self::SimpleLspStatus => "simple_lsp_status",
            Self::SimpleLspRestart => "simple_lsp_restart",
            Self::SimpleLspLog => "simple_lsp_log",
            Self::SimpleTreeRefresh => "simple_tree_refresh",
            Self::SimpleStatuslineToggle => "simple_statusline_toggle",
            Self::SimpleStatuslineHealth => "simple_statusline_health",
            Self::SimpleMarkdownPreview => "simple_markdown_preview",
            Self::SimpleKeys => "simple_keys",
            Self::SimpleDiagnosticsErrors => "simple_diagnostics_errors",
            Self::SimpleNextError => "simple_next_error",
            Self::SimplePreviousError => "simple_previous_error",
            Self::SimpleRefreshDiagnostics => "simple_refresh_diagnostics",
            Self::SimpleHighlightSymbol => "simple_highlight_symbol",
            Self::SimpleClearHighlights => "simple_clear_highlights",
            Self::SimpleTerminalSendLine => "simple_terminal_send_line",
            Self::SimpleTerminalList => "simple_terminal_list",
            Self::SimpleTerminalHealth => "simple_terminal_health",
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
            Self::SimpleDocPin => "Simple: Pin documentation in a split",
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
            Self::SimpleGitNextHunk => "Simple: Go to next Git hunk",
            Self::SimpleGitPreviousHunk => "Simple: Go to previous Git hunk",
            Self::SimpleGitStageHunk => "Simple: Stage current Git hunk",
            Self::SimpleGitUndoHunk => "Simple: Undo current Git hunk with confirmation",
            Self::SimpleGitBlame => "Simple: Show Git blame",
            Self::SimpleGitBlameLine => "Simple: Inspect Git blame at cursor",
            Self::SimpleGitHistory => "Simple: Show Git file history",
            Self::SimplePluginStatus => "Simple: Show plugin status",
            Self::SimplePluginCheck => "Simple: Check plugin updates",
            Self::SimplePluginInstall => "Simple: Install missing plugins",
            Self::SimplePluginUpdate => "Simple: Update plugins",
            Self::SimplePluginStop => "Simple: Stop plugin operation",
            Self::SimplePluginDiff => "Simple: Review plugin changes",
            Self::SimplePluginProfile => "Simple: Profile plugin startup",
            Self::SimplePluginHealth => "Simple: Check plugin manager health",
            Self::SimpleLspStatus => "Simple: Inspect language service status",
            Self::SimpleLspRestart => "Simple: Restart language service",
            Self::SimpleLspLog => "Simple: Open language service log",
            Self::SimpleTreeRefresh => "Simple: Refresh project tree",
            Self::SimpleStatuslineToggle => "Simple: Toggle Vim statusline",
            Self::SimpleStatuslineHealth => "Simple: Inspect statusline health",
            Self::SimpleMarkdownPreview => "Simple: Toggle Markdown preview",
            Self::SimpleKeys => "Simple: Show key hints",
            Self::SimpleDiagnosticsErrors => "Simple: Show errors in this buffer",
            Self::SimpleNextError => "Simple: Go to next error",
            Self::SimplePreviousError => "Simple: Go to previous error",
            Self::SimpleRefreshDiagnostics => "Simple: Refresh language diagnostics",
            Self::SimpleHighlightSymbol => "Simple: Highlight symbol references",
            Self::SimpleClearHighlights => "Simple: Clear symbol reference highlights",
            Self::SimpleTerminalSendLine => "Simple: Send current line to this workspace terminal",
            Self::SimpleTerminalList => "Simple: List terminal sessions",
            Self::SimpleTerminalHealth => "Simple: Inspect terminal health",
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
            Self::SimpleDocPin => &["simplecc pin documentation split", "固定文档分屏"],
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
            Self::SimpleGitNextHunk => &["simplegit next hunk", "下一处 Git 修改"],
            Self::SimpleGitPreviousHunk => &["simplegit previous hunk", "上一处 Git 修改"],
            Self::SimpleGitStageHunk => &["simplegit stage hunk index", "暂存 Git 修改块"],
            Self::SimpleGitUndoHunk => &["simplegit undo reset hunk confirm", "撤销 Git 修改块"],
            Self::SimpleGitBlame => &["simplegit blame authors", "Git 作者追溯"],
            Self::SimpleGitBlameLine => &["simplegit blame current line", "当前行 Git 作者"],
            Self::SimpleGitHistory => &["simplegit file history log", "Git 文件历史"],
            Self::SimplePluginStatus => &["simpleplug plugins status", "插件状态"],
            Self::SimplePluginCheck => &["simpleplug plugins check updates", "检查插件更新"],
            Self::SimplePluginInstall => &["simpleplug install missing plugins", "安装缺失插件"],
            Self::SimplePluginUpdate => &["simpleplug update plugins", "更新插件"],
            Self::SimplePluginStop => &["simpleplug stop cancel operation", "停止插件任务"],
            Self::SimplePluginDiff => &["simpleplug diff review changes", "查看插件变更"],
            Self::SimplePluginProfile => {
                &["simpleplug profile startup performance", "插件启动性能"]
            }
            Self::SimplePluginHealth => &["simpleplug health diagnostics", "插件管理器健康"],
            Self::SimpleLspStatus => &["simplecc lsp server status", "语言服务状态"],
            Self::SimpleLspRestart => &["simplecc lsp restart server", "重启语言服务"],
            Self::SimpleLspLog => &["simplecc lsp log", "语言服务日志"],
            Self::SimpleTreeRefresh => &["simpletree refresh explorer", "刷新项目目录树"],
            Self::SimpleStatuslineToggle => {
                &["simpleline simplestatusbar toggle statusline", "切换状态栏"]
            }
            Self::SimpleStatuslineHealth => {
                &["simpleline simplestatusbar health statusline", "状态栏健康"]
            }
            Self::SimpleMarkdownPreview => &["simplemarkdown preview", "Markdown 预览"],
            Self::SimpleKeys => &["simplewhichkey key hints", "按键提示"],
            Self::SimpleDiagnosticsErrors => &["simplecc errors diagnostics list", "当前文件错误"],
            Self::SimpleNextError => &["simplecc next error severity", "下一条错误"],
            Self::SimplePreviousError => &["simplecc previous error severity", "上一条错误"],
            Self::SimpleRefreshDiagnostics => {
                &["simplecc refresh pull diagnostics", "刷新语言诊断"]
            }
            Self::SimpleHighlightSymbol => &[
                "simplecc highlight symbol references occurrences",
                "高亮符号引用",
            ],
            Self::SimpleClearHighlights => {
                &["simplecc clear remove symbol highlights", "清除引用高亮"]
            }
            Self::SimpleTerminalSendLine => &[
                "simpleterminal send current line repl execute workspace",
                "发送当前行到项目终端",
            ],
            Self::SimpleTerminalList => &["simpleterminal list sessions shells", "终端会话列表"],
            Self::SimpleTerminalHealth => &["simpleterminal health status", "终端健康检查"],
            Self::SimpleWorkbench => &["simple workbench health", "工作台状态"],
        }
    }

    /// Exact picker groups keep fuzzy matching from leaking unrelated actions
    /// into a provider tab (for example, matching "simplegit" across a title).
    pub(super) fn group(self) -> Option<&'static str> {
        match self {
            Self::SimpleGitStatus
            | Self::SimpleGitDiff
            | Self::SimpleGitHunk
            | Self::SimpleGitNextHunk
            | Self::SimpleGitPreviousHunk
            | Self::SimpleGitStageHunk
            | Self::SimpleGitUndoHunk
            | Self::SimpleGitBlame
            | Self::SimpleGitBlameLine
            | Self::SimpleGitHistory => Some("simplegit"),
            Self::SimpleOutline
            | Self::SimpleImplementation
            | Self::SimpleTypeDefinition
            | Self::SimpleWorkspaceSymbols
            | Self::SimpleIncomingCalls
            | Self::SimpleOutgoingCalls
            | Self::SimpleSupertypes
            | Self::SimpleSubtypes
            | Self::SimpleSignatureHelp
            | Self::SimpleDocPin
            | Self::SimpleRename
            | Self::SimpleCodeAction
            | Self::SimpleCodeLens
            | Self::SimpleCodeLensRun
            | Self::SimpleInlayHints
            | Self::SimpleDiagnostics
            | Self::SimpleNextDiagnostic
            | Self::SimplePreviousDiagnostic
            | Self::SimpleLspStatus
            | Self::SimpleLspRestart
            | Self::SimpleLspLog
            | Self::SimpleDiagnosticsErrors
            | Self::SimpleNextError
            | Self::SimplePreviousError
            | Self::SimpleRefreshDiagnostics
            | Self::SimpleHighlightSymbol
            | Self::SimpleClearHighlights => Some("simplecc"),
            Self::SimpleTreeToggle | Self::SimpleTreeReveal | Self::SimpleTreeRefresh => {
                Some("simpletree")
            }
            Self::SimplePluginStatus
            | Self::SimplePluginCheck
            | Self::SimplePluginInstall
            | Self::SimplePluginUpdate
            | Self::SimplePluginStop
            | Self::SimplePluginDiff
            | Self::SimplePluginProfile
            | Self::SimplePluginHealth => Some("simpleplug"),
            Self::SimpleStatuslineToggle | Self::SimpleStatuslineHealth => Some("simpleline"),
            Self::SimpleTerminalToggle
            | Self::SimpleTerminalNew
            | Self::SimpleTerminalNext
            | Self::SimpleTerminalPrevious
            | Self::SimpleTerminalSendLine
            | Self::SimpleTerminalList
            | Self::SimpleTerminalHealth => Some("simpleterminal"),
            _ => None,
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
pub struct Vim9WorkbenchLsp {
    pub available: bool,
    pub status: String,
}

/// Cached progress/results for one plugin-manager operation, not a claim
/// about the number of plugins currently installed on disk.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Vim9WorkbenchPlugins {
    pub available: bool,
    pub known: bool,
    pub running: bool,
    pub operation: String,
    pub total: u64,
    pub completed: u64,
    pub installed: u64,
    pub updated: u64,
    pub errors: u64,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Vim9WorkbenchTree {
    pub available: bool,
    pub visible: bool,
    pub root: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Vim9WorkbenchStatusline {
    pub available: bool,
    pub enabled: bool,
}

/// On-demand navigation inside the existing native command picker. Queries
/// select finite action groups; no provider command is executed by browsing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Vim9WorkbenchSection {
    pub title: &'static str,
    pub query: &'static str,
    pub summary: String,
}

impl Vim9WorkbenchSection {
    /// Use the palette's explicit `>` mode and exact provider token, including
    /// the user's statusbar alias. A literal filename never selects a group.
    pub fn matches_query(&self, input: &str) -> bool {
        let Some(query) = super::vim9_palette_query(input) else {
            return false;
        };
        let prefix = query.split_whitespace().next().unwrap_or("");
        let prefix = if prefix.eq_ignore_ascii_case("simplestatusbar") {
            "simpleline"
        } else {
            prefix
        };
        self.query
            .strip_prefix('>')
            .is_some_and(|group| prefix.eq_ignore_ascii_case(group))
    }
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
    pub lsp: Vim9WorkbenchLsp,
    pub plugins: Vim9WorkbenchPlugins,
    pub tree: Vim9WorkbenchTree,
    pub statusline: Vim9WorkbenchStatusline,
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
        text(&mut self.lsp.status, 256);
        text(&mut self.plugins.operation, 64);
        text(&mut self.tree.root, 4096);
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
            &mut self.plugins.total,
            &mut self.plugins.completed,
            &mut self.plugins.installed,
            &mut self.plugins.updated,
            &mut self.plugins.errors,
        ] {
            *value = (*value).min(1_000_000);
        }
        // Unavailable/unknown providers must not carry stale progress from a
        // previous session, and a hidden tree is not an active project root.
        if !self.lsp.available {
            self.lsp.status.clear();
        }
        if !self.plugins.available || !self.plugins.known {
            self.plugins = Vim9WorkbenchPlugins {
                available: self.plugins.available,
                ..Default::default()
            };
        } else {
            self.plugins.completed = self.plugins.completed.min(self.plugins.total);
        }
        if !self.tree.available {
            self.tree.visible = false;
            self.tree.root.clear();
        }
        if !self.statusline.available {
            self.statusline.enabled = false;
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
        if self.lsp.available && !self.lsp.status.is_empty() {
            parts.push(format!(
                "LSP {}",
                self.lsp.status.chars().take(64).collect::<String>()
            ));
        }
        if self.plugins.available && self.plugins.known {
            if self.plugins.running {
                parts.push(format!(
                    "Plugins {} {}/{}",
                    self.plugins.operation, self.plugins.completed, self.plugins.total
                ));
            } else if self.plugins.errors > 0 {
                parts.push(format!("Plugins {} errors", self.plugins.errors));
            }
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

    /// Summaries use the exact same bounded, source-bound snapshot as the
    /// editor. Opening the picker never refreshes Git, probes a daemon or
    /// performs a plugin install/update.
    pub fn sections(&self) -> Vec<Vim9WorkbenchSection> {
        if self.provider.is_empty() {
            return Vec::new();
        }
        let git = if self.git.head.is_empty() {
            "No cached Git branch for this buffer".into()
        } else {
            format!(
                "{} · ↑{} ↓{} · +{} ~{} -{}",
                self.git.head,
                self.git.ahead,
                self.git.behind,
                self.git.added,
                self.git.changed,
                self.git.removed
            )
        };
        let code = if self.large_file {
            "Large-file mode · language actions paused".into()
        } else if !self.lsp.available {
            "SimpleCC is not loaded".into()
        } else {
            let status = if self.lsp.status.is_empty() {
                "status not reported"
            } else {
                &self.lsp.status
            };
            format!(
                "{} · E{} W{} I{} H{}",
                status,
                self.diagnostics.error,
                self.diagnostics.warning,
                self.diagnostics.info,
                self.diagnostics.hint
            )
        };
        let tree = if !self.tree.available {
            "Project tree is not loaded".into()
        } else if self.tree.visible {
            if self.tree.root.is_empty() {
                "Project tree open".into()
            } else {
                format!("Open · {}", self.tree.root)
            }
        } else {
            "Project tree closed · reveal or open from this group".into()
        };
        let plugins = if !self.plugins.available {
            "SimplePlug is not loaded".into()
        } else if !self.plugins.known {
            "No cached plugin operation state · inspect status".into()
        } else if self.plugins.running {
            format!(
                "{} · {}/{} completed · {} errors",
                self.plugins.operation,
                self.plugins.completed,
                self.plugins.total,
                self.plugins.errors
            )
        } else if self.plugins.operation.is_empty() {
            "Plugin manager ready · no completed operation in this session".into()
        } else {
            format!(
                "Last {} · {} installed · {} updated · {} errors",
                self.plugins.operation,
                self.plugins.installed,
                self.plugins.updated,
                self.plugins.errors
            )
        };
        let status = if !self.statusline.available {
            "SimpleLine is not loaded".into()
        } else if self.statusline.enabled {
            "SimpleLine enabled · native status follows the same provider caches".into()
        } else {
            "SimpleLine disabled · toggle or inspect from this group".into()
        };
        [
            ("Git", ">simplegit", git),
            ("Code", ">simplecc", code),
            ("Tree", ">simpletree", tree),
            ("Plugins", ">simpleplug", plugins),
            ("Status", ">simpleline", status),
        ]
        .into_iter()
        .map(|(title, query, summary)| Vim9WorkbenchSection {
            title,
            query,
            // Long workspace paths must not push all command rows out of a
            // small picker. The full value remains in the workbench state.
            summary: if summary.chars().count() > 256 {
                summary
                    .chars()
                    .take(255)
                    .chain(std::iter::once('…'))
                    .collect()
            } else {
                summary
            },
        })
        .collect()
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

    #[test]
    fn provider_sections_distinguish_absent_unknown_running_and_completed_state() {
        assert!(Vim9WorkbenchState::default().sections().is_empty());
        let mut state: Vim9WorkbenchState = serde_json::from_value(serde_json::json!({
            "provider": "simple", "buffer": 3, "window": 1001, "changedtick": 7,
            "plugins": {"available": true}
        }))
        .unwrap();
        state.bound(3, 1001, 7);
        assert_eq!(state.sections().len(), 5);
        assert!(state.sections()[3].summary.contains("No cached"));
        assert!(state.sections()[4].summary.contains("not loaded"));
        assert!(!state.status_text().contains("errors"));
        state.plugins = Vim9WorkbenchPlugins {
            available: true,
            known: true,
            running: true,
            operation: "update".into(),
            total: 8,
            completed: 3,
            installed: 1,
            updated: 2,
            errors: 1,
        };
        state.lsp = Vim9WorkbenchLsp {
            available: true,
            status: "ready".into(),
        };
        state.statusline = Vim9WorkbenchStatusline {
            available: true,
            enabled: false,
        };
        assert!(state.sections()[3].summary.contains("3/8 completed"));
        assert!(state.status_text().contains("Plugins update 3/8"));
        assert!(state.status_text().contains("LSP ready"));
        assert!(state.sections()[4].summary.contains("disabled"));
        state.plugins.running = false;
        assert!(state.sections()[3].summary.contains("Last update"));
        assert!(state.status_text().contains("Plugins 1 errors"));
        state.large_file = true;
        assert!(state.sections()[1].summary.contains("Large-file mode"));
    }

    #[test]
    fn section_selection_uses_the_palette_mode_token_and_alias_rules() {
        let state = Vim9WorkbenchState {
            provider: "simple".into(),
            ..Default::default()
        };
        let sections = state.sections();
        assert!(sections[0].matches_query(">simplegit"));
        assert!(sections[0].matches_query("  > SIMPLEGIT stage "));
        assert!(sections[4].matches_query(">simplestatusbar health"));
        for query in [
            ">simplegitx",
            "simplegit",
            "./>simplegit",
            ">simpleplug",
            ">",
        ] {
            assert!(!sections[0].matches_query(query), "{query}");
        }
    }

    #[test]
    fn provider_metadata_is_bounded_and_stale_progress_cannot_claim_to_be_live() {
        let mut state = Vim9WorkbenchState {
            buffer: 3,
            window: 1001,
            changedtick: 7,
            provider: "simple".into(),
            plugins: Vim9WorkbenchPlugins {
                available: true,
                known: true,
                running: true,
                operation: "更\n".repeat(100),
                total: 8,
                completed: u64::MAX,
                installed: u64::MAX,
                updated: u64::MAX,
                errors: u64::MAX,
            },
            lsp: Vim9WorkbenchLsp {
                available: true,
                status: "就\n".repeat(400),
            },
            tree: Vim9WorkbenchTree {
                available: true,
                visible: true,
                root: "根\n".repeat(5000),
            },
            ..Default::default()
        };
        state.bound(3, 1001, 7);
        assert_eq!(state.plugins.operation.chars().count(), 64);
        assert_eq!(state.plugins.completed, 8);
        assert_eq!(state.plugins.errors, 1_000_000);
        assert_eq!(state.lsp.status.chars().count(), 256);
        assert_eq!(state.tree.root.chars().count(), 4096);
        assert!(!state
            .sections()
            .iter()
            .any(|section| section.summary.contains('\n')));
        assert!(state
            .sections()
            .iter()
            .all(|section| section.summary.chars().count() <= 256));
        state.plugins.known = false;
        state.lsp.available = false;
        state.tree.available = false;
        state.bound(3, 1001, 7);
        assert!(!state.plugins.running);
        assert_eq!(state.plugins.total, 0);
        assert!(state.plugins.operation.is_empty());
        assert!(state.lsp.status.is_empty());
        assert!(!state.tree.visible);
        assert!(state.sections()[3].summary.contains("No cached"));
        state.bound(3, 1001, 8);
        assert!(state.sections().is_empty());
    }
}
