//! Lightweight, privacy-conscious command history.
//!
//! Full block snapshots are optional because they contain terminal output.
//! This JSONL index stores only the command, cwd, exit status, and completion
//! time so History, the command palette, and opt-in AI context work out of the
//! box without persisting command output.

use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

const COMPACT_EVERY: u64 = 128;
const MAX_FILE_BYTES: usize = 32 * 1024 * 1024;
/// Maximum size of one complete physical JSONL record, including its trailing
/// newline. Writers, readers, and compaction must all measure the same bytes.
const MAX_RECORD_BYTES: usize = 1024 * 1024;
/// Commands are later inserted into an interactive prompt for review, so the
/// persistence contract must match that boundary exactly.
const MAX_COMMAND_BYTES: usize = crate::review_input::MAX_REVIEW_INPUT_BYTES;
/// A cwd is display/context metadata, not a bulk payload. This is generous
/// compared with platform path limits while bounding JSON and UI allocation.
const MAX_CWD_BYTES: usize = 16 * 1024;
const MAX_HISTORY_PATH_BYTES: usize = 16 * 1024;
/// Interactive history consumers only need a recent working set. Keep their
/// synchronous read bounded even when the on-disk history has reached its
/// 32 MiB compaction threshold.
const READ_RECENT_TAIL_BYTES: u64 = 4 * 1024 * 1024;
const LOCK_TIMEOUT: Duration = Duration::from_secs(2);
const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(5);
const TEMP_FILE_ATTEMPTS: usize = 128;
const WRITER_QUEUE_CAPACITY: usize = 256;
static APPEND_COUNT: AtomicU64 = AtomicU64::new(0);
static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);
static HISTORY_WRITER: OnceLock<Result<mpsc::SyncSender<WriterMessage>, String>> = OnceLock::new();

fn validate_history_path(path: &Path) -> io::Result<()> {
    if path.file_name().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "command-history path has no file name",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let bytes = path.as_os_str().as_bytes();
        if bytes.len() > MAX_HISTORY_PATH_BYTES
            || bytes.iter().any(|byte| matches!(*byte, 0..=0x1f | 0x7f))
            || path
                .to_str()
                .is_some_and(crate::review_input::contains_visual_spoofing)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "command-history path is too long or contains unsafe display bytes",
            ));
        }
    }
    #[cfg(not(unix))]
    {
        let text = path.to_string_lossy();
        if text.len() > MAX_HISTORY_PATH_BYTES
            || text.chars().any(char::is_control)
            || crate::review_input::contains_visual_spoofing(&text)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "command-history path is too long or contains unsafe display text",
            ));
        }
    }
    Ok(())
}

fn next_temp_id() -> u64 {
    let sequence = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    #[cfg(target_os = "linux")]
    {
        let mut random = [0_u8; std::mem::size_of::<u64>()];
        // SAFETY: random is a writable buffer of exactly the supplied length;
        // nonblocking entropy failure falls through to the collision-safe
        // monotonic fallback below.
        let read = unsafe {
            libc::getrandom(
                random.as_mut_ptr().cast(),
                random.len(),
                libc::GRND_NONBLOCK,
            )
        };
        if read == random.len() as isize {
            return u64::from_ne_bytes(random) ^ sequence;
        }
    }
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    timestamp ^ sequence.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ u64::from(std::process::id())
}

fn sibling_path(path: &Path, suffix: &str) -> io::Result<PathBuf> {
    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("command history path has no file name: {}", path.display()),
        )
    })?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut sibling_name = file_name.to_os_string();
    sibling_name.push(suffix);
    Ok(parent.join(sibling_name))
}

fn lock_path_for(path: &Path) -> io::Result<PathBuf> {
    sibling_path(path, ".lock")
}

fn history_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

#[cfg(unix)]
fn harden_open_options(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;

    options
        .mode(0o600)
        // O_NONBLOCK is inert for regular files, but prevents a substituted
        // fifo or device from hanging a UI thread before fstat can reject it.
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC);
}

#[cfg(not(unix))]
fn harden_open_options(_options: &mut OpenOptions) {}

fn validate_history_file(file: &File, description: &str) -> io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{description} is not a regular file"),
        ));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        // A second hard link would let an attacker make our append or chmod
        // affect a file reached under another name. History is private state,
        // so there is no legitimate reason for it to be multiply linked.
        if metadata.nlink() != 1 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{description} must have exactly one hard link"),
            ));
        }

        // SAFETY: geteuid has no preconditions and only reads process state.
        let effective_uid = unsafe { libc::geteuid() };
        if metadata.uid() != effective_uid {
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

fn open_lock_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    harden_open_options(&mut options);
    let file = options.open(path)?;
    validate_history_file(&file, "command-history lock")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

#[cfg(unix)]
fn open_history_directory(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let parent = history_parent(path);
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(
            libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        );
    }
    let directory = options.open(parent)?;
    let metadata = directory.metadata()?;
    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "command-history parent {} is not a directory",
                parent.display()
            ),
        ));
    }
    let mode = metadata.permissions().mode();
    // A sticky bit protects an entry from unrelated directory users, but not
    // from the directory owner. Trust only our own namespace or a root-owned
    // shared namespace such as `/tmp`.
    // SAFETY: geteuid has no preconditions and only reads process state.
    let effective_uid = unsafe { libc::geteuid() };
    if metadata.uid() != effective_uid && metadata.uid() != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "command-history parent {} is not owned by the current user or root",
                parent.display()
            ),
        ));
    }
    if mode & 0o022 != 0 && mode & libc::S_ISVTX == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "command-history parent {} is group/world writable without the sticky bit",
                parent.display()
            ),
        ));
    }
    Ok(directory)
}

