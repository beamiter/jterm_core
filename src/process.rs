//! UI-independent process inspection and shell quoting shared by the jterm
//! family. Seeded from anvil's hardened `process.rs`, the only copy whose
//! tests pin the edge cases the other terminals' local copies got wrong.
//!
//! The process and shell integration policies feed each other:
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
//!   after the kernel is free to reuse it. The runtime owns the implementation.

#[cfg(test)]
use serde::Deserialize;
use std::path::Path;

const MAX_PROCESS_LABEL_CHARS: usize = 128;

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

/// Checked form of [`shell_quote_path`]. Refuses terminal controls and Unicode
/// formatting/spacing characters that could make the visible path disagree
/// with the bytes inserted into the shell editor.
pub fn try_shell_quote_path(s: &str) -> Option<String> {
    if s.chars().any(char::is_control) || crate::review_input::contains_visual_spoofing(s) {
        return None;
    }
    if s.is_empty() {
        return Some("''".to_string());
    }
    let safe = s
        .chars()
        .all(|c| c.is_alphanumeric() || "._-/~".contains(c));
    Some(if safe {
        s.to_string()
    } else {
        shell_single_quote(s)
    })
}

/// Quote a filesystem path for insertion into an interactive command line,
/// leaving obviously safe paths unquoted so the inserted text stays readable.
/// Unsafe display/control text fails closed to an empty quoted argument; new
/// callers that can surface an error should use [`try_shell_quote_path`].
pub fn shell_quote_path(s: &str) -> String {
    try_shell_quote_path(s).unwrap_or_else(|| "''".to_string())
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
        || args.iter().any(|argument| {
            argument.chars().any(char::is_control)
                || crate::review_input::contains_visual_spoofing(argument)
        })
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
        || args.iter().any(|argument| {
            argument.chars().any(char::is_control)
                || crate::review_input::contains_visual_spoofing(argument)
        })
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
///
/// `jsh` is on the POSIX list because its single-quote handling was verified
/// against the exact string [`shell_quote_argv`] emits, not because it is the
/// family's shell. Adding it any earlier would have been wrong: jsh used to
/// re-derive glob eligibility from already-expanded text, so quoting did not
/// suppress pathname expansion and a quoted `*` expanded to the directory
/// listing — one argument silently becoming many, the precise failure this
/// function exists to avoid. jsh's `tests/glob_quoting_tests.rs` is the twin of
/// `jsh_quoting_round_trips` below and pins that guarantee from its side.
pub fn shell_quote_argv_for(args: &[String], shell_argv: &[String]) -> Option<String> {
    let shell = Path::new(shell_argv.first()?)
        .file_name()?
        .to_str()?
        .to_ascii_lowercase();
    match shell.as_str() {
        "pwsh" | "powershell" | "powershell.exe" | "pwsh.exe" => powershell_quote_argv(args),
        "bash" | "dash" | "fish" | "jsh" | "ksh" | "mksh" | "sh" | "zsh" => shell_quote_argv(args),
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

/// A restored command is typed into an interactive editor, not passed directly
/// to `execve`. Keep its structured representation far below OS ARG_MAX and
/// bound both pathological argument counts and quoting amplification.
pub const MAX_RESTORABLE_ARG_COUNT: usize = 256;
pub const MAX_RESTORABLE_ARG_BYTES: usize = 64 * 1024;
pub const MAX_RESTORABLE_ARGV_BYTES: usize = 256 * 1024;

pub fn restorable_argv_within_limits(args: &[String]) -> bool {
    if args.is_empty() || args.len() > MAX_RESTORABLE_ARG_COUNT {
        return false;
    }
    let mut total = 0usize;
    for argument in args {
        if argument.len() > MAX_RESTORABLE_ARG_BYTES
            || argument.chars().any(char::is_control)
            || crate::review_input::contains_visual_spoofing(argument)
        {
            return false;
        }
        let Some(next) = total
            .checked_add(argument.len())
            .and_then(|bytes| bytes.checked_add(1))
        else {
            return false;
        };
        if next > MAX_RESTORABLE_ARGV_BYTES {
            return false;
        }
        total = next;
    }
    true
}

/// Check if an argv matches a known restorable command pattern, returning the
/// original argument vector for session persistence. Keeping argv structured is
/// important: joining it here would discard quoting boundaries, so a remote
/// command argument containing `;` could become a new local command on restore.
pub fn match_restorable_command(args: &[String]) -> Option<Vec<String>> {
    if !restorable_argv_within_limits(args) {
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

/// Deserialize a snapshot's restorable command, accepting the legacy joined
/// string but never replaying it.
///
/// The identical duplicate of this lived at anvil `src/session.rs` and forge
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
    struct BoundedArgument(Option<String>);

    impl<'de> serde::Deserialize<'de> for BoundedArgument {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            struct ArgumentVisitor;

            impl<'de> serde::de::Visitor<'de> for ArgumentVisitor {
                type Value = BoundedArgument;

                fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    formatter.write_str("a bounded command argument string")
                }

                fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
                where
                    E: serde::de::Error,
                {
                    Ok(BoundedArgument(
                        (value.len() <= MAX_RESTORABLE_ARG_BYTES
                            && !value.chars().any(char::is_control)
                            && !crate::review_input::contains_visual_spoofing(value))
                        .then(|| value.to_string()),
                    ))
                }

                fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
                where
                    E: serde::de::Error,
                {
                    Ok(BoundedArgument(
                        (value.len() <= MAX_RESTORABLE_ARG_BYTES
                            && !value.chars().any(char::is_control)
                            && !crate::review_input::contains_visual_spoofing(&value))
                        .then_some(value),
                    ))
                }
            }

            deserializer.deserialize_string(ArgumentVisitor)
        }
    }

    struct StoredCommandVisitor;

    impl<'de> serde::de::Visitor<'de> for StoredCommandVisitor {
        type Value = Option<Vec<String>>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("null, a legacy command string, or a bounded argv array")
        }

        fn visit_none<E>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            deserializer.deserialize_any(self)
        }

        fn visit_str<E>(self, joined: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            // Log only a bounded, visible spelling of the first word. Legacy
            // argv boundaries cannot be reconstructed, and the original field
            // is untrusted snapshot text rather than safe diagnostic output.
            let raw_program = joined.split_whitespace().next().unwrap_or("(empty)");
            let program = process_label(raw_program).unwrap_or_else(|| "(empty)".to_string());
            log::warn!(
                "Ignoring legacy session restore command '{program}' without argv boundaries"
            );
            Ok(None)
        }

        fn visit_string<E>(self, joined: String) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            self.visit_str(&joined)
        }

        fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::SeqAccess<'de>,
        {
            let capacity = sequence
                .size_hint()
                .unwrap_or(0)
                .min(MAX_RESTORABLE_ARG_COUNT);
            let mut argv = Vec::with_capacity(capacity);
            let mut total = 0usize;
            let mut count = 0usize;
            let mut valid = true;
            while let Some(BoundedArgument(argument)) =
                sequence.next_element::<BoundedArgument>()?
            {
                count = count.saturating_add(1);
                let Some(argument) = argument else {
                    valid = false;
                    continue;
                };
                let next_total = total
                    .checked_add(argument.len())
                    .and_then(|bytes| bytes.checked_add(1));
                if count > MAX_RESTORABLE_ARG_COUNT
                    || next_total.is_none_or(|bytes| bytes > MAX_RESTORABLE_ARGV_BYTES)
                {
                    valid = false;
                    continue;
                }
                if valid {
                    total = next_total.unwrap_or(total);
                    argv.push(argument);
                }
            }
            Ok(valid.then(|| match_restorable_command(&argv)).flatten())
        }
    }

    deserializer.deserialize_any(StoredCommandVisitor)
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

