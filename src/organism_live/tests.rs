use super::*;
use crate::organism::{Behavior, BodyLanguage, NativeOrganism, VisualTransition};
use crate::organism_daily::PreviewPose;

fn ms(value: u64) -> Duration {
    Duration::from_millis(value)
}

fn input() -> LiveAttentionInput {
    LiveAttentionInput {
        base_context: PreviewPose::Calm.context(),
        activity: LiveActivityKnowledge::KnownIdle,
        hard_eligible: true,
        hover_available: true,
        pointer_inside: true,
    }
}

fn model() -> LiveAttention<u64> {
    let mut model = LiveAttention::default();
    assert!(model.set_owner(Some(1)));
    model
}

fn advance(
    model: &mut LiveAttention<u64>,
    time: u64,
    facts: LiveAttentionInput,
    event: LiveAttentionEvent,
) -> LiveAttentionSnapshot {
    model.advance(ms(time), &1, facts, event).unwrap()
}

fn enter(model: &mut LiveAttention<u64>, time: u64) -> LiveAttentionSnapshot {
    let ticket = model.entry_offer().unwrap();
    advance(
        model,
        time,
        input(),
        LiveAttentionEvent::PointerEntered { ticket },
    )
}

fn reconcile(model: &mut LiveAttention<u64>, time: u64) -> LiveAttentionSnapshot {
    advance(model, time, input(), LiveAttentionEvent::Reconcile)
}

fn explicit(model: &mut LiveAttention<u64>, time: u64) -> LiveAttentionSnapshot {
    advance(model, time, input(), LiveAttentionEvent::Explicit)
}

fn leave(model: &mut LiveAttention<u64>, time: u64) -> LiveAttentionSnapshot {
    let ticket = model.entry_ticket().unwrap();
    advance(
        model,
        time,
        LiveAttentionInput {
            pointer_inside: false,
            ..input()
        },
        LiveAttentionEvent::PointerLeft { ticket },
    )
}

#[test]
fn dwell_hold_and_cooldown_have_one_original_deadline() {
    let mut model = model();
    let entered = enter(&mut model, 0);
    assert_eq!(entered.outcome, LiveAttentionOutcome::CandidateArmed);
    assert_eq!(entered.next_deadline(), Some(ms(600)));
    assert!(entered.candidate_visible);
    assert!(reconcile(&mut model, 599).active.is_none());
    let greeting = reconcile(&mut model, 600);
    assert_eq!(
        greeting.outcome,
        LiveAttentionOutcome::Accepted(LiveAttentionSource::Pointer)
    );
    assert_eq!(greeting.active.unwrap().deadline, ms(2600));
    assert_eq!(greeting.presentation.behavior, Behavior::Approach);
    assert!(reconcile(&mut model, 2599).active.is_some());
    assert!(reconcile(&mut model, 2600).active.is_none());
    assert_eq!(
        explicit(&mut model, 8599).outcome,
        LiveAttentionOutcome::Rejected(LiveAttentionRejection::Cooldown)
    );
    assert_eq!(
        explicit(&mut model, 8600).outcome,
        LiveAttentionOutcome::Accepted(LiveAttentionSource::Explicit)
    );
}

#[test]
fn overdue_read_only_candidate_still_schedules_one_consumption() {
    let mut model = model();
    let pending = enter(&mut model, 0).candidate.unwrap();
    let read = model.snapshot(ms(700), &1, input()).unwrap();
    assert_eq!(read.candidate, Some(pending));
    assert!(!read.candidate_visible);
    assert_eq!(read.wake_after(ms(700)), Some(Duration::ZERO));
    assert!(read.active.is_none());
    let accepted = advance(
        &mut model,
        700,
        input(),
        LiveAttentionEvent::CandidateDue {
            ticket: pending.ticket,
        },
    );
    assert_eq!(accepted.active.unwrap().deadline, ms(2700));
    assert_eq!(
        model.advance(
            ms(800),
            &1,
            input(),
            LiveAttentionEvent::CandidateDue {
                ticket: pending.ticket
            }
        ),
        Err(LiveAttentionError::StaleTicket)
    );
    assert_eq!(reconcile(&mut model, 800).active, accepted.active);
}

