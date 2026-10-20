//! Bounded, asynchronous project discovery for the native Vim9 quick picker.
//! No recursive filesystem or process I/O runs on a frontend's render thread.

use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, SendTimeoutError, Sender};
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use parking_lot::Mutex;

const MAX_FILES: usize = 20_000;
const MAX_PATH_BYTES: usize = 16 * 1024;
const MAX_INDEX_BYTES: usize = 16 * 1024 * 1024;
const MAX_ERROR_BYTES: usize = 4096;
const SCAN_TIMEOUT: Duration = Duration::from_secs(15);
const POLL: Duration = Duration::from_millis(25);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Vim9ProjectFile {
    pub path: PathBuf,
    /// Project-relative display/matching text; never shell-escaped or executed.
    pub relative: String,
}

#[derive(Clone, Debug, Default)]
pub struct Vim9ProjectSnapshot {
    pub scanning: bool,
    /// Immutable and cheap to clone. Frontends can use `Arc::ptr_eq` to detect
    /// a new index and only rerank when the query or this Arc has changed.
    pub files: Arc<Vec<Vim9ProjectFile>>,
    pub truncated: bool,
    pub error: Option<String>,
}

impl Vim9ProjectSnapshot {
    /// Case-insensitive fuzzy ranking with a filename bonus and deterministic
    /// ties. Both input and output are bounded; this never touches the disk.
    pub fn matches(&self, query: &str, limit: usize) -> Vec<Vim9ProjectFile> {
        let limit = limit.min(256);
        let query = query.trim();
        if limit == 0 || query.chars().count() > 256 {
            return Vec::new();
        }
        if query.is_empty() {
            return self.files.iter().take(limit).cloned().collect();
        }
        let matcher = SkimMatcherV2::default().ignore_case().element_limit(4096);
        let mut ranked = self
            .files
            .iter()
            .filter_map(|file| {
                let score = matcher.fuzzy_match(&file.relative, query)?;
                let filename = file.path.file_name().and_then(|s| s.to_str()).unwrap_or("");
                let filename_score = matcher.fuzzy_match(filename, query);
                let exact = filename.eq_ignore_ascii_case(query);
                Some((file, exact, filename_score.is_some(), score))
            })
            .collect::<Vec<_>>();
        ranked.sort_unstable_by(|a, b| {
            b.1.cmp(&a.1)
                .then_with(|| b.2.cmp(&a.2))
                .then_with(|| b.3.cmp(&a.3))
                .then_with(|| a.0.relative.len().cmp(&b.0.relative.len()))
                .then_with(|| a.0.relative.cmp(&b.0.relative))
        });
        ranked
            .into_iter()
            .take(limit)
            .map(|entry| entry.0.clone())
            .collect()
    }
}

struct IndexInner {
    state: Arc<Mutex<Vim9ProjectSnapshot>>,
    stop: Arc<AtomicBool>,
}

impl Drop for IndexInner {
    fn drop(&mut self) {
        // The worker owns and reaps its helper. Never join it on a UI thread.
        self.stop.store(true, Ordering::Release);
    }
}

#[derive(Clone)]
pub struct Vim9ProjectIndex(Arc<IndexInner>);

impl std::fmt::Debug for Vim9ProjectIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.0.state.lock();
        f.debug_struct("Vim9ProjectIndex")
            .field("scanning", &state.scanning)
            .field("files", &state.files.len())
            .field("truncated", &state.truncated)
            .finish()
    }
}

