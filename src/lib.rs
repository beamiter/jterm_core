//! UI-independent core shared by the jterm family of terminal emulators.
//!
//! Everything here must stay free of GTK/relm4/egui/iced dependencies so any
//! frontend can link it. App-specific branding (notification app name, icon)
//! is injected once at startup via [`identity::init`] instead of hardcoding a
//! binary name in shared code.

pub mod exit_status;
pub mod git_meta;
pub mod host;
pub mod identity;
pub mod notify;
pub mod parser;
pub mod review_input;
