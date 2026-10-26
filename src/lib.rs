//! UI-independent core shared by the jterm family of terminal emulators.
//!
//! Everything here must stay free of GTK/relm4/egui/iced dependencies so any
//! frontend can link it. App-specific branding (notification app name, icon)
//! is injected once at startup via [`identity::init`] instead of hardcoding a
//! binary name in shared code.

pub mod agent;
pub mod agent_sessions;
pub mod agent_task;
pub mod ai;
pub mod atomic_file;
pub mod block_contract;
pub mod bottom_bar;
pub mod bounded_json;
pub mod char_width;
pub mod chat_mode;
pub mod child_env;
pub mod click_cursor;
pub mod command_correction;
pub mod command_history;
pub mod cross_block_search;
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
pub mod organism;
pub mod organism_attention;
pub mod organism_clock;
pub mod organism_companion;
pub mod organism_daily;
pub mod organism_live;
pub mod organism_memory;
pub mod organism_motion;
pub mod organism_watch;
pub mod output_compare;
pub mod output_notice;
pub mod pane_layout;
pub mod parser;
pub mod process;
pub mod pty_input;
pub mod redact;
pub mod review_input;
pub mod screen_replay;
pub mod snapshot_file;
pub mod supervised;
pub mod terminal_report;
pub mod theme;
pub mod vendored_script;
pub mod vim9;
pub mod wheel;
pub mod workflows;

#[cfg(feature = "session-parity-tests")]
pub mod session_parity;
