//! Asynchronous bridge from terminal OSC 133 records to rsh's execution log.
//!
//! Terminal parsing runs on the UI thread (and on the bounded background-tab
//! pump), so it must never wait for a filesystem, an advisory lock, or another
//! process.  A small bounded channel hands immutable output snapshots to one
//! writer thread.  rsh owns the rest of the execution lifecycle (`start` and
//! `finish`); jterm contributes the text that was actually rendered by the
//! terminal as an `output` event with the same execution id.

use crossbeam_channel::{bounded, Receiver, Sender, TryRecvError, TrySendError};
use once_cell::sync::OnceCell;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const EXECUTION_JOURNAL_VERSION: u32 = 1;
const WRITER_QUEUE_CAPACITY: usize = 64;
const READER_QUEUE_CAPACITY: usize = 2;
const MAX_EVENT_LINE_BYTES: usize = 1024 * 1024;
const MAX_EXECUTION_ID_BYTES: usize = 192;
const MAX_COMMAND_BYTES: usize = 64 * 1024;
const MAX_CWD_BYTES: usize = 4 * 1024;
const MAX_OUTPUT_BYTES: usize = 256 * 1024;
const MAX_JOURNAL_FILE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_RETAINED_EXECUTIONS: usize = 2_000;
// rsh compacts after its own event. jterm's correlated output can be the next
// line and legitimately leave the file one bounded event over the threshold.
const MAX_JOURNAL_READ_BYTES: u64 = MAX_JOURNAL_FILE_BYTES + MAX_EVENT_LINE_BYTES as u64 + 1;

/// One completed command's captured output, as reported by a terminal app.
/// Only correlation id and output payload matter here; rsh's own events carry
/// the command line, cwd, exit code, and duration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletedExecution {
    pub id: String,
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