fn open_history_for_append(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    harden_open_options(&mut options);
    let file = options.open(path)?;
    validate_history_file(&file, "command-history file")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

fn open_history_for_read(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    harden_open_options(&mut options);
    let file = options.open(path)?;
    validate_history_file(&file, "command-history file")?;
    Ok(file)
}

/// Validate the configured command-history path before the first read or
/// write of a session.
///
/// The immediate parent must be a directory owned by this user and not
/// writable by group or other — stricter than the append path's own check,
/// which also admits a root-owned sticky shared namespace, because a
/// configured history location is application state, not a spool. A missing
/// parent is created private (0700) for a writer and left alone for a
/// reader. Existing history and lock entries are descriptor-checked without
/// following links or blocking on FIFOs; a writer tightens a lax mode to
/// 0600 while a reader rejects it.
pub fn prepare_path(path: &Path, for_write: bool) -> io::Result<()> {
    validate_history_path(path)?;
    let parent = history_parent(path);
    match fs::symlink_metadata(parent) {
        Ok(_) => drop(open_owned_history_parent(parent)?),
        Err(error) if error.kind() == io::ErrorKind::NotFound && for_write => {
            create_private_history_parent(parent)?;
            drop(open_owned_history_parent(parent)?);
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    }
    prepare_optional_entry(path, for_write)?;
    prepare_optional_entry(&lock_path_for(path)?, for_write)
}

#[cfg(unix)]
fn open_owned_history_parent(parent: &Path) -> io::Result<File> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

    let mut options = OpenOptions::new();
    options.read(true);
    options.custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let directory = options.open(parent)?;
    let metadata = directory.metadata()?;
    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "command-history parent {} is not a directory",
                parent.display()
            ),
        ));
    }
    // SAFETY: geteuid has no preconditions and only reads process state.
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "command-history parent {} is not owned by the current user",
                parent.display()
            ),
        ));
    }
    if metadata.permissions().mode() & 0o022 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "command-history parent {} must not be group- or world-writable",
                parent.display()
            ),
        ));
    }
    Ok(directory)
}

#[cfg(not(unix))]
fn open_owned_history_parent(parent: &Path) -> io::Result<File> {
    if fs::metadata(parent)?.is_dir() {
        fs::File::open(parent)
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "command-history parent {} is not a directory",
                parent.display()
            ),
        ))
    }
}

fn create_private_history_parent(parent: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};

        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
        let mut options = OpenOptions::new();
        options.read(true);
        options.custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
        let directory = options.open(parent)?;
        let metadata = directory.metadata()?;
        // SAFETY: geteuid has no preconditions and only reads process state.
        if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "command-history parent {} is not a private directory we own",
                    parent.display()
                ),
            ));
        }
        use std::os::unix::fs::PermissionsExt;
        directory.set_permissions(fs::Permissions::from_mode(0o700))
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(parent)
    }
}

fn prepare_optional_entry(path: &Path, for_write: bool) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.read(true).write(for_write);
    harden_open_options(&mut options);
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is not a regular history file", path.display()),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        // SAFETY: geteuid has no preconditions and only reads process state.
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.nlink() != 1 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{} must be owned by the current user and have exactly one hard link",
                    path.display()
                ),
            ));
        }
        let mode = metadata.permissions().mode();
        if mode & 0o022 != 0 {
            if for_write {
                file.set_permissions(fs::Permissions::from_mode(0o600))?;
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("{} must not be group- or world-writable", path.display()),
                ));
            }
        } else if for_write && mode & 0o077 != 0 {
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn try_lock_exclusive(file: &File) -> io::Result<bool> {
    // SAFETY: `file` owns a live descriptor for this call. flock retains no
    // pointer and the descriptor remains owned by HistoryFileLock on success.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
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
fn try_lock_exclusive(_file: &File) -> io::Result<bool> {
    Ok(true)
}

#[cfg(unix)]
fn unlock(file: &File) {
    // SAFETY: the descriptor remains live until HistoryFileLock is dropped.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) } != 0 {
        log::warn!(
            "failed to release command-history lock: {}",
            io::Error::last_os_error()
        );
    }
}

#[cfg(not(unix))]
fn unlock(_file: &File) {}

struct HistoryFileLock {
    #[cfg(unix)]
    directory: File,
    file: File,
}

impl HistoryFileLock {
    fn acquire(history_path: &Path, timeout: Duration) -> io::Result<Self> {
        let path = lock_path_for(history_path)?;
        let started = Instant::now();
        let wait = |file: &File| -> io::Result<()> {
            loop {
                match try_lock_exclusive(file)? {
                    true => return Ok(()),
                    false if started.elapsed() < timeout => thread::sleep(LOCK_POLL_INTERVAL),
                    false => {
                        return Err(io::Error::new(
                            io::ErrorKind::WouldBlock,
                            format!(
                                "timed out waiting for command-history lock {}",
                                path.display()
                            ),
                        ));
                    }
                }
            }
        };

        // The directory lock stabilizes the sidecar pathname from before it is
        // opened until after its flock is acquired. Without it, a cooperating
        // process could rename the persistent sidecar while it was locked and
        // then acquire a fresh inode under the original name.
        #[cfg(unix)]
        let directory = {
            let directory = open_history_directory(history_path)?;
            wait(&directory)?;
            directory
        };
        // The lock file is deliberately persistent. flock state lives on the
        // open descriptor, so a stale empty sidecar after a crash is harmless.
        let file = match open_lock_file(&path) {
            Ok(file) => file,
            Err(error) => {
                #[cfg(unix)]
                unlock(&directory);
                return Err(error);
            }
        };
        if let Err(error) = wait(&file) {
            #[cfg(unix)]
            unlock(&directory);
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

impl Drop for HistoryFileLock {
    fn drop(&mut self) {
        unlock(&self.file);
        #[cfg(unix)]
        unlock(&self.directory);
    }
}

fn create_unique_temp_file(target: &Path) -> io::Result<(File, PathBuf)> {
    let file_name = target.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "command history path has no file name: {}",
                target.display()
            ),
        )
    })?;
    let parent = history_parent(target);

    for _ in 0..TEMP_FILE_ATTEMPTS {
        let id = next_temp_id();
        let mut temp_name = OsString::from(".");
        temp_name.push(file_name);
        temp_name.push(format!(".tmp-{}-{id}", std::process::id()));
        let temp_path = parent.join(temp_name);

        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        match options.open(&temp_path) {
            Ok(file) => return Ok((file, temp_path)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!(
            "could not allocate a unique command-history temp file beside {}",
            target.display()
        ),
    ))
}

#[cfg(unix)]
fn sync_parent_directory(path: &Path) -> io::Result<()> {
    let parent = history_parent(path);
    File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct CommandHistoryRecord {
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub exit_code: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_time_ms: Option<u64>,
}

fn validate_record_fields(command: &str, cwd: Option<&str>) -> io::Result<()> {
    crate::review_input::validate(command).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("command is unsafe for review-only history: {error}"),
        )
    })?;
    debug_assert!(command.len() <= MAX_COMMAND_BYTES);

    if let Some(cwd) = cwd {
        if cwd.len() > MAX_CWD_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("command-history cwd exceeds {MAX_CWD_BYTES} bytes"),
            ));
        }
        if cwd.chars().any(char::is_control) || crate::review_input::contains_visual_spoofing(cwd) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "command-history cwd contains a control or invisible formatting character",
            ));
        }
    }
    Ok(())
}

