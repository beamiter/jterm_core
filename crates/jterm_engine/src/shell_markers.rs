//! Decoder shared by terminal reducers for complete FinalTerm OSC 133 packets.

/// Per-field caps for the OSC 133 metadata jsh attaches to its C/D packets.
/// jsh bounds these on the way out (`MAX_OSC_COMMAND_BYTES` = 16 KiB,
/// `MAX_OSC_CWD_BYTES` = 4 KiB in jsh/src/osc.rs), but any program can write an
/// OSC 133 packet, so the reader bounds them too rather than trusting a
/// cooperative producer.
/// Maximum decoded command bytes accepted from OSC 133 metadata. This matches
/// jsh's emitter; oversized commands use `cmd_truncated=1` instead.
pub const MAX_OSC133_COMMAND_BYTES: usize = 16 * 1024;
/// Maximum decoded cwd bytes accepted from OSC 133 metadata.
pub const MAX_OSC133_CWD_BYTES: usize = 4 * 1024;
/// Maximum decoded execution-id bytes accepted from OSC 133 metadata. Shells
/// other than jsh may use longer opaque correlation ids; journal persistence
/// independently applies jsh's narrower 192-byte grammar.
pub const MAX_OSC133_ID_BYTES: usize = 1024;
/// Maximum decoded persistent-session identity bytes. Unlike display-only
/// metadata, this must match jsh's exact journal correlation grammar.
pub const MAX_OSC133_SESSION_ID_BYTES: usize = MAX_JSH_SESSION_ID_BYTES;

/// Ordered lifecycle markers carried by one OSC 133 packet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShellMarker {
    PromptStart,
    PromptEnd,
    CommandStart(CommandMeta),
    CommandEnd {
        exit: Option<i32>,
        meta: CommandMeta,
    },
}

/// Decode a complete OSC payload already framed by the terminal parser.
/// The host supplies its terminal text visibility predicate so the established
/// full core and the dependency-light standalone terminal can each apply their
/// existing display policy without making the parser depend on Agent code.
pub fn decode(payload: &[u8], invisible: fn(char) -> bool) -> Option<ShellMarker> {
    let rest = std::str::from_utf8(payload.strip_prefix(b"133;")?).ok()?;
    let mut fields = rest.split(';');
    match fields.next()? {
        "A" => Some(ShellMarker::PromptStart),
        "B" => Some(ShellMarker::PromptEnd),
        "C" => Some(ShellMarker::CommandStart(CommandMeta::from_fields(
            fields, true, invisible,
        ))),
        "D" => Some(ShellMarker::CommandEnd {
            exit: parse_osc133_exit_status(fields.clone()),
            meta: CommandMeta::from_fields(fields, false, invisible),
        }),
        _ => None,
    }
}

/// Maximum bytes in the jsh session identifier announced over OSC 7770 and
/// stored on a journal start event.
pub const MAX_JSH_SESSION_ID_BYTES: usize = 128;

/// Shared definition of a well-formed jsh session id (apps re-use this for
/// their own session-id handling).
pub fn is_valid_jsh_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_JSH_SESSION_ID_BYTES
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

/// Validate exact jsh command spelling before display or journal use.
pub fn valid_command(command: &str, max_bytes: usize, invisible: fn(char) -> bool) -> bool {
    !command.is_empty()
        && command.len() <= max_bytes
        && !command
            .chars()
            .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\t'))
        && !command.chars().any(|ch| !ch.is_control() && invisible(ch))
}

/// Validate an exact cwd without invisible characters or path truncation.
pub fn valid_cwd(cwd: &str, invisible: fn(char) -> bool) -> bool {
    !cwd.is_empty()
        && cwd.len() <= MAX_OSC133_CWD_BYTES
        && !cwd.chars().any(|ch| ch.is_control() || invisible(ch))
}

