//! One-command install and update for the companion shell, `jsh`.
//!
//! Every jterm prefers `jsh` over `bash` when it finds one on `PATH`, so a
//! machine without it silently runs the second choice. This module closes that
//! gap without teaching a terminal how to install software: the install logic
//! lives in the jsh repository, and the crate embeds a copy of that script and
//! runs it.
//!
//! Three consequences of that split are load-bearing:
//!   * The script is the only thing that touches the binary, so atomic
//!     replacement, checksum verification, rollback and the "something else on
//!     PATH is named jsh" case are handled in one place for all four terminals.
//!   * The update check reads the installer's own cache, which is shared
//!     across terminals, so several jterms open at once still make one network
//!     call per interval.
//!   * Nothing here draws anything. A frontend asks [`check_blocking`] what the
//!     situation is (off the UI thread), turns it into a [`Prompt`] with
//!     [`prompt_for`], and runs [`install_argv`] in a terminal tab of its own
//!     making.

use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;

/// Vendored from the jsh repository (`scripts/install-jsh.sh`). Embedding it
/// is what lets a machine with no `jsh` at all bootstrap one.
const INSTALLER_SOURCE: &str = include_str!("../scripts/install-jsh.sh");

const SCRIPT_NAME: &str = "install-jsh.sh";
const MAX_CHECK_OUTPUT_BYTES: u64 = 64 * 1024;
const CHECK_PROCESS_TIMEOUT: Duration = Duration::from_secs(310);
const CHECK_PIPE_CLOSE_GRACE: Duration = Duration::from_millis(100);
const CHECK_POLL_INTERVAL: Duration = Duration::from_millis(10);
const MAX_VERSION_LABEL_BYTES: usize = 128;
const MAX_STATUS_ERROR_CHARS: usize = 512;
const MAX_STATUS_PATH_CHARS: usize = 4 * 1024;

/// Seconds a cached update check stays fresh for [`UpdateCheck::Daily`].
pub const DAILY_MAX_AGE: u64 = 86_400;

/// Keeps the tab open after the script finishes. Without it the child exits,
/// the terminal closes the tab, and the user never sees what happened.
const RUN_WRAPPER: &str = r#"set -f
PATH=/usr/bin:/bin
export PATH

/bin/sh "$1"
status=$?
printf '\n'
if [ "${status}" -eq 0 ]; then
    printf 'Done. Press Enter to close this tab.'
else
    printf 'install-jsh.sh exited %s. Press Enter to close this tab.' "${status}"
fi
read -r _
exit "${status}"
"#;

/// When to look for a newer jsh. The check never installs anything; it only
/// decides whether the offer appears.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UpdateCheck {
    /// Ask the network on every launch.
    Startup,
    /// Reuse the installer's shared cache for a day. Several terminals open at
    /// once then still cost one request.
    #[default]
    Daily,
    Never,
}

impl UpdateCheck {
    pub fn as_str(self) -> &'static str {
        match self {
            UpdateCheck::Startup => "startup",
            UpdateCheck::Daily => "daily",
            UpdateCheck::Never => "never",
        }
    }

    pub fn parse(value: &str) -> UpdateCheck {
        if value.len() > 32
            || value.chars().any(char::is_control)
            || crate::review_input::contains_visual_spoofing(value)
        {
            return UpdateCheck::Daily;
        }
        match value.trim().to_ascii_lowercase().as_str() {
            "startup" | "launch" | "always" => UpdateCheck::Startup,
            "never" | "off" | "disabled" => UpdateCheck::Never,
            _ => UpdateCheck::Daily,
        }
    }

    /// Cache age to hand [`check_blocking`], or `None` when no check should run.
    pub fn max_age(self) -> Option<u64> {
        match self {
            UpdateCheck::Startup => Some(0),
            UpdateCheck::Daily => Some(DAILY_MAX_AGE),
            UpdateCheck::Never => None,
        }
    }
}

/// What `install-jsh.sh --check --json` reports.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Status {
    /// Version of the jsh the terminal would manage, when one is installed.
    pub installed: Option<String>,
    pub installed_path: Option<String>,
    /// Newest published version, or `None` when the check could not reach it.
    pub latest: Option<String>,
    pub update_available: bool,
    /// Set when `PATH` resolves `jsh` to a binary that does not identify itself
    /// as this shell.
    pub shadowed_by: Option<String>,
    pub error: Option<String>,
}