impl HistoryLoad {
    /// Poll a one-shot background read without ever waiting on the UI thread.
    pub fn try_snapshot(&self) -> Result<Option<HistorySnapshot>, ()> {
        match self.receiver.try_recv() {
            Ok(snapshot) => Ok(Some(snapshot)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryRequestError {
    Full,
    Closed,
}

#[derive(Debug, Serialize)]
struct OutputEvent {
    rsh_execution_version: u32,
    event: &'static str,
    id: String,
    text: String,
    truncated: bool,
    total_bytes: u64,
    captured_at_ms: u64,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "event")]
enum PersistedEvent {
    #[serde(rename = "start")]
    Start {
        rsh_execution_version: u32,
        id: String,
        session_id: Option<String>,
        seq: u64,
        command: String,
        #[serde(default)]
        command_truncated: bool,
        cwd: String,
        started_at_ms: u64,
    },
    #[serde(rename = "finish")]
    Finish {
        rsh_execution_version: u32,
        id: String,
        exit_code: i32,
        duration_ms: u64,
        cwd_after: String,
        ended_at_ms: u64,
    },
    #[serde(rename = "output")]
    Output {
        rsh_execution_version: u32,
        id: String,
        text: String,
        truncated: bool,
        total_bytes: u64,
        captured_at_ms: u64,
    },
}

impl PersistedEvent {
    fn version(&self) -> u32 {
        match self {
            Self::Start {
                rsh_execution_version,
                ..
            }
            | Self::Finish {
                rsh_execution_version,
                ..
            }
            | Self::Output {
                rsh_execution_version,
                ..
            } => *rsh_execution_version,
        }
    }
}

enum JournalMessage {
    Output(OutputEvent),
    Flush(Sender<()>),
}

struct HistoryRequest {
    session_id: String,
    reply: Sender<HistorySnapshot>,
}

impl OutputEvent {
    fn from_completed(completed: CompletedExecution) -> Option<Self> {
        // Bare FinalTerm markers receive terminal-local ids so the timeline
        // still works, but there is no matching rsh start/finish lifecycle to
        // correlate on disk.
        if !completed.output_available || !valid_rsh_execution_id(&completed.id) {
            return None;
        }
        let retained_bytes = u64::try_from(completed.output.len()).unwrap_or(u64::MAX);
        Some(Self {
            rsh_execution_version: EXECUTION_JOURNAL_VERSION,
            event: "output",
            id: completed.id,
            text: completed.output,
            truncated: completed.truncated,
            total_bytes: u64::try_from(completed.total_bytes)
                .unwrap_or(u64::MAX)
                .max(retained_bytes),
            captured_at_ms: unix_time_ms(),
        })
    }
}

static WRITER: OnceCell<Option<Sender<JournalMessage>>> = OnceCell::new();
static READER: OnceCell<Option<Sender<HistoryRequest>>> = OnceCell::new();

fn writer() -> Option<&'static Sender<JournalMessage>> {
    WRITER
        .get_or_init(|| {
            let (tx, rx) = bounded::<JournalMessage>(WRITER_QUEUE_CAPACITY);
            match std::thread::Builder::new()
                .name("rsh-execution-journal".to_owned())
                .spawn(move || {
                    while let Ok(message) = rx.recv() {
                        match message {
                            JournalMessage::Output(event) => {
                                if let Err(error) = append_event(event) {
                                    log::warn!("cannot append rsh execution output: {error}");
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
                    log::warn!("cannot start rsh execution journal writer: {error}");
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
                .name("rsh-execution-history".to_owned())
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
                    log::warn!("cannot start rsh execution history reader: {error}");
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
    if !enabled() {
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
/// remains represented by rsh's start/finish events, while memory stays
/// bounded even if the state directory is on a stalled filesystem.
pub fn submit(completed: CompletedExecution) -> Result<(), SubmitError> {
    if !enabled() {
        return Ok(());
    }
    let Some(event) = OutputEvent::from_completed(completed) else {
        return Ok(());
    };
    let writer = writer().ok_or(SubmitError::Closed)?;
    writer
        .try_send(JournalMessage::Output(event))
        .map_err(|error| match error {
            TrySendError::Full(_) => SubmitError::Full,
            TrySendError::Disconnected(_) => SubmitError::Closed,
        })
}

/// Wait briefly for every output accepted before this call to reach disk.
/// Used during orderly application shutdown; normal terminal frames never
/// block on the journal.
pub fn flush(timeout: std::time::Duration) -> bool {
    if !enabled() {
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

fn enabled() -> bool {
    std::env::var("RSH_EXECUTION_JOURNAL")
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
    if let Some(path) = std::env::var_os("RSH_EXECUTION_JOURNAL_PATH")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
    {
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "RSH_EXECUTION_JOURNAL_PATH must be absolute",
            ));
        }
        return Ok((path, true));
    }
    let state_dir = dirs::state_dir()
        .or_else(|| dirs::home_dir().map(|home| home.join(".local/state")))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no user state directory"))?;
    Ok((state_dir.join("rsh/executions.jsonl"), false))
}

fn harden_open_options(options: &mut OpenOptions) {
    #[cfg(unix)]
    {
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
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
    }

    Ok(())
}

fn open_journal_lock(path: &std::path::Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    harden_open_options(&mut options);
    let file = options.open(path)?;
    validate_journal_file(&file, "execution journal lock")?;
    #[cfg(unix)]
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

fn open_journal_for_read(path: &std::path::Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    harden_open_options(&mut options);
    let file = options.open(path)?;
    validate_journal_file(&file, "execution journal")?;
    Ok(file)
}

fn open_journal_for_append(path: &std::path::Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    harden_open_options(&mut options);
    let file = options.open(path)?;
    validate_journal_file(&file, "execution journal")?;
    Ok(file)
}

fn read_session_history(session_id: &str) -> io::Result<Vec<PersistedExecution>> {
    if !valid_rsh_session_id(session_id) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid rsh session ID",
        ));
    }
    let (path, _) = journal_path()?;
    if !path.try_exists()? {
        return Ok(Vec::new());
    }
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "journal has no parent"))?;
    let lock_path = dir.join("executions.lock");
    let lock = open_journal_lock(&lock_path)?;
    lock_shared(&lock)?;

    let read_result = match read_session_history_file(&path, session_id) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        result => result,
    };
    let unlock_result = unlock(&lock);
    match (read_result, unlock_result) {
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
        (Ok(records), Ok(())) => Ok(records),
    }
}

fn read_session_history_file(
    path: &std::path::Path,
    session_id: &str,
) -> io::Result<Vec<PersistedExecution>> {
    let file = open_journal_for_read(path)?;
    if file.metadata()?.len() > MAX_JOURNAL_READ_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "rsh execution journal exceeds its bounded size",
        ));
    }

    let mut records = HashMap::<String, PersistedExecution>::new();
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    while let Some(within_limit) = read_bounded_line(&mut reader, &mut line)? {
        if !within_limit {
            continue;
        }
        let Ok(event) = serde_json::from_slice::<PersistedEvent>(&line) else {
            continue;
        };
        if event.version() != EXECUTION_JOURNAL_VERSION {
            continue;
        }
        match event {
            PersistedEvent::Start {
                id,
                session_id: event_session_id,
                seq,
                command,
                command_truncated,
                cwd,
                started_at_ms,
                ..
            } => {
                if !valid_rsh_execution_id(&id)
                    || event_session_id
                        .as_deref()
                        .is_some_and(|id| !valid_rsh_session_id(id))
                    || command.len() > MAX_COMMAND_BYTES
                    || cwd.len() > MAX_CWD_BYTES
                {
                    continue;
                }
                // A later duplicate start is authoritative, including when it
                // moves an ID out of the requested session.
                records.remove(&id);
                if event_session_id.as_deref() != Some(session_id) {
                    continue;
                }
                records.insert(
                    id.clone(),
                    PersistedExecution {
                        id,
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
                    },
                );
            }
            PersistedEvent::Finish {
                id,
                exit_code,
                duration_ms,
                cwd_after,
                ended_at_ms,
                ..
            } => {
                if !valid_rsh_execution_id(&id) || cwd_after.len() > MAX_CWD_BYTES {
                    continue;
                }
                if let Some(record) = records.get_mut(&id) {
                    record.exit_code = Some(exit_code);
                    record.duration_ms = Some(duration_ms);
                    record.cwd_after = Some(cwd_after);
                    record.ended_at_ms = Some(ended_at_ms);
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
                if !valid_rsh_execution_id(&id) || text.len() > MAX_OUTPUT_BYTES {
                    continue;
                }
                if let Some(record) = records.get_mut(&id) {
                    record.output = Some(PersistedExecutionOutput {
                        total_bytes: total_bytes.max(text.len() as u64),
                        text,
                        truncated,
                        captured_at_ms,
                    });
                }
            }
        }
    }

    let mut records = records.into_values().collect::<Vec<_>>();
    records.sort_by(|left, right| {
        (left.started_at_ms, left.seq, &left.id).cmp(&(right.started_at_ms, right.seq, &right.id))
    });
    let keep_from = records.len().saturating_sub(MAX_RETAINED_EXECUTIONS);
    Ok(records.split_off(keep_from))
}

/// Read and, when necessary, discard one JSONL event without allocating more
/// than rsh's public per-event limit. `false` denotes an oversized event.
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
            if line.len() + consumed <= MAX_EVENT_LINE_BYTES + 1 {
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

fn prepare_journal_path() -> io::Result<PathBuf> {
    let (path, custom_path) = journal_path()?;
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "journal has no parent"))?;
    let dir_already_existed = dir.exists();
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    if !custom_path || !dir_already_existed {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(path)
}

fn append_event(event: OutputEvent) -> io::Result<()> {
    let encoded = encode_event(event)?;
    let journal_path = prepare_journal_path()?;
    append_encoded_event_to_path(&journal_path, &encoded)
}

fn append_encoded_event_to_path(journal_path: &std::path::Path, encoded: &[u8]) -> io::Result<()> {
    let dir = journal_path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "journal has no parent"))?;
    let lock_path = dir.join("executions.lock");

    let lock = open_journal_lock(&lock_path)?;
    lock_exclusive(&lock)?;

    let write_result = (|| {
        let mut journal = open_journal_for_append(journal_path)?;
        let current_len = journal.metadata()?.len();
        if !journal_append_within_bound(current_len, encoded.len()) {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "rsh execution journal is awaiting lifecycle compaction",
            ));
        }
        #[cfg(unix)]
        journal.set_permissions(fs::Permissions::from_mode(0o600))?;
        journal.write_all(encoded)?;
        journal.flush()
    })();

