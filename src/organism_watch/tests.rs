use super::*;

type Identity = (String, u64, u64);

fn ms(value: u64) -> Duration {
    Duration::from_millis(value)
}
fn ns(value: u64) -> Duration {
    Duration::from_nanos(value)
}
fn id(owner: &str, command: u64, context: u64) -> Identity {
    (owner.into(), command, context)
}

fn started(attach: bool) -> (WatchObservation<Identity>, Identity) {
    let identity = id("pane", 1, 1);
    let mut watch = WatchObservation::default();
    watch.set_identity(Some(identity.clone()));
    watch
        .advance(
            Duration::ZERO,
            &identity,
            if attach {
                WatchEvent::AttachRunningObservation
            } else {
                WatchEvent::BeginTrustedCommand
            },
        )
        .unwrap();
    (watch, identity)
}

fn output(
    watch: &mut WatchObservation<Identity>,
    identity: &Identity,
    seq: u64,
    now: Duration,
    disposition: WatchOutputDisposition,
) -> WatchUpdate {
    watch
        .advance(
            now,
            identity,
            WatchEvent::OutputObserved {
                batch_seq: seq,
                disposition,
            },
        )
        .unwrap()
}

fn pulse(
    watch: &mut WatchObservation<Identity>,
    identity: &Identity,
    seq: u64,
    time: u64,
) -> WatchUpdate {
    output(
        watch,
        identity,
        seq,
        ms(time),
        WatchOutputDisposition::Accepted,
    )
}

fn rhythm(watch: &WatchObservation<Identity>, identity: &Identity, time: u64) -> WatchRhythm {
    watch.snapshot(ms(time), identity).unwrap().rhythm
}

#[test]
fn trusted_start_has_exact_busy_and_quiet_boundaries() {
    let (mut watch, identity) = started(false);
    pulse(&mut watch, &identity, 1, 100);
    pulse(&mut watch, &identity, 2, 600);
    let update = pulse(&mut watch, &identity, 3, 1300);
    assert_eq!(update.snapshot.rhythm, WatchRhythm::Busy);
    let deadline = update.snapshot.next_deadline.unwrap();
    assert_eq!(deadline.at(), ms(1300) + ns(1));
    assert_eq!(rhythm(&watch, &identity, 1300), WatchRhythm::Busy);
    assert_eq!(
        watch.snapshot(ms(1300) + ns(1), &identity).unwrap().rhythm,
        WatchRhythm::Steady
    );
    let update = watch
        .advance(
            deadline.at(),
            &identity,
            WatchEvent::DeadlineDue { deadline },
        )
        .unwrap();
    assert_eq!(update.snapshot.next_deadline.unwrap().at(), ms(4300));
    assert_eq!(rhythm(&watch, &identity, 4299), WatchRhythm::Steady);
    assert_eq!(rhythm(&watch, &identity, 4300), WatchRhythm::Waiting);
    let update = watch
        .advance(ms(4300), &identity, WatchEvent::Advance)
        .unwrap();
    assert_eq!(update.snapshot.next_deadline, None);
}

#[test]
fn attach_first_batch_is_neutral_but_real_trusted_start_can_resume() {
    let (mut attached, identity) = started(true);
    let (mut trusted, _) = started(false);
    assert_eq!(rhythm(&attached, &identity, 3000), WatchRhythm::Waiting);
    assert_eq!(
        pulse(&mut attached, &identity, 1, 5000).outcome,
        WatchOutcome::BaselineOnly
    );
    assert_eq!(
        pulse(&mut trusted, &identity, 1, 5000).snapshot.rhythm,
        WatchRhythm::Resumed
    );
    assert_eq!(rhythm(&attached, &identity, 7999), WatchRhythm::Steady);
    assert_eq!(rhythm(&attached, &identity, 8000), WatchRhythm::Waiting);
    assert_eq!(
        pulse(&mut attached, &identity, 2, 8000).snapshot.rhythm,
        WatchRhythm::Resumed
    );
    assert_eq!(rhythm(&attached, &identity, 8899), WatchRhythm::Resumed);
    assert_eq!(rhythm(&attached, &identity, 8900), WatchRhythm::Steady);
}