impl Status {
    fn from_error(message: impl Into<String>) -> Self {
        Self {
            error: Some(bounded_visible_text(
                &message.into(),
                MAX_STATUS_ERROR_CHARS,
            )),
            ..Self::default()
        }
    }

    fn sanitize_untrusted_text(mut self) -> Self {
        self.installed_path = self
            .installed_path
            .map(|value| bounded_visible_text(&value, MAX_STATUS_PATH_CHARS));
        self.shadowed_by = self
            .shadowed_by
            .map(|value| bounded_visible_text(&value, MAX_STATUS_PATH_CHARS));
        self.error = self
            .error
            .map(|value| bounded_visible_text(&value, MAX_STATUS_ERROR_CHARS));
        self
    }
}

/// What is worth telling the user about, if anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Prompt {
    Missing { latest: String },
    Update { from: String, to: String },
}

impl Prompt {
    /// One line, already carrying this process's own name.
    pub fn banner_title(&self) -> String {
        match self {
            Prompt::Missing { latest } => format!(
                "{} prefers jsh as its shell — jsh {} is available",
                crate::identity::get().app_name,
                bounded_visible_text(latest, MAX_VERSION_LABEL_BYTES)
            ),
            Prompt::Update { from, to } => {
                format!(
                    "jsh {} is available — installed: {}",
                    bounded_visible_text(to, MAX_VERSION_LABEL_BYTES),
                    bounded_visible_text(from, MAX_VERSION_LABEL_BYTES)
                )
            }
        }
    }

    pub fn button_label(&self) -> &'static str {
        match self {
            Prompt::Missing { .. } => "Install",
            Prompt::Update { .. } => "Update",
        }
    }
}

/// Decide whether a check result deserves a prompt. A failed check is never
/// one: an offline laptop must not grow a bar it cannot act on.
///
/// `update_available` is necessary but not sufficient. The flag is computed by
/// the installer, and the installer that answered may be an older vendored copy
/// or a cache entry written by one — and until this round it set the flag on
/// mere string inequality, so a published release *older* than the installed
/// one (a yanked tag, or a source build that ran ahead of the last tag) arrived
/// here as an update. Ordering the versions here as well means a downgrade is
/// never labelled "Update" no matter which installer produced the answer.
pub fn prompt_for(status: &Status) -> Option<Prompt> {
    let latest = status.latest.clone()?;
    if !valid_version_label(&latest) {
        return None;
    }
    match status.installed.clone() {
        None => Some(Prompt::Missing { latest }),
        Some(installed)
            if valid_version_label(&installed)
                && status.update_available
                && version_gt(&latest, &installed) =>
        {
            Some(Prompt::Update {
                from: installed,
                to: latest,
            })
        }
        Some(_) => None,
    }
}

fn valid_version_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_VERSION_LABEL_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+' | b'_'))
}

fn bounded_visible_text(value: &str, max_chars: usize) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_control() || crate::review_input::is_visual_spoofing_character(ch) {
                '\u{fffd}'
            } else {
                ch
            }
        })
        .take(max_chars)
        .collect()
}

/// Is `left` a strictly newer release than `right`?
///
/// The twin of `version_gt` in `scripts/install-jsh.sh` and deliberately as
/// blunt: compare `MAJOR.MINOR.PATCH` numerically, drop any pre-release suffix
/// rather than ordering it, and read an unparsable field as 0 so a version this
/// cannot understand sorts as older instead of being offered.
fn version_gt(left: &str, right: &str) -> bool {
    for index in 0..3 {
        let l = version_field(left, index);
        let r = version_field(right, index);
        if l != r {
            return l > r;
        }
    }
    false
}

/// One dotted field of a version, as a number; 0 when absent or unparsable.
fn version_field(version: &str, index: usize) -> u64 {
    version
        .strip_prefix('v')
        .unwrap_or(version)
        .split('-')
        .next()
        .unwrap_or_default()
        .split('.')
        .nth(index)
        .map(|field| {
            let digits: String = field.chars().take_while(char::is_ascii_digit).collect();
            digits.parse().unwrap_or(0)
        })
        .unwrap_or(0)
}