/// The metadata a shell attaches to its OSC 133 `C` and `D` packets.
///
/// jsh — the family's own shell — carries the command line, the execution id it
/// keys its execution journal on, the cwd and the measured duration, all
/// percent-encoded (see `jsh/src/osc.rs`, whose tests pin the exact bytes).
/// This parser used to split on `;`, read the mark, and throw every parameter
/// away, so the two frontends that share it reconstructed the command by
/// scraping it back off the screen and could never correlate their captured
/// output with a journal record — while `jterm_core::execution_journal::submit`
/// existed for exactly that.
///
/// Every field is optional for terminal UI: the same mark is emitted by shells
/// that send no metadata at all, and a field whose encoding is malformed,
/// oversized or not UTF-8 is dropped rather than guessed at. Durable Output is
/// stricter: all four Start identity slots must come from one C packet before
/// `ExecutionLifecycle` can be constructed. D
/// packets never populate those Start-only slots.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommandMeta {
    /// The shell's own execution id — the correlation key into its journal.
    pub id: Option<String>,
    /// Persistent shell session that owns this execution lifecycle.
    pub session_id: Option<String>,
    /// Shell-local sequence number of this exact Start generation.
    pub seq: Option<u64>,
    /// Wall-clock identity copied from the matching journal Start.
    pub started_at_ms: Option<u64>,
    /// The command line as the shell parsed it, not as the screen rendered it.
    pub command: Option<String>,
    /// Working directory the command ran in.
    pub cwd: Option<String>,
    /// Wall-clock duration the shell measured, which beats a timer started when
    /// the frontend happened to notice the mark.
    pub duration_ms: Option<u64>,
    /// The shell had a command line but it exceeded the packet budget, so
    /// [`CommandMeta::command`] is absent for a reason worth telling apart from
    /// "this shell sends no metadata".
    pub command_truncated: bool,
}

impl CommandMeta {
    /// Parse the `key=value` fields of an OSC 133 packet.
    ///
    /// Key aliases follow ember's decoder, which is the family's most complete
    /// and has been reading jsh's packets in production; accepting its spellings
    /// means the shared parser cannot understand *less* than the frontend that
    /// already had its own.
    fn from_fields<'a>(
        fields: impl Iterator<Item = &'a str>,
        accept_start_identity: bool,
        invisible: fn(char) -> bool,
    ) -> Self {
        let mut meta = Self::default();
        // Aliases name the same semantic slot. Reject a repeated slot instead
        // of applying last-wins semantics to a journal correlation id,
        // command, cwd, or duration supplied by untrusted PTY output.
        let mut seen_id = false;
        let mut seen_session_id = false;
        let mut seen_seq = false;
        let mut seen_started_at_ms = false;
        let mut seen_command = false;
        let mut seen_cwd = false;
        let mut seen_duration = false;
        let mut seen_command_truncated = false;
        for field in fields {
            let Some((key, value)) = field.split_once('=') else {
                continue;
            };
            match key {
                "id" | "jsh_id" | "execution_id" | "command_id" => {
                    if std::mem::replace(&mut seen_id, true) {
                        meta.id = None;
                        continue;
                    }
                    meta.id = decode_osc133(value, MAX_OSC133_ID_BYTES).filter(|id| {
                        !id.is_empty()
                            && !id.chars().any(char::is_control)
                            && !id.chars().any(invisible)
                    });
                }
                "session_id" => {
                    if !accept_start_identity {
                        continue;
                    }
                    if std::mem::replace(&mut seen_session_id, true) {
                        meta.session_id = None;
                        continue;
                    }
                    meta.session_id = decode_osc133(value, MAX_OSC133_SESSION_ID_BYTES)
                        .filter(|id| is_valid_jsh_session_id(id));
                }
                "seq" => {
                    if !accept_start_identity {
                        continue;
                    }
                    if std::mem::replace(&mut seen_seq, true) {
                        meta.seq = None;
                        continue;
                    }
                    meta.seq = value.parse().ok();
                }
                "started_at_ms" => {
                    if !accept_start_identity {
                        continue;
                    }
                    if std::mem::replace(&mut seen_started_at_ms, true) {
                        meta.started_at_ms = None;
                        continue;
                    }
                    meta.started_at_ms = value.parse().ok();
                }
                "cmdline_url" | "command_url" | "command" | "cmdline" => {
                    if std::mem::replace(&mut seen_command, true) {
                        meta.command = None;
                        continue;
                    }
                    meta.command = decode_osc133(value, MAX_OSC133_COMMAND_BYTES)
                        .filter(|text| valid_command(text, MAX_OSC133_COMMAND_BYTES, invisible));
                }
                "cwd_url" | "cwd" => {
                    if std::mem::replace(&mut seen_cwd, true) {
                        meta.cwd = None;
                        continue;
                    }
                    meta.cwd = decode_osc133(value, MAX_OSC133_CWD_BYTES)
                        .filter(|text| valid_cwd(text, invisible));
                }
                "duration_ms" | "duration" => {
                    if std::mem::replace(&mut seen_duration, true) {
                        meta.duration_ms = None;
                        continue;
                    }
                    meta.duration_ms = value.trim().parse().ok();
                }
                "cmd_truncated" | "command_truncated" => {
                    if std::mem::replace(&mut seen_command_truncated, true) {
                        // Ambiguous disclosure must not turn a known truncated
                        // command back into an apparently complete one.
                        meta.command_truncated = true;
                        continue;
                    }
                    let value = value.trim();
                    meta.command_truncated = match value {
                        "0" => false,
                        "1" => true,
                        value if value.eq_ignore_ascii_case("false") => false,
                        value if value.eq_ignore_ascii_case("true") => true,
                        // A producer chose to send the disclosure but did not
                        // encode its state. Treat that as inexact, not as the
                        // default `false` that claims a command is complete.
                        _ => true,
                    };
                }
                _ => {}
            }
        }
        // jsh emits either an exact command or `cmd_truncated=1`, never both.
        // A contradictory producer must not smuggle a partial prefix into the
        // exact-command path merely by attaching the honest disclosure too.
        if meta.command_truncated {
            meta.command = None;
        }
        meta
    }
}

