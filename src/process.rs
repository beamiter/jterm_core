//! UI-independent process inspection and shell quoting shared by the jterm
//! family. Seeded from jterm1's hardened `process.rs`, the only copy whose
//! tests pin the edge cases the other terminals' local copies got wrong.
//!
//! Four concerns live here because they feed each other:
//! - **Quoting** renders argv vectors into interactive-shell input without
//!   changing argument boundaries, rejecting control characters that a PTY
//!   line editor would interpret before shell parsing.
//! - **Restorable-command classification** decides which foreground commands
//!   (ssh, mosh, `nix develop`, `docker exec`, …) a session snapshot may
//!   replay, keeping the argv structured — joining it would let a remote
//!   argument containing `;` become a new local command on restore.
//! - **`/proc` probes** discover the foreground process, its argv, and its
//!   cwd; every parser indexes from the last `)` so a `comm` containing
//!   spaces or parens cannot shift fields.
//! - **Child-process lifecycle** owns termination and reaping: a reuse-proof
//!   [`ProcessRef`], the SIGHUP → SIGTERM → SIGKILL [`EscalationPolicy`]
//!   ladder with an optional PTY-session drain, and a [`ChildLifecycle`] that
//!   serializes every signal against `waitpid` so a pid is never signaled
//!   after the kernel is free to reuse it. The `/proc` probes above are what
//!   make the session drain possible, which is why both live in one module.

use serde::Deserialize;
use std::ffi::c_int;
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

// ---------------------------------------------------------------------------
// Shell quoting
// ---------------------------------------------------------------------------

/// Quote one string as a single POSIX-shell word using the portable
/// close/quoted-quote/reopen sequence (`'` → `'"'"'`). The empty string
/// quotes to `''`. Control characters pass through unchanged; use
/// [`shell_quote_argv`] when the output is typed into a live PTY.
pub fn shell_single_quote(s: &str) -> String {
    let mut quoted = String::with_capacity(s.len() + 2);
    quoted.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            quoted.push_str("'\"'\"'");
        } else {
            quoted.push(ch);
        }
    }
    quoted.push('\'');
    quoted
}

/// Quote a filesystem path for insertion into an interactive command line,
/// leaving obviously safe paths unquoted so the inserted text stays readable.
pub fn shell_quote_path(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    let safe = s
        .chars()
        .all(|c| c.is_alphanumeric() || "._-/~".contains(c));
    if safe {
        s.to_string()
    } else {
        shell_single_quote(s)
    }
}

/// Render one argv as a single POSIX-shell command without changing argument
/// boundaries. Every argument is single-quoted; embedded single quotes use the
/// standard close/quoted-quote/reopen sequence.
///
/// Control characters are rejected even though a shell can quote some of them:
/// this command is injected through an interactive PTY, where bytes such as ESC
/// or a newline are interpreted by the line editor before shell parsing.
pub fn shell_quote_argv(args: &[String]) -> Option<String> {
    if args.is_empty()
        || args
            .iter()
            .any(|argument| argument.chars().any(char::is_control))
    {
        return None;
    }
    Some(
        args.iter()
            .map(|argument| format!("'{}'", argument.replace('\'', "'\"'\"'")))
            .collect::<Vec<_>>()
            .join(" "),
    )
}

fn powershell_quote_argv(args: &[String]) -> Option<String> {
    if args.is_empty()
        || args
            .iter()
            .any(|argument| argument.chars().any(char::is_control))
    {
        return None;
    }
    Some(format!(
        "& {}",
        args.iter()
            .map(|argument| format!("'{}'", argument.replace('\'', "''")))
            .collect::<Vec<_>>()
            .join(" ")
    ))
}

/// Quote a restorable argv for the configured interactive shell. Unknown shell
/// grammars are deliberately not guessed: skipping automatic replay is safer
/// than changing argument boundaries.
pub fn shell_quote_argv_for(args: &[String], shell_argv: &[String]) -> Option<String> {
    let shell = Path::new(shell_argv.first()?)
        .file_name()?
        .to_str()?
        .to_ascii_lowercase();
    match shell.as_str() {
        "pwsh" | "powershell" | "powershell.exe" | "pwsh.exe" => powershell_quote_argv(args),
        "bash" | "dash" | "fish" | "ksh" | "mksh" | "sh" | "zsh" => shell_quote_argv(args),
        _ => None,
    }
}

/// Build the `exec` line that replaces a bootstrap shell with jsh, optionally
/// resuming a saved session. Both fields are quoted as single arguments.
pub fn build_jsh_exec_command(shell_path: &str, session_id: Option<&str>) -> String {
    let mut exec_cmd = format!("exec {}", shell_single_quote(shell_path));
    if let Some(sid) = session_id {
        exec_cmd.push_str(" --session ");
        exec_cmd.push_str(&shell_single_quote(sid));
    }
    exec_cmd
}

// ---------------------------------------------------------------------------
// Restorable-command classification
// ---------------------------------------------------------------------------

/// Check if an argv matches a known restorable command pattern, returning the
/// original argument vector for session persistence. Keeping argv structured is
/// important: joining it here would discard quoting boundaries, so a remote
/// command argument containing `;` could become a new local command on restore.
pub fn match_restorable_command(args: &[String]) -> Option<Vec<String>> {
    if args.is_empty() {
        return None;
    }
    let bin = Path::new(&args[0])
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();

    match bin.as_str() {
        "nix" => {
            if args.len() >= 2 && args[1] == "develop" {
                Some(args.to_vec())
            } else {
                None
            }
        }
        // nix develop execs into e.g. `bash --rcfile /tmp/nix-shell.XXXXX`.
        "bash" | "zsh" | "fish" => {
            for arg in &args[1..] {
                if arg.starts_with("/tmp/nix-shell.") || arg.starts_with("/tmp/nix-shell-") {
                    return Some(vec!["nix".to_string(), "develop".to_string()]);
                }
            }
            None
        }
        "ssh" | "mosh" => Some(args.to_vec()),
        "docker" | "podman" => {
            if args.len() >= 2
                && (args[1] == "exec"
                    || (args[1] == "compose" && args.len() >= 3 && args[2] == "exec"))
            {
                Some(args.to_vec())
            } else {
                None
            }
        }
        _ => None,
    }
}

fn path_basename(command: &str) -> &str {
    Path::new(command)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
}

fn command_basename(args: &[String]) -> &str {
    crate::host::unwrap_host_argv(args)
        .first()
        .map(|command| path_basename(command))
        .unwrap_or_default()
}

/// Whether OSC 7 paths reported while this command runs belong to another
/// filesystem namespace and therefore must not drive local cwd operations.
pub fn command_uses_external_cwd(args: &[String]) -> bool {
    matches!(
        command_basename(args),
        "ssh" | "mosh" | "mosh-client" | "docker" | "podman"
    )
}

/// Commands whose restored session semantics require the Block parser even
/// when local panes use the VTE compatibility backend.
pub fn command_requires_block_integration(args: &[String]) -> bool {
    matches!(command_basename(args), "ssh" | "mosh")
}

/// How a snapshot on disk may spell a restorable command. `untagged` so both
/// shapes are accepted from the same field without a schema version.
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum StoredRestorableCommand {
    Argv(Vec<String>),
    LegacyString(String),
}

/// Deserialize a snapshot's restorable command, accepting the legacy joined
/// string but never replaying it.
///
/// The identical duplicate of this lived at jterm1 `src/session.rs` and jterm4
/// `src/state.rs` (byte-for-byte apart from the log level). Older snapshots
/// stored the command as `argv.join(" ")`, whose argument boundaries cannot be
/// recovered: `ssh host 'echo a; rm b'` re-splits into a different command, and
/// re-quoting a guess would run it locally at restore time without the user
/// asking. Deserializing to `None` keeps such a snapshot loadable — its tabs,
/// cwds and layout are still good — while dropping only the command that cannot
/// be replayed safely.
///
/// Use with `#[serde(default, deserialize_with = "...")]` on an
/// `Option<Vec<String>>` field, so a snapshot predating the field also loads.
pub fn deserialize_restorable_argv<'de, D>(deserializer: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let stored = Option::<StoredRestorableCommand>::deserialize(deserializer)?;
    Ok(match stored {
        // An empty argv is not restorable either: replaying it would exec the
        // configured shell as if the pane had never run a command.
        Some(StoredRestorableCommand::Argv(argv)) if !argv.is_empty() => Some(argv),
        Some(StoredRestorableCommand::LegacyString(joined)) => {
            // Log the first word only. It is the one boundary the joined form
            // usually did preserve, so it tells a user which pane lost its
            // command, without echoing arguments that can hold hostnames, remote
            // paths or a `docker exec` target into the log.
            let program = joined.split_whitespace().next().unwrap_or("(empty)");
            log::warn!(
                "Ignoring legacy session restore command '{program}' without argv boundaries"
            );
            None
        }
        _ => None,
    })
}