// Compatibility export: only the bounded read-only argv observation moved.
pub use jterm_runtime::process_argv::read_proc_cmdline;

fn process_label(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let mut chars = raw.chars();
    let mut label = String::new();
    for ch in chars.by_ref().take(MAX_PROCESS_LABEL_CHARS) {
        label.push(
            if ch.is_control() || crate::review_input::is_visual_spoofing_character(ch) {
                '\u{fffd}'
            } else {
                ch
            },
        );
    }
    if chars.next().is_some() {
        label.push('…');
    }
    Some(label)
}

/// The kernel task name from `/proc/<pid>/comm`, trimmed. Truncated by the
/// kernel to `TASK_COMM_LEN`; prefer [`read_proc_cmdline`] when a full `argv[0]`
/// is available, and this when only a short display name is needed.
pub fn process_comm(pid: i32) -> Option<String> {
    if pid <= 0 {
        return None;
    }
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    process_label(&comm)
}

/// The process working directory via `/proc/<pid>/cwd`, when readable.
/// Non-UTF-8 paths return `None`; a lossy spelling could name another directory.
pub fn process_cwd(pid: i32) -> Option<String> {
    if pid <= 0 {
        return None;
    }
    std::fs::read_link(format!("/proc/{pid}/cwd"))
        .ok()
        .and_then(|path| path.into_os_string().into_string().ok())
}

