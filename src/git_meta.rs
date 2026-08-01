//! Coalesced background Git metadata for the active-pane strip.
//!
//! Callers run on the GTK main thread, so they never execute Git directly. A
//! single worker owns all `git status --porcelain=v2 --branch` processes,
//! coalesces concurrent requests for the same directory, and caches the latest
//! result. Slow repositories return a stale value after a short UI wait while
//! the refresh continues in the background.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Stdio};
#[cfg(not(unix))]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

const GIT_STATUS_TIMEOUT: Duration = Duration::from_millis(500);
const GIT_WAIT_POLL: Duration = Duration::from_millis(5);
const MAX_GIT_STATUS_OUTPUT_BYTES: u64 = 512 * 1024;
const MAX_GIT_CWD_BYTES: usize = 16 * 1024;
const MAX_BRANCH_DISPLAY_CHARS: usize = 256;
const MAX_QUEUED_GIT_PROBES: usize = 64;
const MAX_GIT_CACHE_ENTRIES: usize = 256;
const MAX_WAITERS_PER_PATH: usize = 32;
const UNTRUSTED_GIT_ENVIRONMENT: &[&str] = &[
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG_PARAMETERS",
    "GIT_DIR",
    "GIT_EXEC_PATH",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_REPLACE_REF_BASE",
    "GIT_TRACE",
    "GIT_TRACE_CURL",
    "GIT_TRACE_CURL_NO_DATA",
    "GIT_TRACE_PACK_ACCESS",
    "GIT_TRACE_PACKET",
    "GIT_TRACE_PERFORMANCE",
    "GIT_TRACE_SETUP",
    "GIT_TRACE_SHALLOW",
    "GIT_TRACE2",
    "GIT_TRACE2_CONFIG_PARAMS",
    "GIT_TRACE2_EVENT",
    "GIT_TRACE2_PERF",
    "GIT_WORK_TREE",
];
/// Keep first paint responsive on slow disks, remote mounts, and large repos.
const UI_WAIT_BUDGET: Duration = Duration::from_millis(12);

#[cfg(not(unix))]
static PORTABLE_READER_ACTIVE: AtomicBool = AtomicBool::new(false);

#[cfg(not(unix))]
struct PortableReaderGuard;

#[cfg(not(unix))]
impl Drop for PortableReaderGuard {
    fn drop(&mut self) {
        PORTABLE_READER_ACTIVE.store(false, Ordering::Release);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoMeta {
    /// Short branch name, or detached-HEAD short sha.
    pub branch: String,
    /// True if there are any uncommitted changes (tracked or untracked).
    pub dirty: bool,
    /// Commits on local branch not yet on upstream. None if no upstream.
    pub ahead: Option<u32>,
    /// Commits on upstream not yet locally. None if no upstream.
    pub behind: Option<u32>,
}

type ProbeResult = Option<RepoMeta>;
type ReplySender = mpsc::SyncSender<ProbeResult>;

/// One worker serializes probes and prevents a process storm when several panes
/// finish output together. Requests for the same cwd share one in-flight probe.
struct GitMetaService {
    request_tx: mpsc::SyncSender<PathBuf>,
    cache: Arc<Mutex<HashMap<PathBuf, ProbeResult>>>,
    pending: Arc<Mutex<HashMap<PathBuf, Vec<ReplySender>>>>,
}

impl GitMetaService {
    fn new() -> Option<Self> {
        let (request_tx, request_rx) = mpsc::sync_channel::<PathBuf>(MAX_QUEUED_GIT_PROBES);
        let cache = Arc::new(Mutex::new(HashMap::new()));
        let pending = Arc::new(Mutex::new(HashMap::<PathBuf, Vec<ReplySender>>::new()));

        let cache_worker = cache.clone();
        let pending_worker = pending.clone();
        thread::Builder::new()
            .name("jterm-git-meta-worker".to_string())
            .spawn(move || worker_loop(request_rx, &cache_worker, &pending_worker))
            .ok()?;

        Some(Self {
            request_tx,
            cache,
            pending,
        })
    }

    fn cached(&self, path: &Path) -> Option<ProbeResult> {
        self.cache.lock().ok()?.get(path).cloned()
    }

    fn request(&self, path: &Path) -> Option<mpsc::Receiver<ProbeResult>> {
        let path = path.to_path_buf();
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);

        {
            let mut pending = self.pending.lock().ok()?;
            if let Some(waiters) = pending.get_mut(&path) {
                if waiters.len() >= MAX_WAITERS_PER_PATH {
                    return None;
                }
                waiters.push(reply_tx);
                return Some(reply_rx);
            }
            pending.insert(path.clone(), vec![reply_tx]);
        }

        if self.request_tx.try_send(path.clone()).is_err() {
            if let Ok(mut pending) = self.pending.lock() {
                pending.remove(&path);
            }
            return None;
        }
        Some(reply_rx)
    }
}

fn service() -> Option<&'static GitMetaService> {
    static SERVICE: OnceLock<Option<GitMetaService>> = OnceLock::new();
    SERVICE.get_or_init(GitMetaService::new).as_ref()
}

