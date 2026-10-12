use super::*;
use crate::organism::Behavior;
use crate::organism_daily::PreviewPose;

fn ms(value: u64) -> Duration {
    Duration::from_millis(value)
}

fn input(activity: LiveActivityKnowledge) -> LiveAttentionInput {
    LiveAttentionInput {
        base_context: PreviewPose::Calm.context(),
        activity,
        hard_eligible: true,
        hover_available: true,
        pointer_inside: true,
    }
}

fn new_model() -> LiveAttention<&'static str> {
    let mut model = LiveAttention::default();
    model.set_owner(Some("first"));
    model
}

fn advance(
    model: &mut LiveAttention<&'static str>,
    now: u64,
    activity: LiveActivityKnowledge,
    event: LiveAttentionEvent,
) -> LiveAttentionSnapshot {
    model
        .advance(ms(now), &"first", input(activity), event)
        .unwrap()
}

fn enter(
    model: &mut LiveAttention<&'static str>,
    now: u64,
    activity: LiveActivityKnowledge,
) -> LiveAttentionSnapshot {
    let ticket = model.entry_offer().unwrap();
    advance(
        model,
        now,
        activity,
        LiveAttentionEvent::PointerEntered { ticket },
    )
}

fn reconcile(
    model: &mut LiveAttention<&'static str>,
    now: u64,
    activity: LiveActivityKnowledge,
) -> LiveAttentionSnapshot {
    advance(model, now, activity, LiveAttentionEvent::Reconcile)
}

#[test]
fn default_knowledge_is_unknown_not_a_synthetic_idle_claim() {
    assert_eq!(
        LiveActivityKnowledge::default(),
        LiveActivityKnowledge::Unknown
    );
    let mut model = new_model();
    let entered = enter(&mut model, 0, LiveActivityKnowledge::default());
    assert_eq!(
        entered.outcome,
        LiveAttentionOutcome::Rejected(LiveAttentionRejection::UnknownCommandState)
    );
    assert!(entered.entry.is_some());
    assert_eq!(entered.candidate, None);
    assert_eq!(entered.presentation, PreviewPose::Calm.context());
}

#[test]
fn source_admission_matrix_keeps_native_and_semantic_vetoes_independent() {
    for activity in [
        LiveActivityKnowledge::KnownIdle,
        LiveActivityKnowledge::Unknown,
        LiveActivityKnowledge::Busy,
    ] {
        for source in [LiveAttentionSource::Pointer, LiveAttentionSource::Explicit] {
            for hard_eligible in [false, true] {
                for semantic_eligible in [false, true] {
                    let mut model = new_model();
                    let facts = LiveAttentionInput {
                        hard_eligible,
                        base_context: if semantic_eligible {
                            PreviewPose::Calm.context()
                        } else {
                            PreviewPose::Working.context()
                        },
                        ..input(activity)
                    };
                    let event = match source {
                        LiveAttentionSource::Pointer => LiveAttentionEvent::PointerEntered {
                            ticket: model.entry_offer().unwrap(),
                        },
                        LiveAttentionSource::Explicit => LiveAttentionEvent::Explicit,
                    };
                    let snapshot = model.advance(ms(0), &"first", facts, event).unwrap();
                    let expected = if !hard_eligible {
                        LiveAttentionOutcome::Rejected(LiveAttentionRejection::Unavailable)
                    } else if !semantic_eligible || activity == LiveActivityKnowledge::Busy {
                        LiveAttentionOutcome::Rejected(LiveAttentionRejection::BusyContext)
                    } else if source == LiveAttentionSource::Pointer
                        && activity == LiveActivityKnowledge::Unknown
                    {
                        LiveAttentionOutcome::Rejected(LiveAttentionRejection::UnknownCommandState)
                    } else if source == LiveAttentionSource::Pointer {
                        LiveAttentionOutcome::CandidateArmed
                    } else {
                        LiveAttentionOutcome::Accepted(LiveAttentionSource::Explicit)
                    };
                    assert_eq!(
                        snapshot.outcome, expected,
                        "{activity:?} {source:?} {hard_eligible} {semantic_eligible}"
                    );
                    if expected == LiveAttentionOutcome::CandidateArmed {
                        let accepted = model
                            .advance(ms(600), &"first", facts, LiveAttentionEvent::Reconcile)
                            .unwrap();
                        assert_eq!(
                            accepted.active.unwrap().source,
                            LiveAttentionSource::Pointer
                        );
                    } else if matches!(expected, LiveAttentionOutcome::Rejected(_)) {
                        assert!(snapshot.candidate.is_none());
                        assert!(snapshot.active.is_none());
                        assert_eq!(snapshot.presentation, facts.base_context);
                    }
                }
            }
        }
    }
}

