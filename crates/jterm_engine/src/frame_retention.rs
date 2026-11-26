//! Shared retention for published decoded frames. A publisher may defer
//! delivery when capacity is unavailable without delaying a PTY read.
use crate::{
    frame::CellFrame,
    retained_bytes::{RetainedBytes, RetainedBytesBudget, RetainedBytesUsage},
};
use std::{
    ops::Deref,
    sync::{Arc, OnceLock},
};

pub const PROCESS_PUBLISHED_FRAME_BYTES: usize = 128 * 1024 * 1024;

pub fn process_frame_budget() -> &'static Arc<RetainedBytesBudget> {
    static PROCESS: OnceLock<Arc<RetainedBytesBudget>> = OnceLock::new();
    PROCESS.get_or_init(|| Arc::new(RetainedBytesBudget::new(PROCESS_PUBLISHED_FRAME_BYTES)))
}

pub fn process_frame_usage() -> RetainedBytesUsage {
    process_frame_budget().usage()
}

#[derive(Clone)]
pub struct RetainedCellFrame(Arc<Storage>);

struct Storage {
    frame: CellFrame,
    lease: RetainedBytes,
}

// Return the original frame on a budget miss so callers can retry without allocating.
#[allow(clippy::result_large_err)]
impl RetainedCellFrame {
    pub fn try_retain(
        frame: CellFrame,
        budget: &Arc<RetainedBytesBudget>,
    ) -> Result<Self, CellFrame> {
        Self::try_retain_below(frame, budget, usize::MAX)
    }

    pub fn try_retain_below(
        frame: CellFrame,
        budget: &Arc<RetainedBytesBudget>,
        ceiling: usize,
    ) -> Result<Self, CellFrame> {
        let Some(lease) = budget.reserve_below(frame.retained_delivery_bytes(), ceiling) else {
            return Err(frame);
        };
        Ok(Self(Arc::new(Storage { frame, lease })))
    }

    /// Replace the mailbox's unborrowed frame using its original charge.
    /// Borrowed frames remain immutable and keep their reservation.
    pub fn try_replace(&mut self, frame: CellFrame) -> Result<(), CellFrame> {
        let Some(storage) = Arc::get_mut(&mut self.0) else {
            return Err(frame);
        };
        let before = storage.lease.bytes();
        let after = frame.retained_delivery_bytes();
        if after > before && !storage.lease.try_resize(after) {
            return Err(frame);
        }
        storage.frame = frame;
        if after < before {
            storage.lease.try_resize(after);
        }
        Ok(())
    }
}

impl Deref for RetainedCellFrame {
    type Target = CellFrame;

    fn deref(&self) -> &Self::Target {
        &self.0.frame
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::Terminal;

    #[test]
    fn borrow_shares_one_charge_and_full_mailbox_replaces_only_after_release() {
        let mut terminal = Terminal::new(3, 10).unwrap();
        let initial = terminal.frame().unwrap();
        let capacity = initial.retained_delivery_bytes();
        let budget = Arc::new(RetainedBytesBudget::new(capacity));
        let mut mailbox = RetainedCellFrame::try_retain(initial, &budget).unwrap();
        let borrowed = mailbox.clone();
        let revision = borrowed.stamp().revision;
        assert_eq!(budget.usage().retained_bytes, capacity);

        terminal.feed(b"latest").unwrap();
        let next = mailbox.try_replace(terminal.frame().unwrap()).unwrap_err();
        let next = match RetainedCellFrame::try_retain(next, &budget) {
            Ok(_) => panic!("a borrowed frame still owns the entire process allowance"),
            Err(next) => next,
        };
        assert_eq!(borrowed.stamp().revision, revision);
        drop(borrowed);
        mailbox.try_replace(next).unwrap();
        assert!(mailbox.stamp().revision > revision);
        assert_eq!(budget.usage().retained_bytes, capacity);
        drop(mailbox);
        assert_eq!(budget.usage().retained_bytes, 0);
    }
}
