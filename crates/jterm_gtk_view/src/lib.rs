//! GTK4 terminal view adapter. The embedding application supplies a PTY owner.
#![forbid(unsafe_code)]

mod command;
mod document;
mod live_output;
mod pane;
mod pty_dispatch;
#[cfg(target_os = "linux")]
mod runtime_launch;
#[cfg(target_os = "linux")]
mod runtime_shell;
mod view;

pub use command::{
    clear_finished_text_matches, paint_finished_text_matches, select_finished_text_bytes,
    supports_finished_command, FinishedTextView,
};
pub use document::DocumentView;
pub use jterm_engine::authenticated_boundaries;
#[cfg(target_os = "linux")]
pub use jterm_runtime::shell_identity::ShellIdentityChannel;
#[cfg(target_os = "linux")]
pub use jterm_runtime::startup_status;
pub use live_output::{captured_live_row, LiveOutputView, MAX_LIVE_OUTPUT_BYTES};
pub use pane::{LinkActions, Pty, TerminalPane, Theme};
pub use pty_dispatch::{PtyDispatch, PtyEvent};
#[cfg(target_os = "linux")]
pub use runtime_shell::RuntimeShell;

/// Capabilities advertised by the shared standalone shell profile.
pub const CHILD_ENV: [(&str, &str); 3] = [("TERM", "dumb"), ("VTE_VERSION", ""), ("COLORTERM", "")];
