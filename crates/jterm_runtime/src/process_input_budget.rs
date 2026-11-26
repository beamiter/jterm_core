//! Process-wide admission for runtime-owned, accepted PTY input payloads.
//!
//! Every queued byte is reserved before admission. Worker messages keep their
//! charge through the last write attempt. Owner-driven input releases written
//! bytes, and the parser reserves space for replies before reading raw output.
//! This accounts for payload and reply reservations. Allocator spare capacity,
//! producer-owned retry buffers, and renderer allocations have separate lifetimes.

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, OnceLock,
};

// The existing writer permits a 64 MiB queue and up to a 48 MiB reply. Keep
// four such queue allowances in the overall ceiling, reserving one of those
// allowances for replies across the process. Per-queue limits still apply.
const CAPACITY: usize = 256 * 1024 * 1024;
const REPLY_RESERVE: usize = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessInputUsage {
    pub reserved_bytes: usize,
    pub peak_reserved_bytes: usize,
    pub capacity_bytes: usize,
    pub reply_reserve_bytes: usize,
}

pub(crate) fn shared_budget() -> &'static Arc<Budget> {
    static PROCESS: OnceLock<Arc<Budget>> = OnceLock::new();
    PROCESS.get_or_init(|| Arc::new(Budget::new(CAPACITY, REPLY_RESERVE)))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Full {
    pub(crate) pending: usize,
    pub(crate) capacity: usize,
}

pub(crate) struct Budget {
    pending: AtomicUsize,
    peak: AtomicUsize,
    capacity: usize,
    reply_reserve: usize,
}

impl Budget {
    pub(crate) fn new(capacity: usize, reply_reserve: usize) -> Self {
        assert!(reply_reserve <= capacity);
        Self {
            pending: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            capacity,
            reply_reserve,
        }
    }

    pub(crate) fn usage(&self) -> ProcessInputUsage {
        let pending_bytes = self.pending.load(Ordering::Relaxed);
        ProcessInputUsage {
            reserved_bytes: pending_bytes,
            peak_reserved_bytes: self.peak.load(Ordering::Relaxed).max(pending_bytes),
            capacity_bytes: self.capacity,
            reply_reserve_bytes: self.reply_reserve,
        }
    }

    pub(crate) fn empty_charge(self: &Arc<Self>) -> Charge {
        Charge {
            budget: Arc::clone(self),
            bytes: 0,
        }
    }
}

pub(crate) struct Charge {
    budget: Arc<Budget>,
    bytes: usize,
}

impl Charge {
    #[cfg(all(target_os = "linux", feature = "linux-pty"))]
    pub(crate) fn sibling(&self) -> Self {
        self.budget.empty_charge()
    }

    #[cfg(all(target_os = "linux", feature = "linux-pty"))]
    pub(crate) fn transfer_to(&mut self, receiver: &mut Self, bytes: usize) {
        assert!(Arc::ptr_eq(&self.budget, &receiver.budget));
        assert!(
            bytes <= self.bytes,
            "transferred reply beyond the read reservation"
        );
        self.bytes -= bytes;
        receiver.bytes += bytes;
    }

    pub(crate) fn try_add(&mut self, extra: usize, reply: bool) -> Result<(), Full> {
        if extra == 0 {
            return Ok(());
        }
        let capacity = if reply {
            self.budget.capacity
        } else {
            self.budget.capacity - self.budget.reply_reserve
        };
        // Atomic::try_update is unavailable on the runtime's Rust 1.89 MSRV.
        #[allow(deprecated)]
        let previous = self
            .budget
            .pending
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                used.checked_add(extra).filter(|next| *next <= capacity)
            })
            .map_err(|pending| Full { pending, capacity })?;
        self.bytes += extra;
        self.budget
            .peak
            .fetch_max(previous + extra, Ordering::Relaxed);
        Ok(())
    }

    pub(crate) fn release(&mut self, bytes: usize) {
        assert!(
            bytes <= self.bytes,
            "released input without an outstanding charge"
        );
        self.bytes -= bytes;
        if bytes != 0 {
            self.budget.pending.fetch_sub(bytes, Ordering::Relaxed);
        }
    }
}

impl Drop for Charge {
    fn drop(&mut self) {
        self.release(self.bytes);
    }
}
