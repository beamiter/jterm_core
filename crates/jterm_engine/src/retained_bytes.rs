//! Bounded retention for terminal-owned storage.
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

pub struct RetainedBytesBudget {
    limit: usize,
    retained: AtomicUsize,
    peak: AtomicUsize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetainedBytesUsage {
    pub retained_bytes: usize,
    pub peak_retained_bytes: usize,
    pub capacity_bytes: usize,
}

impl RetainedBytesBudget {
    pub fn new(limit: usize) -> Self {
        Self {
            limit,
            retained: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        }
    }

    pub fn usage(&self) -> RetainedBytesUsage {
        let retained = self.retained.load(Ordering::Relaxed);
        RetainedBytesUsage {
            retained_bytes: retained,
            peak_retained_bytes: self.peak.load(Ordering::Relaxed).max(retained),
            capacity_bytes: self.limit,
        }
    }

    /// Keep this lease with the allocation's last application owner.
    pub fn reserve(self: &Arc<Self>, bytes: usize) -> Option<RetainedBytes> {
        self.reserve_below(bytes, self.limit)
    }

    /// Leave capacity for callers that can use the full process limit.
    pub fn reserve_below(self: &Arc<Self>, bytes: usize, ceiling: usize) -> Option<RetainedBytes> {
        let ceiling = ceiling.min(self.limit);
        self.try_charge_below(bytes, ceiling)
            .then(|| RetainedBytes {
                budget: Arc::clone(self),
                bytes,
                ceiling,
            })
    }

    pub(crate) fn try_charge(&self, bytes: usize) -> bool {
        self.try_charge_below(bytes, self.limit)
    }

    fn try_charge_below(&self, bytes: usize, ceiling: usize) -> bool {
        if bytes == 0 {
            return true;
        }
        // Atomic::try_update is unavailable on the engine's Rust 1.86 MSRV.
        #[allow(deprecated)]
        let Ok(previous) =
            self.retained
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                    used.checked_add(bytes)
                        .filter(|combined| *combined <= ceiling)
                })
        else {
            return false;
        };
        self.peak.fetch_max(previous + bytes, Ordering::Relaxed);
        true
    }

    pub(crate) fn release(&self, bytes: usize) {
        if bytes != 0 {
            let previous = self.retained.fetch_sub(bytes, Ordering::Relaxed);
            debug_assert!(previous >= bytes);
        }
    }
}

pub struct RetainedBytes {
    budget: Arc<RetainedBytesBudget>,
    bytes: usize,
    ceiling: usize,
}

impl RetainedBytes {
    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }

    pub(crate) fn try_resize(&mut self, bytes: usize) -> bool {
        if !self
            .budget
            .try_charge_below(bytes.saturating_sub(self.bytes), self.ceiling)
        {
            return false;
        }
        self.budget.release(self.bytes.saturating_sub(bytes));
        self.bytes = bytes;
        true
    }
}

impl Drop for RetainedBytes {
    fn drop(&mut self) {
        self.budget.release(self.bytes);
    }
}