// Compatibility exports: callers retain their old paths and the same type identity.
pub use jterm_runtime::process_stat::{
    parse_process_stat, process_stat, process_stat_result, ProcessStat,
};

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

fn restorable_command_from_foreground(
    shell_pid: i32,
    foreground_pid: Option<i32>,
) -> Option<Vec<String>> {
    // Managed remote panes launch ssh/mosh as the PTY child itself rather than
    // underneath an interactive local shell. Preserve that allowlisted argv
    // too; ordinary bash/zsh/jsh children do not match and continue below.
    if let Some(command) =
        read_proc_cmdline(shell_pid).and_then(|args| match_restorable_command(&args))
    {
        return Some(command);
    }

    let mut pid = foreground_pid?;
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

/// Detect a restorable interactive command (ssh/nix develop/docker exec/…) by
/// walking from the PTY's foreground process up to the shell.
pub fn restorable_command(pty_fd: i32, shell_pid: i32) -> Option<Vec<String>> {
    restorable_command_from_foreground(shell_pid, foreground_pgid(pty_fd, shell_pid))
}

/// Detect a restorable foreground command using only `/proc/<shell>/stat`.
///
/// This is the counterpart to [`restorable_command`] for frontends whose UI
/// owns the shell pid but not the PTY master fd. The command still comes from
/// NUL-delimited `/proc/<pid>/cmdline`; terminal output and OSC command text
/// are never treated as process argv.
pub fn restorable_command_via_stat(shell_pid: i32) -> Option<Vec<String>> {
    let foreground = foreground_pgid_via_stat(shell_pid).filter(|pid| *pid != shell_pid);
    restorable_command_from_foreground(shell_pid, foreground)
}

/// One SSH command observed at a real local process boundary.
///
/// `argv` is retained for cheap foreground-change deduplication. `target` is
/// already classified so a trusted jsh deployment launcher can carry the
/// same result as a direct `ssh`, including an actionable `Unsupported`
/// reason when hidden launcher inputs make exact reuse impossible.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservedSshCommand {
    pub argv: Vec<String>,
    pub target: crate::jsh_remote::ObservedSshTarget,
    /// A jsh-remote master socket that a Files execution profile may overlay
    /// after it has resolved stable saved/transient target identity.
    pub reusable_control_path: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum JshLauncherAuthority {
    Untrusted,
    HiddenSshArgs,
    Trusted { control_path: Option<String> },
}

fn has_control_path_argument(args: &[String]) -> bool {
    let mut index = 0usize;
    while index < args.len() {
        let argument = &args[index];
        if argument == "-S" || (argument.starts_with("-S") && argument.len() > 2) {
            return true;
        }
        if argument == "-o" {
            if args.get(index + 1).is_some_and(|option| {
                option
                    .split_once('=')
                    .map_or(option.as_str(), |(key, _)| key)
                    .eq_ignore_ascii_case("controlpath")
            }) {
                return true;
            }
            index += 1;
        } else if let Some(option) = argument.strip_prefix("-o") {
            if option
                .split_once('=')
                .map_or(option, |(key, _)| key)
                .eq_ignore_ascii_case("controlpath")
            {
                return true;
            }
        }
        index += 1;
    }
    false
}

fn apply_jsh_launcher_authority(
    argv: Vec<String>,
    target: crate::jsh_remote::ObservedSshTarget,
    authority: JshLauncherAuthority,
) -> Option<ObservedSshCommand> {
    use crate::jsh_remote::ObservedSshTarget;

    match authority {
        JshLauncherAuthority::Untrusted => None,
        JshLauncherAuthority::HiddenSshArgs => Some(ObservedSshCommand {
            argv,
            target: match target {
                ObservedSshTarget::Target(_) => ObservedSshTarget::Unsupported(
                    "JSH_REMOTE_SSH_ARGS hides connection options; use a saved Remote Files profile",
                ),
                other => other,
            },
            reusable_control_path: None,
        }),
        JshLauncherAuthority::Trusted { control_path } => {
            let reusable_control_path = match (&target, control_path) {
                (ObservedSshTarget::Target(profile), Some(path))
                    if !has_control_path_argument(&profile.ssh_args) =>
                {
                    Some(path)
                }
                _ => None,
            };
            Some(ObservedSshCommand {
                argv,
                target,
                reusable_control_path,
            })
        }
    }
}

fn observed_ssh_command_from_foreground<ReadArgv, ReadParent, CheckLauncher>(
    shell_pid: i32,
    foreground_pid: Option<i32>,
    mut read_argv: ReadArgv,
    mut read_parent: ReadParent,
    mut check_launcher: CheckLauncher,
) -> Option<ObservedSshCommand>
where
    ReadArgv: FnMut(i32) -> Option<Vec<String>>,
    ReadParent: FnMut(i32) -> Option<i32>,
    CheckLauncher: FnMut(i32, i32, &[String]) -> JshLauncherAuthority,
{
    use crate::jsh_remote::{observed_ssh_target, ObservedSshTarget};

    // A managed plain-SSH pane may make ssh the PTY child itself. Deployment
    // launchers need a jsh parent and are therefore handled only in the
    // foreground ancestry below.
    if let Some(argv) = read_argv(shell_pid) {
        let target = observed_ssh_target(&argv);
        if !matches!(target, ObservedSshTarget::NotSsh)
            && !crate::jsh_remote::is_jsh_remote_launcher_argv(&argv)
        {
            return Some(ObservedSshCommand {
                argv,
                target,
                reusable_control_path: None,
            });
        }
    }

    let mut pid = foreground_pid?;
    for _ in 0..16 {
        if pid == shell_pid || pid <= 1 {
            return None;
        }
        let argv = read_argv(pid)?;
        let target = observed_ssh_target(&argv);
        if !matches!(target, ObservedSshTarget::NotSsh) {
            if crate::jsh_remote::is_jsh_remote_launcher_argv(&argv) {
                let authority = check_launcher(pid, shell_pid, &argv);
                return apply_jsh_launcher_authority(argv, target, authority);
            }
            return Some(ObservedSshCommand {
                argv,
                target,
                reusable_control_path: None,
            });
        }
        pid = read_parent(pid)?;
    }
    None
}

const MAX_PROC_ENVIRON_BYTES: u64 = 2 * 1024 * 1024;

fn read_proc_environ(pid: i32) -> Option<Vec<u8>> {
    if pid <= 0 {
        return None;
    }
    use std::io::Read;

    let file = std::fs::File::open(format!("/proc/{pid}/environ")).ok()?;
    let mut raw = Vec::new();
    file.take(MAX_PROC_ENVIRON_BYTES + 1)
        .read_to_end(&mut raw)
        .ok()?;
    (raw.len() as u64 <= MAX_PROC_ENVIRON_BYTES).then_some(raw)
}

fn environment_value<'a>(environment: &'a [u8], name: &[u8]) -> Option<&'a [u8]> {
    environment.split(|byte| *byte == 0).find_map(|entry| {
        let equals = entry.iter().position(|byte| *byte == b'=')?;
        let (key, value) = entry.split_at(equals);
        let value = value.get(1..)?;
        (key == name).then_some(value)
    })
}

