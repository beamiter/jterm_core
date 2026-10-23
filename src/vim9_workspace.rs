//! Bounded, data-only per-project workbench presentation and command history.
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::{Vim9SearchOptions, Vim9UiConfig};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Vim9WorkbenchFocus {
    #[default]
    Editor,
    Search,
    Build,
    Files,
    Output,
}

/// No argv, shell commands, editor text or executable session fragments.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct Vim9WorkspaceState {
    pub search_query: String,
    pub search_options: Vim9SearchOptions,
    pub files_visible: bool,
    pub tools_visible: bool,
    pub output_visible: bool,
    pub output_tab: bool,
    pub project_width: Option<u16>,
    pub output_height: Option<u16>,
    pub focus: Vim9WorkbenchFocus,
}

impl Vim9WorkspaceState {
    pub fn for_config(config: &Vim9UiConfig) -> Self {
        Self {
            files_visible: config.project_panel,
            tools_visible: config.tool_panel,
            output_visible: config.tool_panel,
            ..Default::default()
        }
    }

    fn bounded(mut self) -> Self {
        self.search_query = self
            .search_query
            .chars()
            .filter(|c| !c.is_control())
            .take(4096)
            .collect();
        self.search_options.glob = self
            .search_options
            .glob
            .chars()
            .filter(|c| !c.is_control())
            .take(1024)
            .collect();
        self.project_width = self.project_width.map(|n| n.clamp(120, 1200));
        self.output_height = self.output_height.map(|n| n.clamp(80, 1200));
        self
    }
}

#[derive(Deserialize, Serialize)]
struct Document {
    version: u8,
    project: String,
    host: String,
    state: Vim9WorkspaceState,
}

fn state_root() -> Option<PathBuf> {
    if cfg!(test) || std::env::var_os("JTERM_VIM9_NO_WORKSPACE_STATE").is_some_and(|v| v == "1") {
        return None;
    }
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute() && p.parent().is_some())
        .or_else(|| dirs::home_dir().map(|p| p.join(".local/state")))
        .map(|p| p.join("jterm/vim9"))
}

fn project_key(root: &Path) -> String {
    fs::canonicalize(root)
        .unwrap_or_else(|_| root.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

fn fingerprint(value: &str) -> u64 {
    value.bytes().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    })
}

