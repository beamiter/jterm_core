//! Shared, private favorites and resume receipts. Every disk operation runs on
//! a bounded worker; transactions merge under a cross-process directory flock.
use super::{valid_id, SessionProvider};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_ENTRIES: usize = 4096;
const MAX_PENDING: usize = 256;
const LOCK_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionLibraryEntry {
    pub provider: SessionProvider,
    pub id: String,
    pub pinned: bool,
    pub last_resumed_ms: Option<u64>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionLibrarySnapshot {
    entries: BTreeMap<String, SessionLibraryEntry>,
}
impl SessionLibrarySnapshot {
    pub fn from_entries(entries: Vec<SessionLibraryEntry>) -> Result<Self, String> {
        if entries.len() > MAX_ENTRIES {
            return Err("Session library exceeds its 4,096-entry limit.".into());
        }
        let mut snapshot = Self::default();
        for mut entry in entries {
            if !valid_id(&entry.id) {
                return Err("Session library contains an invalid session UUID.".into());
            }
            entry.id.make_ascii_lowercase();
            if entry.last_resumed_ms == Some(0) {
                entry.last_resumed_ms = None;
            }
            let key = key(entry.provider, &entry.id);
            if snapshot.entries.insert(key, entry).is_some() {
                return Err("Session library contains duplicate identities.".into());
            }
        }
        snapshot
            .entries
            .retain(|_, entry| entry.pinned || entry.last_resumed_ms.is_some());
        Ok(snapshot)
    }
    pub fn is_pinned(&self, provider: SessionProvider, id: &str) -> bool {
        self.entries
            .get(&key(provider, id))
            .is_some_and(|entry| entry.pinned)
    }
    pub fn last_resumed(&self, provider: SessionProvider, id: &str) -> Option<u64> {
        self.entries
            .get(&key(provider, id))
            .and_then(|entry| entry.last_resumed_ms)
    }
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    pub fn pinned_count(&self) -> usize {
        self.entries.values().filter(|entry| entry.pinned).count()
    }
    pub fn iter(&self) -> impl Iterator<Item = &SessionLibraryEntry> {
        self.entries.values()
    }
}
fn key(provider: SessionProvider, id: &str) -> String {
    format!("{}:{}", provider.label(), id.to_ascii_lowercase())
}

/// One worker and a bounded, coalesced queue per store. `new`, `poll`, and
/// mutation methods perform no filesystem operations on the caller's thread.
pub struct SessionLibraryStore {
    snapshot: SessionLibrarySnapshot,
    state: Arc<Mutex<WorkerState>>,
    wake: mpsc::SyncSender<()>,
    loaded: bool,
    available: bool,
    error: Option<String>,
}
#[derive(Default)]
struct WorkerState {
    pending: BTreeMap<String, Change>,
    refresh: bool,
    working: bool,
    closed: bool,
    latest: Option<Result<SessionLibrarySnapshot, String>>,
}
#[derive(Clone)]
struct Change {
    provider: SessionProvider,
    id: String,
    pinned: Option<bool>,
    resumed: Option<u64>,
}
impl Default for SessionLibraryStore {
    fn default() -> Self {
        Self::new()
    }
}
impl SessionLibraryStore {
    pub fn new() -> Self {
        // Path computation only; creation/reading belongs to the worker.
        let root = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| dirs::home_dir().map(|home| home.join(".local/state")));
        match root {
            Some(root) => Self::with_path(root.join("jterm/agent-sessions-library.json")),
            None => Self::unavailable(
                "Cannot locate a private state directory for the session library.",
            ),
        }
    }
    /// Override for an isolated fixture or an explicitly configured private
    /// state location. The parent must be owned by this user and private.
    pub fn with_path(path: PathBuf) -> Self {
        let state = Arc::new(Mutex::new(WorkerState {
            refresh: true,
            ..WorkerState::default()
        }));
        let (wake, receiver) = mpsc::sync_channel(1);
        let shared = state.clone();
        let available = std::thread::Builder::new()
            .name("agent-session-library".into())
            .spawn(move || {
                while receiver.recv().is_ok() {
                    loop {
                        let job = {
                            let Ok(mut state) = shared.lock() else { return };
                            if state.pending.is_empty() && !state.refresh {
                                if state.closed {
                                    return;
                                }
                                break;
                            }
                            state.working = true;
                            state.refresh = false;
                            std::mem::take(&mut state.pending)
                        };
                        let result = std::panic::catch_unwind(|| transaction(&path, &job))
                            .unwrap_or_else(|_| {
                                Err(
                                    "Session library worker failed; reopen the manager to retry."
                                        .into(),
                                )
                            });
                        let Ok(mut state) = shared.lock() else { return };
                        state.working = false;
                        state.latest = Some(result);
                    }
                }
            })
            .is_ok();
        let mut store = Self {
            snapshot: SessionLibrarySnapshot::default(),
            state,
            wake,
            loaded: false,
            available,
            error: None,
        };
        if available {
            let _ = store.wake.try_send(());
        } else {
            store.loaded = true;
            store.error = Some("Could not start the session library worker.".into());
        }
        store
    }
    fn unavailable(message: &str) -> Self {
        let (wake, _) = mpsc::sync_channel(1);
        Self {
            snapshot: SessionLibrarySnapshot::default(),
            state: Arc::new(Mutex::new(WorkerState::default())),
            wake,
            loaded: true,
            available: false,
            error: Some(message.into()),
        }
    }
    pub fn snapshot(&self) -> &SessionLibrarySnapshot {
        &self.snapshot
    }
    pub fn is_loaded(&self) -> bool {
        self.loaded
    }
    pub fn is_busy(&self) -> bool {
        self.available
            && self.state.lock().is_ok_and(|state| {
                state.working
                    || state.refresh
                    || !state.pending.is_empty()
                    || state.latest.is_some()
            })
    }
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
    /// Apply the newest worker result. Snapshot/error changes both trigger a
    /// repaint; writes progress even if the UI does not call this method.
    pub fn poll(&mut self) -> bool {
        let result = self
            .state
            .lock()
            .ok()
            .and_then(|mut state| state.latest.take());
        let Some(result) = result else { return false };
        let was_loaded = self.loaded;
        let old_error = self.error.clone();
        self.loaded = true;
        match result {
            Ok(snapshot) => {
                let changed = self.snapshot != snapshot;
                self.snapshot = snapshot;
                self.error = None;
                changed || !was_loaded || old_error.is_some()
            }
            Err(error) => {
                self.error = Some(error);
                !was_loaded || self.error != old_error
            }
        }
    }
    pub fn refresh(&mut self) -> bool {
        if !self.available {
            return false;
        }
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        if state.working || state.refresh {
            return false;
        }
        state.refresh = true;
        drop(state);
        let _ = self.wake.try_send(());
        true
    }
    pub fn set_pinned(
        &mut self,
        provider: SessionProvider,
        id: &str,
        pinned: bool,
    ) -> Result<(), String> {
        self.enqueue(provider, id, Some(pinned), None)
    }
    pub fn record_resumed(&mut self, provider: SessionProvider, id: &str) -> Result<(), String> {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u64::MAX as u128) as u64;
        self.enqueue(provider, id, None, Some(timestamp.max(1)))
    }
    fn enqueue(
        &mut self,
        provider: SessionProvider,
        id: &str,
        pinned: Option<bool>,
        resumed: Option<u64>,
    ) -> Result<(), String> {
        if !valid_id(id) {
            return Err("Cannot update session library: invalid session UUID.".into());
        }
        if !self.available {
            return Err(self
                .error
                .clone()
                .unwrap_or_else(|| "Session library worker is unavailable.".into()));
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| "Session library queue is unavailable.")?;
        let identity = key(provider, id);
        if !state.pending.contains_key(&identity) && state.pending.len() >= MAX_PENDING {
            return Err("Session library is busy; wait for pending changes to finish.".into());
        }
        let change = state.pending.entry(identity).or_insert_with(|| Change {
            provider,
            id: id.to_ascii_lowercase(),
            pinned: None,
            resumed: None,
        });
        if pinned.is_some() {
            change.pinned = pinned;
        }
        if let Some(timestamp) = resumed {
            change.resumed = Some(change.resumed.unwrap_or(0).max(timestamp));
        }
        drop(state);
        let _ = self.wake.try_send(());
        Ok(())
    }
}
impl Drop for SessionLibraryStore {
    fn drop(&mut self) {
        if let Ok(mut state) = self.state.lock() {
            state.closed = true;
        }
        let _ = self.wake.try_send(());
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireLibrary {
    version: u32,
    sessions: Vec<WireEntry>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireEntry {
    provider: String,
    id: String,
    pinned: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_resumed_ms: Option<u64>,
}
fn decode(text: &str) -> Result<SessionLibrarySnapshot, String> {
    crate::bounded_json::validate_no_duplicate_members(text.as_bytes())
        .map_err(|_| "Session library contains duplicate or malformed JSON fields.")?;
    let wire: WireLibrary = serde_json::from_str(text)
        .map_err(|_| "Session library JSON is malformed; it was left unchanged.")?;
    if wire.version != 1 {
        return Err("Session library version is unsupported; it was left unchanged.".into());
    }
    let mut entries = Vec::new();
    for entry in wire.sessions {
        let provider = match entry.provider.as_str() {
            "codex" => SessionProvider::Codex,
            "claude" => SessionProvider::Claude,
            _ => return Err("Session library contains an unsupported provider.".into()),
        };
        entries.push(SessionLibraryEntry {
            provider,
            id: entry.id,
            pinned: entry.pinned,
            last_resumed_ms: entry.last_resumed_ms,
        });
    }
    SessionLibrarySnapshot::from_entries(entries)
}
fn transaction(
    path: &Path,
    changes: &BTreeMap<String, Change>,
) -> Result<SessionLibrarySnapshot, String> {
    let _lock = LibraryLock::acquire(path)
        .map_err(|error| format!("Session library is unavailable: {error}"))?;
    let mut snapshot = match read_private(path) {
        Ok(Some(text)) => decode(&text)?,
        Ok(None) => SessionLibrarySnapshot::default(),
        Err(error) => {
            return Err(format!(
                "Cannot read the session library; it was left unchanged: {error}"
            ))
        }
    };
    if changes.is_empty() {
        return Ok(snapshot);
    }
    for (identity, change) in changes {
        let entry =
            snapshot
                .entries
                .entry(identity.clone())
                .or_insert_with(|| SessionLibraryEntry {
                    provider: change.provider,
                    id: change.id.clone(),
                    pinned: false,
                    last_resumed_ms: None,
                });
        if let Some(pinned) = change.pinned {
            entry.pinned = pinned;
        }
        if let Some(timestamp) = change.resumed {
            entry.last_resumed_ms = Some(entry.last_resumed_ms.unwrap_or(0).max(timestamp));
        }
    }
    snapshot
        .entries
        .retain(|_, entry| entry.pinned || entry.last_resumed_ms.is_some());
    if snapshot.entries.len() > MAX_ENTRIES {
        return Err("Session library is full (4,096 identities); no changes were written.".into());
    }
    let wire = WireLibrary {
        version: 1,
        sessions: snapshot
            .entries
            .values()
            .map(|entry| WireEntry {
                provider: match entry.provider {
                    SessionProvider::Codex => "codex",
                    SessionProvider::Claude => "claude",
                }
                .into(),
                id: entry.id.clone(),
                pinned: entry.pinned,
                last_resumed_ms: entry.last_resumed_ms,
            })
            .collect(),
    };
    let bytes = serde_json::to_vec(&wire).map_err(|error| error.to_string())?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err("Session library exceeds its 1 MiB limit; no changes were written.".into());
    }
    crate::snapshot_file::write_atomic_private(path, &bytes)
        .map_err(|error| format!("Could not save the session library: {error}"))?;
    Ok(snapshot)
}
fn read_private(path: &Path) -> std::io::Result<Option<String>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.mode() & 0o077 != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "session library file must be private (0600)",
            ));
        }
    }
    if !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "session library must be a regular non-symlink file",
        ));
    }
    crate::snapshot_file::read_bounded(path, MAX_FILE_BYTES).map(Some)
}
struct LibraryLock(File);
impl LibraryLock {
    #[cfg(unix)]
    fn acquire(path: &Path) -> std::io::Result<Self> {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
        if !path.is_absolute() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "session library path must be absolute",
            ));
        }
        let directory = path
            .parent()
            .filter(|_| path.file_name().is_some())
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "session library path has no parent",
                )
            })?;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(directory)?;
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(directory)?;
        let metadata = file.metadata()?;
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "session library directory must be owned by this user and private (0700)",
            ));
        }
        let started = Instant::now();
        loop {
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(Self(file));
            }
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EWOULDBLOCK)
                && error.raw_os_error() != Some(libc::EAGAIN)
            {
                return Err(error);
            }
            if started.elapsed() >= LOCK_TIMEOUT {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "timed out waiting for another terminal's session library update",
                ));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    #[cfg(not(unix))]
    fn acquire(_path: &Path) -> std::io::Result<Self> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "shared session library locking requires Unix",
        ))
    }
}
impl Drop for LibraryLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            unsafe {
                libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};
    const ID: &str = "11111111-1111-4111-8111-111111111111";
    const OTHER: &str = "22222222-2222-4222-8222-222222222222";
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("session-library-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&root).unwrap();
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
            Self(root)
        }
        fn path(&self) -> PathBuf {
            self.0.join("private/library.json")
        }
        fn write(&self, bytes: &[u8]) {
            let path = self.path();
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
            fs::write(&path, bytes).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn changes(
        provider: SessionProvider,
        id: &str,
        pinned: Option<bool>,
        resumed: Option<u64>,
    ) -> BTreeMap<String, Change> {
        BTreeMap::from([(
            key(provider, id),
            Change {
                provider,
                id: id.into(),
                pinned,
                resumed,
            },
        )])
    }
    fn settle(store: &mut SessionLibraryStore) {
        let start = Instant::now();
        loop {
            store.poll();
            if store.is_loaded() && !store.is_busy() {
                store.poll();
                return;
            }
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "library worker did not finish"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    #[test]
    fn asynchronous_pin_unpin_and_resume_survive_a_fresh_store() {
        let f = Fixture::new();
        let mut store = SessionLibraryStore::with_path(f.path());
        settle(&mut store);
        assert!(store.error().is_none());
        assert!(store.snapshot().is_empty());
        store.set_pinned(SessionProvider::Codex, ID, true).unwrap();
        store.record_resumed(SessionProvider::Codex, ID).unwrap();
        settle(&mut store);
        assert!(store.snapshot().is_pinned(SessionProvider::Codex, ID));
        assert!(!store.snapshot().is_pinned(SessionProvider::Claude, ID));
        let receipt = store
            .snapshot()
            .last_resumed(SessionProvider::Codex, ID)
            .unwrap();
        let mut other = SessionLibraryStore::with_path(f.path());
        settle(&mut other);
        assert_eq!(other.snapshot(), store.snapshot());
        other.set_pinned(SessionProvider::Codex, ID, false).unwrap();
        settle(&mut other);
        store.refresh();
        settle(&mut store);
        assert!(!store.snapshot().is_pinned(SessionProvider::Codex, ID));
        assert_eq!(
            store.snapshot().last_resumed(SessionProvider::Codex, ID),
            Some(receipt)
        );
    }
    #[test]
    fn concurrent_transactions_merge_unrelated_pins_and_receipts() {
        let f = Fixture::new();
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let mut workers = Vec::new();
        for index in 0..8 {
            let path = f.path();
            let barrier = barrier.clone();
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                let id = format!("{index:08}-1111-4111-8111-111111111111");
                transaction(
                    &path,
                    &changes(SessionProvider::Codex, &id, Some(true), Some(index + 1)),
                )
                .unwrap();
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
        let snapshot = transaction(&f.path(), &BTreeMap::new()).unwrap();
        assert_eq!(snapshot.pinned_count(), 8);
        assert_eq!(snapshot.len(), 8);
    }
    #[test]
    fn older_delayed_receipt_does_not_move_a_session_backwards() {
        let f = Fixture::new();
        transaction(
            &f.path(),
            &changes(SessionProvider::Codex, ID, None, Some(200)),
        )
        .unwrap();
        let snapshot = transaction(
            &f.path(),
            &changes(SessionProvider::Codex, ID, Some(true), Some(100)),
        )
        .unwrap();
        assert_eq!(snapshot.last_resumed(SessionProvider::Codex, ID), Some(200));
        assert!(snapshot.is_pinned(SessionProvider::Codex, ID));
    }
    #[test]
    fn corrupt_duplicate_future_and_oversized_files_are_never_overwritten() {
        for bytes in [
            b"not json".to_vec(),
            br#"{"version":1,"version":1,"sessions":[]}"#.to_vec(),
            br#"{"version":2,"sessions":[]}"#.to_vec(),
            vec![b'x'; MAX_FILE_BYTES as usize + 1],
        ] {
            let f = Fixture::new();
            f.write(&bytes);
            let mut store = SessionLibraryStore::with_path(f.path());
            settle(&mut store);
            assert!(store.error().is_some());
            store.set_pinned(SessionProvider::Codex, ID, true).unwrap();
            settle(&mut store);
            assert!(store.error().is_some());
            assert_eq!(fs::read(f.path()).unwrap(), bytes);
        }
    }
    #[test]
    fn files_and_directories_are_private_and_symlinks_are_rejected() {
        let f = Fixture::new();
        transaction(
            &f.path(),
            &changes(SessionProvider::Codex, ID, Some(true), None),
        )
        .unwrap();
        assert_eq!(
            fs::metadata(f.path()).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(f.path().parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let original = fs::read(f.path()).unwrap();
        let linked = f.0.join("linked.json");
        symlink(f.path(), &linked).unwrap();
        assert!(transaction(
            &linked,
            &changes(SessionProvider::Codex, OTHER, Some(true), None)
        )
        .is_err());
        assert_eq!(fs::read(f.path()).unwrap(), original);
        fs::set_permissions(f.path(), fs::Permissions::from_mode(0o644)).unwrap();
        assert!(transaction(&f.path(), &BTreeMap::new()).is_err());
        fs::set_permissions(f.path(), fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(
            f.path().parent().unwrap(),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert!(transaction(&f.path(), &BTreeMap::new()).is_err());
    }
    #[test]
    fn snapshot_validates_identities_and_never_keeps_unpinned_empty_entries() {
        let entry = SessionLibraryEntry {
            provider: SessionProvider::Claude,
            id: ID.into(),
            pinned: false,
            last_resumed_ms: None,
        };
        assert!(SessionLibrarySnapshot::from_entries(vec![entry.clone()])
            .unwrap()
            .is_empty());
        assert!(SessionLibrarySnapshot::from_entries(vec![entry.clone(), entry]).is_err());
        assert!(decode(
            r#"{"version":1,"sessions":[{"provider":"codex","id":"--last","pinned":true}]}"#
        )
        .is_err());
    }
    #[test]
    fn queued_changes_finish_without_ui_polling_and_after_store_drop() {
        let f = Fixture::new();
        {
            let mut store = SessionLibraryStore::with_path(f.path());
            store
                .set_pinned(SessionProvider::Claude, OTHER, true)
                .unwrap();
        }
        let start = Instant::now();
        loop {
            if f.path().exists()
                && transaction(&f.path(), &BTreeMap::new())
                    .is_ok_and(|snapshot| snapshot.is_pinned(SessionProvider::Claude, OTHER))
            {
                break;
            }
            assert!(start.elapsed() < Duration::from_secs(3));
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    #[test]
    fn ui_methods_do_not_wait_for_another_process_library_lock() {
        let f = Fixture::new();
        let held = LibraryLock::acquire(&f.path()).unwrap();
        let start = Instant::now();
        let mut store = SessionLibraryStore::with_path(f.path());
        store.set_pinned(SessionProvider::Codex, ID, true).unwrap();
        assert!(store.is_busy());
        store.poll();
        assert!(start.elapsed() < Duration::from_millis(100));
        drop(held);
        settle(&mut store);
        assert!(store.snapshot().is_pinned(SessionProvider::Codex, ID));
    }
    #[test]
    fn queue_is_bounded_and_coalesces_same_identity_before_io() {
        let f = Fixture::new();
        let held = LibraryLock::acquire(&f.path()).unwrap();
        let mut store = SessionLibraryStore::with_path(f.path());
        let start = Instant::now();
        while !store.state.lock().unwrap().working {
            assert!(start.elapsed() < Duration::from_secs(1));
            std::thread::yield_now();
        }
        for index in 0..MAX_PENDING {
            let id = format!("{index:08}-1111-4111-8111-111111111111");
            store.set_pinned(SessionProvider::Codex, &id, true).unwrap();
        }
        assert!(store.set_pinned(SessionProvider::Codex, ID, true).is_err());
        let first = "00000000-1111-4111-8111-111111111111";
        store
            .set_pinned(SessionProvider::Codex, first, false)
            .unwrap();
        assert_eq!(store.state.lock().unwrap().pending.len(), MAX_PENDING);
        drop(held);
        settle(&mut store);
        assert_eq!(store.snapshot().pinned_count(), MAX_PENDING - 1);
        assert!(!store.snapshot().is_pinned(SessionProvider::Codex, first));
    }
    #[test]
    fn process_fixture() {
        let Some(path) = std::env::var_os("JTERM_TEST_LIBRARY_PATH") else {
            return;
        };
        let id = std::env::var("JTERM_TEST_LIBRARY_ID").unwrap();
        transaction(
            Path::new(&path),
            &changes(SessionProvider::Claude, &id, Some(true), None),
        )
        .unwrap();
    }
    #[test]
    fn independent_processes_merge_pins_without_replacing_another_terminal_snapshot() {
        let f = Fixture::new();
        let held = LibraryLock::acquire(&f.path()).unwrap();
        let executable = std::env::current_exe().unwrap();
        let mut children = Vec::new();
        for id in [ID, OTHER] {
            children.push(
                std::process::Command::new(&executable)
                    .args(["--exact", "agent_sessions::library::tests::process_fixture"])
                    .env("JTERM_TEST_LIBRARY_PATH", f.path())
                    .env("JTERM_TEST_LIBRARY_ID", id)
                    .stdout(std::process::Stdio::null())
                    .spawn()
                    .unwrap(),
            );
        }
        drop(held);
        for mut child in children {
            assert!(child.wait().unwrap().success());
        }
        let snapshot = transaction(&f.path(), &BTreeMap::new()).unwrap();
        assert!(snapshot.is_pinned(SessionProvider::Claude, ID));
        assert!(snapshot.is_pinned(SessionProvider::Claude, OTHER));
    }
    #[test]
    fn completed_results_remain_busy_until_the_ui_acknowledges_them() {
        for corrupt in [false, true] {
            let f = Fixture::new();
            if corrupt {
                f.write(b"malformed metadata");
            }
            let mut store = SessionLibraryStore::with_path(f.path());
            let start = Instant::now();
            loop {
                if store.state.lock().unwrap().latest.is_some() {
                    break;
                }
                assert!(start.elapsed() < Duration::from_secs(3));
                std::thread::sleep(Duration::from_millis(2));
            }
            assert!(store.is_busy());
            assert!(store.poll());
            assert!(!store.is_busy());
            assert_eq!(store.error().is_some(), corrupt);
        }
    }
}
