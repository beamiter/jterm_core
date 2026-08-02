//! Putting an embedded shell script on disk so it can be executed.
//!
//! Two scripts live in the jsh repository and are vendored here so a terminal
//! can run them without jsh being installed first: `install-jsh.sh`, which
//! bootstraps the shell itself, and `jsh-remote.sh`, which runs it on a machine
//! that does not have it. Neither can be executed from `include_str!`, so both
//! have to become a file, and both have to become the *same* file every time
//! rather than a fresh temporary per launch.
//!
//! What that requires is easy to get subtly wrong, which is why it is written
//! once here instead of once per script:
//!
//!   * The directory is private, and the file is opened with `O_NOFOLLOW`, so
//!     nothing can redirect a write through a symlink someone else planted.
//!   * The file must be a regular file, owned by this user, with exactly one
//!     link — a hard link to it from elsewhere would let another process see
//!     every future rewrite.
//!   * A cached copy is reused only when its bytes are exactly the embedded
//!     ones, so upgrading the terminal republishes the script instead of
//!     running last version's copy forever.
//!   * Publication is atomic, so a launch that races a rewrite executes either
//!     the old script or the new one and never a half-written one.

use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// One script embedded in the binary, identified by the name it takes on disk.
#[derive(Clone, Copy, Debug)]
pub struct VendoredScript {
    /// File name to publish under, e.g. `install-jsh.sh`.
    pub name: &'static str,
    /// The script text, normally from `include_str!`.
    pub source: &'static str,
}

impl VendoredScript {
    /// Publish the script under this user's cache directory and return its path.
    pub fn path(&self) -> io::Result<PathBuf> {
        let dir = dirs::cache_dir()
            .ok_or_else(|| io::Error::other("no cache directory for this user"))?
            .join("jterm");
        self.materialize(&dir)
    }

    /// Publish into an explicit directory. Public for tests, which must not
    /// write to the developer's real cache.
    pub fn materialize(&self, dir: &Path) -> io::Result<PathBuf> {
        crate::snapshot_file::ensure_private_directory(dir)?;
        let path = dir.join(self.name);

        let current_matches = match self.open_cached(&path) {
            Ok(Some(file)) => self.cached_matches(file)?,
            Ok(None) | Err(_) => false,
        };
        if !current_matches {
            crate::atomic_file::write_atomic(&path, self.source.as_bytes())?;
        }

        let file = self.open_cached(&path)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("cached {} disappeared", self.name),
            )
        })?;
        file.set_permissions(std::fs::Permissions::from_mode(0o700))?;
        file.sync_all()?;
        Ok(path)
    }

    fn open_cached(&self, path: &Path) -> io::Result<Option<std::fs::File>> {
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let metadata = file.metadata()?;
        // SAFETY: geteuid has no preconditions and only reads process state.
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.nlink() != 1
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("cached {} is not a private regular file", self.name),
            ));
        }
        Ok(Some(file))
    }

    fn cached_matches(&self, file: std::fs::File) -> io::Result<bool> {
        if file.metadata()?.len() != self.source.len() as u64 {
            return Ok(false);
        }
        let mut bytes = Vec::with_capacity(self.source.len());
        file.take(self.source.len() as u64 + 1)
            .read_to_end(&mut bytes)?;
        Ok(bytes == self.source.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::VendoredScript;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::path::PathBuf;

    const SCRIPT: VendoredScript = VendoredScript {
        name: "example.sh",
        source: "#!/bin/sh\nexit 0\n",
    };

    /// A scratch directory of its own per test. The crate has no tempfile
    /// dependency, and these tests must not publish into the developer's real
    /// cache, so they follow the same pid+counter convention `jsh_install`
    /// already uses.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(label: &str) -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Scratch(std::env::temp_dir().join(format!(
                "jterm-core-vendored-{label}-{}-{id}",
                std::process::id()
            )))
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn publishes_a_private_executable_copy() {
        let scratch = Scratch::new("publish");
        let path = SCRIPT.materialize(&scratch.0).expect("materialize");

        assert_eq!(std::fs::read_to_string(&path).expect("read"), SCRIPT.source);
        let mode = |p: &std::path::Path| {
            std::fs::metadata(p).expect("metadata").permissions().mode() & 0o777
        };
        assert_eq!(mode(&path), 0o700);
        assert_eq!(mode(&scratch.0), 0o700);
    }

    #[test]
    fn a_stale_copy_is_replaced_and_a_current_one_is_reused() {
        let scratch = Scratch::new("stale");
        let path = SCRIPT.materialize(&scratch.0).expect("first");

        // A copy from an older build must not survive an upgrade.
        std::fs::write(&path, "#!/bin/sh\nexit 9\n").expect("stale");
        let again = SCRIPT.materialize(&scratch.0).expect("second");
        assert_eq!(again, path);
        assert_eq!(std::fs::read_to_string(&path).expect("read"), SCRIPT.source);

        // An identical copy is left alone, so a launch does not rewrite a file
        // another launch may be executing right now.
        let before = std::fs::metadata(&path).expect("metadata").ino();
        SCRIPT.materialize(&scratch.0).expect("third");
        assert_eq!(std::fs::metadata(&path).expect("metadata").ino(), before);
    }

    #[test]
    fn two_scripts_do_not_collide_in_one_directory() {
        const OTHER: VendoredScript = VendoredScript {
            name: "other.sh",
            source: "#!/bin/sh\nexit 1\n",
        };
        let scratch = Scratch::new("pair");
        let first = SCRIPT.materialize(&scratch.0).expect("first");
        let second = OTHER.materialize(&scratch.0).expect("second");

        assert_ne!(first, second);
        assert_eq!(
            std::fs::read_to_string(&first).expect("read"),
            SCRIPT.source
        );
        assert_eq!(
            std::fs::read_to_string(&second).expect("read"),
            OTHER.source
        );
    }
}
