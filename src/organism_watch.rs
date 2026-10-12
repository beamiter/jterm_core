//! Content-free observation of output cadence for one visible running command.
//!
//! This draft has no native source, parser, attention arbiter or activity-knowledge
//! policy. The caller supplies truthful start/batch facts and an injected clock.
//! In particular, silence never establishes idle, and a rhythm never grants Hello.
//! The existing frontend attention bridge and command-age policy remain separate.

use std::time::Duration;

use crate::organism::WatchRhythm;

const QUIET_AFTER: Duration = Duration::from_secs(3);
const BUSY_WINDOW: Duration = Duration::from_millis(1_200);
const RESUMED_HOLD: Duration = Duration::from_millis(900);

/// Disposition of one real, nonempty PTY read batch, normalized before delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchOutputDisposition {
    /// Count this batch, unless an attached/quarantined observation needs a baseline.
    Accepted,
    /// Use this batch as a neutral baseline and consume first-batch suppression.
    BaselineOnly,
    /// Clear transient activity, reset the baseline, and suppress the next batch.
    /// This does not reset the sequence high-water mark.
    Quarantined,
}

/// A pending wake, scoped to the complete observation identity supplied separately.
/// The opaque revision distinguishes replacement plans with equal absolute times.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatchDeadline {
    at: Duration,
    revision: u64,
}

