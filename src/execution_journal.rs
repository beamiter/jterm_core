//! Asynchronous bridge from terminal OSC 133 records to jsh's execution log.
//!
//! Terminal parsing runs on the UI thread (and on the bounded background-tab
//! pump), so it must never wait for a filesystem, an advisory lock, or another
//! process.  A small bounded channel hands immutable output snapshots to one
//! writer thread.  jsh owns the rest of the execution lifecycle (`start` and
//! `finish`); jterm contributes the text that was actually rendered by the
//! terminal as an `output` event with the same execution id.

use crossbeam_channel::{bounded, Receiver, Sender, TryRecvError, TrySendError};
use once_cell::sync::OnceCell;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
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
/// Size at which jsh compacts its append-only journal.
pub const MAX_JOURNAL_FILE_BYTES: u64 = 32 * 1024 * 1024;
/// Maximum folded execution records retained by either reader.
pub const MAX_RETAINED_EXECUTIONS: usize = 2_000;
// jsh compacts after its own event. jterm's correlated output can be the next
// line and legitimately leave the file one bounded event over the threshold.
const MAX_JOURNAL_READ_BYTES: u64 = MAX_JOURNAL_FILE_BYTES + MAX_EVENT_LINE_BYTES as u64 + 1;
const JOURNAL_LOCK_TIMEOUT: Duration = Duration::from_secs(2);
const JOURNAL_LOCK_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// One completed command's captured output, as reported by a terminal app.
/// Only correlation id and output payload matter here; jsh's own events carry
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

#[derive(Debug, Serialize)]
struct OutputEvent {
    jsh_execution_version: u32,
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
    },
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
}