#[test]
fn unknown_pointer_refusal_spends_no_cooldown_and_explicit_keeps_the_visit() {
    let mut model = new_model();
    let rejected = enter(&mut model, 0, LiveActivityKnowledge::Unknown);
    let explicit = advance(
        &mut model,
        0,
        LiveActivityKnowledge::Unknown,
        LiveAttentionEvent::Explicit,
    );
    assert_eq!(
        explicit.outcome,
        LiveAttentionOutcome::Accepted(LiveAttentionSource::Explicit)
    );
    assert_eq!(explicit.entry, rejected.entry);
    assert_eq!(explicit.active.unwrap().deadline, ms(2000));
    assert_eq!(explicit.presentation.behavior, Behavior::Approach);
    assert!(reconcile(&mut model, 2000, LiveActivityKnowledge::Unknown)
        .active
        .is_none());
    assert!(
        reconcile(&mut model, 9000, LiveActivityKnowledge::KnownIdle)
            .candidate
            .is_none()
    );
    assert_eq!(
        enter(&mut model, 9000, LiveActivityKnowledge::KnownIdle).outcome,
        LiveAttentionOutcome::Rejected(LiveAttentionRejection::EntryConsumed)
    );
    let left = LiveAttentionEvent::PointerLeft {
        ticket: explicit.entry.unwrap(),
    };
    advance(&mut model, 9000, LiveActivityKnowledge::KnownIdle, left);
    assert_eq!(
        enter(&mut model, 9000, LiveActivityKnowledge::KnownIdle)
            .candidate
            .unwrap()
            .deadline,
        ms(9600)
    );
}

#[test]
fn due_candidate_losing_idle_knowledge_is_retired_without_spending_attention() {
    for when in [599, 600, 601] {
        let mut model = new_model();
        let candidate = enter(&mut model, 0, LiveActivityKnowledge::KnownIdle)
            .candidate
            .unwrap();
        let cancelled = advance(
            &mut model,
            when,
            LiveActivityKnowledge::Unknown,
            LiveAttentionEvent::CandidateDue {
                ticket: candidate.ticket,
            },
        );
        assert!(cancelled.active.is_none());
        assert!(cancelled.candidate.is_none());
        assert_eq!(cancelled.entry, Some(candidate.ticket));
        assert!(
            reconcile(&mut model, when, LiveActivityKnowledge::KnownIdle)
                .candidate
                .is_none()
        );
        let explicit = advance(
            &mut model,
            when,
            LiveActivityKnowledge::Unknown,
            LiveAttentionEvent::Explicit,
        );
        assert_eq!(
            explicit.outcome,
            LiveAttentionOutcome::Accepted(LiveAttentionSource::Explicit)
        );
        assert_eq!(explicit.active.unwrap().deadline, ms(when + 2000));
    }
}

#[test]
fn unknown_cancels_pointer_hold_without_resetting_window_cooldown() {
    let mut model = new_model();
    let entry = enter(&mut model, 0, LiveActivityKnowledge::KnownIdle).entry;
    assert!(reconcile(&mut model, 600, LiveActivityKnowledge::KnownIdle)
        .active
        .is_some());
    let cancelled = reconcile(&mut model, 700, LiveActivityKnowledge::Unknown);
    assert!(cancelled.active.is_none());
    assert_eq!(cancelled.entry, entry);
    assert!(reconcile(&mut model, 800, LiveActivityKnowledge::KnownIdle)
        .active
        .is_none());
    assert_eq!(
        advance(
            &mut model,
            8599,
            LiveActivityKnowledge::Unknown,
            LiveAttentionEvent::Explicit
        )
        .outcome,
        LiveAttentionOutcome::Rejected(LiveAttentionRejection::Cooldown)
    );
    assert_eq!(
        advance(
            &mut model,
            8600,
            LiveActivityKnowledge::Unknown,
            LiveAttentionEvent::Explicit
        )
        .outcome,
        LiveAttentionOutcome::Accepted(LiveAttentionSource::Explicit)
    );
}