fn atomic_json(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let directory = path
        .parent()
        .ok_or_else(|| io::Error::other("Missing state directory"))?;
    fs::create_dir_all(directory)?;
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

/// Save only changed presentation, at most twice per second; Drop flushes the
/// latest state so a short session still restores what the user last selected.
pub struct Vim9WorkspaceStateStore {
    path: Option<PathBuf>,
    project: String,
    host: String,
    saved: Vim9WorkspaceState,
    pending: Option<Vim9WorkspaceState>,
    last_write: Instant,
}

impl Vim9WorkspaceStateStore {
    pub fn new(root: &Path, host: &str) -> Self {
        Self::in_directory(root, host, state_root())
    }

    fn in_directory(root: &Path, host: &str, directory: Option<PathBuf>) -> Self {
        let project = project_key(root);
        let host: String = host
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
            .take(32)
            .collect();
        let path = directory.map(|p| {
            p.join(format!(
                "workspace-{:016x}-{host}.json",
                fingerprint(&project)
            ))
        });
        Self {
            path,
            project,
            host,
            saved: Vim9WorkspaceState::default(),
            pending: None,
            last_write: Instant::now() - Duration::from_secs(1),
        }
    }

    pub fn restore(&mut self, fallback: Vim9WorkspaceState) -> Vim9WorkspaceState {
        let state = self
            .path
            .as_ref()
            .and_then(|path| {
                if fs::symlink_metadata(path).ok()?.len() > 64 * 1024 {
                    return None;
                }
                let data = fs::read(path).ok()?;
                let document: Document = serde_json::from_slice(&data).ok()?;
                (document.version == 1
                    && document.project == self.project
                    && document.host == self.host)
                    .then(|| document.state.bounded())
            })
            .unwrap_or(fallback)
            .bounded();
        self.saved = state.clone();
        state
    }

    pub fn remember(&mut self, state: Vim9WorkspaceState) {
        let state = state.bounded();
        if state == self.saved {
            self.pending = None;
            return;
        }
        self.pending = Some(state);
        if self.last_write.elapsed() >= Duration::from_millis(500) {
            let _ = self.flush();
        }
    }

    pub fn flush(&mut self) -> io::Result<()> {
        let Some(state) = self.pending.as_ref() else {
            return Ok(());
        };
        if let Some(path) = &self.path {
            let bytes = serde_json::to_vec(&Document {
                version: 1,
                project: self.project.clone(),
                host: self.host.clone(),
                state: state.clone(),
            })?;
            atomic_json(path, &bytes)?;
        }
        self.saved = self.pending.take().unwrap();
        self.last_write = Instant::now();
        Ok(())
    }
}

impl Drop for Vim9WorkspaceStateStore {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

static RECENT: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
static RECENT_WRITE: Mutex<()> = Mutex::new(());

pub(super) fn recent_commands() -> Vec<String> {
    RECENT
        .get_or_init(|| {
            let items: Vec<String> = state_root()
                .map(|p| p.join("recent-commands.json"))
                .filter(|p| fs::metadata(p).is_ok_and(|m| m.len() <= 8192))
                .and_then(|p| fs::read(p).ok())
                .and_then(|data| serde_json::from_slice(&data).ok())
                .unwrap_or_default();
            Mutex::new(
                items
                    .into_iter()
                    .filter(|id| {
                        id.len() <= 96 && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
                    })
                    .take(32)
                    .collect(),
            )
        })
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

pub(super) fn remember_command(id: &str) {
    let _ = recent_commands();
    let mut recent = RECENT
        .get()
        .unwrap()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    recent.retain(|value| value != id);
    recent.insert(0, id.into());
    recent.truncate(32);
    drop(recent);
    // Palette reads never wait for fsync; serialize only background writers,
    // then capture the newest in-memory history under its short-lived lock.
    let _writer = RECENT_WRITE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(path) = state_root().map(|p| p.join("recent-commands.json")) {
        if let Ok(bytes) = serde_json::to_vec(&recent_commands()) {
            let _ = atomic_json(&path, &bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_state_is_project_and_host_bound_and_data_only() {
        let directory =
            std::env::temp_dir().join(format!("vim-workspace-{}", uuid::Uuid::new_v4()));
        let mut store = Vim9WorkspaceStateStore::in_directory(
            Path::new("/project-one"),
            "anvil",
            Some(directory.clone()),
        );
        let mut state = store.restore(Vim9WorkspaceState::default());
        state.search_query = "needle\nsecret".into();
        state.files_visible = true;
        state.focus = Vim9WorkbenchFocus::Search;
        state.project_width = Some(1);
        store.remember(state);
        store.flush().unwrap();
        let mut reopened = Vim9WorkspaceStateStore::in_directory(
            Path::new("/project-one"),
            "anvil",
            Some(directory.clone()),
        );
        let restored = reopened.restore(Vim9WorkspaceState::default());
        assert_eq!(restored.search_query, "needlesecret");
        assert_eq!(restored.project_width, Some(120));
        assert!(restored.files_visible);
        assert_eq!(restored.focus, Vim9WorkbenchFocus::Search);
        let mut other = Vim9WorkspaceStateStore::in_directory(
            Path::new("/project-two"),
            "anvil",
            Some(directory.clone()),
        );
        assert_eq!(
            other.restore(Vim9WorkspaceState::default()),
            Vim9WorkspaceState::default()
        );
        let path = reopened.path.clone().unwrap();
        fs::write(&path, "{broken").unwrap();
        assert_eq!(
            reopened.restore(Vim9WorkspaceState::default()),
            Vim9WorkspaceState::default()
        );
        fs::remove_dir_all(directory).unwrap();
    }
}