/// Strict percent-decode of one OSC 133 metadata field.
///
/// Strict on purpose, following ember: a truncated escape, an oversized value
/// or invalid UTF-8 yields `None` rather than a best-effort string, because the
/// decoded value is used as a filesystem path, a journal key, and text put in
/// front of the user. Half-decoded input is worse than none.
fn decode_osc133(value: &str, max_bytes: usize) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded: Vec<u8> = Vec::with_capacity(bytes.len().min(max_bytes));
    let mut i = 0;
    while i < bytes.len() {
        let byte = if bytes[i] == b'%' {
            // Needs two more bytes; `i + 2` must be a valid index.
            if i + 2 >= bytes.len() {
                return None;
            }
            let high = (bytes[i + 1] as char).to_digit(16)? as u8;
            let low = (bytes[i + 2] as char).to_digit(16)? as u8;
            i += 3;
            (high << 4) | low
        } else {
            let byte = bytes[i];
            i += 1;
            byte
        };
        if decoded.len() == max_bytes {
            return None;
        }
        decoded.push(byte);
    }
    String::from_utf8(decoded).ok()
}

/// Decode the one outcome slot supported by the family's OSC 133 D packets.
///
/// FinalTerm places it positionally, while existing family integrations also
/// use one of three named spellings. An attempted second slot is ambiguous even
/// if either value is malformed, so it cannot leave an apparently authoritative
/// success or failure behind.
fn parse_osc133_exit_status<'a>(fields: impl Iterator<Item = &'a str>) -> Option<i32> {
    let mut seen = false;
    let mut parsed = None;
    for field in fields {
        let candidate = match field.split_once('=') {
            Some(("exit" | "exit_code" | "exit_status", value)) => {
                Some(value.trim().parse::<i32>().ok())
            }
            Some(_) => None,
            // Unknown bare extension flags are not outcome slots. A numeric
            // bare field is the portable positional status wherever Ember's
            // established decoder already accepts it.
            None => field.trim().parse::<i32>().ok().map(Some),
        };
        let Some(candidate) = candidate else {
            continue;
        };
        if std::mem::replace(&mut seen, true) {
            return None;
        }
        parsed = candidate;
    }
    parsed
}
