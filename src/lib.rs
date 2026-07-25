//! UI-independent core shared by the jterm family of terminal emulators.
//!
//! Everything here must stay free of GTK/relm4/egui/iced dependencies so any
//! frontend can link it. App-specific branding (notification app name, icon)
//! is injected once at startup via [`identity::init`] instead of hardcoding a
//! binary name in shared code.

pub mod agent;
pub mod ai;
pub mod atomic_file;
pub mod char_width;
pub mod command_history;
pub mod exit_status;
pub mod git_meta;
pub mod host;
pub mod identity;
pub mod notebook_text;
pub mod notify;
pub mod pane_layout;
pub mod parser;
pub mod redact;
pub mod review_input;
