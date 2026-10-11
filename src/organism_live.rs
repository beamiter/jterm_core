//! One window's live organism attention, independent of its native renderer.
//!
//! A window owns one [`LiveAttention`] across all panes. Its owner coordinator
//! calls [`LiveAttention::set_owner`]; individual pane callbacks are always
//! owner-scoped. Native hosts supply authoritative context, visibility, input
//! and geometry facts, and translate the returned absolute deadlines into their
//! own timers. This module has no clock, I/O, toolkit or terminal dependency.
//!
//! Position synchronization is not an entry event. Only a genuine native
//! `PointerEntered` starts a visit, and only its matching `PointerLeft` releases
//! that visit. Cancellation and refused entries never queue another greeting.

use std::time::Duration;

use crate::organism::RenderContext;
use crate::organism_daily::GentleInteraction;

/// Opaque identity of one physical pointer visit, never reused by a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LiveAttentionTicket(u64);

/// Native facts for the current owner. Never feed back a painted greeting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveAttentionInput {
    pub base_context: RenderContext,
    /// Enabled, visible/mapped, non-Static, locally settled and input-safe.
    /// The host also checks focus, remote/alternate-screen and held buttons.
    pub hard_eligible: bool,
    /// An actual hover target exists. A body without a glyph may still allow
    /// an explicit greeting, so this is separate from `hard_eligible`.
    pub hover_available: bool,
    /// Current geometry only; changes cannot manufacture an entry or exit.
    pub pointer_inside: bool,
}

