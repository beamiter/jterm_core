//! Coalesced render publication notifications, independent of a UI executor.
//!
//! Each subscriber owns one latest-value slot. A slow or hidden frontend never
//! queues old frames; subscriptions neither retain a session nor run UI code on
//! its socket worker. Wakers must be dispatched after releasing Shared's lock.

use super::Vim9RenderFrame;
use parking_lot::{Condvar, Mutex};
use std::future::poll_fn;
use std::io;
use std::sync::{Arc, Weak};
use std::task::{Poll, Waker};
use std::time::{Duration, Instant};

/// One accepted host publication, not a compositor/presentation timestamp.
#[derive(Clone)]
pub struct Vim9RenderUpdate {
    /// Monotonic within this session; also advances when native paint clears.
    pub generation: u64,
    /// Latest immutable paint, or an explicit return to compatibility display.
    pub frame: Option<Arc<Vim9RenderFrame>>,
    /// Host monotonic clock sampled after validation and before notification.
    pub published_at: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Vim9RenderWaitError {
    Timeout,
    Closed,
}

#[derive(Default)]
struct State {
    pending: Option<Vim9RenderUpdate>,
    closed: bool,
    waker: Option<Waker>,
}

#[derive(Default)]
struct Slot {
    state: Mutex<State>,
    ready: Condvar,
}

/// A single-consumer latest-frame subscription. Use either the blocking API on
/// a dedicated worker, or `changed().await` on a frontend's existing executor.
/// It owns no Vim9Session; dropping the editor still disconnects the bridge.
pub struct Vim9RenderWatch(Arc<Slot>);

/// Cancellation does not revoke the editor or affect any other subscriber.
/// Suitable for promptly stopping a frontend's blocking repaint worker.
pub struct Vim9RenderCancel(Weak<Slot>);

impl Vim9RenderCancel {
    pub fn cancel(&self) {
        if let Some(slot) = self.0.upgrade() {
            let waker = {
                let mut state = slot.state.lock();
                state.closed = true;
                state.pending = None;
                state.waker.take()
            };
            slot.ready.notify_all();
            if let Some(waker) = waker {
                waker.wake();
            }
        }
    }
}

impl Vim9RenderWatch {
    pub fn cancellation(&self) -> Vim9RenderCancel {
        Vim9RenderCancel(Arc::downgrade(&self.0))
    }

    /// Blocks without periodic wakeups. Never call on a UI/event-loop thread.
    pub fn recv(&mut self) -> Option<Vim9RenderUpdate> {
        let mut state = self.0.state.lock();
        loop {
            if let Some(update) = state.pending.take() {
                return Some(update);
            }
            if state.closed {
                return None;
            }
            self.0.ready.wait(&mut state);
        }
    }

    /// Bounded wait for tests/measurement. A timeout does not fabricate a frame.
    pub fn recv_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<Vim9RenderUpdate, Vim9RenderWaitError> {
        let start = Instant::now();
        let mut state = self.0.state.lock();
        loop {
            if let Some(update) = state.pending.take() {
                return Ok(update);
            }
            if state.closed {
                return Err(Vim9RenderWaitError::Closed);
            }
            let remaining = timeout.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                return Err(Vim9RenderWaitError::Timeout);
            }
            self.0.ready.wait_for(&mut state, remaining);
        }
    }

    /// Executor-neutral, cancellation-safe wait. Publication and waker
    /// registration share a lock, so an arrival cannot fall between them.
    pub async fn changed(&mut self) -> Option<Vim9RenderUpdate> {
        poll_fn(|cx| {
            let mut state = self.0.state.lock();
            if let Some(update) = state.pending.take() {
                state.waker = None;
                return Poll::Ready(Some(update));
            }
            if state.closed {
                state.waker = None;
                return Poll::Ready(None);
            }
            if !state
                .waker
                .as_ref()
                .is_some_and(|w| w.will_wake(cx.waker()))
            {
                state.waker = Some(cx.waker().clone());
            }
            Poll::Pending
        })
        .await
    }
}

