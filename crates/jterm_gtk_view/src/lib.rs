//! GTK4 terminal view adapter. The embedding application supplies a PTY owner.
#![forbid(unsafe_code)]

mod command;
mod document;
mod find;
mod finished_output;
mod history_checkpoint;
mod history_search;
mod inline_images;
mod pane;
#[cfg(target_os = "linux")]
mod prepared_pty;
mod pty_dispatch;
#[cfg(target_os = "linux")]
mod runtime_launch;
#[cfg(target_os = "linux")]
mod runtime_shell;
mod text_link;
mod transcript;
mod view;
mod viewport;

pub use command::{
    clear_finished_text_matches, finished_text_at, paint_finished_text_matches,
    select_finished_text_bytes, supports_finished_command, FinishedTextView,
};
pub use document::DocumentView;
pub use find::{FindFrame, FindMatch, FindRegion, FindSnapshot, FindWindow};
pub use finished_output::{scroll_finished_output, FinishedOutputView};
pub use history_checkpoint::HistoryCheckpoint;
pub use inline_images::{InlineImageRequest, PendingInlineImage};
pub use jterm_engine::authenticated_boundaries;
pub use jterm_engine::history_reflow::HistoryAnchor;
pub use jterm_engine::terminal::TranscriptAnchor;
pub use jterm_runtime::exit_status::PtyExit;
#[cfg(target_os = "linux")]
pub use jterm_runtime::shell_identity::ShellIdentityChannel;
#[cfg(target_os = "linux")]
pub use jterm_runtime::startup_status;
pub use jterm_runtime::terminal::{Pty, TerminalHandle, TerminalOutput, TerminalProfile};
pub use pane::{
    CursorColors, EmbeddedTerminal, LinkActions, PaneEvent, PlainLinkFinder, RetainedTextSource,
    TerminalPane, Theme,
};
#[cfg(target_os = "linux")]
pub use prepared_pty::{PreparedPtySession, WriteError};
pub use pty_dispatch::{PtyDispatch, PtyEvent};
#[cfg(target_os = "linux")]
pub use runtime_shell::RuntimeShell;
pub use transcript::{TranscriptRowChrome, TranscriptView, TranscriptZone, TranscriptZoneOrigin};
pub use view::default_ansi_palette;

/// Capabilities advertised by the shared standalone shell profile.
pub const CHILD_ENV: [(&str, &str); 3] = [("TERM", "dumb"), ("VTE_VERSION", ""), ("COLORTERM", "")];

/// Terminfo and color support used by ordinary shared panes and Vim9.
pub const INTERACTIVE_CHILD_ENV: [(&str, &str); 3] = [
    ("TERM", "xterm-256color"),
    ("VTE_VERSION", ""),
    ("COLORTERM", "truecolor"),
];
