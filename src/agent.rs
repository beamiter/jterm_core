//! Review-first agent core, shared with jsh through the `jagent` crate.
//!
//! The pure session state machine, action parser, safety heuristics, and
//! snapshot serialization live in `jagent` (sans-IO). This module re-exports
//! that surface under the historical `jterm_core::agent` paths and adds the
//! filesystem persistence helpers the jterm apps use for
//! `<config-dir>/<app>/agent_session.json`.

pub use jagent::safety::{is_auto_approvable, is_dangerous};
pub use jagent::session::{
    parse_action, sample_observation, AgentSession, AgentSessionSnapshot, AgentSnapshotError,
    AgentState, ApprovedCommand, CancellationToken, ModelOutcome, ParseError, ParsedAction,
    ProposalId, ProposalStatus, SessionError, Turn, MAX_AGENT_SNAPSHOT_JSON_BYTES,
};

/// Persist a snapshot to `path` with private (0600) permissions, creating
/// parent directories and replacing atomically via a sibling temp file.
pub fn write_snapshot_file(
    path: &std::path::Path,
    snapshot: &AgentSessionSnapshot,
) -> Result<(), AgentSnapshotError> {
    use std::io::Write;

    let encoded = snapshot.to_json()?;
    let parent = path
        .parent()
        .ok_or(AgentSnapshotError::Invalid("path has no parent directory"))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| AgentSnapshotError::Encode(format!("create dir: {error}")))?;
    let staged = parent.join(format!(
        ".{}.next.{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("agent_session.json"),
        std::process::id()
    ));
    let mut options = std::fs::OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = options
        .open(&staged)
        .and_then(|mut file| {
            file.write_all(encoded.as_bytes())?;
            file.sync_all()
        })
        .and_then(|_| std::fs::rename(&staged, path));
    if let Err(error) = result {
        let _ = std::fs::remove_file(&staged);
        return Err(AgentSnapshotError::Encode(format!(
            "write {}: {error}",
            path.display()
        )));
    }
    Ok(())
}

/// Best-effort bounded read of a snapshot file. Any failure (missing file,
/// oversize, parse error) yields None — a broken snapshot must never block
/// opening a fresh session.
pub fn read_snapshot_file(path: &std::path::Path) -> Option<AgentSessionSnapshot> {
    use std::io::Read;

    let file = std::fs::File::open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > MAX_AGENT_SNAPSHOT_JSON_BYTES as u64 {
        return None;
    }
    let mut encoded = String::new();
    file.take(MAX_AGENT_SNAPSHOT_JSON_BYTES as u64 + 1)
        .read_to_string(&mut encoded)
        .ok()?;
    AgentSessionSnapshot::from_json(&encoded).ok()
}

/// Remove a persisted snapshot; missing files are fine.
pub fn remove_snapshot_file(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDir(std::path::PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "jterm-core-agent-{label}-{}-{id}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn snapshot_files_round_trip_and_survive_bad_input() {
        let dir = TestDir::new("roundtrip");
        let path = dir.0.join("nested/agent_session.json");

        let mut session = AgentSession::new(10);
        session.submit_user("list files").unwrap();
        let snapshot = session.snapshot().unwrap();
        write_snapshot_file(&path, &snapshot).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        let restored = read_snapshot_file(&path).expect("snapshot reads back");
        let restored = AgentSession::restore(restored).unwrap();
        assert_eq!(restored.transcript(), session.transcript());

        // Corrupt files read as None instead of failing the caller.
        std::fs::write(&path, "not json").unwrap();
        assert!(read_snapshot_file(&path).is_none());

        remove_snapshot_file(&path);
        assert!(read_snapshot_file(&path).is_none());
        // Removing a missing file is fine.
        remove_snapshot_file(&path);
    }
}
