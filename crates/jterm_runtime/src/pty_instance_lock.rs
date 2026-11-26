//! Pin the application's registered instance flock through a PTY fork.

use std::{
    fs::File,
    io,
    sync::{Arc, Mutex, MutexGuard, Weak},
};

static REGISTERED: Mutex<Weak<File>> = Mutex::new(Weak::new());

pub(crate) fn lock_registration() -> MutexGuard<'static, Weak<File>> {
    REGISTERED.lock().unwrap_or_else(|error| error.into_inner())
}

/// Keeps an application-acquired flock alive and makes its descriptor
/// available to the launcher until the next fork can close the child copy.
pub struct InstanceLock {
    _file: Arc<File>,
}

impl InstanceLock {
    pub fn register(file: File) -> io::Result<Self> {
        let mut registration = lock_registration();
        if registration.strong_count() != 0 {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "an instance lock descriptor is already registered",
            ));
        }
        let file = Arc::new(file);
        *registration = Arc::downgrade(&file);
        Ok(Self { _file: file })
    }
}

#[cfg(unix)]
impl std::os::fd::AsFd for InstanceLock {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self._file.as_fd()
    }
}