#[test]
fn resumed_has_priority_then_busy_then_quiet_without_replaying_edges() {
    let (mut watch, identity) = started(false);
    pulse(&mut watch, &identity, 1, 3000);
    pulse(&mut watch, &identity, 2, 3100);
    pulse(&mut watch, &identity, 3, 3200);
    let resumed = watch.snapshot(ms(3899), &identity).unwrap();
    assert_eq!(resumed.rhythm, WatchRhythm::Resumed);
    assert_eq!(resumed.next_deadline.unwrap().at(), ms(3900));
    let busy = watch
        .advance(ms(3900), &identity, WatchEvent::Advance)
        .unwrap()
        .snapshot;
    assert_eq!(busy.rhythm, WatchRhythm::Busy);
    assert_eq!(busy.next_deadline.unwrap().at(), ms(4200) + ns(1));
    assert_eq!(rhythm(&watch, &identity, 4200), WatchRhythm::Busy);
    assert_eq!(rhythm(&watch, &identity, 4201), WatchRhythm::Steady);
    assert_eq!(rhythm(&watch, &identity, 6199), WatchRhythm::Steady);
    let waiting = watch
        .advance(ms(6200), &identity, WatchEvent::Advance)
        .unwrap()
        .snapshot;
    assert_eq!(waiting.rhythm, WatchRhythm::Waiting);
    assert_eq!(waiting.next_deadline, None);
}

#[test]
fn repeated_batch_and_parser_segments_cannot_manufacture_three_pulses() {
    let (mut watch, identity) = started(false);
    pulse(&mut watch, &identity, 10, 100);
    let frozen = watch.clone();
    for time in [100, 200, 100_000] {
        assert_eq!(
            watch.advance(
                ms(time),
                &identity,
                WatchEvent::OutputObserved {
                    batch_seq: 10,
                    disposition: WatchOutputDisposition::Accepted,
                }
            ),
            Err(WatchError::StaleBatch)
        );
        assert_eq!(watch, frozen);
    }
    pulse(&mut watch, &identity, 11, 100);
    let frozen = watch.clone();
    assert_eq!(
        watch.advance(
            ms(999_999),
            &identity,
            WatchEvent::OutputObserved {
                batch_seq: 10,
                disposition: WatchOutputDisposition::Quarantined,
            }
        ),
        Err(WatchError::StaleBatch)
    );
    assert_eq!(watch, frozen);
    assert_eq!(
        pulse(&mut watch, &identity, 12, 100).snapshot.rhythm,
        WatchRhythm::Busy
    );
}

#[test]
fn every_identity_component_rejects_old_events_before_clock_or_rebinding() {
    let (mut watch, identity) = started(false);
    let deadline = watch
        .snapshot(ms(0), &identity)
        .unwrap()
        .next_deadline
        .unwrap();
    for stale in [id("other", 1, 1), id("pane", 0, 1), id("pane", 1, 0)] {
        for event in [
            WatchEvent::BeginTrustedCommand,
            WatchEvent::AttachRunningObservation,
            WatchEvent::OutputObserved {
                batch_seq: u64::MAX,
                disposition: WatchOutputDisposition::Quarantined,
            },
            WatchEvent::DeadlineDue { deadline },
            WatchEvent::Advance,
            WatchEvent::Retire,
        ] {
            let frozen = watch.clone();
            assert_eq!(
                watch.advance(Duration::MAX, &stale, event),
                Err(WatchError::StaleIdentity)
            );
            assert_eq!(watch, frozen);
        }
        assert_eq!(
            watch.snapshot(Duration::MAX, &stale),
            Err(WatchError::StaleIdentity)
        );
    }
}

#[test]
fn owner_return_and_same_owner_new_command_require_fresh_identity() {
    let (mut watch, first) = started(false);
    let old = watch
        .snapshot(ms(0), &first)
        .unwrap()
        .next_deadline
        .unwrap();
    let second = id("other", 1, 2);
    let third = id("pane", 2, 3);
    for current in [&second, &third] {
        watch.set_identity(Some(current.clone()));
        watch
            .advance(ms(10), current, WatchEvent::AttachRunningObservation)
            .unwrap();
        let frozen = watch.clone();
        assert_eq!(
            watch.advance(
                Duration::MAX,
                &first,
                WatchEvent::DeadlineDue { deadline: old }
            ),
            Err(WatchError::StaleIdentity)
        );
        assert_eq!(watch, frozen);
    }
    assert_eq!(watch.identity(), Some(&third));
    assert_eq!(
        pulse(&mut watch, &third, 0, 5000).outcome,
        WatchOutcome::BaselineOnly
    );
}