// ---------------------------------------------------------------------------
// /proc probes
// ---------------------------------------------------------------------------

/// Parse `/proc/<pid>/stat` and return the parent pid (the 4th field, after the
/// `comm` parenthesised name which may itself contain spaces/parens).
pub fn read_ppid(pid: i32) -> Option<i32> {
    if pid <= 0 {
        return None;
    }
    let contents = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = contents.rsplit_once(')')?.1;
    let mut fields = after_comm.split_whitespace();
    fields.next(); // state
    fields.next()?.parse::<i32>().ok()
}

/// Read `/proc/<pid>/cmdline` as a NUL-separated argv vector.
pub fn read_proc_cmdline(pid: i32) -> Option<Vec<String>> {
    if pid <= 0 {
        return None;
    }
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    if raw.is_empty() {
        return None;
    }
    let args: Vec<String> = raw
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).to_string())
        .collect();
    if args.is_empty() {
        None
    } else {
        Some(args)
    }
}

/// The kernel task name from `/proc/<pid>/comm`, trimmed. Truncated by the
/// kernel to `TASK_COMM_LEN`; prefer [`read_proc_cmdline`] when a full argv[0]
/// is available, and this when only a short display name is needed.
pub fn process_comm(pid: i32) -> Option<String> {
    if pid <= 0 {
        return None;
    }
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    let comm = comm.trim();
    (!comm.is_empty()).then(|| comm.to_string())
}

/// The process working directory via `/proc/<pid>/cwd`, when readable.
pub fn process_cwd(pid: i32) -> Option<String> {
    if pid <= 0 {
        return None;
    }
    std::fs::read_link(format!("/proc/{pid}/cwd"))
        .ok()
        .map(|path| path.to_string_lossy().into_owned())
}

/// Fields of `/proc/<pid>/stat` the terminals inspect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessStat {
    pub state: char,
    pub process_group: i32,
    pub session: i32,
    /// Foreground process group of the controlling terminal (`tpgid`); `-1`
    /// when the process has no controlling terminal.
    pub foreground_group: i32,
}

impl ProcessStat {
    pub fn is_live(self) -> bool {
        !matches!(self.state, 'Z' | 'X' | 'x')
    }
}

/// Parse `/proc/<pid>/stat` content. The `comm` field may contain spaces and
/// parens, so fields index from the last `)`.
pub fn parse_process_stat(contents: &str) -> Option<ProcessStat> {
    let rparen_pos = contents.rfind(')')?;
    let after_comm = &contents[rparen_pos + 1..];
    let mut fields = after_comm.split_whitespace();
    let state = fields.next()?.chars().next()?;
    fields.next()?; // parent pid
    let process_group = fields.next()?.parse().ok()?;
    let session = fields.next()?.parse().ok()?;
    fields.next()?; // tty_nr
    let foreground_group = fields.next()?.parse().ok()?;
    Some(ProcessStat {
        state,
        process_group,
        session,
        foreground_group,
    })
}

/// Read and parse `/proc/<pid>/stat`, distinguishing a vanished process
/// (`NotFound`) from an unreadable or malformed entry — the distinction a
/// PID-reuse-safe shutdown drain needs.
pub fn process_stat_result(pid: i32) -> io::Result<ProcessStat> {
    if pid <= 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "process id must be positive",
        ));
    }
    let contents = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    parse_process_stat(&contents)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "malformed proc stat"))
}

pub fn process_stat(pid: i32) -> Option<ProcessStat> {
    process_stat_result(pid).ok()
}

// ---------------------------------------------------------------------------
// Foreground discovery
// ---------------------------------------------------------------------------

/// The foreground process group on a PTY master fd via `tcgetpgrp`.
pub fn tty_foreground_pgid(pty_fd: i32) -> Option<i32> {
    if pty_fd < 0 {
        return None;
    }
    let fg = unsafe { libc::tcgetpgrp(pty_fd) };
    (fg > 0).then_some(fg)
}

/// The foreground process group id on a PTY master fd, or None if the shell
/// itself (`shell_pid`) is in the foreground (nothing interesting is running).
pub fn foreground_pgid(pty_fd: i32, shell_pid: i32) -> Option<i32> {
    tty_foreground_pgid(pty_fd).filter(|foreground| *foreground != shell_pid)
}

/// Foreground process group via the shell's `/proc/<pid>/stat` `tpgid` field.
/// The alternate entry point for UIs that hold no PTY master fd; otherwise
/// prefer [`foreground_pgid`], which asks the tty layer directly.
pub fn foreground_pgid_via_stat(shell_pid: i32) -> Option<i32> {
    let stat = process_stat(shell_pid)?;
    (stat.foreground_group > 0).then_some(stat.foreground_group)
}

fn classify_foreground_external_cwd<ReadArgv, ReadParent>(
    shell_pid: i32,
    foreground_pid: i32,
    mut read_argv: ReadArgv,
    mut read_parent: ReadParent,
) -> Option<bool>
where
    ReadArgv: FnMut(i32) -> Option<Vec<String>>,
    ReadParent: FnMut(i32) -> Option<i32>,
{
    if shell_pid <= 1 || foreground_pid <= 1 {
        return None;
    }

    // A managed ssh/mosh pane launches the external command as the PTY child
    // itself. Reading it before the foreground comparison preserves that case.
    let shell_argv = read_argv(shell_pid)?;
    if command_uses_external_cwd(&shell_argv) {
        return Some(true);
    }
    if crate::host::is_host_wrapper_argv(&shell_argv) {
        // A local shell launched through `flatpak-spawn --host` can start ssh
        // entirely outside this PID namespace. The wrapper argv proves neither
        // local nor external foreground state, so preserve the caller's sticky
        // classification unless OSC authority proves it.
        return None;
    }
    if foreground_pid == shell_pid {
        return Some(false);
    }

    let mut pid = foreground_pid;
    for _ in 0..16 {
        if pid == shell_pid {
            return Some(false);
        }
        if pid <= 1 {
            return None;
        }
        let argv = read_argv(pid)?;
        if command_uses_external_cwd(&argv) {
            return Some(true);
        }
        pid = read_parent(pid)?;
    }
    (pid == shell_pid).then_some(false)
}

/// Determine whether the PTY foreground belongs to an ssh/mosh/container
/// namespace. `None` means the tty or `/proc` ancestry could not be read, so a
/// caller must keep its previous conservative classification.
pub fn foreground_uses_external_cwd(pty_fd: i32, shell_pid: i32) -> Option<bool> {
    let foreground_pid = tty_foreground_pgid(pty_fd)?;
    classify_foreground_external_cwd(shell_pid, foreground_pid, read_proc_cmdline, read_ppid)
}

/// Detect a restorable interactive command (ssh/nix develop/docker exec/…) by
/// walking from the PTY's foreground process up to the shell.
pub fn restorable_command(pty_fd: i32, shell_pid: i32) -> Option<Vec<String>> {
    // Managed remote panes launch ssh/mosh as the PTY child itself rather than
    // underneath an interactive local shell. Preserve that allowlisted argv
    // too; ordinary bash/zsh/jsh children do not match and continue below.
    if let Some(command) =
        read_proc_cmdline(shell_pid).and_then(|args| match_restorable_command(&args))
    {
        return Some(command);
    }

    let mut pid = foreground_pgid(pty_fd, shell_pid)?;
    let mut visited = 0;
    while pid != shell_pid && pid > 1 && visited < 16 {
        if let Some(args) = read_proc_cmdline(pid) {
            if let Some(cmd) = match_restorable_command(&args) {
                return Some(cmd);
            }
        }
        pid = match read_ppid(pid) {
            Some(ppid) => ppid,
            None => break,
        };
        visited += 1;
    }
    None
}

/// Name of the foreground process on a PTY (e.g. "ssh", "vim"), or None if the
/// shell itself is in the foreground. Used for close-confirmation prompts.
pub fn foreground_process_name(pty_fd: i32, shell_pid: i32) -> Option<String> {
    if let Some(args) = read_proc_cmdline(shell_pid) {
        if match_restorable_command(&args).is_some() {
            return Path::new(args.first()?)
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_string);
        }
    }
    let fg = foreground_pgid(pty_fd, shell_pid)?;
    let args = read_proc_cmdline(fg)?;
    Path::new(args.first()?)
        .file_name()
        .and_then(|n| n.to_str())
        .map(|s| s.to_string())
}

// ---------------------------------------------------------------------------
// Child-process lifecycle
// ---------------------------------------------------------------------------

/// How long the final SIGKILL drain keeps re-scanning a PTY session for
/// descendants that forked while the earlier signals were being delivered.
pub const SESSION_KILL_DRAIN_TIMEOUT: Duration = Duration::from_millis(150);

/// Delay between session re-scans inside [`SESSION_KILL_DRAIN_TIMEOUT`].
pub const SESSION_SCAN_INTERVAL: Duration = Duration::from_millis(10);