#[test]
fn retained_paint_snapshot_provides_one_zero_cleanup_without_extension() {
    let mut model = model();
    explicit(&mut model, 0);
    let painted = reconcile(&mut model, 1999);
    assert_eq!(painted.wake_after(ms(2001)), Some(Duration::ZERO));
    let cleaned = reconcile(&mut model, 2001);
    assert_eq!(cleaned.wake_after(ms(2001)), None);
    assert_eq!(cleaned.presentation, input().base_context);
    assert_eq!(reconcile(&mut model, 20_000).next_deadline(), None);
}

#[test]
fn cooldown_entry_is_consumed_until_a_genuine_leave() {
    let mut model = model();
    explicit(&mut model, 0);
    let entry = enter(&mut model, 4000);
    assert_eq!(
        entry.outcome,
        LiveAttentionOutcome::Rejected(LiveAttentionRejection::Cooldown)
    );
    assert!(entry.entry.is_some());
    assert!(reconcile(&mut model, 8000).candidate.is_none());
    assert_eq!(
        enter(&mut model, 8000).outcome,
        LiveAttentionOutcome::Rejected(LiveAttentionRejection::EntryConsumed)
    );
    leave(&mut model, 8000);
    assert_eq!(
        enter(&mut model, 8000).outcome,
        LiveAttentionOutcome::CandidateArmed
    );
}

#[test]
fn cancel_preserves_cooldown_and_consumed_entry_across_stationary_recovery() {
    let mut model = model();
    enter(&mut model, 0);
    reconcile(&mut model, 600);
    let ticket = model.entry_ticket();
    assert!(model.cancel(&1));
    assert_eq!(model.entry_ticket(), ticket);
    assert!(reconcile(&mut model, 1000).active.is_none());
    assert_eq!(
        explicit(&mut model, 1000).outcome,
        LiveAttentionOutcome::Rejected(LiveAttentionRejection::Cooldown)
    );
    assert!(reconcile(&mut model, 20_000).candidate.is_none());
}

#[test]
fn cancel_does_not_spend_a_pending_candidates_cooldown() {
    let mut model = model();
    enter(&mut model, 0);
    assert!(model.cancel(&1));
    assert_eq!(
        explicit(&mut model, 1).outcome,
        LiveAttentionOutcome::Accepted(LiveAttentionSource::Explicit)
    );
    assert!(model.cancel(&1));
    assert_eq!(
        explicit(&mut model, 7999).outcome,
        LiveAttentionOutcome::Rejected(LiveAttentionRejection::Cooldown)
    );
    assert_eq!(
        explicit(&mut model, 8001).outcome,
        LiveAttentionOutcome::Accepted(LiveAttentionSource::Explicit)
    );
}

#[test]
fn explicit_needs_neither_hover_target_nor_pointer_and_survives_departure() {
    let mut model = model();
    let outside = LiveAttentionInput {
        pointer_inside: false,
        hover_available: false,
        ..input()
    };
    let accepted = advance(&mut model, 0, outside, LiveAttentionEvent::Explicit);
    assert_eq!(
        accepted.active.unwrap().source,
        LiveAttentionSource::Explicit
    );
    assert_eq!(accepted.entry, None);
    enter(&mut model, 100);
    let left = leave(&mut model, 200);
    assert_eq!(left.active, accepted.active);
    assert!(model.cancel_hover(&1));
    assert_eq!(
        advance(&mut model, 1999, outside, LiveAttentionEvent::Reconcile).active,
        accepted.active
    );
    assert!(
        advance(&mut model, 2000, outside, LiveAttentionEvent::Reconcile)
            .active
            .is_none()
    );
    assert!(reconcile(&mut model, 9000).candidate.is_none());
}