impl PersistedEvent {
    fn version(&self) -> u32 {
        match self {
            Self::Start {
                jsh_execution_version,
                ..
            }
            | Self::Finish {
                jsh_execution_version,
                ..
            }
            | Self::Output {
                jsh_execution_version,
                ..
            } => *jsh_execution_version,
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
        // still works, but there is no matching jsh start/finish lifecycle to
        // correlate on disk.
        if !completed.output_available || !valid_jsh_execution_id(&completed.id) {
            return None;
        }
        let observed_bytes = completed.output.len();
        let retained_bytes = u64::try_from(observed_bytes).unwrap_or(u64::MAX);
        let (text, truncated) = if observed_bytes > MAX_OUTPUT_BYTES {
            (bounded_text(&completed.output, MAX_OUTPUT_BYTES), true)
        } else {
            (completed.output, completed.truncated)
        };
        Some(Self {
            jsh_execution_version: EXECUTION_JOURNAL_VERSION,
            event: "output",
            id: completed.id,
            text,
            truncated,
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
                .name("jsh-execution-journal".to_owned())
                .spawn(move || {
                    while let Ok(message) = rx.recv() {
                        match message {
                            JournalMessage::Output(event) => {
                                if let Err(error) = append_event(event) {
                                    log::warn!("cannot append jsh execution output: {error}");
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
    if path.file_name().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "execution journal path has no file name",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let bytes = path.as_os_str().as_bytes();
        if bytes.len() > MAX_JOURNAL_PATH_BYTES
            || bytes.iter().any(|byte| matches!(*byte, 0..=0x1f | 0x7f))
            || path
                .to_str()
                .is_some_and(crate::review_input::contains_visual_spoofing)
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
            || crate::review_input::contains_visual_spoofing(&text)
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
fn open_journal_directory(dir: &Path) -> io::Result<File> {
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
    let mode = metadata.permissions().mode();
    // The sticky bit does not constrain the directory owner. Accept a shared
    // namespace only when it is owned by us or by root (for example `/tmp`).
    // SAFETY: geteuid has no preconditions and only reads process state.
    let effective_uid = unsafe { libc::geteuid() };
    if metadata.uid() != effective_uid && metadata.uid() != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "execution journal parent {} is not owned by the current user or root",
                dir.display()
            ),
        ));
    }
    // A sticky shared directory such as /tmp protects entries by owner. Other
    // group/world-writable parents permit a different uid to replace the lock
    // or journal pathname between otherwise safe descriptor operations.
    if mode & 0o022 != 0 && mode & libc::S_ISVTX == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "execution journal parent {} is group/world writable without the sticky bit",
                dir.display()
            ),
        ));
    }
    Ok(directory)
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
    // Read access lets the locked writer inspect the final byte and separate a
    // torn shell event before appending a terminal-owned output event.
    options.create(true).read(true).append(true);
    harden_open_options(&mut options);
    let file = options.open(path)?;
    validate_journal_file(&file, "execution journal")?;
    Ok(file)
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
    fn acquire(dir: &Path, lock_path: &Path, mode: JournalLockMode) -> io::Result<Self> {
        Self::acquire_with_timeout(dir, lock_path, mode, JOURNAL_LOCK_TIMEOUT)
    }

    fn acquire_with_timeout(
        dir: &Path,
        lock_path: &Path,
        mode: JournalLockMode,
        timeout: Duration,
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
            let directory = open_journal_directory(dir)?;
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
    let (path, _) = journal_path()?;
    match fs::symlink_metadata(&path) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    }
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "journal has no parent"))?;
    let lock_path = dir.join("executions.lock");
    let _lock = JournalFileLock::acquire(dir, &lock_path, JournalLockMode::Shared)?;

    match read_session_history_file(&path, session_id) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        result => result,
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
            "jsh execution journal exceeds its bounded size",
        ));
    }

    let mut records = HashMap::<String, PersistedExecution>::new();
    // Keep the working set bounded while folding a journal containing many
    // tiny start records. The file byte cap alone does not bound HashMap
    // overhead. This index mirrors the final ordering and lets us evict the
    // oldest record in logarithmic time.
    let mut record_order = BTreeMap::<(u64, u64, String), String>::new();
    let read_limit = MAX_JOURNAL_READ_BYTES.saturating_add(1);
    let mut reader = BufReader::new(file.take(read_limit));
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
                if !valid_jsh_execution_id(&id)
                    || event_session_id
                        .as_deref()
                        .is_some_and(|id| !valid_jsh_session_id(id))
                    || !is_valid_jsh_command(&command)
                    || !is_valid_jsh_cwd(&cwd)
                {
                    continue;
                }
                // A later duplicate start is authoritative, including when it
                // moves an ID out of the requested session.
                if let Some(previous) = records.remove(&id) {
                    record_order.remove(&(
                        previous.started_at_ms,
                        previous.seq,
                        previous.id.clone(),
                    ));
                }
                if event_session_id.as_deref() != Some(session_id) {
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
                record_order.insert((started_at_ms, seq, id.clone()), id.clone());
                records.insert(id, record);
                while records.len() > MAX_RETAINED_EXECUTIONS {
                    let Some((_, oldest_id)) = record_order.pop_first() else {
                        break;
                    };
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
                if !valid_jsh_execution_id(&id) || !is_valid_jsh_cwd(&cwd_after) {
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
                if !valid_jsh_execution_id(&id) || text.len() > MAX_OUTPUT_BYTES {
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

    let bytes_read = read_limit.saturating_sub(reader.get_ref().limit());
    if bytes_read > MAX_JOURNAL_READ_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "jsh execution journal grew beyond its bounded size while reading",
        ));
    }

    let mut records = records.into_values().collect::<Vec<_>>();
    records.sort_by(|left, right| {
        (left.started_at_ms, left.seq, &left.id).cmp(&(right.started_at_ms, right.seq, &right.id))
    });
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
        use std::os::unix::fs::{DirBuilderExt, MetadataExt};

        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        let directory = open_journal_directory(dir)?;
        if !custom_path || !dir_already_existed {
            let metadata = directory.metadata()?;
            // SAFETY: geteuid has no preconditions and only reads process state.
            if metadata.uid() != unsafe { libc::geteuid() } {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "execution journal directory {} is not owned by the current user",
                        dir.display()
                    ),
                ));
            }
            directory.set_permissions(fs::Permissions::from_mode(0o700))?;
        }
    }
    #[cfg(not(unix))]
    fs::create_dir_all(dir)?;
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

    let lock = JournalFileLock::acquire(dir, &lock_path, JournalLockMode::Exclusive)?;

    (|| {
        let mut journal = open_journal_for_append(journal_path)?;
        let current_len = journal.metadata()?.len();
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
        #[cfg(unix)]
        journal.set_permissions(fs::Permissions::from_mode(0o600))?;
        if needs_separator {
            // Match jsh's lifecycle writer: a crash may leave one incomplete
            // JSON object, but it must not consume the next complete event.
            journal.write_all(b"\n")?;
        }
        journal.write_all(encoded)?;
        journal.sync_data()?;
        lock.sync_directory()
    })()
}

