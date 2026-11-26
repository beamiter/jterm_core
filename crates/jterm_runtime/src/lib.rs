//! UI-independent terminal runtime helpers.
#![deny(unsafe_code)]
#[cfg(unix)]
#[allow(unsafe_code)] // POSIX wait/signal and Linux process descriptors.
pub mod child_lifecycle;
pub mod exit_status;
#[cfg(unix)]
#[allow(unsafe_code)] // Unix native fork/exec and private descriptor admission.
pub mod native_pty_launch;
pub mod output_batch;
#[cfg(unix)]
#[allow(unsafe_code)] // Nonblocking descriptors for separately captured pipes.
pub mod pipe_job;
pub mod process_argv;
pub mod process_stat;
pub mod pty_instance_lock;
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
#[cfg(unix)]
#[allow(unsafe_code)] // Fork-to-exec status through owned Unix descriptors.
pub mod startup_status;
#[allow(unsafe_code)] // Process-group creation and non-consuming Unix wait.
pub mod supervised;