#[test]
fn quarantine_clears_activity_preserves_high_water_and_suppresses_first_recovery() {
    let (mut watch, identity) = started(false);
    pulse(&mut watch, &identity, 1, 3000);
    assert_eq!(
        output(
            &mut watch,
            &identity,
            2,
            ms(3100),
            WatchOutputDisposition::Quarantined
        )
        .snapshot
        .rhythm,
        WatchRhythm::Steady
    );
    let frozen = watch.clone();
    assert_eq!(
        watch.advance(
            Duration::MAX,
            &identity,
            WatchEvent::OutputObserved {
                batch_seq: 1,
                disposition: WatchOutputDisposition::Accepted,
            }
        ),
        Err(WatchError::StaleBatch)
    );
    assert_eq!(watch, frozen);
    assert_eq!(rhythm(&watch, &identity, 6100), WatchRhythm::Waiting);
    let baseline = pulse(&mut watch, &identity, 3, 7000);
    assert_eq!(baseline.outcome, WatchOutcome::BaselineOnly);
    assert_eq!(baseline.snapshot.rhythm, WatchRhythm::Steady);
    assert_eq!(
        pulse(&mut watch, &identity, 4, 10_000).snapshot.rhythm,
        WatchRhythm::Resumed
    );
}

#[test]
fn baseline_only_consumes_first_suppression_without_a_counted_pulse() {
    let (mut watch, identity) = started(true);
    let baseline = output(
        &mut watch,
        &identity,
        1,
        ms(5000),
        WatchOutputDisposition::BaselineOnly,
    );
    assert_eq!(baseline.outcome, WatchOutcome::BaselineOnly);
    assert_eq!(watch.outputs, [None; 3]);
    assert!(!watch.first_pending);
    assert_eq!(
        pulse(&mut watch, &identity, 2, 8000).snapshot.rhythm,
        WatchRhythm::Resumed
    );
}

#[test]
fn retirement_causes_all_share_one_terminal_identity_and_no_deadline() {
    // These native reasons all map to the same fact; the core has no host branch.
    for _reason in [
        "completed",
        "input retreat",
        "hidden",
        "closed",
        "owner lost",
        "alternate screen",
        "remote",
        "Static",
        "observation capability lost",
    ] {
        let (mut watch, identity) = started(false);
        pulse(&mut watch, &identity, 1, 3000);
        let old = watch
            .snapshot(ms(3000), &identity)
            .unwrap()
            .next_deadline
            .unwrap();
        let update = watch
            .advance(ms(3100), &identity, WatchEvent::Retire)
            .unwrap();
        assert_eq!(
            update.snapshot,
            WatchSnapshot {
                rhythm: WatchRhythm::Steady,
                next_deadline: None
            }
        );
        let frozen = watch.clone();
        assert!(!watch.set_identity(Some(identity.clone())));
        assert_eq!(
            watch.advance(
                Duration::MAX,
                &identity,
                WatchEvent::DeadlineDue { deadline: old }
            ),
            Err(WatchError::NotObserving)
        );
        assert_eq!(
            watch.advance(
                Duration::MAX,
                &identity,
                WatchEvent::AttachRunningObservation
            ),
            Err(WatchError::IdentityAlreadyStarted)
        );
        assert_eq!(watch, frozen);
        let recovered = id("pane", 1, 2);
        watch.set_identity(Some(recovered.clone()));
        watch
            .advance(ms(3100), &recovered, WatchEvent::AttachRunningObservation)
            .unwrap();
        assert_eq!(
            pulse(&mut watch, &recovered, 0, 10_000).outcome,
            WatchOutcome::BaselineOnly
        );
    }
}

#[test]
fn retirement_before_queued_start_prevents_late_admission() {
    let mut watch = WatchObservation::default();
    let identity = id("pane", 1, 1);
    watch.set_identity(Some(identity.clone()));
    assert_eq!(
        watch
            .advance(ms(0), &identity, WatchEvent::Retire)
            .unwrap()
            .outcome,
        WatchOutcome::Retired
    );
    assert_eq!(
        watch.advance(ms(100), &identity, WatchEvent::BeginTrustedCommand),
        Err(WatchError::IdentityAlreadyStarted)
    );
    assert_eq!(
        watch.snapshot(ms(100), &identity).unwrap().next_deadline,
        None
    );
}