fn worker_loop(
    request_rx: mpsc::Receiver<PathBuf>,
    cache: &Mutex<HashMap<PathBuf, ProbeResult>>,
    pending: &Mutex<HashMap<PathBuf, Vec<ReplySender>>>,
) {
    for path in request_rx {
        let result = read_uncached(&path);

        if let Ok(mut cache) = cache.lock() {
            if !cache.contains_key(&path) && cache.len() >= MAX_GIT_CACHE_ENTRIES {
                if let Some(evicted) = cache.keys().next().cloned() {
                    cache.remove(&evicted);
                }
            }
            cache.insert(path.clone(), result.clone());
        }

        let waiters = pending
            .lock()
            .ok()
            .and_then(|mut pending| pending.remove(&path))
            .unwrap_or_default();
        for waiter in waiters {
            let _ = waiter.send(result.clone());
        }
    }
}

/// Resolve repo metadata without running Git on the caller's thread.
///
/// A healthy local repository usually completes inside `UI_WAIT_BUDGET`. A slow
/// refresh returns the cached value while the worker continues in the background.
pub fn read(cwd: &Path) -> Option<RepoMeta> {
    // The path becomes both a queue key and a long-lived cache key. Bound it
    // before either cloning or touching the filesystem so a protocol-fed cwd
    // cannot turn 256 otherwise-bounded cache entries into unbounded memory.
    if cwd.as_os_str().as_encoded_bytes().len() > MAX_GIT_CWD_BYTES
        || cwd.as_os_str().as_encoded_bytes().contains(&0)
        || !cwd.is_dir()
    {
        return None;
    }

    let service = service()?;
    let stale = service.cached(cwd).flatten();
    let Some(reply) = service.request(cwd) else {
        return stale;
    };

    match reply.recv_timeout(UI_WAIT_BUDGET) {
        Ok(fresh) => fresh,
        Err(mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected) => stale,
    }
}

fn read_uncached(cwd: &Path) -> ProbeResult {
    let output = run_git_status(cwd)?;
    parse_porcelain_v2(&output)
}

/// Parse stable porcelain-v2 branch headers and worktree records.
fn parse_porcelain_v2(output: &str) -> Option<RepoMeta> {
    let mut oid: Option<&str> = None;
    let mut head: Option<&str> = None;
    let mut ahead_behind: Option<(u32, u32)> = None;
    let mut dirty = false;

    for line in output.lines() {
        if let Some(value) = line.strip_prefix("# branch.oid ") {
            oid = Some(value.trim());
            continue;
        }
        if let Some(value) = line.strip_prefix("# branch.head ") {
            head = Some(value.trim());
            continue;
        }
        if let Some(value) = line.strip_prefix("# branch.ab ") {
            ahead_behind = parse_ahead_behind(value);
            continue;
        }

        if matches!(line.as_bytes().first(), Some(b'1' | b'2' | b'u' | b'?')) {
            dirty = true;
        }
    }

    let head = head?;
    let branch = if head == "(detached)" {
        let oid = oid.filter(|value| *value != "(initial)")?.trim();
        if oid.len() < 7 || !oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        format!("({})", &oid[..7])
    } else {
        sanitize_branch_for_display(head)?
    };
    let (ahead, behind) = match ahead_behind {
        Some((ahead, behind)) => (Some(ahead), Some(behind)),
        None => (None, None),
    };

    Some(RepoMeta {
        branch,
        dirty,
        ahead,
        behind,
    })
}