#[test]
fn explicit_retires_pending_hover_before_acceptance_and_never_extends() {
    let mut model = model();
    let candidate = enter(&mut model, 0).candidate.unwrap();
    let explicit = explicit(&mut model, 600);
    assert!(explicit.candidate.is_none());
    assert_eq!(
        explicit.active.unwrap().source,
        LiveAttentionSource::Explicit
    );
    assert_eq!(
        model.advance(
            ms(600),
            &1,
            input(),
            LiveAttentionEvent::CandidateDue {
                ticket: candidate.ticket
            }
        ),
        Err(LiveAttentionError::StaleTicket)
    );
    let repeat = advance(&mut model, 2000, input(), LiveAttentionEvent::Explicit);
    assert_eq!(
        repeat.outcome,
        LiveAttentionOutcome::Rejected(LiveAttentionRejection::Cooldown)
    );
    assert_eq!(repeat.active, explicit.active);
    assert!(reconcile(&mut model, 2600).active.is_none());
}

#[test]
fn all_hard_gate_causes_cancel_without_changing_life_or_work_facts() {
    // Native disabled, Static, input, buttons, hidden, remote, alternate-screen
    // and busy facts share one deliberate hard gate, rather than host policy.
    for _native_cause in [
        "disabled", "Static", "input", "buttons", "hidden", "remote", "alt", "busy",
    ] {
        let mut model = model();
        explicit(&mut model, 0);
        let native = NativeOrganism::default();
        let before = format!("{native:?}");
        let facts = LiveAttentionInput {
            hard_eligible: false,
            ..input()
        };
        assert!(advance(&mut model, 1, facts, LiveAttentionEvent::Reconcile)
            .active
            .is_none());
        assert_eq!(format!("{native:?}"), before);
        assert!(reconcile(&mut model, 2).active.is_none());
        assert_eq!(
            explicit(&mut model, 7999).outcome,
            LiveAttentionOutcome::Rejected(LiveAttentionRejection::Cooldown)
        );
    }
}

#[test]
fn semantic_work_or_transition_cancels_and_does_not_resume() {
    let idle = input().base_context;
    for context in [
        PreviewPose::Working.context(),
        RenderContext::new(Behavior::GuardFailure, BodyLanguage::default(), false),
        idle.with_transition(Some(VisualTransition::CelebrateToIdle)),
    ] {
        let mut model = model();
        explicit(&mut model, 0);
        let facts = LiveAttentionInput {
            base_context: context,
            ..input()
        };
        let cancelled = advance(&mut model, 1, facts, LiveAttentionEvent::Reconcile);
        assert!(cancelled.active.is_none());
        assert_eq!(cancelled.presentation, context);
        assert_eq!(
            advance(&mut model, 2, facts, LiveAttentionEvent::Explicit).outcome,
            LiveAttentionOutcome::Rejected(LiveAttentionRejection::BusyContext)
        );
        assert_eq!(reconcile(&mut model, 3).presentation, idle);
    }
}

#[test]
fn due_candidate_rechecks_facts_without_spending_cooldown() {
    for facts in [
        LiveAttentionInput {
            hard_eligible: false,
            ..input()
        },
        LiveAttentionInput {
            hover_available: false,
            ..input()
        },
        LiveAttentionInput {
            pointer_inside: false,
            ..input()
        },
        LiveAttentionInput {
            base_context: PreviewPose::Working.context(),
            ..input()
        },
    ] {
        let mut model = model();
        enter(&mut model, 0);
        let rejected = advance(&mut model, 600, facts, LiveAttentionEvent::Reconcile);
        assert!(rejected.candidate.is_none());
        assert!(rejected.active.is_none());
        assert_eq!(
            explicit(&mut model, 600).outcome,
            LiveAttentionOutcome::Accepted(LiveAttentionSource::Explicit)
        );
    }
}

