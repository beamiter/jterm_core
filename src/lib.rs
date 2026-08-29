//! UI-independent core shared by the jterm family of terminal emulators.
//!
//! Everything here must stay free of GTK/relm4/egui/iced dependencies so any
//! frontend can link it. App-specific branding (notification app name, icon)
//! is injected once at startup via [`identity::init`] instead of hardcoding a
//! binary name in shared code.

pub mod agent;
pub mod ai;
pub mod atomic_file;
pub mod block_contract;
pub mod bottom_bar;
pub mod bounded_json;
pub mod char_width;
pub mod child_env;
pub mod click_cursor;
pub mod command_correction;
pub mod command_history;
pub mod execution_journal;
pub mod exit_status;
pub mod git_meta;
pub mod helper;
pub mod host;
pub mod identity;
pub mod jsh_install;
pub mod jsh_remote;
pub mod keybindings;
pub mod kitty_graphics;
pub mod kitty_keyboard;
pub mod link;
pub mod notebook_text;
pub mod notify;
pub mod pane_layout;
pub mod parser;
pub mod process;
pub mod pty_input;
pub mod redact;
pub mod review_input;
pub mod snapshot_file;
pub mod supervised;
pub mod theme;
pub mod vendored_script;