#[test]
fn explicit_hold_survives_unknown_and_knowledge_recovery_at_original_deadline() {
    for start in [
        LiveActivityKnowledge::Unknown,
        LiveActivityKnowledge::KnownIdle,
    ] {
        let mut model = new_model();
        let original = advance(&mut model, 0, start, LiveAttentionEvent::Explicit).active;
        for (now, activity) in [
            (500, LiveActivityKnowledge::Unknown),
            (1000, LiveActivityKnowledge::KnownIdle),
            (1999, LiveActivityKnowledge::Unknown),
        ] {
            let snapshot = reconcile(&mut model, now, activity);
            assert_eq!(snapshot.active, original);
            assert!(snapshot.candidate.is_none());
        }
        let painted = reconcile(&mut model, 1999, LiveActivityKnowledge::Unknown);
        assert_eq!(painted.wake_after(ms(2000)), Some(Duration::ZERO));
        assert_eq!(
            reconcile(&mut model, 2000, LiveActivityKnowledge::Unknown).next_deadline(),
            None
        );
        assert_eq!(
            reconcile(&mut model, 1000, LiveActivityKnowledge::KnownIdle).next_deadline(),
            None
        );
    }
}

#[test]
fn positively_busy_cancels_both_sources_and_retains_their_cooldown() {
    for source in [LiveAttentionSource::Pointer, LiveAttentionSource::Explicit] {
        let mut model = new_model();
        let admitted = match source {
            LiveAttentionSource::Pointer => {
                enter(&mut model, 0, LiveActivityKnowledge::KnownIdle);
                reconcile(&mut model, 600, LiveActivityKnowledge::KnownIdle);
                600
            }
            LiveAttentionSource::Explicit => {
                advance(
                    &mut model,
                    0,
                    LiveActivityKnowledge::Unknown,
                    LiveAttentionEvent::Explicit,
                );
                0
            }
        };
        assert!(reconcile(&mut model, 1000, LiveActivityKnowledge::Busy)
            .active
            .is_none());
        assert!(reconcile(&mut model, 1001, LiveActivityKnowledge::Unknown)
            .active
            .is_none());
        assert_eq!(
            advance(
                &mut model,
                admitted + 7999,
                LiveActivityKnowledge::Unknown,
                LiveAttentionEvent::Explicit
            )
            .outcome,
            LiveAttentionOutcome::Rejected(LiveAttentionRejection::Cooldown)
        );
        assert_eq!(
            advance(
                &mut model,
                admitted + 8000,
                LiveActivityKnowledge::Unknown,
                LiveAttentionEvent::Explicit
            )
            .outcome,
            LiveAttentionOutcome::Accepted(LiveAttentionSource::Explicit)
        );
    }
}

#[test]
fn knowledge_loss_retires_unconsumed_offers_but_recovery_and_sync_do_not() {
    let mut model = new_model();
    reconcile(&mut model, 0, LiveActivityKnowledge::KnownIdle);
    let before_loss = model.entry_offer().unwrap();
    reconcile(&mut model, 1, LiveActivityKnowledge::Unknown);
    let after_loss = model.entry_offer().unwrap();
    assert_ne!(before_loss, after_loss);
    reconcile(&mut model, 2, LiveActivityKnowledge::KnownIdle);
    assert_eq!(model.entry_offer(), Some(after_loss));
    assert!(model.entry_ticket().is_none());
    assert_eq!(
        model.advance(
            ms(99_000),
            &"first",
            input(LiveActivityKnowledge::Busy),
            LiveAttentionEvent::PointerEntered {
                ticket: before_loss
            }
        ),
        Err(LiveAttentionError::StaleTicket)
    );
    let enter = advance(
        &mut model,
        3,
        LiveActivityKnowledge::KnownIdle,
        LiveAttentionEvent::PointerEntered { ticket: after_loss },
    );
    assert_eq!(enter.observed_at, ms(3));
    assert_eq!(enter.outcome, LiveAttentionOutcome::CandidateArmed);
    reconcile(&mut model, 4, LiveActivityKnowledge::Unknown);
    let left = advance(
        &mut model,
        5,
        LiveActivityKnowledge::Unknown,
        LiveAttentionEvent::PointerLeft {
            ticket: enter.entry.unwrap(),
        },
    );
    assert!(left.entry.is_none());
    assert!(reconcile(&mut model, 6, LiveActivityKnowledge::KnownIdle)
        .candidate
        .is_none());
}