/// Materialize the embedded installer under the user's cache directory and
/// return its path. Rewritten only when the embedded copy changed, so the path
/// is stable across launches and shared by every terminal on the machine.
///
/// Under Flatpak this lands in the app's own cache directory, which is a real
/// directory in the user's home. The manifests carry `--filesystem=host`, so
/// the host-side `sh` that [`crate::host::command`] spawns sees the same path.
pub fn script_path() -> io::Result<PathBuf> {
    let dir = dirs::cache_dir()
        .ok_or_else(|| io::Error::other("no cache directory for this user"))?
        .join("jterm");
    materialize_script(&dir)
}

fn open_cached_script(path: &Path) -> io::Result<Option<std::fs::File>> {
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    // SAFETY: geteuid has no preconditions and only reads process state.
    if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } || metadata.nlink() != 1
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "cached jsh installer is not a private regular file",
        ));
    }
    Ok(Some(file))
}

fn cached_script_matches(file: std::fs::File) -> io::Result<bool> {
    let declared = file.metadata()?.len();
    if declared != INSTALLER_SOURCE.len() as u64 {
        return Ok(false);
    }
    let mut bytes = Vec::with_capacity(INSTALLER_SOURCE.len());
    file.take(INSTALLER_SOURCE.len() as u64 + 1)
        .read_to_end(&mut bytes)?;
    Ok(bytes == INSTALLER_SOURCE.as_bytes())
}

fn materialize_script(dir: &Path) -> io::Result<PathBuf> {
    crate::snapshot_file::ensure_private_directory(dir)?;
    let path = dir.join(SCRIPT_NAME);

    let current_matches = match open_cached_script(&path) {
        Ok(Some(file)) => cached_script_matches(file)?,
        Ok(None) | Err(_) => false,
    };
    if current_matches {
        let file = open_cached_script(&path)?.ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "cached jsh installer disappeared")
        })?;
        file.set_permissions(std::fs::Permissions::from_mode(0o700))?;
        file.sync_all()?;
        return Ok(path);
    }

    crate::atomic_file::write_atomic(&path, INSTALLER_SOURCE.as_bytes())?;
    let file = open_cached_script(&path)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "cached jsh installer disappeared after publication",
        )
    })?;
    file.set_permissions(std::fs::Permissions::from_mode(0o700))?;
    file.sync_all()?;
    Ok(path)
}

fn bounded_command_stdout(
    command: &mut std::process::Command,
    max_bytes: u64,
) -> io::Result<Vec<u8>> {
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;

    // The installer may spawn curl/wget. A dedicated group lets timeout and
    // oversize paths terminate the whole local tree rather than only `sh`.
    command.process_group(0);
    let mut child = command.spawn()?;
    let Some(mut stdout) = child.stdout.take() else {
        terminate_check_process(&mut child);
        return Err(io::Error::other("jsh installer check stdout was not piped"));
    };
    let descriptor = stdout.as_raw_fd();
    // SAFETY: fcntl only reads/updates flags on the live pipe descriptor.
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        terminate_check_process(&mut child);
        return Err(io::Error::last_os_error());
    }

    let mut bytes = Vec::with_capacity(max_bytes.min(64 * 1024) as usize);
    let mut buffer = [0_u8; 8 * 1024];
    let started = Instant::now();
    let mut status = None;
    let mut exited_at = None;
    let mut eof = false;
    loop {
        loop {
            match stdout.read(&mut buffer) {
                Ok(0) => {
                    eof = true;
                    break;
                }
                Ok(count) => {
                    if bytes.len().saturating_add(count) > max_bytes as usize {
                        terminate_check_process(&mut child);
                        return Err(io::Error::new(
                            io::ErrorKind::FileTooLarge,
                            "jsh installer check output is too large",
                        ));
                    }
                    bytes.extend_from_slice(&buffer[..count]);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => {
                    terminate_check_process(&mut child);
                    return Err(error);
                }
            }
        }
        if status.is_none() {
            match child.try_wait() {
                Ok(Some(exit)) => {
                    status = Some(exit);
                    exited_at = Some(Instant::now());
                }
                Ok(None) => {}
                Err(error) => {
                    terminate_check_process(&mut child);
                    return Err(error);
                }
            }
        }
        if eof && status.is_some() {
            // A non-zero exit still carries the installer's structured error
            // body, so callers parse bytes independently of status.
            return Ok(bytes);
        }
        if exited_at.is_some_and(|exited| exited.elapsed() >= CHECK_PIPE_CLOSE_GRACE) {
            terminate_check_process(&mut child);
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "jsh installer exited while a descendant kept stdout open",
            ));
        }
        if started.elapsed() >= CHECK_PROCESS_TIMEOUT {
            terminate_check_process(&mut child);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "jsh installer check timed out",
            ));
        }
        thread::sleep(CHECK_POLL_INTERVAL);
    }
}

