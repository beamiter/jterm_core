//! Single-flight workflow discovery without filesystem work on a UI thread.

use std::collections::VecDeque;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, OnceLock};

use parking_lot::{Condvar, Mutex};

const MAX_PENDING_REFRESHES: usize = 64;

/// Immutable last-good data and the current refresh state. Cloning this value
/// does not copy workflow commands or diagnostics.
#[derive(Debug)]
pub struct RefreshSnapshot<T> {
    pub value: Arc<T>,
    /// Changes on admission, completion, and failure, not on reads.
    pub revision: u64,
    pub loading: bool,
    /// A failed refresh retains `value`; this text contains no file content.
    pub error: Option<String>,
}

impl<T> Clone for RefreshSnapshot<T> {
    fn clone(&self) -> Self {
        Self {
            value: self.value.clone(),
            revision: self.revision,
            loading: self.loading,
            error: self.error.clone(),
        }
    }
}

/// Keep this handle at application scope and share it between open pickers.
/// Repeated refreshes coalesce while one is queued or running. Dropping every
/// handle cancels a queued refresh; a running filesystem call may finish, but
/// cannot publish anywhere. No thread is joined on the caller's thread.
#[derive(Debug)]
pub struct BackgroundRefresh<T> {
    state: Arc<Mutex<RefreshSnapshot<T>>>,
}

impl<T> Clone for BackgroundRefresh<T> {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
        }
    }
}

impl<T: Send + Sync + 'static> BackgroundRefresh<T> {
    pub fn new(initial: T) -> Self {
        Self {
            state: Arc::new(Mutex::new(RefreshSnapshot {
                value: Arc::new(initial),
                revision: 0,
                loading: false,
                error: None,
            })),
        }
    }

    pub fn snapshot(&self) -> RefreshSnapshot<T> {
        self.state.lock().clone()
    }

    /// Schedule discovery, returning false when an existing refresh already
    /// owns this library. Admission failures are observable in `snapshot()`.
    /// All workflow libraries share one worker and a bounded queue, so repeated
    /// picker opens on a stalled mount cannot create more blocked I/O threads.
    pub fn refresh(&self, load: impl FnOnce() -> T + Send + 'static) -> bool {
        self.refresh_on(service(), load)
    }

    fn refresh_on(
        &self,
        service: Result<&Service, &str>,
        load: impl FnOnce() -> T + Send + 'static,
    ) -> bool {
        {
            let mut state = self.state.lock();
            if state.loading {
                return false;
            }
            state.loading = true;
            state.error = None;
            state.revision = state.revision.wrapping_add(1);
        }
        let weak = Arc::downgrade(&self.state);
        let live = weak.clone();
        let job = Job {
            live: Box::new(move || live.strong_count() > 0),
            run: Box::new(move || {
                if weak.strong_count() == 0 {
                    return;
                }
                let result = catch_unwind(AssertUnwindSafe(load));
                if let Some(state) = weak.upgrade() {
                    let result = result.map(Arc::new);
                    let old_value = {
                        let mut state = state.lock();
                        let old_value = match result {
                            Ok(value) => Some(std::mem::replace(&mut state.value, value)),
                            Err(_) => {
                                state.error = Some("Workflow refresh failed unexpectedly".into());
                                None
                            }
                        };
                        state.loading = false;
                        state.revision = state.revision.wrapping_add(1);
                        old_value
                    };
                    // Freeing a large old library must not hold the UI's
                    // short snapshot lock while dropping its command strings.
                    drop(old_value);
                }
            }),
        };
        let admitted = service.and_then(|service| service.submit(job));
        if let Err(error) = admitted {
            let mut state = self.state.lock();
            state.loading = false;
            state.error = Some(error.into());
            state.revision = state.revision.wrapping_add(1);
        }
        true
    }
}

struct Job {
    live: Box<dyn Fn() -> bool + Send>,
    run: Box<dyn FnOnce() + Send>,
}

#[derive(Default)]
struct Queue {
    jobs: Mutex<VecDeque<Job>>,
    ready: Condvar,
}

struct Service {
    queue: Arc<Queue>,
}

impl Service {
    fn new() -> Result<Self, &'static str> {
        let service = Self {
            queue: Arc::new(Queue::default()),
        };
        let queue = service.queue.clone();
        std::thread::Builder::new()
            .name("jterm-workflow-loader".into())
            .spawn(move || loop {
                let job = {
                    let mut jobs = queue.jobs.lock();
                    while jobs.is_empty() {
                        queue.ready.wait(&mut jobs);
                    }
                    jobs.pop_front().expect("nonempty workflow queue")
                };
                if (job.live)() {
                    (job.run)();
                }
            })
            .map_err(|_| "Could not start workflow refresh worker")?;
        Ok(service)
    }

    fn submit(&self, job: Job) -> Result<(), &'static str> {
        let mut jobs = self.queue.jobs.lock();
        jobs.retain(|queued| (queued.live)());
        if jobs.len() >= MAX_PENDING_REFRESHES {
            return Err("Workflow refresh queue is full; try again");
        }
        jobs.push_back(job);
        self.queue.ready.notify_one();
        Ok(())
    }
}

