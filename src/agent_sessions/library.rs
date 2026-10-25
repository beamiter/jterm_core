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
pub const SESSION_BATCH_LIMIT: usize = MAX_PENDING;
const LOCK_TIMEOUT: Duration = Duration::from_secs(2);
pub const SESSION_ALIAS_MAX_BYTES: usize = 160;
pub const SESSION_TAG_MAX_BYTES: usize = 32;
pub const SESSION_TAG_LIMIT: usize = 12;

/// Explicit, reversible organization of a captured set of session identities.
/// Tag operations merge relative to the latest disk state, preserving aliases
/// and tags concurrently added by another terminal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionBatchAction {
    Pin(bool),
    Archive(bool),
    AddTag(String),
    RemoveTag(String),
}

impl SessionBatchAction {
    fn normalized(self) -> Result<Self, String> {
        match self {
            Self::AddTag(tag) | Self::RemoveTag(tag) if tag.trim().is_empty() => {
                Err("Enter one session tag for the batch operation.".into())
            }
            Self::AddTag(tag) => Ok(Self::AddTag(normalize_batch_tag(tag)?)),
            Self::RemoveTag(tag) => Ok(Self::RemoveTag(normalize_batch_tag(tag)?)),
            action => Ok(action),
        }
    }
}

fn normalize_batch_tag(tag: String) -> Result<String, String> {
    if tag.len() > SESSION_TAG_MAX_BYTES * 4 {
        return Err("Each session tag must be at most 32 bytes.".into());
    }
    let organization = SessionOrganization {
        tags: vec![tag],
        ..SessionOrganization::default()
    }
    .normalized()?;
    organization
        .tags
        .into_iter()
        .next()
        .ok_or_else(|| "Enter one session tag for the batch operation.".into())
}

/// Personal organization only. This never changes a provider transcript,
/// process, approval, or native task lifecycle.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionOrganization {
    pub alias: Option<String>,
    pub tags: Vec<String>,
    pub archived: bool,
}

impl SessionOrganization {
    fn normalized(self) -> Result<Self, String> {
        let alias = self.alias.unwrap_or_default();
        if alias.len() > SESSION_ALIAS_MAX_BYTES || unsafe_label(&alias) {
            return Err(
                "A session name must be at most 160 bytes without control characters.".into(),
            );
        }
        if self.tags.len() > SESSION_TAG_LIMIT {
            return Err("Use at most 12 session tags.".into());
        }
        let mut tags = Vec::new();
        for tag in self.tags {
            if unsafe_label(&tag) || tag.contains(',') {
                return Err("Tags cannot contain commas or control characters.".into());
            }
            let tag = tag.trim().to_lowercase();
            if tag.len() > SESSION_TAG_MAX_BYTES {
                return Err("Each session tag must be at most 32 bytes.".into());
            }
            if !tag.is_empty() && !tags.contains(&tag) {
                tags.push(tag);
            }
        }
        tags.sort();
        Ok(Self {
            alias: (!alias.trim().is_empty()).then(|| alias.trim().to_owned()),
            tags,
            archived: self.archived,
        })
    }
    fn is_empty(&self) -> bool {
        self.alias.is_none() && self.tags.is_empty() && !self.archived
    }
}
fn unsafe_label(text: &str) -> bool {
    text.chars().any(char::is_control) || crate::review_input::contains_visual_spoofing(text)
}

/// Validate a name/tags editor exactly as `SessionLibraryStore::set_details`
/// will, without queueing a write. Hosts can show the explanation while the
/// user types instead of discovering a byte limit only after Save.
pub fn validate_session_details(alias: &str, tags: &str) -> Result<(), String> {
    editor_details(alias, tags).map(|_| ())
}

/// Validate the single tag of a batch Add/Remove tag action exactly as
/// `SessionLibraryStore::apply_batch` will, without queueing a write.
pub fn validate_session_batch_tag(tag: &str) -> Result<(), String> {
    SessionBatchAction::AddTag(tag.to_owned())
        .normalized()
        .map(|_| ())
}