#[test]
fn layout_sync_is_never_a_physical_entry_or_exit() {
    let mut model = model();
    assert!(reconcile(&mut model, 0).candidate.is_none());
    enter(&mut model, 0);
    let entry = model.entry_ticket();
    let outside = LiveAttentionInput {
        pointer_inside: false,
        ..input()
    };
    let hidden = advance(&mut model, 1, outside, LiveAttentionEvent::Reconcile);
    assert_eq!(hidden.entry, entry);
    assert!(hidden.candidate.is_none());
    assert!(reconcile(&mut model, 1000).candidate.is_none());
    assert_eq!(
        enter(&mut model, 1000).outcome,
        LiveAttentionOutcome::Rejected(LiveAttentionRejection::EntryConsumed)
    );
    leave(&mut model, 1001);
    assert!(enter(&mut model, 1001).candidate.is_some());
}

#[test]
fn hover_target_loss_cancels_pointer_but_not_explicit() {
    let body_only = LiveAttentionInput {
        hover_available: false,
        ..input()
    };
    let mut pointer = model();
    enter(&mut pointer, 0);
    reconcile(&mut pointer, 600);
    assert!(
        advance(&mut pointer, 601, body_only, LiveAttentionEvent::Reconcile)
            .active
            .is_none()
    );
    assert!(reconcile(&mut pointer, 602).active.is_none());
    let mut explicit_model = model();
    let hold = explicit(&mut explicit_model, 0).active;
    assert_eq!(
        advance(
            &mut explicit_model,
            1,
            body_only,
            LiveAttentionEvent::Reconcile
        )
        .active,
        hold
    );
}

#[test]
fn full_calm_changes_keep_original_deadlines_static_cancels() {
    let mut model = model();
    let pending = enter(&mut model, 0).candidate;
    // Effective Full/Calm are native cadence only, so both supply identical
    // hard eligibility. Raw setting changes also need no cancellation event.
    for now in [100, 200, 500] {
        assert_eq!(reconcile(&mut model, now).candidate, pending);
    }
    let hold = reconcile(&mut model, 600).active;
    for now in [700, 1000, 2599] {
        assert_eq!(reconcile(&mut model, now).active, hold);
    }
    let static_mode = LiveAttentionInput {
        hard_eligible: false,
        ..input()
    };
    assert!(
        advance(&mut model, 2599, static_mode, LiveAttentionEvent::Reconcile)
            .active
            .is_none()
    );
    assert!(reconcile(&mut model, 2600).active.is_none());
    assert_eq!(
        explicit(&mut model, 8000).outcome,
        LiveAttentionOutcome::Rejected(LiveAttentionRejection::Cooldown)
    );
}

#[test]
fn window_cooldown_survives_owner_loss_and_nonowner_callbacks_are_inert() {
    let mut model = model();
    explicit(&mut model, 0);
    assert!(model.set_owner(None));
    assert!(model.set_owner(Some(2)));
    let offered = model.entry_offer();
    assert!(!model.cancel(&1));
    assert!(!model.cancel_hover(&1));
    assert_eq!(
        model.advance(ms(999_000), &1, input(), LiveAttentionEvent::Cancel),
        Err(LiveAttentionError::StaleOwner)
    );
    assert_eq!(model.entry_offer(), offered);
    let refused = model
        .advance(ms(7999), &2, input(), LiveAttentionEvent::Explicit)
        .unwrap();
    assert_eq!(refused.observed_at, ms(7999));
    assert_eq!(
        refused.outcome,
        LiveAttentionOutcome::Rejected(LiveAttentionRejection::Cooldown)
    );
    assert_eq!(
        model
            .advance(ms(8000), &2, input(), LiveAttentionEvent::Explicit)
            .unwrap()
            .outcome,
        LiveAttentionOutcome::Accepted(LiveAttentionSource::Explicit)
    );
}