    let unlock_result = unlock(&lock);
    write_result.and(unlock_result)
}

fn journal_append_within_bound(current_bytes: u64, event_bytes: usize) -> bool {
    current_bytes.saturating_add(u64::try_from(event_bytes).unwrap_or(u64::MAX))
        <= MAX_JOURNAL_READ_BYTES
}

/// Match rsh's public execution-id grammar. Generic FinalTerm producers still
/// get the in-memory timeline, but their unrelated IDs must not add orphan
/// output events to rsh's journal.
fn valid_rsh_execution_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_EXECUTION_ID_BYTES
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

const MAX_RSH_SESSION_ID_BYTES: usize = 128;

/// Shared definition of a well-formed rsh session id (apps re-use this for
/// their own session-id handling).
pub fn is_valid_rsh_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_RSH_SESSION_ID_BYTES
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn valid_rsh_session_id(id: &str) -> bool {
    is_valid_rsh_session_id(id)
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

/// Serialize within the same one-line limit enforced by rsh's reader. JSON
/// escaping can expand control-heavy terminal text well beyond its UTF-8 byte
/// count, so retry with a smaller UTF-8-safe head/tail snapshot when needed.
fn encode_event(mut event: OutputEvent) -> io::Result<Vec<u8>> {
    let mut encoded = serde_json::to_vec(&event).map_err(io::Error::other)?;
    if encoded.len() > MAX_EVENT_LINE_BYTES {
        event.text = bounded_text(
            &event.text,
            MAX_OUTPUT_BYTES / 2,
        );
        event.truncated = true;
        event.total_bytes = event.total_bytes.max(event.text.len() as u64);
        encoded = serde_json::to_vec(&event).map_err(io::Error::other)?;
    }
    if encoded.len() > MAX_EVENT_LINE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "rsh execution output event exceeds the journal line limit",
        ));
    }
    encoded.push(b'\n');
    Ok(encoded)
}

