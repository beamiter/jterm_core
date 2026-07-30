//! Host integration for native and Flatpak launches, and the family's
//! executable lookup.
//!
//! A terminal emulator packaged as Flatpak must not silently start a shell
//! inside the application sandbox. In Flatpak mode, interactive shells and
//! optional helper commands are routed through `flatpak-spawn --host`; native
//! builds keep their existing direct-exec behavior.
//!
//! The lookup half is seeded from jterm1 `src/pty.rs::resolve_executable` (the
//! family's strongest implementation: `execvp` semantics resolved eagerly so
//! the forked child only has to call `execve`) and jterm2
//! `src/pty.rs::choose_shell_with_path` (the bare-name rule for a *configured*
//! shell, and the injectable `PATH` that launchers like wofi make necessary).
//! It exists because [`find_executable_in_path`] — the one shared lookup — used
//! to accept any `PATH` entry whose join was `.is_file()`, with no execute bit
//! and no absolutization, so all four repos kept a private exec-bit helper
//! rather than call it. The permissive function was the shared one; the strict
//! ones were the copies.
//!
//! Family decisions frozen here (do not relitigate per-app):
//!
//! - **A lookup result is executable and absolute.** "Exists" is not the
//!   question any caller was asking: a non-executable `PATH` hit means `execve`
//!   fails with `EACCES` after the fork, at which point the pane is already
//!   open and there is nowhere good to report it.
//! - **[`find_executable_in`] ignores non-absolute `PATH` entries;
//!   [`resolve_executable`] resolves them against the child's cwd.**
//!   `PATH=:/usr/bin` is a real thing found in real login scripts, and `execvp`
//!   reads the empty entry as the current directory. Reproducing that is correct
//!   for a command the user typed, which is what [`resolve_executable`] is for.
//!   For "is `bash` installed?" it is a hijack: the answer would depend on the
//!   directory the user happens to be browsing.
//! - **A configured bare name is never `./name`.** See
//!   [`resolve_configured_program`].

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io;
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

// ---------------------------------------------------------------------------
// Executable resolution
// ---------------------------------------------------------------------------

/// Whether `path` is a regular file with at least one execute bit set.
///
/// Follows symlinks (`metadata`, not `symlink_metadata`) because a `PATH` entry
/// is very often a symlink into `/etc/alternatives` or a Nix store path, and the
/// mode that matters is the one `execve` will look at.
pub fn is_executable_file(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// [`is_executable_file`] plus absolutization against the calling process's cwd,
/// so the result stays valid after the caller (or the child) changes directory.
fn absolute_executable(candidate: &Path) -> Option<PathBuf> {
    if !is_executable_file(candidate) {
        return None;
    }
    if candidate.is_absolute() {
        return Some(candidate.to_path_buf());
    }
    Some(std::env::current_dir().ok()?.join(candidate))
}

/// First entry of `path` holding an executable file named `name`, as an
/// absolute path.
///
/// `path` is a `PATH`-shaped list; `None` means "no search path", not "use the
/// process's own", so a caller working on behalf of a child with a different
/// environment cannot silently fall back to ours. Empty and relative entries are
/// skipped — see the module docs — and a `name` that is empty or contains a
/// separator is rejected outright, since `execvp` would not search `PATH` for it
/// either.
pub fn find_executable_in(name: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    if name.is_empty() || name.contains('/') {
        return None;
    }
    std::env::split_paths(path?)
        .filter(|directory| directory.is_absolute())
        .find_map(|directory| absolute_executable(&directory.join(name)))
}

/// [`find_executable_in`] over the calling process's own `$PATH`.
pub fn find_executable_in_path(name: &str) -> Option<PathBuf> {
    find_executable_in(name, std::env::var_os("PATH").as_deref())
}

/// Resolve a user-configured program token (a `shell = ` setting, an override
/// environment variable) to something safe to exec.
///
/// A bare name is a `PATH` lookup and **never** an implicit `./name`. jterm2
/// documents why at `src/pty.rs::choose_shell_with_path`: a terminal opens in
/// whatever directory the user is browsing, so a project checkout containing an
/// executable called `bash` would otherwise hijack `shell = "bash"` for anyone
/// who opened a pane there. Anything else — `./name`, `../name`, an absolute
/// path — is taken as the path it looks like and only checked for the execute
/// bit, because it names one specific file the user asked for.
pub fn resolve_configured_program(token: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    let candidate = Path::new(token);
    // `file_name()` equals the whole token exactly when the token has no
    // directory part at all. It is None for "", "." and "..", which fall
    // through to the path branch and fail the regular-file check there.
    if candidate.file_name() == Some(OsStr::new(token)) {
        return find_executable_in(token, path);
    }
    absolute_executable(candidate)
}

/// Resolve `executable` the way `execvp` would, but eagerly and with the execute
/// bit checked, so the caller can hand `execve` an absolute path.
///
/// Hoisted from jterm1 `src/pty.rs`. Doing this before `fork` is what keeps the
/// child's post-fork code down to `execve`: a lookup between `fork` and `exec`
/// would have to allocate and read directories in a process where only
/// async-signal-safe calls are legal. Failures are reported to the pane that
/// asked, instead of as a child that exits 127 for no visible reason.
///
/// Relative `path` entries and the empty entry follow `execvp` and are resolved
/// against the directory the child will enter, not the one the caller is in.
pub fn resolve_executable(
    executable: &str,
    path: Option<&OsStr>,
    child_cwd: Option<&str>,
) -> io::Result<PathBuf> {
    let executable_path = Path::new(executable);
    if executable_path.is_absolute() {
        return is_executable_file(executable_path)
            .then(|| executable_path.to_path_buf())
            .ok_or_else(not_executable);
    }

    // A process can outlive the directory it started in. Absolute PATH entries
    // remain usable in that situation, so do not fail the whole pane merely
    // because getcwd can no longer reconstruct the application cwd.
    let current_directory = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let child_directory = child_cwd
        .map(PathBuf::from)
        .map(|directory| {
            if directory.is_absolute() {
                directory
            } else {
                current_directory.join(directory)
            }
        })
        .unwrap_or_else(|| current_directory.clone());
    if executable.contains('/') {
        let candidate = child_directory.join(executable_path);
        return is_executable_file(&candidate)
            .then_some(candidate)
            .ok_or_else(not_executable);
    }

    // execvp's own fallback when the environment has no PATH at all.
    let search_path = path.unwrap_or_else(|| OsStr::new("/bin:/usr/bin"));
    for directory in std::env::split_paths(search_path) {
        let directory = if directory.as_os_str().is_empty() {
            child_directory.clone()
        } else if directory.is_absolute() {
            directory
        } else {
            child_directory.join(directory)
        };
        let candidate = directory.join(executable_path);
        if is_executable_file(&candidate) {
            return Ok(candidate);
        }
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "PTY executable was not found in PATH",
    ))
}