fn editor_details(alias: &str, tags: &str) -> Result<SessionOrganization, String> {
    // Bound input before splitting or allocating user-controlled strings.
    // Editors render saved tags with `join(", ")`, so the raw budget must
    // include both delimiter bytes even when all 12 tags fill their limit.
    if alias.len() > SESSION_ALIAS_MAX_BYTES {
        return Err(format!(
            "A session name must be at most {SESSION_ALIAS_MAX_BYTES} bytes."
        ));
    }
    if tags.len() > (SESSION_TAG_MAX_BYTES + 2) * SESSION_TAG_LIMIT {
        return Err(format!(
            "Use at most {SESSION_TAG_LIMIT} tags of {SESSION_TAG_MAX_BYTES} bytes each."
        ));
    }
    if unsafe_label(tags) {
        return Err("Tags cannot contain control or formatting characters.".into());
    }
    SessionOrganization {
        alias: Some(alias.into()),
        tags: if tags.trim().is_empty() {
            Vec::new()
        } else {
            tags.split(',').map(str::to_owned).collect()
        },
        archived: false,
    }
    .normalized()
}

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
    organization: BTreeMap<String, SessionOrganization>,
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
    /// Construct or augment a snapshot without disk access. The same validation
    /// is used when reading persistent metadata and accepting editor input.
    pub fn with_organization(
        mut self,
        provider: SessionProvider,
        id: &str,
        organization: SessionOrganization,
    ) -> Result<Self, String> {
        if !valid_id(id) {
            return Err("Session library contains an invalid session UUID.".into());
        }
        let organization = organization.normalized()?;
        let identity = key(provider, id);
        if organization.is_empty() {
            self.organization.remove(&identity);
            if self
                .entries
                .get(&identity)
                .is_some_and(|entry| !entry.pinned && entry.last_resumed_ms.is_none())
            {
                self.entries.remove(&identity);
            }
        } else {
            self.entries
                .entry(identity.clone())
                .or_insert_with(|| SessionLibraryEntry {
                    provider,
                    id: id.to_ascii_lowercase(),
                    pinned: false,
                    last_resumed_ms: None,
                });
            self.organization.insert(identity, organization);
        }
        if self.entries.len() > MAX_ENTRIES {
            return Err("Session library exceeds its 4,096-entry limit.".into());
        }
        Ok(self)
    }
    pub fn organization(
        &self,
        provider: SessionProvider,
        id: &str,
    ) -> Option<&SessionOrganization> {
        self.organization.get(&key(provider, id))
    }
    pub fn alias(&self, provider: SessionProvider, id: &str) -> Option<&str> {
        self.organization(provider, id)
            .and_then(|entry| entry.alias.as_deref())
    }
    pub fn tags(&self, provider: SessionProvider, id: &str) -> &[String] {
        self.organization(provider, id)
            .map_or(&[], |entry| entry.tags.as_slice())
    }
    pub fn is_archived(&self, provider: SessionProvider, id: &str) -> bool {
        self.organization(provider, id)
            .is_some_and(|entry| entry.archived)
    }
    pub fn display_title<'a>(
        &'a self,
        provider: SessionProvider,
        id: &str,
        fallback: &'a str,
    ) -> &'a str {
        self.alias(provider, id).unwrap_or(fallback)
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
    /// Every library fact for one conversation from a single identity
    /// lookup. Row renderers should prefer this to separate pin, archive,
    /// alias, tag and resume queries, which each rebuild the identity key.
    pub fn facts(&self, provider: SessionProvider, id: &str) -> SessionLibraryFacts<'_> {
        let identity = key(provider, id);
        let entry = self.entries.get(&identity);
        let organization = self.organization.get(&identity);
        SessionLibraryFacts {
            pinned: entry.is_some_and(|entry| entry.pinned),
            last_resumed_ms: entry.and_then(|entry| entry.last_resumed_ms),
            archived: organization.is_some_and(|organization| organization.archived),
            alias: organization.and_then(|organization| organization.alias.as_deref()),
            tags: organization.map_or(&[], |organization| organization.tags.as_slice()),
        }
    }
    /// Resolve library facts for identities prepared by [`key`] and supplied
    /// in ascending key order. One linear merge with both ordered maps
    /// replaces a tree lookup per pin, archive, alias and tag question.
    /// Repeated identities receive the same facts.
    pub(super) fn resolve_sorted<'a, 'k>(
        &'a self,
        sorted: impl Iterator<Item = (usize, &'k str)>,
        facts: &mut [SessionLibraryFacts<'a>],
    ) {
        let mut entries = self.entries.iter().peekable();
        let mut organization = self.organization.iter().peekable();
        for (index, identity) in sorted {
            while entries
                .next_if(|(key, _)| key.as_str() < identity)
                .is_some()
            {}
            if let Some(&(key, entry)) = entries.peek() {
                if key == identity {
                    facts[index].pinned = entry.pinned;
                    facts[index].last_resumed_ms = entry.last_resumed_ms;
                }
            }
            while organization
                .next_if(|(key, _)| key.as_str() < identity)
                .is_some()
            {}
            if let Some(&(key, details)) = organization.peek() {
                if key == identity {
                    facts[index].archived = details.archived;
                    facts[index].alias = details.alias.as_deref();
                    facts[index].tags = details.tags.as_slice();
                }
            }
        }
    }
}

/// Borrowed personal organization for one conversation identity. These are
/// presentation facts; none of them launches, resumes or stops a provider.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SessionLibraryFacts<'a> {
    pub pinned: bool,
    /// Most recent launch/switch receipt, never proof of a completed turn.
    pub last_resumed_ms: Option<u64>,
    /// Local archive, independent of the provider's archived store.
    pub archived: bool,
    pub alias: Option<&'a str>,
    pub tags: &'a [String],
}