fn sanitize_branch_for_display(branch: &str) -> Option<String> {
    let branch = branch.trim();
    if branch.is_empty() {
        return None;
    }

    let mut safe = String::new();
    let mut chars = branch.chars();
    for ch in chars.by_ref().take(MAX_BRANCH_DISPLAY_CHARS) {
        if ch.is_control() || crate::review_input::is_visual_spoofing_character(ch) {
            safe.push('\u{fffd}');
        } else {
            safe.push(ch);
        }
    }
    if chars.next().is_some() {
        safe.push('…');
    }
    Some(safe)
}

fn parse_ahead_behind(value: &str) -> Option<(u32, u32)> {
    let mut fields = value.split_whitespace();
    let ahead = fields.next()?.strip_prefix('+')?.parse().ok()?;
    let behind = fields.next()?.strip_prefix('-')?.parse().ok()?;
    Some((ahead, behind))
}

/// Run one bounded Git process. A helper drains stdout so a very dirty worktree
/// cannot fill the pipe and deadlock before the child exits.
fn run_git_status(cwd: &Path) -> Option<String> {
    let mut command = crate::host::helper_command_with_cwd("git", cwd).ok()?;
    harden_git_environment(&mut command);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .args([
            // A repository-local core.fsmonitor path is executable code. Use
            // a known inert hook on every supported Unix, including Git
            // versions old enough to interpret the value `false` as a hook
            // pathname rather than a boolean. Likewise suppress ordinary
            // repository hooks; rendering a status strip must not execute
            // project-controlled programs.
            "-c",
            "core.fsmonitor=/bin/false",
            "-c",
            "core.hooksPath=/dev/null",
            "status",
            "--porcelain=v2",
            "--branch",
            "--untracked-files=normal",
        ])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let stdout = child.stdout.take()?;

    #[cfg(unix)]
    {
        collect_git_status_output(child, stdout)
    }
    #[cfg(not(unix))]
    {
        collect_git_status_output_portable(child, stdout)
    }
}

fn harden_git_environment(command: &mut std::process::Command) {
    for name in UNTRUSTED_GIT_ENVIRONMENT {
        // Repository selection must come from `cwd`, and an inherited trace or
        // config-injection variable must not redirect a supposedly read-only
        // strip probe into another repository or an attacker-chosen output
        // file. Ordinary global/system Git configuration remains available.
        command.env_remove(name);
    }
}

#[cfg(unix)]
fn collect_git_status_output(mut child: Child, mut stdout: ChildStdout) -> Option<String> {
    use std::os::fd::AsRawFd;

    // Keep stdout in the same bounded polling loop as the child. A Git hook or
    // replacement executable may leave a background descendant holding the
    // pipe open; a blocking reader thread would then outlive the direct child.
    let descriptor = stdout.as_raw_fd();
    // SAFETY: F_GETFL/F_SETFL only inspect and update flags on the live stdout
    // descriptor. Existing flags are preserved.
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        terminate_child(&mut child);
        return None;
    }

    let started = Instant::now();
    let mut status = None;
    let mut eof = false;
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 16 * 1024];

    loop {
        loop {
            match stdout.read(&mut buffer) {
                Ok(0) => {
                    eof = true;
                    break;
                }
                Ok(read) => {
                    if bytes.len().saturating_add(read) > MAX_GIT_STATUS_OUTPUT_BYTES as usize {
                        terminate_child(&mut child);
                        return None;
                    }
                    bytes.extend_from_slice(&buffer[..read]);
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    terminate_child(&mut child);
                    return None;
                }
            }
        }

        if status.is_none() {
            match child.try_wait() {
                Ok(Some(exit)) => status = Some(exit),
                Ok(None) => {}
                Err(_) => {
                    terminate_child(&mut child);
                    return None;
                }
            }
        }
        if eof {
            if let Some(status) = status {
                return status
                    .success()
                    .then(|| String::from_utf8(bytes).ok())
                    .flatten();
            }
        }
        if started.elapsed() >= GIT_STATUS_TIMEOUT {
            terminate_child(&mut child);
            return None;
        }
        thread::sleep(GIT_WAIT_POLL);
    }
}

