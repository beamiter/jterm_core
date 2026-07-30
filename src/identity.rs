//! Process-wide application identity for shared modules.
//!
//! Shared code must not hardcode one binary's name or desktop icon. Each app
//! calls [`init`] once during startup; modules that surface user-visible
//! branding (desktop notifications) read the identity from here. Before
//! `init`, a neutral "jterm" identity applies so shared code never panics.

use std::sync::OnceLock;

pub struct AppIdentity {
    /// Short binary name, e.g. "jterm4". Used as the notification app-name.
    pub app_name: &'static str,
    /// Reverse-DNS application id, e.g. "io.github.beamiter.jterm4". Used as
    /// the desktop icon name.
    pub app_id: &'static str,
    /// The app's own `env!("CARGO_PKG_VERSION")`. Reported to child shells as
    /// `TERM_PROGRAM_VERSION` by [`crate::child_env`]; a tool that feature-gates
    /// on the pair reads a version, never nothing.
    pub app_version: &'static str,
}

const DEFAULT: AppIdentity = AppIdentity {
    app_name: "jterm",
    app_id: "io.github.beamiter.jterm",
    // The core crate's version stands in until an app identifies itself, for
    // the same reason "jterm" stands in for the binary name.
    app_version: env!("CARGO_PKG_VERSION"),
};

static IDENTITY: OnceLock<AppIdentity> = OnceLock::new();

/// Set the process identity. The first call wins; later calls are ignored.
pub fn init(identity: AppIdentity) {
    let _ = IDENTITY.set(identity);
}

pub fn get() -> &'static AppIdentity {
    IDENTITY.get().unwrap_or(&DEFAULT)
}
