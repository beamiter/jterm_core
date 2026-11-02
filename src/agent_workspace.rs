//! Shared conversations and a persistent, per-user PTY session host.
//!
//! Frontends call [`run_embedded_if_requested`] before initialising their GUI,
//! launch [`managed_launch`] in their ordinary terminal pane, and use the
//! bounded [`WorkspaceClient`] for database operations. No GUI toolkit, shell
//! interpolation, or provider credentials cross this API.
//!
//! # Lifecycle and ownership
//!
//! The host is started lazily by an explicit workspace command or terminal
//! attachment. It runs outside the GUI's process group, owns each provider's
//! controlling PTY, and keeps a kernel `flock` lease for the entire runtime.
//! Both host and provider retain the lease descriptor. A stale heartbeat or
//! an exited frontend therefore never authorizes a second input writer.
//! PID and kernel process-start identity qualify every recorded owner.
//!
//! Closing an attachment or pressing **Ctrl+]** detaches; the provider keeps
//! running. Another terminal can attach after the old input lease is revoked
//! and previously accepted input has drained. Stop sends TERM, then kills the
//! owned process group before reaping its retained leader. Unknown external
//! processes are conflicts, never silently adopted or killed. A host crash
//! cannot reconstruct a lost PTY; surviving inherited leases block relaunch
//! until the old owner exits. Provider history remains resumable after that.
//!
//! # Shared storage
//!
//! The private database is `workspace.sqlite3` below
//! `$XDG_STATE_HOME/jterm/agent-workspace` (or `~/.local/state/jterm/agent-workspace`).
//! `JTERM_WORKSPACE_STATE_DIR` selects an explicit absolute private directory.
//! One host thread is the only SQLite writer. Provider history files remain
//! authoritative for CLI transcripts; discovery sends bounded metadata only.
//! API conversations use globally unique IDs, append successful turns, and
//! retain older messages outside ChatStore's bounded active context window.
//!
//! ImportChats is transactional and idempotent per legacy application/state
//! namespace and numeric ID. Restored shared numeric IDs are recognized even
//! when a window's persistence filename changes. Pre-existing truncation
//! markers remain explicit: migration cannot recover already deleted turns.
//! New shared conversations must use CreateChat rather than recycling legacy
//! numeric IDs. IndexSessions and Organize keep CLI aliases, pins, virtual
//! archives and tags in source-qualified rows shared by all frontends.
//!
//! SaveChat and SetDraft compare revisions. BeginSubmission acknowledges an
//! exact revision and returns a nonce/epoch/draft receipt **before** a frontend
//! starts its provider request. Complete/Cancel accept that receipt once and
//! never clear a newer draft. Peer credentials identify the submitting GUI;
//! a dead owner's request is revoked without replacing a live process on a
//! timer. API execution belongs to a supervised host worker; views observe
//! sequenced snapshots and detach independently from the transport.
//!
//! # Bounds and supported surface
//!
//! The host supports local Linux, same-UID Unix sockets and at most 16 live
//! provider runtimes. GUI queues, input queues and attachment output queues
//! are bounded. An output consumer that falls behind must reconnect to a
//! fresh viewport snapshot; it cannot backpressure the provider indefinitely.
//! Reattachment restores the primary/alternate text viewports, cursor, common
//! input modes and pending parser bytes. It does not replay historical OSC
//! side effects, terminal images, or the entire terminal scrollback. Viewport
//! snapshots have a 2 MiB wire budget. API history is separately paged in
//! complete user/assistant pairs and never inferred from a terminal screen.

mod api;
mod api_host;
mod client;
mod host;
mod model;
mod native;
mod native_handoff;
mod native_host;
mod native_view;
mod ownership;
mod protocol;
mod store;

pub use api::{
    ApiControlLease, ApiLaunch, ApiObservation, ApiObservationToken, ApiRequest, ApiSnapshot,
    ApiState, HostedApiClient,
};
pub use client::{
    managed_launch, managed_session_running, run_embedded_if_requested, WorkspaceClient,
};
pub use model::*;
pub use native::{
    NativeAction, NativeCatalogEntry, NativeControlLease, NativeLaunch, NativeLaunchContext,
    NativeReportEvent, NativeSnapshot,
};
pub use ownership::NativeSessionLease;
pub use protocol::{WorkspaceCommand, WorkspaceEvent, WorkspaceReply};

/// Host protocol version. Older installed frontends fail clearly instead of
/// interpreting a newer host's records with an incompatible schema.
pub const PROTOCOL_VERSION: u32 = 1;

/// Worker-only location for durable, isolated native provider profiles.
pub fn native_profiles_directory() -> Result<std::path::PathBuf, String> {
    let path = ownership::Paths::discover()?.state.join("native-profiles");
    ownership::private_dir(&path)?;
    Ok(path)
}

/// Tests proving the final close of a kernel lease must create that lease
/// after exec. Other parallel fixtures intentionally pause forked children
/// before exec, where ordinary CLOEXEC descriptors are still inherited.
#[cfg(test)]
pub(crate) fn isolated_lease_test(name: &str) -> bool {
    const PROBE: &str = "JTERM_ISOLATED_LEASE_TEST";
    if std::env::var(PROBE).as_deref() == Ok(name) {
        return false;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture", "--test-threads=1"])
        .env(PROBE, name)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "isolated lease test failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    true
}

pub(crate) fn cleanup_cli_grant(profile: &std::path::Path) -> Result<(), String> {
    native_handoff::cleanup_stale_grant(profile)
}