fn private_owned_regular_file(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;

    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return false;
    };
    metadata.file_type().is_file()
        && metadata.nlink() == 1
        && metadata.uid() == unsafe { libc::geteuid() }
        && metadata.mode() & 0o022 == 0
}

fn jsh_launcher_authority(
    launcher_pid: i32,
    shell_pid: i32,
    argv: &[String],
) -> JshLauncherAuthority {
    if !crate::jsh_remote::is_jsh_ssh_upgrade_launcher_argv(argv)
        || read_ppid(launcher_pid) != Some(shell_pid)
        || std::fs::read_link(format!("/proc/{shell_pid}/exe"))
            .ok()
            .and_then(|path| path.file_name().map(|name| name.to_owned()))
            .and_then(|name| name.to_str().map(str::to_owned))
            .as_deref()
            != Some("jsh")
    {
        return JshLauncherAuthority::Untrusted;
    }

    let Some(environment) = read_proc_environ(launcher_pid) else {
        return JshLauncherAuthority::Untrusted;
    };
    let value = |name: &[u8]| {
        environment_value(&environment, name)
            .and_then(|value| std::str::from_utf8(value).ok())
            .filter(|value| !value.is_empty())
    };
    let cache_home = value(b"XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            value(b"HOME")
                .map(std::path::PathBuf::from)
                .filter(|path| path.is_absolute())
                .map(|home| home.join(".cache"))
        });
    let Some(script) = argv.get(1).map(Path::new) else {
        return JshLauncherAuthority::Untrusted;
    };
    if !private_owned_regular_file(script) {
        return JshLauncherAuthority::Untrusted;
    }

    if environment_value(&environment, b"JSH_REMOTE_SSH_ARGS")
        .is_some_and(|value| !value.is_empty())
    {
        return JshLauncherAuthority::HiddenSshArgs;
    }

    let control_base = value(b"XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| cache_home.map(|cache| cache.join("anvil")));
    let control_path = control_base.and_then(|base| {
        let path = base.join("cm-%C");
        let path = path.to_str()?;
        (path.len() <= 512
            && !path
                .chars()
                .any(|character| character.is_whitespace() || character.is_control())
            && !crate::review_input::contains_visual_spoofing(path))
        .then(|| path.to_string())
    });
    JshLauncherAuthority::Trusted { control_path }
}

