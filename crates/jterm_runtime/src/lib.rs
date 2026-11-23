//! UI-independent terminal runtime helpers.
#![deny(unsafe_code)]
#[cfg(unix)]
#[allow(unsafe_code)] // POSIX wait/signal and Linux process descriptors.
pub mod child_lifecycle;
pub mod exit_status;
pub mod local_shell;
#[cfg(all(target_os = "linux", feature = "linux-pty"))]
#[allow(unsafe_code)] // Linux native fork/exec and private descriptor admission.
mod native_pty_launch;
pub mod output_batch;
#[cfg(unix)]
#[allow(unsafe_code)] // Nonblocking descriptors for separately captured pipes.
pub mod pipe_job;
pub mod process_argv;
pub mod process_stat;
#[cfg(unix)]
#[allow(unsafe_code)] // Unix PTY descriptors, polling and pipe ownership.
pub mod pty_reader;
#[cfg(all(target_os = "linux", feature = "linux-pty"))]
#[allow(unsafe_code)] // Linux PTY descriptors, child ownership and pidfd.
pub mod pty_session;
pub mod runtime_wake;
#[cfg(all(unix, feature = "session-actor"))]
#[allow(unsafe_code)] // POSIX poll, foreground inspection and immediate writes.
pub mod session_actor;
#[cfg(target_os = "linux")]
#[allow(unsafe_code)] // Linux entropy and CLOEXEC pipe ownership.
pub mod shell_identity;
#[cfg(target_os = "linux")]
#[allow(unsafe_code)] // Fork-to-exec status through owned Linux descriptors.
pub mod startup_status;
#[allow(unsafe_code)] // Process-group creation and non-consuming Unix wait.
pub mod supervised;
