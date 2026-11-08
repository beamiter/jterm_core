//! Scheduling-only notification boundary for terminal runtime workers.
//!
//! The queue owns the data and its ordering. A wake conveys no payload and may
//! be coalesced by the frontend. Call it after publishing, outside queue locks.
//! Callbacks must be nonblocking, thread-safe, and must not panic. UI adapters
//! schedule their event loop here; they must not access thread-affine widgets.
use std::sync::Arc;

#[derive(Clone)]
pub struct WakeSink(Arc<dyn Fn() + Send + Sync + 'static>);

impl WakeSink {
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Self {
        Self(Arc::new(wake))
    }

    pub fn wake(&self) {
        (self.0)();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn clones_notify_the_same_sink_from_worker_threads() {
        let count = Arc::new(AtomicUsize::new(0));
        let observed = count.clone();
        let sink = WakeSink::new(move || {
            observed.fetch_add(1, Ordering::SeqCst);
        });
        let worker_sink = sink.clone();
        std::thread::spawn(move || worker_sink.wake())
            .join()
            .unwrap();
        sink.wake();
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn wake_observes_previously_published_data() {
        let queue = Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed = queue.clone();
        let sink = WakeSink::new(move || assert_eq!(*observed.lock().unwrap(), [42]));
        queue.lock().unwrap().push(42);
        sink.wake();
    }
}