/// Observe a direct foreground SSH command or a provenance-checked jsh remote
/// launcher through a PTY master fd.
pub fn observed_ssh_command(pty_fd: i32, shell_pid: i32) -> Option<ObservedSshCommand> {
    observed_ssh_command_from_foreground(
        shell_pid,
        foreground_pgid(pty_fd, shell_pid),
        read_proc_cmdline,
        read_ppid,
        jsh_launcher_authority,
    )
}

/// [`observed_ssh_command`] for frontends that own only the shell pid.
pub fn observed_ssh_command_via_stat(shell_pid: i32) -> Option<ObservedSshCommand> {
    let foreground = foreground_pgid_via_stat(shell_pid).filter(|pid| *pid != shell_pid);
    observed_ssh_command_from_foreground(
        shell_pid,
        foreground,
        read_proc_cmdline,
        read_ppid,
        jsh_launcher_authority,
    )
}

/// Name of the foreground process on a PTY (e.g. "ssh", "vim"), or None if the
/// shell itself is in the foreground. Used for close-confirmation prompts.
pub fn foreground_process_name(pty_fd: i32, shell_pid: i32) -> Option<String> {
    if let Some(args) = read_proc_cmdline(shell_pid) {
        if match_restorable_command(&args).is_some() {
            return Path::new(args.first()?)
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(process_label);
        }
    }
    let fg = foreground_pgid(pty_fd, shell_pid)?;
    let args = read_proc_cmdline(fg)?;
    Path::new(args.first()?)
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(process_label)
}

