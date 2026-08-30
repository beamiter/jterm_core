//! Asynchronous bridge from terminal OSC 133 records to jsh's execution log.
//!
//! Terminal parsing runs on the UI thread (and on the bounded background-tab
//! pump), so it must never wait for a filesystem, an advisory lock, or another
//! process.  A small bounded channel hands immutable output snapshots to one
//! writer thread.  jsh owns the rest of the execution lifecycle (`start` and
//! `finish`); jterm contributes the text that was actually rendered by the
//! terminal as an `output` event with the same execution id.
//! Existing owner-only journal files with extra read bits are tightened to
//! `0600` after validation; group/world-writable files are rejected.
//! The implicit default directory is restored to `0700` after verifying its
//! owner, while an existing custom namespace is never repaired in place.

use crossbeam_channel::{bounded, Receiver, Sender, TryRecvError, TrySendError};
use once_cell::sync::OnceCell;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Current jsh execution-journal JSONL schema version.
pub const EXECUTION_JOURNAL_VERSION: u32 = 1;
const WRITER_QUEUE_CAPACITY: usize = 64;
const READER_QUEUE_CAPACITY: usize = 2;
/// Maximum encoded bytes in one journal JSONL event, excluding its newline.
pub const MAX_EVENT_LINE_BYTES: usize = 1024 * 1024;
/// Maximum bytes in the shell/frontend correlation identifier.
pub const MAX_EXECUTION_ID_BYTES: usize = 192;
/// Maximum retained UTF-8 bytes in one command.
pub const MAX_COMMAND_BYTES: usize = 64 * 1024;
/// Maximum retained UTF-8 bytes in one working directory.
pub const MAX_CWD_BYTES: usize = 4 * 1024;
/// Maximum retained UTF-8 bytes in one terminal-output snapshot.
pub const MAX_OUTPUT_BYTES: usize = 256 * 1024;
const MAX_JOURNAL_PATH_BYTES: usize = 16 * 1024;
const JOURNAL_LOCK_FILE_NAME: &str = "executions.lock";
/// Size at which jsh compacts its append-only journal.
pub const MAX_JOURNAL_FILE_BYTES: u64 = 32 * 1024 * 1024;
/// Maximum physical JSONL records inspected during one journal read.
///
/// This is above the number of shortest recognized v1 events that fit in the
/// byte window, so conforming journals remain governed by the byte ceiling.
/// It separately bounds CPU spent rejecting empty or malformed short lines.
pub const MAX_JOURNAL_EVENT_LINES: usize = 512 * 1024;
/// Maximum folded execution records retained by either reader.
pub const MAX_RETAINED_EXECUTIONS: usize = 2_000;
// jsh compacts after its own event. jterm's correlated output can be the next
// line and legitimately leave the file one bounded event over the threshold.
const MAX_JOURNAL_READ_BYTES: u64 = MAX_JOURNAL_FILE_BYTES + MAX_EVENT_LINE_BYTES as u64 + 1;
const JOURNAL_LOCK_TIMEOUT: Duration = Duration::from_secs(2);
const JOURNAL_LOCK_POLL_INTERVAL: Duration = Duration::from_millis(5);

#[derive(Debug)]
struct AppendLineCountCache {
    path: PathBuf,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    scanned_bytes: u64,
    newline_count: usize,
    ends_with_newline: bool,
}

static APPEND_LINE_COUNT_CACHE: Mutex<Option<AppendLineCountCache>> = Mutex::new(None);

#[derive(Debug)]
struct CommitStateUnknown(String);

impl std::fmt::Display for CommitStateUnknown {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for CommitStateUnknown {}

fn commit_state_unknown(kind: io::ErrorKind, message: String) -> io::Error {
    io::Error::new(kind, CommitStateUnknown(message))
}

fn is_commit_state_unknown(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|source| source.downcast_ref::<CommitStateUnknown>().is_some())
}

/// Exact Start identity carried from one parsed OSC 133 `C` mark to its
/// asynchronous terminal-output completion.
///
/// The fields are private so a durable-output producer must derive the token
/// from one complete [`crate::parser::CommandMeta`] rather than combining
/// partial metadata observed at different times.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionLifecycle {
    id: String,
    session_id: String,
    seq: u64,
    started_at_ms: u64,
}

impl ExecutionLifecycle {
    /// Build a durable journal capability only from a complete, exact jsh
    /// lifecycle envelope. Missing, duplicated, empty, or invalid slots have
    /// already degraded to `None` in the OSC parser and fail closed here.
    pub fn from_command_meta(meta: &crate::parser::CommandMeta) -> Option<Self> {
        let id = meta
            .id
            .as_deref()
            .filter(|id| is_valid_jsh_execution_id(id))?;
        let session_id = meta
            .session_id
            .as_deref()
            .filter(|id| valid_jsh_session_id(id))?;
        Some(Self {
            id: id.to_owned(),
            session_id: session_id.to_owned(),
            seq: meta.seq?,
            started_at_ms: meta.started_at_ms?,
        })
    }

    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    #[must_use]
    pub const fn seq(&self) -> u64 {
        self.seq
    }

    #[must_use]
    pub const fn started_at_ms(&self) -> u64 {
        self.started_at_ms
    }

    fn is_valid(&self) -> bool {
        is_valid_jsh_execution_id(&self.id) && valid_jsh_session_id(&self.session_id)
    }
}

/// One completed command's captured output, as reported by a terminal app.
/// The lifecycle token binds the asynchronous payload to the exact journal
/// Start generation that the terminal observed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletedExecution {
    pub lifecycle: ExecutionLifecycle,
    pub output: String,
    pub output_available: bool,
    pub truncated: bool,
    pub total_bytes: usize,
}

#[derive(Debug)]
pub enum SubmitError {
    Full,
    Closed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PersistedExecutionOutput {
    pub text: String,
    pub truncated: bool,
    pub total_bytes: u64,
    pub captured_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PersistedExecution {
    pub id: String,
    pub seq: u64,
    pub command: String,
    pub command_truncated: bool,
    pub cwd: String,
    pub started_at_ms: u64,
    pub exit_code: Option<i32>,
    pub duration_ms: Option<u64>,
    pub cwd_after: Option<String>,
    pub ended_at_ms: Option<u64>,
    pub output: Option<PersistedExecutionOutput>,
}

struct FoldedExecution {
    session_id: Option<String>,
    record: PersistedExecution,
    finish_conflicted: bool,
    output_conflicted: bool,
}

#[derive(Debug)]
pub struct HistorySnapshot {
    pub session_id: String,
    pub records: Vec<PersistedExecution>,
    pub error: Option<String>,
}

#[derive(Debug)]
pub struct HistoryLoad {
    receiver: Receiver<HistorySnapshot>,
}

/// The background reader hung up without ever delivering a snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HistoryLoadDisconnected;

impl HistoryLoad {
    /// Poll a one-shot background read without ever waiting on the UI thread.
    pub fn try_snapshot(&self) -> Result<Option<HistorySnapshot>, HistoryLoadDisconnected> {
        match self.receiver.try_recv() {
            Ok(snapshot) => Ok(Some(snapshot)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(HistoryLoadDisconnected),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryRequestError {
    Full,
    Closed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct OutputEvent {
    jsh_execution_version: u32,
    event: &'static str,
    id: String,
    text: String,
    truncated: bool,
    total_bytes: u64,
    captured_at_ms: u64,
}

#[derive(Debug)]
struct BoundOutput {
    lifecycle: ExecutionLifecycle,
    event: OutputEvent,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "event", deny_unknown_fields)]
enum PersistedEvent {
    #[serde(rename = "start")]
    Start(StartEvent),
    #[serde(rename = "finish")]
    Finish {
        #[serde(alias = "rsh_execution_version")]
        jsh_execution_version: u32,
        id: String,
        exit_code: i32,
        duration_ms: u64,
        cwd_after: String,
        ended_at_ms: u64,
    },
    #[serde(rename = "output")]
    Output {
        #[serde(alias = "rsh_execution_version")]
        jsh_execution_version: u32,
        id: String,
        text: String,
        truncated: bool,
        total_bytes: u64,
        captured_at_ms: u64,
    },
    /// Durable ambiguity marker emitted by jsh's compactor. Older v1 readers
    /// ignore this additive event and therefore also leave the slot unknown.
    #[serde(rename = "conflict")]
    Conflict(ConflictEvent),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StartEvent {
    #[serde(alias = "rsh_execution_version")]
    jsh_execution_version: u32,
    id: String,
    session_id: Option<String>,
    seq: u64,
    command: String,
    #[serde(default)]
    command_truncated: bool,
    cwd: String,
    started_at_ms: u64,
}

#[derive(Debug, Deserialize)]
struct EventIdentity<'a> {
    #[serde(borrow)]
    event: Cow<'a, str>,
    #[serde(alias = "rsh_execution_version")]
    jsh_execution_version: u32,
    #[serde(borrow)]
    id: Cow<'a, str>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConflictEvent {
    #[serde(alias = "rsh_execution_version")]
    jsh_execution_version: u32,
    id: String,
    slot: ConflictSlot,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ConflictSlot {
    Finish,
    Output,
}

impl PersistedEvent {
    fn version(&self) -> u32 {
        match self {
            Self::Start(event) => event.jsh_execution_version,
            Self::Finish {
                jsh_execution_version,
                ..
            }
            | Self::Output {
                jsh_execution_version,
                ..
            } => *jsh_execution_version,
            Self::Conflict(event) => event.jsh_execution_version,
        }
    }
}

/// Extract only the fields that make a v1 Start an authoritative lifecycle
/// barrier. Other Start fields are deliberately skipped here so a wrong type,
/// unknown member, or oversized semantic value cannot leave an older record
/// with the same valid correlation ID active.
fn recognized_v1_start_id(line: &[u8]) -> Option<Cow<'_, str>> {
    let identity = serde_json::from_slice::<EventIdentity<'_>>(line).ok()?;
    (identity.event == "start"
        && identity.jsh_execution_version == EXECUTION_JOURNAL_VERSION
        && is_valid_jsh_execution_id(&identity.id))
    .then_some(identity.id)
}

enum JournalMessage {
    Output(BoundOutput),
    Flush(Sender<()>),
}

struct HistoryRequest {
    session_id: String,
    reply: Sender<HistorySnapshot>,
}

impl OutputEvent {
    fn from_completed(completed: CompletedExecution) -> Option<BoundOutput> {
        // Bare FinalTerm markers receive terminal-local ids so the timeline
        // still works, but there is no matching jsh start/finish lifecycle to
        // correlate on disk.
        if !completed.output_available || !completed.lifecycle.is_valid() {
            return None;
        }
        let observed_bytes = completed.output.len();
        let observed_total_bytes = u64::try_from(observed_bytes).unwrap_or(u64::MAX);
        let (text, truncated) = if observed_bytes > MAX_OUTPUT_BYTES {
            (bounded_text(&completed.output, MAX_OUTPUT_BYTES), true)
        } else {
            (completed.output, completed.truncated)
        };
        let supplied_total = u64::try_from(completed.total_bytes).unwrap_or(u64::MAX);
        let (truncated, total_bytes) = normalize_output_metadata(
            text.len(),
            truncated,
            supplied_total.max(observed_total_bytes),
        );
        let lifecycle = completed.lifecycle;
        let event = Self {
            jsh_execution_version: EXECUTION_JOURNAL_VERSION,
            event: "output",
            id: lifecycle.id.clone(),
            text,
            truncated,
            total_bytes,
            captured_at_ms: unix_time_ms(),
        };
        Some(BoundOutput { lifecycle, event })
    }
}

fn normalize_output_metadata(
    retained_bytes: usize,
    truncated: bool,
    total_bytes: u64,
) -> (bool, u64) {
    let retained_bytes = u64::try_from(retained_bytes).unwrap_or(u64::MAX);
    let total_bytes = total_bytes.max(retained_bytes);
    (truncated || total_bytes > retained_bytes, total_bytes)
}

static WRITER: OnceCell<Option<Sender<JournalMessage>>> = OnceCell::new();
static READER: OnceCell<Option<Sender<HistoryRequest>>> = OnceCell::new();

fn writer() -> Option<&'static Sender<JournalMessage>> {
    WRITER
        .get_or_init(|| {
            let (tx, rx) = bounded::<JournalMessage>(WRITER_QUEUE_CAPACITY);
            match std::thread::Builder::new()
                .name("jsh-execution-journal".to_owned())
                .spawn(move || {
                    while let Ok(message) = rx.recv() {
                        match message {
                            JournalMessage::Output(output) => {
                                if let Err(error) = append_event(output) {
                                    // Journal diagnostics can describe a
                                    // command or captured output. The terminal
                                    // log needs only the stable error class.
                                    log::warn!(
                                        "cannot append jsh execution output ({:?})",
                                        error.kind()
                                    );
                                }
                            }
                            JournalMessage::Flush(acknowledge) => {
                                let _ = acknowledge.send(());
                            }
                        }
                    }
                }) {
                Ok(_) => Some(tx),
                Err(error) => {
                    log::warn!("cannot start jsh execution journal writer: {error}");
                    None
                }
            }
        })
        .as_ref()
}

fn reader() -> Option<&'static Sender<HistoryRequest>> {
    READER
        .get_or_init(|| {
            let (tx, rx) = bounded::<HistoryRequest>(READER_QUEUE_CAPACITY);
            match std::thread::Builder::new()
                .name("jsh-execution-history".to_owned())
                .spawn(move || {
                    while let Ok(request) = rx.recv() {
                        let result = read_session_history(&request.session_id);
                        let (records, error) = match result {
                            Ok(records) => (records, None),
                            Err(error) => (Vec::new(), Some(error.to_string())),
                        };
                        // A dropped sidebar must not stall the single reader.
                        let _ = request.reply.try_send(HistorySnapshot {
                            session_id: request.session_id,
                            records,
                            error,
                        });
                    }
                }) {
                Ok(_) => Some(tx),
                Err(error) => {
                    log::warn!("cannot start jsh execution history reader: {error}");
                    None
                }
            }
        })
        .as_ref()
}

/// Start one bounded journal read on a dedicated worker. Disabled persistence
/// resolves immediately to an empty snapshot so callers can share one state
/// machine for both configurations.
pub fn request_history(session_id: String) -> Result<HistoryLoad, HistoryRequestError> {
    let (reply, receiver) = bounded(1);
    if !valid_jsh_session_id(&session_id) {
        let _ = reply.try_send(HistorySnapshot {
            session_id,
            records: Vec::new(),
            error: Some("invalid jsh session ID".to_string()),
        });
        return Ok(HistoryLoad { receiver });
    }
    if !output_capture_enabled() {
        let _ = reply.try_send(HistorySnapshot {
            session_id,
            records: Vec::new(),
            error: None,
        });
        return Ok(HistoryLoad { receiver });
    }
    let reader = reader().ok_or(HistoryRequestError::Closed)?;
    reader
        .try_send(HistoryRequest { session_id, reply })
        .map_err(|error| match error {
            TrySendError::Full(_) => HistoryRequestError::Full,
            TrySendError::Disconnected(_) => HistoryRequestError::Closed,
        })?;
    Ok(HistoryLoad { receiver })
}

/// Queue one completed output without blocking the terminal/UI thread.
///
/// A saturated queue deliberately rejects the newest item. Each command
/// remains represented by jsh's start/finish events, while memory stays
/// bounded even if the state directory is on a stalled filesystem.
pub fn submit(completed: CompletedExecution) -> Result<(), SubmitError> {
    if !output_capture_enabled() {
        return Ok(());
    }
    let Some(output) = OutputEvent::from_completed(completed) else {
        return Ok(());
    };
    let writer = writer().ok_or(SubmitError::Closed)?;
    writer
        .try_send(JournalMessage::Output(output))
        .map_err(|error| match error {
            TrySendError::Full(_) => SubmitError::Full,
            TrySendError::Disconnected(_) => SubmitError::Closed,
        })
}

/// Wait briefly for every output accepted before this call to reach disk.
/// Used during orderly application shutdown; normal terminal frames never
/// block on the journal.
pub fn flush(timeout: std::time::Duration) -> bool {
    if !output_capture_enabled() {
        return true;
    }
    let Some(Some(writer)) = WRITER.get() else {
        return true;
    };
    let (ack_tx, ack_rx) = bounded(1);
    let started = std::time::Instant::now();
    if writer
        .send_timeout(JournalMessage::Flush(ack_tx), timeout)
        .is_err()
    {
        return false;
    }
    ack_rx
        .recv_timeout(timeout.saturating_sub(started.elapsed()))
        .is_ok()
}

/// Whether a terminal output producer has a journal consumer to serve.
///
/// Callers which hold output lazily use this capability before constructing a
/// snapshot. `submit` repeats the check at the queue boundary because the
/// environment can change between the two calls (and because direct callers
/// must remain safe on their own).
pub fn output_capture_enabled() -> bool {
    std::env::var("JSH_EXECUTION_JOURNAL")
        .ok()
        .map(|value| {
            !matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "" | "0" | "false" | "no" | "off"
            )
        })
        .unwrap_or(true)
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

fn journal_path() -> io::Result<(PathBuf, bool)> {
    if let Some(path) = std::env::var_os("JSH_EXECUTION_JOURNAL_PATH")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
    {
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "JSH_EXECUTION_JOURNAL_PATH must be absolute",
            ));
        }
        validate_journal_path(&path)?;
        return Ok((path, true));
    }
    let state_dir = dirs::state_dir()
        .or_else(|| dirs::home_dir().map(|home| home.join(".local/state")))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no user state directory"))?;
    let path = state_dir.join("jsh/executions.jsonl");
    validate_journal_path(&path)?;
    Ok((path, false))
}

fn validate_journal_path(path: &Path) -> io::Result<()> {
    let Some(file_name) = path.file_name() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "execution journal path has no file name",
        ));
    };
    if file_name
        .to_str()
        .is_some_and(|name| name.eq_ignore_ascii_case(JOURNAL_LOCK_FILE_NAME))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "execution journal path collides with its lock sidecar",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let bytes = path.as_os_str().as_bytes();
        if bytes.len() > MAX_JOURNAL_PATH_BYTES
            || bytes.iter().any(|byte| matches!(*byte, 0..=0x1f | 0x7f))
            || path.to_str().is_some_and(|text| {
                text.chars().any(|ch| {
                    ch.is_control()
                        || crate::review_input::is_terminal_visual_spoofing_character(ch)
                })
            })
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "execution journal path is too long or contains unsafe display bytes",
            ));
        }
    }
    #[cfg(not(unix))]
    {
        let text = path.to_string_lossy();
        if text.len() > MAX_JOURNAL_PATH_BYTES
            || text.chars().any(char::is_control)
            || text
                .chars()
                .any(crate::review_input::is_terminal_visual_spoofing_character)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "execution journal path is too long or contains unsafe display text",
            ));
        }
    }
    Ok(())
}