impl<'a> SessionLibraryFacts<'a> {
    /// The local name when one is saved, otherwise the provider title.
    pub fn display_title(&self, fallback: &'a str) -> &'a str {
        self.alias.unwrap_or(fallback)
    }
}

/// Stable library identity: provider label plus the ASCII-lowercased UUID.
pub(super) fn key(provider: SessionProvider, id: &str) -> String {
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
    /// The in-flight transaction carries user changes, not only a reload.
    writing: bool,
    closed: bool,
    /// Newest snapshot read or written since the UI last polled.
    latest: Option<SessionLibrarySnapshot>,
    /// Newest unacknowledged failure. A failed write is kept until the UI
    /// polls or a later write succeeds; a read-only reload cannot hide it.
    failure: Option<WorkerFailure>,
    /// A write transaction finished since the UI last polled.
    wrote: bool,
}
struct WorkerFailure {
    message: String,
    write: bool,
}
impl WorkerState {
    fn has_result(&self) -> bool {
        self.latest.is_some() || self.failure.is_some()
    }
    fn finish(&mut self, write: bool, result: Result<SessionLibrarySnapshot, String>) {
        self.working = false;
        self.writing = false;
        self.wrote |= write;
        match result {
            Ok(snapshot) => {
                self.latest = Some(snapshot);
                if write || self.failure.as_ref().is_some_and(|failure| !failure.write) {
                    self.failure = None;
                }
            }
            Err(message) => {
                if write || !self.failure.as_ref().is_some_and(|failure| failure.write) {
                    self.failure = Some(WorkerFailure { message, write });
                }
            }
        }
    }
}
#[derive(Clone)]
struct Change {
    provider: SessionProvider,
    id: String,
    pinned: Option<bool>,
    resumed: Option<u64>,
    details: Option<(Option<String>, Vec<String>)>,
    archived: Option<bool>,
    tag_changes: BTreeMap<String, bool>,
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
                            state.writing = !state.pending.is_empty();
                            std::mem::take(&mut state.pending)
                        };
                        let write = !job.is_empty();
                        let result = std::panic::catch_unwind(|| transaction(&path, &job))
                            .unwrap_or_else(|_| {
                                Err(
                                    "Session library worker failed; reopen the manager to retry."
                                        .into(),
                                )
                            });
                        let Ok(mut state) = shared.lock() else { return };
                        state.finish(write, result);
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
    /// Any queued, running or unacknowledged worker result, including the
    /// periodic read-only reload that observes other terminal windows.
    pub fn is_busy(&self) -> bool {
        self.available
            && self.state.lock().is_ok_and(|state| {
                state.working || state.refresh || !state.pending.is_empty() || state.has_result()
            })
    }
    /// User changes are queued, being written, or finished without having
    /// been acknowledged by `poll`. A read-only reload never counts, so
    /// hosts can show "Saving" and gate editing without a periodic flicker.
    pub fn is_saving(&self) -> bool {
        self.available
            && self
                .state
                .lock()
                .is_ok_and(|state| state.writing || !state.pending.is_empty() || state.wrote)
    }
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
    /// Apply the newest worker result. Snapshot/error changes both trigger a
    /// repaint; writes progress even if the UI does not call this method.
    pub fn poll(&mut self) -> bool {
        let Some((snapshot, failure)) = self.state.lock().ok().and_then(|mut state| {
            state.has_result().then(|| {
                state.wrote = false;
                (state.latest.take(), state.failure.take())
            })
        }) else {
            return false;
        };
        let was_loaded = self.loaded;
        let old_error = self.error.take();
        self.loaded = true;
        let mut changed = !was_loaded;
        if let Some(snapshot) = snapshot {
            changed |= self.snapshot != snapshot;
            self.snapshot = snapshot;
        }
        // A result without a failure is a success and clears the old error.
        self.error = failure.map(|failure| failure.message);
        changed || self.error != old_error
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
        self.enqueue(provider, id, Some(pinned), None, None, None)
    }
    /// Save the editor as one transaction. Pin/history/archive updates from
    /// other windows are merged independently under the cross-process lock.
    pub fn set_details(
        &mut self,
        provider: SessionProvider,
        id: &str,
        alias: &str,
        tags: &str,
    ) -> Result<(), String> {
        let organization = editor_details(alias, tags)?;
        self.enqueue(
            provider,
            id,
            None,
            None,
            Some((organization.alias, organization.tags)),
            None,
        )
    }
    /// Reversible local archive. Provider-owned history and running processes
    /// remain unchanged, and no launch/action capability is created.
    pub fn set_archived(
        &mut self,
        provider: SessionProvider,
        id: &str,
        archived: bool,
    ) -> Result<(), String> {
        self.enqueue(provider, id, None, None, None, Some(archived))
    }
    pub fn record_resumed(&mut self, provider: SessionProvider, id: &str) -> Result<(), String> {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u64::MAX as u128) as u64;
        self.enqueue(provider, id, None, Some(timestamp.max(1)), None, None)
    }

    /// Validate and admit every target before changing the bounded queue. The
    /// worker applies the captured identities in one atomic disk transaction.
    /// This returns the unique accepted count, not an optimistic success receipt;
    /// callers must poll completion and preserve selection when saving fails.
    pub fn apply_batch(
        &mut self,
        identities: &[(SessionProvider, String)],
        action: SessionBatchAction,
    ) -> Result<usize, String> {
        if identities.is_empty() || identities.len() > SESSION_BATCH_LIMIT {
            return Err("Select between 1 and 256 conversations for a batch operation.".into());
        }
        let action = action.normalized()?;
        let mut targets = BTreeMap::new();
        for (provider, id) in identities {
            if !valid_id(id) {
                return Err("Cannot update session library: invalid session UUID; no batch changes were queued.".into());
            }
            targets.insert(key(*provider, id), (*provider, id.to_ascii_lowercase()));
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
        let additional = targets
            .keys()
            .filter(|identity| !state.pending.contains_key(*identity))
            .count();
        if state.pending.len().saturating_add(additional) > MAX_PENDING {
            return Err("Session library is busy; no batch changes were queued. Wait for pending changes to finish.".into());
        }
        let count = targets.len();
        for (identity, (provider, id)) in targets {
            let change = state.pending.entry(identity).or_insert_with(|| Change {
                provider,
                id,
                pinned: None,
                resumed: None,
                details: None,
                archived: None,
                tag_changes: BTreeMap::new(),
            });
            match &action {
                SessionBatchAction::Pin(value) => change.pinned = Some(*value),
                SessionBatchAction::Archive(value) => change.archived = Some(*value),
                SessionBatchAction::AddTag(tag) => {
                    change.tag_changes.insert(tag.clone(), true);
                }
                SessionBatchAction::RemoveTag(tag) => {
                    change.tag_changes.insert(tag.clone(), false);
                }
            }
        }
        drop(state);
        let _ = self.wake.try_send(());
        Ok(count)
    }
    fn enqueue(
        &mut self,
        provider: SessionProvider,
        id: &str,
        pinned: Option<bool>,
        resumed: Option<u64>,
        details: Option<(Option<String>, Vec<String>)>,
        archived: Option<bool>,
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
            details: None,
            archived: None,
            tag_changes: BTreeMap::new(),
        });
        if pinned.is_some() {
            change.pinned = pinned;
        }
        if let Some(timestamp) = resumed {
            change.resumed = Some(change.resumed.unwrap_or(0).max(timestamp));
        }
        if details.is_some() {
            change.details = details;
            // An explicitly saved editor replaces the tags visible there;
            // subsequent batch tag operations still apply after this save.
            change.tag_changes.clear();
        }
        if archived.is_some() {
            change.archived = archived;
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    alias: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tags: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    archived: bool,
}
fn decode(text: &str) -> Result<SessionLibrarySnapshot, String> {
    crate::bounded_json::validate_no_duplicate_members(text.as_bytes())
        .map_err(|_| "Session library contains duplicate or malformed JSON fields.")?;
    let wire: WireLibrary = serde_json::from_str(text)
        .map_err(|_| "Session library JSON is malformed; it was left unchanged.")?;
    if !matches!(wire.version, 1 | 2) {
        return Err("Session library version is unsupported; it was left unchanged.".into());
    }
    let mut entries = Vec::new();
    let mut organization = Vec::new();
    for entry in wire.sessions {
        if wire.version == 1 && (entry.alias.is_some() || !entry.tags.is_empty() || entry.archived)
        {
            return Err(
                "Session organization requires library version 2; it was left unchanged.".into(),
            );
        }
        let provider = match entry.provider.as_str() {
            "codex" => SessionProvider::Codex,
            "claude" => SessionProvider::Claude,
            _ => return Err("Session library contains an unsupported provider.".into()),
        };
        organization.push((
            provider,
            entry.id.clone(),
            SessionOrganization {
                alias: entry.alias,
                tags: entry.tags,
                archived: entry.archived,
            },
        ));
        entries.push(SessionLibraryEntry {
            provider,
            id: entry.id,
            pinned: entry.pinned,
            last_resumed_ms: entry.last_resumed_ms,
        });
    }
    let mut snapshot = SessionLibrarySnapshot::from_entries(entries)?;
    for (provider, id, details) in organization {
        snapshot = snapshot.with_organization(provider, &id, details)?;
    }
    Ok(snapshot)
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
        if change.details.is_some() || change.archived.is_some() || !change.tag_changes.is_empty() {
            let organization = snapshot.organization.entry(identity.clone()).or_default();
            if let Some((alias, tags)) = &change.details {
                organization.alias.clone_from(alias);
                organization.tags.clone_from(tags);
            }
            if let Some(archived) = change.archived {
                organization.archived = archived;
            }
            for (tag, present) in &change.tag_changes {
                if *present {
                    if !organization.tags.contains(tag) {
                        organization.tags.push(tag.clone());
                    }
                } else {
                    organization.tags.retain(|existing| existing != tag);
                }
            }
            // Check every merged target before the atomic file replacement.
            // A full target fails the entire transaction, never a partial batch.
            *organization = organization.clone().normalized()?;
        }
    }
    snapshot.organization.retain(|_, entry| !entry.is_empty());
    snapshot.entries.retain(|identity, entry| {
        entry.pinned
            || entry.last_resumed_ms.is_some()
            || snapshot.organization.contains_key(identity)
    });
    if snapshot.entries.len() > MAX_ENTRIES {
        return Err("Session library is full (4,096 identities); no changes were written.".into());
    }
    let wire = WireLibrary {
        version: 2,
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
                alias: snapshot.alias(entry.provider, &entry.id).map(str::to_owned),
                tags: snapshot.tags(entry.provider, &entry.id).to_vec(),
                archived: snapshot.is_archived(entry.provider, &entry.id),
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
                details: None,
                archived: None,
                tag_changes: BTreeMap::new(),
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
    fn batch_validation_and_capacity_admission_never_queue_a_partial_selection() {
        let (wake, _receiver) = mpsc::sync_channel(1);
        let mut store = SessionLibraryStore {
            snapshot: SessionLibrarySnapshot::default(),
            state: Arc::new(Mutex::new(WorkerState::default())),
            wake,
            loaded: true,
            available: true,
            error: None,
        };
        let bad = vec![
            (SessionProvider::Codex, ID.into()),
            (SessionProvider::Codex, "invalid".into()),
        ];
        assert!(store
            .apply_batch(&bad, SessionBatchAction::Pin(true))
            .is_err());
        assert!(store.state.lock().unwrap().pending.is_empty());
        for number in 0..(MAX_PENDING - 1) {
            store
                .set_pinned(
                    SessionProvider::Codex,
                    &format!("{number:08x}-1111-4111-8111-111111111111"),
                    false,
                )
                .unwrap();
        }
        let existing = "00000000-1111-4111-8111-111111111111";
        let targets = vec![
            (SessionProvider::Codex, existing.into()),
            (SessionProvider::Codex, ID.into()),
            (SessionProvider::Claude, OTHER.into()),
        ];
        assert!(store
            .apply_batch(&targets, SessionBatchAction::Pin(true))
            .is_err());
        let state = store.state.lock().unwrap();
        assert_eq!(state.pending.len(), MAX_PENDING - 1);
        assert_eq!(
            state.pending[&key(SessionProvider::Codex, existing)].pinned,
            Some(false)
        );
        assert!(!state.pending.contains_key(&key(SessionProvider::Codex, ID)));
    }

    #[test]
    fn batch_tags_merge_with_other_windows_and_preserve_identity_fields() {
        let fixture = Fixture::new();
        let mut first = SessionLibraryStore::with_path(fixture.path());
        let mut second = SessionLibraryStore::with_path(fixture.path());
        settle(&mut first);
        settle(&mut second);
        first
            .set_details(SessionProvider::Codex, ID, "Keep this alias", "existing")
            .unwrap();
        first.record_resumed(SessionProvider::Codex, ID).unwrap();
        settle(&mut first);
        let receipt = first.snapshot().last_resumed(SessionProvider::Codex, ID);
        let targets = vec![
            (SessionProvider::Codex, ID.into()),
            (SessionProvider::Claude, OTHER.into()),
        ];
        let held = LibraryLock::acquire(&fixture.path()).unwrap();
        assert_eq!(
            first
                .apply_batch(&targets, SessionBatchAction::AddTag(" REVIEW ".into()))
                .unwrap(),
            2
        );
        second
            .apply_batch(&targets, SessionBatchAction::AddTag("urgent".into()))
            .unwrap();
        drop(held);
        settle(&mut first);
        settle(&mut second);
        first.refresh();
        settle(&mut first);
        assert_eq!(
            first.snapshot().tags(SessionProvider::Codex, ID),
            ["existing", "review", "urgent"]
        );
        assert_eq!(
            first.snapshot().tags(SessionProvider::Claude, OTHER),
            ["review", "urgent"]
        );
        assert_eq!(
            first.snapshot().alias(SessionProvider::Codex, ID),
            Some("Keep this alias")
        );
        assert_eq!(
            first.snapshot().last_resumed(SessionProvider::Codex, ID),
            receipt
        );
        first
            .apply_batch(&targets, SessionBatchAction::RemoveTag("review".into()))
            .unwrap();
        first
            .apply_batch(&targets, SessionBatchAction::Pin(true))
            .unwrap();
        first
            .apply_batch(&targets, SessionBatchAction::Archive(true))
            .unwrap();
        settle(&mut first);
        for (provider, id) in &targets {
            assert!(first.snapshot().is_pinned(*provider, id));
            assert!(first.snapshot().is_archived(*provider, id));
            assert!(!first
                .snapshot()
                .tags(*provider, id)
                .contains(&"review".into()));
        }
        first
            .apply_batch(&targets, SessionBatchAction::Archive(false))
            .unwrap();
        first
            .apply_batch(&targets, SessionBatchAction::Pin(false))
            .unwrap();
        settle(&mut first);
        assert_eq!(
            first.snapshot().alias(SessionProvider::Codex, ID),
            Some("Keep this alias")
        );
        assert_eq!(
            first.snapshot().last_resumed(SessionProvider::Codex, ID),
            receipt
        );
    }

    #[test]
    fn one_full_tag_set_aborts_the_entire_batch_on_disk() {
        let fixture = Fixture::new();
        let mut store = SessionLibraryStore::with_path(fixture.path());
        settle(&mut store);
        let tags = (0..SESSION_TAG_LIMIT)
            .map(|i| format!("tag{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        store
            .set_details(SessionProvider::Codex, ID, "Full", &tags)
            .unwrap();
        store
            .set_details(SessionProvider::Claude, OTHER, "Other", "keep")
            .unwrap();
        settle(&mut store);
        let before = fs::read(fixture.path()).unwrap();
        let targets = vec![
            (SessionProvider::Claude, OTHER.into()),
            (SessionProvider::Codex, ID.into()),
        ];
        store
            .apply_batch(&targets, SessionBatchAction::AddTag("overflow".into()))
            .unwrap();
        settle(&mut store);
        assert!(store.error().is_some());
        assert_eq!(fs::read(fixture.path()).unwrap(), before);
        assert_eq!(
            store.snapshot().tags(SessionProvider::Claude, OTHER),
            ["keep"]
        );
    }

    #[test]
    fn batch_deduplicates_provider_identity_and_respects_editor_order() {
        let (wake, _receiver) = mpsc::sync_channel(1);
        let mut store = SessionLibraryStore {
            snapshot: SessionLibrarySnapshot::default(),
            state: Arc::new(Mutex::new(WorkerState::default())),
            wake,
            loaded: true,
            available: true,
            error: None,
        };
        let targets = vec![
            (SessionProvider::Codex, ID.into()),
            (SessionProvider::Codex, ID.to_uppercase()),
            (SessionProvider::Claude, ID.into()),
        ];
        assert_eq!(
            store
                .apply_batch(&targets, SessionBatchAction::AddTag("before".into()))
                .unwrap(),
            2
        );
        store
            .set_details(SessionProvider::Codex, ID, "Saved", "explicit")
            .unwrap();
        store
            .apply_batch(&targets, SessionBatchAction::AddTag("after".into()))
            .unwrap();
        let fixture = Fixture::new();
        let snapshot = transaction(&fixture.path(), &store.state.lock().unwrap().pending).unwrap();
        assert_eq!(
            snapshot.tags(SessionProvider::Codex, ID),
            ["after", "explicit"]
        );
        assert_eq!(
            snapshot.tags(SessionProvider::Claude, ID),
            ["after", "before"]
        );
        assert_eq!(snapshot.alias(SessionProvider::Codex, ID), Some("Saved"));
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
    fn legacy_library_migrates_organization_without_losing_pin_or_resume_receipts() {
        let f = Fixture::new();
        f.write(format!(r#"{{"version":1,"sessions":[{{"provider":"codex","id":"{ID}","pinned":true,"last_resumed_ms":42}}]}}"#).as_bytes());
        let mut store = SessionLibraryStore::with_path(f.path());
        settle(&mut store);
        store
            .set_details(
                SessionProvider::Codex,
                ID,
                "  Renderer overhaul  ",
                "GUI, 项目, gui",
            )
            .unwrap();
        store
            .set_archived(SessionProvider::Codex, ID, true)
            .unwrap();
        settle(&mut store);
        assert!(store.error().is_none());
        let mut reopened = SessionLibraryStore::with_path(f.path());
        settle(&mut reopened);
        let snapshot = reopened.snapshot();
        assert_eq!(
            snapshot.alias(SessionProvider::Codex, ID),
            Some("Renderer overhaul")
        );
        assert_eq!(snapshot.tags(SessionProvider::Codex, ID), ["gui", "项目"]);
        assert!(snapshot.is_archived(SessionProvider::Codex, ID));
        assert!(snapshot.is_pinned(SessionProvider::Codex, ID));
        assert_eq!(snapshot.last_resumed(SessionProvider::Codex, ID), Some(42));
        assert_eq!(snapshot.alias(SessionProvider::Claude, ID), None);
        let wire: serde_json::Value = serde_json::from_slice(&fs::read(f.path()).unwrap()).unwrap();
        assert_eq!(wire["version"], 2);
    }
    #[test]
    fn multiple_windows_merge_details_archive_pin_and_history_for_the_same_identity() {
        let f = Fixture::new();
        let mut editor = SessionLibraryStore::with_path(f.path());
        let mut browser = SessionLibraryStore::with_path(f.path());
        settle(&mut editor);
        settle(&mut browser);
        let held = LibraryLock::acquire(&f.path()).unwrap();
        editor
            .set_details(SessionProvider::Codex, ID, "Release", "ready, review")
            .unwrap();
        browser
            .set_archived(SessionProvider::Codex, ID, true)
            .unwrap();
        browser
            .set_pinned(SessionProvider::Codex, ID, true)
            .unwrap();
        browser.record_resumed(SessionProvider::Codex, ID).unwrap();
        drop(held);
        settle(&mut editor);
        settle(&mut browser);
        editor.refresh();
        settle(&mut editor);
        let snapshot = editor.snapshot();
        assert_eq!(snapshot.alias(SessionProvider::Codex, ID), Some("Release"));
        assert_eq!(
            snapshot.tags(SessionProvider::Codex, ID),
            ["ready", "review"]
        );
        assert!(snapshot.is_pinned(SessionProvider::Codex, ID));
        assert!(snapshot.is_archived(SessionProvider::Codex, ID));
        assert!(snapshot.last_resumed(SessionProvider::Codex, ID).is_some());
        // Clearing details must not clear independently written archive/pin/history.
        editor
            .set_details(SessionProvider::Codex, ID, "", "")
            .unwrap();
        settle(&mut editor);
        assert_eq!(editor.snapshot().alias(SessionProvider::Codex, ID), None);
        assert!(editor.snapshot().is_archived(SessionProvider::Codex, ID));
        assert!(editor.snapshot().is_pinned(SessionProvider::Codex, ID));
    }
    #[test]
    fn metadata_only_identity_survives_then_disappears_after_explicit_clear() {
        let f = Fixture::new();
        let mut store = SessionLibraryStore::with_path(f.path());
        store
            .set_details(SessionProvider::Claude, ID, "项目", "")
            .unwrap();
        settle(&mut store);
        assert_eq!(store.snapshot().len(), 1);
        assert_eq!(
            store.snapshot().last_resumed(SessionProvider::Claude, ID),
            None
        );
        store
            .set_archived(SessionProvider::Claude, ID, true)
            .unwrap();
        store
            .set_details(SessionProvider::Claude, ID, "", "")
            .unwrap();
        settle(&mut store);
        assert_eq!(store.snapshot().len(), 1);
        store
            .set_archived(SessionProvider::Claude, ID, false)
            .unwrap();
        settle(&mut store);
        assert!(store.snapshot().is_empty());
    }
    #[test]
    fn invalid_editor_values_do_not_replace_saved_organization() {
        let f = Fixture::new();
        let mut store = SessionLibraryStore::with_path(f.path());
        store
            .set_details(SessionProvider::Codex, ID, "Keep me", "good")
            .unwrap();
        settle(&mut store);
        let original = fs::read(f.path()).unwrap();
        for (alias, tags) in [
            ("x".repeat(161), "good".into()),
            ("ok".into(), "x".repeat(33)),
            ("invisible\u{202e}".into(), "good".into()),
            ("newline\nname".into(), "good".into()),
            ("ok".into(), "\n".into()),
            ("ok".into(), "tag,".repeat(13)),
        ] {
            assert!(store
                .set_details(SessionProvider::Codex, ID, &alias, &tags)
                .is_err());
        }
        assert!(store
            .set_archived(SessionProvider::Codex, "--last", true)
            .is_err());
        settle(&mut store);
        assert_eq!(fs::read(f.path()).unwrap(), original);
    }
    #[test]
    fn full_tag_set_can_be_reopened_and_saved_using_editor_delimiters() {
        let f = Fixture::new();
        let tags = (0..SESSION_TAG_LIMIT)
            .map(|index| format!("{index:02}{}", "x".repeat(SESSION_TAG_MAX_BYTES - 2)))
            .collect::<Vec<_>>();
        {
            let mut store = SessionLibraryStore::with_path(f.path());
            store
                .set_details(SessionProvider::Codex, ID, "Original", &tags.join(","))
                .unwrap();
            settle(&mut store);
        }
        let mut reopened = SessionLibraryStore::with_path(f.path());
        settle(&mut reopened);
        let editor_tags = reopened
            .snapshot()
            .tags(SessionProvider::Codex, ID)
            .join(", ");
        assert_eq!(editor_tags.len(), 406);
        reopened
            .set_details(SessionProvider::Codex, ID, "Renamed", &editor_tags)
            .unwrap();
        settle(&mut reopened);
        assert!(reopened.error().is_none());
        assert_eq!(reopened.snapshot().tags(SessionProvider::Codex, ID), tags);
        assert_eq!(
            reopened.snapshot().alias(SessionProvider::Codex, ID),
            Some("Renamed")
        );
    }
    #[test]
    fn archive_and_details_queue_coalesce_without_erasing_one_another() {
        let f = Fixture::new();
        let held = LibraryLock::acquire(&f.path()).unwrap();
        let mut store = SessionLibraryStore::with_path(f.path());
        store
            .set_details(SessionProvider::Claude, ID, "First draft", "one")
            .unwrap();
        store
            .set_archived(SessionProvider::Claude, ID, true)
            .unwrap();
        store.set_pinned(SessionProvider::Claude, ID, true).unwrap();
        store
            .set_details(SessionProvider::Claude, ID, "Final draft", "two")
            .unwrap();
        store
            .set_archived(SessionProvider::Claude, ID, false)
            .unwrap();
        drop(held);
        settle(&mut store);
        assert!(store.error().is_none());
        assert_eq!(
            store.snapshot().alias(SessionProvider::Claude, ID),
            Some("Final draft")
        );
        assert_eq!(store.snapshot().tags(SessionProvider::Claude, ID), ["two"]);
        assert!(store.snapshot().is_pinned(SessionProvider::Claude, ID));
        assert!(!store.snapshot().is_archived(SessionProvider::Claude, ID));
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
            br#"{"version":3,"sessions":[]}"#.to_vec(),
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
                if store.state.lock().unwrap().has_result() {
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

    /// Wait for the worker to finish everything queued, without polling.
    fn wait_for_unacknowledged_result(store: &SessionLibraryStore) {
        let start = Instant::now();
        loop {
            {
                let state = store.state.lock().unwrap();
                if !state.working
                    && !state.refresh
                    && state.pending.is_empty()
                    && state.has_result()
                {
                    return;
                }
            }
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "library worker did not finish"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn row_facts_match_individual_queries_for_every_identity_shape() {
        let library = SessionLibrarySnapshot::from_entries(vec![SessionLibraryEntry {
            provider: SessionProvider::Codex,
            id: ID.to_uppercase(),
            pinned: true,
            last_resumed_ms: Some(7),
        }])
        .unwrap()
        .with_organization(
            SessionProvider::Claude,
            OTHER,
            SessionOrganization {
                alias: Some("Named".into()),
                tags: vec!["b".into(), "a".into()],
                archived: true,
            },
        )
        .unwrap();
        let upper = ID.to_uppercase();
        for (provider, id) in [
            (SessionProvider::Codex, ID),
            (SessionProvider::Codex, upper.as_str()),
            (SessionProvider::Claude, OTHER),
            (SessionProvider::Claude, ID),
            (SessionProvider::Codex, OTHER),
        ] {
            let facts = library.facts(provider, id);
            assert_eq!(facts.pinned, library.is_pinned(provider, id));
            assert_eq!(facts.last_resumed_ms, library.last_resumed(provider, id));
            assert_eq!(facts.archived, library.is_archived(provider, id));
            assert_eq!(facts.alias, library.alias(provider, id));
            assert_eq!(facts.tags, library.tags(provider, id));
            assert_eq!(
                facts.display_title("Provider title"),
                library.display_title(provider, id, "Provider title")
            );
        }
        let named = library.facts(SessionProvider::Claude, OTHER);
        assert_eq!(
            (named.alias, named.tags, named.archived),
            (Some("Named"), &["a".to_string(), "b".to_string()][..], true)
        );
        assert_eq!(
            library.facts(SessionProvider::Codex, OTHER),
            SessionLibraryFacts::default()
        );
    }

    #[test]
    fn batch_tag_validation_matches_batch_admission() {
        let fixture = Fixture::new();
        let mut store = SessionLibraryStore::with_path(fixture.path());
        settle(&mut store);
        let padded = format!("  {}  ", "t".repeat(SESSION_TAG_MAX_BYTES));
        let long = "t".repeat(SESSION_TAG_MAX_BYTES + 1);
        let huge = "t".repeat(SESSION_TAG_MAX_BYTES * 4 + 1);
        for tag in [
            "review",
            "  Review  ",
            "",
            "   ",
            "a,b",
            "bad\u{7}",
            padded.as_str(),
            long.as_str(),
            huge.as_str(),
        ] {
            let validated = validate_session_batch_tag(tag);
            let admitted = store
                .apply_batch(
                    &[(SessionProvider::Codex, ID.into())],
                    SessionBatchAction::AddTag(tag.into()),
                )
                .map(|_| ());
            assert_eq!(validated, admitted, "{tag:?}");
            settle(&mut store);
        }
    }

    #[test]
    fn periodic_reload_is_never_reported_as_saving() {
        let fixture = Fixture::new();
        let mut store = SessionLibraryStore::with_path(fixture.path());
        settle(&mut store);
        assert!(!store.is_saving());
        assert!(store.refresh());
        assert!(store.is_busy());
        assert!(!store.is_saving(), "a read-only reload is not a save");
        wait_for_unacknowledged_result(&store);
        assert!(!store.is_saving());
        assert!(
            store.is_busy(),
            "an unacknowledged reload still awaits poll"
        );
        store.poll();
        assert!(!store.is_busy());

        store.set_pinned(SessionProvider::Codex, ID, true).unwrap();
        assert!(store.is_saving());
        wait_for_unacknowledged_result(&store);
        assert!(
            store.is_saving(),
            "a finished write stays visible until poll acknowledges it"
        );
        assert!(store.poll());
        assert!(!store.is_saving());
        assert!(store.error().is_none());
        assert!(store.snapshot().is_pinned(SessionProvider::Codex, ID));
    }

    #[test]
    fn a_reload_cannot_hide_a_failed_write_before_the_ui_polls() {
        let fixture = Fixture::new();
        let mut store = SessionLibraryStore::with_path(fixture.path());
        settle(&mut store);
        let tags = (0..SESSION_TAG_LIMIT)
            .map(|i| format!("tag{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        store
            .set_details(SessionProvider::Codex, ID, "Full", &tags)
            .unwrap();
        settle(&mut store);
        store
            .apply_batch(
                &[(SessionProvider::Codex, ID.into())],
                SessionBatchAction::AddTag("overflow".into()),
            )
            .unwrap();
        wait_for_unacknowledged_result(&store);
        assert!(
            store.is_saving(),
            "the failed write is not yet acknowledged"
        );
        assert!(
            store.refresh(),
            "another window's reload may run before poll"
        );
        wait_for_unacknowledged_result(&store);
        assert!(store.poll());
        assert!(
            store.error().is_some(),
            "the failed write must remain visible after the successful reload"
        );
        assert!(!store.is_saving());
        assert_eq!(
            store.snapshot().tags(SessionProvider::Codex, ID).len(),
            SESSION_TAG_LIMIT
        );

        // A later reload failure cannot replace an unacknowledged write
        // failure either; a later successful write clears it.
        store
            .apply_batch(
                &[(SessionProvider::Codex, ID.into())],
                SessionBatchAction::AddTag("overflow".into()),
            )
            .unwrap();
        wait_for_unacknowledged_result(&store);
        let write_failure = store
            .state
            .lock()
            .unwrap()
            .failure
            .as_ref()
            .map(|f| f.message.clone());
        store
            .state
            .lock()
            .unwrap()
            .finish(false, Err("reload failed".into()));
        assert_eq!(
            store
                .state
                .lock()
                .unwrap()
                .failure
                .as_ref()
                .map(|f| f.message.clone()),
            write_failure
        );
        store.set_pinned(SessionProvider::Codex, ID, true).unwrap();
        settle(&mut store);
        assert!(store.error().is_none());
        assert!(store.snapshot().is_pinned(SessionProvider::Codex, ID));
    }

    #[test]
    fn editor_validation_matches_save_admission() {
        let fixture = Fixture::new();
        let mut store = SessionLibraryStore::with_path(fixture.path());
        settle(&mut store);
        let long_alias = "名".repeat(SESSION_ALIAS_MAX_BYTES / 3 + 1);
        let too_many = (0..=SESSION_TAG_LIMIT)
            .map(|i| format!("t{i}"))
            .collect::<Vec<_>>()
            .join(",");
        let long_tag = "t".repeat(SESSION_TAG_MAX_BYTES + 1);
        for (alias, tags) in [
            ("Renderer", "gui, review"),
            ("", ""),
            (long_alias.as_str(), ""),
            ("Name", too_many.as_str()),
            ("Name", long_tag.as_str()),
            ("Name", "bad\u{202e}tag"),
            ("Name\u{7}", "a"),
        ] {
            let validated = validate_session_details(alias, tags);
            let saved = store.set_details(SessionProvider::Codex, ID, alias, tags);
            assert_eq!(validated, saved, "{alias:?} {tags:?}");
        }
        settle(&mut store);
    }
}
