//! Host integration for native and Flatpak launches.
//!
//! A terminal emulator packaged as Flatpak must not silently start a shell
//! inside the application sandbox. In Flatpak mode, interactive shells and
//! optional helper commands are routed through `flatpak-spawn --host`; native
//! builds keep their existing direct-exec behavior.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

pub fn is_flatpak() -> bool {
    static VALUE: OnceLock<bool> = OnceLock::new();
    *VALUE.get_or_init(|| {
        std::env::var_os("FLATPAK_ID").is_some() || Path::new("/.flatpak-info").is_file()
    })
}

/// First `$PATH` entry containing an executable file named `name`.
pub fn find_executable_in_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|directory| directory.join(name))
            .find(|candidate| candidate.is_file())
    })
}

pub fn bridge_available() -> bool {
    !is_flatpak()
        || Path::new("/usr/bin/flatpak-spawn").is_file()
        || find_executable_in_path("flatpak-spawn").is_some()
}

#[derive(Default)]
struct CwdProbeCache {
    entries: HashMap<String, (Instant, bool)>,
    bridge_timeout_until: Option<Instant>,
}

#[derive(Clone, Copy)]
enum ProbeOutcome {
    Finished(bool),
    TimedOut,
}

/// Check a requested terminal cwd in the same filesystem namespace where the
/// child will run. A Flatpak may not be able to stat an otherwise valid host
/// directory directly, so ask the host bridge instead.
pub fn working_directory_available(path: &str) -> bool {
    if !is_flatpak() {
        return Path::new(path).is_dir();
    }

    const CACHE_TTL: Duration = Duration::from_secs(2);
    const MAX_CACHE_ENTRIES: usize = 256;
    static CACHE: OnceLock<Mutex<CwdProbeCache>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(CwdProbeCache::default()));
    if let Ok(cache) = cache.lock() {
        if cache
            .bridge_timeout_until
            .is_some_and(|until| until > Instant::now())
        {
            return false;
        }
        if let Some((checked_at, available)) = cache.entries.get(path) {
            if checked_at.elapsed() < CACHE_TTL {
                return *available;
            }
        }
    }

    let mut check = command("test");
    check
        .args(["-d", path])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let outcome = status_with_timeout(check, Duration::from_millis(250));
    let available = matches!(outcome, ProbeOutcome::Finished(true));

    if let Ok(mut cache) = cache.lock() {
        cache
            .entries
            .retain(|_, (checked_at, _)| checked_at.elapsed() < CACHE_TTL);
        if cache.entries.len() >= MAX_CACHE_ENTRIES {
            cache.entries.clear();
        }
        if matches!(outcome, ProbeOutcome::TimedOut) {
            // One wedged bridge implies every immediately following path probe
            // would hit the same timeout. Bound an N-pane restore to one wait.
            cache.bridge_timeout_until = Some(Instant::now() + CACHE_TTL);
        } else {
            cache.bridge_timeout_until = None;
        }
        cache
            .entries
            .insert(path.to_string(), (Instant::now(), available));
    }
    available
}

/// Wait for a small host probe without ever letting a missing or wedged bridge
/// freeze the UI main thread indefinitely.
fn status_with_timeout(mut command: Command, timeout: Duration) -> ProbeOutcome {
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            log::warn!("failed to start host working-directory probe: {error}");
            return ProbeOutcome::Finished(false);
        }
    };
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return ProbeOutcome::Finished(status.success()),
            Ok(None) if started.elapsed() < timeout => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(None) => {
                log::warn!("host working-directory probe timed out");
                terminate_probe(child);
                return ProbeOutcome::TimedOut;
            }
            Err(error) => {
                log::warn!("host working-directory probe failed: {error}");
                terminate_probe(child);
                return ProbeOutcome::Finished(false);
            }
        }
    }
}

fn terminate_probe(mut child: std::process::Child) {
    let _ = child.kill();
    let deadline = Instant::now() + Duration::from_millis(50);
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) => std::thread::sleep(Duration::from_millis(2)),
        }
    }
    if let Err(error) = std::thread::Builder::new()
        .name("jterm-host-probe-reaper".to_string())
        .spawn(move || {
            let _ = child.wait();
        })
    {
        log::warn!("failed to start host-probe reaper: {error}");
    }
}