/// One native event. Same-time events are processed in caller order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveAttentionEvent {
    /// Recheck facts, expire a hold, and consume a due candidate if present.
    Reconcile,
    PointerEntered {
        ticket: LiveAttentionTicket,
    },
    PointerLeft {
        ticket: LiveAttentionTicket,
    },
    Explicit,
    CandidateDue {
        ticket: LiveAttentionTicket,
    },
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveAttentionSource {
    Pointer,
    Explicit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveAttentionError {
    StaleOwner,
    StaleTicket,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveAttentionRejection {
    Unavailable,
    BusyContext,
    Cooldown,
    PointerUnavailable,
    EntryConsumed,
    ClockExhausted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveAttentionOutcome {
    None,
    CandidateArmed,
    Accepted(LiveAttentionSource),
    Rejected(LiveAttentionRejection),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveAttentionCandidate {
    pub ticket: LiveAttentionTicket,
    pub deadline: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveAttentionActive {
    pub source: LiveAttentionSource,
    pub deadline: Duration,
}

/// Immutable output of one observation, safe to retain through painting.
/// Mutating a copied snapshot cannot change the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveAttentionSnapshot {
    pub observed_at: Duration,
    pub entry: Option<LiveAttentionTicket>,
    pub entry_offer: Option<LiveAttentionTicket>,
    pub candidate: Option<LiveAttentionCandidate>,
    /// Whether the pending dwell may show passive hover feedback right now.
    pub candidate_visible: bool,
    pub active: Option<LiveAttentionActive>,
    pub presentation: RenderContext,
    pub outcome: LiveAttentionOutcome,
}

impl LiveAttentionSnapshot {
    /// Earliest required state transition, including a candidate already due.
    pub fn next_deadline(self) -> Option<Duration> {
        match (self.candidate, self.active) {
            (Some(candidate), Some(active)) => Some(candidate.deadline.min(active.deadline)),
            (Some(candidate), None) => Some(candidate.deadline),
            (None, Some(active)) => Some(active.deadline),
            (None, None) => None,
        }
    }

    /// Remaining time from this snapshot's original deadline. If painting
    /// crosses the endpoint this returns zero; schedule one native minimum
    /// delay, then reconcile. The next snapshot contains no expired hold.
    pub fn wake_after(self, now: Duration) -> Option<Duration> {
        self.next_deadline()
            .map(|deadline| deadline.saturating_sub(now.max(self.observed_at)))
    }
}

/// Window-local live interaction policy. Preview interactions stay separate.
///
/// Owner tokens must be stable, non-recycled identities for live panes, not
/// widget addresses. Only owner changes clone a token. Hold and cooldown
/// remain exclusively in [`GentleInteraction`]; no independent timestamp
/// ledger is maintained here or required in a host adapter.
#[derive(Debug, Clone)]
pub struct LiveAttention<Owner> {
    owner: Option<Owner>,
    interaction: GentleInteraction,
    entry: Option<LiveAttentionTicket>,
    candidate: Option<LiveAttentionCandidate>,
    source: Option<LiveAttentionSource>,
    entry_offer: Option<LiveAttentionTicket>,
    next_ticket: Option<u64>,
    last_hard_eligible: bool,
    last_hover_available: bool,
    observed_at: Duration,
}

impl<Owner> Default for LiveAttention<Owner> {
    fn default() -> Self {
        Self {
            owner: None,
            interaction: GentleInteraction::default(),
            entry: None,
            candidate: None,
            source: None,
            entry_offer: Some(LiveAttentionTicket(0)),
            next_ticket: Some(1),
            last_hard_eligible: false,
            last_hover_available: false,
            observed_at: Duration::ZERO,
        }
    }
}

impl<Owner: Eq + Clone> LiveAttention<Owner> {
    pub const DWELL: Duration = Duration::from_millis(600);

    pub fn owner(&self) -> Option<&Owner> {
        self.owner.as_ref()
    }

    pub fn entry_ticket(&self) -> Option<LiveAttentionTicket> {
        self.entry
    }

    /// Pre-issued admission ticket for native Enter/Exit closures. Enter
    /// consumes this exact ticket into the physical visit, so an Exit queued
    /// before a view rebuild can already carry the right visit identity.
    pub fn entry_offer(&self) -> Option<LiveAttentionTicket> {
        self.entry_offer
    }

    /// Set by the window's owner coordinator, never by an arbitrary pane's
    /// refresh/Drop callback. Rebinding the same owner is a no-op. Losing or
    /// changing it retires feedback and the visit, but keeps window cooldown.
    pub fn set_owner(&mut self, owner: Option<Owner>) -> bool {
        if self.owner == owner {
            return false;
        }
        self.clear_feedback();
        self.entry = None;
        self.retire_offer();
        self.last_hard_eligible = false;
        self.last_hover_available = false;
        self.owner = owner;
        true
    }

    /// Immediately cancel current feedback without releasing the physical
    /// visit or resetting cooldown. A retired pane cannot cancel its successor.
    pub fn cancel(&mut self, owner: &Owner) -> bool {
        if self.owner.as_ref() != Some(owner) {
            return false;
        }
        self.clear_feedback();
        self.retire_offer();
        true
    }

    /// Retire hover feedback when its native target disappears. Explicit
    /// greetings and the consumed physical entry survive this operation.
    pub fn cancel_hover(&mut self, owner: &Owner) -> bool {
        if self.owner.as_ref() != Some(owner) {
            return false;
        }
        self.clear_hover();
        self.retire_offer();
        true
    }

    /// Process one current-owner event and return its complete presentation.
    /// Stale owner/ticket errors do not even advance the observation clock.
    /// Other timestamps clamp to the greatest previously observed value.
    ///
    /// Only `Reconcile` and a matching `CandidateDue` can accept a pending
    /// pointer greeting. Explicit requests, leave and cancellation take effect
    /// before any due candidate. Refused events are never saved for replay.
    pub fn advance(
        &mut self,
        now: Duration,
        owner: &Owner,
        input: LiveAttentionInput,
        event: LiveAttentionEvent,
    ) -> Result<LiveAttentionSnapshot, LiveAttentionError> {
        if self.owner.as_ref() != Some(owner) {
            return Err(LiveAttentionError::StaleOwner);
        }
        match event {
            LiveAttentionEvent::PointerEntered { ticket } if self.entry_offer != Some(ticket) => {
                return Err(LiveAttentionError::StaleTicket);
            }
            LiveAttentionEvent::PointerLeft { ticket } if self.entry != Some(ticket) => {
                return Err(LiveAttentionError::StaleTicket);
            }
            LiveAttentionEvent::CandidateDue { ticket }
                if self.candidate.map(|candidate| candidate.ticket) != Some(ticket) =>
            {
                return Err(LiveAttentionError::StaleTicket);
            }
            _ => {}
        }

        self.observed_at = self.observed_at.max(now);
        let now = self.observed_at;
        let hard_eligible = self.admission_block(input).is_none();
        if (self.last_hard_eligible && !hard_eligible)
            || (self.last_hover_available && !input.hover_available)
        {
            self.retire_offer();
        }
        self.last_hard_eligible = hard_eligible;
        self.last_hover_available = input.hover_available;
        if !hard_eligible {
            self.clear_feedback();
        } else if !input.hover_available || !input.pointer_inside {
            self.clear_hover();
        }
        // Expiration and semantic cancellation use the same original hold.
        self.interaction.apply(now, input.base_context);
        if self.interaction.active_deadline(now).is_none() {
            self.source = None;
        }

        let outcome = match event {
            LiveAttentionEvent::Cancel => {
                self.clear_feedback();
                self.retire_offer();
                LiveAttentionOutcome::None
            }
            LiveAttentionEvent::PointerLeft { .. } => {
                self.clear_hover();
                self.entry = None;
                self.retire_offer();
                LiveAttentionOutcome::None
            }
            LiveAttentionEvent::Explicit => {
                self.candidate = None;
                self.request(now, input, LiveAttentionSource::Explicit)
            }
            LiveAttentionEvent::PointerEntered { ticket } => self.enter(now, input, ticket),
            LiveAttentionEvent::Reconcile | LiveAttentionEvent::CandidateDue { .. } => {
                if self
                    .candidate
                    .is_some_and(|candidate| now >= candidate.deadline)
                {
                    self.candidate = None;
                    self.request(now, input, LiveAttentionSource::Pointer)
                } else {
                    LiveAttentionOutcome::None
                }
            }
        };
        let mut snapshot = self.snapshot(now, owner, input)?;
        snapshot.outcome = outcome;
        Ok(snapshot)
    }

    /// Read presentation and scheduling state without admitting a candidate,
    /// observing time, cancelling feedback, or changing a pointer visit.
    /// This supports immutable native view/subscription passes. Native changes
    /// must also reach `advance`/`cancel`; a read is not a cancellation event.
    /// Due candidates remain visible to scheduling and are consumed only by
    /// a subsequent `Reconcile` or matching `CandidateDue`.
    pub fn snapshot(
        &self,
        now: Duration,
        owner: &Owner,
        input: LiveAttentionInput,
    ) -> Result<LiveAttentionSnapshot, LiveAttentionError> {
        if self.owner.as_ref() != Some(owner) {
            return Err(LiveAttentionError::StaleOwner);
        }
        let now = now.max(self.observed_at);
        let hard_eligible = self.admission_block(input).is_none();
        let pointer_available = input.hover_available && input.pointer_inside;
        let candidate = self.candidate;
        let candidate_visible = candidate.is_some_and(|candidate| {
            now < candidate.deadline
                && hard_eligible
                && pointer_available
                && self.interaction.can_request(now, input.base_context)
        });
        let active = self
            .source
            .filter(|source| {
                hard_eligible && (*source == LiveAttentionSource::Explicit || pointer_available)
            })
            .zip(self.interaction.active_deadline(now))
            .map(|(source, deadline)| LiveAttentionActive { source, deadline });
        let presentation = if active.is_some() {
            self.interaction.presentation(now, input.base_context)
        } else {
            input.base_context
        };
        Ok(LiveAttentionSnapshot {
            observed_at: now,
            entry: self.entry,
            entry_offer: self.entry_offer,
            candidate,
            candidate_visible,
            active,
            presentation,
            outcome: LiveAttentionOutcome::None,
        })
    }

    fn admission_block(&self, input: LiveAttentionInput) -> Option<LiveAttentionRejection> {
        if !input.hard_eligible {
            Some(LiveAttentionRejection::Unavailable)
        } else if !GentleInteraction::eligible(input.base_context) {
            Some(LiveAttentionRejection::BusyContext)
        } else {
            None
        }
    }

    fn enter(
        &mut self,
        now: Duration,
        input: LiveAttentionInput,
        ticket: LiveAttentionTicket,
    ) -> LiveAttentionOutcome {
        use LiveAttentionRejection as Rejection;
        if self.entry.is_some() {
            return LiveAttentionOutcome::Rejected(Rejection::EntryConsumed);
        }
        self.retire_offer();
        self.entry = Some(ticket);
        if let Some(reason) = self.admission_block(input) {
            return LiveAttentionOutcome::Rejected(reason);
        }
        if !input.hover_available || !input.pointer_inside {
            return LiveAttentionOutcome::Rejected(Rejection::PointerUnavailable);
        }
        if !self.interaction.can_request(now, input.base_context) {
            return LiveAttentionOutcome::Rejected(Rejection::Cooldown);
        }
        let Some(deadline) = now.checked_add(Self::DWELL) else {
            return LiveAttentionOutcome::Rejected(Rejection::ClockExhausted);
        };
        if deadline.checked_add(GentleInteraction::HOLD).is_none() {
            return LiveAttentionOutcome::Rejected(Rejection::ClockExhausted);
        }
        self.candidate = Some(LiveAttentionCandidate { ticket, deadline });
        LiveAttentionOutcome::CandidateArmed
    }

    fn request(
        &mut self,
        now: Duration,
        input: LiveAttentionInput,
        source: LiveAttentionSource,
    ) -> LiveAttentionOutcome {
        use LiveAttentionRejection as Rejection;
        if let Some(reason) = self.admission_block(input) {
            return LiveAttentionOutcome::Rejected(reason);
        }
        if source == LiveAttentionSource::Pointer
            && (!input.hover_available || !input.pointer_inside || self.entry.is_none())
        {
            return LiveAttentionOutcome::Rejected(Rejection::PointerUnavailable);
        }
        if now.checked_add(GentleInteraction::HOLD).is_none() {
            return LiveAttentionOutcome::Rejected(Rejection::ClockExhausted);
        }
        if !self.interaction.request(now, input.base_context) {
            return LiveAttentionOutcome::Rejected(Rejection::Cooldown);
        }
        self.source = Some(source);
        self.retire_offer();
        LiveAttentionOutcome::Accepted(source)
    }

    fn retire_offer(&mut self) {
        self.entry_offer = self.next_ticket.map(LiveAttentionTicket);
        self.next_ticket = self.next_ticket.and_then(|serial| serial.checked_add(1));
    }

    fn clear_feedback(&mut self) {
        self.candidate = None;
        self.source = None;
        self.interaction.cancel();
    }

    fn clear_hover(&mut self) {
        self.candidate = None;
        if self.source == Some(LiveAttentionSource::Pointer) {
            self.source = None;
            self.interaction.cancel();
        }
    }
}

#[cfg(test)]
#[path = "organism_live/tests.rs"]
mod tests;