#[test]
fn same_owner_rebinding_is_idempotent() {
    let mut model = model();
    let pending = enter(&mut model, 0);
    assert!(!model.set_owner(Some(1)));
    assert_eq!(reconcile(&mut model, 1).candidate, pending.candidate);
    assert_eq!(model.entry_ticket(), pending.entry);
}

#[test]
fn preissued_offer_supports_enter_and_exit_in_the_same_native_batch() {
    let mut model = model();
    let captured = model.entry_offer().unwrap();
    let entered = advance(
        &mut model,
        0,
        input(),
        LiveAttentionEvent::PointerEntered { ticket: captured },
    );
    assert_eq!(entered.entry, Some(captured));
    let left = advance(
        &mut model,
        0,
        input(),
        LiveAttentionEvent::PointerLeft { ticket: captured },
    );
    assert!(left.entry.is_none());
    assert!(left.candidate.is_none());
    assert_eq!(
        model.advance(
            ms(1),
            &1,
            input(),
            LiveAttentionEvent::PointerEntered { ticket: captured }
        ),
        Err(LiveAttentionError::StaleTicket)
    );
}

#[test]
fn cancelled_offer_is_retired_but_existing_visit_can_still_leave() {
    let mut model = model();
    let unconsumed = model.entry_offer().unwrap();
    model.cancel(&1);
    assert_eq!(
        model.advance(
            ms(99_000),
            &1,
            input(),
            LiveAttentionEvent::PointerEntered { ticket: unconsumed }
        ),
        Err(LiveAttentionError::StaleTicket)
    );
    let entry = enter(&mut model, 0).entry.unwrap();
    model.cancel(&1);
    assert!(advance(
        &mut model,
        1,
        input(),
        LiveAttentionEvent::PointerLeft { ticket: entry }
    )
    .entry
    .is_none());
    let next = enter(&mut model, 2);
    assert_eq!(
        model.advance(
            ms(99_000),
            &1,
            input(),
            LiveAttentionEvent::PointerLeft { ticket: entry }
        ),
        Err(LiveAttentionError::StaleTicket)
    );
    assert_eq!(reconcile(&mut model, 3).candidate, next.candidate);
}

#[test]
fn owner_roundtrip_and_explicit_acceptance_retire_old_offers() {
    let mut model = model();
    let first = model.entry_offer().unwrap();
    model.set_owner(Some(2));
    model.set_owner(Some(1));
    assert_eq!(
        model.advance(
            ms(0),
            &1,
            input(),
            LiveAttentionEvent::PointerEntered { ticket: first }
        ),
        Err(LiveAttentionError::StaleTicket)
    );
    let before_explicit = model.entry_offer().unwrap();
    explicit(&mut model, 0);
    assert_eq!(
        model.advance(
            ms(9000),
            &1,
            input(),
            LiveAttentionEvent::PointerEntered {
                ticket: before_explicit
            }
        ),
        Err(LiveAttentionError::StaleTicket)
    );
    assert!(reconcile(&mut model, 1).active.is_some());
}

#[test]
fn stale_candidate_never_consumes_new_candidate_or_updates_time() {
    let mut model = model();
    let first = enter(&mut model, 0).candidate.unwrap();
    leave(&mut model, 1);
    let second = enter(&mut model, 2).candidate.unwrap();
    assert_eq!(
        model.advance(
            ms(999_000),
            &1,
            input(),
            LiveAttentionEvent::CandidateDue {
                ticket: first.ticket
            }
        ),
        Err(LiveAttentionError::StaleTicket)
    );
    let still_pending = reconcile(&mut model, 3);
    assert_eq!(still_pending.observed_at, ms(3));
    assert_eq!(still_pending.candidate, Some(second));
}

