//! Read-only, bounded publication from the existing PTY owner.
use crate::viewport::ViewportFrame;
use jterm_engine::{
    colors::ColorState,
    frame_retention::{process_frame_budget, RetainedCellFrame, PROCESS_PUBLISHED_FRAME_BYTES},
    retained_bytes::RetainedBytesBudget,
};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};

const MAX_OBSERVERS: usize = 8;
const FOREGROUND_FRAME_HEADROOM: usize = 16 * 1024 * 1024;

/// Immutable owner-grid presentation. This is not another terminal or PTY.
pub struct ObserverFrame {
    pub frame: RetainedCellFrame,
    pub live_margin_gaps: u128,
    pub colors: Option<(u64, Arc<ColorState>)>,
    pub closed: bool,
    pub exit_code: Option<i32>,
}

pub(crate) struct ObserverHub {
    readers: AtomicUsize,
    requested: AtomicBool,
    latest: Mutex<Option<Arc<ObserverFrame>>>,
    closed: AtomicBool,
    failure: Mutex<Option<String>>,
    budget: Arc<RetainedBytesBudget>,
    ceiling: usize,
}

impl Default for ObserverHub {
    fn default() -> Self {
        Self::with_budget(
            Arc::clone(process_frame_budget()),
            PROCESS_PUBLISHED_FRAME_BYTES - FOREGROUND_FRAME_HEADROOM,
        )
    }
}