fn encode_record(record: &CommandHistoryRecord) -> io::Result<Vec<u8>> {
    validate_record_fields(&record.command, record.cwd.as_deref())?;
    let mut encoded = serde_json::to_vec(record).map_err(io::Error::other)?;
    if encoded
        .len()
        .checked_add(1)
        .is_none_or(|physical_len| physical_len > MAX_RECORD_BYTES)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "command history record exceeds 1 MiB",
        ));
    }
    encoded.push(b'\n');
    Ok(encoded)
}

struct AppendRequest {
    path: PathBuf,
    max_entries: usize,
    command: String,
    cwd: Option<String>,
    exit_code: i32,
    end_time_ms: Option<u64>,
}

enum WriterMessage {
    Append(AppendRequest),
    Flush(mpsc::SyncSender<io::Result<()>>),
}

fn run_history_writer(receiver: mpsc::Receiver<WriterMessage>) {
    let mut pending_error: Option<(io::ErrorKind, String)> = None;
    while let Ok(message) = receiver.recv() {
        match message {
            WriterMessage::Append(request) => {
                if let Err(error) = append(
                    &request.path,
                    request.max_entries,
                    &request.command,
                    request.cwd.as_deref(),
                    request.exit_code,
                    request.end_time_ms,
                ) {
                    log::warn!("command history: {error}");
                    // Preserve the first failure in this flush generation: it
                    // is normally the most useful root cause, while retaining
                    // only bounded metadata no matter how many writes fail.
                    pending_error.get_or_insert_with(|| (error.kind(), error.to_string()));
                }
            }
            WriterMessage::Flush(acknowledge) => {
                let result = pending_error
                    .take()
                    .map_or(Ok(()), |(kind, message)| Err(io::Error::new(kind, message)));
                let _ = acknowledge.send(result);
            }
        }
    }
}

fn history_writer() -> io::Result<&'static mpsc::SyncSender<WriterMessage>> {
    let result = HISTORY_WRITER.get_or_init(|| {
        let (sender, receiver) = mpsc::sync_channel(WRITER_QUEUE_CAPACITY);
        thread::Builder::new()
            .name("jterm-command-history".to_string())
            .spawn(move || run_history_writer(receiver))
            .map(|_| sender)
            .map_err(|error| error.to_string())
    });
    result
        .as_ref()
        .map_err(|error| io::Error::other(error.clone()))
}

/// Queue a history write without waiting on filesystem locks or compaction in
/// GTK's main thread. A bounded queue protects memory if storage stalls.
pub fn enqueue(
    path: &Path,
    max_entries: usize,
    command: &str,
    cwd: Option<&str>,
    exit_code: i32,
    end_time_ms: Option<u64>,
) -> io::Result<()> {
    if command.trim().is_empty() {
        return Ok(());
    }
    validate_history_path(path)?;
    validate_record_fields(command, cwd)?;

    let record = CommandHistoryRecord {
        command: command.to_string(),
        cwd: cwd.map(str::to_string),
        exit_code,
        end_time_ms,
    };
    // Reject records that can never be persisted before they consume a slot in
    // the bounded worker queue. `append` repeats this at the filesystem boundary
    // so direct callers and future request producers get the identical check.
    encode_record(&record)?;
    let request = AppendRequest {
        path: path.to_path_buf(),
        max_entries,
        command: record.command,
        cwd: record.cwd,
        exit_code: record.exit_code,
        end_time_ms: record.end_time_ms,
    };
    history_writer()?
        .try_send(WriterMessage::Append(request))
        .map_err(|error| match error {
            mpsc::TrySendError::Full(_) => io::Error::new(
                io::ErrorKind::WouldBlock,
                "command-history writer queue is full",
            ),
            mpsc::TrySendError::Disconnected(_) => {
                io::Error::new(io::ErrorKind::BrokenPipe, "command-history writer stopped")
            }
        })
}

/// Wait until every history write accepted before this call has completed.
pub fn flush_pending(timeout: Duration) -> io::Result<()> {
    let Some(result) = HISTORY_WRITER.get() else {
        return Ok(());
    };
    let sender = result
        .as_ref()
        .map_err(|error| io::Error::other(error.clone()))?;
    let (acknowledge, received) = mpsc::sync_channel(0);
    let started = Instant::now();
    let mut message = WriterMessage::Flush(acknowledge);

    loop {
        match sender.try_send(message) {
            Ok(()) => break,
            Err(mpsc::TrySendError::Full(returned)) if started.elapsed() < timeout => {
                message = returned;
                thread::sleep(LOCK_POLL_INTERVAL);
            }
            Err(mpsc::TrySendError::Full(_)) => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out queueing command-history flush",
                ));
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "command-history writer stopped",
                ));
            }
        }
    }

    let remaining = timeout.saturating_sub(started.elapsed());
    received
        .recv_timeout(remaining)
        .map_err(|error| match error {
            mpsc::RecvTimeoutError::Timeout => io::Error::new(
                io::ErrorKind::TimedOut,
                "timed out flushing command history",
            ),
            mpsc::RecvTimeoutError::Disconnected => {
                io::Error::new(io::ErrorKind::BrokenPipe, "command-history writer stopped")
            }
        })?
}

pub fn append(
    path: &Path,
    max_entries: usize,
    command: &str,
    cwd: Option<&str>,
    exit_code: i32,
    end_time_ms: Option<u64>,
) -> io::Result<()> {
    if command.trim().is_empty() {
        return Ok(());
    }
    validate_history_path(path)?;
    validate_record_fields(command, cwd)?;
    let record = CommandHistoryRecord {
        command: command.to_string(),
        cwd: cwd.map(str::to_string),
        exit_code,
        end_time_ms,
    };
    // Keep one physical JSONL record in one write_all call. The flock below is
    // the cross-process consistency boundary; combining the newline also keeps
    // readers safe if a future caller writes without sharing this process.
    let encoded = encode_record(&record)?;

    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)?;
        }
        #[cfg(not(unix))]
        fs::create_dir_all(parent)?;
    }

    let lock = HistoryFileLock::acquire(path, LOCK_TIMEOUT)?;
    let mut file = open_history_for_append(path)?;
    file.write_all(&encoded)?;
    // The writer runs off the UI thread, so make an acknowledged append mean
    // that both the record and a newly created directory entry reached stable
    // storage rather than only stdio buffers.
    file.sync_data()?;
    lock.sync_directory()?;

    let append_number = APPEND_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    let oversized = file.metadata()?.len() > MAX_FILE_BYTES as u64;
    drop(file);
    if oversized || append_number % COMPACT_EVERY == 0 {
        compact_locked(path, max_entries.max(1))?;
    }
    Ok(())
}