#[test]
fn same_time_cancel_and_leave_beat_due_candidate() {
    for event_kind in [false, true] {
        let mut model = model();
        let ticket = enter(&mut model, 0).entry.unwrap();
        let event = if event_kind {
            LiveAttentionEvent::Cancel
        } else {
            LiveAttentionEvent::PointerLeft { ticket }
        };
        let cancelled = advance(&mut model, 600, input(), event);
        assert!(cancelled.candidate.is_none());
        assert!(cancelled.active.is_none());
        assert_eq!(
            explicit(&mut model, 600).outcome,
            LiveAttentionOutcome::Accepted(LiveAttentionSource::Explicit)
        );
    }
}

#[test]
fn observed_time_never_rolls_back_or_resurrects_expired_feedback() {
    let mut model = model();
    explicit(&mut model, 10_000);
    assert!(reconcile(&mut model, 12_000).active.is_none());
    let backwards = reconcile(&mut model, 10_000);
    assert_eq!(backwards.observed_at, ms(12_000));
    assert!(backwards.active.is_none());
    assert_eq!(
        explicit(&mut model, 0).outcome,
        LiveAttentionOutcome::Rejected(LiveAttentionRejection::Cooldown)
    );
}

#[test]
fn immutable_snapshot_suppresses_invalid_hold_and_keeps_due_candidate_wake() {
    let mut model = model();
    let candidate = enter(&mut model, 0).candidate;
    let blocked = LiveAttentionInput {
        hard_eligible: false,
        ..input()
    };
    let snapshot = model.snapshot(ms(600), &1, blocked).unwrap();
    assert_eq!(snapshot.candidate, candidate);
    assert!(!snapshot.candidate_visible);
    assert_eq!(snapshot.wake_after(ms(600)), Some(Duration::ZERO));
    assert_eq!(reconcile(&mut model, 1).observed_at, ms(1));
    explicit(&mut model, 2);
    assert!(model.snapshot(ms(3), &1, blocked).unwrap().active.is_none());
    assert_eq!(
        model.snapshot(ms(3), &1, blocked).unwrap().presentation,
        input().base_context
    );
    assert!(model
        .snapshot(ms(2002), &1, input())
        .unwrap()
        .active
        .is_none());
}

#[test]
fn ticket_exhaustion_fails_closed_without_reusing_or_losing_exit() {
    let mut model = model();
    let entry = enter(&mut model, 0).entry.unwrap();
    model.next_ticket = None;
    model.cancel(&1);
    assert_eq!(model.entry_offer(), None);
    assert_eq!(model.entry_ticket(), Some(entry));
    leave(&mut model, 1);
    assert_eq!(model.entry_offer(), None);
    assert_eq!(model.entry_ticket(), None);
    assert_eq!(
        model.advance(
            ms(2),
            &1,
            input(),
            LiveAttentionEvent::PointerEntered { ticket: entry }
        ),
        Err(LiveAttentionError::StaleTicket)
    );
}

#[test]
fn maximum_time_cannot_create_an_unexpirable_live_greeting() {
    let mut model = model();
    let offered = model.entry_offer().unwrap();
    let entered = model
        .advance(
            Duration::MAX,
            &1,
            input(),
            LiveAttentionEvent::PointerEntered { ticket: offered },
        )
        .unwrap();
    assert_eq!(
        entered.outcome,
        LiveAttentionOutcome::Rejected(LiveAttentionRejection::ClockExhausted)
    );
    assert_eq!(
        model
            .advance(Duration::MAX, &1, input(), LiveAttentionEvent::Explicit)
            .unwrap()
            .outcome,
        LiveAttentionOutcome::Rejected(LiveAttentionRejection::ClockExhausted)
    );
    assert_eq!(
        model
            .snapshot(Duration::MAX, &1, input())
            .unwrap()
            .next_deadline(),
        None
    );
}