#[cfg(unix)]
fn lock_exclusive(file: &std::fs::File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: `file` remains open for the entire flock lifetime and flock does
    // not dereference userspace pointers.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(unix)]
fn lock_shared(file: &std::fs::File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: `file` remains open for the flock lifetime; flock does not
    // dereference userspace pointers.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(unix))]
fn lock_exclusive(_file: &std::fs::File) -> io::Result<()> {
    Ok(())
}

#[cfg(not(unix))]
fn lock_shared(_file: &std::fs::File) -> io::Result<()> {
    Ok(())
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

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let id = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "jterm2-execution-journal-{}-{label}-{id}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
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
            "jterm2-execution-journal-{}-{name}.jsonl",
            std::process::id()
        ))
    }

    #[test]
    fn unavailable_output_is_not_persisted() {
        let completed = CompletedExecution {
            id: "id".to_owned(),
            output: String::new(),
            output_available: false,
            truncated: false,
            total_bytes: 0,
        };
        assert!(OutputEvent::from_completed(completed).is_none());
    }

    #[test]
    fn only_rsh_compatible_execution_ids_are_persisted() {
        for id in ["local:1", "rsh:1", "contains space", "雪"] {
            let completed = CompletedExecution {
                id: id.to_owned(),
                output: "output".to_owned(),
                output_available: true,
                truncated: false,
                total_bytes: 6,
            };
            assert!(OutputEvent::from_completed(completed).is_none(), "{id}");
        }

        let valid = CompletedExecution {
            id: "rsh-a_b.c-1".to_owned(),
            output: "output".to_owned(),
            output_available: true,
            truncated: false,
            total_bytes: 6,
        };
        assert!(OutputEvent::from_completed(valid).is_some());
    }

    #[test]
    fn output_event_matches_rsh_envelope() {
        let completed = CompletedExecution {
            id: "exec-1".to_owned(),
            output: "hi".to_owned(),
            output_available: true,
            truncated: false,
            total_bytes: 2,
        };
        let value = serde_json::to_value(OutputEvent::from_completed(completed).unwrap()).unwrap();
        assert_eq!(value["rsh_execution_version"], 1);
        assert_eq!(value["event"], "output");
        assert_eq!(value["id"], "exec-1");
        assert_eq!(value["text"], "hi");
        assert_eq!(value["total_bytes"], 2);
        assert!(value.get("command").is_none());
    }

    #[test]
    fn control_heavy_output_stays_within_rshs_jsonl_limit() {
        let output = "\0".repeat(MAX_OUTPUT_BYTES);
        let total_bytes = output.len();
        let completed = CompletedExecution {
            id: "rsh-control-heavy".to_owned(),
            output,
            output_available: true,
            truncated: false,
            total_bytes,
        };
        let encoded = encode_event(OutputEvent::from_completed(completed).unwrap()).unwrap();

        assert!(encoded.len() <= MAX_EVENT_LINE_BYTES + 1);
        assert_eq!(encoded.last(), Some(&b'\n'));
        let value: serde_json::Value =
            serde_json::from_slice(&encoded[..encoded.len() - 1]).unwrap();
        assert_eq!(value["truncated"], true);
        assert_eq!(value["total_bytes"], total_bytes as u64);
        assert!(
            value["text"].as_str().unwrap().len()
                <= MAX_OUTPUT_BYTES / 2
        );
    }

    #[test]
    fn history_reader_folds_only_the_requested_session() {
        let path = temporary_journal("fold-session");
        let journal = concat!(
            "not json\n",
            "{\"rsh_execution_version\":99,\"event\":\"start\",\"id\":\"future\",\"session_id\":\"wanted\",\"seq\":0,\"command\":\"future\",\"cwd\":\"/\",\"started_at_ms\":0}\n",
            "{\"rsh_execution_version\":1,\"event\":\"start\",\"id\":\"other-1\",\"session_id\":\"other\",\"seq\":1,\"command\":\"ignore\",\"cwd\":\"/other\",\"started_at_ms\":1}\n",
            "{\"rsh_execution_version\":1,\"event\":\"start\",\"id\":\"wanted-1\",\"session_id\":\"wanted\",\"seq\":7,\"command\":\"printf hi\",\"command_truncated\":false,\"cwd\":\"/tmp\",\"started_at_ms\":10}\n",
            "{\"rsh_execution_version\":1,\"event\":\"output\",\"id\":\"wanted-1\",\"text\":\"hi\",\"truncated\":false,\"total_bytes\":1,\"captured_at_ms\":12}\n",
            "{\"rsh_execution_version\":1,\"event\":\"finish\",\"id\":\"wanted-1\",\"exit_code\":3,\"duration_ms\":2,\"cwd_after\":\"/tmp/after\",\"ended_at_ms\":12}\n"
        );
        fs::write(&path, journal).unwrap();

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
        assert_eq!(output.total_bytes, 2);
    }

    #[test]
    fn history_reader_discards_an_oversized_line_and_resumes() {
        let path = temporary_journal("oversized-line");
        let valid = b"{\"rsh_execution_version\":1,\"event\":\"start\",\"id\":\"after-large\",\"session_id\":\"wanted\",\"seq\":1,\"command\":\"true\",\"cwd\":\"/\",\"started_at_ms\":1}\n";
        let mut journal = vec![b'x'; MAX_EVENT_LINE_BYTES + 2];
        journal.push(b'\n');
        journal.extend_from_slice(valid);
        fs::write(&path, journal).unwrap();

        let records = read_session_history_file(&path, "wanted").unwrap();
        let _ = fs::remove_file(&path);

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].id, "after-large");
    }

    #[test]
    fn history_reader_keeps_only_the_latest_bounded_records() {
        let path = temporary_journal("record-limit");
        let mut journal = Vec::new();
        for seq in 0..=MAX_RETAINED_EXECUTIONS {
            writeln!(
                journal,
                "{{\"rsh_execution_version\":1,\"event\":\"start\",\"id\":\"exec-{seq}\",\"session_id\":\"wanted\",\"seq\":{seq},\"command\":\"true\",\"cwd\":\"/\",\"started_at_ms\":{seq}}}"
            )
            .unwrap();
        }
        fs::write(&path, journal).unwrap();

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
    fn rsh_session_id_validation_matches_the_public_grammar() {
        for valid in ["123-456", "tab_1", "ABC"] {
            assert!(valid_rsh_session_id(valid), "{valid}");
        }
        for invalid in ["", "has.dot", "has space", "雪"] {
            assert!(!valid_rsh_session_id(invalid), "{invalid}");
        }
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