impl Vim9ProjectIndex {
    /// Start one scan. Replacing the handle refreshes the index and cancels
    /// the old scan when its last frontend handle is dropped. Errors are
    /// published in the snapshot instead of blocking creation of a workbench.
    pub fn start(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref().to_owned();
        let state = Arc::new(Mutex::new(Vim9ProjectSnapshot {
            scanning: true,
            ..Default::default()
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let worker_state = state.clone();
        let worker_stop = stop.clone();
        if let Err(error) = thread::Builder::new()
            .name("vim9-project-index".into())
            .spawn(move || {
                let result = scan(&root, &worker_stop, MAX_FILES, SCAN_TIMEOUT);
                if !worker_stop.load(Ordering::Acquire) {
                    *worker_state.lock() = result;
                }
            })
        {
            let mut snapshot = state.lock();
            snapshot.scanning = false;
            snapshot.error = Some(format!("Could not start project index: {error}"));
        }
        Self(Arc::new(IndexInner { state, stop }))
    }

    pub fn snapshot(&self) -> Vim9ProjectSnapshot {
        self.0.state.lock().clone()
    }
}

enum PipeEvent {
    Data { stderr: bool, bytes: Vec<u8> },
    Closed,
    Failed(io::Error),
}

fn send_event(sender: &Sender<PipeEvent>, mut event: PipeEvent, stop: &AtomicBool) -> bool {
    while !stop.load(Ordering::Acquire) {
        match sender.send_timeout(event, POLL) {
            Ok(()) => return true,
            Err(SendTimeoutError::Timeout(pending)) => event = pending,
            Err(SendTimeoutError::Disconnected(_)) => break,
        }
    }
    false
}

fn read_pipe(
    mut pipe: impl Read + Send + 'static,
    stderr: bool,
    sender: Sender<PipeEvent>,
    stop: Arc<AtomicBool>,
) -> io::Result<()> {
    thread::Builder::new()
        .name("vim9-index-pipe".into())
        .spawn(move || {
            let mut buffer = [0u8; 8192];
            while !stop.load(Ordering::Acquire) {
                let event = match pipe.read(&mut buffer) {
                    Ok(0) => {
                        send_event(&sender, PipeEvent::Closed, &stop);
                        break;
                    }
                    Ok(count) => PipeEvent::Data {
                        stderr,
                        bytes: buffer[..count].to_vec(),
                    },
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => {
                        send_event(&sender, PipeEvent::Failed(error), &stop);
                        break;
                    }
                };
                if !send_event(&sender, event, &stop) {
                    break;
                }
            }
        })
        .map(|_| ())
}

fn scan(
    root: &Path,
    stop: &Arc<AtomicBool>,
    limit: usize,
    timeout: Duration,
) -> Vim9ProjectSnapshot {
    let mut result = Vim9ProjectSnapshot::default();
    let mut files = Vec::new();
    if let Err(error) = collect_files(
        root,
        stop,
        limit,
        timeout,
        &mut files,
        &mut result.truncated,
    ) {
        result.error = Some(format!("Project file index: {error}"));
    }
    files.sort_unstable_by(|a, b| a.relative.cmp(&b.relative));
    files.dedup_by(|a, b| a.relative == b.relative);
    result.files = Arc::new(files);
    result
}

fn collect_files(
    root: &Path,
    stop: &Arc<AtomicBool>,
    limit: usize,
    timeout: Duration,
    files: &mut Vec<Vim9ProjectFile>,
    truncated: &mut bool,
) -> io::Result<()> {
    if stop.load(Ordering::Acquire) {
        return Ok(());
    }
    let root = if root.is_absolute() {
        root.to_owned()
    } else {
        std::env::current_dir()?.join(root)
    };
    // Quick Open is an explicit editor action, like Vim's own project search.
    // Support the user's installed rg, resolving its absolute executable before
    // changing directory and ignoring empty/relative PATH entries. A checkout
    // containing an rg file must never take over this action by virtue of cwd.
    let mut command = if crate::host::is_flatpak() {
        crate::host::helper_command_with_cwd("rg", &root)?
    } else {
        let program = crate::host::find_executable_in_path("rg").ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "ripgrep (rg) is required on PATH")
        })?;
        crate::host::command_with_cwd(program, &root)
    };
    command
        .args([
            "--no-config",
            "--files",
            "--hidden",
            "--null",
            "--no-require-git",
            "--glob",
            "!.git",
            "--",
            ".",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = crate::supervised::SupervisedChild::spawn(&mut command)?;
    let (sender, receiver) = bounded(16);
    read_pipe(
        child
            .take_stdout()
            .ok_or_else(|| io::Error::other("missing rg stdout"))?,
        false,
        sender.clone(),
        stop.clone(),
    )?;
    read_pipe(
        child
            .take_stderr()
            .ok_or_else(|| io::Error::other("missing rg stderr"))?,
        true,
        sender,
        stop.clone(),
    )?;
    let deadline = Instant::now() + timeout;
    let mut pending = Vec::new();
    let mut errors = Vec::new();
    let mut closed = 0;
    let mut bytes_retained = 0usize;
    loop {
        if stop.load(Ordering::Acquire) {
            return Ok(()); // SupervisedChild drops/reaps on this worker.
        }
        if Instant::now() >= deadline {
            *truncated = true;
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "scan exceeded 15 seconds; showing partial results",
            ));
        }
        match receiver.recv_timeout(POLL) {
            Ok(PipeEvent::Data {
                stderr: true,
                bytes,
            }) => {
                let remaining = MAX_ERROR_BYTES.saturating_sub(errors.len());
                errors.extend_from_slice(&bytes[..bytes.len().min(remaining)]);
            }
            Ok(PipeEvent::Data {
                stderr: false,
                bytes,
            }) => {
                for byte in bytes {
                    if byte != 0 {
                        if pending.len() >= MAX_PATH_BYTES {
                            *truncated = true;
                            return Err(io::Error::other(
                                "a filename exceeded the index path limit",
                            ));
                        }
                        pending.push(byte);
                        continue;
                    }
                    if let Some(file) = indexed_file(&root, &pending) {
                        let bytes = file.relative.len() + file.path.as_os_str().len();
                        if files.len() >= limit
                            || bytes_retained.saturating_add(bytes) > MAX_INDEX_BYTES
                        {
                            *truncated = true;
                            return Ok(());
                        }
                        bytes_retained += bytes;
                        files.push(file);
                    }
                    pending.clear();
                }
            }
            Ok(PipeEvent::Closed) => closed += 1,
            Ok(PipeEvent::Failed(error)) => return Err(error),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                closed = 2;
                // A helper can close both streams before exiting. Keep its
                // bounded lifetime without busy-polling a disconnected queue.
                thread::sleep(POLL);
            }
        }
        if closed == 2 && child.root_has_exited()? {
            let status = child.reap_after_group_kill()?;
            // rg --files exits 1 for a project with no included files.
            if status.success() || status.code() == Some(1) {
                return Ok(());
            }
            return Err(io::Error::other(format!(
                "rg failed ({status}): {}",
                String::from_utf8_lossy(&errors).trim()
            )));
        }
    }
}