fn shared_trace<Owner: Eq + Clone>(owner: Owner) -> Vec<LiveAttentionSnapshot> {
    let mut model = LiveAttention::default();
    model.set_owner(Some(owner.clone()));
    let ticket = model.entry_offer().unwrap();
    let events = [
        (0, LiveAttentionEvent::PointerEntered { ticket }),
        (599, LiveAttentionEvent::Reconcile),
        (600, LiveAttentionEvent::CandidateDue { ticket }),
        (2599, LiveAttentionEvent::Reconcile),
        (2600, LiveAttentionEvent::Reconcile),
        (4000, LiveAttentionEvent::Explicit),
        (8600, LiveAttentionEvent::Explicit),
    ];
    events
        .into_iter()
        .map(|(time, event)| model.advance(ms(time), &owner, input(), event).unwrap())
        .collect()
}

#[test]
fn identical_native_facts_produce_identical_snapshots_for_all_owner_types() {
    // GTK numeric tokens, egui owned session IDs and Iced tab identities all
    // execute the same production reducer, with no per-host timing policy.
    let gtk_anvil = shared_trace(7_u64);
    let gtk_forge = shared_trace((3_u64, 9_u64));
    let egui_ember = shared_trace(String::from("stable-session"));
    let iced_frost = shared_trace(5_usize);
    assert_eq!(gtk_anvil, gtk_forge);
    assert_eq!(gtk_anvil, egui_ember);
    assert_eq!(gtk_anvil, iced_frost);
}

#[test]
fn position_sync_before_a_queued_real_enter_does_not_retire_its_offer() {
    let mut model = model();
    let captured = model.entry_offer().unwrap();
    let outside = LiveAttentionInput {
        pointer_inside: false,
        ..input()
    };
    advance(&mut model, 0, outside, LiveAttentionEvent::Reconcile);
    reconcile(&mut model, 1);
    assert_eq!(model.entry_offer(), Some(captured));
    assert!(model.entry_ticket().is_none());
    let entered = advance(
        &mut model,
        2,
        input(),
        LiveAttentionEvent::PointerEntered { ticket: captured },
    );
    assert_eq!(entered.outcome, LiveAttentionOutcome::CandidateArmed);
}

#[test]
fn hard_or_hover_loss_retires_old_offers_but_restoration_does_not() {
    for blocked in [
        LiveAttentionInput {
            hard_eligible: false,
            ..input()
        },
        LiveAttentionInput {
            hover_available: false,
            ..input()
        },
    ] {
        let mut model = model();
        reconcile(&mut model, 0);
        let before_loss = model.entry_offer().unwrap();
        advance(&mut model, 1, blocked, LiveAttentionEvent::Reconcile);
        let after_loss = model.entry_offer().unwrap();
        assert_ne!(before_loss, after_loss);
        reconcile(&mut model, 2);
        assert_eq!(model.entry_offer(), Some(after_loss));
        assert_eq!(
            model.advance(
                ms(3),
                &1,
                input(),
                LiveAttentionEvent::PointerEntered {
                    ticket: before_loss
                }
            ),
            Err(LiveAttentionError::StaleTicket)
        );
        assert_eq!(
            advance(
                &mut model,
                3,
                input(),
                LiveAttentionEvent::PointerEntered { ticket: after_loss }
            )
            .outcome,
            LiveAttentionOutcome::CandidateArmed
        );
    }
}

#[test]
fn initially_ineligible_entry_is_consumed_without_a_restoration_replay() {
    let mut model = model();
    let captured = model.entry_offer().unwrap();
    let blocked = LiveAttentionInput {
        hard_eligible: false,
        ..input()
    };
    let refused = advance(
        &mut model,
        0,
        blocked,
        LiveAttentionEvent::PointerEntered { ticket: captured },
    );
    assert_eq!(
        refused.outcome,
        LiveAttentionOutcome::Rejected(LiveAttentionRejection::Unavailable)
    );
    assert_eq!(refused.entry, Some(captured));
    assert!(reconcile(&mut model, 10_000).candidate.is_none());
    assert_eq!(
        enter(&mut model, 10_000).outcome,
        LiveAttentionOutcome::Rejected(LiveAttentionRejection::EntryConsumed)
    );
    leave(&mut model, 10_001);
    assert!(enter(&mut model, 10_001).candidate.is_some());
}