#[test]
fn no_observation_or_unknown_running_capability_cannot_create_a_rhythm() {
    let mut watch: WatchObservation<Identity> = WatchObservation::default();
    let identity = id("vte-neutral", 1, 1);
    for seq in 0..10 {
        assert_eq!(
            watch.advance(
                ms(seq * 5000),
                &identity,
                WatchEvent::OutputObserved {
                    batch_seq: seq,
                    disposition: WatchOutputDisposition::Accepted,
                }
            ),
            Err(WatchError::StaleIdentity)
        );
    }
    assert_eq!(watch.clock, Duration::ZERO);
    assert_eq!(watch.deadline, None);
    // Binding alone is also not evidence of a running command.
    watch.set_identity(Some(identity.clone()));
    assert_eq!(
        watch.advance(ms(10_000), &identity, WatchEvent::Advance),
        Err(WatchError::NotObserving)
    );
    assert_eq!(
        watch.snapshot(ms(10_000), &identity).unwrap().rhythm,
        WatchRhythm::Steady
    );
}

#[test]
fn future_read_only_sample_does_not_advance_real_time_or_consume_due_wake() {
    let (mut watch, identity) = started(false);
    let frozen = watch.clone();
    let future = watch.snapshot(ms(60_000), &identity).unwrap();
    assert_eq!(future.rhythm, WatchRhythm::Waiting);
    assert_eq!(future.next_deadline.unwrap().at(), ms(3000));
    assert_eq!(watch, frozen);
    assert_eq!(
        pulse(&mut watch, &identity, 1, 100).outcome,
        WatchOutcome::Counted
    );
    assert_eq!(watch.clock, ms(100));
    assert_eq!(rhythm(&watch, &identity, 100), WatchRhythm::Steady);
}

#[test]
fn backwards_batches_are_consumed_without_changing_baseline_or_first_pending() {
    let (mut watch, identity) = started(true);
    watch
        .advance(ms(1000), &identity, WatchEvent::Advance)
        .unwrap();
    let baseline = watch.baseline;
    let deadline = watch.deadline;
    for (seq, disposition) in [
        (1, WatchOutputDisposition::Accepted),
        (2, WatchOutputDisposition::BaselineOnly),
    ] {
        assert_eq!(
            output(&mut watch, &identity, seq, ms(999), disposition).outcome,
            WatchOutcome::BackwardsBatchConsumed
        );
        assert_eq!(watch.baseline, baseline);
        assert_eq!(watch.deadline, deadline);
        assert!(watch.first_pending);
        assert_eq!(watch.clock, ms(1000));
    }
    assert_eq!(
        pulse(&mut watch, &identity, 3, 6000).outcome,
        WatchOutcome::BaselineOnly
    );
    let frozen = watch.clone();
    assert_eq!(
        watch.advance(
            ms(9000),
            &identity,
            WatchEvent::OutputObserved {
                batch_seq: 2,
                disposition: WatchOutputDisposition::Accepted,
            }
        ),
        Err(WatchError::StaleBatch)
    );
    assert_eq!(watch, frozen);
}

#[test]
fn backwards_quarantine_is_real_invalidation_at_the_monotonic_clock() {
    let (mut watch, identity) = started(false);
    pulse(&mut watch, &identity, 1, 3000);
    assert_eq!(
        output(
            &mut watch,
            &identity,
            2,
            ms(2000),
            WatchOutputDisposition::Quarantined
        )
        .outcome,
        WatchOutcome::Quarantined
    );
    assert_eq!(watch.clock, ms(3000));
    assert_eq!(watch.baseline, Some(ms(3000)));
    assert_eq!(watch.outputs, [None; 3]);
    assert!(watch.first_pending);
    assert_eq!(watch.deadline.unwrap().at(), ms(6000));
    assert_eq!(
        pulse(&mut watch, &identity, 3, 7000).outcome,
        WatchOutcome::BaselineOnly
    );
}