#[test]
fn stale_owner_busy_input_cannot_cancel_new_owner_or_advance_its_clock() {
    let mut model = new_model();
    let old = enter(&mut model, 0, LiveActivityKnowledge::KnownIdle)
        .candidate
        .unwrap();
    model.set_owner(Some("second"));
    let current = model
        .advance(
            ms(1),
            &"second",
            input(LiveActivityKnowledge::Unknown),
            LiveAttentionEvent::Explicit,
        )
        .unwrap();
    assert_eq!(
        model.advance(
            ms(99_000),
            &"first",
            input(LiveActivityKnowledge::Busy),
            LiveAttentionEvent::CandidateDue { ticket: old.ticket }
        ),
        Err(LiveAttentionError::StaleOwner)
    );
    assert_eq!(
        model.snapshot(ms(99_000), &"first", input(LiveActivityKnowledge::Busy)),
        Err(LiveAttentionError::StaleOwner)
    );
    let after = model
        .advance(
            ms(2),
            &"second",
            input(LiveActivityKnowledge::Unknown),
            LiveAttentionEvent::Reconcile,
        )
        .unwrap();
    assert_eq!(after.active, current.active);
    assert_eq!(after.observed_at, ms(2));
}

#[test]
fn stale_ticket_busy_input_cannot_cancel_a_new_current_owner_hold() {
    let mut model = new_model();
    let old = enter(&mut model, 0, LiveActivityKnowledge::KnownIdle)
        .candidate
        .unwrap();
    let current = advance(
        &mut model,
        1,
        LiveActivityKnowledge::Unknown,
        LiveAttentionEvent::Explicit,
    );
    assert_eq!(
        model.advance(
            ms(99_000),
            &"first",
            input(LiveActivityKnowledge::Busy),
            LiveAttentionEvent::CandidateDue { ticket: old.ticket }
        ),
        Err(LiveAttentionError::StaleTicket)
    );
    let after = reconcile(&mut model, 2, LiveActivityKnowledge::Unknown);
    assert_eq!(after.active, current.active);
    assert_eq!(after.observed_at, ms(2));
}

#[test]
fn snapshots_mask_unknown_pointer_and_busy_feedback_without_mutating_the_model() {
    let mut model = new_model();
    let pending = enter(&mut model, 0, LiveActivityKnowledge::KnownIdle).candidate;
    let before = format!("{model:?}");
    for activity in [LiveActivityKnowledge::Unknown, LiveActivityKnowledge::Busy] {
        let snapshot = model.snapshot(ms(600), &"first", input(activity)).unwrap();
        assert_eq!(snapshot.candidate, pending);
        assert!(!snapshot.candidate_visible);
        assert!(snapshot.active.is_none());
        assert_eq!(snapshot.wake_after(ms(600)), Some(Duration::ZERO));
        assert_eq!(format!("{model:?}"), before);
    }
    let hold = reconcile(&mut model, 600, LiveActivityKnowledge::KnownIdle).active;
    let unknown = model
        .snapshot(ms(700), &"first", input(LiveActivityKnowledge::Unknown))
        .unwrap();
    assert!(unknown.active.is_none());
    assert_eq!(unknown.presentation, PreviewPose::Calm.context());
    assert_eq!(
        reconcile(&mut model, 700, LiveActivityKnowledge::KnownIdle).active,
        hold
    );

    let mut explicit = new_model();
    let accepted = advance(
        &mut explicit,
        0,
        LiveActivityKnowledge::Unknown,
        LiveAttentionEvent::Explicit,
    )
    .active;
    assert_eq!(
        explicit
            .snapshot(ms(1), &"first", input(LiveActivityKnowledge::Unknown))
            .unwrap()
            .active,
        accepted
    );
    assert!(explicit
        .snapshot(ms(1), &"first", input(LiveActivityKnowledge::Busy))
        .unwrap()
        .active
        .is_none());
    assert_eq!(
        reconcile(&mut explicit, 1, LiveActivityKnowledge::Unknown).active,
        accepted
    );
}