#[cfg(not(unix))]
fn collect_git_status_output_portable(mut child: Child, stdout: ChildStdout) -> Option<String> {
    // Portable std does not expose a nonblocking pipe descriptor or a process
    // group kill. Permit at most one reader that has not reached EOF: if a
    // detached descendant inherits stdout, later probes fail closed instead
    // of leaking one permanently blocked thread per refresh.
    if PORTABLE_READER_ACTIVE
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        terminate_child(&mut child);
        return None;
    }
    let (output_tx, output_rx) = mpsc::sync_channel(1);
    let reader = thread::Builder::new()
        .name("jterm-git-status-reader".to_string())
        .spawn(move || {
            let _active = PortableReaderGuard;
            let _ = output_tx.send(read_bounded_status_output(stdout));
        });
    if reader.is_err() {
        PORTABLE_READER_ACTIVE.store(false, Ordering::Release);
        terminate_child(&mut child);
        return None;
    }

    let started = Instant::now();
    let mut status = None;
    let mut output = None;
    loop {
        match output_rx.try_recv() {
            Ok(Some(captured)) => output = Some(captured),
            Ok(None) => {
                terminate_child(&mut child);
                return None;
            }
            Err(mpsc::TryRecvError::Disconnected) if output.is_none() => {
                terminate_child(&mut child);
                return None;
            }
            Err(mpsc::TryRecvError::Disconnected) => {}
            Err(mpsc::TryRecvError::Empty) => {}
        }

        if status.is_none() {
            match child.try_wait() {
                Ok(Some(exit)) => status = Some(exit),
                Ok(None) => {}
                Err(_) => {
                    terminate_child(&mut child);
                    return None;
                }
            }
        }
        if let (Some(status), Some(output)) = (status.as_ref(), output.as_ref()) {
            return status.success().then(|| output.clone());
        }
        if started.elapsed() >= GIT_STATUS_TIMEOUT {
            terminate_child(&mut child);
            // Deliberately do not join: a detached descendant may still own
            // the pipe. The active guard keeps that leak bounded to one.
            return None;
        }
        thread::sleep(GIT_WAIT_POLL);
    }
}

#[cfg(any(not(unix), test))]
fn read_bounded_status_output(reader: impl Read) -> Option<String> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_GIT_STATUS_OUTPUT_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_GIT_STATUS_OUTPUT_BYTES {
        return None;
    }
    String::from_utf8(bytes).ok()
}