#[test]
fn ordinary_advances_preserve_ticket_but_same_time_replacement_retires_it() {
    let (mut watch, identity) = started(false);
    let original = watch.deadline.unwrap();
    for time in [0, 100, 200, 100, 2999] {
        watch
            .advance(ms(time), &identity, WatchEvent::Advance)
            .unwrap();
        assert_eq!(watch.deadline, Some(original));
    }
    let frozen = watch.clone();
    assert_eq!(
        watch.advance(
            ms(2999),
            &identity,
            WatchEvent::DeadlineDue { deadline: original }
        ),
        Err(WatchError::DeadlineNotDue)
    );
    assert_eq!(watch, frozen);

    let (mut watch, identity) = started(false);
    let old = watch.deadline.unwrap();
    output(
        &mut watch,
        &identity,
        1,
        ms(0),
        WatchOutputDisposition::Quarantined,
    );
    let new = watch.deadline.unwrap();
    assert_eq!(old.at(), new.at());
    assert_ne!(old, new);
    let frozen = watch.clone();
    assert_eq!(
        watch.advance(
            Duration::MAX,
            &identity,
            WatchEvent::DeadlineDue { deadline: old }
        ),
        Err(WatchError::StaleDeadline)
    );
    assert_eq!(watch, frozen);
    assert_eq!(
        watch
            .advance(
                ms(3000),
                &identity,
                WatchEvent::DeadlineDue { deadline: new }
            )
            .unwrap()
            .snapshot
            .next_deadline,
        None
    );
    let frozen = watch.clone();
    assert_eq!(
        watch.advance(
            Duration::MAX,
            &identity,
            WatchEvent::DeadlineDue { deadline: new }
        ),
        Err(WatchError::StaleDeadline)
    );
    assert_eq!(watch, frozen);
}

#[test]
fn overdue_deadline_consumes_once_without_catching_up_intermediate_rhythms() {
    let (mut watch, identity) = started(false);
    pulse(&mut watch, &identity, 1, 3000);
    pulse(&mut watch, &identity, 2, 3100);
    pulse(&mut watch, &identity, 3, 3200);
    let deadline = watch.deadline.unwrap();
    let update = watch
        .advance(ms(100_000), &identity, WatchEvent::DeadlineDue { deadline })
        .unwrap();
    assert_eq!(update.snapshot.rhythm, WatchRhythm::Waiting);
    assert_eq!(update.snapshot.next_deadline, None);
    for _ in 0..3 {
        assert_eq!(
            watch
                .advance(ms(100_000), &identity, WatchEvent::Advance)
                .unwrap()
                .snapshot,
            update.snapshot
        );
    }
}

#[test]
fn sequence_exhaustion_fails_closed_until_a_fresh_observation_epoch() {
    let (mut watch, identity) = started(false);
    pulse(&mut watch, &identity, u64::MAX - 1, 100);
    pulse(&mut watch, &identity, u64::MAX, 200);
    for seq in [0, 1, u64::MAX - 1, u64::MAX] {
        let frozen = watch.clone();
        assert_eq!(
            watch.advance(
                Duration::MAX,
                &identity,
                WatchEvent::OutputObserved {
                    batch_seq: seq,
                    disposition: WatchOutputDisposition::Quarantined,
                }
            ),
            Err(WatchError::StaleBatch)
        );
        assert_eq!(watch, frozen);
    }
    let next = id("pane", 1, 2);
    watch.set_identity(Some(next.clone()));
    watch
        .advance(ms(200), &next, WatchEvent::AttachRunningObservation)
        .unwrap();
    assert_eq!(
        pulse(&mut watch, &next, 0, 200).outcome,
        WatchOutcome::BaselineOnly
    );
}

#[test]
fn ticket_exhaustion_never_reuses_a_revision_or_leaves_a_background_deadline() {
    let (mut watch, identity) = started(false);
    watch.next_revision = Some(u64::MAX);
    let last = output(
        &mut watch,
        &identity,
        1,
        ms(0),
        WatchOutputDisposition::Quarantined,
    )
    .snapshot
    .next_deadline
    .unwrap();
    assert_eq!(last.revision, u64::MAX);
    assert_eq!(watch.next_revision, None);
    let update = output(
        &mut watch,
        &identity,
        2,
        ms(0),
        WatchOutputDisposition::Quarantined,
    );
    assert_eq!(update.outcome, WatchOutcome::RetiredOnDeadlineExhaustion);
    assert_eq!(
        update.snapshot,
        WatchSnapshot {
            rhythm: WatchRhythm::Steady,
            next_deadline: None
        }
    );
    let frozen = watch.clone();
    assert_eq!(
        watch.advance(
            Duration::MAX,
            &identity,
            WatchEvent::DeadlineDue { deadline: last }
        ),
        Err(WatchError::NotObserving)
    );
    assert_eq!(watch, frozen);
    let next = id("pane", 1, 2);
    watch.set_identity(Some(next.clone()));
    watch
        .advance(ms(0), &next, WatchEvent::AttachRunningObservation)
        .unwrap();
    assert_eq!(watch.deadline.unwrap().revision, 0);
}