pub(super) struct Publisher {
    subscribers: Vec<Weak<Slot>>,
    generation: u64,
    published_at: Instant,
    closed: bool,
}

impl Default for Publisher {
    fn default() -> Self {
        Self {
            subscribers: Vec::new(),
            generation: 0,
            published_at: Instant::now(),
            closed: false,
        }
    }
}

impl Publisher {
    pub(super) fn subscribe(
        &mut self,
        frame: Option<Arc<Vim9RenderFrame>>,
    ) -> io::Result<Vim9RenderWatch> {
        self.subscribers
            .retain(|s| s.upgrade().is_some_and(|s| !s.state.lock().closed));
        if self.closed {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "Vim render publication ended",
            ));
        }
        if self.subscribers.len() >= 32 {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "Too many Vim render subscribers",
            ));
        }
        let slot = Arc::new(Slot::default());
        slot.state.lock().pending = Some(Vim9RenderUpdate {
            generation: self.generation,
            frame,
            published_at: self.published_at,
        });
        self.subscribers.push(Arc::downgrade(&slot));
        Ok(Vim9RenderWatch(slot))
    }

    pub(super) fn publish(
        &mut self,
        frame: Option<Arc<Vim9RenderFrame>>,
        close: bool,
    ) -> Vec<Waker> {
        if self.closed {
            return Vec::new();
        }
        self.generation = self.generation.saturating_add(1);
        self.published_at = Instant::now();
        self.closed = close;
        let update = Vim9RenderUpdate {
            generation: self.generation,
            frame,
            published_at: self.published_at,
        };
        let mut wakers = Vec::new();
        self.subscribers.retain(|weak| {
            let Some(slot) = weak.upgrade() else {
                return false;
            };
            let mut state = slot.state.lock();
            if state.closed {
                return false;
            }
            // Overwrite, never enqueue: a stalled frontend receives the newest
            // validated full frame or explicit clear, not an obsolete delta.
            state.pending = Some(update.clone());
            state.closed = close;
            if let Some(waker) = state.waker.take() {
                wakers.push(waker);
            }
            slot.ready.notify_all();
            true
        });
        wakers
    }
}

