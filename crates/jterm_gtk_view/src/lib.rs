//! GTK4 terminal view adapter. The embedding application supplies a PTY owner.
#![forbid(unsafe_code)]

mod command;
mod document;
mod history_search;
mod live_output;
mod pane;
mod pty_dispatch;
#[cfg(target_os = "linux")]
mod runtime_launch;
#[cfg(target_os = "linux")]
mod runtime_shell;
mod text_link;
mod view;
mod viewport;

pub use command::{
    clear_finished_text_matches, paint_finished_text_matches, select_finished_text_bytes,
    supports_finished_command, FinishedTextView,
};
pub use document::DocumentView;
pub use jterm_engine::authenticated_boundaries;
pub use jterm_engine::history_reflow::HistoryAnchor;
pub use jterm_runtime::exit_status::PtyExit;
#[cfg(target_os = "linux")]
pub use jterm_runtime::shell_identity::ShellIdentityChannel;
#[cfg(target_os = "linux")]
pub use jterm_runtime::startup_status;
pub use live_output::{
    captured_live_rows, CapturedLiveRow, LiveOutputRow, LiveOutputView, MAX_LIVE_OUTPUT_BYTES,
};
pub use pane::{
    EmbeddedTerminal, LinkActions, PaneEvent, PlainLinkFinder, Pty, TerminalPane, Theme,
};
pub use pty_dispatch::{PtyDispatch, PtyEvent};
#[cfg(target_os = "linux")]
pub use runtime_shell::RuntimeShell;
pub use view::default_ansi_palette;

/// Capabilities advertised by the shared standalone shell profile.
pub const CHILD_ENV: [(&str, &str); 3] = [("TERM", "dumb"), ("VTE_VERSION", ""), ("COLORTERM", "")];

/// Terminfo and color support used by ordinary shared panes and Vim9.
pub const INTERACTIVE_CHILD_ENV: [(&str, &str); 3] = [
    ("TERM", "xterm-256color"),
    ("VTE_VERSION", ""),
    ("COLORTERM", "truecolor"),
];
