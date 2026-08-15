//! Host integration for native and Flatpak launches, and the family's
//! executable lookup.
//!
//! A terminal emulator packaged as Flatpak must not silently start a shell
//! inside the application sandbox. In Flatpak mode, interactive shells and
//! optional helper commands are routed through `flatpak-spawn --host`; native
//! builds keep their existing direct-exec behavior.
//!
//! The lookup half is seeded from anvil `src/pty.rs::resolve_executable` (the
//! family's strongest implementation: `execvp` semantics resolved eagerly so
//! the forked child only has to call `execve`) and ember
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
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const MAX_HOST_PATH_BYTES: usize = 16 * 1024;
const MAX_HOST_COMMAND_NAME_BYTES: usize = 4 * 1024;
const TRUSTED_HELPER_PATH: &str = "/usr/bin:/bin";
// The pre-clamp PATH is preserved for lookup only: the jsh install check
// resolves the user's own jsh through it (`~/.cargo/bin` is the common home)
// while every tool it executes still comes from the clamped PATH.
const HOST_HELPER_LAUNCHER: &str = r#"set -f
JSH_LOOKUP_PATH=${PATH-}
export JSH_LOOKUP_PATH
PATH=/usr/bin:/bin
export PATH
exec "$0" "$@"
"#;

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
    if name.is_empty()
        || name.len() > MAX_HOST_COMMAND_NAME_BYTES
        || name.contains('/')
        || name.contains('\0')
    {
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
/// A bare name is a `PATH` lookup and **never** an implicit `./name`. ember
/// documents why at `src/pty.rs::choose_shell_with_path`: a terminal opens in
/// whatever directory the user is browsing, so a project checkout containing an
/// executable called `bash` would otherwise hijack `shell = "bash"` for anyone
/// who opened a pane there. Anything else — `./name`, `../name`, an absolute
/// path — is taken as the path it looks like and only checked for the execute
/// bit, because it names one specific file the user asked for.
pub fn resolve_configured_program(token: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    if token.is_empty() || token.len() > MAX_HOST_PATH_BYTES || token.contains('\0') {
        return None;
    }
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
/// Hoisted from anvil `src/pty.rs`. Doing this before `fork` is what keeps the
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
    if executable.is_empty()
        || executable.len() > MAX_HOST_PATH_BYTES
        || (!executable.contains('/') && executable.len() > MAX_HOST_COMMAND_NAME_BYTES)
        || executable.contains('\0')
        || child_cwd.is_some_and(|cwd| cwd.len() > MAX_HOST_PATH_BYTES || cwd.contains('\0'))
    {
        return Err(not_executable());
    }
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

/// Resolve the sandbox-side bridge without ever consulting an empty or
/// relative PATH entry. The absolute fallback deliberately fails closed when
/// Flatpak support is unavailable instead of executing a project-local file
/// named `flatpak-spawn`.
fn flatpak_spawn_program() -> PathBuf {
    let conventional = PathBuf::from("/usr/bin/flatpak-spawn");
    if is_executable_file(&conventional) {
        conventional
    } else {
        find_executable_in_path("flatpak-spawn").unwrap_or(conventional)
    }
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
    if path.is_empty() || path.len() > MAX_HOST_PATH_BYTES || path.contains('\0') {
        return false;
    }
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

    let Ok(mut check) = helper_command("test") else {
        return false;
    };
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
fn status_with_timeout(command: Command, timeout: Duration) -> ProbeOutcome {
    match command_status_with_timeout(command, timeout) {
        Ok(Some(status)) => ProbeOutcome::Finished(status.success()),
        Ok(None) => ProbeOutcome::TimedOut,
        Err(error) => {
            log::warn!("host subprocess failed: {error}");
            ProbeOutcome::Finished(false)
        }
    }
}

/// Run a small helper without allowing it or descendants in its process group
/// to outlive a bounded caller. `None` means the timeout elapsed.
///
/// The host probes above share this contract so a missing or wedged Flatpak
/// bridge can never leave an unbounded thread per child.
pub(crate) fn command_status_with_timeout(
    mut command: Command,
    timeout: Duration,
) -> io::Result<Option<ExitStatus>> {
    let deadline = Instant::now() + timeout;
    let mut child = crate::supervised::SupervisedChild::spawn(&mut command)?;
    loop {
        if child.root_has_exited()? {
            return child.reap_after_group_kill().map(Some);
        }
        if Instant::now() >= deadline {
            child.reap_after_group_kill()?;
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(5));
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
        flatpak_spawn_program().to_string_lossy().into_owned(),
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
        let mut command = Command::new(flatpak_spawn_program());
        command.args(["--host", "--watch-bus"]);
        command.arg(program);
        command
    } else {
        Command::new(program)
    }
}

pub fn command_with_cwd(program: impl AsRef<OsStr>, cwd: &Path) -> Command {
    if is_flatpak() {
        let mut command = Command::new(flatpak_spawn_program());
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

fn trusted_helper_program(flatpak: bool, name: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    if name.is_empty()
        || name.len() > MAX_HOST_COMMAND_NAME_BYTES
        || name.contains('/')
        || name.contains('\0')
        || name.chars().any(char::is_control)
    {
        return None;
    }
    if flatpak {
        // An absolute path visible inside the sandbox need not exist on the
        // host. Keep host-side lookup for Flatpak; native launches can and must
        // resolve from absolute PATH entries before changing cwd.
        Some(PathBuf::from(name))
    } else {
        std::env::split_paths(path?).find_map(|directory| {
            if !directory.is_absolute() {
                return None;
            }
            trusted_system_executable(&directory.join(name))
        })
    }
}

/// Resolve an automatic helper to its canonical, non-user-writable target.
///
/// Returning the canonical path is important: validating a symlink in a
/// writable PATH directory and then executing the symlink would leave a race
/// between validation and `execve`. Every namespace component of the resolved
/// target must also be non-writable by this process's user and by group/other.
#[cfg(unix)]
fn trusted_system_executable(candidate: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    fn writable_by_current_user(metadata: &std::fs::Metadata) -> bool {
        let mode = metadata.permissions().mode();
        mode & 0o022 != 0 || (metadata.uid() == unsafe { libc::geteuid() } && mode & 0o200 != 0)
    }

    let canonical = std::fs::canonicalize(candidate).ok()?;
    let metadata = std::fs::metadata(&canonical).ok()?;
    if !metadata.is_file()
        || metadata.permissions().mode() & 0o111 == 0
        || writable_by_current_user(&metadata)
    {
        return None;
    }
    for parent in canonical.ancestors().skip(1) {
        let metadata = std::fs::metadata(parent).ok()?;
        if !metadata.is_dir() || writable_by_current_user(&metadata) {
            return None;
        }
    }
    Some(canonical)
}

#[cfg(not(unix))]
fn trusted_system_executable(candidate: &Path) -> Option<PathBuf> {
    // Automatic helper integrations are Unix-only today. Keep other targets
    // fail-closed until they have an equivalent ownership policy.
    let _ = candidate;
    None
}

/// Construct a command for an application-owned helper. Unlike [`command`],
/// native lookup ignores empty and relative PATH entries: opening a project
/// containing a file named `git`, `curl`, or `notify-send` must never turn a
/// background integration into repository-controlled code execution.
pub(crate) fn helper_command(name: &str) -> io::Result<Command> {
    helper_command_for(
        is_flatpak(),
        name,
        None,
        std::env::var_os("PATH").as_deref(),
    )
}

/// [`helper_command`] with a child working directory.
pub(crate) fn helper_command_with_cwd(name: &str, cwd: &Path) -> io::Result<Command> {
    helper_command_for(
        is_flatpak(),
        name,
        Some(cwd),
        std::env::var_os("PATH").as_deref(),
    )
}

fn helper_command_for(
    flatpak: bool,
    name: &str,
    cwd: Option<&Path>,
    path: Option<&OsStr>,
) -> io::Result<Command> {
    let program = trusted_helper_program(flatpak, name, path).ok_or_else(not_executable)?;
    if !flatpak {
        let mut command = match cwd {
            Some(cwd) => command_with_cwd(program, cwd),
            None => command(program),
        };
        command.env("PATH", TRUSTED_HELPER_PATH);
        return Ok(command);
    }

    // Resolve the helper in the host namespace, but filter empty and relative
    // PATH entries there before exec. A project-local `curl` or `git` must not
    // become trusted merely because the Flatpak bridge changed directory to
    // that project. `/bin/sh` is absolute and is only a small launcher whose
    // script uses shell builtins before replacing itself with the helper.
    let mut command = Command::new(flatpak_spawn_program());
    command.args(["--host", "--watch-bus"]);
    if let Some(cwd) = cwd {
        command.arg(format!("--directory={}", cwd.display()));
    }
    command
        .args(["/bin/sh", "-c", HOST_HELPER_LAUNCHER])
        .arg(program);
    command.env("PATH", TRUSTED_HELPER_PATH);
    Ok(command)
}

pub fn command_available(name: &str) -> bool {
    if name.is_empty()
        || name.len() > MAX_HOST_COMMAND_NAME_BYTES
        || name.contains('/')
        || name.contains('\0')
        || name.chars().any(char::is_control)
    {
        return false;
    }
    if !is_flatpak() {
        return find_executable_in_path(name).is_some();
    }
    if !bridge_available() {
        return false;
    }

    let Ok(mut probe) = helper_command("sh") else {
        return false;
    };
    probe
        .args([
            "-lc",
            "command -v -- \"$1\" >/dev/null 2>&1",
            "jterm-host-probe",
            name,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    matches!(
        status_with_timeout(probe, Duration::from_millis(500)),
        ProbeOutcome::Finished(true)
    )
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

    #[cfg(unix)]
    #[test]
    fn implicit_helpers_cannot_be_hijacked_by_the_child_directory() {
        let root = TestDir::new("trusted-helper");
        root.program("curl", true);

        assert_eq!(
            trusted_helper_program(false, "curl", Some(std::ffi::OsStr::new(":."))),
            None
        );
        assert_eq!(
            trusted_helper_program(false, "curl", Some(&path_var(&[&root.0]))),
            None,
            "an executable in a user-writable absolute PATH directory is still untrusted"
        );
        // Host lookup happens in a different namespace, so Flatpak retains a
        // bare token for the bridge instead of reusing a sandbox path.
        assert_eq!(
            trusted_helper_program(true, "curl", Some(std::ffi::OsStr::new(":."))),
            Some(PathBuf::from("curl"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn native_automatic_helpers_are_canonical_and_use_a_fixed_child_path() {
        let Some(program) =
            trusted_helper_program(false, "sh", Some(std::ffi::OsStr::new("/usr/bin:/bin")))
        else {
            // Non-standard development hosts may not have a system-owned sh.
            return;
        };
        assert!(program.is_absolute());
        assert_eq!(std::fs::canonicalize(&program).unwrap(), program);

        let command = helper_command_for(
            false,
            "sh",
            None,
            Some(std::ffi::OsStr::new("/usr/bin:/bin")),
        )
        .unwrap();
        let child_path = command
            .get_envs()
            .find_map(|(name, value)| (name == "PATH").then_some(value))
            .flatten();
        assert_eq!(child_path, Some(std::ffi::OsStr::new(TRUSTED_HELPER_PATH)));
    }

    #[test]
    fn flatpak_helpers_filter_the_host_path_before_changing_directory() {
        let cwd = Path::new("/tmp/untrusted-project");
        let command =
            helper_command_for(true, "curl", Some(cwd), Some(std::ffi::OsStr::new(":."))).unwrap();
        assert!(Path::new(command.get_program()).is_absolute());
        let arguments = command
            .get_args()
            .map(|value| value.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            arguments[0..3],
            [
                "--host",
                "--watch-bus",
                "--directory=/tmp/untrusted-project"
            ]
        );
        assert_eq!(arguments[3], "/bin/sh");
        assert_eq!(arguments[4], "-c");
        assert_eq!(arguments[5], HOST_HELPER_LAUNCHER);
        assert_eq!(arguments[6], "curl");
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

    #[cfg(unix)]
    #[test]
    fn bounded_status_kills_a_descendant_holding_the_process_alive() {
        let root = TestDir::new("bounded-status-reap");
        let pid_file = root.0.join("root.pid");
        let mut command = Command::new("sh");
        command
            .args([
                std::ffi::OsStr::new("-c"),
                std::ffi::OsStr::new("printf '%s' \"$$\" > \"$1\"; sleep 5 & wait"),
                std::ffi::OsStr::new("jterm-host-test"),
                pid_file.as_os_str(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let started = Instant::now();
        assert!(
            command_status_with_timeout(command, Duration::from_millis(50))
                .unwrap()
                .is_none()
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        let pid = std::fs::read_to_string(pid_file)
            .unwrap()
            .parse::<i32>()
            .unwrap();
        let mut status = 0;
        // SAFETY: status is writable and the bounded runner owned this child.
        assert_eq!(
            unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) },
            -1
        );
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
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
                flatpak_spawn_program().to_string_lossy().into_owned(),
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
    fn flatpak_bridge_argv_never_uses_an_implicit_path_lookup() {
        let wrapped = wrap_argv_for(true, &["sh".to_string()], None, &[]);
        assert!(Path::new(&wrapped[0]).is_absolute());
        assert_eq!(
            Path::new(&wrapped[0]).file_name(),
            Some(OsStr::new("flatpak-spawn"))
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