#[test]
fn duration_max_has_no_fake_quiet_or_inclusive_busy_wakeup() {
    let mut watch = WatchObservation::default();
    let identity = id("pane", 1, 1);
    watch.set_identity(Some(identity.clone()));
    watch
        .advance(
            Duration::MAX - ms(1000),
            &identity,
            WatchEvent::BeginTrustedCommand,
        )
        .unwrap();
    assert_eq!(watch.deadline, None); // 3000ms of silence cannot fit in this timeline.
    for seq in 0..3 {
        output(
            &mut watch,
            &identity,
            seq,
            Duration::MAX,
            WatchOutputDisposition::Accepted,
        );
    }
    let snapshot = watch.snapshot(Duration::MAX, &identity).unwrap();
    assert_eq!(snapshot.rhythm, WatchRhythm::Busy);
    assert_eq!(snapshot.next_deadline, None);
    for _ in 0..3 {
        assert_eq!(
            watch
                .advance(Duration::MAX, &identity, WatchEvent::Advance)
                .unwrap()
                .snapshot,
            snapshot
        );
    }
}

#[test]
fn saturated_resumed_hold_expires_at_max_and_has_no_zero_time_loop() {
    let mut watch = WatchObservation::default();
    let identity = id("pane", 1, 1);
    watch.set_identity(Some(identity.clone()));
    watch
        .advance(
            Duration::MAX - ms(4000),
            &identity,
            WatchEvent::BeginTrustedCommand,
        )
        .unwrap();
    let update = output(
        &mut watch,
        &identity,
        1,
        Duration::MAX - ms(500),
        WatchOutputDisposition::Accepted,
    );
    assert_eq!(update.snapshot.rhythm, WatchRhythm::Resumed);
    let deadline = update.snapshot.next_deadline.unwrap();
    assert_eq!(deadline.at(), Duration::MAX);
    let end = watch
        .advance(
            Duration::MAX,
            &identity,
            WatchEvent::DeadlineDue { deadline },
        )
        .unwrap();
    assert_eq!(end.snapshot.rhythm, WatchRhythm::Steady);
    assert_eq!(end.snapshot.next_deadline, None);
    assert_eq!(
        watch.advance(
            Duration::MAX,
            &identity,
            WatchEvent::DeadlineDue { deadline }
        ),
        Err(WatchError::StaleDeadline)
    );
}

#[test]
fn context_replacement_preserves_monotonic_clock_and_rejects_backdated_start() {
    let (mut watch, identity) = started(false);
    watch
        .advance(ms(1000), &identity, WatchEvent::Advance)
        .unwrap();
    let next = id("pane", 2, 2);
    watch.set_identity(Some(next.clone()));
    let frozen = watch.clone();
    assert_eq!(
        watch.advance(ms(999), &next, WatchEvent::BeginTrustedCommand),
        Err(WatchError::StartTimeRegressed)
    );
    assert_eq!(watch, frozen);
    assert_eq!(
        watch
            .advance(ms(1000), &next, WatchEvent::AttachRunningObservation)
            .unwrap()
            .outcome,
        WatchOutcome::Started
    );
}

#[test]
fn normalized_event_trace_is_deterministic_independent_of_native_presentation() {
    // These are model instances, not a claim of executed frontend pipeline tests.
    let mut observations = [
        started(false),
        started(false),
        started(false),
        started(false),
    ];
    let trace = [
        (100, 0, WatchOutputDisposition::Accepted),
        (600, 1, WatchOutputDisposition::Accepted),
        (1300, 2, WatchOutputDisposition::Accepted),
        (5000, 3, WatchOutputDisposition::Accepted),
        (5100, 4, WatchOutputDisposition::Quarantined),
        (9000, 5, WatchOutputDisposition::Accepted),
        (12_000, 6, WatchOutputDisposition::Accepted),
    ];
    for (time, seq, disposition) in trace {
        let updates: Vec<_> = observations
            .iter_mut()
            .map(|(watch, identity)| output(watch, identity, seq, ms(time), disposition))
            .collect();
        assert!(updates.windows(2).all(|pair| pair[0] == pair[1]));
    }
}