/// Consecutive empty scans required before a session counts as drained. One
/// empty scan is not enough: a descendant can fork between two scans, so the
/// drain only concludes after seeing the session empty twice in a row.
pub const REQUIRED_QUIET_SESSION_SCANS: u8 = 2;

/// Interpret a raw `waitpid`/`wait4` status with the family's shell
/// convention: a normal exit yields its own code, and a fatal signal yields
/// `128 + signal` — the encoding [`crate::exit_status::signal_name_for_exit`]
/// decodes back into a name.
///
/// `None` means the status described a stop or continue rather than a
/// terminal state, so the child is still alive and must not be treated as
/// reaped. Frontends whose child watch is owned by another runtime (glib/vte
/// hands the raw status to its `child-exited` handler) use this to normalize
/// that status before [`ChildLifecycle::note_foreign_exit`].
pub fn exit_code_from_wait_status(status: c_int) -> Option<i32> {
    if libc::WIFEXITED(status) {
        Some(libc::WEXITSTATUS(status))
    } else if libc::WIFSIGNALED(status) {
        Some(128 + libc::WTERMSIG(status))
    } else {
        // WIFSTOPPED / WIFCONTINUED: reported only with WUNTRACED/WCONTINUED,
        // and never a terminal status.
        None
    }
}

#[cfg(target_os = "linux")]
fn open_pidfd(pid: i32) -> io::Result<OwnedFd> {
    // SAFETY: pidfd_open takes a pid and a flags word by value.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: pidfd_open returns a new owned descriptor on success.
        Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
    }
}

#[cfg(target_os = "linux")]
fn send_pidfd_signal(pidfd: &OwnedFd, signal: c_int) -> io::Result<()> {
    // SAFETY: the descriptor is borrowed for the call and the siginfo pointer
    // is explicitly null, which asks the kernel to synthesize SI_USER.
    let result = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd(),
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// A handle to a running process that survives pid reuse.
///
/// A raw pid stops identifying anything the instant its process is reaped: the
/// kernel may hand the same number to an unrelated process before the next
/// `kill`. On Linux this type holds a pidfd, so every signal is bound to the
/// process that was opened and a reused number is rejected with `ESRCH`.
///
/// Where `pidfd_open` does not exist (non-Linux, or `ENOSYS`/`EPERM` from a
/// pre-5.3 kernel or a seccomp filter) the handle degrades to the raw pid and
/// [`ProcessRef::is_kernel_bound`] reports `false`. Callers that need the
/// stronger guarantee must check it rather than assume it.
#[derive(Debug)]
pub struct ProcessRef {
    pid: i32,
    #[cfg(target_os = "linux")]
    pidfd: Option<OwnedFd>,
    /// Mirrors `pidfd.is_some()` so platform-independent code can branch
    /// without repeating the cfg.
    kernel_bound: bool,
}

impl ProcessRef {
    /// Open a reuse-proof reference to `pid`.
    ///
    /// Fails when `pid` is not positive, or when the process cannot be
    /// referenced at all — most importantly `ESRCH`, which means the pid is
    /// already gone and any signal sent to that number would land on whatever
    /// the kernel assigns next. Only "pidfd is unavailable here" errors
    /// (`ENOSYS`, `EPERM`) degrade to the raw-pid fallback.
    pub fn open(pid: i32) -> io::Result<Self> {
        if pid <= 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "process id must be positive",
            ));
        }
        #[cfg(target_os = "linux")]
        {
            match open_pidfd(pid) {
                Ok(pidfd) => Ok(Self {
                    pid,
                    pidfd: Some(pidfd),
                    kernel_bound: true,
                }),
                Err(error)
                    if matches!(error.raw_os_error(), Some(libc::ENOSYS) | Some(libc::EPERM)) =>
                {
                    Ok(Self::unverified(pid))
                }
                Err(error) => Err(error),
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            Ok(Self::unverified(pid))
        }
    }

    /// The raw-pid fallback. Deliberately private: a public constructor taking
    /// a bare pid would be an unverified `kill(pid, …)` one call away, which is
    /// precisely the hazard this type exists to remove.
    fn unverified(pid: i32) -> Self {
        Self {
            pid,
            #[cfg(target_os = "linux")]
            pidfd: None,
            kernel_bound: false,
        }
    }

    /// The numeric pid. Safe to display or to pass to `/proc` probes; never
    /// signal it directly — use [`ProcessRef::signal`].
    pub fn pid(&self) -> i32 {
        self.pid
    }

    /// Whether signals through this handle are bound to a kernel object and
    /// therefore immune to pid reuse.
    pub fn is_kernel_bound(&self) -> bool {
        self.kernel_bound
    }

    /// Send `signal` to exactly this process.
    ///
    /// Kernel-bound handles cannot reach a reused pid. Fallback handles carry
    /// the same reuse exposure as a plain `kill(pid, …)`, so callers holding
    /// one are responsible for the pid still being theirs — which is what
    /// [`ChildLifecycle`] guarantees by never releasing a pid while a signal
    /// may be in flight.
    pub fn signal(&self, signal: c_int) -> io::Result<()> {
        if self.pid <= 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "process id must be positive",
            ));
        }
        #[cfg(target_os = "linux")]
        if let Some(pidfd) = self.pidfd.as_ref() {
            return send_pidfd_signal(pidfd, signal);
        }
        // SAFETY: kill takes a pid and a signal number by value.
        let result = unsafe { libc::kill(self.pid, signal) };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    /// Send `signal` to this process' whole group, but only after `/proc`
    /// confirms the process still leads that group.
    ///
    /// The check is what makes the negative-pid `kill` safe to expose: without
    /// it, `kill(-pid, …)` on a reused or non-leader pid would broadcast to an
    /// unrelated group. Returns `InvalidInput` when the process does not lead
    /// its group (including when `/proc` cannot answer).
    pub fn signal_process_group(&self, signal: c_int) -> io::Result<()> {
        if !process_stat(self.pid).is_some_and(|stat| stat.process_group == self.pid) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "process does not lead its own process group",
            ));
        }
        // SAFETY: a negative pid addresses the process group of its absolute
        // value, which the check above proved this process leads.
        let result = unsafe { libc::kill(-self.pid, signal) };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

/// Who calls `waitpid` for a child.
///
/// This is the single most consequential field on a [`ChildLifecycle`]:
/// `waitpid` is destructive, so exactly one owner in the process may call it
/// for a given pid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReapOwner {
    /// This lifecycle reaps the child. It calls `waitpid`, caches the status,
    /// and its `Drop` guarantees the child is never left a zombie.
    Ours,
    /// Another runtime in this process reaps the child — a glib/vte pane
    /// watch, or a `std::process::Child` the caller still owns.
    ///
    /// A `Foreign` lifecycle **never** calls `waitpid`: doing so would consume
    /// the status the real owner is waiting for, and would release the pid for
    /// reuse while that owner still believes it holds the child. It still
    /// signals safely through [`ProcessRef`] and still runs the escalation
    /// ladder; it simply learns the exit status via
    /// [`ChildLifecycle::note_foreign_exit`] instead of taking it.
    Foreign,
}

/// The SIGHUP → SIGTERM → SIGKILL ladder, its timings, and whether the final
/// SIGKILL sweeps the child's whole PTY session.
///
/// Use [`EscalationPolicy::SESSION_DRAIN`] for a PTY whose child is a session
/// leader, and [`EscalationPolicy::PROCESS_GROUP`] when the caller only owns a
/// process group and must not enumerate `/proc`.
#[derive(Clone, Copy, Debug)]
pub struct EscalationPolicy {
    /// First, polite signal — a PTY hangup the shell can act on.
    pub hangup_signal: c_int,
    /// Grace period before escalating past `hangup_signal`.
    pub term_delay: Duration,
    /// Second signal, still catchable.
    pub term_signal: c_int,
    /// Grace period before the uncatchable signal.
    pub kill_delay: Duration,
    /// Final, uncatchable signal.
    pub kill_signal: c_int,
    /// Upper bound on the final drain (see [`SESSION_KILL_DRAIN_TIMEOUT`]).
    pub drain_timeout: Duration,
    /// Delay between drain scans (see [`SESSION_SCAN_INTERVAL`]).
    pub scan_interval: Duration,
    /// Empty scans required to declare quiescence (see
    /// [`REQUIRED_QUIET_SESSION_SCANS`]).
    pub required_quiet_scans: u8,
    /// Poll interval while waiting for the child to become reapable.
    pub reap_poll_interval: Duration,
    /// Whether the ladder enumerates `/proc` for every member of the child's
    /// session.
    ///
    /// Shell job control puts each foreground command in its own process
    /// group, so a single `kill(-pgid, …)` misses stubborn background jobs
    /// that a `setsid` PTY child still owns. Enumerating the session reaches
    /// them. Callers that own only a process group set this `false` and get
    /// the same ladder without the `/proc` sweep.
    pub drain_session: bool,
}

