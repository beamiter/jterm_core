//! Read-only discovery of provider-owned CLI history, independent of native tasks.
//!
//! Status is the **last recorded transcript state**, never process liveness.
//! Files are sampled at their head and tail within hard byte and entry ceilings;
//! discovery belongs on a worker, not the GUI thread.

mod desktop_launch;
mod library;
mod preflight;
pub use library::{
    SessionBatchAction, SessionLibraryEntry, SessionLibrarySnapshot, SessionLibraryStore,
    SessionOrganization, SESSION_ALIAS_MAX_BYTES, SESSION_BATCH_LIMIT, SESSION_TAG_LIMIT,
    SESSION_TAG_MAX_BYTES,
};
pub use preflight::{SessionPreflight, SessionPreflightState};
mod search;
pub use search::{
    SessionArchiveFilter, SessionLibraryFilter, SessionProjectSummary, SessionQuery,
    SessionSearchIndex, SessionSort,
};

use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_ENTRIES: usize = 32_768;
const MAX_SESSIONS: usize = 2_048;
const MAX_SCAN_BYTES: usize = 128 * 1024 * 1024;
const HEAD_BYTES: usize = 64 * 1024;
const TAIL_BYTES: usize = 64 * 1024;
const MAX_HEAD_BYTES: usize = 1024 * 1024;
const MAX_WARNINGS: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SessionProvider {
    Codex,
    Claude,
}
impl SessionProvider {
    pub fn label(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude",
        }
    }
    pub fn display_name(self) -> &'static str {
        self.label()
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SessionStatus {
    Working,
    Waiting,
    Idle,
    Unknown,
    Archived,
}
impl SessionStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Working => "Last recorded: working",
            Self::Waiting => "Last recorded: needs input",
            Self::Idle => "Ready to resume",
            Self::Unknown => "State unknown",
            Self::Archived => "Archived",
        }
    }
    pub fn display_name(self) -> &'static str {
        self.label()
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentSession {
    pub provider: SessionProvider,
    pub id: String,
    pub title: String,
    pub cwd: PathBuf,
    pub updated_at_ms: u64,
    pub status: SessionStatus,
    pub preview: String,
    pub branch: Option<String>,
    pub source: PathBuf,
    pub archived: bool,
}
impl AgentSession {
    /// All whitespace-separated terms must match some visible metadata field.
    pub fn matches_query(&self, query: &str) -> bool {
        let haystack = format!(
            "{} {} {} {} {} {} {}",
            self.provider.label(),
            self.id,
            self.title,
            self.cwd.to_string_lossy(),
            self.status.label(),
            self.preview,
            self.branch.as_deref().unwrap_or("")
        )
        .to_lowercase();
        query
            .split_whitespace()
            .all(|term| haystack.contains(&term.to_lowercase()))
    }
    /// Revalidate at activation time. No shell is involved in the launch plan.
    pub fn resume_launch(&self) -> Result<SessionLaunch, String> {
        if !valid_id(&self.id) {
            return Err("The provider session ID is invalid; refresh the catalog.".into());
        }
        if !self.cwd.is_absolute() || !self.cwd.is_dir() {
            return Err(format!(
                "The original project directory is unavailable: {}",
                safe_text(&self.cwd.to_string_lossy(), 512)
            ));
        }
        let Some(cwd) = self.cwd.to_str() else {
            return Err("The project path is not valid UTF-8.".into());
        };
        if crate::review_input::validate(cwd).is_err() {
            return Err(
                "The project path contains unsupported control or formatting characters.".into(),
            );
        }
        if !self.source.is_file() {
            return Err("The session history no longer exists; refresh the catalog.".into());
        }
        if self.archived {
            return Err("This session is archived. Unarchive it in Codex before resuming.".into());
        }
        let (program, flag, variable) = match self.provider {
            SessionProvider::Codex => ("codex", "resume", "CODEX_HOME"),
            SessionProvider::Claude => ("claude", "--resume", "CLAUDE_CONFIG_DIR"),
        };
        let mut launch = SessionLaunch {
            program: program.into(),
            args: vec![flag.into(), self.id.clone()],
            cwd: self.cwd.clone(),
        };
        if let Some(source_home) = self.provider_home() {
            let inherited = provider_home(self.provider);
            let inherited = inherited
                .as_ref()
                .and_then(|p| fs::canonicalize(p).ok())
                .or(inherited);
            if inherited.as_ref() != Some(&source_home) {
                let Some(home) = source_home.to_str() else {
                    return Err("The provider history path is not valid UTF-8.".into());
                };
                if crate::review_input::validate(home).is_err() {
                    return Err("The provider history path contains unsupported formatting.".into());
                }
                launch.args = vec![
                    format!("{variable}={home}"),
                    program.into(),
                    flag.into(),
                    self.id.clone(),
                ];
                launch.program = "env".into();
            }
        }
        Ok(launch)
    }
    /// The original provider home is derived from the indexed source path, so
    /// a restored session does not accidentally use another account's store.
    pub fn provider_home(&self) -> Option<PathBuf> {
        let boundary = match self.provider {
            SessionProvider::Codex => ["sessions", "archived_sessions"].as_slice(),
            SessionProvider::Claude => ["projects"].as_slice(),
        };
        let directory = self.source.ancestors().skip(1).find(|path| {
            path.file_name()
                .and_then(|v| v.to_str())
                .is_some_and(|name| boundary.contains(&name))
        })?;
        fs::canonicalize(directory.parent()?).ok()
    }
}
#[derive(Clone, Debug)]
pub struct SessionLaunch {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
}
impl SessionLaunch {
    pub fn display_command(&self) -> String {
        let args = std::iter::once(self.program.clone())
            .chain(self.args.iter().cloned())
            .collect::<Vec<_>>();
        crate::process::shell_quote_argv(&args).unwrap_or_else(|| "Invalid resume command".into())
    }
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionCatalog {
    pub sessions: Vec<AgentSession>,
    pub warnings: Vec<String>,
    pub live_processes: Vec<LiveSessionProcess>,
}
impl SessionCatalog {
    pub fn live_pids(&self, session: &AgentSession) -> Vec<u32> {
        self.live_processes
            .iter()
            .filter(|p| p.provider == session.provider && p.id == session.id)
            .map(|p| p.pid)
            .collect()
    }
    pub fn discover() -> Self {
        let Some(home) = dirs::home_dir() else {
            return Self {
                sessions: vec![],
                warnings: vec!["Cannot locate your home directory.".into()],
                ..Self::default()
            };
        };
        let mut codex = vec![home.join(".codex")];
        let mut claude = vec![home.join(".claude")];
        if let Some(root) = provider_home(SessionProvider::Codex) {
            if !codex.contains(&root) {
                codex.push(root);
            }
        }
        if let Some(root) = provider_home(SessionProvider::Claude) {
            if !claude.contains(&root) {
                claude.push(root);
            }
        }
        Self::discover_roots(&codex, &claude)
    }
    pub fn discover_with_roots(codex_home: &Path, claude_home: &Path) -> Self {
        Self::discover_roots(&[codex_home.to_path_buf()], &[claude_home.to_path_buf()])
    }
    fn discover_roots(codex_homes: &[PathBuf], claude_homes: &[PathBuf]) -> Self {
        let mut catalog = Self::default();
        let mut candidates = Vec::new();
        let mut entries = 0;
        let mut roots = Vec::new();
        for home in codex_homes {
            roots.push((home.join("sessions"), SessionProvider::Codex, false, 5));
            roots.push((
                home.join("archived_sessions"),
                SessionProvider::Codex,
                true,
                5,
            ));
        }
        for home in claude_homes {
            roots.push((home.join("projects"), SessionProvider::Claude, false, 2));
        }
        for (root, provider, archived, depth) in roots {
            collect(
                &root,
                provider,
                archived,
                depth,
                &mut entries,
                &mut candidates,
                &mut catalog,
            );
        }
        candidates.sort_by(|a, b| {
            b.modified
                .cmp(&a.modified)
                .then_with(|| a.path.cmp(&b.path))
        });
        if candidates.len() > MAX_SESSIONS {
            catalog
                .warn("History is large: showing the 2,048 most recently modified session files.");
            candidates.truncate(MAX_SESSIONS);
        }
        let mut bytes_left = MAX_SCAN_BYTES;
        let mut identities = HashMap::<(SessionProvider, String), usize>::new();
        for candidate in candidates {
            if bytes_left < HEAD_BYTES + TAIL_BYTES {
                catalog
                    .warn("History scan reached its 128 MiB budget; older sessions were omitted.");
                break;
            }
            match parse_cached(&candidate, &mut bytes_left) {
                Ok((Some(session), sampled)) => {
                    if sampled {
                        catalog.warn("Some compressed sessions exceed the decompression budget; their last state is unknown.");
                    }
                    let key = (session.provider, session.id.clone());
                    if let Some(&index) = identities.get(&key) {
                        // Prefer ordinary history over archived copies, then newer evidence.
                        let prior = &catalog.sessions[index];
                        if (prior.archived && !session.archived)
                            || (prior.archived == session.archived
                                && session.updated_at_ms > prior.updated_at_ms)
                        {
                            catalog.sessions[index] = session;
                        }
                    } else {
                        identities.insert(key, catalog.sessions.len());
                        catalog.sessions.push(session);
                    }
                }
                Ok((None, _)) => {}
                Err(_) => catalog.warn(
                    "Some session files could not be read; refresh after checking file access.",
                ),
            }
        }
        apply_session_titles(&mut catalog, codex_homes);
        catalog.live_processes = discover_live_processes(&catalog.sessions);
        catalog.sessions.sort_by(|a, b| {
            b.updated_at_ms
                .cmp(&a.updated_at_ms)
                .then_with(|| a.id.cmp(&b.id))
        });
        catalog
    }
    fn warn(&mut self, message: &str) {
        if self.warnings.len() < MAX_WARNINGS && !self.warnings.iter().any(|w| w == message) {
            self.warnings.push(message.into());
        }
    }
}

/// One bounded background scan at a time; call `poll` from the host event loop.
#[derive(Default)]
pub struct SessionScanner {
    pending: Option<Receiver<SessionCatalog>>,
}
impl SessionScanner {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn is_scanning(&self) -> bool {
        self.pending.is_some()
    }
    pub fn refresh(&mut self) -> bool {
        self.start(SessionCatalog::discover)
    }
    fn start(&mut self, scan: fn() -> SessionCatalog) -> bool {
        if self.pending.is_some() {
            return false;
        }
        let (sender, receiver) = mpsc::sync_channel(1);
        match std::thread::Builder::new()
            .name("agent-session-index".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(scan).unwrap_or_else(|_| SessionCatalog {
                    sessions: vec![],
                    warnings: vec!["Session discovery failed; try refreshing.".into()],
                    ..SessionCatalog::default()
                });
                let _ = sender.send(result);
            }) {
            Ok(_) => {
                self.pending = Some(receiver);
                true
            }
            Err(_) => {
                let (tx, rx) = mpsc::sync_channel(1);
                let _ = tx.send(SessionCatalog {
                    sessions: vec![],
                    warnings: vec!["Could not start session discovery worker.".into()],
                    ..SessionCatalog::default()
                });
                self.pending = Some(rx);
                true
            }
        }
    }
    pub fn poll(&mut self) -> Option<SessionCatalog> {
        let result = self.pending.as_ref()?.try_recv();
        match result {
            Ok(catalog) => {
                self.pending = None;
                Some(catalog)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.pending = None;
                Some(SessionCatalog {
                    sessions: vec![],
                    warnings: vec!["Session discovery worker stopped; try refreshing.".into()],
                    ..SessionCatalog::default()
                })
            }
        }
    }
}
struct Candidate {
    path: PathBuf,
    provider: SessionProvider,
    archived: bool,
    modified: u64,
    stamp: (u128, u64),
}
fn collect(
    root: &Path,
    provider: SessionProvider,
    archived: bool,
    depth: usize,
    entries: &mut usize,
    files: &mut Vec<Candidate>,
    catalog: &mut SessionCatalog,
) {
    let mut queue = VecDeque::from([(root.to_path_buf(), 0)]);
    while let Some((dir, level)) = queue.pop_front() {
        let reader = match fs::read_dir(&dir) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => {
                catalog.warn("Some session directories could not be read.");
                continue;
            }
        };
        for entry in reader {
            if *entries >= MAX_ENTRIES {
                catalog.warn("History scan reached its 32,768-entry limit; narrow your provider history directory.");
                return;
            }
            *entries += 1;
            let Ok(entry) = entry else { continue };
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            // Never follow links out of a provider's history tree.
            if kind.is_symlink() {
                continue;
            }
            let path = entry.path();
            if kind.is_dir() && level < depth {
                queue.push_back((path, level + 1));
                continue;
            }
            if !kind.is_file() {
                continue;
            }
            let compressed = path
                .file_name()
                .and_then(|v| v.to_str())
                .is_some_and(|v| v.ends_with(".jsonl.zst"));
            if !compressed && path.extension().is_none_or(|v| v != "jsonl") {
                continue;
            }
            if provider == SessionProvider::Claude
                && path
                    .file_stem()
                    .and_then(|v| v.to_str())
                    .is_none_or(|v| !valid_id(v))
            {
                continue;
            }
            let metadata = entry.metadata().ok();
            let modified_time = metadata
                .as_ref()
                .and_then(|m| m.modified().ok())
                .unwrap_or(UNIX_EPOCH);
            let modified = epoch_ms(modified_time);
            let stamp = (
                modified_time
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos(),
                metadata.map(|m| m.len()).unwrap_or(0),
            );
            files.push(Candidate {
                path,
                provider,
                archived,
                modified,
                stamp,
            });
        }
    }
}
// The cache contains only bounded display metadata, never full transcripts.
#[derive(Clone)]
struct CachedSession {
    stamp: (u128, u64),
    session: Option<AgentSession>,
    sampled: bool,
}
static SESSION_CACHE: OnceLock<Mutex<HashMap<PathBuf, CachedSession>>> = OnceLock::new();
fn parse_cached(
    candidate: &Candidate,
    bytes_left: &mut usize,
) -> std::io::Result<(Option<AgentSession>, bool)> {
    let cache = SESSION_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(entries) = cache.lock() {
        if let Some(entry) = entries
            .get(&candidate.path)
            .filter(|e| e.stamp == candidate.stamp)
        {
            return Ok((entry.session.clone(), entry.sampled));
        }
    }
    let (session, sampled) = parse_file(candidate, bytes_left)?;
    if let Ok(mut entries) = cache.lock() {
        if entries.len() >= 4096 {
            entries.clear();
        }
        entries.insert(
            candidate.path.clone(),
            CachedSession {
                stamp: candidate.stamp,
                session: session.clone(),
                sampled,
            },
        );
    }
    Ok((session, sampled))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveSessionProcess {
    pub provider: SessionProvider,
    pub id: String,
    pub pid: u32,
}
fn process_cli(args: &[&str]) -> Option<(SessionProvider, usize)> {
    // Node launchers may precede the actual CLI script. Only recognize a
    // basename exactly equal to the provider executable, never prompt text.
    let first = Path::new(*args.first()?).file_name()?.to_str()?;
    let cli = if matches!(first, "node" | "nodejs") {
        1
    } else {
        0
    };
    args.get(cli)?;
    let name = Path::new(args[cli]).file_name()?.to_str()?;
    let provider = match name {
        "codex" | "codex.js" => SessionProvider::Codex,
        "claude" => SessionProvider::Claude,
        "cli.js" if args[cli].contains("@anthropic-ai/claude-code/") => SessionProvider::Claude,
        _ => return None,
    };
    Some((provider, cli))
}
fn process_identity(args: &[&str]) -> Option<(SessionProvider, String)> {
    let (provider, cli) = process_cli(args)?;
    let all_flags = &args[cli + 1..];
    let flags = &all_flags[..all_flags
        .iter()
        .position(|v| *v == "--")
        .unwrap_or(all_flags.len())];
    let id = match provider {
        SessionProvider::Codex => {
            if flags.first() != Some(&"resume") {
                return None;
            }
            *flags.get(1)?
        }
        SessionProvider::Claude => {
            let index = flags
                .iter()
                .position(|v| matches!(*v, "--resume" | "-r" | "--session-id"))?;
            *flags.get(index + 1)?
        }
    };
    valid_id(id).then(|| (provider, id.to_owned()))
}
#[cfg(target_os = "linux")]
fn discover_live_processes(sessions: &[AgentSession]) -> Vec<LiveSessionProcess> {
    discover_live_processes_from(Path::new("/proc"), sessions, unsafe { libc::geteuid() })
}
#[cfg(target_os = "linux")]
fn discover_live_processes_from(
    proc_root: &Path,
    sessions: &[AgentSession],
    uid: u32,
) -> Vec<LiveSessionProcess> {
    use std::os::unix::fs::MetadataExt;
    let sources = sessions
        .iter()
        .filter_map(|session| {
            fs::canonicalize(&session.source)
                .ok()
                .map(|path| ((session.provider, path), session.id.clone()))
        })
        .collect::<HashMap<_, _>>();
    let Ok(reader) = fs::read_dir(proc_root) else {
        return vec![];
    };
    let mut processes = Vec::new();
    let mut fd_budget = 4096usize;
    for entry in reader.take(32_768).flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|v| v.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.uid() != uid {
            continue;
        }
        let Ok(file) = File::open(entry.path().join("cmdline")) else {
            continue;
        };
        let mut raw = Vec::new();
        if file.take(16 * 1024 + 1).read_to_end(&mut raw).is_err() || raw.len() > 16 * 1024 {
            continue;
        }
        let Some(args) = raw
            .split(|b| *b == 0)
            .filter(|v| !v.is_empty())
            .map(|v| std::str::from_utf8(v).ok())
            .collect::<Option<Vec<_>>>()
        else {
            continue;
        };
        if let Some((provider, id)) = process_identity(&args) {
            processes.push(LiveSessionProcess { provider, id, pid });
        } else if let Some((provider, _)) = process_cli(&args) {
            if let Ok(descriptors) = fs::read_dir(entry.path().join("fd")) {
                for descriptor in descriptors.take(256).flatten() {
                    if fd_budget == 0 {
                        break;
                    }
                    fd_budget -= 1;
                    let Ok(target) = fs::read_link(descriptor.path()) else {
                        continue;
                    };
                    if let Some(id) = sources.get(&(provider, target)) {
                        if !processes
                            .iter()
                            .any(|p| p.provider == provider && p.id == *id && p.pid == pid)
                        {
                            processes.push(LiveSessionProcess {
                                provider,
                                id: id.clone(),
                                pid,
                            });
                        }
                    }
                }
            }
        }
        if processes.len() >= 2048 {
            break;
        }
    }
    processes.sort_by_key(|p| p.pid);
    processes
}
#[cfg(not(target_os = "linux"))]
fn discover_live_processes(_sessions: &[AgentSession]) -> Vec<LiveSessionProcess> {
    vec![]
}

