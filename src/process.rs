//! UI-independent process inspection and shell quoting shared by the jterm
//! family. Seeded from jterm1's hardened `process.rs`, the only copy whose
//! tests pin the edge cases the other terminals' local copies got wrong.
//!
//! Three concerns live here because they feed each other:
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

use std::io;
use std::path::Path;

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

/// Build the `exec` line that replaces a bootstrap shell with rsh, optionally
/// resuming a saved session. Both fields are quoted as single arguments.
pub fn build_rsh_exec_command(shell_path: &str, session_id: Option<&str>) -> String {
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
    // too; ordinary bash/zsh/rsh children do not match and continue below.
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
    fn rsh_exec_command_quotes_shell_and_session() {
        assert_eq!(
            build_rsh_exec_command("/tmp/it's/rsh", None),
            "exec '/tmp/it'\"'\"'s/rsh'"
        );
        assert_eq!(
            build_rsh_exec_command("/usr/bin/rsh", Some("123-456")),
            "exec '/usr/bin/rsh' --session '123-456'"
        );
        assert_eq!(
            build_rsh_exec_command("/usr/bin/rsh", Some("it's; printf injected")),
            "exec '/usr/bin/rsh' --session 'it'\"'\"'s; printf injected'"
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