impl EscalationPolicy {
    const fn ladder(drain_session: bool) -> Self {
        Self {
            hangup_signal: libc::SIGHUP,
            term_delay: Duration::from_millis(120),
            term_signal: libc::SIGTERM,
            kill_delay: Duration::from_millis(250),
            kill_signal: libc::SIGKILL,
            drain_timeout: SESSION_KILL_DRAIN_TIMEOUT,
            scan_interval: SESSION_SCAN_INTERVAL,
            required_quiet_scans: REQUIRED_QUIET_SESSION_SCANS,
            reap_poll_interval: Duration::from_millis(25),
            drain_session,
        }
    }

    /// The full PTY behavior: HUP → TERM → KILL, with the final KILL repeated
    /// across the child's whole session until two consecutive scans find it
    /// empty.
    pub const SESSION_DRAIN: Self = Self::ladder(true);

    /// The same ladder restricted to the child and its process group. No
    /// `/proc` enumeration and no quiet-scan loop, for callers that do not own
    /// a session.
    pub const PROCESS_GROUP: Self = Self::ladder(false);
}

impl Default for EscalationPolicy {
    fn default() -> Self {
        Self::SESSION_DRAIN
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct SessionScan {
    live_members: usize,
    failed_members: usize,
    complete: bool,
}

/// Signal every live process whose session is `session_leader`, excluding the
/// leader itself, and report what the sweep saw.
fn signal_session_members(session_leader: i32, signal: c_int) -> SessionScan {
    let mut scan = SessionScan::default();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return scan;
    };
    scan.complete = true;

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                scan.complete = false;
                continue;
            }
        };
        let Some(member) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        if member == session_leader {
            continue;
        }
        let observed = match process_stat_result(member) {
            Ok(observed) => observed,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => {
                scan.complete = false;
                continue;
            }
        };
        if observed.session != session_leader || !observed.is_live() {
            continue;
        }

        #[cfg(target_os = "linux")]
        {
            // Open the stable process reference before the authoritative second
            // stat read. If the numeric pid was reused at either boundary, the
            // second read will describe a different session and we skip it;
            // pidfd_send_signal can therefore never target that replacement.
            //
            // `open_pidfd` is used directly rather than `ProcessRef::open`:
            // this path must NOT take the ENOSYS/EPERM raw-pid fallback, since
            // these pids were discovered by scanning /proc and are not owned by
            // this process at all.
            match open_pidfd(member) {
                Ok(pidfd) => {
                    let current = match process_stat_result(member) {
                        Ok(current) => current,
                        Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                        Err(_) => {
                            scan.complete = false;
                            scan.failed_members += 1;
                            continue;
                        }
                    };
                    if current.session != session_leader || !current.is_live() {
                        continue;
                    }
                    scan.live_members += 1;
                    if let Err(error) = send_pidfd_signal(&pidfd, signal) {
                        if error.raw_os_error() != Some(libc::ESRCH) {
                            scan.failed_members += 1;
                        }
                    }
                }
                Err(_) => {
                    // Count a still-live member so the bounded final drain keeps
                    // retrying. Never fall back to kill(raw_pid): doing so would
                    // recreate the PID-reuse race this path exists to prevent.
                    match process_stat_result(member) {
                        Ok(current) if current.session == session_leader && current.is_live() => {
                            scan.live_members += 1;
                            scan.failed_members += 1;
                        }
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(_) => {
                            scan.complete = false;
                            scan.failed_members += 1;
                        }
                        Ok(_) => {}
                    }
                }
            }
        }

        #[cfg(not(target_os = "linux"))]
        {
            scan.live_members += 1;
            // SAFETY: kill takes a pid and a signal number by value.
            let result = unsafe { libc::kill(member, signal) };
            if result != 0 {
                scan.failed_members += 1;
            }
        }
    }
    scan
}

/// Deliver `signal` to a child and everything it dragged along: its session
/// (when `drain_session` and the child leads one), otherwise its process
/// group, and always the child itself through the verified [`ProcessRef`].
///
/// Private on purpose. The direct-hit step is the one place a raw pid could
/// leak into a `kill`, and routing it through `ProcessRef` is what keeps that
/// impossible for callers.
fn signal_process_tree(process: &ProcessRef, signal: c_int, drain_session: bool) -> SessionScan {
    let pid = process.pid();
    if pid <= 0 {
        return SessionScan::default();
    }

    let scan = if drain_session && process_stat(pid).is_some_and(|stat| stat.session == pid) {
        signal_session_members(pid, signal)
    } else {
        // Not a session leader (or session drain disabled): the group is the
        // widest safe target. `signal_process_group` verifies leadership.
        let _ = process.signal_process_group(signal);
        SessionScan::default()
    };
    // Always hit the child itself: a shell that has not yet called
    // setsid/setpgid appears in neither sweep above.
    let _ = process.signal(signal);
    scan
}

fn repeat_session_scans_until_quiet<Scan>(
    deadline: Instant,
    interval: Duration,
    required_quiet_scans: u8,
    mut scan: Scan,
) -> bool
where
    Scan: FnMut() -> usize,
{
    let mut quiet_scans = 0u8;
    loop {
        if Instant::now() >= deadline {
            return false;
        }
        if scan() == 0 {
            quiet_scans = quiet_scans.saturating_add(1);
            if quiet_scans >= required_quiet_scans {
                return true;
            }
        } else {
            quiet_scans = 0;
        }

        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        std::thread::sleep(interval.min(deadline.saturating_duration_since(now)));
    }
}

/// Signal the tree, then — for a session leader under a session-draining
/// policy — keep re-signaling until two consecutive scans find the session
/// empty or `policy.drain_timeout` expires.
fn signal_process_tree_until_quiet(process: &ProcessRef, signal: c_int, policy: &EscalationPolicy) {
    let pid = process.pid();
    if pid <= 0 {
        return;
    }
    let is_session_leader =
        policy.drain_session && process_stat(pid).is_some_and(|stat| stat.session == pid);
    let deadline = Instant::now() + policy.drain_timeout;
    let initial = signal_process_tree(process, signal, policy.drain_session);
    if !is_session_leader {
        return;
    }

    let mut remaining = initial.live_members;
    let mut failures = initial.failed_members;
    let mut scan_complete = initial.complete;
    let quiet = repeat_session_scans_until_quiet(
        deadline,
        policy.scan_interval,
        policy.required_quiet_scans,
        || {
            let current = signal_session_members(pid, signal);
            remaining = current.live_members;
            failures = current.failed_members;
            scan_complete = current.complete;
            if current.complete {
                current.live_members
            } else {
                usize::MAX
            }
        },
    );
    if !quiet {
        log::warn!(
            "session {pid} did not reach verified quiescence after the final signal drain \
             ({remaining} live member(s), {failures} safe-signal failure(s), scan complete: {scan_complete})"
        );
    }
}

#[derive(Default)]
struct ChildState {
    exit_code: Option<i32>,
    reaped: bool,
    termination_started: bool,
    cleanup_complete: bool,
}

/// Serializes child reaping with every signal sent during shutdown.
///
/// A raw pid becomes unsafe the instant `waitpid` releases it: the kernel may
/// immediately assign it to an unrelated process. Keeping both operations
/// behind one mutex means no cleanup worker can signal between a successful
/// reap and the corresponding state update — and on Linux the signal is bound
/// to a pidfd on top of that.
///
/// Three properties matter to callers:
/// - **Cached status.** Once the child is reaped its code is remembered, so
///   later drop paths never call `waitpid` a second time on a released pid.
/// - **One escalation, ever.** [`ChildLifecycle::terminate`] returns `false`
///   if termination already began, so two shutdown paths racing on the same
///   child cannot start two escalation threads.
/// - **No leaked zombie.** For [`ReapOwner::Ours`], `Drop` reaps a child that
///   was never terminated, killing it first if necessary. For
///   [`ReapOwner::Foreign`] the real owner is left undisturbed.
pub struct ChildLifecycle {
    process: ProcessRef,
    owner: ReapOwner,
    state: Mutex<ChildState>,
}

impl ChildLifecycle {
    /// Take ownership of the termination path for `pid`.
    ///
    /// `owner` states who calls `waitpid`; read [`ReapOwner`] before choosing,
    /// because getting it wrong either steals another runtime's exit status or
    /// leaks a zombie.
    ///
    /// Fails when `pid` is not positive, or when the process cannot be
    /// referenced at all (`ESRCH` — it is already gone, so its number may
    /// already belong to somebody else). A merely unavailable `pidfd_open`
    /// degrades to the raw-pid fallback instead, reported by
    /// [`ChildLifecycle::is_kernel_bound`].
    pub fn new(pid: i32, owner: ReapOwner) -> io::Result<Arc<Self>> {
        // A non-positive pid must never reach waitpid: 0, -1 and negatives all
        // mean "some other set of children" there, not "this child".
        let process = ProcessRef::open(pid)?;
        Ok(Arc::new(Self {
            process,
            owner,
            state: Mutex::new(ChildState::default()),
        }))
    }

