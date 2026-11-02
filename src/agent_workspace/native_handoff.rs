//! Access-token-only CLI handoff grant. Credential bytes live in a sealed
//! memfd; the private profile contains only a process-local descriptor link.
use crate::agent_task::native::NativeCodexCredentials;
use crate::agent_task::pinned_dir::PinnedDirectory;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

pub(super) struct CodexCliGrant {
    profile: PinnedDirectory,
    token: File,
    link_target: PathBuf,
    link_identity: (u64, u64),
    marker_identity: (u64, u64),
}
impl std::fmt::Debug for CodexCliGrant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexCliGrant").finish_non_exhaustive()
    }
}
#[derive(Serialize)]
struct AuthDocument<'a> {
    auth_mode: &'static str,
    #[serde(rename = "OPENAI_API_KEY")]
    openai_api_key: Option<&'a str>,
    tokens: Tokens<'a>,
}
#[derive(Serialize)]
struct Tokens<'a> {
    id_token: &'a str,
    access_token: &'a str,
    refresh_token: &'static str,
    account_id: &'a str,
}
#[derive(Serialize, Deserialize)]
struct GrantMarker {
    version: u32,
    dev: u64,
    ino: u64,
    target: PathBuf,
}
const GRANT_MARKER: &str = "jterm-cli-auth-grant.json";
/// The caller holds the native profile lease. Remove only the exact link
/// created by our previous grant, retaining any replacement/user auth entry.
pub(super) fn cleanup_stale_grant(profile: &Path) -> Result<(), String> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let marker_path = profile.join(GRANT_MARKER);
    let mut file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(&marker_path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.to_string()),
    };
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
        || metadata.nlink() != 1
        || metadata.len() > 4096
    {
        return Err("CLI access grant marker is unsafe; retain the profile for repair".into());
    }
    let mut encoded = Vec::new();
    file.read_to_end(&mut encoded).map_err(|e| e.to_string())?;
    let marker: GrantMarker = serde_json::from_slice(&encoded).map_err(|e| e.to_string())?;
    if marker.version != 1
        || marker.target.parent() != Some(Path::new("/proc/self/fd"))
        || marker
            .target
            .file_name()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<i32>().ok())
            .is_none()
    {
        return Err("Unknown CLI access grant marker; no credential entry was changed".into());
    }
    let auth = profile.join("auth.json");
    match std::fs::symlink_metadata(&auth) {
        Ok(m)
            if m.file_type().is_symlink()
                && (m.dev(), m.ino()) == (marker.dev, marker.ino)
                && std::fs::read_link(&auth).is_ok_and(|target| target == marker.target) =>
        {
            std::fs::remove_file(auth).map_err(|e| e.to_string())?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        _ => {
            return Err(
                "CLI access grant entry was replaced; existing credentials were retained".into(),
            )
        }
    }
    if std::fs::symlink_metadata(&marker_path)
        .is_ok_and(|m| (m.dev(), m.ino()) == (metadata.dev(), metadata.ino()))
    {
        std::fs::remove_file(marker_path).map_err(|e| e.to_string())?;
    }
    Ok(())
}
impl CodexCliGrant {
    pub fn new(profile: &Path, credentials: &NativeCodexCredentials) -> Result<Self, String> {
        let profile = PinnedDirectory::open(profile)?;
        let metadata = std::fs::metadata(profile.proc_path()).map_err(|e| e.to_string())?;
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err("CLI handoff profile must be owned and private".into());
        }
        let raw = unsafe {
            libc::memfd_create(
                c"jterm-cli-access-grant".as_ptr(),
                libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
            )
        };
        if raw < 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let mut token = unsafe { File::from_raw_fd(raw) };
        token
            .set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|e| e.to_string())?;
        let document = AuthDocument {
            auth_mode: "chatgptAuthTokens",
            openai_api_key: None,
            tokens: Tokens {
                // Codex's external-token mode parses identity claims from the
                // existing access JWT and never stores or refreshes this grant.
                id_token: credentials.access_token(),
                access_token: credentials.access_token(),
                refresh_token: "",
                account_id: credentials.account_id(),
            },
        };
        let mut encoded = serde_json::to_vec(&document).map_err(|e| e.to_string())?;
        let written = token.write_all(&encoded);
        encoded.fill(0);
        written.map_err(|e| e.to_string())?;
        token.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
        if unsafe {
            libc::fcntl(
                raw,
                libc::F_ADD_SEALS,
                libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE,
            )
        } < 0
        {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let link_target = PathBuf::from(format!("/proc/self/fd/{raw}"));
        // symlinkat fails rather than replacing any user-created auth file.
        let target = std::ffi::CString::new(link_target.as_os_str().as_encoded_bytes())
            .map_err(|e| e.to_string())?;
        if unsafe { libc::symlinkat(target.as_ptr(), profile.as_raw_fd(), c"auth.json".as_ptr()) }
            != 0
        {
            return Err("CLI handoff requires an unused private auth.json entry; existing credentials were retained".into());
        }
        let identity = std::fs::symlink_metadata(profile.proc_path().join("auth.json"))
            .map_err(|e| e.to_string())?;
        use std::os::unix::fs::OpenOptionsExt;
        let marker_result = (|| {
            let mut marker = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(profile.proc_path().join(GRANT_MARKER))
                .map_err(|e| e.to_string())?;
            serde_json::to_writer(
                &mut marker,
                &GrantMarker {
                    version: 1,
                    dev: identity.dev(),
                    ino: identity.ino(),
                    target: link_target.clone(),
                },
            )
            .map_err(|e| e.to_string())?;
            marker.sync_all().map_err(|e| e.to_string())?;
            let m = marker.metadata().map_err(|e| e.to_string())?;
            Ok::<_, String>((m.dev(), m.ino()))
        })();
        let marker_identity = match marker_result {
            Ok(identity) => identity,
            Err(error) => {
                let _ = std::fs::remove_file(profile.proc_path().join("auth.json"));
                return Err(error);
            }
        };
        Ok(Self {
            profile,
            token,
            link_target,
            link_identity: (identity.dev(), identity.ino()),
            marker_identity,
        })
    }
    pub fn configure_child(&self, command: &mut std::process::Command) {
        command.env("CODEX_HOME", self.profile.proc_path());
        let fds = [self.profile.as_raw_fd(), self.token.as_raw_fd()];
        unsafe {
            command.pre_exec(move || {
                for fd in fds {
                    let flags = libc::fcntl(fd, libc::F_GETFD);
                    if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
    }
}
impl Drop for CodexCliGrant {
    fn drop(&mut self) {
        let path = self.profile.proc_path().join("auth.json");
        if std::fs::symlink_metadata(&path)
            .is_ok_and(|m| m.file_type().is_symlink() && (m.dev(), m.ino()) == self.link_identity)
            && std::fs::read_link(&path).is_ok_and(|target| target == self.link_target)
        {
            let _ = std::fs::remove_file(path);
            let marker = self.profile.proc_path().join(GRANT_MARKER);
            if std::fs::symlink_metadata(&marker)
                .is_ok_and(|m| (m.dev(), m.ino()) == self.marker_identity)
            {
                let _ = std::fs::remove_file(marker);
            }
        }
    }
}

pub(super) struct NativeCliOptions {
    pub argv: Vec<String>,
    pub context: super::native::NativeLaunchContext,
    pub execution_lease: std::sync::Arc<super::ownership::NativeSessionLease>,
    // Declaration order drops the grant/link before releasing the profile.
    pub grant: Option<CodexCliGrant>,
    pub profile: Option<crate::agent_task::native::PreparedNativeCodexHome>,
    pub session_lease: Option<super::ownership::NativeSessionLease>,
}
pub(super) struct PreparedHandoff {
    pub session: crate::agent_sessions::AgentSession,
    pub launch: super::model::ManagedLaunch,
    pub options: NativeCliOptions,
}
pub(super) fn prepare(
    snapshot: super::native::NativeSnapshot,
    context: super::native::NativeLaunchContext,
    lease: std::sync::Arc<super::ownership::NativeSessionLease>,
    paths: super::ownership::Paths,
) -> Result<PreparedHandoff, String> {
    use crate::agent_sessions::{SessionCatalog, SessionProvider};
    use crate::agent_task::native::PreparedNativeCodexHome;
    use crate::agent_task::{AgentLaunchSpec, AgentProvider};
    let id = snapshot
        .view
        .as_ref()
        .and_then(|v| v.provider_thread_id.clone())
        .ok_or("No exact native provider conversation was recorded; history is still readable")?;
    let missing = paths.state.join("no-other-provider-history");
    let provider = match snapshot.task.provider {
        AgentProvider::Codex => SessionProvider::Codex,
        AgentProvider::Claude => SessionProvider::Claude,
        _ => return Err("This provider has no exact CLI conversation handoff".into()),
    };
    let mut profile = None;
    let mut grant = None;
    let mut session_lease = None;
    if provider == SessionProvider::Codex {
        let home = snapshot
            .native_home
            .as_ref()
            .ok_or("No app-owned native profile was recorded")?;
        let mut reopened = PreparedNativeCodexHome::reopen_persistent_with_source(
            &snapshot.task,
            home,
            &context.codex_home,
        )
        .map_err(|e| e.to_string())?;
        if reopened.resume_thread().map(|id| id.opaque()) != Some(id.as_str()) {
            return Err(
                "Recorded native thread no longer matches the profile; no CLI was started".into(),
            );
        }
        let credentials = reopened.take_credentials().map_err(|e| e.to_string())?;
        grant = Some(CodexCliGrant::new(home, &credentials)?);
        profile = Some(reopened);
    } else {
        session_lease = Some(super::ownership::NativeSessionLease::acquire_at(
            &paths.state,
            provider,
            &snapshot.provider_home,
            &id,
        )?);
    }
    let catalog = if provider == SessionProvider::Codex {
        SessionCatalog::discover_with_roots(&snapshot.provider_home, &missing)
    } else {
        SessionCatalog::discover_with_roots(&missing, &snapshot.provider_home)
    };
    let session=catalog.sessions.iter().find(|s|s.provider==provider&&s.id==id&&s.provider_home().as_ref()==Some(&snapshot.provider_home)).cloned().ok_or("The exact provider history is unavailable. Keep the readable native view and restore its original history before continuing")?;
    if let Some(conflict) = catalog.resume_conflict(&session) {
        return Err(conflict);
    }
    if !session.cwd.starts_with(&snapshot.task.worktree_path) {
        return Err("Provider history no longer points to this task worktree".into());
    }
    session.resume_launch()?;
    let argv = AgentLaunchSpec::resolve_native_with_path(
        snapshot.task.provider,
        &snapshot.task.repo_root,
        &snapshot.task.worktree_path,
        context.search_path.as_deref().map(std::ffi::OsStr::new),
    )
    .map_err(|e| e.to_string())?;
    let row = super::model::WorkspaceRow::from_agent(&session)?;
    let launch = super::model::ManagedLaunch {
        row,
        provider: if provider == SessionProvider::Codex {
            "codex"
        } else {
            "claude"
        }
        .into(),
        native_id: session.id.clone(),
        cwd: session.cwd.clone(),
        source: session.source.clone(),
        provider_home: snapshot.provider_home,
        cols: 100,
        rows: 30,
    };
    Ok(PreparedHandoff {
        session,
        launch,
        options: NativeCliOptions {
            argv,
            context,
            execution_lease: lease,
            grant,
            profile,
            session_lease,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn profile() -> PathBuf {
        use std::os::unix::fs::DirBuilderExt;
        let path = std::env::temp_dir().join(format!("native-grant-{}", uuid::Uuid::new_v4()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        path
    }
    #[test]
    fn access_grant_is_sealed_and_removed_without_storing_tokens() {
        let path = profile();
        let secret = "synthetic-memory-only-access-grant";
        let credentials =
            NativeCodexCredentials::new(secret.into(), "fixture-account".into()).unwrap();
        let grant = CodexCliGrant::new(&path, &credentials).unwrap();
        let auth: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path.join("auth.json")).unwrap()).unwrap();
        assert_eq!(auth["auth_mode"], "chatgptAuthTokens");
        assert_eq!(auth["tokens"]["refresh_token"], "");
        for file in std::fs::read_dir(&path).unwrap() {
            let file = file.unwrap();
            if file.file_type().unwrap().is_file() {
                assert!(!std::fs::read_to_string(file.path())
                    .unwrap()
                    .contains(secret));
            }
        }
        assert!(grant
            .token
            .try_clone()
            .unwrap()
            .write_all(b"replacement")
            .is_err());
        drop(grant);
        assert!(!path.join("auth.json").exists());
        assert!(!path.join(GRANT_MARKER).exists());
        std::fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn stale_grant_cleanup_retains_replacement_credentials() {
        let path = profile();
        let credentials =
            NativeCodexCredentials::new("synthetic-access".into(), "fixture-account".into())
                .unwrap();
        let grant = CodexCliGrant::new(&path, &credentials).unwrap();
        std::fs::remove_file(path.join("auth.json")).unwrap();
        std::fs::write(path.join("auth.json"), "replacement-user-auth").unwrap();
        assert!(cleanup_stale_grant(&path).is_err());
        drop(grant);
        assert_eq!(
            std::fs::read_to_string(path.join("auth.json")).unwrap(),
            "replacement-user-auth"
        );
        std::fs::remove_dir_all(path).unwrap();
    }
}