fn terminate_check_process(child: &mut Child) {
    let process_group = child.id() as i32;
    if process_group > 0 {
        // SAFETY: bounded_command_stdout places the child in a fresh process
        // group; a negative target reaches local descendants such as curl.
        let _ = unsafe { libc::kill(-process_group, libc::SIGKILL) };
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Ask the installer what is installed and what is published. Blocking: call it
/// from a worker thread, never from a UI event loop.
///
/// `max_age` is handed to the installer's shared cache; 0 always goes to the
/// network.
pub fn check_blocking(max_age: u64) -> Status {
    let script = match script_path() {
        Ok(path) => path,
        Err(error) => return Status::from_error(format!("cannot write install-jsh.sh: {error}")),
    };

    let mut command = match crate::host::helper_command("sh") {
        Ok(command) => command,
        Err(error) => return Status::from_error(format!("cannot find sh: {error}")),
    };
    command
        .arg(&script)
        .arg("--check")
        .arg("--json")
        .arg("--max-age")
        .arg(max_age.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    match bounded_command_stdout(&mut command, MAX_CHECK_OUTPUT_BYTES) {
        Ok(output) => {
            let text = String::from_utf8_lossy(&output);
            serde_json::from_str::<Status>(text.trim())
                .map(Status::sanitize_untrusted_text)
                .unwrap_or_else(|error| Status::from_error(format!("unreadable check: {error}")))
        }
        Err(error) => Status::from_error(format!("cannot run install-jsh.sh: {error}")),
    }
}

/// argv for the tab that performs the install. The script prints what it is
/// doing, so the tab itself is the progress UI.
///
/// Frontends should launch this the same way they launch any other command,
/// which on Flatpak means passing it through [`crate::host::wrap_argv`].
pub fn install_argv() -> io::Result<Vec<String>> {
    let script = script_path()?;
    Ok(install_argv_for(&script))
}

fn install_argv_for(script: &Path) -> Vec<String> {
    vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        RUN_WRAPPER.to_string(),
        "jterm-jsh-install".to_string(),
        script.to_string_lossy().into_owned(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status_from(json: &str) -> Status {
        serde_json::from_str(json).expect("valid check output")
    }

    #[test]
    fn vendored_installer_still_looks_like_the_script_we_call() {
        assert!(INSTALLER_SOURCE.starts_with("#!/bin/sh"));
        assert!(INSTALLER_SOURCE.contains("--check"));
        assert!(INSTALLER_SOURCE.contains("--max-age"));
        assert!(INSTALLER_SOURCE.contains("\"update_available\""));
    }

    #[cfg(unix)]
    #[test]
    fn cached_installer_is_private_bounded_and_replaces_hostile_entries() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::symlink;

        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "jterm-core-jsh-installer-{}-{id}",
            std::process::id()
        ));
        let path = materialize_script(&dir).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), INSTALLER_SOURCE.as_bytes());
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700
        );

        let victim = dir.join("victim");
        std::fs::write(&victim, b"leave me").unwrap();
        std::fs::remove_file(&path).unwrap();
        symlink(&victim, &path).unwrap();
        materialize_script(&dir).unwrap();
        assert_eq!(std::fs::read(&victim).unwrap(), b"leave me");
        assert_eq!(std::fs::read(&path).unwrap(), INSTALLER_SOURCE.as_bytes());

        std::fs::remove_file(&path).unwrap();
        std::fs::hard_link(&victim, &path).unwrap();
        materialize_script(&dir).unwrap();
        assert_eq!(std::fs::read(&victim).unwrap(), b"leave me");
        assert_eq!(std::fs::read(&path).unwrap(), INSTALLER_SOURCE.as_bytes());

        std::fs::remove_file(&path).unwrap();
        let encoded = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: encoded is a live NUL-terminated path and the mode is valid.
        assert_eq!(unsafe { libc::mkfifo(encoded.as_ptr(), 0o600) }, 0);
        materialize_script(&dir).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), INSTALLER_SOURCE.as_bytes());

        std::fs::write(&path, vec![b'x'; INSTALLER_SOURCE.len() + 1]).unwrap();
        materialize_script(&dir).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), INSTALLER_SOURCE.as_bytes());
        assert!(std::fs::read_dir(&dir).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".tmp.")));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn installer_check_stdout_is_bounded_before_json_parsing() {
        let mut exact = std::process::Command::new("sh");
        exact
            .args(["-c", "printf test"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        assert_eq!(bounded_command_stdout(&mut exact, 4).unwrap(), b"test");

        let mut oversized = std::process::Command::new("sh");
        oversized
            .args(["-c", "printf tests"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        assert_eq!(
            bounded_command_stdout(&mut oversized, 4)
                .unwrap_err()
                .kind(),
            io::ErrorKind::FileTooLarge
        );

        let mut inherited_pipe = std::process::Command::new("sh");
        inherited_pipe
            .args(["-c", "sleep 5 &"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let started = Instant::now();
        assert_eq!(
            bounded_command_stdout(&mut inherited_pipe, 4)
                .unwrap_err()
                .kind(),
            io::ErrorKind::BrokenPipe
        );
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    /// This copy is vendored by hand from the jsh repository, so nothing forces
    /// it to be current — and a stale copy is not inert: it is what computes
    /// `update_available` and what actually installs. Assert on the behaviour
    /// that was wrong, so re-vendoring an older script fails here instead of
    /// silently reintroducing a downgrade offer.
    #[test]
    fn the_vendored_installer_orders_versions_rather_than_comparing_strings() {
        assert!(
            INSTALLER_SOURCE.contains("version_gt()"),
            "vendored install-jsh.sh predates the version-ordering fix; re-vendor it \
             from the jsh repository"
        );
        assert!(INSTALLER_SOURCE.contains("version_gt \"$1\" \"${installed_version}\""));
        // And the install path refuses to walk backwards on a bare run.
        assert!(INSTALLER_SOURCE.contains("is newer than the published"));
    }

    #[test]
    fn update_check_policies_round_trip_and_map_to_cache_ages() {
        for policy in [UpdateCheck::Startup, UpdateCheck::Daily, UpdateCheck::Never] {
            assert_eq!(UpdateCheck::parse(policy.as_str()), policy);
        }
        assert_eq!(UpdateCheck::parse("  NEVER "), UpdateCheck::Never);
        // Unknown values fall back to the conservative default rather than
        // disabling the check silently.
        assert_eq!(UpdateCheck::parse("weekly"), UpdateCheck::Daily);
        assert_eq!(UpdateCheck::Startup.max_age(), Some(0));
        assert_eq!(UpdateCheck::Daily.max_age(), Some(DAILY_MAX_AGE));
        assert_eq!(UpdateCheck::Never.max_age(), None);
        assert_eq!(UpdateCheck::parse(&"x".repeat(1_000)), UpdateCheck::Daily);
    }

    #[test]
    fn a_missing_jsh_is_offered_for_install() {
        let status = status_from(
            r#"{"schema":1,"installed":null,"installed_path":null,"latest":"0.3.0",
                "target":"x86_64-unknown-linux-gnu","update_available":true,
                "shadowed_by":"/usr/bin/jsh","error":null}"#,
        );
        assert_eq!(
            prompt_for(&status),
            Some(Prompt::Missing {
                latest: "0.3.0".into()
            })
        );
        assert_eq!(prompt_for(&status).unwrap().button_label(), "Install");
    }

    #[test]
    fn an_outdated_jsh_is_offered_for_update() {
        let status = status_from(
            r#"{"installed":"0.2.0","installed_path":"/home/u/.local/bin/jsh",
                "latest":"0.3.0","update_available":true}"#,
        );
        assert_eq!(
            prompt_for(&status),
            Some(Prompt::Update {
                from: "0.2.0".into(),
                to: "0.3.0".into()
            })
        );
    }

    #[test]
    fn a_current_install_says_nothing() {
        let status =
            status_from(r#"{"installed":"0.3.0","latest":"0.3.0","update_available":false}"#);
        assert_eq!(prompt_for(&status), None);
    }

    /// A published release older than the installed one is not an update, even
    /// when the answering installer insists it is. Reachable from a yanked tag,
    /// from a build made from source that ran ahead of the last tag, and from a
    /// cache entry written by a pre-fix installer — which set the flag whenever
    /// the two version strings merely differed.
    #[test]
    fn an_older_published_release_is_never_offered_as_an_update() {
        let status =
            status_from(r#"{"installed":"0.4.0","latest":"0.3.9","update_available":true}"#);
        assert_eq!(prompt_for(&status), None);
    }

    /// Versions order numerically, not as text: `0.10.0` is newer than `0.9.0`
    /// though it sorts below it as a string.
    #[test]
    fn versions_order_numerically() {
        assert!(version_gt("0.10.0", "0.9.0"));
        assert!(!version_gt("0.9.0", "0.10.0"));
        assert!(version_gt("1.0.0", "0.99.99"));
        assert!(version_gt("0.3.1", "0.3.0"));
        assert!(!version_gt("0.3.0", "0.3.0"));
        assert!(version_gt("v0.4.0", "0.3.0"));

        // A pre-release suffix is dropped rather than ordered: neither side wins,
        // so nothing is offered. Ordering them needs the full semver rule.
        assert!(!version_gt("0.3.0-rc1", "0.3.0"));
        assert!(!version_gt("0.3.0", "0.3.0-rc1"));

        // Unparsable reads as 0, so a version this cannot understand sorts as
        // older and is never offered over a real one.
        assert!(!version_gt("garbage", "0.1.0"));
        assert!(version_gt("0.1.0", "garbage"));
        assert!(!version_gt("", ""));

        // Missing trailing fields are zero, not an error.
        assert!(version_gt("0.4", "0.3.9"));
        assert!(!version_gt("0.3", "0.3.0"));
    }

    /// The prompt still fires for a genuine newer release whose ordering is only
    /// visible numerically — the case a lexicographic guard would have blocked.
    #[test]
    fn a_numerically_newer_release_is_still_offered() {
        let status =
            status_from(r#"{"installed":"0.9.0","latest":"0.10.0","update_available":true}"#);
        assert_eq!(
            prompt_for(&status),
            Some(Prompt::Update {
                from: "0.9.0".into(),
                to: "0.10.0".into()
            })
        );
    }

    #[test]
    fn an_unreachable_check_says_nothing() {
        // No prompt the user cannot act on, even though nothing is installed.
        let status = status_from(
            r#"{"installed":null,"latest":null,"update_available":false,
                "error":"cannot reach https://github.com/beamiter/jsh/releases"}"#,
        );
        assert_eq!(prompt_for(&status), None);
        assert!(status.error.is_some());
    }

    #[test]
    fn installer_status_text_cannot_inject_log_lines_or_grow_without_bound() {
        let status = Status {
            installed_path: Some(format!("/tmp/jsh\nforged{}", "x".repeat(8 * 1024))),
            shadowed_by: Some("evil\u{202e}txt\rnext".into()),
            error: Some(format!("offline\nforged{}", "x".repeat(2 * 1024))),
            ..Status::default()
        }
        .sanitize_untrusted_text();

        for value in [
            status.installed_path.as_deref().unwrap(),
            status.shadowed_by.as_deref().unwrap(),
            status.error.as_deref().unwrap(),
        ] {
            assert!(!value.chars().any(char::is_control));
            assert!(!crate::review_input::contains_visual_spoofing(value));
        }
        assert_eq!(
            status.installed_path.unwrap().chars().count(),
            MAX_STATUS_PATH_CHARS
        );
        assert_eq!(
            status.error.unwrap().chars().count(),
            MAX_STATUS_ERROR_CHARS
        );
    }

    #[test]
    fn unknown_fields_and_missing_fields_are_tolerated() {
        // The installer may grow fields; an older terminal must keep working.
        let status = status_from(r#"{"latest":"9.9.9","future_field":{"a":1}}"#);
        assert_eq!(
            prompt_for(&status),
            Some(Prompt::Missing {
                latest: "9.9.9".into()
            })
        );
    }

    #[test]
    fn release_labels_cannot_spoof_the_install_banner() {
        for latest in [
            "0.3.0\nforged",
            "0.3.0\u{202e}txt",
            "0.3.0\u{00a0}hidden",
            &"x".repeat(MAX_VERSION_LABEL_BYTES + 1),
        ] {
            let status = Status {
                latest: Some(latest.to_string()),
                update_available: true,
                ..Status::default()
            };
            assert!(prompt_for(&status).is_none(), "{latest:?}");
        }

        // The enum remains public for frontend state, so its rendering sink
        // repeats the sanitisation even for a directly constructed value.
        let title = Prompt::Missing {
            latest: "0.3.0\u{202e}forged".into(),
        }
        .banner_title();
        assert!(!title.contains('\u{202e}'));
        assert!(title.contains('\u{fffd}'));
    }

    #[test]
    fn the_install_tab_waits_for_the_user_before_closing() {
        assert!(RUN_WRAPPER.contains("read -r _"));
        assert!(RUN_WRAPPER.contains("exit \"${status}\""));
        assert!(RUN_WRAPPER.contains("PATH=/usr/bin:/bin"));
        assert!(RUN_WRAPPER.contains("/bin/sh \"$1\""));
        let argv = install_argv_for(Path::new("/tmp/install-jsh.sh"));
        assert_eq!(argv[0], "/bin/sh");
        assert_eq!(argv.last().map(String::as_str), Some("/tmp/install-jsh.sh"));
    }

    /// The riskiest seam is the JSON contract: the installer lives in another
    /// repository and can be re-vendored at any time. Run the embedded script
    /// for real against a local release tree and parse what it prints with the
    /// same type the terminals use.
    #[test]
    fn the_embedded_script_emits_json_this_module_understands() {
        use std::process::Command;

        let downloader_present = Command::new("sh")
            .arg("-c")
            .arg("command -v curl >/dev/null || command -v wget >/dev/null")
            .status()
            .is_ok_and(|status| status.success());
        if !downloader_present {
            eprintln!("skipped: neither curl nor wget is available");
            return;
        }

        let root = std::env::temp_dir().join(format!("jterm-jsh-contract-{}", std::process::id()));
        let release = root.join("release/latest/download");
        let home = root.join("home");
        std::fs::create_dir_all(&release).expect("release tree");
        std::fs::create_dir_all(&home).expect("home");
        std::fs::write(
            release.join("manifest.json"),
            r#"{"schema":1,"name":"jsh","version":"9.9.9","tag":"v9.9.9","artifacts":[]}"#,
        )
        .expect("manifest");

        let script = root.join(SCRIPT_NAME);
        std::fs::write(&script, INSTALLER_SOURCE).expect("script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).expect("mode");

        // Whatever this machine has on PATH, the check must see the same thing:
        // a `jsh` that fails identification.
        let stub_bin = root.join("bin");
        std::fs::create_dir_all(&stub_bin).expect("stub bin");
        let stub_jsh = stub_bin.join("jsh");
        std::fs::write(&stub_jsh, "#!/bin/sh\nexit 1\n").expect("stub jsh");
        std::fs::set_permissions(&stub_jsh, std::fs::Permissions::from_mode(0o755)).expect("mode");
        let path = format!(
            "{}:{}",
            stub_bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );

        let output = Command::new("sh")
            .arg(&script)
            .args(["--check", "--json", "--max-age", "0"])
            .env("HOME", &home)
            .env("XDG_CACHE_HOME", home.join("cache"))
            .env("XDG_STATE_HOME", home.join("state"))
            .env(
                "JSH_INSTALL_BASE_URL",
                format!("file://{}", root.join("release").display()),
            )
            .env("PATH", &path)
            .output()
            .expect("run install-jsh.sh --check");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let status: Status = serde_json::from_str(stdout.trim())
            .unwrap_or_else(|error| panic!("unparseable check output {stdout:?}: {error}"));
        let _ = std::fs::remove_dir_all(&root);

        assert_eq!(status.latest.as_deref(), Some("9.9.9"));
        assert_eq!(status.installed, None);
        assert_eq!(status.error, None);
        assert_eq!(
            status.shadowed_by.as_deref(),
            Some(&*stub_jsh.to_string_lossy())
        );
        assert_eq!(
            prompt_for(&status),
            Some(Prompt::Missing {
                latest: "9.9.9".into()
            })
        );
    }
}