fn read_recent_from<R: Read + Seek>(
    input: &mut R,
    file_len: u64,
    max_entries: usize,
) -> io::Result<Vec<CommandHistoryRecord>> {
    if max_entries == 0 || file_len == 0 {
        return Ok(Vec::new());
    }

    let start = file_len.saturating_sub(READ_RECENT_TAIL_BYTES);
    let starts_at_line_boundary = if start == 0 {
        true
    } else {
        input.seek(SeekFrom::Start(start - 1))?;
        let mut previous = [0_u8; 1];
        input.read(&mut previous)? == 1 && previous[0] == b'\n'
    };

    input.seek(SeekFrom::Start(start))?;
    let tail_len = file_len - start;
    let mut tail = Vec::with_capacity(tail_len as usize);
    input.take(tail_len).read_to_end(&mut tail)?;
    if tail.is_empty() {
        return Ok(Vec::new());
    }

    // A bounded tail normally starts in the middle of a physical JSONL line.
    // Drop that fragment unless the byte immediately before the window was a
    // newline. Likewise, ignore an unterminated final line: another process
    // may still be appending it.
    let first_complete = if starts_at_line_boundary {
        0
    } else {
        let Some(newline) = tail.iter().position(|byte| *byte == b'\n') else {
            return Ok(Vec::new());
        };
        newline + 1
    };
    let Some(last_newline) = tail.iter().rposition(|byte| *byte == b'\n') else {
        return Ok(Vec::new());
    };
    if first_complete >= last_newline {
        return Ok(Vec::new());
    }

    let mut seen = HashSet::new();
    let mut records = Vec::with_capacity(max_entries.min(256));
    for line in tail[first_complete..last_newline]
        .rsplit(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        // Match append/read's physical-record limit, including the newline
        // excluded by `rsplit`.
        if line.len().saturating_add(1) > MAX_RECORD_BYTES {
            continue;
        }
        let Ok(record) = serde_json::from_slice::<CommandHistoryRecord>(line) else {
            continue;
        };
        if validate_record_fields(&record.command, record.cwd.as_deref()).is_err()
            || !seen.insert(record.command.clone())
        {
            continue;
        }
        records.push(record);
        if records.len() == max_entries {
            break;
        }
    }
    Ok(records)
}

/// Recent history plus whether the bounded tail window skipped older bytes.
pub struct RecentHistory {
    pub records: Vec<CommandHistoryRecord>,
    /// True when older bytes existed outside the synchronous 4 MiB tail
    /// window. Consumers must not describe a short result as the complete
    /// history then.
    pub tail_truncated: bool,
}

/// Read newest-first from a bounded tail window, deduplicating commands while
/// retaining newest metadata and reporting whether older bytes were skipped.
/// Corrupt, oversized, incomplete, and unsafe review-only records are ignored.
pub fn read_recent_with_status(path: &Path, max_entries: usize) -> io::Result<RecentHistory> {
    validate_history_path(path)?;
    let mut input = open_history_for_read(path)?;
    let file_len = input.metadata()?.len();
    let records = read_recent_from(&mut input, file_len, max_entries)?;
    Ok(RecentHistory {
        records,
        tail_truncated: file_len > READ_RECENT_TAIL_BYTES,
    })
}

/// Read newest-first from a bounded tail window, deduplicating commands while
/// retaining newest metadata. Corrupt, oversized, incomplete, and unsafe
/// review-only records are ignored.
pub fn read_recent(path: &Path, max_entries: usize) -> io::Result<Vec<CommandHistoryRecord>> {
    read_recent_with_status(path, max_entries).map(|recent| recent.records)
}

#[cfg(test)]
fn compact(path: &Path, max_entries: usize) -> io::Result<()> {
    validate_history_path(path)?;
    let _lock = HistoryFileLock::acquire(path, LOCK_TIMEOUT)?;
    compact_locked(path, max_entries.max(1))
}

/// Consume the remainder of one physical line without allocating storage
/// proportional to its length. `BufRead::read_until` appends every skipped byte
/// to its output buffer, so using it for a corrupt multi-gigabyte line could
/// exhaust the process even though command-history records are size-bounded.
fn discard_through_newline<R: BufRead>(reader: &mut R) -> io::Result<()> {
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(());
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(available.len(), |index| index + 1);
        reader.consume(consumed);
        if newline.is_some() {
            return Ok(());
        }
    }
}

/// Compact while the caller owns the history sidecar lock. Keeping append,
/// scan, temp write, and rename in one critical section prevents a second
/// anvil process from appending to an inode that is about to be replaced.
fn compact_locked(path: &Path, max_entries: usize) -> io::Result<()> {
    compact_locked_with_budget(path, max_entries, MAX_FILE_BYTES)
}