fn wrap_argv_for(
    flatpak: bool,
    argv: &[String],
    cwd: Option<&str>,
    env_extra: &[(&str, &str)],
) -> Vec<String> {
    if !flatpak {
        return argv.to_vec();
    }

    let mut wrapped = vec![
        "flatpak-spawn".to_string(),
        "--host".to_string(),
        "--watch-bus".to_string(),
    ];
    if let Some(cwd) = cwd.filter(|value| !value.is_empty()) {
        wrapped.push(format!("--directory={cwd}"));
    }
    // The host child starts from the host session's environment, so every
    // variable that identifies *this* terminal has to be spelled out. This used
    // to be a lone TERM, which is how a Flatpak jterm advertised 256 colours to
    // tools that would have used 24-bit ones.
    wrapped.extend(crate::child_env::host_bridge_args(
        &crate::child_env::ChildEnv::from_identity(),
        env_extra,
    ));
    wrapped.extend(argv.iter().cloned());
    wrapped
}

pub fn wrap_argv(argv: &[String], cwd: Option<&str>, env_extra: &[(&str, &str)]) -> Vec<String> {
    wrap_argv_for(is_flatpak(), argv, cwd, env_extra)
}

/// Whether an argv is a `flatpak-spawn --host` wrapper produced by
/// [`wrap_argv`] (or equivalent).
pub fn is_host_wrapper_argv(args: &[String]) -> bool {
    args.first().is_some_and(|command| {
        Path::new(command)
            .file_name()
            .is_some_and(|name| name == "flatpak-spawn")
    }) && args.iter().any(|argument| argument == "--host")
}

/// Reverse of [`wrap_argv`]: skip the `flatpak-spawn --host` prefix and the
/// exact option forms that wrapper emits, recovering the host command argv so
/// process-based checks work identically inside and outside the sandbox.
/// Non-wrapper argv is returned unchanged.
pub fn unwrap_host_argv(args: &[String]) -> &[String] {
    if !is_host_wrapper_argv(args) {
        return args;
    }
    let start = args
        .iter()
        .enumerate()
        .skip(1)
        .find(|(_, argument)| {
            !matches!(argument.as_str(), "--host" | "--watch-bus")
                && !argument.starts_with("--directory=")
                && !argument.starts_with("--env=")
        })
        .map(|(index, _)| index)
        .unwrap_or(args.len());
    &args[start..]
}

pub fn command(program: impl AsRef<OsStr>) -> Command {
    if is_flatpak() {
        let mut command = Command::new("flatpak-spawn");
        command.args(["--host", "--watch-bus"]);
        command.arg(program);
        command
    } else {
        Command::new(program)
    }
}

pub fn command_with_cwd(program: impl AsRef<OsStr>, cwd: &Path) -> Command {
    if is_flatpak() {
        let mut command = Command::new("flatpak-spawn");
        command.args(["--host", "--watch-bus"]);
        command.arg(format!("--directory={}", cwd.display()));
        command.arg(program);
        command
    } else {
        let mut command = Command::new(program);
        command.current_dir(cwd);
        command
    }
}

pub fn command_available(name: &str) -> bool {
    if !is_flatpak() {
        return find_executable_in_path(name).is_some();
    }
    if !bridge_available() {
        return false;
    }

    command("sh")
        .args([
            "-lc",
            "command -v -- \"$1\" >/dev/null 2>&1",
            "jterm-host-probe",
            name,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_argv_is_unchanged() {
        let argv = vec!["bash".to_string(), "-l".to_string()];
        assert_eq!(
            wrap_argv_for(false, &argv, Some("/tmp"), &[("LESS", "R")]),
            argv
        );
    }

    #[test]
    fn flatpak_argv_routes_cwd_and_environment_to_host() {
        let argv = vec!["bash".to_string(), "-l".to_string()];
        // The environment block is whatever `child_env` reports for the process
        // identity — TERM alone was the bug, not the contract. Tests never call
        // `identity::init`, so the neutral "jterm" identity holds here.
        assert_eq!(
            wrap_argv_for(true, &argv, Some("/home/alice/project"), &[("LESS", "R")]),
            vec![
                "flatpak-spawn".to_string(),
                "--host".to_string(),
                "--watch-bus".to_string(),
                "--directory=/home/alice/project".to_string(),
                "--env=TERM=xterm-256color".to_string(),
                "--env=COLORTERM=truecolor".to_string(),
                "--env=TERM_PROGRAM=jterm".to_string(),
                format!("--env=TERM_PROGRAM_VERSION={}", env!("CARGO_PKG_VERSION")),
                "--env=VTE_VERSION=7802".to_string(),
                "--env=LESS=R".to_string(),
                "bash".to_string(),
                "-l".to_string(),
            ]
        );
    }

    #[test]
    fn flatpak_shell_identity_reaches_the_host_child() {
        let argv = vec!["bash".to_string(), "-l".to_string()];
        let wrapped = wrap_argv_for(true, &argv, None, &[("TERM_PROGRAM", "jterm")]);
        assert!(wrapped
            .iter()
            .any(|argument| argument == "--env=TERM_PROGRAM=jterm"));
        assert_eq!(&wrapped[wrapped.len() - 2..], ["bash", "-l"]);
    }
}
