//! Shared live provider view with a process-unique change revision.
//!
//! Workers mutate the bounded view through [`ViewCell::lock`]. Any mutable
//! access publishes a new revision before the lock is released, so a host can
//! keep an unchanged snapshot instead of cloning assistant text, command
//! output and diffs on every frame. Revisions come from one process-wide
//! counter: a restarted run can never reuse an older run's revision.

use super::codex_app_server::CodexAppServerViewSnapshot;
use parking_lot::{Mutex, MutexGuard};
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_REVISION: AtomicU64 = AtomicU64::new(1);

/// Allocate a revision no other view in this process has used.
pub(crate) fn next_view_revision() -> u64 {
    NEXT_REVISION.fetch_add(1, Ordering::Relaxed)
}

pub(crate) struct ViewCell {
    view: Mutex<CodexAppServerViewSnapshot>,
    revision: AtomicU64,
}

impl ViewCell {
    pub(crate) fn new(view: CodexAppServerViewSnapshot) -> Self {
        Self {
            view: Mutex::new(view),
            revision: AtomicU64::new(next_view_revision()),
        }
    }

    pub(crate) fn lock(&self) -> ViewGuard<'_> {
        ViewGuard {
            guard: self.view.lock(),
            revision: &self.revision,
            changed: false,
        }
    }

    /// Identity of the current content; any mutable access changes it.
    pub(crate) fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }

    /// The current view and its revision, read under one lock.
    pub(crate) fn snapshot(&self) -> (u64, CodexAppServerViewSnapshot) {
        let guard = self.view.lock();
        (self.revision.load(Ordering::Acquire), guard.clone())
    }
}

pub(crate) struct ViewGuard<'a> {
    guard: MutexGuard<'a, CodexAppServerViewSnapshot>,
    revision: &'a AtomicU64,
    changed: bool,
}

impl Deref for ViewGuard<'_> {
    type Target = CodexAppServerViewSnapshot;

    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}

impl DerefMut for ViewGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.changed = true;
        &mut self.guard
    }
}

impl Drop for ViewGuard<'_> {
    fn drop(&mut self) {
        if self.changed {
            // Fields drop after this body, so the new revision is visible
            // before the mutex is released: content and revision stay paired.
            self.revision.store(next_view_revision(), Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_view_enums_have_readable_labels() {
        use super::super::codex_app_server::{
            CodexAppServerApprovalKind, CodexAppServerExitCause, CodexAppServerPhase,
        };
        use CodexAppServerPhase::*;
        let phases = [
            Created,
            Spawning,
            Initializing,
            StartingThread,
            StartingTurn,
            Ready,
            Running,
            WaitingForApproval,
            Cancelling,
            Stopping,
            Ended,
            Failed,
        ];
        for phase in phases {
            // Readable prose, never a CamelCase enum name such as
            // "WaitingForApproval" leaking into a native view.
            let label = phase.label();
            assert!(!label.is_empty(), "{phase:?}");
            assert!(
                !label.chars().skip(1).any(char::is_uppercase),
                "{phase:?}: {label}"
            );
        }
        assert_eq!(WaitingForApproval.label(), "Waiting for approval");
        assert_eq!(
            CodexAppServerApprovalKind::FileChange.label(),
            "File change"
        );
        assert_eq!(
            CodexAppServerExitCause::SpawnFailed.label(),
            "Provider could not start"
        );
    }

    #[test]
    fn mutation_publishes_a_new_unique_revision_and_reads_do_not() {
        let first = ViewCell::new(CodexAppServerViewSnapshot::default());
        let second = ViewCell::new(CodexAppServerViewSnapshot::default());
        assert_ne!(first.revision(), second.revision());
        let initial = first.revision();
        let _ = first.lock().agent_text.len();
        assert_eq!(first.revision(), initial, "reads keep the revision");
        first.lock().agent_text.push_str("delta");
        let changed = first.revision();
        assert!(changed > initial);
        assert_ne!(changed, second.revision());
        let (revision, view) = first.snapshot();
        assert_eq!((revision, view.agent_text.as_str()), (changed, "delta"));
        {
            let mut guard = first.lock();
            guard.agent_text.push('!');
            assert_eq!(first.revision.load(Ordering::Acquire), changed);
        }
        assert!(first.revision() > changed);
    }

    #[test]
    fn contended_view_publication_keeps_snapshot_and_revision_consistent() {
        use std::sync::{mpsc, Arc, Barrier};
        use std::time::Duration;
        let view = Arc::new(ViewCell::new(CodexAppServerViewSnapshot::default()));
        let start = Arc::new(Barrier::new(5));
        let (done, completed) = mpsc::channel();
        let mut workers = Vec::new();
        for _ in 0..4 {
            let view = view.clone();
            let start = start.clone();
            let done = done.clone();
            workers.push(std::thread::spawn(move || {
                start.wait();
                for _ in 0..500 {
                    view.lock().agent_text.push('x');
                    std::thread::yield_now();
                }
                done.send(()).unwrap();
            }));
        }
        start.wait();
        let mut previous = view.snapshot();
        for _ in 0..2000 {
            let current = view.snapshot();
            assert!(current.0 >= previous.0);
            assert!(current.1.agent_text.len() >= previous.1.agent_text.len());
            if current.1.agent_text.len() != previous.1.agent_text.len() {
                assert!(current.0 > previous.0);
            }
            previous = current;
            std::thread::yield_now();
        }
        for _ in 0..4 {
            completed.recv_timeout(Duration::from_secs(10)).unwrap();
        }
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(view.snapshot().1.agent_text.len(), 2000);
        // A replacement runtime cannot reuse the retired view's revision.
        let replacement = ViewCell::new(view.snapshot().1);
        assert!(replacement.revision() > view.revision());
    }
}