fn terminate_child(child: &mut Child) {
    #[cfg(unix)]
    {
        let process_group = child.id() as i32;
        if process_group > 0 {
            // SAFETY: run_git_status places this child in a fresh process
            // group whose id is the child pid. A negative target reaches any
            // descendant that inherited stdout and kept the pipe open.
            let _ = unsafe { libc::kill(-process_group, libc::SIGKILL) };
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Format a RepoMeta into the compact status-strip text.
pub fn format_strip(meta: &RepoMeta) -> String {
    let mut s = String::new();
    s.push_str(&meta.branch);
    if meta.dirty {
        s.push_str(" ●");
    }
    match (meta.ahead, meta.behind) {
        (Some(a), Some(b)) if a > 0 || b > 0 => {
            s.push_str("  ");
            if a > 0 {
                s.push_str(&format!("↑{a} "));
            }
            if b > 0 {
                s.push_str(&format!("↓{b}"));
            }
        }
        _ => {}
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    struct TestRepo(PathBuf);

    #[cfg(unix)]
    impl TestRepo {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "jterm-core-git-meta-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn git(&self, arguments: &[&str]) -> bool {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&self.0)
                .args(arguments)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        }
    }

    #[cfg(unix)]
    impl Drop for TestRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn parses_clean_branch_with_upstream() {
        let output = concat!(
            "# branch.oid 0123456789abcdef\n",
            "# branch.head feature/cache\n",
            "# branch.upstream origin/feature/cache\n",
            "# branch.ab +2 -1\n",
        );
        assert_eq!(
            parse_porcelain_v2(output),
            Some(RepoMeta {
                branch: "feature/cache".into(),
                dirty: false,
                ahead: Some(2),
                behind: Some(1),
            })
        );
    }

    #[test]
    fn parses_dirty_records_and_no_upstream() {
        let output = concat!(
            "# branch.oid 0123456789abcdef\n",
            "# branch.head main\n",
            "1 .M N... 100644 100644 100644 abc abc src/main.rs\n",
            "? scratch.txt\n",
        );
        assert_eq!(
            parse_porcelain_v2(output),
            Some(RepoMeta {
                branch: "main".into(),
                dirty: true,
                ahead: None,
                behind: None,
            })
        );
    }

    #[test]
    fn parses_detached_head_as_short_oid() {
        let output = concat!(
            "# branch.oid 89abcdef01234567\n",
            "# branch.head (detached)\n",
        );
        assert_eq!(
            parse_porcelain_v2(output).map(|meta| meta.branch),
            Some("(89abcde)".into())
        );
    }

    #[test]
    fn rejects_missing_or_initial_detached_oid() {
        assert_eq!(parse_porcelain_v2("# branch.oid abc\n"), None);
        assert_eq!(
            parse_porcelain_v2("# branch.oid (initial)\n# branch.head (detached)\n"),
            None
        );
        assert_eq!(
            parse_porcelain_v2("# branch.oid éééééééé\n# branch.head (detached)\n"),
            None
        );
        assert_eq!(
            parse_porcelain_v2("# branch.oid zzzzzzz\n# branch.head (detached)\n"),
            None
        );
    }

    #[test]
    fn branch_display_replaces_visual_controls_and_is_bounded() {
        let long = format!(
            "safe\u{00a0}\u{2003}\u{202e}\u{200b}\u{00ad}\u{fe0f}\u{e0020}{}",
            "界".repeat(MAX_BRANCH_DISPLAY_CHARS + 10)
        );
        let output = format!("# branch.oid 0123456789abcdef\n# branch.head {long}\n");
        let branch = parse_porcelain_v2(&output).unwrap().branch;

        assert!(!branch.contains('\u{202e}'));
        assert!(!branch.contains('\u{200b}'));
        assert!(!branch.contains('\u{00a0}'));
        assert!(!branch.contains('\u{2003}'));
        assert!(!branch.contains('\u{00ad}'));
        assert!(!branch.contains('\u{fe0f}'));
        assert!(!branch.contains('\u{e0020}'));
        assert!(branch.contains("��"));
        assert!(branch.ends_with('…'));
        assert_eq!(branch.chars().count(), MAX_BRANCH_DISPLAY_CHARS + 1);
    }

    #[test]
    fn git_status_reader_rejects_oversize_and_non_utf8_output() {
        let exact = vec![b'x'; MAX_GIT_STATUS_OUTPUT_BYTES as usize];
        assert_eq!(
            read_bounded_status_output(std::io::Cursor::new(exact.clone()))
                .unwrap()
                .len(),
            exact.len()
        );
        let oversized = vec![b'x'; MAX_GIT_STATUS_OUTPUT_BYTES as usize + 1];
        assert!(read_bounded_status_output(std::io::Cursor::new(oversized)).is_none());
        assert!(read_bounded_status_output(std::io::Cursor::new([0xff])).is_none());
    }

    #[test]
    fn probe_paths_are_bounded_before_queue_or_filesystem_work() {
        let oversized = PathBuf::from("x".repeat(MAX_GIT_CWD_BYTES + 1));
        assert_eq!(read(&oversized), None);
        assert_eq!(read(Path::new("bad\0cwd")), None);
    }

    #[test]
    fn coalesces_waiters_for_the_same_path() {
        let (request_tx, request_rx) = mpsc::sync_channel(1);
        let service = GitMetaService {
            request_tx,
            cache: Arc::new(Mutex::new(HashMap::new())),
            pending: Arc::new(Mutex::new(HashMap::new())),
        };
        let path = Path::new("/tmp/repo");

        let _first = service.request(path).expect("first request");
        let _second = service.request(path).expect("coalesced request");

        assert_eq!(request_rx.try_iter().count(), 1);
        assert_eq!(
            service.pending.lock().unwrap().get(path).map(Vec::len),
            Some(2)
        );
    }

    #[test]
    fn probe_queue_and_per_path_waiters_are_bounded() {
        let (request_tx, _request_rx) = mpsc::sync_channel(1);
        let service = GitMetaService {
            request_tx,
            cache: Arc::new(Mutex::new(HashMap::new())),
            pending: Arc::new(Mutex::new(HashMap::new())),
        };
        let first = Path::new("/tmp/repo-one");
        assert!(service.request(first).is_some());
        assert!(service.request(Path::new("/tmp/repo-two")).is_none());
        assert!(!service
            .pending
            .lock()
            .unwrap()
            .contains_key(Path::new("/tmp/repo-two")));

        for _ in 1..MAX_WAITERS_PER_PATH {
            assert!(service.request(first).is_some());
        }
        assert!(service.request(first).is_none());
        assert_eq!(
            service.pending.lock().unwrap().get(first).map(Vec::len),
            Some(MAX_WAITERS_PER_PATH)
        );
    }

    #[cfg(unix)]
    #[test]
    fn status_collector_does_not_wait_for_background_pipe_holder() {
        use std::os::unix::process::CommandExt;

        let mut command = std::process::Command::new("sh");
        command
            .args(["-c", "sleep 5 &"])
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = command.spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let started = Instant::now();

        assert!(collect_git_status_output(child, stdout).is_none());
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn status_probe_scrubs_repository_and_trace_environment_overrides() {
        let mut command = std::process::Command::new("git");
        for name in UNTRUSTED_GIT_ENVIRONMENT {
            command.env(name, "attacker-controlled");
        }
        harden_git_environment(&mut command);

        for name in UNTRUSTED_GIT_ENVIRONMENT {
            let value = command
                .get_envs()
                .find(|(key, _)| *key == std::ffi::OsStr::new(name))
                .map(|(_, value)| value);
            assert_eq!(value, Some(None), "{name}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn status_probe_never_runs_repository_configured_programs() {
        use std::os::unix::fs::PermissionsExt;

        let repo = TestRepo::new();
        if !repo.git(&["init", "--quiet"]) {
            // Git is an optional runtime integration; an installation without
            // it still has a meaningful unit-test configuration.
            return;
        }
        std::fs::write(repo.0.join("tracked"), "original\n").unwrap();
        assert!(repo.git(&["add", "tracked"]));

        let sentinel = repo.0.join("hook-ran");
        let hook = repo.0.join("hostile-hook");
        std::fs::write(
            &hook,
            format!("#!/bin/sh\n: > '{}'\nexit 0\n", sentinel.display()),
        )
        .unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o700)).unwrap();
        let filter_sentinel = repo.0.join("filter-ran");
        let filter = repo.0.join("hostile-filter");
        std::fs::write(
            &filter,
            format!("#!/bin/sh\n: > '{}'\ncat\n", filter_sentinel.display()),
        )
        .unwrap();
        std::fs::set_permissions(&filter, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(repo.0.join(".gitattributes"), "tracked filter=hostile\n").unwrap();
        assert!(repo.git(&["add", ".gitattributes"]));
        assert!(repo.git(&["config", "filter.hostile.clean", filter.to_str().unwrap()]));
        // Stage once with the filter active, then clear the setup sentinel so
        // only work performed by the status-strip probe is observed below.
        assert!(repo.git(&["add", "tracked"]));
        let _ = std::fs::remove_file(&filter_sentinel);
        assert!(repo.git(&["config", "core.fsmonitor", hook.to_str().unwrap()]));

        std::fs::write(repo.0.join("tracked"), "changed\n").unwrap();
        let meta = run_git_status(&repo.0);
        assert!(
            meta.is_some(),
            "the hardened status probe should still work"
        );
        assert!(
            !sentinel.exists(),
            "status-strip probing executed repository-controlled code"
        );
        assert!(
            !filter_sentinel.exists(),
            "status-strip probing executed a repository-controlled content filter"
        );
    }

    #[test]
    fn format_strip_clean_no_upstream() {
        let meta = RepoMeta {
            branch: "main".into(),
            dirty: false,
            ahead: None,
            behind: None,
        };
        assert_eq!(format_strip(&meta), "main");
    }

    #[test]
    fn format_strip_dirty_marker() {
        let meta = RepoMeta {
            branch: "feature/x".into(),
            dirty: true,
            ahead: None,
            behind: None,
        };
        assert_eq!(format_strip(&meta), "feature/x ●");
    }

    #[test]
    fn format_strip_ahead_behind() {
        let meta = RepoMeta {
            branch: "main".into(),
            dirty: false,
            ahead: Some(2),
            behind: Some(1),
        };
        assert_eq!(format_strip(&meta), "main  ↑2 ↓1");
    }

    #[test]
    fn format_strip_ahead_only() {
        let meta = RepoMeta {
            branch: "main".into(),
            dirty: true,
            ahead: Some(3),
            behind: Some(0),
        };
        assert_eq!(format_strip(&meta), "main ●  ↑3 ");
    }

    #[test]
    fn format_strip_zero_zero_hidden() {
        let meta = RepoMeta {
            branch: "main".into(),
            dirty: false,
            ahead: Some(0),
            behind: Some(0),
        };
        assert_eq!(format_strip(&meta), "main");
    }
}