/// A capability that can only read immutable owner frames. Dropping it cannot
/// stop, resize, retain, or send input to the owner's PTY.
pub struct RuntimeObserver {
    hub: Arc<ObserverHub>,
}
impl RuntimeObserver {
    /// Nonblocking latest-value read; a slow view never queues output.
    pub fn is_closed(&self) -> bool {
        self.hub.is_closed()
    }
    pub fn failure(&self) -> Option<String> {
        self.hub.failure.try_lock().ok()?.clone()
    }
    pub fn latest(&self) -> Option<Arc<ObserverFrame>> {
        self.hub.latest.try_lock().ok()?.clone()
    }
}
impl Drop for RuntimeObserver {
    fn drop(&mut self) {
        if self.hub.readers.fetch_sub(1, Ordering::Relaxed) == 1 {
            if let Ok(mut latest) = self.hub.latest.lock() {
                if !self.hub.active() {
                    latest.take();
                }
            }
        }
    }
}
impl ObserverHub {
    fn with_budget(budget: Arc<RetainedBytesBudget>, ceiling: usize) -> Self {
        Self {
            readers: AtomicUsize::new(0),
            requested: AtomicBool::new(false),
            latest: Mutex::new(None),
            closed: AtomicBool::new(false),
            failure: Mutex::new(None),
            budget,
            ceiling,
        }
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    pub(crate) fn subscribe(self: &Arc<Self>) -> Result<RuntimeObserver, String> {
        self.readers
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                (n < MAX_OBSERVERS).then_some(n + 1)
            })
            .map_err(|_| "At most eight read-only terminal observers can be open".to_owned())?;
        self.requested.store(true, Ordering::Relaxed);
        Ok(RuntimeObserver { hub: self.clone() })
    }
    pub(crate) fn request_refresh(&self) {
        self.requested.store(true, Ordering::Relaxed);
    }
    pub(crate) fn active(&self) -> bool {
        self.readers.load(Ordering::Relaxed) != 0
    }
    pub(crate) fn requested(&self) -> bool {
        self.active() && self.requested.load(Ordering::Relaxed)
    }
    /// Keep the previous coherent view when its allocation is still held by
    /// a consumer. The owner can retry publication after that consumer yields.
    pub(crate) fn publish(
        &self,
        view: ViewportFrame,
        closed: bool,
        exit_code: Option<i32>,
    ) -> bool {
        let published = self.latest.lock().ok().is_some_and(|mut latest| {
            if !self.active() {
                return true;
            }
            let mut next = view.frame;
            if let Some(previous) = latest.as_mut().and_then(Arc::get_mut) {
                match previous.frame.try_replace(next) {
                    Ok(()) => {
                        previous.live_margin_gaps = view.live_margin_gaps;
                        previous.colors = view.colors;
                        previous.closed = closed;
                        previous.exit_code = exit_code;
                        return true;
                    }
                    Err(frame) => next = frame,
                }
            }
            let Ok(frame) = RetainedCellFrame::try_retain_below(next, &self.budget, self.ceiling)
            else {
                return false;
            };
            *latest = Some(Arc::new(ObserverFrame {
                frame,
                live_margin_gaps: view.live_margin_gaps,
                colors: view.colors,
                closed,
                exit_code,
            }));
            true
        });
        if !published {
            if closed {
                self.close(exit_code);
            }
            self.request_refresh();
            return false;
        }
        self.closed.store(closed, Ordering::Release);
        if let Ok(mut failure) = self.failure.lock() {
            *failure = None;
        }
        self.requested.store(false, Ordering::Relaxed);
        true
    }
    pub(crate) fn report_failure(&self, error: &str) {
        if let Ok(mut failure) = self.failure.lock() {
            *failure = Some(error.chars().take(1024).collect());
        }
        self.requested.store(false, Ordering::Relaxed);
    }
    pub(crate) fn close(&self, exit_code: Option<i32>) {
        if let Ok(mut latest) = self.latest.lock() {
            if let Some(previous) = latest.take() {
                *latest = Some(Arc::new(ObserverFrame {
                    frame: previous.frame.clone(),
                    live_margin_gaps: previous.live_margin_gaps,
                    colors: previous.colors.clone(),
                    closed: true,
                    exit_code: exit_code.or(previous.exit_code),
                }));
            }
        }
        self.closed.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn publication_is_latest_only_shared_and_close_is_observable_without_a_frame() {
        let hub = Arc::new(ObserverHub::default());
        let first = hub.subscribe().unwrap();
        let second = hub.subscribe().unwrap();
        let terminal = jterm_engine::terminal::Terminal::new(2, 8).unwrap();
        let request = crate::viewport::ViewportRequest {
            top: None,
            painted_history: None,
        };
        hub.publish(
            request.capture(&terminal, false).unwrap().unwrap(),
            false,
            None,
        );
        let old = first.latest().unwrap();
        assert!(Arc::ptr_eq(&old, &second.latest().unwrap()));
        hub.publish(
            request.capture(&terminal, false).unwrap().unwrap(),
            false,
            None,
        );
        assert!(!Arc::ptr_eq(&old, &first.latest().unwrap()));
        drop(old);
        assert_eq!(Arc::strong_count(&first.latest().unwrap()), 2);
        hub.close(Some(7));
        assert!(first.is_closed() && second.latest().unwrap().closed);
        assert_eq!(first.latest().unwrap().exit_code, Some(7));
        hub.close(None);
        assert_eq!(
            first.latest().unwrap().exit_code,
            Some(7),
            "disconnect must preserve observed exit status"
        );
        let empty = Arc::new(ObserverHub::default());
        let observer = empty.subscribe().unwrap();
        empty.close(None);
        assert!(observer.is_closed() && observer.latest().is_none());
    }

    #[test]
    fn observer_capture_respects_synchronized_publication_and_last_detach_releases_frame() {
        let hub = Arc::new(ObserverHub::default());
        let observer = hub.subscribe().unwrap();
        let request = crate::viewport::ViewportRequest {
            top: None,
            painted_history: None,
        };
        let mut terminal = jterm_engine::terminal::Terminal::new(2, 8).unwrap();
        hub.publish(
            request.capture(&terminal, false).unwrap().unwrap(),
            false,
            None,
        );
        let before = observer.latest().unwrap();
        terminal.feed(b"\x1b[?2026hhidden").unwrap();
        assert!(request.capture(&terminal, false).unwrap().is_none());
        assert!(Arc::ptr_eq(&before, &observer.latest().unwrap()));
        terminal.feed(b"\x1b[?2026l").unwrap();
        hub.publish(
            request.capture(&terminal, false).unwrap().unwrap(),
            false,
            None,
        );
        assert!(!Arc::ptr_eq(&before, &observer.latest().unwrap()));
        drop(observer);
        assert!(hub.latest.lock().unwrap().is_none());
        assert!(!hub.requested());
    }

    #[test]
    fn subscriptions_are_bounded_and_detach_without_retaining_a_runtime() {
        let hub = Arc::new(ObserverHub::default());
        let views: Vec<_> = (0..MAX_OBSERVERS)
            .map(|_| hub.subscribe().unwrap())
            .collect();
        assert!(hub.subscribe().is_err());
        assert!(hub.active());
        drop(views);
        assert!(!hub.active());
        assert!(hub.subscribe().is_ok());
    }

    #[test]
    fn process_pressure_keeps_a_borrowed_frame_and_recovers_latest_after_release() {
        let mut terminal = jterm_engine::terminal::Terminal::new(2, 12).unwrap();
        let request = crate::viewport::ViewportRequest {
            top: None,
            painted_history: None,
        };
        let capture = |terminal: &jterm_engine::terminal::Terminal| {
            request.capture(terminal, false).unwrap().unwrap()
        };
        let first = capture(&terminal);
        let capacity = first.frame.retained_delivery_bytes();
        let budget = Arc::new(RetainedBytesBudget::new(capacity));
        let hub = Arc::new(ObserverHub::with_budget(Arc::clone(&budget), capacity));
        let observer = hub.subscribe().unwrap();
        assert!(hub.publish(first, false, None));
        let borrowed = observer.latest().unwrap();
        let revision = borrowed.frame.stamp().revision;

        terminal.feed(b"latest").unwrap();
        assert!(!hub.publish(capture(&terminal), false, None));
        assert!(hub.requested());
        assert_eq!(observer.latest().unwrap().frame.stamp().revision, revision);
        assert_eq!(budget.usage().retained_bytes, budget.usage().capacity_bytes);
        drop(borrowed);
        assert!(hub.publish(capture(&terminal), false, None));
        assert!(!hub.requested());
        assert!(observer.latest().unwrap().frame.stamp().revision > revision);
        hub.close(Some(13));
        assert_eq!(observer.latest().unwrap().exit_code, Some(13));
        drop(observer);
        assert_eq!(budget.usage().retained_bytes, 0);
    }

    #[test]
    fn observers_leave_room_for_a_foreground_publication() {
        let terminal = jterm_engine::terminal::Terminal::new(2, 12).unwrap();
        let request = crate::viewport::ViewportRequest {
            top: None,
            painted_history: None,
        };
        let capture = || request.capture(&terminal, false).unwrap().unwrap();
        let frame_bytes = capture().frame.retained_delivery_bytes();
        let budget = Arc::new(RetainedBytesBudget::new(2 * frame_bytes));
        let first = Arc::new(ObserverHub::with_budget(Arc::clone(&budget), frame_bytes));
        let second = Arc::new(ObserverHub::with_budget(Arc::clone(&budget), frame_bytes));
        let _first = first.subscribe().unwrap();
        let second_view = second.subscribe().unwrap();
        assert!(first.publish(capture(), false, None));
        assert!(!second.publish(capture(), false, None));
        assert!(second_view.latest().is_none());

        let foreground = capture().reserve_delivery(&budget).unwrap();
        assert_eq!(budget.usage().retained_bytes, 2 * frame_bytes);
        drop(foreground);
        assert_eq!(budget.usage().retained_bytes, frame_bytes);
    }
}
