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
}

const DEFAULT: AppIdentity = AppIdentity {
    app_name: "jterm",
    app_id: "io.github.beamiter.jterm",
};

static IDENTITY: OnceLock<AppIdentity> = OnceLock::new();

/// Set the process identity. The first call wins; later calls are ignored.
pub fn init(identity: AppIdentity) {
    let _ = IDENTITY.set(identity);
}

pub fn get() -> &'static AppIdentity {
    IDENTITY.get().unwrap_or(&DEFAULT)
}