#[test]
fn unknown_explicit_and_known_idle_other_owner_share_one_cooldown() {
    let mut model = new_model();
    advance(
        &mut model,
        0,
        LiveActivityKnowledge::Unknown,
        LiveAttentionEvent::Explicit,
    );
    model.set_owner(Some("second"));
    let ticket = model.entry_offer().unwrap();
    let entered = model
        .advance(
            ms(2000),
            &"second",
            input(LiveActivityKnowledge::KnownIdle),
            LiveAttentionEvent::PointerEntered { ticket },
        )
        .unwrap();
    assert_eq!(
        entered.outcome,
        LiveAttentionOutcome::Rejected(LiveAttentionRejection::Cooldown)
    );
    let recovered = model
        .advance(
            ms(8000),
            &"second",
            input(LiveActivityKnowledge::KnownIdle),
            LiveAttentionEvent::Reconcile,
        )
        .unwrap();
    assert!(recovered.candidate.is_none());
    assert_eq!(recovered.entry, Some(ticket));
    model
        .advance(
            ms(8000),
            &"second",
            input(LiveActivityKnowledge::KnownIdle),
            LiveAttentionEvent::PointerLeft { ticket },
        )
        .unwrap();
    let ticket = model.entry_offer().unwrap();
    assert_eq!(
        model
            .advance(
                ms(8000),
                &"second",
                input(LiveActivityKnowledge::KnownIdle),
                LiveAttentionEvent::PointerEntered { ticket }
            )
            .unwrap()
            .outcome,
        LiveAttentionOutcome::CandidateArmed
    );
}

#[test]
fn knowledge_does_not_impersonate_geometry_or_bypass_static_input_and_vte_gates() {
    let mut model = new_model();
    let unknown = input(LiveActivityKnowledge::Unknown);
    let no_target = LiveAttentionInput {
        hover_available: false,
        pointer_inside: false,
        ..unknown
    };
    let accepted = model
        .advance(ms(0), &"first", no_target, LiveAttentionEvent::Explicit)
        .unwrap();
    assert!(accepted.active.is_some());
    // Full/Calm use the same factual hard eligibility and preserve this hold.
    assert_eq!(
        model
            .advance(ms(1000), &"first", no_target, LiveAttentionEvent::Reconcile)
            .unwrap()
            .active,
        accepted.active
    );
    // Static, input retreat or unknown VTE alternate-screen safety remain a
    // genuine independent veto; Unknown knowledge never grants permission.
    let unavailable = LiveAttentionInput {
        hard_eligible: false,
        ..unknown
    };
    assert_eq!(
        model
            .advance(
                ms(1001),
                &"first",
                unavailable,
                LiveAttentionEvent::Explicit
            )
            .unwrap()
            .outcome,
        LiveAttentionOutcome::Rejected(LiveAttentionRejection::Unavailable)
    );
    assert!(reconcile(&mut model, 1002, LiveActivityKnowledge::Unknown)
        .active
        .is_none());
    assert_eq!(
        advance(
            &mut model,
            7999,
            LiveActivityKnowledge::Unknown,
            LiveAttentionEvent::Explicit
        )
        .outcome,
        LiveAttentionOutcome::Rejected(LiveAttentionRejection::Cooldown)
    );
}

#[test]
fn unknown_does_not_override_a_real_unknown_outcome_or_other_work_reaction() {
    let mut model = new_model();
    let protected = LiveAttentionInput {
        base_context: crate::organism::RenderContext::new(
            Behavior::UnknownOutcome,
            Default::default(),
            false,
        ),
        ..input(LiveActivityKnowledge::Unknown)
    };
    let snapshot = model
        .advance(ms(0), &"first", protected, LiveAttentionEvent::Explicit)
        .unwrap();
    assert_eq!(
        snapshot.outcome,
        LiveAttentionOutcome::Rejected(LiveAttentionRejection::BusyContext)
    );
    assert_eq!(snapshot.presentation, protected.base_context);
    assert!(snapshot.active.is_none());
}

#[test]
fn unknown_explicit_retains_rollback_and_unrepresentable_deadline_protection() {
    let mut model = new_model();
    let accepted = advance(
        &mut model,
        10_000,
        LiveActivityKnowledge::Unknown,
        LiveAttentionEvent::Explicit,
    );
    assert_eq!(
        reconcile(&mut model, 11_999, LiveActivityKnowledge::KnownIdle).active,
        accepted.active
    );
    assert!(
        reconcile(&mut model, 12_000, LiveActivityKnowledge::Unknown)
            .active
            .is_none()
    );
    assert!(
        reconcile(&mut model, 10_000, LiveActivityKnowledge::Unknown)
            .active
            .is_none()
    );
    assert_eq!(
        advance(
            &mut model,
            0,
            LiveActivityKnowledge::Unknown,
            LiveAttentionEvent::Explicit
        )
        .outcome,
        LiveAttentionOutcome::Rejected(LiveAttentionRejection::Cooldown)
    );
    let last = model
        .advance(
            Duration::MAX,
            &"first",
            input(LiveActivityKnowledge::Unknown),
            LiveAttentionEvent::Explicit,
        )
        .unwrap();
    assert_eq!(
        last.outcome,
        LiveAttentionOutcome::Rejected(LiveAttentionRejection::ClockExhausted)
    );
    assert_eq!(last.next_deadline(), None);
}