fn indexed_file(root: &Path, bytes: &[u8]) -> Option<Vim9ProjectFile> {
    let relative = std::str::from_utf8(bytes).ok()?;
    if relative.is_empty() || relative.contains(['\n', '\r', '\0']) {
        return None;
    }
    let path = Path::new(relative);
    if path
        .components()
        .any(|part| !matches!(part, Component::Normal(_) | Component::CurDir))
    {
        return None;
    }
    let path = path
        .components()
        .filter(|part| *part != Component::CurDir)
        .collect::<PathBuf>();
    Some(Vim9ProjectFile {
        relative: path.to_str()?.to_owned(),
        path: root.join(path),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("vim9-index-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn fuzzy_picker_ranks_files_and_keeps_literal_unicode_paths() {
        let root = Path::new("/project");
        let snapshot = Vim9ProjectSnapshot {
            files: Arc::new(
                [
                    "main.rs",
                    "src/main.rs",
                    "src/domain/resource.rs",
                    "文档/数据 |%.rs",
                    "MAIN.rs.backup",
                ]
                .map(|name| indexed_file(root, name.as_bytes()).unwrap())
                .into(),
            ),
            ..Default::default()
        };
        let results = snapshot.matches("main.rs", 3);
        assert_eq!(results[0].relative, "main.rs");
        assert_eq!(results[1].relative, "src/main.rs");
        assert_eq!(snapshot.matches("src/mrs", 1)[0].relative, "src/main.rs");
        assert_eq!(
            snapshot.matches("数据", 1)[0].path,
            root.join("文档/数据 |%.rs")
        );
        assert!(snapshot.matches("no-such-file", 20).is_empty());
        assert!(snapshot.matches("", 0).is_empty());
        assert!(snapshot.matches(&"a".repeat(257), 20).is_empty());
        assert_eq!(snapshot.matches("", 2).len(), 2);
        assert!(Arc::ptr_eq(&snapshot.files, &snapshot.clone().files));
        assert!(indexed_file(root, b"../outside").is_none());
        assert!(indexed_file(root, b"/absolute").is_none());
        assert!(indexed_file(root, b"new\nline").is_none());
        assert_eq!(
            indexed_file(root, b"./-literal $file").unwrap().relative,
            "-literal $file"
        );
    }

    #[test]
    fn actual_rg_index_obeys_ignore_rules_and_reports_limits_and_refresh() {
        let scratch = Scratch::new();
        fs::create_dir(scratch.0.join("src")).unwrap();
        fs::create_dir(scratch.0.join("build")).unwrap();
        fs::create_dir(scratch.0.join(".git")).unwrap();
        fs::write(scratch.0.join(".gitignore"), "build/\n*.tmp\n").unwrap();
        fs::write(scratch.0.join(".ignore"), "*.skip\n").unwrap();
        for path in [
            "src/main.rs",
            ".hidden",
            "文档 |%.txt",
            "build/object",
            "noise.tmp",
            "noise.skip",
            ".git/config",
        ] {
            fs::write(scratch.0.join(path), "").unwrap();
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(&scratch.0, scratch.0.join("src/cycle")).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let snapshot = scan(&scratch.0, &stop, MAX_FILES, SCAN_TIMEOUT);
        assert_eq!(snapshot.error, None);
        let names = snapshot
            .files
            .iter()
            .map(|file| file.relative.as_str())
            .collect::<Vec<_>>();
        assert!(names.contains(&"src/main.rs"));
        assert!(names.contains(&".hidden"));
        assert!(names.contains(&"文档 |%.txt"));
        assert!(!names.iter().any(|name| name.contains("object")
            || name.contains("noise")
            || name.contains("config")
            || name.contains("cycle")));
        let limited = scan(&scratch.0, &stop, 2, SCAN_TIMEOUT);
        assert!(limited.truncated);
        assert_eq!(limited.files.len(), 2);
        assert_eq!(limited.error, None);
        fs::write(scratch.0.join("new.rs"), "").unwrap();
        let index = Vim9ProjectIndex::start(&scratch.0);
        let until = Instant::now() + Duration::from_secs(4);
        while index.snapshot().scanning {
            assert!(Instant::now() < until);
            thread::sleep(POLL);
        }
        assert_eq!(index.snapshot().matches("new.rs", 1)[0].relative, "new.rs");
        assert!(Arc::ptr_eq(
            &index.snapshot().files,
            &index.snapshot().files
        ));
    }

    #[test]
    fn index_errors_are_visible_and_last_handle_drop_cancels() {
        let scratch = Scratch::new();
        let stop = Arc::new(AtomicBool::new(false));
        let missing = scan(&scratch.0.join("missing"), &stop, MAX_FILES, SCAN_TIMEOUT);
        assert!(missing.error.is_some());
        let index = Vim9ProjectIndex::start(&scratch.0);
        let flag = index.0.stop.clone();
        let other = index.clone();
        drop(index);
        assert!(!flag.load(Ordering::Acquire));
        drop(other);
        assert!(flag.load(Ordering::Acquire));
    }
}