    /// The child's pid, for logging and `/proc` probes.
    pub fn pid(&self) -> i32 {
        self.process.pid()
    }

    /// Who reaps this child.
    pub fn owner(&self) -> ReapOwner {
        self.owner
    }

    /// Whether signals to this child are immune to pid reuse.
    pub fn is_kernel_bound(&self) -> bool {
        self.process.is_kernel_bound()
    }

    /// The cached exit code, if the status has already been observed. Never
    /// calls `waitpid`, so it is safe on any thread and after any drop path.
    pub fn exit_code(&self) -> Option<i32> {
        self.state().exit_code
    }

    fn state(&self) -> MutexGuard<'_, ChildState> {
        self.state.lock().unwrap_or_else(|poisoned| {
            log::error!("child lifecycle lock was poisoned; recovering its state");
            poisoned.into_inner()
        })
    }

    fn poll_reap_locked(&self, state: &mut ChildState) -> Option<i32> {
        if state.reaped {
            return Some(state.exit_code.unwrap_or(1));
        }
        if self.owner == ReapOwner::Foreign {
            // Another runtime is blocked in waitpid on this pid. Reaping here
            // would consume the status it is waiting for and release the pid
            // for reuse behind its back.
            return None;
        }
        // During explicit shutdown the leader intentionally remains a zombie
        // until HUP → TERM → KILL has been delivered to its whole PTY session.
        // Retaining the pid prevents the kernel from reusing the session id
        // while the cleanup worker is still signaling descendants.
        if state.termination_started && !state.cleanup_complete {
            return None;
        }

        let mut status: c_int = 0;
        // SAFETY: `status` is a live c_int for the duration of the call.
        let waited = unsafe { libc::waitpid(self.process.pid(), &mut status, libc::WNOHANG) };
        if waited > 0 {
            // A terminal status; a stop/continue report leaves the child alive.
            let code = exit_code_from_wait_status(status)?;
            state.exit_code = Some(code);
            state.reaped = true;
            return Some(code);
        }
        if waited == 0 {
            // WNOHANG and no state change: still running.
            return None;
        }

        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ECHILD) => {
                // Another in-process owner should not reap this child, but if
                // it did, the pid is no longer ours and must never be signaled.
                state.exit_code = Some(1);
                state.reaped = true;
                Some(1)
            }
            Some(libc::EINTR) => None,
            _ => {
                log::warn!("failed to poll child {}: {error}", self.process.pid());
                None
            }
        }
    }

    /// Reap the child without blocking, returning its exit code once known.
    ///
    /// Always `None` for [`ReapOwner::Foreign`] until
    /// [`ChildLifecycle::note_foreign_exit`] supplies the status.
    pub fn poll_reap(&self) -> Option<i32> {
        let mut state = self.state();
        self.poll_reap_locked(&mut state)
    }

    /// Record the exit code a foreign reaper observed (see
    /// [`exit_code_from_wait_status`] to normalize a raw `waitpid` status).
    ///
    /// This retires the pid: after it, the lifecycle reports the status from
    /// its cache and refuses to send any further signal, because the number is
    /// now free for reuse. Ignored — with a log line — for
    /// [`ReapOwner::Ours`], whose status comes from its own `waitpid`.
    pub fn note_foreign_exit(&self, exit_code: i32) {
        if self.owner != ReapOwner::Foreign {
            log::error!(
                "ignoring a foreign exit status for child {}, which this process reaps itself",
                self.process.pid()
            );
            return;
        }
        let mut state = self.state();
        if state.reaped {
            return;
        }
        state.exit_code = Some(exit_code);
        state.reaped = true;
        state.cleanup_complete = true;
    }

    /// Send `signal` to the child, refusing once its status has been observed.
    ///
    /// The refusal is the point: after the status is known the pid may already
    /// belong to another process.
    pub fn signal(&self, signal: c_int) -> io::Result<()> {
        let state = self.state();
        if state.reaped {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "child has already been reaped",
            ));
        }
        self.process.signal(signal)
    }

    /// Send `signal` to the child's process group, refusing once its status
    /// has been observed and unless `/proc` confirms it leads that group.
    pub fn signal_process_group(&self, signal: c_int) -> io::Result<()> {
        let state = self.state();
        if state.reaped {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "child has already been reaped",
            ));
        }
        self.process.signal_process_group(signal)
    }

    fn signal_during_cleanup(&self, signal: c_int, policy: &EscalationPolicy) -> bool {
        // The guard is held across the signal: that is the invariant.
        let state = self.state();
        if state.reaped {
            return false;
        }
        signal_process_tree(&self.process, signal, policy.drain_session);
        true
    }

    fn kill_during_cleanup(&self, policy: &EscalationPolicy) -> bool {
        let state = self.state();
        if state.reaped {
            return false;
        }
        signal_process_tree_until_quiet(&self.process, policy.kill_signal, policy);
        true
    }

    fn begin_termination(&self, policy: &EscalationPolicy) -> bool {
        let mut state = self.state();
        if state.reaped || state.termination_started {
            return false;
        }
        state.termination_started = true;
        signal_process_tree(&self.process, policy.hangup_signal, policy.drain_session);
        true
    }

    fn finish_termination_and_reap(&self, policy: &EscalationPolicy) {
        {
            let mut state = self.state();
            state.cleanup_complete = true;
        }
        if self.owner == ReapOwner::Foreign {
            // The foreign owner's watch delivers the status; polling here
            // would either spin forever or steal it.
            return;
        }
        while self.poll_reap().is_none() {
            std::thread::sleep(policy.reap_poll_interval);
        }
    }

    /// Start the escalation ladder on a background thread and return `true`.
    ///
    /// Returns `false` — doing nothing — when the child was already reaped or
    /// termination already began. That makes a second call from a racing
    /// shutdown path (a widget drop concurrent with an explicit close, say)
    /// harmless: exactly one escalation thread can ever exist per child.
    ///
    /// The ladder runs off the caller's thread so a UI main loop is never
    /// blocked by the grace periods. For [`ReapOwner::Ours`] the worker holds
    /// an `Arc` until the child is reaped, so the lifecycle outlives the
    /// caller's handle and `Drop` finds nothing left to do.
    pub fn terminate(self: &Arc<Self>, policy: EscalationPolicy) -> bool {
        if !self.begin_termination(&policy) {
            return false;
        }
        let worker_child = Arc::clone(self);
        if let Err(error) = std::thread::Builder::new()
            .name("jterm-process-cleanup".to_string())
            .spawn(move || {
                std::thread::sleep(policy.term_delay);
                worker_child.signal_during_cleanup(policy.term_signal, &policy);
                std::thread::sleep(policy.kill_delay);
                worker_child.kill_during_cleanup(&policy);
                worker_child.finish_termination_and_reap(&policy);
            })
        {
            log::error!("failed to start the process cleanup worker: {error}");
            self.force_kill_and_reap_with(&policy);
        }
        true
    }

    /// Kill the child now and, for [`ReapOwner::Ours`], block until it is
    /// reaped — skipping the ladder's grace periods entirely.
    ///
    /// Intended for the paths where no concurrent reader exists to observe the
    /// exit (a reader thread that could not be spawned, an aborted setup), and
    /// used by `Drop` for a child that was never terminated. Every later drop
    /// path becomes a no-op because the status is cached.
    ///
    /// For [`ReapOwner::Foreign`] this kills and drains but does not wait; the
    /// foreign owner still delivers the status.
    pub fn force_kill_and_reap(&self) {
        self.force_kill_and_reap_with(&EscalationPolicy::SESSION_DRAIN);
    }

    fn force_kill_and_reap_with(&self, policy: &EscalationPolicy) {
        let mut state = self.state();
        if state.reaped {
            return;
        }
        state.termination_started = true;
        signal_process_tree_until_quiet(&self.process, policy.kill_signal, policy);
        if self.owner == ReapOwner::Foreign {
            state.cleanup_complete = true;
            return;
        }
        let code = blocking_reap(self.process.pid());
        state.exit_code = Some(code);
        state.reaped = true;
        state.cleanup_complete = true;
    }
}

impl Drop for ChildLifecycle {
    fn drop(&mut self) {
        if self.owner == ReapOwner::Foreign {
            // Not ours to wait for, and not ours to kill on the way out.
            return;
        }
        // Normally the escalation worker already reaped the child and this is
        // a cached lookup. Otherwise the child was never terminated, and
        // letting it go would leak a zombie for the process' lifetime.
        if self.poll_reap().is_some() {
            return;
        }
        self.force_kill_and_reap_with(&EscalationPolicy::SESSION_DRAIN);
    }
}