fn not_executable() -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        "PTY executable does not exist or is not executable",
    )
}

pub fn bridge_available() -> bool {
    // The well-known path is checked first because a Flatpak's own PATH does not
    // always contain it; both checks now require the execute bit, since a
    // present-but-unexecutable bridge cannot spawn anything.
    !is_flatpak()
        || is_executable_file(Path::new("/usr/bin/flatpak-spawn"))
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
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        /// A directory under the system temporary directory.
        fn new(label: &str) -> Self {
            Self::under(&std::env::temp_dir(), label)
        }

        fn under(base: &Path, label: &str) -> Self {
            let id = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
            let path = base.join(format!(
                "jterm-core-host-{label}-{}-{id}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn program(&self, name: &str, executable: bool) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, b"#!/bin/sh\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = if executable { 0o755 } else { 0o644 };
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            }
            path
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn path_var(directories: &[&Path]) -> std::ffi::OsString {
        std::env::join_paths(directories.iter().map(|directory| directory.as_os_str())).unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn lookup_requires_the_execute_bit() {
        let root = TestDir::new("exec-bit");
        root.program("prog", false);
        let path = path_var(&[&root.0]);

        assert_eq!(find_executable_in("prog", Some(&path)), None);
        assert!(!is_executable_file(&root.0.join("prog")));

        let executable = root.program("prog", true);
        assert!(is_executable_file(&executable));
        assert_eq!(find_executable_in("prog", Some(&path)), Some(executable));
    }

    #[cfg(unix)]
    #[test]
    fn lookup_rejects_a_directory_with_the_execute_bit() {
        // A directory always has execute bits and `PATH` entries commonly hold
        // same-named directories (`/usr/bin/X11`). Only regular files count.
        let root = TestDir::new("exec-dir");
        std::fs::create_dir(root.0.join("prog")).unwrap();

        assert!(!is_executable_file(&root.0.join("prog")));
        assert_eq!(
            find_executable_in("prog", Some(&path_var(&[&root.0]))),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn lookup_takes_the_first_matching_entry() {
        let first = TestDir::new("first");
        let second = TestDir::new("second");
        first.program("prog", false);
        let winner = second.program("prog", true);
        let path = path_var(&[&first.0, &second.0]);

        // A non-executable earlier hit must not shadow a later usable one: that
        // is what makes the old `.is_file()` check wrong rather than merely lax.
        assert_eq!(find_executable_in("prog", Some(&path)), Some(winner));
    }

    #[cfg(unix)]
    #[test]
    fn lookup_ignores_empty_and_relative_path_entries() {
        let root = TestDir::new("relative-entries");
        let program = root.program("prog", true);
        // "" and "." both mean "wherever the user is browsing", which must never
        // decide whether a command is available.
        let hijackable = std::ffi::OsString::from(format!(":.:{}", root.0.display()));

        assert_eq!(find_executable_in("prog", Some(&hijackable)), Some(program));
        assert_eq!(
            find_executable_in("prog", Some(std::ffi::OsStr::new(":."))),
            None
        );
    }

    #[test]
    fn lookup_refuses_a_name_with_a_separator_or_no_search_path() {
        let root = TestDir::new("no-path");
        root.program("prog", true);
        let path = path_var(&[&root.0]);

        // `../../etc/passwd` through a PATH search is not a lookup, it is a
        // traversal; execvp would not search PATH for it either.
        assert_eq!(find_executable_in("sub/prog", Some(&path)), None);
        assert_eq!(find_executable_in("", Some(&path)), None);
        assert_eq!(find_executable_in("prog", None), None);
    }

    #[cfg(unix)]
    #[test]
    fn configured_bare_name_is_a_path_lookup_and_never_a_local_file() {
        let shells = TestDir::new("configured-path");
        let installed = shells.program("bash", true);
        // The directory the user happens to have open, holding a hostile `bash`.
        let browsed = TestDir::new("configured-cwd");
        browsed.program("bash", true);

        assert_eq!(
            resolve_configured_program("bash", Some(&path_var(&[&shells.0]))),
            Some(installed)
        );
        // Only the supplied search path is consulted; the browsed copy is not
        // reachable by the bare name under any PATH that does not list it.
        assert_eq!(
            resolve_configured_program("bash", Some(&path_var(&[&browsed.0]))),
            Some(browsed.0.join("bash"))
        );
        // No implicit fallback to the process's own PATH either, even though
        // `sh` is certainly installed on the machine running this test.
        assert_eq!(resolve_configured_program("sh", None), None);
        assert!(find_executable_in_path("sh").is_some());
    }

    #[cfg(unix)]
    #[test]
    fn configured_relative_token_resolves_against_the_process_directory() {
        // A "./name" token is only meaningful relative to this process's cwd, so
        // the fixture has to live there. Changing the cwd instead would race
        // every other test in this binary.
        let cwd = std::env::current_dir().unwrap();
        let root = TestDir::under(&cwd, "configured-relative");
        let name = root.0.file_name().unwrap().to_str().unwrap().to_string();
        let program = root.program("prog", true);

        let token = format!("./{name}/prog");
        assert_eq!(resolve_configured_program(&token, None), Some(program));

        root.program("prog", false);
        assert_eq!(resolve_configured_program(&token, None), None);
    }

    #[cfg(unix)]
    #[test]
    fn resolves_absolute_and_slashed_and_bare_and_empty_path_entries() {
        let root = TestDir::new("resolve");
        let bin = TestDir::under(&root.0, "bin");
        let absolute = bin.program("prog", true);
        // `child_cwd` is the directory the child will enter, which is what both
        // the slashed form and an empty PATH entry resolve against.
        let child_cwd = bin.0.to_str().unwrap();
        let path = path_var(&[&bin.0]);

        // 1. absolute
        assert_eq!(
            resolve_executable(absolute.to_str().unwrap(), None, None).unwrap(),
            absolute
        );
        // 2. contains a separator: joined onto the child's cwd, never searched
        assert_eq!(
            resolve_executable("./prog", None, Some(child_cwd)).unwrap(),
            bin.0.join("./prog")
        );
        assert_eq!(
            resolve_executable("./prog", None, Some(root.0.to_str().unwrap()))
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
        // 3. bare name: searched in the supplied PATH
        assert_eq!(
            resolve_executable("prog", Some(&path), None).unwrap(),
            absolute
        );
        // 4. an empty PATH entry is the child's cwd, not the caller's
        let empty_entry = std::ffi::OsString::from(":/nonexistent-jterm-core-dir");
        assert_eq!(
            resolve_executable("prog", Some(&empty_entry), Some(child_cwd)).unwrap(),
            bin.0.join("prog")
        );
        assert_eq!(
            resolve_executable("prog", Some(&empty_entry), Some(root.0.to_str().unwrap()))
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolving_reports_a_present_but_unexecutable_program_as_not_found() {
        let root = TestDir::new("resolve-mode");
        let program = root.program("prog", false);

        // Resolving eagerly is what turns this into an error the pane can show,
        // instead of a child that forks and then exits 127 for no visible
        // reason.
        for outcome in [
            resolve_executable(program.to_str().unwrap(), None, None),
            resolve_executable("prog", Some(&path_var(&[&root.0])), None),
        ] {
            assert_eq!(outcome.unwrap_err().kind(), io::ErrorKind::NotFound);
        }
    }

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