fn harden_open_options(options: &mut OpenOptions) {
    #[cfg(unix)]
    {
        options
            .mode(0o600)
            // Nonblocking is inert for regular files and prevents a FIFO or
            // device substituted at a persistence pathname from hanging the
            // single journal worker before fstat can reject it.
            .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
}

fn validate_journal_file(file: &File, description: &str) -> io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{description} is not a regular file"),
        ));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{description} must have exactly one hard link"),
            ));
        }
        // SAFETY: geteuid has no preconditions and only reads process state.
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{description} is not owned by the current user"),
            ));
        }
        if metadata.mode() & 0o022 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{description} is writable by another user or group"),
            ));
        }
    }

    Ok(())
}

#[cfg(unix)]
fn set_private_open_file_permissions(file: &File) -> io::Result<()> {
    // fchmod updates ctime even when the requested mode is already present.
    // Reads and rejected appends must not manufacture metadata changes on an
    // already-private journal or lockfile.
    if file.metadata()?.permissions().mode() & 0o7777 == 0o600 {
        Ok(())
    } else {
        file.set_permissions(fs::Permissions::from_mode(0o600))
    }
}

#[cfg(unix)]
fn validate_journal_directory_trust(
    dir: &Path,
    owner_uid: u32,
    mode: u32,
    effective_uid: u32,
) -> io::Result<()> {
    if owner_uid != effective_uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "execution journal parent {} is not owned by the current user",
                dir.display()
            ),
        ));
    }
    // The sidecar name is fixed (`executions.lock`). Even a sticky shared
    // directory would let another account pre-create that name and split or
    // deny the cross-process lock namespace. Custom journals therefore live
    // in a namespace writable only by their owner.
    if mode & 0o022 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "execution journal parent {} is writable by another user or group",
                dir.display()
            ),
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn open_journal_directory_with_policy(dir: &Path, harden: bool) -> io::Result<File> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(dir)?;
    let metadata = directory.metadata()?;
    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "execution journal parent {} is not a directory",
                dir.display()
            ),
        ));
    }
    // SAFETY: geteuid has no preconditions and only reads process state.
    let effective_uid = unsafe { libc::geteuid() };
    // Only the implicit default (or a directory just created for either
    // source) may be repaired. Check ownership on the opened inode first so a
    // path race or another account's directory can never be chmodded.
    if harden && metadata.uid() == effective_uid {
        directory.set_permissions(fs::Permissions::from_mode(0o700))?;
    }
    let metadata = directory.metadata()?;
    validate_journal_directory_trust(
        dir,
        metadata.uid(),
        metadata.permissions().mode(),
        effective_uid,
    )?;
    Ok(directory)
}

fn open_journal_lock(path: &std::path::Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    harden_open_options(&mut options);
    let file = options.open(path)?;
    validate_journal_file(&file, "execution journal lock")?;
    #[cfg(unix)]
    set_private_open_file_permissions(&file)?;
    Ok(file)
}

fn open_journal_for_read(path: &std::path::Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    harden_open_options(&mut options);
    let file = options.open(path)?;
    validate_journal_file(&file, "execution journal")?;
    #[cfg(unix)]
    set_private_open_file_permissions(&file)?;
    Ok(file)
}

fn open_existing_journal_for_append(path: &std::path::Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    // Read access lets the locked writer inspect the final byte and separate a
    // torn shell event before appending a terminal-owned output event.
    options.read(true).append(true);
    harden_open_options(&mut options);
    let file = options.open(path)?;
    validate_journal_file(&file, "execution journal")?;
    Ok(file)
}

fn open_or_create_journal_for_append(path: &std::path::Path) -> io::Result<(File, bool)> {
    let mut options = OpenOptions::new();
    options.read(true).append(true).create_new(true);
    harden_open_options(&mut options);
    match options.open(path) {
        Ok(file) => {
            validate_journal_file(&file, "execution journal")?;
            Ok((file, true))
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            open_existing_journal_for_append(path).map(|file| (file, false))
        }
        Err(error) => Err(error),
    }
}

#[derive(Clone, Copy)]
enum JournalLockMode {
    Shared,
    Exclusive,
}

struct JournalFileLock {
    #[cfg(unix)]
    directory: File,
    file: File,
}

impl JournalFileLock {
    fn acquire(
        dir: &Path,
        lock_path: &Path,
        mode: JournalLockMode,
        harden_directory: bool,
    ) -> io::Result<Self> {
        Self::acquire_with_timeout(dir, lock_path, mode, JOURNAL_LOCK_TIMEOUT, harden_directory)
    }

    fn acquire_with_timeout(
        dir: &Path,
        lock_path: &Path,
        mode: JournalLockMode,
        timeout: Duration,
        harden_directory: bool,
    ) -> io::Result<Self> {
        let started = Instant::now();
        let wait = |file: &File| -> io::Result<()> {
            loop {
                if try_lock(file, mode)? {
                    return Ok(());
                }
                let elapsed = started.elapsed();
                if elapsed >= timeout {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!(
                            "timed out after {} ms waiting for execution journal lock {}",
                            timeout.as_millis(),
                            lock_path.display()
                        ),
                    ));
                }
                thread::sleep(JOURNAL_LOCK_POLL_INTERVAL.min(timeout - elapsed));
            }
        };

        // Stabilize the sidecar pathname before opening it. Every cooperating
        // reader/writer locks the directory in the same mode, so renaming a
        // locked sidecar and creating a new inode cannot split the protocol.
        #[cfg(unix)]
        let directory = {
            let directory = open_journal_directory_with_policy(dir, harden_directory)?;
            wait(&directory)?;
            directory
        };
        let file = match open_journal_lock(lock_path) {
            Ok(file) => file,
            Err(error) => {
                #[cfg(unix)]
                let _ = unlock(&directory);
                return Err(error);
            }
        };
        if let Err(error) = wait(&file) {
            #[cfg(unix)]
            let _ = unlock(&directory);
            return Err(error);
        }
        Ok(Self {
            #[cfg(unix)]
            directory,
            file,
        })
    }

    fn sync_directory(&self) -> io::Result<()> {
        #[cfg(unix)]
        {
            self.directory.sync_all()
        }
        #[cfg(not(unix))]
        {
            Ok(())
        }
    }
}

trait JournalAppendIo {
    fn write(&self, file: &mut File, bytes: &[u8]) -> io::Result<usize>;
    fn sync_data(&self, file: &File) -> io::Result<()>;
    fn sync_directory(&self, lock: &JournalFileLock) -> io::Result<()>;
}

struct SyncJournalAppendIo;

impl JournalAppendIo for SyncJournalAppendIo {
    fn write(&self, file: &mut File, bytes: &[u8]) -> io::Result<usize> {
        file.write(bytes)
    }

    fn sync_data(&self, file: &File) -> io::Result<()> {
        file.sync_data()
    }

    fn sync_directory(&self, lock: &JournalFileLock) -> io::Result<()> {
        lock.sync_directory()
    }
}

fn write_all_counted(
    append_io: &impl JournalAppendIo,
    file: &mut File,
    mut bytes: &[u8],
    written: &mut usize,
) -> io::Result<()> {
    while !bytes.is_empty() {
        match append_io.write(file, bytes) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "failed to write the complete execution journal event",
                ));
            }
            Ok(count) if count <= bytes.len() => {
                *written = written.saturating_add(count);
                bytes = &bytes[count..];
            }
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "execution journal writer reported an invalid byte count",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn append_write_error(error: io::Error, written: usize) -> io::Error {
    if written == 0 {
        io::Error::new(
            error.kind(),
            format!("execution journal append failed before writing any bytes: {error}"),
        )
    } else {
        commit_state_unknown(
            error.kind(),
            format!(
                "execution journal append failed after writing a visible prefix; commit state is unknown: {error}"
            ),
        )
    }
}

fn append_durability_error(stage: &str, error: io::Error) -> io::Error {
    commit_state_unknown(
        error.kind(),
        format!(
            "execution journal {stage} durability barrier failed after writing; commit state is unknown: {error}"
        ),
    )
}

fn append_post_write_error(stage: &str, error: io::Error) -> io::Error {
    commit_state_unknown(
        error.kind(),
        format!("execution journal {stage} failed after writing; commit state is unknown: {error}"),
    )
}

impl Drop for JournalFileLock {
    fn drop(&mut self) {
        if let Err(error) = unlock(&self.file) {
            log::warn!("failed to release execution journal lock: {error}");
        }
        #[cfg(unix)]
        if let Err(error) = unlock(&self.directory) {
            log::warn!("failed to release execution journal directory lock: {error}");
        }
    }
}

fn read_session_history(session_id: &str) -> io::Result<Vec<PersistedExecution>> {
    if !valid_jsh_session_id(session_id) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid jsh session ID",
        ));
    }
    let (path, custom_path) = journal_path()?;
    match fs::symlink_metadata(&path) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    }
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "journal has no parent"))?;
    let lock_path = dir.join(JOURNAL_LOCK_FILE_NAME);
    let _lock = JournalFileLock::acquire(dir, &lock_path, JournalLockMode::Shared, !custom_path)?;

    match read_session_history_file(&path, session_id) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        result => result,
    }
}

fn read_session_history_file(
    path: &std::path::Path,
    session_id: &str,
) -> io::Result<Vec<PersistedExecution>> {
    read_session_history_file_with_line_limit(path, session_id, MAX_JOURNAL_EVENT_LINES)
}

fn read_session_history_file_with_line_limit(
    path: &std::path::Path,
    session_id: &str,
    max_event_lines: usize,
) -> io::Result<Vec<PersistedExecution>> {
    let file = open_journal_for_read(path)?;
    if file.metadata()?.len() > MAX_JOURNAL_READ_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "jsh execution journal exceeds its bounded size",
        ));
    }

    let mut records = HashMap::<String, FoldedExecution>::new();
    // Keep the working set bounded while folding a journal containing many
    // tiny start records. The file byte cap alone does not bound HashMap
    // overhead. jsh applies this limit globally before filtering one terminal
    // session, and its compactor permanently discards the same global oldest
    // records. Retention authority follows physical Start order rather than
    // untrusted or reset-prone event timestamps and sequence numbers. Keep the
    // presentation sort separate below. The two indexes make replacement and
    // eviction logarithmic without storing the ordinal in the public record.
    let mut record_order = BTreeMap::<usize, String>::new();
    let mut record_positions = HashMap::<String, usize>::new();
    let read_limit = MAX_JOURNAL_READ_BYTES.saturating_add(1);
    let mut reader = BufReader::new(file.take(read_limit));
    let mut line = Vec::new();
    let mut event_lines = 0usize;
    while let Some(within_limit) = read_bounded_line(&mut reader, &mut line)? {
        event_lines = event_lines.checked_add(1).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "jsh execution journal event count overflowed",
            )
        })?;
        if event_lines > max_event_lines {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "jsh execution journal exceeds its event-count limit",
            ));
        }
        if !within_limit {
            continue;
        }
        // JSON object identity must be unambiguous before even the lightweight
        // Start envelope can retire an existing lifecycle. This also rejects
        // duplicate unknown members that Serde would otherwise skip while
        // decoding Finish/Output events.
        if crate::bounded_json::validate_no_duplicate_members(&line).is_err() {
            continue;
        }
        // A recognized v1 Start with a valid ID is authoritative even when
        // strict decoding of its remaining metadata fails. Clear the old
        // lifecycle first so later Finish/Output events cannot bind to it.
        if let Some(id) = recognized_v1_start_id(&line) {
            if records.remove(id.as_ref()).is_some() {
                if let Some(position) = record_positions.remove(id.as_ref()) {
                    record_order.remove(&position);
                }
            }
        }
        let Ok(event) = serde_json::from_slice::<PersistedEvent>(&line) else {
            continue;
        };
        if event.version() != EXECUTION_JOURNAL_VERSION {
            continue;
        }
        match event {
            PersistedEvent::Start(StartEvent {
                id,
                session_id: event_session_id,
                seq,
                command,
                command_truncated,
                cwd,
                started_at_ms,
                ..
            }) => {
                if !is_valid_jsh_execution_id(&id)
                    || event_session_id
                        .as_deref()
                        .is_some_and(|id| !valid_jsh_session_id(id))
                    || !is_valid_jsh_command(&command)
                    || !is_valid_jsh_cwd(&cwd)
                {
                    continue;
                }
                let record = PersistedExecution {
                    id: id.clone(),
                    seq,
                    command,
                    command_truncated,
                    cwd,
                    started_at_ms,
                    exit_code: None,
                    duration_ms: None,
                    cwd_after: None,
                    ended_at_ms: None,
                    output: None,
                };
                record_order.insert(event_lines, id.clone());
                record_positions.insert(id.clone(), event_lines);
                records.insert(
                    id,
                    FoldedExecution {
                        session_id: event_session_id,
                        record,
                        finish_conflicted: false,
                        output_conflicted: false,
                    },
                );
                while records.len() > MAX_RETAINED_EXECUTIONS {
                    let Some((_, oldest_id)) = record_order.pop_first() else {
                        break;
                    };
                    record_positions.remove(&oldest_id);
                    records.remove(&oldest_id);
                }
            }
            PersistedEvent::Finish {
                id,
                exit_code,
                duration_ms,
                cwd_after,
                ended_at_ms,
                ..
            } => {
                if !is_valid_jsh_execution_id(&id) || !is_valid_jsh_cwd(&cwd_after) {
                    continue;
                }
                if let Some(folded) = records.get_mut(&id) {
                    if folded.finish_conflicted {
                        continue;
                    }
                    let record = &mut folded.record;
                    if record.exit_code.is_none() {
                        record.exit_code = Some(exit_code);
                        record.duration_ms = Some(duration_ms);
                        record.cwd_after = Some(cwd_after);
                        record.ended_at_ms = Some(ended_at_ms);
                    } else if record.exit_code != Some(exit_code)
                        || record.duration_ms != Some(duration_ms)
                        || record.cwd_after.as_deref() != Some(cwd_after.as_str())
                        || record.ended_at_ms != Some(ended_at_ms)
                    {
                        record.exit_code = None;
                        record.duration_ms = None;
                        record.cwd_after = None;
                        record.ended_at_ms = None;
                        folded.finish_conflicted = true;
                    }
                }
            }
            PersistedEvent::Output {
                id,
                text,
                truncated,
                total_bytes,
                captured_at_ms,
                ..
            } => {
                if !is_valid_jsh_execution_id(&id) || text.len() > MAX_OUTPUT_BYTES {
                    continue;
                }
                if let Some(folded) = records.get_mut(&id) {
                    let (truncated, total_bytes) =
                        normalize_output_metadata(text.len(), truncated, total_bytes);
                    let output = PersistedExecutionOutput {
                        total_bytes,
                        text,
                        truncated,
                        captured_at_ms,
                    };
                    if folded.output_conflicted {
                        continue;
                    }
                    match folded.record.output.as_ref() {
                        None => folded.record.output = Some(output),
                        Some(existing) if existing == &output => {}
                        Some(_) => {
                            folded.record.output = None;
                            folded.output_conflicted = true;
                        }
                    }
                }
            }
            PersistedEvent::Conflict(event) => {
                if !is_valid_jsh_execution_id(&event.id) {
                    continue;
                }
                if let Some(folded) = records.get_mut(&event.id) {
                    match event.slot {
                        ConflictSlot::Finish => {
                            folded.record.exit_code = None;
                            folded.record.duration_ms = None;
                            folded.record.cwd_after = None;
                            folded.record.ended_at_ms = None;
                            folded.finish_conflicted = true;
                        }
                        ConflictSlot::Output => {
                            folded.record.output = None;
                            folded.output_conflicted = true;
                        }
                    }
                }
            }
        }
    }

    let bytes_read = read_limit.saturating_sub(reader.get_ref().limit());
    if bytes_read > MAX_JOURNAL_READ_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "jsh execution journal grew beyond its bounded size while reading",
        ));
    }

    let mut ordered_records = Vec::with_capacity(records.len());
    for entry in records.into_values() {
        let Some(position) = record_positions.remove(&entry.record.id) else {
            return Err(io::Error::other(
                "jsh execution journal retention index became inconsistent",
            ));
        };
        if entry.session_id.as_deref() == Some(session_id) {
            ordered_records.push((position, entry.record));
        }
    }
    ordered_records.sort_by(|left, right| (left.0, &left.1.id).cmp(&(right.0, &right.1.id)));
    let mut records = ordered_records
        .into_iter()
        .map(|(_, record)| record)
        .collect::<Vec<_>>();
    let keep_from = records.len().saturating_sub(MAX_RETAINED_EXECUTIONS);
    Ok(records.split_off(keep_from))
}