fn compact_locked_with_budget(
    path: &Path,
    max_entries: usize,
    max_file_bytes: usize,
) -> io::Result<()> {
    let input = open_history_for_read(path)?;
    let mut reader = BufReader::new(input);
    let mut recent = VecDeque::with_capacity(max_entries.min(16_384));
    let mut recent_bytes = 0usize;
    let mut line = String::new();
    loop {
        line.clear();
        let bytes = reader
            .by_ref()
            .take((MAX_RECORD_BYTES + 1) as u64)
            .read_line(&mut line)?;
        if bytes == 0 {
            break;
        }
        if bytes > MAX_RECORD_BYTES || !line.ends_with('\n') {
            // Finish consuming a corrupt/oversized physical line before
            // looking for the next valid record.
            if !line.ends_with('\n') {
                discard_through_newline(&mut reader)?;
            }
            continue;
        }
        let Ok(record) = serde_json::from_str::<CommandHistoryRecord>(line.trim_end()) else {
            continue;
        };
        if validate_record_fields(&record.command, record.cwd.as_deref()).is_err() {
            continue;
        }
        recent_bytes = recent_bytes.saturating_add(line.len());
        recent.push_back(line.clone());
        // The deque is oldest-to-newest. Evicting from its front preserves the
        // newest records while keeping their original on-disk order. Enforce
        // both contracts together so a history with a few very large valid
        // records cannot remain above MAX_FILE_BYTES after every compaction.
        while recent.len() > max_entries || recent_bytes > max_file_bytes {
            let Some(evicted) = recent.pop_front() else {
                break;
            };
            recent_bytes = recent_bytes.saturating_sub(evicted.len());
        }
    }

    let (output, temp_path) = create_unique_temp_file(path)?;
    let result = (|| {
        {
            let mut writer = BufWriter::new(output);
            for record in recent {
                writer.write_all(record.as_bytes())?;
            }
            writer.flush()?;
            writer.get_ref().sync_all()?;
        }
        fs::rename(&temp_path, path)?;
        sync_parent_directory(path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::process::{Command, Stdio};

    const CHILD_PATH_ENV: &str = "ANVIL_HISTORY_TEST_CHILD_PATH";
    const CHILD_PREFIX_ENV: &str = "ANVIL_HISTORY_TEST_CHILD_PREFIX";
    const CHILD_COUNT_ENV: &str = "ANVIL_HISTORY_TEST_CHILD_COUNT";
    static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

    fn temp_path(name: &str) -> std::path::PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "jterm-command-history-{name}-{}-{}",
            std::process::id(),
            NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        }
        directory.join("history.jsonl")
    }

    fn cleanup(path: &Path) {
        if let Some(parent) = path.parent() {
            let _ = fs::remove_dir_all(parent);
        }
    }

    fn write_test_history(path: &Path, contents: impl AsRef<[u8]>) {
        fs::write(path, contents).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    #[test]
    fn history_paths_are_bounded_before_entering_the_writer_queue() {
        assert!(validate_history_path(Path::new("history.jsonl")).is_ok());
        assert!(validate_history_path(Path::new("bad\nname.jsonl")).is_err());
        assert!(validate_history_path(Path::new("bad\u{202e}name.jsonl")).is_err());
        let oversized = PathBuf::from("x".repeat(MAX_HISTORY_PATH_BYTES + 1));
        assert!(validate_history_path(&oversized).is_err());
    }

    struct CountingCursor {
        inner: Cursor<Vec<u8>>,
        bytes_read: usize,
    }

    impl std::io::Read for CountingCursor {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let read = self.inner.read(buffer)?;
            self.bytes_read += read;
            Ok(read)
        }
    }

    impl std::io::Seek for CountingCursor {
        fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
            self.inner.seek(position)
        }
    }

    #[test]
    fn append_writes_palette_compatible_jsonl() {
        let path = temp_path("append");
        append(&path, 100, "cargo test", Some("/tmp/project"), 0, Some(42)).unwrap();
        let records = read_recent(&path, 10).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].command, "cargo test");
        assert_eq!(records[0].cwd.as_deref(), Some("/tmp/project"));
        assert_eq!(records[0].exit_code, 0);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        cleanup(&path);
    }

    #[test]
    fn field_limits_round_trip_with_headroom_under_the_physical_record_limit() {
        let record = CommandHistoryRecord {
            command: "x".repeat(MAX_COMMAND_BYTES),
            cwd: Some("y".repeat(MAX_CWD_BYTES)),
            exit_code: 0,
            end_time_ms: None,
        };
        let encoded = encode_record(&record).expect("records at every field limit are valid");
        assert!(encoded.len() < MAX_RECORD_BYTES);
        assert_eq!(encoded.last(), Some(&b'\n'));

        let mut input = Cursor::new(encoded.clone());
        assert_eq!(
            read_recent_from(&mut input, encoded.len() as u64, 1).unwrap(),
            vec![record.clone()]
        );

        let mut oversized_command = record.clone();
        oversized_command.command.push('x');
        assert_eq!(
            encode_record(&oversized_command).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        let mut oversized_cwd = record;
        oversized_cwd.cwd.as_mut().unwrap().push('y');
        assert_eq!(
            encode_record(&oversized_cwd).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn read_recent_deduplicates_and_skips_corruption() {
        let path = temp_path("read");
        write_test_history(
            &path,
            "{\"command\":\"one\",\"exit_code\":1}\nnot-json\n{\"command\":\"two\",\"exit_code\":0}\n{\"command\":\"one\",\"exit_code\":0}\n",
        );
        let records = read_recent(&path, 2).unwrap();
        assert_eq!(
            records
                .iter()
                .map(|record| record.command.as_str())
                .collect::<Vec<_>>(),
            vec!["one", "two"]
        );
        assert_eq!(records[0].exit_code, 0);
        cleanup(&path);
    }

    #[test]
    fn read_recent_reads_only_the_bounded_tail() {
        let mut contents = vec![b'x'; READ_RECENT_TAIL_BYTES as usize + 64 * 1024];
        contents.extend_from_slice(
            b"\n{\"command\":\"older\",\"exit_code\":0}\n{\"command\":\"newer\",\"exit_code\":0}\n",
        );
        let file_len = contents.len() as u64;
        let mut input = CountingCursor {
            inner: Cursor::new(contents),
            bytes_read: 0,
        };

        let records = read_recent_from(&mut input, file_len, 10).unwrap();
        assert_eq!(
            records
                .iter()
                .map(|record| record.command.as_str())
                .collect::<Vec<_>>(),
            vec!["newer", "older"]
        );
        assert!(
            input.bytes_read <= READ_RECENT_TAIL_BYTES as usize + 1,
            "read {} bytes for a {}-byte tail budget",
            input.bytes_read,
            READ_RECENT_TAIL_BYTES
        );
    }

    #[test]
    fn read_recent_with_status_reports_a_skipped_older_prefix() {
        let path = temp_path("read-status");
        write_test_history(
            &path,
            "{\"command\":\"one\",\"exit_code\":0}\n{\"command\":\"two\",\"exit_code\":1}\n",
        );
        let recent = read_recent_with_status(&path, 10).unwrap();
        assert_eq!(recent.records.len(), 2);
        assert!(!recent.tail_truncated);
        cleanup(&path);

        let path = temp_path("read-status-big");
        let mut contents = vec![b'x'; READ_RECENT_TAIL_BYTES as usize + 64 * 1024];
        contents.extend_from_slice(b"\n{\"command\":\"newer\",\"exit_code\":0}\n");
        write_test_history(&path, &contents);
        let recent = read_recent_with_status(&path, 10).unwrap();
        assert_eq!(recent.records.len(), 1);
        assert!(
            recent.tail_truncated,
            "a file larger than the tail window must report skipped older bytes"
        );
        cleanup(&path);
    }

    #[test]
    fn read_recent_skips_oversized_invalid_and_incomplete_tail_records() {
        let path = temp_path("tail-corruption");
        let mut contents = b"{\"command\":\"older\",\"exit_code\":0}\n".to_vec();
        let oversized = CommandHistoryRecord {
            command: "x".repeat(MAX_RECORD_BYTES),
            cwd: None,
            exit_code: 0,
            end_time_ms: None,
        };
        contents.extend_from_slice(&serde_json::to_vec(&oversized).unwrap());
        contents.push(b'\n');
        contents.extend_from_slice(b"\xff\n");
        contents.extend_from_slice(b"{\"command\":\"newer\",\"exit_code\":0}\n");
        contents.extend_from_slice(b"{\"command\":\"still-being-written\"");
        write_test_history(&path, contents);

        let records = read_recent(&path, 10).unwrap();
        assert_eq!(
            records
                .iter()
                .map(|record| record.command.as_str())
                .collect::<Vec<_>>(),
            vec!["newer", "older"]
        );
        cleanup(&path);
    }

    #[test]
    fn unsafe_command_or_cwd_text_never_reaches_the_palette() {
        let path = temp_path("control");
        let error = append(&path, 100, "echo one\necho two", Some("/tmp"), 0, None).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        let error = append(
            &path,
            100,
            "safe",
            Some("/tmp/looks-safe\u{202e}txt"),
            0,
            None,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

        write_test_history(
            &path,
            "{\"command\":\"safe\",\"cwd\":\"/tmp\",\"exit_code\":0}\n{\"command\":\"unsafe cwd\",\"cwd\":\"/tmp/looks-safe\\u202etxt\",\"exit_code\":0}\n{\"command\":\"echo one\\necho two\",\"exit_code\":0}\n{\"command\":\"nul\\u0000byte\",\"exit_code\":0}\n",
        );
        let records = read_recent(&path, 10).unwrap();
        assert_eq!(
            records
                .iter()
                .map(|record| record.command.as_str())
                .collect::<Vec<_>>(),
            vec!["safe"]
        );
        cleanup(&path);
    }

    #[test]
    fn compact_keeps_only_recent_valid_records() {
        let path = temp_path("compact");
        write_test_history(
            &path,
            "{\"command\":\"one\",\"exit_code\":0}\nnot-json\n{\"command\":\"missing-status\"}\n{\"command\":\"echo one\\necho two\",\"exit_code\":0}\n{\"command\":\"two\",\"exit_code\":0}\n{\"command\":\"three\",\"exit_code\":0}\n",
        );
        compact(&path, 2).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("one"));
        assert!(!text.contains("not-json"));
        assert!(!text.contains("missing-status"));
        assert!(!text.contains("echo one"));
        assert!(text.contains("two"));
        assert!(text.contains("three"));
        cleanup(&path);
    }

    #[test]
    fn compact_keeps_newest_records_in_original_order_within_byte_budget() {
        let path = temp_path("compact-byte-budget");
        let records: Vec<Vec<u8>> = (0..8)
            .map(|index| {
                encode_record(&CommandHistoryRecord {
                    command: format!("record-{index}-{}", "x".repeat(index * 17 + 31)),
                    cwd: None,
                    exit_code: index as i32,
                    end_time_ms: None,
                })
                .unwrap()
            })
            .collect();
        let original: Vec<u8> = records.iter().flatten().copied().collect();
        write_test_history(&path, &original);

        let expected: Vec<u8> = records[5..].iter().flatten().copied().collect();
        let budget = expected.len();
        assert!(original.len() > budget);

        compact_locked_with_budget(&path, 100, budget).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), budget as u64);
        assert_eq!(fs::read(&path).unwrap(), expected);

        // Once compacted, an unchanged valid file is a fixed point: another
        // compaction neither evicts a newer record nor grows back over budget.
        let once = fs::read(&path).unwrap();
        compact_locked_with_budget(&path, 100, budget).unwrap();
        assert_eq!(fs::read(&path).unwrap(), once);
        cleanup(&path);
    }

    #[test]
    fn compact_streams_past_oversized_unterminated_record() {
        let path = temp_path("compact-oversized-unterminated");
        let oversized = vec![b'x'; MAX_RECORD_BYTES * 3 + 17];
        write_test_history(&path, oversized);

        compact(&path, 10).unwrap();

        assert_eq!(fs::metadata(&path).unwrap().len(), 0);
        cleanup(&path);
    }

    #[cfg(unix)]
    #[test]
    fn lock_file_is_private_and_contention_times_out() {
        use std::os::unix::fs::PermissionsExt;

        let path = temp_path("lock");
        let lock_path = lock_path_for(&path).unwrap();
        fs::write(&lock_path, b"").unwrap();
        fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o644)).unwrap();

        let held = HistoryFileLock::acquire(&path, LOCK_TIMEOUT).unwrap();
        assert_eq!(
            fs::metadata(&lock_path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let error = HistoryFileLock::acquire(&path, Duration::from_millis(30))
            .err()
            .expect("contended lock must time out");
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert!(error.to_string().contains("command-history lock"));

        drop(held);
        append(&path, 100, "after unlock", None, 0, None).unwrap();
        assert_eq!(read_recent(&path, 10).unwrap()[0].command, "after unlock");
        cleanup(&path);
    }

    #[cfg(unix)]
    #[test]
    fn directory_lock_prevents_sidecar_replacement_bypass() {
        let path = temp_path("lock-entry-replacement");
        let lock_path = lock_path_for(&path).unwrap();
        let retired_path = sibling_path(&path, ".retired-lock").unwrap();
        let held = HistoryFileLock::acquire(&path, LOCK_TIMEOUT).unwrap();
        fs::rename(&lock_path, &retired_path).unwrap();

        let error = HistoryFileLock::acquire(&path, Duration::from_millis(30))
            .err()
            .expect("renaming the visible sidecar must not bypass its directory lock");
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);

        drop(held);
        let reacquired = HistoryFileLock::acquire(&path, Duration::from_millis(30))
            .expect("history locking remains usable after the original guard exits");
        drop(reacquired);
        fs::remove_file(retired_path).unwrap();
        cleanup(&path);
    }

    #[cfg(unix)]
    #[test]
    fn history_parent_is_created_private_and_rejects_nonsticky_shared_writes() {
        use std::os::unix::fs::PermissionsExt;

        let id = APPEND_COUNT.fetch_add(1, Ordering::Relaxed);
        let private_parent = std::env::temp_dir().join(format!(
            "jterm-command-history-private-parent-{}-{id}",
            std::process::id()
        ));
        let private_path = private_parent.join("history.jsonl");
        append(&private_path, 100, "private", None, 0, None).unwrap();
        assert_eq!(
            fs::metadata(&private_parent).unwrap().permissions().mode() & 0o777,
            0o700
        );
        fs::remove_dir_all(&private_parent).unwrap();

        let id = APPEND_COUNT.fetch_add(1, Ordering::Relaxed);
        let shared_parent = std::env::temp_dir().join(format!(
            "jterm-command-history-shared-parent-{}-{id}",
            std::process::id()
        ));
        fs::create_dir(&shared_parent).unwrap();
        fs::set_permissions(&shared_parent, fs::Permissions::from_mode(0o777)).unwrap();
        let shared_path = shared_parent.join("history.jsonl");
        assert!(append(&shared_path, 100, "blocked", None, 0, None).is_err());
        assert!(!shared_path.exists());
        fs::remove_dir_all(shared_parent).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn lock_symlink_never_chmods_or_locks_its_target() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let path = temp_path("lock-symlink");
        let lock_path = lock_path_for(&path).unwrap();
        let victim = temp_path("lock-symlink-victim");
        fs::write(&victim, b"do not touch").unwrap();
        fs::set_permissions(&victim, fs::Permissions::from_mode(0o644)).unwrap();
        symlink(&victim, &lock_path).unwrap();

        assert!(append(&path, 100, "blocked", None, 0, None).is_err());
        assert_eq!(fs::read(&victim).unwrap(), b"do not touch");
        assert_eq!(
            fs::metadata(&victim).unwrap().permissions().mode() & 0o777,
            0o644
        );

        cleanup(&path);
        cleanup(&victim);
    }

    #[cfg(unix)]
    #[test]
    fn history_symlink_never_appends_to_or_chmods_its_target() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let path = temp_path("history-symlink");
        let victim = temp_path("history-symlink-victim");
        fs::write(&victim, b"do not touch").unwrap();
        fs::set_permissions(&victim, fs::Permissions::from_mode(0o644)).unwrap();
        symlink(&victim, &path).unwrap();

        assert!(append(&path, 100, "blocked", None, 0, None).is_err());
        assert!(read_recent(&path, 10).is_err());
        assert_eq!(fs::read(&victim).unwrap(), b"do not touch");
        assert_eq!(
            fs::metadata(&victim).unwrap().permissions().mode() & 0o777,
            0o644
        );

        cleanup(&path);
        cleanup(&victim);
    }

    #[cfg(unix)]
    #[test]
    fn history_hard_link_is_rejected_before_append() {
        let path = temp_path("history-hard-link");
        let victim = temp_path("history-hard-link-victim");
        fs::write(&victim, b"do not touch").unwrap();
        fs::hard_link(&victim, &path).unwrap();

        assert!(append(&path, 100, "blocked", None, 0, None).is_err());
        assert!(read_recent(&path, 10).is_err());
        assert_eq!(fs::read(&victim).unwrap(), b"do not touch");

        cleanup(&path);
        cleanup(&victim);
    }

    #[cfg(unix)]
    #[test]
    fn history_writable_by_other_users_is_rejected() {
        use std::os::unix::fs::PermissionsExt;

        let path = temp_path("shared-write");
        fs::write(&path, b"{\"command\":\"do not trust\",\"exit_code\":0}\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o622)).unwrap();

        assert!(read_recent(&path, 10).is_err());
        assert!(append(&path, 100, "blocked", None, 0, None).is_err());
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "{\"command\":\"do not trust\",\"exit_code\":0}\n"
        );
        cleanup(&path);
    }

    #[cfg(unix)]
    #[test]
    fn fifo_history_is_rejected_without_blocking() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let path = temp_path("history-fifo");
        let path_bytes = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: path_bytes is NUL-terminated and points to valid storage for
        // the duration of the call; the mode has no invalid bit pattern.
        assert_eq!(unsafe { libc::mkfifo(path_bytes.as_ptr(), 0o600) }, 0);

        let started = Instant::now();
        assert!(read_recent(&path, 10).is_err());
        assert!(append(&path, 100, "blocked", None, 0, None).is_err());
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "fifo rejection unexpectedly blocked for {:?}",
            started.elapsed()
        );

        cleanup(&path);
    }

    #[cfg(unix)]
    #[test]
    fn history_descriptors_close_across_exec() {
        use std::os::fd::AsRawFd;

        let path = temp_path("cloexec");
        write_test_history(&path, b"{\"command\":\"safe\",\"exit_code\":0}\n");
        let history = open_history_for_read(&path).unwrap();
        let directory = open_history_directory(&path).unwrap();
        let lock = open_lock_file(&lock_path_for(&path).unwrap()).unwrap();

        for file in [&history, &directory, &lock] {
            // SAFETY: file owns a live descriptor and F_GETFD does not mutate
            // memory through pointers.
            let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) };
            assert!(flags >= 0);
            assert_ne!(flags & libc::FD_CLOEXEC, 0);
        }

        cleanup(&path);
    }

    #[test]
    fn queued_appends_are_persisted_by_flush() {
        let path = temp_path("queued");
        enqueue(&path, 100, "first", Some("/tmp"), 0, Some(1)).unwrap();
        enqueue(&path, 100, "second", Some("/tmp"), 1, Some(2)).unwrap();
        flush_pending(Duration::from_secs(2)).unwrap();

        let records = read_recent(&path, 10).unwrap();
        assert_eq!(
            records
                .iter()
                .map(|record| record.command.as_str())
                .collect::<Vec<_>>(),
            vec!["second", "first"]
        );
        cleanup(&path);
    }

    #[cfg(unix)]
    #[test]
    fn writer_flush_reports_and_then_clears_the_generation_error() {
        use std::os::unix::fs::symlink;

        let path = temp_path("writer-error");
        let victim = temp_path("writer-error-victim");
        fs::write(&victim, b"keep me").unwrap();
        symlink(&victim, &path).unwrap();

        let (sender, receiver) = mpsc::sync_channel(4);
        let worker = thread::spawn(move || run_history_writer(receiver));
        sender
            .send(WriterMessage::Append(AppendRequest {
                path: path.clone(),
                max_entries: 100,
                command: "blocked".to_string(),
                cwd: None,
                exit_code: 0,
                end_time_ms: None,
            }))
            .unwrap();

        let flush = |sender: &mpsc::SyncSender<WriterMessage>| {
            let (acknowledge, received) = mpsc::sync_channel(0);
            sender.send(WriterMessage::Flush(acknowledge)).unwrap();
            received.recv_timeout(Duration::from_secs(1)).unwrap()
        };
        assert!(flush(&sender).is_err());
        assert!(flush(&sender).is_ok(), "the next generation starts clean");
        assert_eq!(fs::read(&victim).unwrap(), b"keep me");

        drop(sender);
        worker.join().unwrap();
        cleanup(&path);
        cleanup(&victim);
    }

    #[test]
    fn concurrent_append_child_process() {
        let Some(path) = std::env::var_os(CHILD_PATH_ENV) else {
            return;
        };
        let prefix = std::env::var(CHILD_PREFIX_ENV).unwrap();
        let count = std::env::var(CHILD_COUNT_ENV)
            .unwrap()
            .parse::<usize>()
            .unwrap();
        let path = PathBuf::from(path);
        for index in 0..count {
            append(
                &path,
                10_000,
                &format!("{prefix}-{index}"),
                Some("/tmp"),
                0,
                Some(index as u64),
            )
            .unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn concurrent_processes_keep_every_record_valid_through_compaction() {
        const WORKERS: usize = 2;
        // Each child crosses COMPACT_EVERY independently, forcing append and
        // rename transactions from separate processes to contend on the lock.
        const RECORDS_PER_WORKER: usize = COMPACT_EVERY as usize + 12;

        let path = temp_path("multiprocess");
        let executable = std::env::current_exe().unwrap();
        let test_name = "command_history::tests::concurrent_append_child_process";
        let mut children = Vec::new();
        for worker in 0..WORKERS {
            children.push(
                Command::new(&executable)
                    .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
                    .env(CHILD_PATH_ENV, &path)
                    .env(CHILD_PREFIX_ENV, format!("worker-{worker}"))
                    .env(CHILD_COUNT_ENV, RECORDS_PER_WORKER.to_string())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap(),
            );
        }
        for child in children {
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "history writer child failed:\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let contents = fs::read(&path).unwrap();
        assert!(contents.ends_with(b"\n"));
        let mut commands = HashSet::new();
        for line in contents.split(|byte| *byte == b'\n') {
            if line.is_empty() {
                continue;
            }
            let record: CommandHistoryRecord = serde_json::from_slice(line).unwrap();
            assert!(commands.insert(record.command));
        }
        assert_eq!(commands.len(), WORKERS * RECORDS_PER_WORKER);
        cleanup(&path);
    }

    fn temp_root(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "jterm-command-history-{name}-{}-{}",
            std::process::id(),
            NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        }
        directory
    }

    #[cfg(unix)]
    #[test]
    fn preflight_creates_a_private_parent_and_tightens_entries() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_root("preflight-create");
        let parent = root.join("state");
        let path = parent.join("history.jsonl");

        prepare_path(&path, true).unwrap();
        assert_eq!(
            fs::metadata(&parent).unwrap().permissions().mode() & 0o777,
            0o700
        );

        let lock = lock_path_for(&path).unwrap();
        fs::write(&path, b"history\n").unwrap();
        fs::write(&lock, b"").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
        fs::set_permissions(&lock, fs::Permissions::from_mode(0o666)).unwrap();

        assert!(prepare_path(&path, false).is_err());

        prepare_path(&path, true).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&lock).unwrap().permissions().mode() & 0o777,
            0o600
        );
        cleanup(&path);
    }

    #[cfg(unix)]
    #[test]
    fn preflight_rejects_links_and_fifos_without_blocking() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::{symlink, PermissionsExt};

        let root = temp_root("preflight-unsafe");
        let victim = root.join("victim.jsonl");
        fs::write(&victim, b"victim\n").unwrap();
        fs::set_permissions(&victim, fs::Permissions::from_mode(0o600)).unwrap();

        let symlink_path = root.join("symlink.jsonl");
        symlink(&victim, &symlink_path).unwrap();
        assert!(prepare_path(&symlink_path, false).is_err());
        assert!(prepare_path(&symlink_path, true).is_err());

        let hard_link_path = root.join("hard-link.jsonl");
        fs::hard_link(&victim, &hard_link_path).unwrap();
        assert!(prepare_path(&hard_link_path, false).is_err());
        assert!(prepare_path(&hard_link_path, true).is_err());

        let fifo_path = root.join("fifo.jsonl");
        let encoded = CString::new(fifo_path.as_os_str().as_bytes()).unwrap();
        // SAFETY: encoded is a live NUL-terminated pathname for this call.
        assert_eq!(unsafe { libc::mkfifo(encoded.as_ptr(), 0o600) }, 0);
        let started = Instant::now();
        assert!(prepare_path(&fifo_path, false).is_err());
        assert!(prepare_path(&fifo_path, true).is_err());
        assert!(started.elapsed() < Duration::from_secs(1));

        let safe_history = root.join("safe.jsonl");
        let unsafe_lock = lock_path_for(&safe_history).unwrap();
        symlink(&victim, &unsafe_lock).unwrap();
        assert!(prepare_path(&safe_history, true).is_err());
        assert!(!safe_history.exists());
        assert_eq!(fs::read(&victim).unwrap(), b"victim\n");
        cleanup(&safe_history);
    }

    #[cfg(unix)]
    #[test]
    fn preflight_rejects_a_writable_parent_without_chmodding_it() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_root("preflight-writable-parent");
        let parent = root.join("shared");
        fs::create_dir(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o777)).unwrap();
        let path = parent.join("history.jsonl");

        assert!(prepare_path(&path, false).is_err());
        assert!(prepare_path(&path, true).is_err());
        assert!(!path.exists());
        assert_eq!(
            fs::metadata(&parent).unwrap().permissions().mode() & 0o777,
            0o777
        );
        cleanup(&path);
    }
}