#[test]
fn already_blocked_knowledge_changes_do_not_retire_an_offer_twice() {
    for missing_target in [false, true] {
        let blocked = |activity| LiveAttentionInput {
            hard_eligible: missing_target,
            hover_available: !missing_target,
            ..input(activity)
        };
        let mut model = new_model();
        model
            .advance(
                ms(0),
                &"first",
                blocked(LiveActivityKnowledge::KnownIdle),
                LiveAttentionEvent::Reconcile,
            )
            .unwrap();
        let offered = model.entry_offer().unwrap();
        model
            .advance(
                ms(1),
                &"first",
                blocked(LiveActivityKnowledge::Unknown),
                LiveAttentionEvent::Reconcile,
            )
            .unwrap();
        assert_eq!(model.entry_offer(), Some(offered));
        let refused = model
            .advance(
                ms(2),
                &"first",
                blocked(LiveActivityKnowledge::Unknown),
                LiveAttentionEvent::PointerEntered { ticket: offered },
            )
            .unwrap();
        let reason = if missing_target {
            LiveAttentionRejection::UnknownCommandState
        } else {
            LiveAttentionRejection::Unavailable
        };
        assert_eq!(refused.outcome, LiveAttentionOutcome::Rejected(reason));
        assert_eq!(refused.entry, Some(offered));
        assert!(reconcile(&mut model, 3, LiveActivityKnowledge::KnownIdle)
            .candidate
            .is_none());
        assert_eq!(
            model.advance(
                ms(4),
                &"first",
                input(LiveActivityKnowledge::KnownIdle),
                LiveAttentionEvent::PointerEntered { ticket: offered }
            ),
            Err(LiveAttentionError::StaleTicket)
        );
        assert_eq!(
            enter(&mut model, 4, LiveActivityKnowledge::KnownIdle).outcome,
            LiveAttentionOutcome::Rejected(LiveAttentionRejection::EntryConsumed)
        );
        advance(
            &mut model,
            5,
            LiveActivityKnowledge::KnownIdle,
            LiveAttentionEvent::PointerLeft { ticket: offered },
        );
        assert_eq!(
            enter(&mut model, 5, LiveActivityKnowledge::KnownIdle).outcome,
            LiveAttentionOutcome::CandidateArmed
        );

        // If no visit was delivered, recovery retains the unused offer. A
        // real native Enter is still required; reconciliation never arms it.
        let mut untouched = new_model();
        untouched
            .advance(
                ms(0),
                &"first",
                blocked(LiveActivityKnowledge::KnownIdle),
                LiveAttentionEvent::Reconcile,
            )
            .unwrap();
        let offered = untouched.entry_offer().unwrap();
        for (now, activity) in [
            (1, LiveActivityKnowledge::Unknown),
            (2, LiveActivityKnowledge::Busy),
            (3, LiveActivityKnowledge::Unknown),
        ] {
            untouched
                .advance(
                    ms(now),
                    &"first",
                    blocked(activity),
                    LiveAttentionEvent::Reconcile,
                )
                .unwrap();
        }
        // In the no-target case Busy removes the still-available Explicit
        // permission, so that genuine all-source loss retires the offer.
        let current = untouched.entry_offer().unwrap();
        if missing_target {
            assert_ne!(current, offered);
        } else {
            assert_eq!(current, offered);
        }
        let restored = reconcile(&mut untouched, 4, LiveActivityKnowledge::KnownIdle);
        assert_eq!(restored.entry_offer, Some(current));
        assert!(restored.entry.is_none());
        assert!(restored.candidate.is_none());
        assert_eq!(
            advance(
                &mut untouched,
                5,
                LiveActivityKnowledge::KnownIdle,
                LiveAttentionEvent::PointerEntered { ticket: current }
            )
            .outcome,
            LiveAttentionOutcome::CandidateArmed
        );
    }
}