#[test]
fn every_deadline_consumption_moves_strictly_forward_or_stops() {
    for attach in [false, true] {
        for times in [
            [0, 0, 0],
            [100, 600, 1300],
            [3000, 3100, 3200],
            [5000, 8000, 8000],
        ] {
            let (mut watch, identity) = started(attach);
            for (seq, time) in times.into_iter().enumerate() {
                pulse(&mut watch, &identity, seq as u64, time);
            }
            let mut consumed = 0;
            while let Some(deadline) = watch.deadline {
                let next = watch
                    .advance(
                        deadline.at(),
                        &identity,
                        WatchEvent::DeadlineDue { deadline },
                    )
                    .unwrap()
                    .snapshot
                    .next_deadline;
                assert!(next.is_none_or(|new| new.at() > deadline.at()));
                consumed += 1;
                assert!(
                    consumed <= 3,
                    "one finite trace must not schedule an endless poll"
                );
            }
            assert_eq!(rhythm(&watch, &identity, 100_000), WatchRhythm::Waiting);
        }
    }
}

#[test]
fn exact_max_quiet_boundary_and_unrepresentable_busy_successor_stop_scheduling() {
    let identity = id("pane", 1, 1);
    let mut quiet = WatchObservation::default();
    quiet.set_identity(Some(identity.clone()));
    quiet
        .advance(
            Duration::MAX - ms(3000),
            &identity,
            WatchEvent::BeginTrustedCommand,
        )
        .unwrap();
    let deadline = quiet.deadline.unwrap();
    assert_eq!(deadline.at(), Duration::MAX);
    let end = quiet
        .advance(
            Duration::MAX,
            &identity,
            WatchEvent::DeadlineDue { deadline },
        )
        .unwrap();
    assert_eq!(end.snapshot.rhythm, WatchRhythm::Waiting);
    assert_eq!(end.snapshot.next_deadline, None);

    let mut busy = WatchObservation::default();
    busy.set_identity(Some(identity.clone()));
    busy.advance(
        Duration::MAX - ms(2000),
        &identity,
        WatchEvent::BeginTrustedCommand,
    )
    .unwrap();
    for (seq, ago) in [1200, 600, 0].into_iter().enumerate() {
        output(
            &mut busy,
            &identity,
            seq as u64,
            Duration::MAX - ms(ago),
            WatchOutputDisposition::Accepted,
        );
    }
    assert_eq!(
        busy.outputs[0].unwrap().checked_add(BUSY_WINDOW),
        Some(Duration::MAX)
    );
    let end = busy
        .advance(Duration::MAX, &identity, WatchEvent::Advance)
        .unwrap();
    assert_eq!(end.snapshot.rhythm, WatchRhythm::Busy);
    assert_eq!(end.snapshot.next_deadline, None); // MAX + 1ns cannot be scheduled.
}

#[test]
fn repeated_identity_or_start_cannot_restart_or_extend_the_observation() {
    let (mut watch, identity) = started(true);
    let frozen = watch.clone();
    assert!(!watch.set_identity(Some(identity.clone())));
    for event in [
        WatchEvent::BeginTrustedCommand,
        WatchEvent::AttachRunningObservation,
    ] {
        assert_eq!(
            watch.advance(Duration::MAX, &identity, event),
            Err(WatchError::IdentityAlreadyStarted)
        );
        assert_eq!(watch, frozen);
    }
    watch.set_identity(None);
    let retired = watch.clone();
    assert_eq!(watch.deadline, None);
    assert_eq!(
        watch.snapshot(Duration::MAX, &identity),
        Err(WatchError::StaleIdentity)
    );
    assert_eq!(
        watch.advance(Duration::MAX, &identity, WatchEvent::Retire),
        Err(WatchError::StaleIdentity)
    );
    assert_eq!(watch, retired);
}

#[test]
fn output_at_max_does_not_start_a_zero_duration_resumed_hold() {
    let mut watch = WatchObservation::default();
    let identity = id("pane", 1, 1);
    watch.set_identity(Some(identity.clone()));
    watch
        .advance(
            Duration::MAX - ms(3000),
            &identity,
            WatchEvent::BeginTrustedCommand,
        )
        .unwrap();
    let update = output(
        &mut watch,
        &identity,
        0,
        Duration::MAX,
        WatchOutputDisposition::Accepted,
    );
    assert_eq!(update.snapshot.rhythm, WatchRhythm::Steady);
    assert_eq!(update.snapshot.next_deadline, None);
    assert_eq!(watch.resumed_until, None);
}
