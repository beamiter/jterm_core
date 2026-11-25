//! Pace live history checkpoints independently of the PTY reader.

use gtk4::glib;
use std::{cell::Cell, io, rc::Rc, sync::Arc, time::Duration};

const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(1);

/// Coalesces completion bursts before copying a pane's history for its archive
/// writer. The application supplies the same transaction it uses on close.
#[derive(Default)]
pub struct HistoryCheckpoint(Rc<State>);

#[derive(Default)]
struct State {
    scheduled: Cell<bool>,
    writes: Arc<()>,
}

impl HistoryCheckpoint {
    /// Schedule one save for the latest state. The callback should borrow its
    /// pane weakly. A continuous stream still gets a checkpoint once the
    /// interval has elapsed and the previous write has finished.
    pub fn schedule(&self, save: impl FnOnce() + 'static) {
        if self.0.scheduled.replace(true) {
            return;
        }
        let state = Rc::downgrade(&self.0);
        let mut save = Some(save);
        glib::timeout_add_local(CHECKPOINT_INTERVAL, move || {
            let Some(state) = state.upgrade() else {
                return glib::ControlFlow::Break;
            };
            // Only the GTK thread creates new jobs. A job holds the other
            // reference until it finishes or the queue discards it.
            if Arc::strong_count(&state.writes) > 1 {
                return glib::ControlFlow::Continue;
            }
            state.scheduled.set(false);
            if let Some(save) = save.take() {
                save();
            }
            glib::ControlFlow::Break
        });
    }

    /// Retain the write's activity through queue admission, coalescing and
    /// execution. A refused or superseded job releases it on drop.
    pub fn track_write(
        &self,
        write: impl FnOnce() -> io::Result<()> + Send + 'static,
    ) -> impl FnOnce() -> io::Result<()> + Send + 'static {
        let activity = Arc::clone(&self.0.writes);
        move || {
            let _activity = activity;
            write()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn completion_bursts_wait_for_the_queued_write_and_schedule_once() {
        let context = glib::MainContext::default();
        let _owner = context.acquire().expect("own the GLib main context");
        let checkpoint = HistoryCheckpoint::default();
        let queued = checkpoint.track_write(|| Ok(()));
        let saved = Rc::new(Cell::new(0));
        let count = saved.clone();
        checkpoint.schedule(move || count.set(count.get() + 1));
        checkpoint.schedule(|| panic!("a second checkpoint ran for the same completion burst"));

        assert!(context.iteration(true));
        assert_eq!(saved.get(), 0, "a queued write still owns the checkpoint");
        drop(queued);
        let deadline = Instant::now() + Duration::from_secs(3);
        while saved.get() == 0 && Instant::now() < deadline {
            context.iteration(false);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(saved.get(), 1);
    }
}