// Preserve the existing frontend imports. Runtime is the sole implementation.
pub use jterm_runtime::child_lifecycle::{
    decode_wait_status, exit_code_from_wait_status, observe_child_exit, ChildExitOutcome,
    ChildLifecycle, EscalationPolicy, ProcessRef, ReapOwner, REQUIRED_QUIET_SESSION_SCANS,
    SESSION_KILL_DRAIN_TIMEOUT, SESSION_SCAN_INTERVAL,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn process_cwd_preserves_utf8_and_refuses_lossy_paths() {
        use std::os::unix::ffi::OsStringExt;
        use std::path::PathBuf;
        const CHILD_MODE: &str = "JTERM_PROCESS_CWD_REGRESSION_CHILD";
        if let Some(mode) = std::env::var_os(CHILD_MODE) {
            let actual = process_cwd(std::process::id() as i32);
            if mode == "utf8" {
                assert_eq!(
                    actual,
                    std::env::current_dir()
                        .unwrap()
                        .into_os_string()
                        .into_string()
                        .ok()
                );
                assert!(actual.as_ref().unwrap().contains("cwd-目录"));
            } else {
                assert_eq!(mode, "nonutf8");
                assert_eq!(
                    actual, None,
                    "a non-UTF8 cwd must not name its lossy replacement"
                );
            }
            return;
        }
        let base = std::env::temp_dir().join(format!("jterm-process-cwd-{}", std::process::id()));
        std::fs::create_dir(&base).unwrap();
        let utf8 = base.join("cwd-目录");
        let nonutf8 = base.join(std::ffi::OsString::from_vec(b"cwd-\xff".to_vec()));
        std::fs::create_dir(&utf8).unwrap();
        std::fs::create_dir(&nonutf8).unwrap();
        let lossy = PathBuf::from(nonutf8.to_string_lossy().into_owned());
        assert_ne!(lossy, nonutf8);
        std::fs::create_dir(&lossy).unwrap();
        let mut successes = Vec::new();
        for (mode, cwd) in [("utf8", &utf8), ("nonutf8", &nonutf8)] {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("process_cwd_preserves_utf8_and_refuses_lossy_paths")
                .arg("--nocapture")
                .env(CHILD_MODE, mode)
                .current_dir(cwd)
                .output()
                .unwrap();
            successes.push((
                mode,
                output.status.success(),
                String::from_utf8_lossy(&output.stdout).into_owned(),
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ));
        }
        std::fs::remove_dir(&utf8).unwrap();
        std::fs::remove_dir(&nonutf8).unwrap();
        std::fs::remove_dir(&lossy).unwrap();
        std::fs::remove_dir(&base).unwrap();
        for (mode, success, stdout, stderr) in successes {
            assert!(success, "{mode} child failed: {stdout}\n{stderr}");
        }
    }

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
        for unsafe_path in ["line\nbreak", "hidden\u{202e}txt", "not\u{00a0}space"] {
            assert_eq!(try_shell_quote_path(unsafe_path), None);
            assert_eq!(shell_quote_path(unsafe_path), "''");
        }
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
    fn jsh_remote_launchers_are_observed_but_never_added_to_session_restore() {
        use crate::jsh_remote::ObservedSshTarget;

        let launcher = argv(&[
            "/bin/sh",
            "/home/alice/.cache/jsh/jsh-remote.sh",
            "--persist",
            "--local-jsh",
            "/home/alice/bin/jsh",
            "root@box.example",
            "--",
            "-p",
            "22",
        ]);
        assert!(match_restorable_command(&launcher).is_none());

        let command = observed_ssh_command_from_foreground(
            100,
            Some(200),
            |pid| match pid {
                100 => Some(argv(&["jsh"])),
                200 => Some(launcher.clone()),
                _ => None,
            },
            |pid| (pid == 200).then_some(100),
            |pid, shell, _| {
                assert_eq!((pid, shell), (200, 100));
                JshLauncherAuthority::Trusted {
                    control_path: Some("/run/user/1000/cm-%C".to_string()),
                }
            },
        )
        .expect("trusted launcher");
        assert_eq!(command.argv, launcher);
        assert_eq!(
            command.reusable_control_path.as_deref(),
            Some("/run/user/1000/cm-%C")
        );
        let ObservedSshTarget::Target(target) = command.target else {
            panic!("expected SSH target");
        };
        assert_eq!(target.host, "box.example");
        assert_eq!(target.user.as_deref(), Some("root"));
        assert_eq!(target.ssh_args, ["-p", "22"]);
    }

    #[test]
    fn observed_jsh_launcher_fails_closed_on_provenance_and_hidden_flags() {
        use crate::jsh_remote::ObservedSshTarget;

        let launcher = argv(&[
            "/bin/sh",
            "/home/alice/.cache/jsh/jsh-remote.sh",
            "--persist",
            "--local-jsh",
            "/home/alice/bin/jsh",
            "box.example",
        ]);
        let read = |pid| match pid {
            100 => Some(argv(&["jsh"])),
            200 => Some(launcher.clone()),
            _ => None,
        };
        assert!(observed_ssh_command_from_foreground(
            100,
            Some(200),
            read,
            |pid| (pid == 200).then_some(100),
            |_, _, _| JshLauncherAuthority::Untrusted,
        )
        .is_none());

        let hidden = observed_ssh_command_from_foreground(
            100,
            Some(200),
            read,
            |pid| (pid == 200).then_some(100),
            |_, _, _| JshLauncherAuthority::HiddenSshArgs,
        )
        .expect("trusted process with hidden input gets a diagnostic");
        assert!(matches!(hidden.target, ObservedSshTarget::Unsupported(_)));
    }

    #[test]
    fn direct_observed_ssh_does_not_need_launcher_authority() {
        use crate::jsh_remote::ObservedSshTarget;

        let command = observed_ssh_command_from_foreground(
            100,
            Some(200),
            |pid| match pid {
                100 => Some(argv(&["jsh"])),
                200 => Some(argv(&["ssh", "dev@box", "-p", "2200"])),
                _ => None,
            },
            |pid| (pid == 200).then_some(100),
            |_, _, _| panic!("direct ssh must not ask for launcher authority"),
        )
        .expect("direct ssh");
        let ObservedSshTarget::Target(target) = command.target else {
            panic!("expected SSH target");
        };
        assert_eq!(target.user.as_deref(), Some("dev"));
        assert_eq!(target.ssh_args, ["-p", "2200"]);
        assert!(command.reusable_control_path.is_none());
    }

    #[test]
    fn launcher_environment_lookup_and_control_path_detection_are_exact() {
        let environment = b"HOME=/home/a\0JSH_REMOTE_SSH_ARGS=\0XDG_RUNTIME_DIR=/run/u\0";
        assert_eq!(
            environment_value(environment, b"HOME"),
            Some(&b"/home/a"[..])
        );
        assert_eq!(
            environment_value(environment, b"JSH_REMOTE_SSH_ARGS"),
            Some(&b""[..])
        );
        assert_eq!(environment_value(environment, b"NOT_HOME"), None);
        assert!(has_control_path_argument(&argv(&[
            "-p",
            "22",
            "-o",
            "ControlPath=/tmp/cm-%C"
        ])));
        assert!(has_control_path_argument(&argv(&["-S/tmp/cm-%C"])));
        assert!(!has_control_path_argument(&argv(&[
            "-o",
            "ControlMaster=auto"
        ])));
    }

    #[test]
    fn launcher_script_authority_rejects_links_and_writable_files() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let dir = std::env::temp_dir().join(format!(
            "jterm-core-launcher-authority-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).expect("scratch directory");
        let script = dir.join("jsh-remote.sh");
        std::fs::write(&script, "exit 0\n").expect("script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))
            .expect("private mode");
        assert!(private_owned_regular_file(&script));

        let link = dir.join("linked-jsh-remote.sh");
        symlink(&script, &link).expect("symlink");
        assert!(!private_owned_regular_file(&link));
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o722))
            .expect("writable mode");
        assert!(!private_owned_regular_file(&script));
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn restorable_commands_enforce_count_field_total_and_control_bounds() {
        let too_many = std::iter::once("ssh".to_string())
            .chain((0..MAX_RESTORABLE_ARG_COUNT).map(|_| "x".to_string()))
            .collect::<Vec<_>>();
        assert!(match_restorable_command(&too_many).is_none());

        let oversized_field = vec!["ssh".to_string(), "x".repeat(MAX_RESTORABLE_ARG_BYTES + 1)];
        assert!(match_restorable_command(&oversized_field).is_none());

        let chunk = "x".repeat(MAX_RESTORABLE_ARG_BYTES);
        let oversized_total = vec![
            "ssh".to_string(),
            chunk.clone(),
            chunk.clone(),
            chunk.clone(),
            chunk,
        ];
        assert!(match_restorable_command(&oversized_total).is_none());
        assert!(match_restorable_command(&argv(&["ssh", "host\nname"])).is_none());
        assert!(match_restorable_command(&argv(&["ssh", "host\u{202e}name"])).is_none());
        assert!(match_restorable_command(&argv(&["ssh", "host\u{00a0}name"])).is_none());

        let boundary_chunk = (MAX_RESTORABLE_ARGV_BYTES - "ssh".len() - 5) / 4;
        let boundary = vec![
            "ssh".to_string(),
            "x".repeat(boundary_chunk),
            "x".repeat(boundary_chunk),
            "x".repeat(boundary_chunk),
            "x".repeat(boundary_chunk),
        ];
        assert!(restorable_argv_within_limits(&boundary));
        assert!(match_restorable_command(&boundary).is_some());
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
    fn arbitrary_structured_argv_is_loaded_without_replay() {
        assert_eq!(
            stored_pane(r#"{"cmds":["sh","-c","touch /tmp/from-snapshot"]}"#).cmds,
            None
        );
        assert_eq!(stored_pane(r#"{"cmds":["git","status"]}"#).cmds, None);
    }

    #[test]
    fn snapshot_argv_is_bounded_while_the_sequence_is_consumed() {
        let too_many = std::iter::once("ssh".to_string())
            .chain((0..MAX_RESTORABLE_ARG_COUNT).map(|_| String::new()))
            .collect::<Vec<_>>();
        let json = serde_json::json!({ "cmds": too_many }).to_string();
        assert_eq!(stored_pane(&json).cmds, None);

        let oversized = serde_json::json!({
            "cmds": ["ssh", "x".repeat(MAX_RESTORABLE_ARG_BYTES + 1)]
        })
        .to_string();
        assert_eq!(stored_pane(&oversized).cmds, None);

        let spoofed = serde_json::json!({ "cmds": ["ssh", "host\u{202e}txt"] }).to_string();
        assert_eq!(stored_pane(&spoofed).cmds, None);
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

    /// The family's own shell has to be on the list, or the terminals silently
    /// decline to replay a restored command into the shell they ship with.
    ///
    /// The expectation here is the exact byte string handed to jsh; jsh's
    /// `tests/glob_quoting_tests.rs` asserts from the other side that parsing it
    /// yields the original arguments. Changing the quoting on either side must
    /// redden both.
    #[test]
    fn jsh_quoting_round_trips() {
        let hostile = argv(&[
            "printf", "[%s]", "a b", "it's", "*", "$(id)", "~", "a;b", "a?", "x[ab]",
        ]);
        assert_eq!(
            shell_quote_argv_for(&hostile, &argv(&["/usr/local/bin/jsh"])),
            Some(
                "'printf' '[%s]' 'a b' 'it'\"'\"'s' '*' '$(id)' '~' 'a;b' 'a?' 'x[ab]'".to_string()
            )
        );
        // Reached through a login argv and through a bare name, as the terminals
        // spawn it both ways.
        assert_eq!(
            shell_quote_argv_for(&argv(&["ssh", "*"]), &argv(&["jsh", "--login"])),
            Some("'ssh' '*'".to_string())
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
    fn process_labels_cannot_reformat_terminal_chrome() {
        let raw = format!("build\nleft\u{202e}right\u{00a0}{}", "x".repeat(200));
        let label = process_label(&raw).unwrap();
        assert!(!label.contains('\n'));
        assert!(!label.contains('\u{202e}'));
        assert!(!label.contains('\u{00a0}'));
        assert!(label.contains('\u{fffd}'));
        assert!(label.ends_with('…'));
        assert_eq!(label.chars().count(), MAX_PROCESS_LABEL_CHARS + 1);
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
        assert!(restorable_command_via_stat(0).is_none());
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