/// Block until `pid` yields a terminal status, retrying through `EINTR`.
///
/// Returns `1` for an unexpected `waitpid` failure (including `ECHILD`, where
/// someone else already reaped the child) so callers always get a code.
fn blocking_reap(pid: i32) -> i32 {
    loop {
        let mut status: c_int = 0;
        // SAFETY: `status` is a live c_int for the duration of the call.
        let waited = unsafe { libc::waitpid(pid, &mut status, 0) };
        if waited > 0 {
            if let Some(code) = exit_code_from_wait_status(status) {
                return code;
            }
            // A stop/continue report; keep waiting for the terminal status.
            continue;
        }
        if waited == 0 {
            // POSIX only allows 0 with WNOHANG, which is not set here.
            continue;
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        log::warn!("failed to reap child {pid}: {error}");
        return 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn single_quote_escapes_embedded_quotes_and_keeps_empty_visible() {
        assert_eq!(shell_single_quote("/tmp/it's"), "'/tmp/it'\"'\"'s'");
        assert_eq!(shell_single_quote(""), "''");
        assert_eq!(shell_single_quote("plain"), "'plain'");
    }

    #[test]
    fn path_quoting_leaves_safe_paths_readable() {
        assert_eq!(
            shell_quote_path("/home/user/src-1.2/x_y"),
            "/home/user/src-1.2/x_y"
        );
        assert_eq!(shell_quote_path("~/notes"), "~/notes");
        assert_eq!(shell_quote_path("a b"), "'a b'");
        assert_eq!(shell_quote_path("it's"), "'it'\"'\"'s'");
        assert_eq!(shell_quote_path(""), "''");
    }

    #[test]
    fn jsh_exec_command_quotes_shell_and_session() {
        assert_eq!(
            build_jsh_exec_command("/tmp/it's/jsh", None),
            "exec '/tmp/it'\"'\"'s/jsh'"
        );
        assert_eq!(
            build_jsh_exec_command("/usr/bin/jsh", Some("123-456")),
            "exec '/usr/bin/jsh' --session '123-456'"
        );
        assert_eq!(
            build_jsh_exec_command("/usr/bin/jsh", Some("it's; printf injected")),
            "exec '/usr/bin/jsh' --session 'it'\"'\"'s; printf injected'"
        );
    }

    #[test]
    fn restorable_commands_preserve_original_argv_boundaries() {
        let args = argv(&["ssh", "host", "printf '%s, %s; still remote' one two"]);
        let restored = match_restorable_command(&args).expect("ssh argv is restorable");
        assert_eq!(restored.len(), 3);
        assert_eq!(restored[2], "printf '%s, %s; still remote' one two");
    }

    #[test]
    fn restored_argv_is_shell_quoted_as_one_safe_command() {
        let args = argv(&["ssh", "host", "echo it's; touch /tmp/pwned"]);
        let quoted = shell_quote_argv(&args).expect("no control characters");
        assert_eq!(quoted, "'ssh' 'host' 'echo it'\"'\"'s; touch /tmp/pwned'");
        assert!(shell_quote_argv(&[]).is_none());
    }

    #[test]
    fn restored_argv_rejects_pty_control_characters() {
        assert!(shell_quote_argv(&argv(&["ssh", "host\n"])).is_none());
        assert!(shell_quote_argv(&argv(&["ssh", "\x1b]0;x\x07"])).is_none());
    }

    /// Stand-in for the frontends' snapshot leaf: only the field attribute
    /// matters, and it is the attribute this test is here to pin.
    #[derive(Deserialize, Debug, PartialEq, Eq)]
    struct StoredPane {
        #[serde(default, deserialize_with = "deserialize_restorable_argv")]
        cmds: Option<Vec<String>>,
    }

    fn stored_pane(json: &str) -> StoredPane {
        serde_json::from_str(json).expect("snapshot leaf must stay loadable")
    }

    #[test]
    fn restorable_argv_deserializes_from_the_argv_form() {
        assert_eq!(
            stored_pane(r#"{"cmds":["ssh","host","echo a; rm b"]}"#).cmds,
            Some(argv(&["ssh", "host", "echo a; rm b"]))
        );
    }

    #[test]
    fn legacy_joined_command_loads_but_is_never_replayed() {
        // The whole point: the snapshot still loads (its tabs, cwds and layout
        // are good) but the one field whose argument boundaries were destroyed
        // by `argv.join(" ")` is dropped rather than re-split into a different
        // command than the user ran.
        assert_eq!(
            stored_pane(r#"{"cmds":"ssh host echo a; rm b"}"#).cmds,
            None
        );
    }

    #[test]
    fn missing_null_and_empty_restorable_commands_are_all_none() {
        for json in ["{}", r#"{"cmds":null}"#, r#"{"cmds":[]}"#, r#"{"cmds":""}"#] {
            assert_eq!(stored_pane(json).cmds, None, "{json}");
        }
    }

    #[test]
    fn a_malformed_restorable_command_fails_the_whole_field() {
        // Neither shape: not silently swallowed, because a number or object here
        // means the snapshot was written by something that is not this family.
        assert!(serde_json::from_str::<StoredPane>(r#"{"cmds":7}"#).is_err());
        assert!(serde_json::from_str::<StoredPane>(r#"{"cmds":[1,2]}"#).is_err());
    }

    #[test]
    fn restored_argv_uses_the_configured_shell_grammar() {
        let args = argv(&["ssh", "it's"]);
        assert_eq!(
            shell_quote_argv_for(&args, &argv(&["/usr/bin/pwsh", "-l"])),
            Some("& 'ssh' 'it''s'".to_string())
        );
        assert_eq!(
            shell_quote_argv_for(&args, &argv(&["/bin/zsh"])),
            Some("'ssh' 'it'\"'\"'s'".to_string())
        );
        assert_eq!(
            shell_quote_argv_for(&args, &argv(&["/opt/exotic-shell"])),
            None
        );
    }

    #[test]
    fn external_cwd_and_block_requirements_are_classified_separately() {
        assert!(command_uses_external_cwd(&argv(&[
            "docker", "exec", "-it", "x", "sh"
        ])));
        assert!(!command_requires_block_integration(&argv(&[
            "docker", "exec", "-it", "x", "sh"
        ])));
        assert!(command_uses_external_cwd(&argv(&["/usr/bin/ssh", "host"])));
        assert!(command_requires_block_integration(&argv(&[
            "/usr/bin/ssh",
            "host"
        ])));
        assert!(command_uses_external_cwd(&argv(&[
            "mosh-client",
            "1.2.3.4",
            "60001"
        ])));
        // The flatpak-spawn wrapper is unwrapped before classification.
        assert!(command_uses_external_cwd(&argv(&[
            "/usr/bin/flatpak-spawn",
            "--host",
            "--watch-bus",
            "--directory=/home/x",
            "--env=TERM=xterm-256color",
            "--env=COLORTERM=truecolor",
            "ssh",
            "host",
        ])));
        assert!(!command_uses_external_cwd(&argv(&["bash", "-l"])));
    }

    #[test]
    fn foreground_external_cwd_classification_is_conservative_and_walks_ancestry() {
        let shell = 100;
        let table_argv = |pid: i32| -> Option<Vec<String>> {
            match pid {
                100 => Some(argv(&["zsh"])),
                200 => Some(argv(&["make"])),
                201 => Some(argv(&["docker", "exec", "-it", "x", "sh"])),
                202 => Some(argv(&["sh"])),
                _ => None,
            }
        };
        // Foreground is the shell: local.
        assert_eq!(
            classify_foreground_external_cwd(shell, shell, table_argv, |_| None),
            Some(false)
        );
        // docker exec found mid-ancestry: external.
        assert_eq!(
            classify_foreground_external_cwd(shell, 202, table_argv, |pid| match pid {
                202 => Some(201),
                201 => Some(100),
                _ => None,
            }),
            Some(true)
        );
        // Plain local job whose ancestry reaches the shell: local.
        assert_eq!(
            classify_foreground_external_cwd(shell, 200, table_argv, |pid| (pid == 200)
                .then_some(100)),
            Some(false)
        );
        // Unreadable /proc: undecided, keep the sticky classification.
        assert_eq!(
            classify_foreground_external_cwd(shell, 999, table_argv, |_| None),
            None
        );
        // Wrapper shell argv proves nothing either way.
        let wrapper_argv = |pid: i32| -> Option<Vec<String>> {
            (pid == 100).then(|| argv(&["flatpak-spawn", "--host", "--watch-bus", "zsh"]))
        };
        assert_eq!(
            classify_foreground_external_cwd(shell, 200, wrapper_argv, |_| None),
            None
        );
        // The managed-remote case: ssh IS the PTY child.
        let ssh_shell = |pid: i32| (pid == 100).then(|| argv(&["ssh", "host"]));
        assert_eq!(
            classify_foreground_external_cwd(shell, 200, ssh_shell, |_| None),
            Some(true)
        );
    }

    #[test]
    fn proc_stat_parser_keeps_fields_with_tricky_names() {
        let stat = "123 (name with ) parens) S 1 77 88 34816 4242 0 0";
        let parsed = parse_process_stat(stat).expect("parses");
        assert_eq!(parsed.state, 'S');
        assert_eq!(parsed.process_group, 77);
        assert_eq!(parsed.session, 88);
        assert_eq!(parsed.foreground_group, 4242);
        assert!(parsed.is_live());

        let zombie = parse_process_stat("9 (z) Z 1 9 9 0 -1 0 0").unwrap();
        assert!(!zombie.is_live());
        assert_eq!(zombie.foreground_group, -1);

        assert!(parse_process_stat("garbage").is_none());
        assert!(parse_process_stat("1 (short) S 1").is_none());
    }

    #[test]
    fn stat_probes_reject_invalid_pids_before_touching_proc() {
        assert!(process_stat_result(0).is_err());
        assert!(process_stat_result(-4).is_err());
        assert!(read_ppid(0).is_none());
        assert!(read_proc_cmdline(-1).is_none());
        assert!(process_comm(0).is_none());
        assert!(process_cwd(0).is_none());
        assert!(foreground_pgid_via_stat(0).is_none());
    }

    #[test]
    fn self_probes_read_this_process() {
        let pid = std::process::id() as i32;
        let args = read_proc_cmdline(pid).expect("own cmdline is readable");
        assert!(!args.is_empty());
        assert!(process_comm(pid).is_some());
        assert!(process_cwd(pid).is_some());
        let stat = process_stat(pid).expect("own stat is readable");
        assert!(stat.is_live());
    }
}

/// Lifecycle tests spawn real short-lived processes: the invariants under test
/// (pid reuse, session drains, who owns `waitpid`) exist only against a real
/// kernel and cannot be observed through a fake.
#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use crate::exit_status::signal_name_for_exit;
    use std::os::unix::process::ExitStatusExt;
    use std::process::{Child, Command};
    use std::time::{Duration, Instant};

    /// A child that outlives the test only if the test fails.
    #[allow(clippy::zombie_processes)] // Each test decides who reaps.
    fn spawn_sleeper() -> Child {
        Command::new("sh")
            .args(["-c", "exec sleep 30"])
            .spawn()
            .expect("spawn test child")
    }

    fn wait_for<Ready: FnMut() -> bool>(timeout: Duration, mut ready: Ready) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if ready() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// True when `pid` is not a reapable child of this process any more.
    fn is_fully_reaped(pid: i32) -> bool {
        let mut status: c_int = 0;
        let waited = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        waited < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD)
    }

    #[cfg(target_os = "linux")]
    fn pidfd_is_unavailable(error: &io::Error) -> bool {
        matches!(error.raw_os_error(), Some(libc::ENOSYS) | Some(libc::EPERM))
    }

    /// A `sh` in a session of its own with one background sleeper in it — the
    /// shape a PTY child has, and the only shape where a session sweep does
    /// something a `kill(-pgid, …)` would not.
    #[cfg(target_os = "linux")]
    struct SessionLeader {
        child: Option<Child>,
        leader: i32,
    }

    #[cfg(target_os = "linux")]
    impl SessionLeader {
        fn spawn() -> Self {
            use std::os::unix::process::CommandExt;

            let mut command = Command::new("sh");
            command.args(["-c", "sleep 30 & wait"]);
            // SAFETY: setsid is async-signal-safe and the closure performs no
            // allocation or other work after Command forks.
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() < 0 {
                        Err(io::Error::last_os_error())
                    } else {
                        Ok(())
                    }
                });
            }
            let child = command.spawn().expect("spawn session leader");
            let leader = child.id() as i32;
            Self {
                child: Some(child),
                leader,
            }
        }

        fn live_members(&self) -> Vec<i32> {
            std::fs::read_dir("/proc")
                .expect("read proc")
                .flatten()
                .filter_map(|entry| entry.file_name().to_str()?.parse::<i32>().ok())
                .filter(|pid| {
                    *pid != self.leader
                        && process_stat(*pid)
                            .is_some_and(|stat| stat.session == self.leader && stat.is_live())
                })
                .collect()
        }

        /// Block until the background sleeper has joined the session.
        fn await_member(&self) -> i32 {
            let mut member = None;
            let appeared = wait_for(Duration::from_secs(2), || {
                member = self.live_members().first().copied();
                member.is_some()
            });
            assert!(appeared, "background session member did not appear");
            member.expect("a member was observed")
        }

        fn wait(&mut self) -> std::process::ExitStatus {
            self.child
                .take()
                .expect("the leader has not been reaped yet")
                .wait()
                .expect("reap session leader")
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for SessionLeader {
        fn drop(&mut self) {
            // Only while the pid is still held: once reaped it may be reused.
            if let Some(mut child) = self.child.take() {
                unsafe {
                    libc::kill(-self.leader, libc::SIGKILL);
                    libc::kill(self.leader, libc::SIGKILL);
                }
                let _ = child.wait();
            }
        }
    }

    // -- hoisted from jterm1 ------------------------------------------------

    #[test]
    fn session_drain_requires_quiet_after_a_late_fork_appears() {
        let mut scans = std::collections::VecDeque::from([1usize, 0, 1, 0, 0]);
        let mut calls = 0usize;
        let quiet = repeat_session_scans_until_quiet(
            Instant::now() + Duration::from_secs(1),
            Duration::ZERO,
            REQUIRED_QUIET_SESSION_SCANS,
            || {
                calls += 1;
                scans.pop_front().unwrap_or_default()
            },
        );
        assert!(quiet);
        assert_eq!(calls, 5);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pidfd_signal_is_bound_to_the_opened_process() {
        let mut child = spawn_sleeper();
        let pidfd = match open_pidfd(child.id() as i32) {
            Ok(pidfd) => pidfd,
            Err(error) if pidfd_is_unavailable(&error) => {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("pidfd_open failed: {error}");
            }
        };
        if let Err(error) = send_pidfd_signal(&pidfd, libc::SIGKILL) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("pidfd_send_signal failed: {error}");
        }
        let status = child.wait().expect("reap pidfd target");
        assert_eq!(status.signal(), Some(libc::SIGKILL));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn final_drain_kills_a_background_member_of_the_child_session() {
        let mut session = SessionLeader::spawn();
        let member = session.await_member();
        if let Err(error) = open_pidfd(member) {
            assert!(
                pidfd_is_unavailable(&error),
                "pidfd_open for session member failed: {error}"
            );
            return;
        }

        let leader_ref = ProcessRef::open(session.leader).expect("reference the session leader");
        signal_process_tree_until_quiet(
            &leader_ref,
            libc::SIGKILL,
            &EscalationPolicy::SESSION_DRAIN,
        );
        let survivors = session.live_members();
        let status = session.wait();
        assert_eq!(status.signal(), Some(libc::SIGKILL));
        assert!(
            survivors.is_empty(),
            "live descendants {survivors:?} remained in the drained PTY session"
        );
    }

    #[test]
    #[allow(clippy::zombie_processes)] // ChildLifecycle, not Child, owns waitpid here.
    fn child_lifecycle_caches_reaped_status_for_later_drop_paths() {
        let child = Command::new("sh")
            .args(["-c", "exit 23"])
            .spawn()
            .expect("spawn exiting child");
        let lifecycle =
            ChildLifecycle::new(child.id() as i32, ReapOwner::Ours).expect("reference the child");
        assert!(
            wait_for(Duration::from_secs(2), || lifecycle.poll_reap().is_some()),
            "child was not reaped before the test deadline"
        );
        assert_eq!(lifecycle.poll_reap(), Some(23));
        // The cached status is what makes every later drop path safe.
        assert_eq!(lifecycle.exit_code(), Some(23));
        assert_eq!(lifecycle.poll_reap(), Some(23));
    }

    // -- new surface --------------------------------------------------------

    #[test]
    fn the_presets_share_one_ladder_and_differ_only_in_the_session_sweep() {
        let drain = EscalationPolicy::SESSION_DRAIN;
        let group = EscalationPolicy::PROCESS_GROUP;
        assert!(drain.drain_session);
        assert!(!group.drain_session);
        assert!(EscalationPolicy::default().drain_session);
        // jterm1's timings, which both presets inherit.
        assert_eq!(drain.hangup_signal, libc::SIGHUP);
        assert_eq!(drain.term_signal, libc::SIGTERM);
        assert_eq!(drain.kill_signal, libc::SIGKILL);
        assert_eq!(drain.drain_timeout, SESSION_KILL_DRAIN_TIMEOUT);
        assert_eq!(drain.scan_interval, SESSION_SCAN_INTERVAL);
        assert_eq!(drain.required_quiet_scans, REQUIRED_QUIET_SESSION_SCANS);
        assert_eq!(group.hangup_signal, drain.hangup_signal);
        assert_eq!(group.term_delay, drain.term_delay);
        assert_eq!(group.kill_delay, drain.kill_delay);
        assert_eq!(group.drain_timeout, drain.drain_timeout);
    }

    /// The defining property of the `PROCESS_GROUP` preset: a caller that owns
    /// only a process group must not have its shutdown enumerate `/proc` and
    /// signal processes it never spawned.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_process_group_preset_never_sweeps_the_session() {
        let session = SessionLeader::spawn();
        session.await_member();
        let leader_ref = ProcessRef::open(session.leader).expect("reference the session leader");

        // Signal 0 probes for existence without disturbing anything.
        let group_only = signal_process_tree(&leader_ref, 0, false);
        assert!(
            !group_only.complete && group_only.live_members == 0,
            "PROCESS_GROUP must not enumerate /proc: {group_only:?}"
        );

        let swept = signal_process_tree(&leader_ref, 0, true);
        assert!(swept.complete, "SESSION_DRAIN must enumerate /proc");
        assert!(
            swept.live_members >= 1,
            "the session sweep must see the background member"
        );

        // Nothing was killed by a null signal; the fixture's Drop cleans up.
        assert!(!session.live_members().is_empty());
    }

    #[test]
    fn terminate_is_idempotent_and_starts_exactly_one_escalation() {
        let child = spawn_sleeper();
        let pid = child.id() as i32;
        std::mem::forget(child); // The lifecycle owns waitpid from here on.
        let lifecycle = ChildLifecycle::new(pid, ReapOwner::Ours).expect("reference the child");

        let policy = EscalationPolicy {
            term_delay: Duration::from_millis(5),
            kill_delay: Duration::from_millis(5),
            drain_timeout: Duration::from_millis(50),
            reap_poll_interval: Duration::from_millis(5),
            ..EscalationPolicy::PROCESS_GROUP
        };
        assert!(
            lifecycle.terminate(policy),
            "first terminate runs the ladder"
        );
        assert!(
            !lifecycle.terminate(policy),
            "a second terminate must not start a second escalation thread"
        );

        assert!(
            wait_for(Duration::from_secs(5), || lifecycle.exit_code().is_some()),
            "the escalation worker did not reap the child"
        );
        let code = lifecycle.exit_code().expect("the worker cached a status");
        assert!(
            matches!(
                signal_name_for_exit(code),
                Some("SIGHUP") | Some("SIGTERM") | Some("SIGKILL")
            ),
            "expected a ladder signal code, got {code}"
        );
        // Terminating an already-reaped child is a no-op too.
        assert!(!lifecycle.terminate(policy));
        assert!(lifecycle.signal(libc::SIGKILL).is_err());
    }

    #[test]
    fn foreign_owner_never_reaps_the_childs_status() {
        let mut child = Command::new("sh")
            .args(["-c", "exit 23"])
            .spawn()
            .expect("spawn exiting child");
        let lifecycle = ChildLifecycle::new(child.id() as i32, ReapOwner::Foreign)
            .expect("reference the child");

        // Poll well past the child's exit; a Foreign lifecycle must never call
        // waitpid, so it can never learn the status this way.
        let stole = wait_for(Duration::from_millis(300), || {
            lifecycle.poll_reap().is_some()
        });
        assert!(!stole, "a Foreign lifecycle called waitpid");

        // The real owner still finds the status waiting for it.
        let status = child.wait().expect("the foreign owner reaps its own child");
        assert_eq!(status.code(), Some(23));

        assert_eq!(lifecycle.poll_reap(), None);
        lifecycle.note_foreign_exit(23);
        assert_eq!(lifecycle.poll_reap(), Some(23));
        assert_eq!(lifecycle.exit_code(), Some(23));
        // A retired pid may already belong to somebody else.
        assert!(lifecycle.signal(libc::SIGTERM).is_err());
        // Dropping must not waitpid a pid the foreign owner already released.
        drop(lifecycle);
    }

    #[test]
    fn foreign_force_kill_leaves_the_status_for_its_owner() {
        let mut child = spawn_sleeper();
        let lifecycle = ChildLifecycle::new(child.id() as i32, ReapOwner::Foreign)
            .expect("reference the child");
        lifecycle.force_kill_and_reap();
        assert_eq!(
            lifecycle.exit_code(),
            None,
            "a Foreign force-kill must not consume the status"
        );
        let status = child.wait().expect("the foreign owner reaps its own child");
        assert_eq!(status.signal(), Some(libc::SIGKILL));
        drop(lifecycle);
    }

    #[test]
    fn drop_reaps_a_child_that_was_never_terminated() {
        let child = spawn_sleeper();
        let pid = child.id() as i32;
        std::mem::forget(child); // The lifecycle owns waitpid from here on.
        let lifecycle = ChildLifecycle::new(pid, ReapOwner::Ours).expect("reference the child");
        assert_eq!(lifecycle.poll_reap(), None);
        drop(lifecycle);
        assert!(
            is_fully_reaped(pid),
            "Drop left a zombie behind for pid {pid}"
        );
    }

    #[test]
    fn process_ref_rejects_pids_that_cannot_be_signaled_safely() {
        assert_eq!(
            ProcessRef::open(0).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            ProcessRef::open(-1).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(ChildLifecycle::new(0, ReapOwner::Ours).is_err());
        assert!(ChildLifecycle::new(-1, ReapOwner::Foreign).is_err());
    }

    #[test]
    fn process_ref_falls_back_to_a_raw_pid_when_pidfd_is_unavailable() {
        let mut child = spawn_sleeper();
        let pid = child.id() as i32;

        let opened = ProcessRef::open(pid).expect("reference a live child");
        assert_eq!(opened.pid(), pid);
        #[cfg(target_os = "linux")]
        {
            // Kernel-bound unless pidfd_open is unavailable, in which case
            // `open` already degraded to the fallback under test below.
            assert_eq!(opened.is_kernel_bound(), open_pidfd(pid).is_ok());
        }
        drop(opened);

        // The path taken on non-Linux and on ENOSYS/EPERM kernels: no pidfd,
        // signals go straight to the pid, and callers can tell.
        let fallback = ProcessRef::unverified(pid);
        assert!(!fallback.is_kernel_bound());
        assert_eq!(fallback.pid(), pid);
        fallback
            .signal(libc::SIGKILL)
            .expect("the raw-pid fallback delivers signals");
        let status = child.wait().expect("reap the signaled child");
        assert_eq!(status.signal(), Some(libc::SIGKILL));
    }

    #[test]
    fn process_group_signals_require_verified_group_leadership() {
        let mut child = spawn_sleeper();
        let pid = child.id() as i32;
        let process = ProcessRef::open(pid).expect("reference a live child");

        // A plain Command child inherits this process' group, so it leads no
        // group of its own and the negative-pid kill must be refused.
        if process_stat(pid).is_some_and(|stat| stat.process_group != pid) {
            assert_eq!(
                process
                    .signal_process_group(libc::SIGKILL)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidInput
            );
        }
        process.signal(libc::SIGKILL).expect("signal the child");
        let status = child.wait().expect("reap the signaled child");
        assert_eq!(status.signal(), Some(libc::SIGKILL));
    }

    #[test]
    #[allow(clippy::zombie_processes)] // ChildLifecycle, not Child, owns waitpid here.
    fn exit_codes_follow_the_familys_signal_offset_convention() {
        let exited = Command::new("sh")
            .args(["-c", "exit 23"])
            .spawn()
            .expect("spawn exiting child");
        let lifecycle =
            ChildLifecycle::new(exited.id() as i32, ReapOwner::Ours).expect("reference the child");
        assert!(wait_for(Duration::from_secs(2), || lifecycle
            .poll_reap()
            .is_some()));
        assert_eq!(lifecycle.poll_reap(), Some(23));
        assert_eq!(signal_name_for_exit(23), None, "23 is a plain exit code");

        let signaled = spawn_sleeper();
        let pid = signaled.id() as i32;
        std::mem::forget(signaled); // The lifecycle owns waitpid from here on.
        let lifecycle = ChildLifecycle::new(pid, ReapOwner::Ours).expect("reference the child");
        lifecycle.signal(libc::SIGKILL).expect("signal the child");
        assert!(wait_for(Duration::from_secs(2), || lifecycle
            .poll_reap()
            .is_some()));
        let code = lifecycle.poll_reap().expect("a cached status");
        assert_eq!(code, 128 + libc::SIGKILL);
        assert_eq!(
            signal_name_for_exit(code),
            Some("SIGKILL"),
            "the 128+signal convention must round-trip through exit_status"
        );
    }

    #[test]
    fn stops_and_continues_are_not_terminal_statuses() {
        let mut child = spawn_sleeper();
        let pid = child.id() as i32;
        assert_eq!(unsafe { libc::kill(pid, libc::SIGSTOP) }, 0);

        let mut status: c_int = 0;
        let waited = loop {
            let waited = unsafe { libc::waitpid(pid, &mut status, libc::WUNTRACED) };
            if waited < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            break waited;
        };
        assert_eq!(waited, pid);
        assert!(libc::WIFSTOPPED(status));
        assert_eq!(
            exit_code_from_wait_status(status),
            None,
            "a stop report must never be mistaken for an exit"
        );

        // SIGKILL reaches a stopped process without SIGCONT.
        assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
        let status = child.wait().expect("reap the killed child");
        assert_eq!(status.signal(), Some(libc::SIGKILL));
    }
}