fn service() -> Result<&'static Service, &'static str> {
    static SERVICE: OnceLock<Result<Service, &'static str>> = OnceLock::new();
    SERVICE
        .get_or_init(Service::new)
        .as_ref()
        .map_err(|error| *error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    fn wait<T: Send + Sync + 'static>(library: &BackgroundRefresh<T>) -> RefreshSnapshot<T> {
        let until = Instant::now() + Duration::from_secs(3);
        loop {
            let snapshot = library.snapshot();
            if !snapshot.loading {
                return snapshot;
            }
            assert!(Instant::now() < until, "refresh did not complete");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn coalesces_refreshes_and_keeps_last_good_data_readable() {
        let service = Service::new().unwrap();
        let library = BackgroundRefresh::new(vec![1]);
        let before = library.snapshot();
        let (started, start) = mpsc::channel();
        let (release, resume) = mpsc::channel();
        assert!(library.refresh_on(Ok(&service), move || {
            started.send(()).unwrap();
            resume.recv().unwrap();
            vec![2]
        }));
        start.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(!library
            .clone()
            .refresh_on(Ok(&service), || panic!("duplicate must not run")));
        let during = library.snapshot();
        assert!(during.loading);
        assert!(Arc::ptr_eq(&before.value, &during.value));
        release.send(()).unwrap();
        let after = wait(&library);
        assert_eq!(*after.value, vec![2]);
        assert!(after.revision > during.revision);
        assert!(after.error.is_none());
        assert!(Arc::ptr_eq(&after.value, &library.clone().snapshot().value));
    }

    #[test]
    fn panic_retains_last_good_value_and_does_not_retire_worker() {
        let service = Service::new().unwrap();
        let library = BackgroundRefresh::new(7);
        library.refresh_on(Ok(&service), || panic!("fixture panic"));
        let failed = wait(&library);
        assert_eq!(*failed.value, 7);
        assert!(failed.error.is_some());
        library.refresh_on(Ok(&service), || 9);
        let recovered = wait(&library);
        assert_eq!(*recovered.value, 9);
        assert!(recovered.error.is_none());
    }

    #[test]
    fn different_libraries_share_one_io_worker() {
        let service = Service::new().unwrap();
        let first = BackgroundRefresh::new(0);
        let second = BackgroundRefresh::new(0);
        let (started, start) = mpsc::channel();
        let (release, resume) = mpsc::channel();
        first.refresh_on(Ok(&service), move || {
            started.send(()).unwrap();
            resume.recv().unwrap();
            1
        });
        start.recv_timeout(Duration::from_secs(3)).unwrap();
        let (second_started, second_start) = mpsc::channel();
        second.refresh_on(Ok(&service), move || {
            second_started.send(()).unwrap();
            2
        });
        assert!(second_start.try_recv().is_err());
        assert!(second.snapshot().loading);
        release.send(()).unwrap();
        second_start.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(*wait(&first).value, 1);
        assert_eq!(*wait(&second).value, 2);
    }

    #[test]
    fn dropped_libraries_never_run_queued_io_and_free_queue_capacity() {
        let service = Service {
            queue: Arc::new(Queue::default()),
        };
        let called = Arc::new(AtomicUsize::new(0));
        for _ in 0..MAX_PENDING_REFRESHES * 2 {
            let library = BackgroundRefresh::new(0);
            let called = called.clone();
            library.refresh_on(Ok(&service), move || called.fetch_add(1, Ordering::Relaxed));
            drop(library);
        }
        assert_eq!(service.queue.jobs.lock().len(), 1);
        let job = service.queue.jobs.lock().pop_front().unwrap();
        assert!(!(job.live)());
        (job.run)();
        assert_eq!(called.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn bounded_queue_and_startup_failure_are_visible_and_retryable() {
        let service = Service {
            queue: Arc::new(Queue::default()),
        };
        let libraries = (0..MAX_PENDING_REFRESHES)
            .map(|_| BackgroundRefresh::new(0))
            .collect::<Vec<_>>();
        for library in &libraries {
            library.refresh_on(Ok(&service), || 1);
        }
        let extra = BackgroundRefresh::new(42);
        extra.refresh_on(Ok(&service), || 43);
        let snapshot = extra.snapshot();
        assert!(!snapshot.loading);
        assert_eq!(*snapshot.value, 42);
        assert!(snapshot.error.unwrap().contains("full"));
        drop(libraries);
        assert!(extra.refresh_on(Ok(&service), || 44));
        let job = service.queue.jobs.lock().pop_front().unwrap();
        (job.run)();
        assert_eq!(*extra.snapshot().value, 44);
        extra.refresh_on(Err("worker unavailable"), || 45);
        assert_eq!(
            extra.snapshot().error.as_deref(),
            Some("worker unavailable")
        );
        assert!(!extra.snapshot().loading);
    }
}