impl WatchDeadline {
    pub fn at(self) -> Duration {
        self.at
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchEvent {
    /// A currently observed, trusted command-start event, never a historical record.
    BeginTrustedCommand,
    /// Observe a known-running command without claiming a trustworthy start time.
    AttachRunningObservation,
    /// One batch, not one parser segment. Sequences strictly increase without wrap.
    OutputObserved {
        batch_seq: u64,
        disposition: WatchOutputDisposition,
    },
    /// Update time using an existing host heartbeat, without adding an output fact.
    Advance,
    /// A host callback must return the exact pending token, not just its timestamp.
    DeadlineDue { deadline: WatchDeadline },
    /// End this identity. Restoration requires a fresh context identity and start.
    Retire,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchError {
    StaleIdentity,
    IdentityAlreadyStarted,
    NotObserving,
    StaleBatch,
    StaleDeadline,
    DeadlineNotDue,
    StartTimeRegressed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchOutcome {
    Started,
    Counted,
    BaselineOnly,
    Quarantined,
    /// A fresh sequence was consumed, but backwards time supplied no new activity.
    BackwardsBatchConsumed,
    Advanced,
    Retired,
    /// No revision was reused. A fresh context identity is required to observe again.
    RetiredOnDeadlineExhaustion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatchSnapshot {
    pub rhythm: WatchRhythm,
    /// May be due in a read-only snapshot; consume once through Advance/DeadlineDue.
    /// Every successful mutable update leaves this strictly after the stored clock,
    /// or None. Waiting and retired observations do not request periodic polling.
    pub next_deadline: Option<WatchDeadline>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatchUpdate {
    pub outcome: WatchOutcome,
    pub snapshot: WatchSnapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Prepared,
    Active,
    Retired,
}

/// A volatile reducer with three output slots and one sequence high-water mark.
///
/// `I` is the complete opaque (owner, command epoch, context epoch) identity.
/// The authoritative context coordinator alone calls [`Self::set_identity`].
/// Every identity must be unique over this controller's lifetime, including after
/// A -> B -> A owner changes. A pane ID alone is insufficient. No historical ledger
/// is added to detect caller reuse of an old identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchObservation<I> {
    identity: Option<I>,
    phase: Phase,
    clock: Duration,
    baseline: Option<Duration>,
    outputs: [Option<Duration>; 3],
    first_pending: bool,
    resumed_until: Option<Duration>,
    last_batch: Option<u64>,
    deadline: Option<WatchDeadline>,
    next_revision: Option<u64>,
}

impl<I> Default for WatchObservation<I> {
    fn default() -> Self {
        Self {
            identity: None,
            phase: Phase::Retired,
            clock: Duration::ZERO,
            baseline: None,
            outputs: [None; 3],
            first_pending: false,
            resumed_until: None,
            last_batch: None,
            deadline: None,
            next_revision: Some(0),
        }
    }
}

impl<I: Eq + Clone> WatchObservation<I> {
    /// Bind a fresh context without inventing a start or advancing time.
    /// Repeating the same identity is inert, even after Retire.
    pub fn set_identity(&mut self, identity: Option<I>) -> bool {
        if self.identity == identity {
            return false;
        }
        self.clear_observation();
        self.phase = if identity.is_some() {
            Phase::Prepared
        } else {
            Phase::Retired
        };
        self.identity = identity;
        self.last_batch = None;
        self.next_revision = Some(0);
        true
    }

    pub fn identity(&self) -> Option<&I> {
        self.identity.as_ref()
    }

    /// Read-only presentation at max(now, last accepted time). This neither admits
    /// output nor consumes a pending wake. A stale reader cannot see another owner.
    pub fn snapshot(&self, now: Duration, identity: &I) -> Result<WatchSnapshot, WatchError> {
        self.check_identity(identity)?;
        Ok(self.sample(now.max(self.clock)))
    }

    pub fn advance(
        &mut self,
        now: Duration,
        identity: &I,
        event: WatchEvent,
    ) -> Result<WatchUpdate, WatchError> {
        // Reject old owners, commands and contexts before touching any clock/state.
        self.check_identity(identity)?;
        if matches!(
            event,
            WatchEvent::BeginTrustedCommand | WatchEvent::AttachRunningObservation
        ) {
            if self.phase != Phase::Prepared {
                return Err(WatchError::IdentityAlreadyStarted);
            }
            if now < self.clock {
                return Err(WatchError::StartTimeRegressed);
            }
            self.clock = now;
            self.phase = Phase::Active;
            self.baseline = Some(now);
            self.first_pending = event == WatchEvent::AttachRunningObservation;
            return Ok(self.finish(WatchOutcome::Started, true));
        }
        if event == WatchEvent::Retire && self.phase != Phase::Retired {
            // A gate may close after binding but before a queued start arrives.
            self.clock = now.max(self.clock);
            self.clear_observation();
            return Ok(self.finish(WatchOutcome::Retired, true));
        }
        if self.phase != Phase::Active {
            return Err(WatchError::NotObserving);
        }

        if let WatchEvent::OutputObserved { batch_seq, .. } = event {
            if self.last_batch.is_some_and(|last| batch_seq <= last) {
                return Err(WatchError::StaleBatch);
            }
        }
        if let WatchEvent::DeadlineDue { deadline } = event {
            if self.deadline != Some(deadline) {
                return Err(WatchError::StaleDeadline);
            }
            if now < deadline.at {
                return Err(WatchError::DeadlineNotDue);
            }
        }

        let backwards = now < self.clock;
        if let WatchEvent::OutputObserved {
            batch_seq,
            disposition,
        } = event
        {
            // A fresh sequence is consumed even if its timestamp regresses. It may
            // not be replayed later at a more convenient time to create a pulse.
            self.last_batch = Some(batch_seq);
            if backwards && disposition != WatchOutputDisposition::Quarantined {
                return Ok(WatchUpdate {
                    outcome: WatchOutcome::BackwardsBatchConsumed,
                    snapshot: self.sample(self.clock),
                });
            }
        }
        self.clock = now.max(self.clock);
        if self.resumed_until.is_some_and(|until| until <= self.clock) {
            self.resumed_until = None;
        }

        let (outcome, replace_plan) = match event {
            WatchEvent::OutputObserved {
                disposition: WatchOutputDisposition::Quarantined,
                ..
            } => {
                self.reset_baseline(true);
                (WatchOutcome::Quarantined, true)
            }
            WatchEvent::OutputObserved { disposition, .. } => {
                if self.first_pending || disposition == WatchOutputDisposition::BaselineOnly {
                    self.reset_baseline(false);
                    (WatchOutcome::BaselineOnly, true)
                } else {
                    let was_waiting = self
                        .quiet_since()
                        .is_some_and(|last| self.clock.saturating_sub(last) >= QUIET_AFTER);
                    self.outputs = [self.outputs[1], self.outputs[2], Some(self.clock)];
                    if was_waiting {
                        let until = self.clock.saturating_add(RESUMED_HOLD);
                        self.resumed_until = (until > self.clock).then_some(until);
                    }
                    (WatchOutcome::Counted, true)
                }
            }
            WatchEvent::Advance | WatchEvent::DeadlineDue { .. } => (WatchOutcome::Advanced, false),
            WatchEvent::BeginTrustedCommand
            | WatchEvent::AttachRunningObservation
            | WatchEvent::Retire => unreachable!(),
        };
        Ok(self.finish(outcome, replace_plan))
    }

    fn check_identity(&self, identity: &I) -> Result<(), WatchError> {
        if self.identity.as_ref() == Some(identity) {
            Ok(())
        } else {
            Err(WatchError::StaleIdentity)
        }
    }

    fn clear_observation(&mut self) {
        self.phase = Phase::Retired;
        self.baseline = None;
        self.outputs = [None; 3];
        self.first_pending = false;
        self.resumed_until = None;
        self.deadline = None;
    }

    fn reset_baseline(&mut self, first_pending: bool) {
        self.baseline = Some(self.clock);
        self.outputs = [None; 3];
        self.resumed_until = None;
        self.first_pending = first_pending;
    }

    fn quiet_since(&self) -> Option<Duration> {
        self.outputs[2].or(self.baseline)
    }

    fn rhythm(&self, now: Duration) -> WatchRhythm {
        if self.phase != Phase::Active {
            WatchRhythm::Steady
        } else if self.resumed_until.is_some_and(|until| now < until) {
            WatchRhythm::Resumed
        } else if self
            .quiet_since()
            .is_some_and(|last| now.saturating_sub(last) >= QUIET_AFTER)
        {
            WatchRhythm::Waiting
        } else if self.outputs[0].is_some_and(|oldest| now.saturating_sub(oldest) <= BUSY_WINDOW) {
            WatchRhythm::Busy
        } else {
            WatchRhythm::Steady
        }
    }

    fn sample(&self, now: Duration) -> WatchSnapshot {
        WatchSnapshot {
            rhythm: self.rhythm(now),
            next_deadline: self.deadline,
        }
    }

    fn next_transition(&self) -> Option<Duration> {
        if self.phase != Phase::Active {
            return None;
        }
        let next = match self.rhythm(self.clock) {
            WatchRhythm::Resumed => self.resumed_until,
            WatchRhythm::Waiting => None,
            // Busy includes exact 1200ms. Its next transition is one nanosecond
            // later, never the already-due inclusive endpoint.
            WatchRhythm::Busy => self.outputs[0]
                .and_then(|oldest| oldest.checked_add(BUSY_WINDOW))
                .and_then(|end| end.checked_add(Duration::from_nanos(1))),
            WatchRhythm::Steady => self
                .quiet_since()
                .and_then(|last| last.checked_add(QUIET_AFTER)),
        };
        next.filter(|deadline| *deadline > self.clock)
    }

    fn finish(&mut self, mut outcome: WatchOutcome, replace_plan: bool) -> WatchUpdate {
        let next = self.next_transition();
        if !replace_plan && self.deadline.map(|deadline| deadline.at) == next {
            // Ordinary host ticks before a transition must not churn native plans.
        } else if let Some(at) = next {
            if let Some(revision) = self.next_revision {
                self.deadline = Some(WatchDeadline { at, revision });
                self.next_revision = revision.checked_add(1);
            } else {
                self.clear_observation();
                outcome = WatchOutcome::RetiredOnDeadlineExhaustion;
            }
        } else {
            self.deadline = None;
        }
        WatchUpdate {
            outcome,
            snapshot: self.sample(self.clock),
        }
    }
}

#[cfg(test)]
#[path = "organism_watch/tests.rs"]
mod tests;