/// Read and, when necessary, discard one JSONL event without allocating more
/// than jsh's public per-event limit. `false` denotes an oversized event.
fn read_bounded_line(reader: &mut impl BufRead, line: &mut Vec<u8>) -> io::Result<Option<bool>> {
    line.clear();
    let mut saw_bytes = false;
    let mut oversized = false;
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            return Ok(saw_bytes.then_some(!oversized));
        }
        saw_bytes = true;
        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(buffer.len(), |index| index + 1);
        if !oversized {
            // The public limit covers the JSON event, not its optional line
            // delimiter. Counting `+ 1` unconditionally admitted an
            // unterminated event with MAX_EVENT_LINE_BYTES + 1 payload bytes.
            // Discount a newline only when this chunk actually contains one.
            let payload_bytes = consumed.saturating_sub(usize::from(newline.is_some()));
            if line.len().saturating_add(payload_bytes) <= MAX_EVENT_LINE_BYTES {
                line.extend_from_slice(&buffer[..consumed]);
            } else {
                line.clear();
                oversized = true;
            }
        }
        reader.consume(consumed);
        if newline.is_some() {
            return Ok(Some(!oversized));
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BoundOutputSnapshot {
    Ready,
    ExactTerminal,
    Rejected,
}

fn start_matches_lifecycle(start: &StartEvent, lifecycle: &ExecutionLifecycle) -> bool {
    start.jsh_execution_version == EXECUTION_JOURNAL_VERSION
        && start.id == lifecycle.id
        && start.session_id.as_deref() == Some(lifecycle.session_id.as_str())
        && start.seq == lifecycle.seq
        && start.started_at_ms == lifecycle.started_at_ms
        && is_valid_jsh_execution_id(&start.id)
        && start
            .session_id
            .as_deref()
            .is_some_and(valid_jsh_session_id)
        && is_valid_jsh_command(&start.command)
        && is_valid_jsh_cwd(&start.cwd)
}

fn output_matches_expected(
    jsh_execution_version: u32,
    id: &str,
    text: &str,
    truncated: bool,
    total_bytes: u64,
    captured_at_ms: u64,
    expected: &OutputEvent,
) -> bool {
    jsh_execution_version == expected.jsh_execution_version
        && id == expected.id
        && text == expected.text
        && truncated == expected.truncated
        && total_bytes == expected.total_bytes
        && captured_at_ms == expected.captured_at_ms
}

/// Inspect the exact inode snapshot protected by the writer's exclusive lock.
/// A bound Output may be appended only while its complete Start remains the
/// authoritative same-ID lifecycle and its output slot is still empty.
fn inspect_bound_output_snapshot(
    journal: &mut File,
    current_len: u64,
    lifecycle: &ExecutionLifecycle,
    expected: &OutputEvent,
    max_event_lines: usize,
) -> io::Result<BoundOutputSnapshot> {
    if current_len == 0 || current_len > MAX_JOURNAL_READ_BYTES {
        return Ok(BoundOutputSnapshot::Rejected);
    }
    journal.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::new((&mut *journal).take(current_len));
    let mut line = Vec::new();
    let mut event_lines = 0usize;
    let mut active_lifecycle = false;
    let mut output_seen = false;
    let mut output_conflicted = false;
    let mut matching_outputs = 0usize;
    let mut terminal_exact = false;

    while let Some(within_limit) = read_bounded_line(&mut reader, &mut line)? {
        event_lines = event_lines.checked_add(1).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "execution journal event count overflowed during Output binding",
            )
        })?;
        if event_lines > max_event_lines {
            return Ok(BoundOutputSnapshot::Rejected);
        }
        terminal_exact = false;
        // No unterminated physical tail is a safe boundary for asynchronous
        // output. In particular, never place a second Output after a torn one.
        if !within_limit || !line.ends_with(b"\n") {
            return Ok(BoundOutputSnapshot::Rejected);
        }
        if crate::bounded_json::validate_no_duplicate_members(&line).is_err() {
            continue;
        }

        let decoded = serde_json::from_slice::<PersistedEvent>(&line).ok();
        if recognized_v1_start_id(&line).as_deref() == Some(lifecycle.id.as_str()) {
            active_lifecycle = false;
            output_seen = false;
            output_conflicted = false;
            matching_outputs = 0;
            if matches!(decoded, Some(PersistedEvent::Start(ref start)) if start_matches_lifecycle(start, lifecycle))
            {
                active_lifecycle = true;
            }
            continue;
        }
        if !active_lifecycle {
            continue;
        }

        match decoded {
            Some(PersistedEvent::Output {
                jsh_execution_version,
                id,
                text,
                truncated,
                total_bytes,
                captured_at_ms,
            }) if jsh_execution_version == EXECUTION_JOURNAL_VERSION
                && id == lifecycle.id
                && is_valid_jsh_execution_id(&id)
                && text.len() <= MAX_OUTPUT_BYTES =>
            {
                if output_seen || output_conflicted {
                    output_conflicted = true;
                } else {
                    output_seen = true;
                    if output_matches_expected(
                        jsh_execution_version,
                        &id,
                        &text,
                        truncated,
                        total_bytes,
                        captured_at_ms,
                        expected,
                    ) {
                        matching_outputs = matching_outputs.saturating_add(1);
                        terminal_exact = true;
                    }
                }
            }
            Some(PersistedEvent::Conflict(ConflictEvent {
                jsh_execution_version,
                id,
                slot: ConflictSlot::Output,
            })) if jsh_execution_version == EXECUTION_JOURNAL_VERSION
                && id == lifecycle.id
                && is_valid_jsh_execution_id(&id) =>
            {
                output_seen = true;
                output_conflicted = true;
            }
            _ => {}
        }
    }

    Ok(if !active_lifecycle || output_conflicted {
        BoundOutputSnapshot::Rejected
    } else if output_seen {
        if terminal_exact && matching_outputs == 1 {
            BoundOutputSnapshot::ExactTerminal
        } else {
            BoundOutputSnapshot::Rejected
        }
    } else {
        BoundOutputSnapshot::Ready
    })
}

fn recover_exact_terminal_output_locked(
    journal_path: &Path,
    lock: &JournalFileLock,
    output: &BoundOutput,
    max_event_lines: usize,
) -> io::Result<bool> {
    let mut journal = open_existing_journal_for_append(journal_path)?;
    let current_len = journal.metadata()?.len();
    if inspect_bound_output_snapshot(
        &mut journal,
        current_len,
        &output.lifecycle,
        &output.event,
        max_event_lines,
    )? != BoundOutputSnapshot::ExactTerminal
    {
        return Ok(false);
    }
    journal.sync_data()?;
    lock.sync_directory()?;
    Ok(true)
}

fn prepare_journal_path() -> io::Result<PathBuf> {
    let (path, custom_path) = journal_path()?;
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "journal has no parent"))?;
    let dir_already_existed = match fs::symlink_metadata(dir) {
        Ok(_) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(error),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;

        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        let _directory =
            open_journal_directory_with_policy(dir, !custom_path || !dir_already_existed)?;
    }
    #[cfg(not(unix))]
    fs::create_dir_all(dir)?;
    Ok(path)
}

fn append_event(mut output: BoundOutput) -> io::Result<()> {
    let (event, encoded) = encode_event_with_value(output.event)?;
    output.event = event;
    let journal_path = prepare_journal_path()?;
    append_bound_output_to_path(&journal_path, &encoded, &output)
}

fn append_bound_output_to_path(
    journal_path: &Path,
    encoded: &[u8],
    output: &BoundOutput,
) -> io::Result<()> {
    append_encoded_event_to_path_with_line_limit_and_io_inner(
        journal_path,
        encoded,
        MAX_JOURNAL_EVENT_LINES,
        &SyncJournalAppendIo,
        Some(output),
    )
}

#[cfg(test)]
fn append_encoded_event_to_path(journal_path: &std::path::Path, encoded: &[u8]) -> io::Result<()> {
    append_encoded_event_to_path_with_line_limit(journal_path, encoded, MAX_JOURNAL_EVENT_LINES)
}

#[cfg(test)]
fn append_encoded_event_to_path_with_line_limit(
    journal_path: &std::path::Path,
    encoded: &[u8],
    max_event_lines: usize,
) -> io::Result<()> {
    append_encoded_event_to_path_with_line_limit_and_io(
        journal_path,
        encoded,
        max_event_lines,
        &SyncJournalAppendIo,
    )
}

#[cfg(test)]
fn append_encoded_event_to_path_with_line_limit_and_io(
    journal_path: &std::path::Path,
    encoded: &[u8],
    max_event_lines: usize,
    append_io: &impl JournalAppendIo,
) -> io::Result<()> {
    append_encoded_event_to_path_with_line_limit_and_io_inner(
        journal_path,
        encoded,
        max_event_lines,
        append_io,
        None,
    )
}

fn append_encoded_event_to_path_with_line_limit_and_io_inner(
    journal_path: &Path,
    encoded: &[u8],
    max_event_lines: usize,
    append_io: &impl JournalAppendIo,
    bound_output: Option<&BoundOutput>,
) -> io::Result<()> {
    if !encoded.ends_with(b"\n") || encoded[..encoded.len() - 1].contains(&b'\n') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "encoded journal event must contain exactly one trailing newline",
        ));
    }
    let dir = journal_path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "journal has no parent"))?;
    let lock_path = dir.join(JOURNAL_LOCK_FILE_NAME);

    let lock = JournalFileLock::acquire(dir, &lock_path, JournalLockMode::Exclusive, false)?;

    let append_result = (|| {
        let (mut journal, created_pathname) = match bound_output {
            Some(_) => (open_existing_journal_for_append(journal_path)?, false),
            None => open_or_create_journal_for_append(journal_path)?,
        };
        let current_len = journal.metadata()?.len();
        if let Some(output) = bound_output {
            if inspect_bound_output_snapshot(
                &mut journal,
                current_len,
                &output.lifecycle,
                &output.event,
                max_event_lines,
            )? != BoundOutputSnapshot::Ready
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "execution output does not match the current journal lifecycle",
                ));
            }
        }
        let needs_separator = if current_len == 0 {
            false
        } else {
            journal.seek(SeekFrom::End(-1))?;
            let mut last = [0_u8; 1];
            journal.read_exact(&mut last)?;
            last[0] != b'\n'
        };
        let appended_bytes = encoded.len().saturating_add(usize::from(needs_separator));
        if !journal_append_within_bound(current_len, appended_bytes) {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "jsh execution journal is awaiting lifecycle compaction",
            ));
        }
        let exact_line_count = if current_len < max_event_lines as u64 {
            None
        } else {
            Some(count_physical_lines_cached(
                &mut journal,
                journal_path,
                current_len,
            )?)
        };
        if exact_line_count.is_some_and(|line_count| line_count >= max_event_lines) {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "jsh execution journal append exceeds its event-count limit",
            ));
        }
        #[cfg(unix)]
        set_private_open_file_permissions(&journal)?;
        invalidate_append_line_count_cache(journal_path)?;
        let mut written = 0;
        if needs_separator {
            // Match jsh's lifecycle writer: a crash may leave one incomplete
            // JSON object, but it must not consume the next complete event.
            if let Err(error) = write_all_counted(append_io, &mut journal, b"\n", &mut written) {
                return finish_failed_append_write(
                    journal_path,
                    journal,
                    created_pathname,
                    written,
                    error,
                );
            }
        }
        if let Err(error) = write_all_counted(append_io, &mut journal, encoded, &mut written) {
            return finish_failed_append_write(
                journal_path,
                journal,
                created_pathname,
                written,
                error,
            );
        }
        if let Some(line_count) = exact_line_count {
            cache_completed_append(journal_path, &journal, line_count + 1)
                .map_err(|error| append_post_write_error("line-count bookkeeping", error))?;
        }
        append_io
            .sync_data(&journal)
            .map_err(|error| append_durability_error("data", error))?;
        if created_pathname {
            append_io
                .sync_directory(&lock)
                .map_err(|error| append_durability_error("directory", error))?;
        }
        Ok(())
    })();

    match append_result {
        Err(error) if bound_output.is_some() && is_commit_state_unknown(&error) => {
            let output = bound_output.expect("presence checked above");
            match recover_exact_terminal_output_locked(
                journal_path,
                &lock,
                output,
                max_event_lines,
            ) {
                Ok(true) => Ok(()),
                Ok(false) => Err(error),
                Err(recovery_error) => Err(commit_state_unknown(
                    recovery_error.kind(),
                    format!(
                        "execution journal Output recovery failed after an unknown append result: {recovery_error}; original error: {error}"
                    ),
                )),
            }
        }
        result => result,
    }
}

fn finish_failed_append_write(
    journal_path: &Path,
    journal: File,
    created_pathname: bool,
    written: usize,
    error: io::Error,
) -> io::Result<()> {
    if created_pathname && written == 0 {
        remove_created_empty_journal(journal_path, &journal).map_err(|cleanup_error| {
            io::Error::new(
                cleanup_error.kind(),
                format!(
                    "execution journal append failed before writing and its empty pathname could not be removed: {error}; cleanup failed: {cleanup_error}"
                ),
            )
        })?;
    }
    drop(journal);
    Err(append_write_error(error, written))
}

fn remove_created_empty_journal(journal_path: &Path, journal: &File) -> io::Result<()> {
    let opened = journal.metadata()?;
    let named = fs::symlink_metadata(journal_path)?;
    if opened.len() != 0 || !named.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "new execution journal pathname no longer names the empty opened file",
        ));
    }
    #[cfg(unix)]
    if (opened.dev(), opened.ino()) != (named.dev(), named.ino()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "new execution journal pathname was replaced before cleanup",
        ));
    }
    fs::remove_file(journal_path)
}

fn count_physical_lines_cached(
    journal: &mut File,
    journal_path: &Path,
    current_len: u64,
) -> io::Result<usize> {
    let metadata = journal.metadata()?;
    let mut cache = APPEND_LINE_COUNT_CACHE
        .lock()
        .map_err(|_| io::Error::other("journal line-count cache is poisoned"))?;
    let can_resume = cache.as_ref().is_some_and(|cached| {
        cached.path == journal_path
            && cached.scanned_bytes <= current_len
            && same_cached_file(cached, &metadata)
    });
    let (offset, mut newline_count, mut ends_with_newline) = if can_resume {
        let cached = cache.as_ref().expect("cache presence checked above");
        (
            cached.scanned_bytes,
            cached.newline_count,
            cached.ends_with_newline,
        )
    } else {
        (0, 0, false)
    };

    journal.seek(SeekFrom::Start(offset))?;
    let mut remaining = current_len.saturating_sub(offset);
    let mut buffer = [0_u8; 64 * 1024];
    while remaining != 0 {
        let requested = usize::try_from(remaining.min(buffer.len() as u64)).unwrap_or(buffer.len());
        let read = journal.read(&mut buffer[..requested])?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "execution journal changed while counting events",
            ));
        }
        newline_count = newline_count
            .checked_add(buffer[..read].iter().filter(|byte| **byte == b'\n').count())
            .ok_or_else(|| io::Error::other("execution journal event count overflowed"))?;
        ends_with_newline = buffer[read - 1] == b'\n';
        remaining -= read as u64;
    }

    *cache = Some(AppendLineCountCache {
        path: journal_path.to_owned(),
        #[cfg(unix)]
        device: metadata.dev(),
        #[cfg(unix)]
        inode: metadata.ino(),
        scanned_bytes: current_len,
        newline_count,
        ends_with_newline,
    });
    newline_count
        .checked_add(usize::from(current_len != 0 && !ends_with_newline))
        .ok_or_else(|| io::Error::other("execution journal event count overflowed"))
}

#[cfg(unix)]
fn same_cached_file(cache: &AppendLineCountCache, metadata: &fs::Metadata) -> bool {
    cache.device == metadata.dev() && cache.inode == metadata.ino()
}

#[cfg(not(unix))]
fn same_cached_file(_cache: &AppendLineCountCache, _metadata: &fs::Metadata) -> bool {
    false
}

fn invalidate_append_line_count_cache(journal_path: &Path) -> io::Result<()> {
    let mut cache = APPEND_LINE_COUNT_CACHE
        .lock()
        .map_err(|_| io::Error::other("journal line-count cache is poisoned"))?;
    if cache
        .as_ref()
        .is_some_and(|cached| cached.path == journal_path)
    {
        *cache = None;
    }
    Ok(())
}

fn cache_completed_append(
    journal_path: &Path,
    journal: &File,
    physical_lines: usize,
) -> io::Result<()> {
    let metadata = journal.metadata()?;
    let mut cache = APPEND_LINE_COUNT_CACHE
        .lock()
        .map_err(|_| io::Error::other("journal line-count cache is poisoned"))?;
    *cache = Some(AppendLineCountCache {
        path: journal_path.to_owned(),
        #[cfg(unix)]
        device: metadata.dev(),
        #[cfg(unix)]
        inode: metadata.ino(),
        scanned_bytes: metadata.len(),
        newline_count: physical_lines,
        ends_with_newline: true,
    });
    Ok(())
}

fn journal_append_within_bound(current_bytes: u64, event_bytes: usize) -> bool {
    current_bytes.saturating_add(u64::try_from(event_bytes).unwrap_or(u64::MAX))
        <= MAX_JOURNAL_READ_BYTES
}

/// Whether an execution id can correlate jsh's OSC lifecycle with its journal.
///
/// Generic FinalTerm producers may use broader opaque IDs in the in-memory
/// timeline, but only this exact ASCII token grammar identifies jsh's durable
/// lifecycle and output events.
pub fn is_valid_jsh_execution_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_EXECUTION_ID_BYTES
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
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

fn valid_jsh_session_id(id: &str) -> bool {
    is_valid_jsh_session_id(id)
}

/// Whether command text matches the exact metadata contract emitted by jsh.
///
/// The generic review validator intentionally permits the assigned interlinear
/// annotation controls U+FFF9..=U+FFFB. jsh's terminal renderer rejects them
/// because they remain invisible there, so its OSC and journal consumers must
/// apply that narrower protocol contract too.
pub(crate) fn is_valid_jsh_command(command: &str) -> bool {
    !command.is_empty()
        && command.len() <= MAX_COMMAND_BYTES
        // jsh commands are structurally multiline. Preserve newline and tab,
        // which its JSONL serializer safely escapes, while still refusing
        // terminal controls and non-control display ambiguity on replay.
        && !command
            .chars()
            .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\t'))
        && !command.chars().any(|ch| {
            !ch.is_control() && crate::review_input::is_terminal_visual_spoofing_character(ch)
        })
}

/// Whether a cwd can identify the same directory across jsh's OSC and journal
/// channels without truncation, terminal controls, or visual ambiguity.
pub fn is_valid_jsh_cwd(cwd: &str) -> bool {
    !cwd.is_empty()
        && cwd.len() <= MAX_CWD_BYTES
        && !cwd.chars().any(char::is_control)
        && !cwd
            .chars()
            .any(crate::review_input::is_terminal_visual_spoofing_character)
}

fn bounded_text(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let head_budget = max_bytes / 2;
    let tail_budget = max_bytes - head_budget;
    let mut head_end = head_budget;
    while !value.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = value.len() - tail_budget;
    while !value.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let mut bounded = String::with_capacity(max_bytes);
    bounded.push_str(&value[..head_end]);
    bounded.push_str(&value[tail_start..]);
    bounded
}

/// Serialize within the same one-line limit enforced by jsh's reader. JSON
/// escaping can expand control-heavy terminal text well beyond its UTF-8 byte
/// count, so retry with a smaller UTF-8-safe head/tail snapshot when needed.
#[cfg(test)]
fn encode_event(event: OutputEvent) -> io::Result<Vec<u8>> {
    encode_event_with_value(event).map(|(_, encoded)| encoded)
}

fn encode_event_with_value(mut event: OutputEvent) -> io::Result<(OutputEvent, Vec<u8>)> {
    let mut encoded = serde_json::to_vec(&event).map_err(io::Error::other)?;
    if encoded.len() > MAX_EVENT_LINE_BYTES {
        event.text = bounded_text(&event.text, MAX_OUTPUT_BYTES / 2);
        event.truncated = true;
        event.total_bytes = event.total_bytes.max(event.text.len() as u64);
        encoded = serde_json::to_vec(&event).map_err(io::Error::other)?;
    }
    if encoded.len() > MAX_EVENT_LINE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "jsh execution output event exceeds the journal line limit",
        ));
    }
    encoded.push(b'\n');
    Ok((event, encoded))
}