fn journal_append_within_bound(current_bytes: u64, event_bytes: usize) -> bool {
    current_bytes.saturating_add(u64::try_from(event_bytes).unwrap_or(u64::MAX))
        <= MAX_JOURNAL_READ_BYTES
}

/// Match jsh's public execution-id grammar. Generic FinalTerm producers still
/// get the in-memory timeline, but their unrelated IDs must not add orphan
/// output events to jsh's journal.
fn valid_jsh_execution_id(id: &str) -> bool {
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
fn encode_event(mut event: OutputEvent) -> io::Result<Vec<u8>> {
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
    Ok(encoded)
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
    fn only_jsh_compatible_execution_ids_are_persisted() {
        for id in ["local:1", "jsh:1", "contains space", "雪"] {
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
            id: "jsh-a_b.c-1".to_owned(),
            output: "output".to_owned(),
            output_available: true,
            truncated: false,
            total_bytes: 6,
        };
        assert!(OutputEvent::from_completed(valid).is_some());
    }

    #[test]
    fn output_event_matches_jsh_envelope() {
        let completed = CompletedExecution {
            id: "exec-1".to_owned(),
            output: "hi".to_owned(),
            output_available: true,
            truncated: false,
            total_bytes: 2,
        };
        let value = serde_json::to_value(OutputEvent::from_completed(completed).unwrap()).unwrap();
        assert_eq!(value["jsh_execution_version"], 1);
        assert_eq!(value["event"], "output");
        assert_eq!(value["id"], "exec-1");
        assert_eq!(value["text"], "hi");
        assert_eq!(value["total_bytes"], 2);
        assert!(value.get("command").is_none());
    }

    #[test]
    fn oversized_output_is_bounded_before_it_enters_the_writer_queue() {
        let output = "界".repeat(MAX_OUTPUT_BYTES);
        let observed = output.len();
        let event = OutputEvent::from_completed(CompletedExecution {
            id: "exec-large".to_owned(),
            output,
            output_available: true,
            truncated: false,
            total_bytes: 0,
        })
        .unwrap();
        assert!(event.text.len() <= MAX_OUTPUT_BYTES);
        assert!(event.truncated);
        assert_eq!(event.total_bytes, observed as u64);
    }

    #[test]
    fn control_heavy_output_stays_within_jshs_jsonl_limit() {
        let output = "\0".repeat(MAX_OUTPUT_BYTES);
        let total_bytes = output.len();
        let completed = CompletedExecution {
            id: "jsh-control-heavy".to_owned(),
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
        assert!(value["text"].as_str().unwrap().len() <= MAX_OUTPUT_BYTES / 2);
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
        assert_eq!(output.total_bytes, 2);
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
        assert!(validate_journal_path(Path::new("bad\nname.jsonl")).is_err());
        assert!(validate_journal_path(Path::new("bad\u{202e}name.jsonl")).is_err());
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
        let held =
            JournalFileLock::acquire(&root.0, &lock_path, JournalLockMode::Exclusive).unwrap();
        fs::rename(&lock_path, &retired_path).unwrap();

        let started = Instant::now();
        let error = JournalFileLock::acquire_with_timeout(
            &root.0,
            &lock_path,
            JournalLockMode::Exclusive,
            Duration::from_millis(25),
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
        let held =
            JournalFileLock::acquire(&root.0, &lock_path, JournalLockMode::Exclusive).unwrap();
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

        assert!(read_session_history_file(&journal_path, "wanted").is_err());
        assert!(append_encoded_event_to_path(&journal_path, b"event\n").is_err());
        assert_eq!(fs::read_to_string(journal_path).unwrap(), "event\n");
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
