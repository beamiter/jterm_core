//! Crash-safe replacement of small persistence files.
//!
//! The temporary file is created next to the destination so the final rename
//! stays on the same filesystem and remains atomic.

use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

struct TempFileGuard {
    path: PathBuf,
    committed: bool,
}

impl TempFileGuard {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            committed: false,
        }
    }

    fn commit(&mut self) {
        self.committed = true;
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn destination_parent(path: &Path) -> io::Result<&Path> {
    if path.file_name().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "atomic-write destination has no file name",
        ));
    }

    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => Ok(parent),
        _ => Ok(Path::new(".")),
    }
}

/// Name of the sibling temporary file used while replacing `destination_name`.
///
/// Exposed to the crate because [`crate::snapshot_file`] has to assert that the
/// name cannot be read back as a session snapshot by the frontends' directory
/// scans, and a test asserting against a *copy* of this formula would keep
/// passing after the formula changed.
pub(crate) fn temp_file_name(destination_name: &OsStr, id: u64) -> OsString {
    let mut temp_name = OsString::from(".");
    temp_name.push(destination_name);
    temp_name.push(format!(".tmp.{}.{id}", std::process::id()));
    temp_name
}

fn create_unique_temp(path: &Path, parent: &Path) -> io::Result<(File, PathBuf)> {
    let destination_name = path
        .file_name()
        .expect("destination_parent validates the file name");

    // A stale file from a PID reused after a crash can collide with the first
    // counter value. create_new plus retry handles that without clobbering it.
    for _ in 0..128 {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let temp_path = parent.join(temp_file_name(destination_name, id));

        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }

        match options.open(&temp_path) {
            Ok(file) => return Ok((file, temp_path)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique atomic-write temporary file",
    ))
}

#[cfg(unix)]
fn sync_parent(parent: &Path) -> io::Result<()> {
    File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent(_parent: &Path) -> io::Result<()> {
    // Opening directories is not portable on every non-Unix target. The file
    // data is still synced before replacement there.
    Ok(())
}

/// Write `contents` durably and atomically replace `path`.
///
/// Temporary files are mode `0600` on Unix and are removed after every error
/// that occurs before the rename.
pub fn write_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    let parent = destination_parent(path)?;
    std::fs::create_dir_all(parent)?;

    let (mut file, temp_path) = create_unique_temp(path, parent)?;
    let mut cleanup = TempFileGuard::new(temp_path.clone());

    file.write_all(contents)?;
    file.sync_all()?;
    drop(file);

    std::fs::rename(&temp_path, path)?;
    cleanup.commit();

    // Persist the replacement directory entry, not just the new file data.
    sync_parent(parent)
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
                "jterm-core-atomic-file-{label}-{}-{id}",
                std::process::id()
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn atomically_replaces_existing_contents() {
        let root = TestDir::new("replace");
        let destination = root.0.join("state.json");
        std::fs::write(&destination, b"old").unwrap();

        write_atomic(&destination, b"new state").unwrap();

        assert_eq!(std::fs::read(&destination).unwrap(), b"new state");
        assert_eq!(std::fs::read_dir(&root.0).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn creates_private_files_on_unix() {
        use std::os::unix::fs::PermissionsExt;

        let root = TestDir::new("mode");
        let destination = root.0.join("private.json");
        write_atomic(&destination, b"secret").unwrap();

        let mode = std::fs::metadata(destination).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn removes_temporary_file_when_rename_fails() {
        let root = TestDir::new("cleanup");
        let destination = root.0.join("occupied");
        std::fs::create_dir(&destination).unwrap();

        assert!(write_atomic(&destination, b"cannot replace a directory").is_err());

        let entries: Vec<_> = std::fs::read_dir(&root.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![OsString::from("occupied")]);
    }
}