pub(super) fn dispatch(wakers: Vec<Waker>) {
    for waker in wakers {
        waker.wake();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Wake};

    #[derive(Default)]
    struct Counter(AtomicUsize);
    impl Wake for Counter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn coalesces_bursts_without_retaining_old_frames() {
        let mut publisher = Publisher::default();
        let mut watch = publisher.subscribe(None).unwrap();
        assert_eq!(watch.recv().unwrap().generation, 0);
        for _ in 0..10000 {
            dispatch(publisher.publish(None, false));
        }
        assert_eq!(watch.recv().unwrap().generation, 10000);
        assert!(matches!(
            watch.recv_timeout(Duration::ZERO),
            Err(Vim9RenderWaitError::Timeout)
        ));
    }

    #[test]
    fn async_registration_and_repoll_cannot_lose_publication() {
        let mut publisher = Publisher::default();
        let mut watch = publisher.subscribe(None).unwrap();
        watch.recv();
        let counter = Arc::new(Counter::default());
        let waker = Waker::from(counter.clone());
        let mut context = Context::from_waker(&waker);
        let mut next = Box::pin(watch.changed());
        assert!(next.as_mut().poll(&mut context).is_pending());
        let wakers = publisher.publish(None, false);
        assert_eq!(
            counter.0.load(Ordering::Relaxed),
            0,
            "dispatch is explicitly outside the publisher lock"
        );
        dispatch(wakers);
        assert_eq!(counter.0.load(Ordering::Relaxed), 1);
        assert!(matches!(
            next.as_mut().poll(&mut context),
            Poll::Ready(Some(Vim9RenderUpdate { generation: 1, .. }))
        ));
    }

    #[test]
    fn cancelled_future_preserves_the_next_publication_and_replaces_waker() {
        let mut publisher = Publisher::default();
        let mut watch = publisher.subscribe(None).unwrap();
        watch.recv();
        let old = Arc::new(Counter::default());
        let old_waker = Waker::from(old.clone());
        assert!(Box::pin(watch.changed())
            .as_mut()
            .poll(&mut Context::from_waker(&old_waker))
            .is_pending());
        let new = Arc::new(Counter::default());
        let new_waker = Waker::from(new.clone());
        assert!(Box::pin(watch.changed())
            .as_mut()
            .poll(&mut Context::from_waker(&new_waker))
            .is_pending());
        dispatch(publisher.publish(None, false));
        assert_eq!(old.0.load(Ordering::Relaxed), 0);
        assert_eq!(new.0.load(Ordering::Relaxed), 1);
        assert_eq!(watch.recv().unwrap().generation, 1);
    }

    #[test]
    fn independent_consumers_and_cancel_do_not_close_the_publisher() {
        let mut publisher = Publisher::default();
        let mut first = publisher.subscribe(None).unwrap();
        let mut second = publisher.subscribe(None).unwrap();
        first.cancellation().cancel();
        assert!(first.recv().is_none());
        dispatch(publisher.publish(None, false));
        assert_eq!(second.recv().unwrap().generation, 1);
        assert_eq!(publisher.subscribers.len(), 1);
    }

    #[test]
    fn close_replaces_queued_paint_then_terminates_without_session_ownership() {
        let mut publisher = Publisher::default();
        let mut watch = publisher.subscribe(None).unwrap();
        dispatch(publisher.publish(None, true));
        assert!(watch.recv().unwrap().frame.is_none());
        assert!(watch.recv().is_none());
        assert!(publisher.subscribe(None).is_err());
        dispatch(publisher.publish(None, false));
        assert!(watch.recv().is_none());
    }

    #[test]
    fn dead_subscribers_are_reclaimed_and_live_subscribers_are_bounded() {
        let mut publisher = Publisher::default();
        let subscribers: Vec<_> = (0..32)
            .map(|_| publisher.subscribe(None).unwrap())
            .collect();
        assert!(publisher.subscribe(None).is_err());
        drop(subscribers);
        assert!(publisher.subscribe(None).is_ok());
        assert_eq!(publisher.subscribers.len(), 1);
    }

    #[test]
    fn blocking_consumer_is_woken_by_cancellation_without_a_poll_timer() {
        let mut publisher = Publisher::default();
        let mut watch = publisher.subscribe(None).unwrap();
        watch.recv();
        let cancel = watch.cancellation();
        let worker = std::thread::spawn(move || watch.recv().is_none());
        cancel.cancel();
        assert!(worker.join().unwrap());
    }

    #[test]
    fn contended_blocking_publication_and_close_do_not_lose_wakeups() {
        let mut publisher = Publisher::default();
        let mut watch = publisher.subscribe(None).unwrap();
        watch.recv();
        let (ack, received) = std::sync::mpsc::channel();
        let consumer = std::thread::spawn(move || {
            for expected in 1..=1000 {
                let update = watch.recv_timeout(Duration::from_secs(10)).unwrap();
                assert_eq!(update.generation, expected);
                ack.send(()).unwrap();
            }
            assert_eq!(
                watch
                    .recv_timeout(Duration::from_secs(10))
                    .unwrap()
                    .generation,
                1001
            );
            assert!(matches!(
                watch.recv_timeout(Duration::from_secs(10)),
                Err(Vim9RenderWaitError::Closed)
            ));
        });
        for _ in 0..1000 {
            dispatch(publisher.publish(None, false));
            received.recv_timeout(Duration::from_secs(10)).unwrap();
        }
        dispatch(publisher.publish(None, true));
        consumer.join().unwrap();
    }
}