#[cfg(unix)]
fn try_lock(file: &std::fs::File, mode: JournalLockMode) -> io::Result<bool> {
    use std::os::fd::AsRawFd;
    let operation = match mode {
        JournalLockMode::Shared => libc::LOCK_SH,
        JournalLockMode::Exclusive => libc::LOCK_EX,
    } | libc::LOCK_NB;
    // SAFETY: `file` remains open for the flock lifetime and flock does not
    // dereference userspace pointers.
    let result = unsafe { libc::flock(file.as_raw_fd(), operation) };
    if result == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error
        .raw_os_error()
        .is_some_and(|code| code == libc::EAGAIN || code == libc::EWOULDBLOCK)
    {
        Ok(false)
    } else {
        Err(error)
    }
}

#[cfg(not(unix))]
fn try_lock(_file: &std::fs::File, _mode: JournalLockMode) -> io::Result<bool> {
    Ok(true)
}

#[cfg(unix)]
fn unlock(file: &std::fs::File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: see `lock_exclusive`; the descriptor is still owned by `file`.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(unix))]
fn unlock(_file: &std::fs::File) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

    fn lifecycle(id: &str) -> ExecutionLifecycle {
        ExecutionLifecycle {
            id: id.to_owned(),
            session_id: "wanted".to_owned(),
            seq: 1,
            started_at_ms: 1,
        }
    }

    fn lifecycle_start_line(lifecycle: &ExecutionLifecycle) -> Vec<u8> {
        format!(
            "{{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"{}\",\"session_id\":\"{}\",\"seq\":{},\"command\":\"true\",\"cwd\":\"/tmp\",\"started_at_ms\":{}}}\n",
            lifecycle.id, lifecycle.session_id, lifecycle.seq, lifecycle.started_at_ms
        )
        .into_bytes()
    }

    fn bound_output(lifecycle: ExecutionLifecycle, text: &str) -> BoundOutput {
        OutputEvent::from_completed(CompletedExecution {
            lifecycle,
            output: text.to_owned(),
            output_available: true,
            truncated: false,
            total_bytes: text.len(),
        })
        .expect("valid available test output")
    }

    fn encoded_bound_output(lifecycle: ExecutionLifecycle, text: &str) -> (BoundOutput, Vec<u8>) {
        let mut output = bound_output(lifecycle, text);
        let (event, encoded) = encode_event_with_value(output.event).unwrap();
        output.event = event;
        (output, encoded)
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum AppendFaultStage {
        WriteAfter(usize),
        DataSync,
        DirectorySync,
    }

    struct FaultingAppendIo {
        stage: AppendFaultStage,
        write_calls: std::cell::Cell<usize>,
        written: std::cell::Cell<usize>,
        data_sync_calls: std::cell::Cell<usize>,
        directory_sync_calls: std::cell::Cell<usize>,
    }

    impl FaultingAppendIo {
        fn new(stage: AppendFaultStage) -> Self {
            Self {
                stage,
                write_calls: std::cell::Cell::new(0),
                written: std::cell::Cell::new(0),
                data_sync_calls: std::cell::Cell::new(0),
                directory_sync_calls: std::cell::Cell::new(0),
            }
        }
    }

    impl JournalAppendIo for FaultingAppendIo {
        fn write(&self, file: &mut File, bytes: &[u8]) -> io::Result<usize> {
            self.write_calls.set(self.write_calls.get() + 1);
            let requested = match self.stage {
                AppendFaultStage::WriteAfter(limit) => {
                    let remaining = limit.saturating_sub(self.written.get());
                    if remaining == 0 {
                        return Err(io::Error::other("injected append-write failure"));
                    }
                    remaining.min(bytes.len())
                }
                AppendFaultStage::DataSync | AppendFaultStage::DirectorySync => bytes.len(),
            };
            let count = file.write(&bytes[..requested])?;
            self.written.set(self.written.get() + count);
            Ok(count)
        }

        fn sync_data(&self, file: &File) -> io::Result<()> {
            self.data_sync_calls.set(self.data_sync_calls.get() + 1);
            if self.stage == AppendFaultStage::DataSync {
                Err(io::Error::other("injected data-sync failure"))
            } else {
                file.sync_data()
            }
        }

        fn sync_directory(&self, lock: &JournalFileLock) -> io::Result<()> {
            self.directory_sync_calls
                .set(self.directory_sync_calls.get() + 1);
            if self.stage == AppendFaultStage::DirectorySync {
                Err(io::Error::other("injected directory-sync failure"))
            } else {
                lock.sync_directory()
            }
        }
    }

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let id = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "ember-execution-journal-{}-{label}-{id}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            #[cfg(unix)]
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn temporary_journal(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "ember-execution-journal-{}-{name}.jsonl",
            std::process::id()
        ))
    }

    fn write_temporary_journal(path: &Path, contents: impl AsRef<[u8]>) {
        fs::write(path, contents).unwrap();
        #[cfg(unix)]
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[cfg(unix)]
    fn stable_metadata(path: &Path) -> (u64, u64, u32, u64, u32, i64, i64, i64, i64) {
        let metadata = fs::metadata(path).unwrap();
        (
            metadata.dev(),
            metadata.ino(),
            metadata.uid(),
            metadata.nlink(),
            metadata.mode(),
            metadata.mtime(),
            metadata.mtime_nsec(),
            metadata.ctime(),
            metadata.ctime_nsec(),
        )
    }

    fn start_event_with_raw_bytes(id: &str, raw_bytes: usize) -> Vec<u8> {
        let prefix = format!(
            "{{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"{id}\",\"session_id\":\"wanted\",\"seq\":9,\"command\":\""
        );
        let suffix = b"\",\"cwd\":\"/new\",\"started_at_ms\":9}";
        assert!(raw_bytes >= prefix.len() + suffix.len());
        let mut event = prefix.into_bytes();
        event.resize(raw_bytes - suffix.len(), b'x');
        event.extend_from_slice(suffix);
        assert_eq!(event.len(), raw_bytes);
        assert!(serde_json::from_slice::<serde_json::Value>(&event).is_ok());
        event
    }

    fn escaped_multibyte_output_event(id: &str, decoded_bytes: usize) -> Vec<u8> {
        let mut event = format!(
            "{{\"jsh_execution_version\":1,\"event\":\"output\",\"id\":\"{id}\",\"text\":\""
        )
        .into_bytes();
        for _ in 0..decoded_bytes / "界".len() {
            event.extend_from_slice(br"\u754c");
        }
        event.resize(event.len() + decoded_bytes % "界".len(), b'x');
        event.extend_from_slice(
            format!(
                "\",\"truncated\":false,\"total_bytes\":{decoded_bytes},\"captured_at_ms\":2}}"
            )
            .as_bytes(),
        );
        event
    }

    fn unknown_event_with_raw_bytes(id: &str, raw_bytes: usize) -> Vec<u8> {
        let prefix = format!(
            "{{\"jsh_execution_version\":1,\"event\":\"future\",\"id\":\"{id}\",\"payload\":\""
        );
        let suffix = b"\"}";
        assert!(raw_bytes >= prefix.len() + suffix.len());
        let mut event = prefix.into_bytes();
        event.resize(raw_bytes - suffix.len(), b'x');
        event.extend_from_slice(suffix);
        assert_eq!(event.len(), raw_bytes);
        event
    }

    #[test]
    fn unavailable_output_is_not_persisted() {
        let completed = CompletedExecution {
            lifecycle: lifecycle("id"),
            output: String::new(),
            output_available: false,
            truncated: false,
            total_bytes: 0,
        };
        assert!(OutputEvent::from_completed(completed).is_none());
    }

    #[test]
    fn only_jsh_compatible_execution_ids_are_persisted() {
        for id in ["local:1", "jsh:1", "contains space", "雪"] {
            let completed = CompletedExecution {
                lifecycle: lifecycle(id),
                output: "output".to_owned(),
                output_available: true,
                truncated: false,
                total_bytes: 6,
            };
            assert!(OutputEvent::from_completed(completed).is_none(), "{id}");
        }

        let valid = CompletedExecution {
            lifecycle: lifecycle("jsh-a_b.c-1"),
            output: "output".to_owned(),
            output_available: true,
            truncated: false,
            total_bytes: 6,
        };
        assert!(OutputEvent::from_completed(valid).is_some());
    }

    #[test]
    fn jsh_execution_id_validation_matches_the_public_grammar() {
        for valid in [
            "jsh-a_b.c-1".to_string(),
            "x".repeat(MAX_EXECUTION_ID_BYTES),
        ] {
            assert!(is_valid_jsh_execution_id(&valid), "id={valid:?}");
        }
        for invalid in [
            String::new(),
            "x".repeat(MAX_EXECUTION_ID_BYTES + 1),
            "jsh:1".to_string(),
            "has space".to_string(),
            "line\nbreak".to_string(),
            "雪".to_string(),
        ] {
            assert!(!is_valid_jsh_execution_id(&invalid), "id={invalid:?}");
        }
    }

    #[test]
    fn output_event_matches_jsh_envelope() {
        let completed = CompletedExecution {
            lifecycle: lifecycle("exec-1"),
            output: "hi".to_owned(),
            output_available: true,
            truncated: false,
            total_bytes: 2,
        };
        let value =
            serde_json::to_value(OutputEvent::from_completed(completed).unwrap().event).unwrap();
        assert_eq!(value["jsh_execution_version"], 1);
        assert_eq!(value["event"], "output");
        assert_eq!(value["id"], "exec-1");
        assert_eq!(value["text"], "hi");
        assert_eq!(value["truncated"], false);
        assert_eq!(value["total_bytes"], 2);
        assert!(value.get("command").is_none());
    }

    #[test]
    fn output_metadata_cannot_deny_bytes_it_says_were_observed() {
        assert_eq!(normalize_output_metadata(2, false, 1), (false, 2));
        assert_eq!(normalize_output_metadata(2, false, 2), (false, 2));
        assert_eq!(normalize_output_metadata(2, false, 3), (true, 3));
        assert_eq!(normalize_output_metadata(2, true, 2), (true, 2));

        let event = OutputEvent::from_completed(CompletedExecution {
            lifecycle: lifecycle("exec-inconsistent"),
            output: "hi".to_owned(),
            output_available: true,
            truncated: false,
            total_bytes: 3,
        })
        .unwrap()
        .event;
        assert!(event.truncated);
        assert_eq!(event.total_bytes, 3);
    }

    #[test]
    fn oversized_output_is_bounded_before_it_enters_the_writer_queue() {
        let output = "界".repeat(MAX_OUTPUT_BYTES);
        let observed = output.len();
        let event = OutputEvent::from_completed(CompletedExecution {
            lifecycle: lifecycle("exec-large"),
            output,
            output_available: true,
            truncated: false,
            total_bytes: 0,
        })
        .unwrap()
        .event;
        assert!(event.text.len() <= MAX_OUTPUT_BYTES);
        assert!(event.truncated);
        assert_eq!(event.total_bytes, observed as u64);
    }

    #[test]
    fn control_heavy_output_stays_within_jshs_jsonl_limit() {
        let output = "\0".repeat(MAX_OUTPUT_BYTES);
        let total_bytes = output.len();
        let completed = CompletedExecution {
            lifecycle: lifecycle("jsh-control-heavy"),
            output,
            output_available: true,
            truncated: false,
            total_bytes,
        };
        let encoded = encode_event(OutputEvent::from_completed(completed).unwrap().event).unwrap();

        assert!(encoded.len() <= MAX_EVENT_LINE_BYTES + 1);
        assert_eq!(encoded.last(), Some(&b'\n'));
        let value: serde_json::Value =
            serde_json::from_slice(&encoded[..encoded.len() - 1]).unwrap();
        assert_eq!(value["truncated"], true);
        assert_eq!(value["total_bytes"], total_bytes as u64);
        assert!(value["text"].as_str().unwrap().len() <= MAX_OUTPUT_BYTES / 2);

        let retained_text = value["text"].as_str().unwrap().to_owned();
        let path = temporary_journal("control-heavy-round-trip");
        let mut journal = b"{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"jsh-control-heavy\",\"session_id\":\"wanted\",\"seq\":1,\"command\":\"true\",\"cwd\":\"/\",\"started_at_ms\":1}\n".to_vec();
        journal.extend_from_slice(&encoded);
        write_temporary_journal(&path, journal);
        let records = read_session_history_file(&path, "wanted").unwrap();
        let _ = fs::remove_file(&path);
        assert_eq!(records[0].output.as_ref().unwrap().text, retained_text);
    }

    #[test]
    fn bound_output_snapshot_tracks_the_exact_current_start_generation() {
        let expected_lifecycle = lifecycle("jsh-bound-output");
        let expected = bound_output(expected_lifecycle.clone(), "captured");
        let start = lifecycle_start_line(&expected_lifecycle);
        let inspect = |label: &str, bytes: &[u8], lifecycle: &ExecutionLifecycle| {
            let directory = TestDir::new(label);
            let path = directory.0.join("executions.jsonl");
            write_temporary_journal(&path, bytes);
            let mut file = open_existing_journal_for_append(&path).unwrap();
            let len = file.metadata().unwrap().len();
            inspect_bound_output_snapshot(
                &mut file,
                len,
                lifecycle,
                &expected.event,
                MAX_JOURNAL_EVENT_LINES,
            )
            .unwrap()
        };

        assert_eq!(
            inspect("matching-start", &start, &expected_lifecycle),
            BoundOutputSnapshot::Ready
        );

        let finish = b"{\"jsh_execution_version\":1,\"event\":\"finish\",\"id\":\"jsh-bound-output\",\"exit_code\":7,\"duration_ms\":2,\"cwd_after\":\"/after\",\"ended_at_ms\":3}\n";
        let mut finished = start.clone();
        finished.extend_from_slice(finish);
        assert_eq!(
            inspect("finish-before-output", &finished, &expected_lifecycle),
            BoundOutputSnapshot::Ready,
            "a late terminal snapshot may follow its own Finish"
        );

        let replacement = ExecutionLifecycle {
            id: expected_lifecycle.id.clone(),
            session_id: "new-session".to_owned(),
            seq: 2,
            started_at_ms: 9,
        };
        let mut reset = finished.clone();
        reset.extend_from_slice(&lifecycle_start_line(&replacement));
        assert_eq!(
            inspect("restart-stale", &reset, &expected_lifecycle),
            BoundOutputSnapshot::Rejected,
            "old PTY bytes must not bind after a same-ID reset"
        );

        let replacement_expected = bound_output(replacement.clone(), "captured");
        let directory = TestDir::new("restart-current");
        let path = directory.0.join("executions.jsonl");
        write_temporary_journal(&path, &reset);
        let mut file = open_existing_journal_for_append(&path).unwrap();
        let len = file.metadata().unwrap().len();
        assert_eq!(
            inspect_bound_output_snapshot(
                &mut file,
                len,
                &replacement,
                &replacement_expected.event,
                MAX_JOURNAL_EVENT_LINES,
            )
            .unwrap(),
            BoundOutputSnapshot::Ready
        );

        for (label, mut conflicting_start) in [
            ("session", expected_lifecycle.clone()),
            ("sequence", expected_lifecycle.clone()),
            ("generation", expected_lifecycle.clone()),
        ] {
            match label {
                "session" => conflicting_start.session_id = "other".to_owned(),
                "sequence" => conflicting_start.seq += 1,
                "generation" => conflicting_start.started_at_ms += 1,
                _ => unreachable!(),
            }
            assert_eq!(
                inspect(
                    label,
                    &lifecycle_start_line(&conflicting_start),
                    &expected_lifecycle
                ),
                BoundOutputSnapshot::Rejected,
                "mismatched {label}"
            );
        }

        let mut malformed_barrier = start;
        malformed_barrier.extend_from_slice(
            b"{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"jsh-bound-output\",\"extra\":true}\n",
        );
        assert_eq!(
            inspect(
                "malformed-same-id-barrier",
                &malformed_barrier,
                &expected_lifecycle
            ),
            BoundOutputSnapshot::Rejected
        );
    }

    #[test]
    fn output_recovery_requires_one_complete_exact_physical_tail() {
        let lifecycle = lifecycle("jsh-output-recovery");
        let (expected, encoded) = encoded_bound_output(lifecycle.clone(), "captured");
        let start = lifecycle_start_line(&lifecycle);
        let inspect = |label: &str, bytes: &[u8]| {
            let directory = TestDir::new(label);
            let path = directory.0.join("executions.jsonl");
            write_temporary_journal(&path, bytes);
            let mut file = open_existing_journal_for_append(&path).unwrap();
            let len = file.metadata().unwrap().len();
            inspect_bound_output_snapshot(
                &mut file,
                len,
                &lifecycle,
                &expected.event,
                MAX_JOURNAL_EVENT_LINES,
            )
            .unwrap()
        };

        let mut exact = start.clone();
        exact.extend_from_slice(&encoded);
        assert_eq!(
            inspect("exact-terminal-output", &exact),
            BoundOutputSnapshot::ExactTerminal
        );

        let mut duplicate = exact.clone();
        duplicate.extend_from_slice(&encoded);
        assert_eq!(
            inspect("duplicate-output", &duplicate),
            BoundOutputSnapshot::Rejected
        );

        let (_, different) = encoded_bound_output(lifecycle.clone(), "different");
        let mut conflict = start.clone();
        conflict.extend_from_slice(&different);
        conflict.extend_from_slice(&encoded);
        assert_eq!(
            inspect("conflicting-output", &conflict),
            BoundOutputSnapshot::Rejected
        );

        let mut successor = exact.clone();
        successor.extend_from_slice(
            b"{\"jsh_execution_version\":1,\"event\":\"finish\",\"id\":\"jsh-output-recovery\",\"exit_code\":0,\"duration_ms\":1,\"cwd_after\":\"/tmp\",\"ended_at_ms\":2}\n",
        );
        assert_eq!(
            inspect("successor-after-output", &successor),
            BoundOutputSnapshot::Rejected
        );

        let mut torn = start;
        torn.extend_from_slice(&encoded[..encoded.len() / 2]);
        assert_eq!(
            inspect("torn-output", &torn),
            BoundOutputSnapshot::Rejected,
            "a partial Output is never a safe continuation boundary"
        );
    }

    #[test]
    fn bound_output_append_recovers_barriers_without_rewriting() {
        let directory = TestDir::new("bound-output-barriers");
        let path = directory.0.join("executions.jsonl");
        let lifecycle = lifecycle("jsh-bound-barrier");
        let start = lifecycle_start_line(&lifecycle);
        write_temporary_journal(&path, &start);
        let (output, encoded) = encoded_bound_output(lifecycle.clone(), "captured");
        let data_fault = FaultingAppendIo::new(AppendFaultStage::DataSync);

        append_encoded_event_to_path_with_line_limit_and_io_inner(
            &path,
            &encoded,
            MAX_JOURNAL_EVENT_LINES,
            &data_fault,
            Some(&output),
        )
        .unwrap();
        assert_eq!(data_fault.data_sync_calls.get(), 1);
        assert_eq!(data_fault.directory_sync_calls.get(), 0);
        let mut expected_bytes = start;
        expected_bytes.extend_from_slice(&encoded);
        assert_eq!(fs::read(&path).unwrap(), expected_bytes);
        assert_eq!(
            fs::read_to_string(&path).unwrap().lines().count(),
            2,
            "barrier recovery must not append Output twice"
        );
        let records = read_session_history_file(&path, "wanted").unwrap();
        assert_eq!(records[0].output.as_ref().unwrap().text, "captured");
    }

    #[test]
    fn bound_output_append_rejects_stale_and_torn_state_before_writing() {
        let directory = TestDir::new("bound-output-rejections");
        let path = directory.0.join("executions.jsonl");
        let stale = lifecycle("jsh-stale-output");
        let current = ExecutionLifecycle {
            id: stale.id.clone(),
            session_id: "new-session".to_owned(),
            seq: 2,
            started_at_ms: 2,
        };
        let current_start = lifecycle_start_line(&current);
        write_temporary_journal(&path, &current_start);
        let (stale_output, stale_encoded) = encoded_bound_output(stale, "old bytes");
        let no_write = FaultingAppendIo::new(AppendFaultStage::DataSync);
        let error = append_encoded_event_to_path_with_line_limit_and_io_inner(
            &path,
            &stale_encoded,
            MAX_JOURNAL_EVENT_LINES,
            &no_write,
            Some(&stale_output),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(no_write.write_calls.get(), 0);
        assert_eq!(fs::read(&path).unwrap(), current_start);

        let (current_output, current_encoded) = encoded_bound_output(current, "new bytes");
        let mut torn_source = current_start;
        torn_source.extend_from_slice(&current_encoded[..17]);
        write_temporary_journal(&path, &torn_source);
        let error = append_encoded_event_to_path_with_line_limit_and_io_inner(
            &path,
            &current_encoded,
            MAX_JOURNAL_EVENT_LINES,
            &no_write,
            Some(&current_output),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(no_write.write_calls.get(), 0);
        assert_eq!(fs::read(&path).unwrap(), torn_source);
    }

    #[test]
    fn partial_bound_output_stays_unknown_and_is_never_retried() {
        let directory = TestDir::new("bound-output-partial");
        let path = directory.0.join("executions.jsonl");
        let lifecycle = lifecycle("jsh-partial-output");
        let start = lifecycle_start_line(&lifecycle);
        write_temporary_journal(&path, &start);
        let (output, encoded) = encoded_bound_output(lifecycle, "captured");
        let zero = FaultingAppendIo::new(AppendFaultStage::WriteAfter(0));
        let error = append_encoded_event_to_path_with_line_limit_and_io_inner(
            &path,
            &encoded,
            MAX_JOURNAL_EVENT_LINES,
            &zero,
            Some(&output),
        )
        .unwrap_err();
        assert!(!is_commit_state_unknown(&error));
        assert_eq!(zero.written.get(), 0);
        assert_eq!(zero.data_sync_calls.get(), 0);
        assert_eq!(fs::read(&path).unwrap(), start);

        let partial = FaultingAppendIo::new(AppendFaultStage::WriteAfter(19));
        let error = append_encoded_event_to_path_with_line_limit_and_io_inner(
            &path,
            &encoded,
            MAX_JOURNAL_EVENT_LINES,
            &partial,
            Some(&output),
        )
        .unwrap_err();
        assert!(is_commit_state_unknown(&error));
        assert_eq!(partial.written.get(), 19);
        assert_eq!(partial.data_sync_calls.get(), 0);
        let bytes = fs::read(&path).unwrap();
        assert_eq!(&bytes[..start.len()], start.as_slice());
        assert!(!bytes.ends_with(b"\n"));
        assert_eq!(bytes.len(), start.len() + 19);
    }

    #[test]
    fn raw_line_budget_precedes_start_barrier_semantics() {
        let path = temporary_journal("raw-line-before-barrier");
        let mut journal = concat!(
            "{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"raw-exact\",\"session_id\":\"wanted\",\"seq\":1,\"command\":\"old\",\"cwd\":\"/old\",\"started_at_ms\":1}\n",
            "{\"jsh_execution_version\":1,\"event\":\"finish\",\"id\":\"raw-exact\",\"exit_code\":9,\"duration_ms\":1,\"cwd_after\":\"/old\",\"ended_at_ms\":2}\n",
            "{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"raw-over\",\"session_id\":\"wanted\",\"seq\":2,\"command\":\"old\",\"cwd\":\"/old\",\"started_at_ms\":2}\n",
            "{\"jsh_execution_version\":1,\"event\":\"finish\",\"id\":\"raw-over\",\"exit_code\":9,\"duration_ms\":1,\"cwd_after\":\"/old\",\"ended_at_ms\":3}\n"
        )
        .as_bytes()
        .to_vec();
        journal.extend_from_slice(&start_event_with_raw_bytes(
            "raw-exact",
            MAX_EVENT_LINE_BYTES,
        ));
        journal.push(b'\n');
        journal.extend_from_slice(&start_event_with_raw_bytes(
            "raw-over",
            MAX_EVENT_LINE_BYTES + 1,
        ));
        journal.push(b'\n');
        write_temporary_journal(&path, journal);

        let records = read_session_history_file(&path, "wanted").unwrap();
        let _ = fs::remove_file(&path);

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].id, "raw-over");
        assert_eq!(records[0].command, "old");
        assert_eq!(records[0].exit_code, Some(9));
    }

    #[test]
    fn decoded_utf8_budget_is_charged_after_json_unescaping() {
        let path = temporary_journal("decoded-utf8-budget");
        let mut journal = concat!(
            "{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"decoded-exact\",\"session_id\":\"wanted\",\"seq\":1,\"command\":\"true\",\"cwd\":\"/\",\"started_at_ms\":1}\n",
            "{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"decoded-over\",\"session_id\":\"wanted\",\"seq\":2,\"command\":\"true\",\"cwd\":\"/\",\"started_at_ms\":2}\n"
        )
        .as_bytes()
        .to_vec();
        journal.extend_from_slice(&escaped_multibyte_output_event(
            "decoded-exact",
            MAX_OUTPUT_BYTES,
        ));
        journal.push(b'\n');
        journal.extend_from_slice(&escaped_multibyte_output_event(
            "decoded-over",
            MAX_OUTPUT_BYTES + 1,
        ));
        journal.push(b'\n');
        write_temporary_journal(&path, journal);

        let records = read_session_history_file(&path, "wanted").unwrap();
        let _ = fs::remove_file(&path);

        assert_eq!(records.len(), 2);
        assert_eq!(records[0].id, "decoded-exact");
        assert_eq!(
            records[0].output.as_ref().unwrap().text.len(),
            MAX_OUTPUT_BYTES
        );
        assert_eq!(records[1].id, "decoded-over");
        assert_eq!(records[1].output, None);
    }

    #[test]
    fn history_reader_folds_only_the_requested_session() {
        let path = temporary_journal("fold-session");
        let journal = concat!(
            "not json\n",
            "{\"jsh_execution_version\":99,\"event\":\"start\",\"id\":\"future\",\"session_id\":\"wanted\",\"seq\":0,\"command\":\"future\",\"cwd\":\"/\",\"started_at_ms\":0}\n",
            "{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"other-1\",\"session_id\":\"other\",\"seq\":1,\"command\":\"ignore\",\"cwd\":\"/other\",\"started_at_ms\":1}\n",
            "{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"wanted-1\",\"session_id\":\"wanted\",\"seq\":7,\"command\":\"printf hi\",\"command_truncated\":false,\"cwd\":\"/tmp\",\"started_at_ms\":10}\n",
            "{\"jsh_execution_version\":1,\"event\":\"output\",\"id\":\"wanted-1\",\"text\":\"hi\",\"truncated\":false,\"total_bytes\":1,\"captured_at_ms\":12}\n",
            "{\"jsh_execution_version\":1,\"event\":\"finish\",\"id\":\"wanted-1\",\"exit_code\":3,\"duration_ms\":2,\"cwd_after\":\"/tmp/after\",\"ended_at_ms\":12}\n"
        );
        write_temporary_journal(&path, journal);

        let records = read_session_history_file(&path, "wanted").unwrap();
        let _ = fs::remove_file(&path);

        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.id, "wanted-1");
        assert_eq!(record.seq, 7);
        assert_eq!(record.command, "printf hi");
        assert_eq!(record.exit_code, Some(3));
        assert_eq!(record.duration_ms, Some(2));
        assert_eq!(record.cwd_after.as_deref(), Some("/tmp/after"));
        let output = record.output.as_ref().unwrap();
        assert_eq!(output.text, "hi");
        assert!(!output.truncated);
        assert_eq!(output.total_bytes, 2);
    }

    #[test]
    fn history_reader_marks_a_contradictory_output_as_truncated() {
        let path = temporary_journal("contradictory-output");
        let journal = concat!(
            "{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"wanted-1\",\"session_id\":\"wanted\",\"seq\":1,\"command\":\"true\",\"cwd\":\"/tmp\",\"started_at_ms\":1}\n",
            "{\"jsh_execution_version\":1,\"event\":\"output\",\"id\":\"wanted-1\",\"text\":\"hi\",\"truncated\":false,\"total_bytes\":3,\"captured_at_ms\":2}\n"
        );
        write_temporary_journal(&path, journal);

        let records = read_session_history_file(&path, "wanted").unwrap();
        let _ = fs::remove_file(&path);

        let output = records[0].output.as_ref().unwrap();
        assert!(output.truncated);
        assert_eq!(output.total_bytes, 3);
    }

    #[test]
    fn history_reader_does_not_last_win_conflicting_lifecycle_slots() {
        let path = temporary_journal("conflicting-lifecycle-slots");
        let journal = concat!(
            "{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"wanted-1\",\"session_id\":\"wanted\",\"seq\":1,\"command\":\"first\",\"cwd\":\"/tmp\",\"started_at_ms\":1}\n",
            "{\"jsh_execution_version\":1,\"event\":\"finish\",\"id\":\"wanted-1\",\"exit_code\":0,\"duration_ms\":2,\"cwd_after\":\"/tmp\",\"ended_at_ms\":3}\n",
            "{\"jsh_execution_version\":1,\"event\":\"finish\",\"id\":\"wanted-1\",\"exit_code\":0,\"duration_ms\":2,\"cwd_after\":\"/tmp\",\"ended_at_ms\":3}\n",
            "{\"jsh_execution_version\":1,\"event\":\"finish\",\"id\":\"wanted-1\",\"exit_code\":9,\"duration_ms\":8,\"cwd_after\":\"/other\",\"ended_at_ms\":9}\n",
            "{\"jsh_execution_version\":1,\"event\":\"finish\",\"id\":\"wanted-1\",\"exit_code\":0,\"duration_ms\":2,\"cwd_after\":\"/tmp\",\"ended_at_ms\":3}\n",
            "{\"jsh_execution_version\":1,\"event\":\"output\",\"id\":\"wanted-1\",\"text\":\"first\",\"truncated\":false,\"total_bytes\":5,\"captured_at_ms\":4}\n",
            "{\"jsh_execution_version\":1,\"event\":\"output\",\"id\":\"wanted-1\",\"text\":\"first\",\"truncated\":false,\"total_bytes\":5,\"captured_at_ms\":4}\n",
            "{\"jsh_execution_version\":1,\"event\":\"output\",\"id\":\"wanted-1\",\"text\":\"second\",\"truncated\":false,\"total_bytes\":6,\"captured_at_ms\":10}\n",
            "{\"jsh_execution_version\":1,\"event\":\"output\",\"id\":\"wanted-1\",\"text\":\"first\",\"truncated\":false,\"total_bytes\":5,\"captured_at_ms\":4}\n",
            "{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"wanted-2\",\"session_id\":\"wanted\",\"seq\":2,\"command\":\"stale\",\"cwd\":\"/tmp\",\"started_at_ms\":11}\n",
            "{\"jsh_execution_version\":1,\"event\":\"finish\",\"id\":\"wanted-2\",\"exit_code\":1,\"duration_ms\":1,\"cwd_after\":\"/tmp\",\"ended_at_ms\":12}\n",
            "{\"jsh_execution_version\":1,\"event\":\"finish\",\"id\":\"wanted-2\",\"exit_code\":2,\"duration_ms\":2,\"cwd_after\":\"/other\",\"ended_at_ms\":13}\n",
            "{\"jsh_execution_version\":1,\"event\":\"output\",\"id\":\"wanted-2\",\"text\":\"stale-a\",\"truncated\":false,\"total_bytes\":7,\"captured_at_ms\":14}\n",
            "{\"jsh_execution_version\":1,\"event\":\"output\",\"id\":\"wanted-2\",\"text\":\"stale-b\",\"truncated\":false,\"total_bytes\":7,\"captured_at_ms\":15}\n",
            "{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"wanted-2\",\"session_id\":\"wanted\",\"seq\":3,\"command\":\"fresh\",\"cwd\":\"/tmp\",\"started_at_ms\":20}\n",
            "{\"jsh_execution_version\":1,\"event\":\"finish\",\"id\":\"wanted-2\",\"exit_code\":7,\"duration_ms\":2,\"cwd_after\":\"/after\",\"ended_at_ms\":22}\n",
            "{\"jsh_execution_version\":1,\"event\":\"output\",\"id\":\"wanted-2\",\"text\":\"ok\",\"truncated\":false,\"total_bytes\":2,\"captured_at_ms\":23}\n"
        );
        write_temporary_journal(&path, journal);

        let records = read_session_history_file(&path, "wanted").unwrap();
        let _ = fs::remove_file(&path);

        assert_eq!(records.len(), 2);
        let ambiguous = &records[0];
        assert_eq!(ambiguous.id, "wanted-1");
        assert_eq!(ambiguous.exit_code, None);
        assert_eq!(ambiguous.duration_ms, None);
        assert_eq!(ambiguous.cwd_after, None);
        assert_eq!(ambiguous.ended_at_ms, None);
        assert_eq!(ambiguous.output, None);

        let exact = &records[1];
        assert_eq!(exact.seq, 3);
        assert_eq!(exact.command, "fresh");
        assert_eq!(exact.exit_code, Some(7));
        assert_eq!(exact.duration_ms, Some(2));
        assert_eq!(exact.cwd_after.as_deref(), Some("/after"));
        assert_eq!(exact.ended_at_ms, Some(22));
        assert_eq!(
            exact.output.as_ref().map(|output| output.text.as_str()),
            Some("ok")
        );
    }

    #[test]
    fn history_reader_applies_strict_durable_conflict_tombstones() {
        let path = temporary_journal("conflict-tombstones");
        let journal = concat!(
            "{\"jsh_execution_version\":1,\"event\":\"conflict\",\"id\":\"orphan\",\"slot\":\"finish\"}\n",
            "{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"finish-poisoned\",\"session_id\":\"wanted\",\"seq\":1,\"command\":\"one\",\"cwd\":\"/tmp\",\"started_at_ms\":1}\n",
            "{\"jsh_execution_version\":1,\"event\":\"conflict\",\"id\":\"finish-poisoned\",\"slot\":\"finish\"}\n",
            "{\"jsh_execution_version\":1,\"event\":\"conflict\",\"id\":\"finish-poisoned\",\"slot\":\"finish\"}\n",
            "{\"jsh_execution_version\":1,\"event\":\"finish\",\"id\":\"finish-poisoned\",\"exit_code\":0,\"duration_ms\":2,\"cwd_after\":\"/after\",\"ended_at_ms\":3}\n",
            "{\"jsh_execution_version\":1,\"event\":\"output\",\"id\":\"finish-poisoned\",\"text\":\"kept\",\"truncated\":false,\"total_bytes\":4,\"captured_at_ms\":4}\n",
            "{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"output-poisoned\",\"session_id\":\"wanted\",\"seq\":2,\"command\":\"two\",\"cwd\":\"/tmp\",\"started_at_ms\":5}\n",
            "{\"jsh_execution_version\":1,\"event\":\"conflict\",\"id\":\"output-poisoned\",\"slot\":\"output\"}\n",
            "{\"jsh_execution_version\":1,\"event\":\"conflict\",\"id\":\"output-poisoned\",\"slot\":\"output\"}\n",
            "{\"jsh_execution_version\":1,\"event\":\"output\",\"id\":\"output-poisoned\",\"text\":\"dropped\",\"truncated\":false,\"total_bytes\":7,\"captured_at_ms\":6}\n",
            "{\"jsh_execution_version\":1,\"event\":\"finish\",\"id\":\"output-poisoned\",\"exit_code\":7,\"duration_ms\":2,\"cwd_after\":\"/after\",\"ended_at_ms\":7}\n",
            "{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"strict\",\"session_id\":\"wanted\",\"seq\":3,\"command\":\"three\",\"cwd\":\"/tmp\",\"started_at_ms\":8}\n",
            "{\"jsh_execution_version\":1,\"event\":\"conflict\",\"id\":\"strict\",\"slot\":\"finish\",\"extra\":true}\n",
            "{\"jsh_execution_version\":1,\"event\":\"conflict\",\"id\":\"strict\",\"slot\":\"unknown\"}\n",
            "{\"jsh_execution_version\":99,\"event\":\"conflict\",\"id\":\"strict\",\"slot\":\"finish\"}\n",
            "{\"jsh_execution_version\":1,\"event\":\"finish\",\"id\":\"strict\",\"exit_code\":9,\"duration_ms\":1,\"cwd_after\":\"/after\",\"ended_at_ms\":9}\n",
            "{\"jsh_execution_version\":1,\"event\":\"output\",\"id\":\"strict\",\"text\":\"exact\",\"truncated\":false,\"total_bytes\":5,\"captured_at_ms\":10}\n"
        );
        write_temporary_journal(&path, journal);

        let records = read_session_history_file(&path, "wanted").unwrap();
        let _ = fs::remove_file(&path);

        assert_eq!(records.len(), 3, "orphan tombstones create no lifecycle");
        assert_eq!(records[0].exit_code, None);
        assert_eq!(
            records[0]
                .output
                .as_ref()
                .map(|output| output.text.as_str()),
            Some("kept"),
            "finish poison does not affect output"
        );
        assert_eq!(records[1].exit_code, Some(7));
        assert_eq!(records[1].output, None, "output poison survives replay");
        assert_eq!(records[2].exit_code, Some(9));
        assert_eq!(
            records[2]
                .output
                .as_ref()
                .map(|output| output.text.as_str()),
            Some("exact"),
            "malformed, extra-field, and future-version tombstones are ignored"
        );
    }

    #[test]
    fn known_finish_and_output_reject_extra_identity_fields() {
        let path = temporary_journal("strict-known-event-fields");
        let journal = concat!(
            "{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"strict-known\",\"session_id\":\"wanted\",\"seq\":1,\"command\":\"true\",\"cwd\":\"/tmp\",\"started_at_ms\":1}\n",
            "{\"jsh_execution_version\":1,\"event\":\"vendor_extension\",\"id\":\"strict-known\",\"session_id\":\"other\",\"execution_id\":\"other\",\"exit_code\":90,\"text\":\"ignored\"}\n",
            "{\"jsh_execution_version\":1,\"event\":\"finish\",\"id\":\"strict-known\",\"session_id\":\"other\",\"exit_code\":91,\"duration_ms\":91,\"cwd_after\":\"/wrong\",\"ended_at_ms\":91}\n",
            "{\"jsh_execution_version\":1,\"event\":\"output\",\"id\":\"strict-known\",\"execution_id\":\"other\",\"text\":\"wrong\",\"truncated\":false,\"total_bytes\":5,\"captured_at_ms\":92}\n",
            "{\"jsh_execution_version\":1,\"event\":\"finish\",\"id\":\"strict-known\",\"exit_code\":7,\"duration_ms\":2,\"cwd_after\":\"/after\",\"ended_at_ms\":3}\n",
            "{\"jsh_execution_version\":1,\"event\":\"output\",\"id\":\"strict-known\",\"text\":\"exact\",\"truncated\":false,\"total_bytes\":5,\"captured_at_ms\":4}\n"
        );
        write_temporary_journal(&path, journal);

        let records = read_session_history_file(&path, "wanted").unwrap();
        let _ = fs::remove_file(&path);

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].exit_code, Some(7));
        assert_eq!(records[0].duration_ms, Some(2));
        assert_eq!(records[0].cwd_after.as_deref(), Some("/after"));
        assert_eq!(records[0].ended_at_ms, Some(3));
        assert_eq!(
            records[0]
                .output
                .as_ref()
                .map(|output| output.text.as_str()),
            Some("exact"),
            "unknown event kinds remain skippable while known variants are strict"
        );
    }

    #[test]
    fn later_start_resets_prior_slots_even_when_restart_metadata_goes_backwards() {
        let path = temporary_journal("restart-reset-order");
        let journal = concat!(
            "{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"restarted\",\"session_id\":\"wanted\",\"seq\":90,\"command\":\"old\",\"cwd\":\"/old\",\"started_at_ms\":900}\n",
            "{\"jsh_execution_version\":1,\"event\":\"finish\",\"id\":\"restarted\",\"exit_code\":9,\"duration_ms\":8,\"cwd_after\":\"/old-after\",\"ended_at_ms\":908}\n",
            "{\"jsh_execution_version\":1,\"event\":\"output\",\"id\":\"restarted\",\"text\":\"old output\",\"truncated\":false,\"total_bytes\":10,\"captured_at_ms\":909}\n",
            "{\"jsh_execution_version\":1,\"event\":\"conflict\",\"id\":\"restarted\",\"slot\":\"finish\"}\n",
            "{\"jsh_execution_version\":1,\"event\":\"conflict\",\"id\":\"restarted\",\"slot\":\"output\"}\n",
            "{\"jsh_execution_version\":2,\"event\":\"start\",\"id\":\"restarted\",\"session_id\":\"wanted\",\"seq\":0,\"command\":\"future\",\"cwd\":\"/future\",\"started_at_ms\":0}\n",
            "{\"jsh_execution_version\":1,\"event\":\"future\",\"id\":\"restarted\",\"payload\":true}\n",
            "{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"restarted\",\"session_id\":\"wanted\",\"seq\":1,\"command\":\"new\",\"cwd\":\"/new\",\"started_at_ms\":10}\n"
        );
        write_temporary_journal(&path, journal);

        let records = read_session_history_file(&path, "wanted").unwrap();
        let _ = fs::remove_file(&path);

        assert_eq!(records.len(), 1);
        let restarted = &records[0];
        assert_eq!(restarted.seq, 1);
        assert_eq!(restarted.started_at_ms, 10);
        assert_eq!(restarted.command, "new");
        assert_eq!(restarted.cwd, "/new");
        assert_eq!(restarted.exit_code, None);
        assert_eq!(restarted.duration_ms, None);
        assert_eq!(restarted.cwd_after, None);
        assert_eq!(restarted.ended_at_ms, None);
        assert_eq!(restarted.output, None);
    }

    #[test]
    fn legacy_v1_readers_ignore_additive_conflict_tombstones() {
        #[derive(Deserialize)]
        #[serde(tag = "event")]
        enum LegacyEvent {
            #[serde(rename = "start")]
            Start,
        }

        let start = r#"{"jsh_execution_version":1,"event":"start","id":"one"}"#;
        let conflict =
            r#"{"jsh_execution_version":1,"event":"conflict","id":"one","slot":"finish"}"#;
        assert!(matches!(
            serde_json::from_str::<LegacyEvent>(start),
            Ok(LegacyEvent::Start)
        ));
        assert!(serde_json::from_str::<LegacyEvent>(conflict).is_err());
    }

    #[test]
    fn recognized_start_ids_barrier_invalid_replacement_lifecycles() {
        let path = temporary_journal("invalid-start-barriers");
        let mut journal = Vec::new();
        let valid_start = |id: &str, seq: u64| {
            serde_json::json!({
                "jsh_execution_version": 1,
                "event": "start",
                "id": id,
                "session_id": "wanted",
                "seq": seq,
                "command": "old",
                "cwd": "/old",
                "started_at_ms": seq,
            })
        };
        let finish = |id: &str| {
            serde_json::json!({
                "jsh_execution_version": 1,
                "event": "finish",
                "id": id,
                "exit_code": 9,
                "duration_ms": 1,
                "cwd_after": "/after",
                "ended_at_ms": 99,
            })
        };

        let mut replacements = vec![
            (
                "bad-session",
                serde_json::json!({
                    "jsh_execution_version": 1,
                    "event": "start",
                    "id": "bad-session",
                    "session_id": "bad session",
                    "seq": 10,
                    "command": "new",
                    "cwd": "/new",
                    "started_at_ms": 10,
                }),
            ),
            (
                "bad-command",
                serde_json::json!({
                    "jsh_execution_version": 1,
                    "event": "start",
                    "id": "bad-command",
                    "session_id": "wanted",
                    "seq": 11,
                    "command": "hidden\u{202e}command",
                    "cwd": "/new",
                    "started_at_ms": 11,
                }),
            ),
            (
                "bad-cwd",
                serde_json::json!({
                    "jsh_execution_version": 1,
                    "event": "start",
                    "id": "bad-cwd",
                    "session_id": "wanted",
                    "seq": 12,
                    "command": "new",
                    "cwd": "",
                    "started_at_ms": 12,
                }),
            ),
            (
                "bad-type",
                serde_json::json!({
                    "jsh_execution_version": 1,
                    "event": "start",
                    "id": "bad-type",
                    "session_id": "wanted",
                    "seq": "13",
                    "command": "new",
                    "cwd": "/new",
                    "started_at_ms": 13,
                }),
            ),
            (
                "extra-field",
                serde_json::json!({
                    "jsh_execution_version": 1,
                    "event": "start",
                    "id": "extra-field",
                    "session_id": "wanted",
                    "seq": 14,
                    "command": "new",
                    "cwd": "/new",
                    "started_at_ms": 14,
                    "extra": true,
                }),
            ),
            (
                "legacy-barrier",
                serde_json::json!({
                    "rsh_execution_version": 1,
                    "event": "start",
                    "id": "legacy-barrier",
                    "session_id": "wanted",
                    "seq": 15,
                    "command": "new",
                    "cwd": "",
                    "started_at_ms": 15,
                }),
            ),
        ];
        replacements.push((
            "oversized-command",
            serde_json::json!({
                "jsh_execution_version": 1,
                "event": "start",
                "id": "oversized-command",
                "session_id": "wanted",
                "seq": 16,
                "command": "x".repeat(MAX_COMMAND_BYTES + 1),
                "cwd": "/new",
                "started_at_ms": 16,
            }),
        ));

        for (index, (id, replacement)) in replacements.iter().enumerate() {
            writeln!(journal, "{}", valid_start(id, index as u64 + 1)).unwrap();
            writeln!(journal, "{replacement}").unwrap();
            writeln!(journal, "{}", finish(id)).unwrap();
        }

        writeln!(journal, "{}", valid_start("escaped-id", 17)).unwrap();
        writeln!(journal, "{{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"escaped\\u002did\",\"session_id\":\"wanted\",\"seq\":17,\"command\":\"new\",\"cwd\":\"\",\"started_at_ms\":17}}").unwrap();
        writeln!(journal, "{}", finish("escaped-id")).unwrap();

        writeln!(journal, "{}", valid_start("survivor", 20)).unwrap();
        for ignored in [
            "{\"jsh_execution_version\":1,\"event\":\"start\",\"session_id\":\"wanted\",\"seq\":21,\"command\":\"missing id\",\"cwd\":\"/new\",\"started_at_ms\":21}",
            "{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":7,\"session_id\":\"wanted\",\"seq\":22,\"command\":\"wrong id type\",\"cwd\":\"/new\",\"started_at_ms\":22}",
            "{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"survivor \" ,\"session_id\":\"wanted\",\"seq\":22,\"command\":\"invalid id\",\"cwd\":\"/new\",\"started_at_ms\":22}",
            "{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"survivor\"",
            "{\"jsh_execution_version\":99,\"event\":\"start\",\"id\":\"survivor\",\"session_id\":\"wanted\",\"seq\":23,\"command\":\"future\",\"cwd\":\"/new\",\"started_at_ms\":23}",
            "{\"jsh_execution_version\":1,\"event\":\"future-start\",\"id\":\"survivor\",\"session_id\":\"wanted\",\"seq\":24,\"command\":\"extension\",\"cwd\":\"/new\",\"started_at_ms\":24}",
        ] {
            writeln!(journal, "{ignored}").unwrap();
        }
        writeln!(journal, "{}", finish("survivor")).unwrap();
        write_temporary_journal(&path, journal);

        let records = read_session_history_file(&path, "wanted").unwrap();
        let _ = fs::remove_file(&path);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].id, "survivor");
        assert_eq!(records[0].exit_code, Some(9));
    }

    #[test]
    fn duplicate_json_members_never_mutate_lifecycle_state() {
        let path = temporary_journal("duplicate-members");
        let journal = concat!(
            "{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"duplicate-start\",\"session_id\":\"wanted\",\"seq\":1,\"command\":\"old\",\"cwd\":\"/old\",\"started_at_ms\":1}\n",
            "{\"jsh_execution_version\":1,\"event\":\"finish\",\"id\":\"duplicate-start\",\"exit_code\":9,\"duration_ms\":1,\"cwd_after\":\"/old\",\"ended_at_ms\":2}\n",
            "{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"duplicate-start\",\"session_id\":\"wanted\",\"seq\":4,\"command\":\"new\",\"cwd\":\"/new\",\"started_at_ms\":4,\"extension\":1,\"\\u0065xtension\":2}\n",
            "{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"duplicate-finish\",\"session_id\":\"wanted\",\"seq\":2,\"command\":\"two\",\"cwd\":\"/\",\"started_at_ms\":2}\n",
            "{\"jsh_execution_version\":1,\"event\":\"finish\",\"id\":\"duplicate-finish\",\"exit_code\":7,\"duration_ms\":1,\"cwd_after\":\"/\",\"ended_at_ms\":3,\"extension\":1,\"\\u0065xtension\":2}\n",
            "{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"duplicate-output\",\"session_id\":\"wanted\",\"seq\":3,\"command\":\"three\",\"cwd\":\"/\",\"started_at_ms\":3}\n",
            "{\"jsh_execution_version\":1,\"event\":\"output\",\"id\":\"duplicate-output\",\"text\":\"wrong\",\"truncated\":false,\"total_bytes\":5,\"captured_at_ms\":4,\"extension\":{\"nested\":1,\"nested\":2}}\n"
        );
        write_temporary_journal(&path, journal);

        let records = read_session_history_file(&path, "wanted").unwrap();
        let _ = fs::remove_file(&path);

        assert_eq!(records.len(), 3);
        assert_eq!(records[0].id, "duplicate-start");
        assert_eq!(records[0].command, "old");
        assert_eq!(records[0].exit_code, Some(9));
        assert_eq!(records[1].id, "duplicate-finish");
        assert_eq!(records[1].exit_code, None);
        assert_eq!(records[2].id, "duplicate-output");
        assert_eq!(records[2].output, None);
    }

    #[test]
    fn ambiguous_envelopes_and_future_known_kinds_never_select_v1_state() {
        let path = temporary_journal("ambiguous-event-envelopes");
        let journal = concat!(
            "{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"stable\",\"session_id\":\"wanted\",\"seq\":1,\"command\":\"old\",\"cwd\":\"/old\",\"started_at_ms\":1}\n",
            "{\"jsh_execution_version\":1,\"event\":\"finish\",\"id\":\"stable\",\"exit_code\":9,\"duration_ms\":2,\"cwd_after\":\"/old-after\",\"ended_at_ms\":3}\n",
            "{\"jsh_execution_version\":1,\"event\":\"output\",\"id\":\"stable\",\"text\":\"old output\",\"truncated\":false,\"total_bytes\":10,\"captured_at_ms\":4}\n",
            "{\"jsh_execution_version\":1,\"event\":\"start\",\"\\u0065vent\":\"finish\",\"id\":\"stable\",\"session_id\":\"wanted\",\"seq\":10,\"command\":\"duplicate kind\",\"cwd\":\"/new\",\"started_at_ms\":10}\n",
            "{\"jsh_execution_version\":1,\"jsh_execution_\\u0076ersion\":1,\"event\":\"start\",\"id\":\"stable\",\"session_id\":\"wanted\",\"seq\":11,\"command\":\"duplicate version\",\"cwd\":\"/new\",\"started_at_ms\":11}\n",
            "{\"jsh_execution_version\":2,\"rsh_execution_version\":1,\"event\":\"start\",\"id\":\"stable\",\"session_id\":\"wanted\",\"seq\":12,\"command\":\"alias last v1\",\"cwd\":\"/new\",\"started_at_ms\":12}\n",
            "{\"rsh_execution_version\":1,\"jsh_execution_version\":2,\"event\":\"start\",\"id\":\"stable\",\"session_id\":\"wanted\",\"seq\":13,\"command\":\"alias first v1\",\"cwd\":\"/new\",\"started_at_ms\":13}\n",
            "{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"stable\",\"\\u0069d\":\"other\",\"session_id\":\"wanted\",\"seq\":14,\"command\":\"duplicate id\",\"cwd\":\"/new\",\"started_at_ms\":14}\n",
            "{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"stable\",\"session_id\":\"wanted\",\"\\u0073ession_id\":\"other\",\"seq\":15,\"command\":\"duplicate session\",\"cwd\":\"/new\",\"started_at_ms\":15}\n",
            "{\"jsh_execution_version\":1,\"event\":\"finish\",\"id\":\"other\",\"\\u0069d\":\"stable\",\"exit_code\":70,\"duration_ms\":70,\"cwd_after\":\"/wrong\",\"ended_at_ms\":70}\n",
            "{\"jsh_execution_version\":2,\"rsh_execution_version\":1,\"event\":\"finish\",\"id\":\"stable\",\"exit_code\":71,\"duration_ms\":71,\"cwd_after\":\"/wrong\",\"ended_at_ms\":71}\n",
            "{\"jsh_execution_version\":1,\"event\":\"output\",\"id\":\"other\",\"\\u0069d\":\"stable\",\"text\":\"duplicate id output\",\"truncated\":false,\"total_bytes\":19,\"captured_at_ms\":72}\n",
            "{\"jsh_execution_version\":2,\"event\":\"start\",\"id\":\"stable\",\"session_id\":\"wanted\",\"seq\":20,\"command\":\"future start\",\"cwd\":\"/future\",\"started_at_ms\":20}\n",
            "{\"jsh_execution_version\":2,\"event\":\"finish\",\"id\":\"stable\",\"exit_code\":72,\"duration_ms\":72,\"cwd_after\":\"/future\",\"ended_at_ms\":72}\n",
            "{\"jsh_execution_version\":2,\"event\":\"output\",\"id\":\"stable\",\"text\":\"future output\",\"truncated\":false,\"total_bytes\":13,\"captured_at_ms\":73}\n"
        );
        write_temporary_journal(&path, journal);

        let records = read_session_history_file(&path, "wanted").unwrap();
        let _ = fs::remove_file(&path);

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].id, "stable");
        assert_eq!(records[0].seq, 1);
        assert_eq!(records[0].command, "old");
        assert_eq!(records[0].cwd, "/old");
        assert_eq!(records[0].exit_code, Some(9));
        assert_eq!(records[0].duration_ms, Some(2));
        assert_eq!(records[0].cwd_after.as_deref(), Some("/old-after"));
        assert_eq!(
            records[0]
                .output
                .as_ref()
                .map(|output| output.text.as_str()),
            Some("old output")
        );
    }

    #[test]
    fn unknown_additive_events_never_accumulate_state_at_raw_boundaries() {
        let path = temporary_journal("unknown-additive-boundaries");
        let mut journal = b"{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"known\",\"session_id\":\"wanted\",\"seq\":1,\"command\":\"old\",\"cwd\":\"/old\",\"started_at_ms\":1}\n".to_vec();
        journal.extend_from_slice(&unknown_event_with_raw_bytes("known", MAX_EVENT_LINE_BYTES));
        journal.push(b'\n');
        journal.extend_from_slice(&unknown_event_with_raw_bytes(
            "orphan-over",
            MAX_EVENT_LINE_BYTES + 1,
        ));
        journal.extend_from_slice(b"\n{\"jsh_execution_version\":1,\"event\":\"finish\",\"id\":\"known\",\"exit_code\":9,\"duration_ms\":1,\"cwd_after\":\"/old\",\"ended_at_ms\":2}\n");
        write_temporary_journal(&path, journal);

        let records = read_session_history_file(&path, "wanted").unwrap();
        let _ = fs::remove_file(&path);

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].id, "known");
        assert_eq!(records[0].command, "old");
        assert_eq!(records[0].exit_code, Some(9));
    }

    #[test]
    fn history_reader_preserves_structural_multiline_but_drops_ambiguous_replay_text() {
        let path = temporary_journal("unsafe-display-text");
        let journal = concat!(
            "{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"bad-command\",\"session_id\":\"wanted\",\"seq\":1,\"command\":\"echo\\nrun\",\"cwd\":\"/tmp\",\"started_at_ms\":1}\n",
            "{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"bad-cwd\",\"session_id\":\"wanted\",\"seq\":2,\"command\":\"true\",\"cwd\":\"/tmp/SPOOFhidden\",\"started_at_ms\":2}\n",
            "{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"safe\",\"session_id\":\"wanted\",\"seq\":3,\"command\":\"printf hi\",\"cwd\":\"/tmp/a b\",\"started_at_ms\":3}\n",
            "{\"jsh_execution_version\":1,\"event\":\"finish\",\"id\":\"safe\",\"exit_code\":0,\"duration_ms\":1,\"cwd_after\":\"/tmp/SPACEhidden\",\"ended_at_ms\":4}\n",
            "{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"jsh-hidden-command\",\"session_id\":\"wanted\",\"seq\":4,\"command\":\"echoANCHORhidden\",\"cwd\":\"/tmp\",\"started_at_ms\":5}\n",
            "{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"jsh-hidden-cwd\",\"session_id\":\"wanted\",\"seq\":5,\"command\":\"true\",\"cwd\":\"/tmp/ANCHORhidden\",\"started_at_ms\":6}\n"
        )
        .replace("SPOOF", "\u{202e}")
        .replace("SPACE", "\u{00a0}")
        .replace("ANCHOR", "\u{fff9}");
        write_temporary_journal(&path, journal);

        let records = read_session_history_file(&path, "wanted").unwrap();
        let _ = fs::remove_file(&path);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].id, "bad-command");
        assert_eq!(records[0].command, "echo\nrun");
        assert_eq!(records[1].id, "safe");
        assert_eq!(records[1].cwd_after, None);
    }

    /// Twin fixture for jsh's serializer. It covers the complete lifecycle,
    /// the pre-rename version alias jsh migrates in place, and a structural
    /// multiline command that older core readers incorrectly discarded.
    #[test]
    fn history_reader_accepts_jsh_v1_and_legacy_lifecycle_fixtures() {
        let path = temporary_journal("jsh-golden-lifecycle");
        let journal = concat!(
            "{\"rsh_execution_version\":1,\"event\":\"start\",\"id\":\"legacy-1\",\"session_id\":\"wanted\",\"seq\":1,\"command\":\"printf one\\nprintf two\",\"command_truncated\":false,\"cwd\":\"/tmp\",\"started_at_ms\":10}\n",
            "{\"rsh_execution_version\":1,\"event\":\"finish\",\"id\":\"legacy-1\",\"exit_code\":7,\"duration_ms\":3,\"cwd_after\":\"/tmp\",\"ended_at_ms\":13}\n",
            "{\"jsh_execution_version\":1,\"event\":\"output\",\"id\":\"legacy-1\",\"text\":\"one\\ntwo\",\"truncated\":false,\"total_bytes\":7,\"captured_at_ms\":14}\n"
        );
        write_temporary_journal(&path, journal);

        let records = read_session_history_file(&path, "wanted").unwrap();
        let _ = fs::remove_file(&path);
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.command, "printf one\nprintf two");
        assert_eq!(record.exit_code, Some(7));
        assert_eq!(
            record.output.as_ref().map(|output| output.text.as_str()),
            Some("one\ntwo")
        );
    }

    #[test]
    fn public_journal_contract_values_match_jsh_v1() {
        assert_eq!(EXECUTION_JOURNAL_VERSION, 1);
        assert_eq!(MAX_EVENT_LINE_BYTES, 1024 * 1024);
        assert_eq!(MAX_EXECUTION_ID_BYTES, 192);
        assert_eq!(MAX_COMMAND_BYTES, 64 * 1024);
        assert_eq!(MAX_CWD_BYTES, 4 * 1024);
        assert_eq!(MAX_OUTPUT_BYTES, 256 * 1024);
        assert_eq!(MAX_JOURNAL_FILE_BYTES, 32 * 1024 * 1024);
        assert_eq!(MAX_JOURNAL_EVENT_LINES, 512 * 1024);
        assert_eq!(MAX_RETAINED_EXECUTIONS, 2_000);
        assert_eq!(MAX_JSH_SESSION_ID_BYTES, 128);

        assert!(is_valid_jsh_cwd("/home/u/my project/雪"));
        assert!(is_valid_jsh_cwd(&"x".repeat(MAX_CWD_BYTES)));
        for invalid in [
            String::new(),
            "x".repeat(MAX_CWD_BYTES + 1),
            "/tmp/line\nbreak".to_string(),
            "/tmp/left\u{202e}right".to_string(),
        ] {
            assert!(!is_valid_jsh_cwd(&invalid), "cwd={invalid:?}");
        }
    }

    #[test]
    fn history_reader_discards_an_oversized_line_and_resumes() {
        let path = temporary_journal("oversized-line");
        let valid = b"{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"after-large\",\"session_id\":\"wanted\",\"seq\":1,\"command\":\"true\",\"cwd\":\"/\",\"started_at_ms\":1}\n";
        let mut journal = vec![b'x'; MAX_EVENT_LINE_BYTES + 2];
        journal.push(b'\n');
        journal.extend_from_slice(valid);
        write_temporary_journal(&path, journal);

        let records = read_session_history_file(&path, "wanted").unwrap();
        let _ = fs::remove_file(&path);

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].id, "after-large");
    }

    #[test]
    fn history_reader_charges_unknown_events_to_the_physical_line_limit() {
        let path = temporary_journal("event-count-limit");
        let accepted = concat!(
            "{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"bounded\",\"session_id\":\"wanted\",\"seq\":1,\"command\":\"true\",\"cwd\":\"/\",\"started_at_ms\":1}\n",
            "{\"jsh_execution_version\":2,\"event\":\"start\",\"id\":\"bounded\",\"session_id\":\"wanted\",\"seq\":2,\"command\":\"future\",\"cwd\":\"/future\",\"started_at_ms\":2}\n",
            "{\"jsh_execution_version\":1,\"event\":\"future\",\"id\":\"bounded\",\"payload\":true}\n",
            "{\"jsh_execution_version\":1,\"event\":\"finish\",\"id\":\"bounded\",\"exit_code\":0,\"duration_ms\":1,\"cwd_after\":\"/\",\"ended_at_ms\":2}\n"
        );
        write_temporary_journal(&path, accepted);
        let records = read_session_history_file_with_line_limit(&path, "wanted", 4).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].command, "true");
        assert_eq!(records[0].exit_code, Some(0));

        let over_limit = format!(
            "{accepted}{{\"jsh_execution_version\":2,\"event\":\"future\",\"id\":\"orphan\"}}\n"
        );
        write_temporary_journal(&path, over_limit);
        let error = read_session_history_file_with_line_limit(&path, "wanted", 4).unwrap_err();
        let _ = fs::remove_file(&path);

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("event-count limit"));
    }

    #[test]
    fn event_line_limit_counts_only_a_real_newline_as_framing() {
        for terminated in [false, true] {
            let mut bytes = vec![b'x'; MAX_EVENT_LINE_BYTES];
            if terminated {
                bytes.push(b'\n');
            }
            let mut reader = bytes.as_slice();
            let mut line = Vec::new();
            assert_eq!(
                read_bounded_line(&mut reader, &mut line).unwrap(),
                Some(true),
                "an event exactly at the payload limit must be retained"
            );
            assert_eq!(line, bytes);
        }

        for terminated in [false, true] {
            let mut bytes = vec![b'x'; MAX_EVENT_LINE_BYTES + 1];
            if terminated {
                bytes.push(b'\n');
            }
            let mut reader = bytes.as_slice();
            let mut line = Vec::new();
            assert_eq!(
                read_bounded_line(&mut reader, &mut line).unwrap(),
                Some(false),
                "one byte beyond the payload limit must be discarded"
            );
            assert!(line.is_empty());
        }
    }

    #[test]
    fn history_reader_keeps_only_the_latest_bounded_records() {
        let path = temporary_journal("record-limit");
        let mut journal = Vec::new();
        for seq in 0..=MAX_RETAINED_EXECUTIONS {
            writeln!(
                journal,
                "{{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"exec-{seq}\",\"session_id\":\"wanted\",\"seq\":{seq},\"command\":\"true\",\"cwd\":\"/\",\"started_at_ms\":{seq}}}"
            )
            .unwrap();
        }
        write_temporary_journal(&path, journal);

        let records = read_session_history_file(&path, "wanted").unwrap();
        let _ = fs::remove_file(&path);

        assert_eq!(records.len(), MAX_RETAINED_EXECUTIONS);
        assert_eq!(records.first().unwrap().id, "exec-1");
        assert_eq!(
            records.last().unwrap().id,
            format!("exec-{MAX_RETAINED_EXECUTIONS}")
        );
    }

    #[test]
    fn session_history_uses_physical_start_order_across_clock_reset() {
        let path = temporary_journal("physical-history-order");
        write_temporary_journal(
            &path,
            concat!(
                "{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"older\",\"session_id\":\"wanted\",\"seq\":99,\"command\":\"old\",\"cwd\":\"/\",\"started_at_ms\":999}\n",
                "{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"newer\",\"session_id\":\"wanted\",\"seq\":1,\"command\":\"new\",\"cwd\":\"/\",\"started_at_ms\":1}\n",
                "{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"newest\",\"session_id\":\"wanted\",\"seq\":1,\"command\":\"newest\",\"cwd\":\"/\",\"started_at_ms\":1}\n"
            ),
        );

        let records = read_session_history_file(&path, "wanted").unwrap();
        let _ = fs::remove_file(&path);

        assert_eq!(
            records
                .iter()
                .map(|record| record.id.as_str())
                .collect::<Vec<_>>(),
            ["older", "newer", "newest"]
        );
    }

    #[test]
    fn history_retention_uses_physical_start_order_across_restart() {
        let path = temporary_journal("physical-record-order");
        let mut journal = Vec::new();
        writeln!(
            journal,
            "{{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"restarted\",\"session_id\":\"wanted\",\"seq\":{},\"command\":\"old\",\"cwd\":\"/\",\"started_at_ms\":{}}}",
            u64::MAX,
            u64::MAX
        )
        .unwrap();
        for ordinal in 1..MAX_RETAINED_EXECUTIONS {
            writeln!(
                journal,
                "{{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"other-{ordinal}\",\"session_id\":\"wanted\",\"seq\":{ordinal},\"command\":\"other\",\"cwd\":\"/\",\"started_at_ms\":{ordinal}}}"
            )
            .unwrap();
        }
        writeln!(
            journal,
            "{{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"restarted\",\"session_id\":\"wanted\",\"seq\":0,\"command\":\"new\",\"cwd\":\"/\",\"started_at_ms\":0}}"
        )
        .unwrap();
        writeln!(
            journal,
            "{{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"zz-newest\",\"session_id\":\"wanted\",\"seq\":0,\"command\":\"newest\",\"cwd\":\"/\",\"started_at_ms\":0}}"
        )
        .unwrap();
        write_temporary_journal(&path, journal);

        let records = read_session_history_file(&path, "wanted").unwrap();
        let _ = fs::remove_file(&path);

        assert_eq!(records.len(), MAX_RETAINED_EXECUTIONS);
        assert!(records.iter().any(|record| {
            record.id == "restarted" && record.seq == 0 && record.command == "new"
        }));
        assert!(records.iter().any(|record| record.id == "zz-newest"));
        assert!(!records.iter().any(|record| record.id == "other-1"));
        assert!(records.iter().any(|record| record.id == "other-2"));
    }

    #[test]
    fn history_reader_applies_the_retention_limit_before_session_filtering() {
        let path = temporary_journal("global-record-limit");
        let mut journal = Vec::new();
        writeln!(
            journal,
            "{{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"wanted-old\",\"session_id\":\"wanted\",\"seq\":0,\"command\":\"true\",\"cwd\":\"/\",\"started_at_ms\":0}}"
        )
        .unwrap();
        for seq in 1..=MAX_RETAINED_EXECUTIONS {
            writeln!(
                journal,
                "{{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"other-{seq}\",\"session_id\":\"other\",\"seq\":{seq},\"command\":\"true\",\"cwd\":\"/\",\"started_at_ms\":{seq}}}"
            )
            .unwrap();
        }
        writeln!(
            journal,
            "{{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"wanted-new\",\"session_id\":\"wanted\",\"seq\":{},\"command\":\"true\",\"cwd\":\"/\",\"started_at_ms\":{}}}",
            MAX_RETAINED_EXECUTIONS + 1,
            MAX_RETAINED_EXECUTIONS + 1
        )
        .unwrap();
        write_temporary_journal(&path, journal);

        let records = read_session_history_file(&path, "wanted").unwrap();
        let _ = fs::remove_file(&path);

        assert_eq!(
            records
                .iter()
                .map(|record| record.id.as_str())
                .collect::<Vec<_>>(),
            ["wanted-new"]
        );
    }

    #[test]
    fn jsh_session_id_validation_matches_the_public_grammar() {
        for valid in ["123-456", "tab_1", "ABC"] {
            assert!(valid_jsh_session_id(valid), "{valid}");
        }
        for invalid in ["", "has.dot", "has space", "雪"] {
            assert!(!valid_jsh_session_id(invalid), "{invalid}");
        }
    }

    #[test]
    fn invalid_history_requests_resolve_without_entering_the_reader_queue() {
        let session_id = "x".repeat(MAX_EXECUTION_ID_BYTES + 1);
        let load = request_history(session_id.clone()).unwrap();
        let snapshot = load
            .try_snapshot()
            .unwrap()
            .expect("invalid requests resolve synchronously");

        assert_eq!(snapshot.session_id, session_id);
        assert!(snapshot.records.is_empty());
        assert_eq!(snapshot.error.as_deref(), Some("invalid jsh session ID"));
    }

    #[test]
    fn journal_paths_are_bounded_and_safe_to_report() {
        assert!(validate_journal_path(Path::new("executions.jsonl")).is_ok());
        assert!(validate_journal_path(Path::new("executions.locked")).is_ok());
        for reserved in [
            "executions.lock",
            "/tmp/executions.lock",
            "/tmp/./executions.lock",
            "/tmp/EXECUTIONS.LOCK",
        ] {
            assert!(
                validate_journal_path(Path::new(reserved)).is_err(),
                "accepted reserved lock alias {reserved:?}"
            );
        }
        assert!(validate_journal_path(Path::new("bad\nname.jsonl")).is_err());
        assert!(validate_journal_path(Path::new("bad\u{0080}name.jsonl")).is_err());
        assert!(validate_journal_path(Path::new("bad\u{202e}name.jsonl")).is_err());
        assert!(validate_journal_path(Path::new("bad\u{fff9}name.jsonl")).is_err());
        let oversized = PathBuf::from("x".repeat(MAX_JOURNAL_PATH_BYTES + 1));
        assert!(validate_journal_path(&oversized).is_err());
    }

    #[test]
    fn terminal_output_cannot_grow_the_journal_past_the_reader_bound() {
        assert!(journal_append_within_bound(MAX_JOURNAL_READ_BYTES - 10, 10));
        assert!(!journal_append_within_bound(
            MAX_JOURNAL_READ_BYTES - 10,
            11
        ));
        assert!(!journal_append_within_bound(u64::MAX, usize::MAX));
    }

    #[test]
    fn terminal_append_rejects_ambiguous_exact_budget_atomically() {
        let root = TestDir::new("ambiguous-event-budget-append");
        let journal_path = root.0.join("executions.jsonl");
        let original = concat!(
            "{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"known\",\"session_id\":\"wanted\",\"seq\":1,\"command\":\"true\",\"cwd\":\"/\",\"started_at_ms\":1}\n",
            "{\"jsh_execution_version\":1,\"event\":\"finish\",\"id\":\"known\",\"exit_code\":9,\"duration_ms\":1,\"cwd_after\":\"/\",\"ended_at_ms\":2}\n",
            "{\"jsh_execution_version\":1,\"event\":\"start\",\"event\":\"finish\",\"id\":\"known\",\"session_id\":\"wanted\",\"seq\":3,\"command\":\"exact duplicate\",\"cwd\":\"/wrong\",\"started_at_ms\":3}\n",
            "{\"jsh_execution_version\":1,\"event\":\"finish\",\"id\":\"known\",\"\\u0069d\":\"other\",\"exit_code\":30,\"duration_ms\":30,\"cwd_after\":\"/wrong\",\"ended_at_ms\":30}\n",
            "{\"jsh_execution_version\":2,\"rsh_execution_version\":1,\"event\":\"start\",\"id\":\"known\",\"session_id\":\"wanted\",\"seq\":4,\"command\":\"canonical first\",\"cwd\":\"/wrong\",\"started_at_ms\":4}\n",
            "{\"rsh_execution_version\":1,\"jsh_execution_version\":2,\"event\":\"finish\",\"id\":\"known\",\"exit_code\":40,\"duration_ms\":40,\"cwd_after\":\"/wrong\",\"ended_at_ms\":40}\n",
            "{\"jsh_execution_version\":2,\"event\":\"start\",\"id\":\"known\",\"session_id\":\"wanted\",\"seq\":5,\"command\":\"future start\",\"cwd\":\"/future\",\"started_at_ms\":5}\n",
            "{\"jsh_execution_version\":2,\"event\":\"finish\",\"id\":\"known\",\"exit_code\":50,\"duration_ms\":50,\"cwd_after\":\"/future\",\"ended_at_ms\":50}\n",
            "{\"jsh_execution_version\":2,\"event\":\"output\",\"id\":\"known\",\"text\":\"future\",\"truncated\":false,\"total_bytes\":6,\"captured_at_ms\":2}\n"
        );
        fs::write(&journal_path, original).unwrap();
        #[cfg(unix)]
        fs::set_permissions(&journal_path, fs::Permissions::from_mode(0o600)).unwrap();
        let records = read_session_history_file_with_line_limit(&journal_path, "wanted", 9)
            .expect("ambiguous and future-version rows are forward-compatible");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].exit_code, Some(9));
        assert_eq!(records[0].output, None);

        // Establish the protocol sidecar before taking the atomicity snapshot:
        // a rejected append may lock existing coordination state, but must not
        // create, replace, or remove any directory entry of its own.
        drop(
            JournalFileLock::acquire(
                &root.0,
                &root.0.join(JOURNAL_LOCK_FILE_NAME),
                JournalLockMode::Exclusive,
                false,
            )
            .unwrap(),
        );
        let directory_entries = || {
            let mut entries = fs::read_dir(&root.0)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<Vec<_>>();
            entries.sort();
            entries
        };
        let entries_before = directory_entries();
        #[cfg(unix)]
        let metadata_before = {
            let journal = stable_metadata(&journal_path);
            let lock = stable_metadata(&root.0.join(JOURNAL_LOCK_FILE_NAME));
            let parent = stable_metadata(&root.0);
            std::thread::sleep(Duration::from_millis(2));
            (journal, lock, parent)
        };

        let event = b"{\"jsh_execution_version\":1,\"event\":\"output\",\"id\":\"known\",\"text\":\"new\",\"truncated\":false,\"total_bytes\":3,\"captured_at_ms\":2}\n";
        let error =
            append_encoded_event_to_path_with_line_limit(&journal_path, event, 9).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::FileTooLarge);
        assert_eq!(fs::read(&journal_path).unwrap(), original.as_bytes());
        assert_eq!(directory_entries(), entries_before);
        let rejected = read_session_history_file_with_line_limit(&journal_path, "wanted", 9)
            .expect("a rejected append leaves the source readable");
        assert_eq!(rejected, records);
        #[cfg(unix)]
        assert_eq!(
            (
                stable_metadata(&journal_path),
                stable_metadata(&root.0.join(JOURNAL_LOCK_FILE_NAME)),
                stable_metadata(&root.0),
            ),
            metadata_before,
            "read and rejected append must not fchmod private journal state"
        );

        append_encoded_event_to_path_with_line_limit(&journal_path, event, 10).unwrap();
        let restarted =
            read_session_history_file_with_line_limit(&journal_path, "wanted", 10).unwrap();
        assert_eq!(restarted.len(), 1);
        assert_eq!(restarted[0].exit_code, Some(9));
        assert_eq!(restarted[0].output.as_ref().unwrap().text, "new");
    }

    #[test]
    fn peer_tail_growth_is_counted_once_by_the_append_cache() {
        let root = TestDir::new("peer-line-cache");
        let journal_path = root.0.join("executions.jsonl");
        let start = b"{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"known\",\"session_id\":\"wanted\",\"seq\":1,\"command\":\"true\",\"cwd\":\"/\",\"started_at_ms\":1}\n";
        fs::write(&journal_path, start).unwrap();
        #[cfg(unix)]
        fs::set_permissions(&journal_path, fs::Permissions::from_mode(0o600)).unwrap();

        let mut observer = open_existing_journal_for_append(&journal_path).unwrap();
        let mut observed_len = observer.metadata().unwrap().len();
        assert_eq!(
            count_physical_lines_cached(&mut observer, &journal_path, observed_len).unwrap(),
            1
        );

        let mut peer = OpenOptions::new().append(true).open(&journal_path).unwrap();
        peer.write_all(b"{\"jsh_execution_version\":1,\"event\":\"future\",\"payload\":true}\n")
            .unwrap();
        observed_len = observer.metadata().unwrap().len();
        assert_eq!(
            count_physical_lines_cached(&mut observer, &journal_path, observed_len).unwrap(),
            2
        );
        assert_eq!(
            read_session_history_file_with_line_limit(&journal_path, "wanted", 2)
                .unwrap()
                .len(),
            1
        );

        peer.write_all(b"{\"malformed\":").unwrap();
        observed_len = observer.metadata().unwrap().len();
        assert_eq!(
            count_physical_lines_cached(&mut observer, &journal_path, observed_len).unwrap(),
            3
        );
        assert_eq!(
            read_session_history_file_with_line_limit(&journal_path, "wanted", 3)
                .unwrap()
                .len(),
            1
        );

        peer.write_all(b"\n").unwrap();
        drop(peer);
        observed_len = observer.metadata().unwrap().len();
        assert_eq!(
            count_physical_lines_cached(&mut observer, &journal_path, observed_len).unwrap(),
            3,
            "a later LF terminates the cached tail instead of adding a line"
        );
        drop(observer);

        let output = b"{\"jsh_execution_version\":1,\"event\":\"output\",\"id\":\"known\",\"text\":\"new\",\"truncated\":false,\"total_bytes\":3,\"captured_at_ms\":2}\n";
        let unchanged = fs::read(&journal_path).unwrap();
        let error =
            append_encoded_event_to_path_with_line_limit(&journal_path, output, 3).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::FileTooLarge);
        assert_eq!(fs::read(&journal_path).unwrap(), unchanged);

        append_encoded_event_to_path_with_line_limit(&journal_path, output, 4).unwrap();
        let reopened =
            read_session_history_file_with_line_limit(&journal_path, "wanted", 4).unwrap();
        assert_eq!(reopened.len(), 1);
        assert_eq!(reopened[0].output.as_ref().unwrap().text, "new");
    }

    #[test]
    fn ordinary_journal_append_still_creates_private_regular_files() {
        let root = TestDir::new("ordinary-append");
        let journal_path = root.0.join("executions.jsonl");
        let lock_path = root.0.join("executions.lock");

        append_encoded_event_to_path(&journal_path, b"event\n").unwrap();

        assert_eq!(fs::read(&journal_path).unwrap(), b"event\n");
        assert!(fs::metadata(&journal_path).unwrap().is_file());
        assert!(fs::metadata(&lock_path).unwrap().is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&journal_path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(&lock_path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn append_failures_preserve_their_visible_commit_stage_without_retry() {
        let event = b"event\n";

        let absent_root = TestDir::new("pre-write-create-fault");
        let absent_path = absent_root.0.join("executions.jsonl");
        drop(
            JournalFileLock::acquire(
                &absent_root.0,
                &absent_root.0.join(JOURNAL_LOCK_FILE_NAME),
                JournalLockMode::Exclusive,
                false,
            )
            .unwrap(),
        );
        let mut entries_before = fs::read_dir(&absent_root.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        entries_before.sort();
        let pre_write = FaultingAppendIo::new(AppendFaultStage::WriteAfter(0));
        let error = append_encoded_event_to_path_with_line_limit_and_io(
            &absent_path,
            event,
            MAX_JOURNAL_EVENT_LINES,
            &pre_write,
        )
        .unwrap_err();
        assert!(!error.to_string().contains("commit state is unknown"));
        assert!(
            !absent_path.exists(),
            "the new empty pathname is rolled back"
        );
        let mut entries_after = fs::read_dir(&absent_root.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        entries_after.sort();
        assert_eq!(entries_after, entries_before);
        assert_eq!(pre_write.write_calls.get(), 1);
        assert_eq!(pre_write.written.get(), 0);
        assert_eq!(pre_write.data_sync_calls.get(), 0);
        assert_eq!(pre_write.directory_sync_calls.get(), 0);

        let existing_root = TestDir::new("pre-write-existing-fault");
        let existing_path = existing_root.0.join("executions.jsonl");
        fs::write(&existing_path, b"stable\n").unwrap();
        #[cfg(unix)]
        fs::set_permissions(&existing_path, fs::Permissions::from_mode(0o600)).unwrap();
        let existing_identity = fs::metadata(&existing_path).unwrap();
        let pre_write = FaultingAppendIo::new(AppendFaultStage::WriteAfter(0));
        let error = append_encoded_event_to_path_with_line_limit_and_io(
            &existing_path,
            event,
            MAX_JOURNAL_EVENT_LINES,
            &pre_write,
        )
        .unwrap_err();
        assert!(!error.to_string().contains("commit state is unknown"));
        assert_eq!(fs::read(&existing_path).unwrap(), b"stable\n");
        #[cfg(unix)]
        {
            let after = fs::metadata(&existing_path).unwrap();
            assert_eq!(
                (after.dev(), after.ino()),
                (existing_identity.dev(), existing_identity.ino())
            );
        }

        let partial_root = TestDir::new("partial-write-fault");
        let partial_path = partial_root.0.join("executions.jsonl");
        fs::write(&partial_path, b"torn").unwrap();
        #[cfg(unix)]
        fs::set_permissions(&partial_path, fs::Permissions::from_mode(0o600)).unwrap();
        let partial = FaultingAppendIo::new(AppendFaultStage::WriteAfter(3));
        let error = append_encoded_event_to_path_with_line_limit_and_io(
            &partial_path,
            event,
            MAX_JOURNAL_EVENT_LINES,
            &partial,
        )
        .unwrap_err();
        assert!(error.to_string().contains("commit state is unknown"));
        assert_eq!(fs::read(&partial_path).unwrap(), b"torn\nev");
        assert_eq!(partial.write_calls.get(), 3);
        assert_eq!(partial.written.get(), 3);
        assert_eq!(partial.data_sync_calls.get(), 0);
        assert_eq!(partial.directory_sync_calls.get(), 0);
        append_encoded_event_to_path(&partial_path, event).unwrap();
        assert_eq!(fs::read(&partial_path).unwrap(), b"torn\nev\nevent\n");

        let data_root = TestDir::new("data-sync-fault");
        let data_path = data_root.0.join("executions.jsonl");
        let data = FaultingAppendIo::new(AppendFaultStage::DataSync);
        let error = append_encoded_event_to_path_with_line_limit_and_io(
            &data_path,
            event,
            MAX_JOURNAL_EVENT_LINES,
            &data,
        )
        .unwrap_err();
        assert!(error.to_string().contains("commit state is unknown"));
        assert_eq!(fs::read(&data_path).unwrap(), event);
        assert_eq!(data.written.get(), event.len());
        assert_eq!(data.data_sync_calls.get(), 1);
        assert_eq!(data.directory_sync_calls.get(), 0);

        let directory_root = TestDir::new("directory-sync-fault");
        let directory_path = directory_root.0.join("executions.jsonl");
        let directory = FaultingAppendIo::new(AppendFaultStage::DirectorySync);
        let error = append_encoded_event_to_path_with_line_limit_and_io(
            &directory_path,
            event,
            MAX_JOURNAL_EVENT_LINES,
            &directory,
        )
        .unwrap_err();
        assert!(error.to_string().contains("commit state is unknown"));
        assert_eq!(fs::read(&directory_path).unwrap(), event);
        assert_eq!(directory.written.get(), event.len());
        assert_eq!(directory.data_sync_calls.get(), 1);
        assert_eq!(directory.directory_sync_calls.get(), 1);

        let existing = FaultingAppendIo::new(AppendFaultStage::DirectorySync);
        append_encoded_event_to_path_with_line_limit_and_io(
            &directory_path,
            b"second\n",
            MAX_JOURNAL_EVENT_LINES,
            &existing,
        )
        .unwrap();
        assert_eq!(existing.data_sync_calls.get(), 1);
        assert_eq!(
            existing.directory_sync_calls.get(),
            0,
            "an existing journal pathname needs only the data barrier"
        );
        assert_eq!(fs::read(&directory_path).unwrap(), b"event\nsecond\n");
    }

    #[test]
    fn terminal_append_separates_a_torn_shell_event_tail() {
        let root = TestDir::new("torn-tail");
        let journal_path = root.0.join("executions.jsonl");
        fs::write(&journal_path, b"{\"partial\":true").unwrap();
        #[cfg(unix)]
        fs::set_permissions(&journal_path, fs::Permissions::from_mode(0o600)).unwrap();

        append_encoded_event_to_path(&journal_path, b"terminal-output\n").unwrap();

        assert_eq!(
            fs::read(&journal_path).unwrap(),
            b"{\"partial\":true\nterminal-output\n"
        );
    }

    #[test]
    fn torn_tail_separator_counts_toward_the_reader_bound() {
        let root = TestDir::new("torn-tail-bound");
        let journal_path = root.0.join("executions.jsonl");
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&journal_path)
            .unwrap();
        file.set_len(MAX_JOURNAL_READ_BYTES - 2).unwrap();
        drop(file);
        #[cfg(unix)]
        fs::set_permissions(&journal_path, fs::Permissions::from_mode(0o600)).unwrap();

        let error = append_encoded_event_to_path(&journal_path, b"x\n").unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::FileTooLarge);
        assert_eq!(
            fs::metadata(&journal_path).unwrap().len(),
            MAX_JOURNAL_READ_BYTES - 2
        );
    }

    #[cfg(unix)]
    #[test]
    fn journal_lock_is_bounded_and_survives_sidecar_replacement() {
        let root = TestDir::new("lock-replacement");
        let lock_path = root.0.join("executions.lock");
        let retired_path = root.0.join("retired.lock");
        let held = JournalFileLock::acquire(&root.0, &lock_path, JournalLockMode::Exclusive, false)
            .unwrap();
        fs::rename(&lock_path, &retired_path).unwrap();

        let started = Instant::now();
        let error = JournalFileLock::acquire_with_timeout(
            &root.0,
            &lock_path,
            JournalLockMode::Exclusive,
            Duration::from_millis(25),
            false,
        )
        .err()
        .expect("renaming the sidecar must not bypass the directory lock");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(1));

        drop(held);
        let reacquired = JournalFileLock::acquire_with_timeout(
            &root.0,
            &lock_path,
            JournalLockMode::Exclusive,
            Duration::from_millis(25),
            false,
        )
        .expect("the protocol remains usable after the original guard exits");
        drop(reacquired);
        fs::remove_file(retired_path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn journal_lock_descriptors_are_close_on_exec() {
        use std::os::fd::AsRawFd;

        let root = TestDir::new("lock-cloexec");
        let lock_path = root.0.join("executions.lock");
        let held = JournalFileLock::acquire(&root.0, &lock_path, JournalLockMode::Exclusive, false)
            .unwrap();
        for file in [&held.directory, &held.file] {
            // SAFETY: each File owns a live descriptor and F_GETFD only reads
            // descriptor flags.
            let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) };
            assert!(flags >= 0);
            assert_ne!(flags & libc::FD_CLOEXEC, 0);
        }
    }

    #[cfg(unix)]
    #[test]
    fn journal_fifo_is_rejected_without_blocking() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let root = TestDir::new("journal-fifo");
        let journal_path = root.0.join("executions.jsonl");
        let encoded = CString::new(journal_path.as_os_str().as_bytes()).unwrap();
        // SAFETY: encoded is a live NUL-terminated path and the mode is valid.
        assert_eq!(unsafe { libc::mkfifo(encoded.as_ptr(), 0o600) }, 0);

        let started = Instant::now();
        assert!(read_session_history_file(&journal_path, "wanted").is_err());
        assert!(append_encoded_event_to_path(&journal_path, b"event\n").is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[cfg(unix)]
    #[test]
    fn nonsticky_writable_journal_parent_is_rejected() {
        use std::os::unix::fs::PermissionsExt;

        let root = TestDir::new("writable-parent");
        fs::set_permissions(&root.0, fs::Permissions::from_mode(0o777)).unwrap();
        let journal_path = root.0.join("executions.jsonl");

        assert!(append_encoded_event_to_path(&journal_path, b"event\n").is_err());
        assert!(!journal_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn sticky_writable_journal_parent_is_also_rejected() {
        use std::os::unix::fs::PermissionsExt;

        let root = TestDir::new("sticky-writable-parent");
        fs::set_permissions(&root.0, fs::Permissions::from_mode(0o1777)).unwrap();
        let journal_path = root.0.join("executions.jsonl");

        assert!(append_encoded_event_to_path(&journal_path, b"event\n").is_err());
        assert!(!journal_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn journal_parent_trust_never_exempts_a_different_owner() {
        let path = Path::new("/shared");

        assert!(validate_journal_directory_trust(path, 1_000, 0o700, 1_000).is_ok());
        assert!(validate_journal_directory_trust(path, 0, 0o700, 1_000).is_err());
        assert!(validate_journal_directory_trust(path, 1_000, 0o1777, 1_000).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn implicit_default_parent_is_hardened_before_the_mode_gate() {
        use std::os::unix::fs::PermissionsExt;

        let root = TestDir::new("default-parent-hardening");
        fs::set_permissions(&root.0, fs::Permissions::from_mode(0o770)).unwrap();

        let directory = open_journal_directory_with_policy(&root.0, true).unwrap();
        assert_eq!(
            directory.metadata().unwrap().permissions().mode() & 0o777,
            0o700
        );

        fs::set_permissions(&root.0, fs::Permissions::from_mode(0o770)).unwrap();
        assert!(open_journal_directory_with_policy(&root.0, false).is_err());
        assert_eq!(
            fs::metadata(&root.0).unwrap().permissions().mode() & 0o777,
            0o770
        );
    }

    #[cfg(unix)]
    #[test]
    fn journal_lock_symlink_never_changes_its_target() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let root = TestDir::new("lock-symlink");
        let target = root.0.join("do-not-touch");
        let lock_path = root.0.join("executions.lock");
        let journal_path = root.0.join("executions.jsonl");
        fs::write(&target, "sentinel contents").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
        symlink(&target, &lock_path).unwrap();

        assert!(append_encoded_event_to_path(&journal_path, b"event\n").is_err());
        assert_eq!(fs::read_to_string(&target).unwrap(), "sentinel contents");
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert!(!journal_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn journal_symlink_is_rejected_for_reads_and_appends() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let root = TestDir::new("journal-symlink");
        let target = root.0.join("do-not-touch");
        let journal_path = root.0.join("executions.jsonl");
        fs::write(&target, "sentinel contents").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
        symlink(&target, &journal_path).unwrap();

        assert!(read_session_history_file(&journal_path, "wanted").is_err());
        assert!(append_encoded_event_to_path(&journal_path, b"event\n").is_err());
        assert_eq!(fs::read_to_string(&target).unwrap(), "sentinel contents");
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }

    #[cfg(unix)]
    #[test]
    fn journal_writable_by_other_users_is_rejected() {
        use std::os::unix::fs::PermissionsExt;

        let root = TestDir::new("shared-write");
        let journal_path = root.0.join("executions.jsonl");
        fs::write(&journal_path, "event\n").unwrap();
        fs::set_permissions(&journal_path, fs::Permissions::from_mode(0o622)).unwrap();
        let metadata_before = stable_metadata(&journal_path);
        std::thread::sleep(Duration::from_millis(2));

        assert!(read_session_history_file(&journal_path, "wanted").is_err());
        assert!(append_encoded_event_to_path(&journal_path, b"event\n").is_err());
        assert_eq!(fs::read_to_string(journal_path).unwrap(), "event\n");
        assert_eq!(
            stable_metadata(&root.0.join("executions.jsonl")),
            metadata_before,
            "an unsafe mode is rejected before any attempted repair"
        );
    }

    #[cfg(unix)]
    #[test]
    fn owner_readable_journal_is_tightened_after_validation() {
        use std::os::unix::fs::PermissionsExt;

        let root = TestDir::new("owner-readable");
        let journal_path = root.0.join("executions.jsonl");
        fs::write(
            &journal_path,
            b"{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"jsh-readable\",\"session_id\":\"wanted\",\"seq\":1,\"command\":\"true\",\"cwd\":\"/tmp\",\"started_at_ms\":1}\n",
        )
        .unwrap();
        fs::set_permissions(&journal_path, fs::Permissions::from_mode(0o644)).unwrap();

        assert_eq!(
            read_session_history_file(&journal_path, "wanted")
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            fs::metadata(&journal_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn journal_hard_link_never_changes_its_target() {
        let root = TestDir::new("journal-hard-link");
        let target = root.0.join("do-not-touch");
        let journal_path = root.0.join("executions.jsonl");
        fs::write(&target, "sentinel contents").unwrap();
        fs::hard_link(&target, &journal_path).unwrap();

        assert!(read_session_history_file(&journal_path, "wanted").is_err());
        assert!(append_encoded_event_to_path(&journal_path, b"event\n").is_err());
        assert_eq!(fs::read_to_string(&target).unwrap(), "sentinel contents");
    }
}