fn apply_session_titles(catalog: &mut SessionCatalog, homes: &[PathBuf]) {
    for home in homes {
        let Ok(mut file) = File::open(home.join("session_index.jsonl")) else {
            continue;
        };
        let Ok(metadata) = file.metadata() else {
            continue;
        };
        let offset = metadata.len().saturating_sub(2 * 1024 * 1024);
        if file.seek(SeekFrom::Start(offset)).is_err() {
            continue;
        }
        let mut bytes = Vec::new();
        if file.take(2 * 1024 * 1024).read_to_end(&mut bytes).is_err() {
            continue;
        }
        if offset > 0 {
            catalog.warn(
                "Large Codex title index was sampled; some older custom names may be unavailable.",
            );
        }
        let first = if offset > 0 {
            bytes
                .iter()
                .position(|b| *b == b'\n')
                .map(|i| i + 1)
                .unwrap_or(bytes.len())
        } else {
            0
        };
        let mut titles = HashMap::new();
        for line in bytes[first..].split(|b| *b == b'\n') {
            if crate::bounded_json::validate_no_duplicate_members(line).is_err() {
                continue;
            }
            let Ok(record) = serde_json::from_slice::<Value>(line) else {
                continue;
            };
            if let (Some(id), Some(title)) = (
                record.get("id").and_then(Value::as_str),
                record.get("thread_name").and_then(Value::as_str),
            ) {
                let title = safe_text(title, 256);
                if valid_id(id) && !title.is_empty() {
                    titles.insert(id.to_owned(), title);
                }
            }
        }
        let canonical = fs::canonicalize(home).ok();
        for session in &mut catalog.sessions {
            if session.provider == SessionProvider::Codex && session.provider_home() == canonical {
                if let Some(title) = titles.get(&session.id) {
                    session.title = title.clone();
                }
            }
        }
    }
}
fn provider_home(provider: SessionProvider) -> Option<PathBuf> {
    let (variable, default) = match provider {
        SessionProvider::Codex => ("CODEX_HOME", ".codex"),
        SessionProvider::Claude => ("CLAUDE_CONFIG_DIR", ".claude"),
    };
    std::env::var_os(variable)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|p| p.join(default)))
}
fn epoch_ms(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}
fn valid_id(id: &str) -> bool {
    uuid::Uuid::parse_str(id).is_ok()
        && id.len() == 36
        && id.bytes().all(|c| c.is_ascii_hexdigit() || c == b'-')
}
fn safe_text(text: &str, limit: usize) -> String {
    let mut output = String::new();
    for ch in text.chars() {
        if output.len() + ch.len_utf8() > limit {
            break;
        }
        if ch.is_control() || crate::review_input::is_visual_spoofing_character(ch) {
            if !output.ends_with(' ') {
                output.push(' ');
            }
        } else {
            output.push(ch);
        }
    }
    output.split_whitespace().collect::<Vec<_>>().join(" ")
}
fn content_text(value: &Value) -> String {
    if let Some(s) = value.as_str() {
        return safe_text(s, 2048);
    }
    let mut output = String::new();
    if let Some(items) = value.as_array() {
        for item in items.iter().take(64) {
            let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
            if matches!(kind, "text" | "input_text" | "output_text") {
                if let Some(text) = item.get("text").and_then(Value::as_str) {
                    if !output.is_empty() {
                        output.push(' ');
                    }
                    output.push_str(&safe_text(text, 2048usize.saturating_sub(output.len())));
                }
            }
            if output.len() >= 2048 {
                break;
            }
        }
    }
    output
}
fn parse_file(
    candidate: &Candidate,
    bytes_left: &mut usize,
) -> std::io::Result<(Option<AgentSession>, bool)> {
    let mut file = File::open(&candidate.path)?;
    let len = file.metadata()?.len();
    let compressed = candidate.path.extension().is_some_and(|v| v == "zst");
    let (head, tail, sampled) = if compressed {
        // Bound both compressed input and expanded bytes, and the zstd window.
        let mut decoder = zstd::stream::read::Decoder::new(file.take(8 * 1024 * 1024))?;
        decoder.window_log_max(23)?;
        // Large compressed rollouts cannot seek to their tail. Index their
        // bounded header instead of spending the entire catalog budget on
        // a few old conversations; label their current state unknown.
        let limit = (*bytes_left).min(if len > 64 * 1024 {
            HEAD_BYTES
        } else {
            2 * 1024 * 1024
        });
        let mut head = Vec::new();
        let mut tail = VecDeque::new();
        let mut read = 0usize;
        let mut complete = false;
        let mut chunk = [0u8; 8192];
        let hard_limit = (*bytes_left).min(2 * 1024 * 1024);
        while read < hard_limit
            && (read < limit || (!head.contains(&b'\n') && read < MAX_HEAD_BYTES))
        {
            let allowance = chunk.len().min(hard_limit - read);
            match decoder.read(&mut chunk[..allowance]) {
                Ok(0) => {
                    complete = true;
                    break;
                }
                Ok(n) => {
                    if head.len() < HEAD_BYTES
                        || (!head.contains(&b'\n') && head.len() < MAX_HEAD_BYTES)
                    {
                        head.extend_from_slice(&chunk[..n.min(MAX_HEAD_BYTES - head.len())]);
                    }
                    for &byte in &chunk[..n] {
                        if tail.len() == TAIL_BYTES {
                            tail.pop_front();
                        }
                        tail.push_back(byte);
                    }
                    read += n;
                }
                Err(_) if !head.is_empty() => break,
                Err(e) => return Err(e),
            }
        }
        *bytes_left = bytes_left.saturating_sub(read);
        let mut tail = tail.into_iter().collect::<Vec<_>>();
        if read <= HEAD_BYTES {
            tail.clear();
        } else if read <= TAIL_BYTES {
            tail = tail[HEAD_BYTES..].to_vec();
        } else if let Some(n) = tail.iter().position(|b| *b == b'\n') {
            tail.drain(..=n);
        }
        (head, tail, !complete)
    } else {
        let head_len = len.min(HEAD_BYTES as u64) as usize;
        let mut head = vec![0; head_len];
        file.read_exact(&mut head)?;
        while !head.contains(&b'\n')
            && head.len() < MAX_HEAD_BYTES
            && head.len() < (len as usize)
            && head.len() < *bytes_left
        {
            let count = (64 * 1024)
                .min(MAX_HEAD_BYTES - head.len())
                .min(len as usize - head.len())
                .min(*bytes_left - head.len());
            if count == 0 {
                break;
            }
            let old = head.len();
            head.resize(old + count, 0);
            file.read_exact(&mut head[old..])?;
        }
        *bytes_left = bytes_left.saturating_sub(head.len());
        let actual_head_len = head.len() as u64;
        let mut tail = Vec::new();
        if len > actual_head_len {
            let start = (len.saturating_sub(TAIL_BYTES as u64)).max(actual_head_len);
            file.seek(SeekFrom::Start(start))?;
            let mut raw = vec![0; (len - start) as usize];
            file.read_exact(&mut raw)?;
            *bytes_left = bytes_left.saturating_sub(raw.len());
            if start == actual_head_len && head.last() == Some(&b'\n') {
                tail = raw;
            } else if let Some(newline) = raw.iter().position(|b| *b == b'\n') {
                tail.extend_from_slice(&raw[newline + 1..]);
            }
        }
        (head, tail, false)
    };
    let mut state = Parsed::default();
    for bytes in [&head[..], &tail[..]] {
        for line in bytes.split(|b| *b == b'\n') {
            if line.is_empty() {
                continue;
            }
            if crate::bounded_json::validate_no_duplicate_members(line).is_err() {
                continue;
            }
            let Ok(value) = serde_json::from_slice::<Value>(line) else {
                continue;
            };
            state.observe(&value, candidate.provider);
        }
    }
    let id = state.id.or_else(|| {
        candidate
            .path
            .file_stem()
            .and_then(|v| v.to_str())
            .filter(|v| valid_id(v))
            .map(str::to_owned)
    });
    let (Some(id), Some(cwd)) = (id, state.cwd) else {
        return Ok((None, sampled));
    };
    if !valid_id(&id)
        || !cwd.is_absolute()
        || cwd
            .to_str()
            .is_none_or(|s| crate::review_input::validate(s).is_err())
    {
        return Ok((None, sampled));
    }
    let title = state
        .title
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("{} session {}", candidate.provider.label(), &id[..8]));
    Ok((
        Some(AgentSession {
            provider: candidate.provider,
            id,
            title,
            cwd,
            updated_at_ms: candidate.modified,
            status: if candidate.archived {
                SessionStatus::Archived
            } else if sampled {
                SessionStatus::Unknown
            } else {
                state.status.unwrap_or(SessionStatus::Unknown)
            },
            preview: state.preview,
            branch: state.branch,
            source: candidate.path.clone(),
            archived: candidate.archived,
        }),
        sampled,
    ))
}
#[derive(Default)]
struct Parsed {
    id: Option<String>,
    cwd: Option<PathBuf>,
    title: Option<String>,
    preview: String,
    branch: Option<String>,
    status: Option<SessionStatus>,
}
impl Parsed {
    fn observe(&mut self, value: &Value, provider: SessionProvider) {
        let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
        if provider == SessionProvider::Codex {
            let payload = &value["payload"];
            match kind {
                "session_meta" => {
                    self.id = payload
                        .get("id")
                        .or_else(|| payload.get("session_id"))
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    self.cwd = payload
                        .get("cwd")
                        .and_then(Value::as_str)
                        .map(PathBuf::from);
                    self.branch = payload
                        .pointer("/git/branch")
                        .and_then(Value::as_str)
                        .map(|s| safe_text(s, 256));
                }
                "turn_context" => {
                    if let Some(cwd) = payload.get("cwd").and_then(Value::as_str) {
                        self.cwd = Some(cwd.into());
                    }
                }
                "event_msg" => match payload.get("type").and_then(Value::as_str).unwrap_or("") {
                    "user_message" => {
                        if let Some(text) = payload.get("message").and_then(Value::as_str) {
                            self.user_text(text);
                        }
                        self.status = Some(SessionStatus::Working);
                    }
                    "agent_message" => {
                        if let Some(text) = payload.get("message").and_then(Value::as_str) {
                            self.preview = safe_text(text, 2048);
                        }
                    }
                    "task_started" | "turn_started" => self.status = Some(SessionStatus::Working),
                    "task_complete" | "task_completed" | "turn_complete" | "turn_completed" => {
                        self.status = Some(SessionStatus::Idle)
                    }
                    "turn_aborted" | "error" => self.status = Some(SessionStatus::Unknown),
                    "approval_request" | "request_user_input" => {
                        self.status = Some(SessionStatus::Waiting)
                    }
                    _ => {}
                },
                "response_item"
                    if payload.get("type").and_then(Value::as_str) == Some("message") =>
                {
                    let text = content_text(&payload["content"]);
                    match payload.get("role").and_then(Value::as_str) {
                        Some("user") => self.user_text(&text),
                        Some("assistant") if !text.is_empty() => self.preview = text,
                        _ => {}
                    }
                }
                _ => {}
            }
        } else {
            if value.get("isSidechain").and_then(Value::as_bool) == Some(true) {
                return;
            }
            if let Some(id) = value.get("sessionId").and_then(Value::as_str) {
                if valid_id(id) {
                    self.id = Some(id.into());
                }
            }
            if let Some(cwd) = value.get("cwd").and_then(Value::as_str) {
                self.cwd = Some(cwd.into());
            }
            if let Some(branch) = value
                .get("gitBranch")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                self.branch = Some(safe_text(branch, 256));
            }
            match kind {
                "custom-title" => {
                    if let Some(title) = value.get("customTitle").and_then(Value::as_str) {
                        self.title = Some(safe_text(title, 256));
                    }
                }
                "summary" => {
                    if let Some(title) = value.get("summary").and_then(Value::as_str) {
                        if self.title.is_none() {
                            self.title = Some(safe_text(title, 256));
                        }
                    }
                }
                "user" => {
                    if value.get("isMeta").and_then(Value::as_bool) != Some(true) {
                        let text = content_text(&value["message"]["content"]);
                        self.user_text(&text);
                        self.status = Some(SessionStatus::Working);
                    }
                }
                "assistant" => {
                    let text = content_text(&value["message"]["content"]);
                    if !text.is_empty() {
                        self.preview = text;
                    }
                    self.status = Some(
                        match value
                            .pointer("/message/stop_reason")
                            .and_then(Value::as_str)
                        {
                            Some("end_turn" | "stop_sequence") => SessionStatus::Idle,
                            Some("tool_use") => SessionStatus::Working,
                            _ => SessionStatus::Unknown,
                        },
                    );
                }
                _ => {}
            }
        }
    }
    fn user_text(&mut self, text: &str) {
        let text = safe_text(text, 2048);
        if text.is_empty()
            || text.starts_with("<environment_context>")
            || text.starts_with("# AGENTS.md")
            || text.starts_with("<system-reminder>")
        {
            return;
        }
        if self.title.is_none() {
            self.title = Some(safe_text(&text, 256));
        }
        self.preview = text;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("agent-sessions-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn write(&self, relative: &str, records: &[Value]) -> PathBuf {
            let path = self.0.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(
                &path,
                records.iter().map(|v| format!("{v}\n")).collect::<String>(),
            )
            .unwrap();
            path
        }
        fn catalog(&self) -> SessionCatalog {
            SessionCatalog::discover_with_roots(&self.0.join("codex"), &self.0.join("claude"))
        }
        fn meta(&self, id: &str) -> Value {
            json!({"type":"session_meta","payload":{"id":id,"cwd":self.0,"git":{"branch":"feature/agents"}}})
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    const ID: &str = "11111111-1111-4111-8111-111111111111";
    #[test]
    fn codex_transcript_search_and_exact_resume_identity() {
        let f = Fixture::new();
        f.write("codex/sessions/2026/10/23/rollout.jsonl",&[
            f.meta(ID),json!({"type":"event_msg","payload":{"type":"user_message","message":"Fix the session picker"}}),
            json!({"type":"event_msg","payload":{"type":"agent_message","message":"Picker implemented"}}),
            json!({"type":"event_msg","payload":{"type":"task_complete"}})]);
        let catalog = f.catalog();
        assert_eq!(catalog.sessions.len(), 1);
        let session = &catalog.sessions[0];
        assert_eq!(session.title, "Fix the session picker");
        assert_eq!(session.preview, "Picker implemented");
        assert_eq!(session.status, SessionStatus::Idle);
        assert!(session.matches_query("CODEX picker feature/agents"));
        assert!(!session.matches_query("claude"));
        let launch = session.resume_launch().unwrap();
        assert_eq!(launch.program, "env");
        assert_eq!(&launch.args[1..], ["codex", "resume", ID]);
        assert_eq!(
            launch.args[0],
            format!("CODEX_HOME={}", f.0.join("codex").display())
        );
        assert_eq!(launch.cwd, f.0);
    }
    #[test]
    fn claude_custom_title_and_stop_reason() {
        let f = Fixture::new();
        f.write(&format!("claude/projects/project/{ID}.jsonl"),&[
            json!({"type":"user","sessionId":ID,"cwd":f.0,"gitBranch":"main","message":{"content":"Debug deployment"}}),
            json!({"type":"custom-title","sessionId":ID,"customTitle":"Deployment investigation"}),
            json!({"type":"assistant","sessionId":ID,"message":{"content":[{"type":"text","text":"Root cause found"}],"stop_reason":"end_turn"}})]);
        let catalog = f.catalog();
        let session = &catalog.sessions[0];
        assert_eq!(session.title, "Deployment investigation");
        assert_eq!(session.status, SessionStatus::Idle);
        assert_eq!(session.branch.as_deref(), Some("main"));
        assert_eq!(
            &session.resume_launch().unwrap().args[1..],
            ["claude", "--resume", ID]
        );
    }
    #[test]
    fn head_and_tail_preserve_metadata_and_latest_status() {
        let f = Fixture::new();
        let path = f.write("codex/sessions/large.jsonl", &[f.meta(ID)]);
        use std::io::Write;
        let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
        for _ in 0..5000 {
            writeln!(
                file,
                "{{\"type\":\"ignored\",\"padding\":\"{}\"}}",
                "x".repeat(100)
            )
            .unwrap();
        }
        writeln!(
            file,
            "{}",
            json!({"type":"event_msg","payload":{"type":"task_complete"}})
        )
        .unwrap();
        drop(file);
        assert_eq!(f.catalog().sessions[0].status, SessionStatus::Idle);
    }
    #[test]
    fn compressed_transcript_preserves_latest_state() {
        let f = Fixture::new();
        let records = format!(
            "{}\n{}\n{}\n",
            f.meta(ID),
            json!({"type":"event_msg","payload":{"type":"user_message","message":"Compressed history"}}),
            json!({"type":"event_msg","payload":{"type":"task_complete"}})
        );
        let path = f.0.join("codex/sessions/rollout.jsonl.zst");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            path,
            zstd::stream::encode_all(records.as_bytes(), 1).unwrap(),
        )
        .unwrap();
        let catalog = f.catalog();
        assert_eq!(catalog.sessions.len(), 1);
        assert_eq!(catalog.sessions[0].title, "Compressed history");
        assert_eq!(catalog.sessions[0].status, SessionStatus::Idle);
    }
    #[test]
    fn archived_is_visible_but_never_silently_resumed() {
        let f = Fixture::new();
        f.write("codex/archived_sessions/old.jsonl", &[f.meta(ID)]);
        let catalog = f.catalog();
        assert_eq!(catalog.sessions[0].status, SessionStatus::Archived);
        assert!(catalog.sessions[0]
            .resume_launch()
            .unwrap_err()
            .contains("Unarchive"));
    }
    #[test]
    fn active_copy_wins_over_archived_identity() {
        let f = Fixture::new();
        f.write("codex/sessions/current.jsonl", &[f.meta(ID)]);
        f.write("codex/archived_sessions/old.jsonl", &[f.meta(ID)]);
        let catalog = f.catalog();
        assert_eq!(catalog.sessions.len(), 1);
        assert!(!catalog.sessions[0].archived);
    }
    #[test]
    fn malformed_duplicate_keys_and_foreign_identity_are_ignored() {
        let f = Fixture::new();
        let path = f.write("codex/sessions/bad.jsonl", &[]);
        fs::write(path,format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{ID}\",\"id\":\"bad\",\"cwd\":\"/tmp\"}}}}\n")).unwrap();
        assert!(f.catalog().sessions.is_empty());
        f.write("codex/sessions/invalid.jsonl", &[f.meta("--last")]);
        assert!(f.catalog().sessions.is_empty());
    }
    #[test]
    fn missing_project_or_source_refuses_launch() {
        let f = Fixture::new();
        let path = f.write("codex/sessions/current.jsonl", &[f.meta(ID)]);
        let mut session = f.catalog().sessions.remove(0);
        session.cwd = f.0.join("missing");
        assert!(session.resume_launch().is_err());
        session.cwd = f.0.clone();
        fs::remove_file(path).unwrap();
        assert!(session.resume_launch().is_err());
    }
    #[cfg(unix)]
    #[test]
    fn symlinked_history_is_not_followed() {
        let f = Fixture::new();
        let source = f.write("elsewhere/current.jsonl", &[f.meta(ID)]);
        fs::create_dir_all(f.0.join("codex/sessions")).unwrap();
        std::os::unix::fs::symlink(source, f.0.join("codex/sessions/link.jsonl")).unwrap();
        assert!(f.catalog().sessions.is_empty());
    }
    #[test]
    fn display_fields_cannot_carry_terminal_controls() {
        let f = Fixture::new();
        f.write("codex/sessions/current.jsonl",&[f.meta(ID),json!({"type":"event_msg","payload":{"type":"user_message","message":"hello\u{1b}]52;c;secret\u{7}\u{202e}world"}})]);
        let catalog = f.catalog();
        assert!(!catalog.sessions[0].title.chars().any(char::is_control));
        assert!(!crate::review_input::contains_visual_spoofing(
            &catalog.sessions[0].title
        ));
    }
    #[test]
    fn stale_working_is_explicitly_transcript_evidence() {
        let f = Fixture::new();
        f.write(
            "codex/sessions/current.jsonl",
            &[
                f.meta(ID),
                json!({"type":"event_msg","payload":{"type":"task_started"}}),
            ],
        );
        let catalog = f.catalog();
        assert_eq!(catalog.sessions[0].status, SessionStatus::Working);
        assert!(catalog.sessions[0]
            .status
            .label()
            .starts_with("Last recorded:"));
    }
    #[test]
    fn claude_sidechains_and_nested_agent_logs_are_excluded() {
        let f = Fixture::new();
        let record = json!({"type":"user","sessionId":ID,"cwd":f.0,"isSidechain":true,"message":{"content":"Internal agent task"}});
        f.write(
            &format!("claude/projects/project/{ID}.jsonl"),
            std::slice::from_ref(&record),
        );
        f.write(
            &format!("claude/projects/project/{ID}/subagents/{ID}.jsonl"),
            &[record],
        );
        assert!(f.catalog().sessions.is_empty());
    }
    #[test]
    fn launch_display_quotes_every_argument() {
        let launch = SessionLaunch {
            program: "codex".into(),
            args: vec!["resume".into(), ID.into()],
            cwd: PathBuf::from("/tmp"),
        };
        assert_eq!(launch.display_command(), format!("'codex' 'resume' '{ID}'"));
    }
    #[test]
    fn scanner_starts_at_most_one_worker() {
        let mut scanner = SessionScanner::new();
        assert!(!scanner.is_scanning());
        assert!(scanner.poll().is_none());
        assert!(scanner.start(SessionCatalog::default));
        assert!(!scanner.start(SessionCatalog::default));
        assert!(scanner.is_scanning());
    }
    #[test]
    fn metadata_larger_than_the_regular_head_window_is_discovered() {
        let f = Fixture::new();
        let mut metadata = f.meta(ID);
        metadata["payload"]["base_instructions"] = json!("x".repeat(200_000));
        f.write(
            "codex/sessions/long-metadata.jsonl",
            &[
                metadata,
                json!({"type":"event_msg","payload":{"type":"task_complete"}}),
            ],
        );
        let catalog = f.catalog();
        assert_eq!(catalog.sessions.len(), 1);
        assert_eq!(catalog.sessions[0].status, SessionStatus::Idle);
    }
    #[test]
    fn cache_reuses_unchanged_metadata_and_invalidates_a_changed_file() {
        let f = Fixture::new();
        let path = f.write("codex/sessions/cached.jsonl", &[f.meta(ID)]);
        let candidate = Candidate {
            path: path.clone(),
            provider: SessionProvider::Codex,
            archived: false,
            modified: 0,
            stamp: (0, fs::metadata(&path).unwrap().len()),
        };
        let mut budget = MAX_SCAN_BYTES;
        let first = parse_cached(&candidate, &mut budget).unwrap();
        assert!(budget < MAX_SCAN_BYTES);
        let mut budget = MAX_SCAN_BYTES;
        let second = parse_cached(&candidate, &mut budget).unwrap();
        assert_eq!(budget, MAX_SCAN_BYTES);
        assert_eq!(first.0.unwrap().id, second.0.unwrap().id);
        f.write(
            "codex/sessions/cached.jsonl",
            &[
                f.meta(ID),
                json!({"type":"event_msg","payload":{"type":"task_complete"}}),
            ],
        );
        let updated = Candidate {
            stamp: (0, fs::metadata(&path).unwrap().len()),
            ..candidate
        };
        let session = parse_cached(&updated, &mut budget).unwrap().0.unwrap();
        assert_eq!(session.status, SessionStatus::Idle);
        assert!(budget < MAX_SCAN_BYTES);
    }
    #[test]
    fn live_identity_requires_exact_cli_argv_and_uuid() {
        assert_eq!(
            process_identity(&["/usr/bin/codex", "resume", ID]),
            Some((SessionProvider::Codex, ID.into()))
        );
        assert_eq!(
            process_identity(&["node", "/pkg/bin/codex.js", "resume", ID]),
            Some((SessionProvider::Codex, ID.into()))
        );
        assert_eq!(
            process_identity(&["claude", "--resume", ID]),
            Some((SessionProvider::Claude, ID.into()))
        );
        assert_eq!(
            process_identity(&["claude", "--session-id", ID]),
            Some((SessionProvider::Claude, ID.into()))
        );
        assert_eq!(
            process_identity(&["node", "/pkg/@anthropic-ai/claude-code/cli.js", "-r", ID]),
            Some((SessionProvider::Claude, ID.into()))
        );
        assert!(process_identity(&["bash", "-c", "codex", "resume", ID]).is_none());
        assert!(process_identity(&["claude", "--resume", "--last"]).is_none());
        assert!(process_identity(&["claude", "--", "--resume", ID]).is_none());
        assert!(process_identity(&["node", "/other/cli.js", "--resume", ID]).is_none());
    }
    #[test]
    fn compressed_budget_never_turns_a_prefix_into_live_state() {
        let f = Fixture::new();
        let mut records = format!("{}\n", f.meta(ID));
        let mut seed = 7u32;
        for _ in 0..5000 {
            let padding = (0..100)
                .map(|_| {
                    seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                    char::from(b'a' + (seed % 26) as u8)
                })
                .collect::<String>();
            records.push_str(&format!(
                "{}\n",
                json!({"type":"ignored","padding":padding})
            ));
        }
        records.push_str(&format!(
            "{}\n",
            json!({"type":"event_msg","payload":{"type":"task_complete"}})
        ));
        let path = f.0.join("codex/sessions/large.jsonl.zst");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let compressed = zstd::stream::encode_all(records.as_bytes(), 1).unwrap();
        assert!(compressed.len() > 64 * 1024);
        fs::write(path, compressed).unwrap();
        let catalog = f.catalog();
        assert_eq!(catalog.sessions.len(), 1);
        assert_eq!(catalog.sessions[0].status, SessionStatus::Unknown);
        assert!(!catalog.warnings.is_empty());
    }
    #[test]
    fn multiple_provider_homes_are_scanned_together() {
        let f = Fixture::new();
        f.write("codex/sessions/one.jsonl", &[f.meta(ID)]);
        let other = "22222222-2222-4222-8222-222222222222";
        f.write("alternate/sessions/two.jsonl", &[f.meta(other)]);
        let catalog = SessionCatalog::discover_roots(
            &[f.0.join("codex"), f.0.join("alternate")],
            &[f.0.join("claude")],
        );
        assert_eq!(catalog.sessions.len(), 2);
        let session = catalog.sessions.iter().find(|s| s.id == other).unwrap();
        let launch = session.resume_launch().unwrap();
        assert_eq!(launch.program, "env");
        assert_eq!(
            launch.args[0],
            format!("CODEX_HOME={}", f.0.join("alternate").display())
        );
    }
    #[test]
    fn codex_title_index_uses_latest_exact_identity_and_store() {
        let f = Fixture::new();
        f.write("codex/sessions/current.jsonl", &[f.meta(ID)]);
        f.write(
            "codex/session_index.jsonl",
            &[
                json!({"id":ID,"thread_name":"Old name"}),
                json!({"id":"--last","thread_name":"Invalid identity"}),
                json!({"id":ID,"thread_name":"Renamed conversation"}),
            ],
        );
        assert_eq!(f.catalog().sessions[0].title, "Renamed conversation");
        f.write(
            "other/session_index.jsonl",
            &[json!({"id":ID,"thread_name":"Wrong provider store"})],
        );
        let catalog = SessionCatalog::discover_roots(
            &[f.0.join("codex"), f.0.join("other")],
            &[f.0.join("claude")],
        );
        assert_eq!(catalog.sessions[0].title, "Renamed conversation");
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn open_descriptor_correlates_new_cli_without_uuid_and_respects_provider() {
        let f = Fixture::new();
        let source = f.write("codex/sessions/current.jsonl", &[f.meta(ID)]);
        let session = f.catalog().sessions.remove(0);
        let proc_root = f.0.join("proc");
        let process = proc_root.join("100");
        fs::create_dir_all(process.join("fd")).unwrap();
        fs::write(process.join("cmdline"), b"codex\0--model\0example\0").unwrap();
        std::os::unix::fs::symlink(fs::canonicalize(source).unwrap(), process.join("fd/3"))
            .unwrap();
        let uid = unsafe { libc::geteuid() };
        let live = discover_live_processes_from(&proc_root, std::slice::from_ref(&session), uid);
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].id, ID);
        assert_eq!(live[0].pid, 100);
        fs::write(process.join("cmdline"), b"claude\0").unwrap();
        assert!(discover_live_processes_from(&proc_root, &[session], uid).is_empty());
    }
}
