use jterm_engine::{
    frame::*,
    grid::{CursorShape, TerminalCell},
    publication::{self, Sequence, Subscriber, Update},
};
fn frame(epoch: u64, revision: u64) -> CellFrame {
    CellFrame::new(
        Stamp {
            epoch,
            revision,
            layout: 1,
            screen: Screen::Primary,
        },
        1,
        vec![Row {
            cells: vec![TerminalCell::default()].into(),
            wrapped: false,
        }]
        .into(),
        Cursor {
            row: 0,
            col: 0,
            pending_wrap: false,
            visible: true,
            shape: CursorShape::Block,
        },
        Limits {
            max_rows: 1,
            max_cols: 1,
            max_cells: 1,
        },
    )
    .unwrap()
}
#[test]
fn stalled_subscriber_is_bounded_and_latest_wins() {
    let mut s = Subscriber::new(1).unwrap();
    s.offer(frame(1, 1)).unwrap();
    let first = s.take().unwrap();
    for revision in 2..10000 {
        s.offer(frame(1, revision)).unwrap();
        assert!(s.retained_frames() <= 3);
        assert!(s.take().is_none());
    }
    s.acknowledge(first.stamp()).unwrap();
    match s.take().unwrap() {
        Update::Delta(d) => {
            assert_eq!(d.base.revision, 1);
            assert_eq!(d.next.revision, 9999);
        }
        _ => panic!("expected exact-base delta"),
    }
}
#[test]
fn two_subscribers_advance_independently() {
    let mut slow = Subscriber::new(1).unwrap();
    let mut fast = Subscriber::new(1).unwrap();
    let mut slow_first = None;
    for revision in 1..50 {
        let f = frame(1, revision);
        slow.offer(f.clone()).unwrap();
        fast.offer(f).unwrap();
        let update = fast.take().unwrap();
        fast.acknowledge(update.stamp()).unwrap();
        if revision == 1 {
            slow_first = slow.take();
        }
    }
    assert!(slow.take().is_none());
    slow.acknowledge(slow_first.unwrap().stamp()).unwrap();
    assert_eq!(slow.take().unwrap().stamp().revision, 49);
}
#[test]
fn wrong_ack_and_epoch_are_atomic() {
    let mut s = Subscriber::new(1).unwrap();
    s.offer(frame(1, 1)).unwrap();
    let first = s.take().unwrap();
    let count = s.retained_frames();
    assert_eq!(
        s.acknowledge(frame(2, 1).stamp()),
        Err(publication::Error::Ack)
    );
    assert_eq!(
        s.acknowledge(frame(1, 2).stamp()),
        Err(publication::Error::Ack)
    );
    assert_eq!(s.offer(frame(2, 2)), Err(publication::Error::Epoch));
    assert_eq!(s.retained_frames(), count);
    assert!(s.take().is_none());
    s.acknowledge(first.stamp()).unwrap();
    assert_eq!(s.acknowledge(first.stamp()), Err(publication::Error::Ack));
    assert_eq!(s.offer(frame(1, 1)), Err(publication::Error::Stale));
}
#[test]
fn rejected_delta_yields_new_full_without_old_base() {
    let mut s = Subscriber::new(1).unwrap();
    s.offer(frame(1, 1)).unwrap();
    let first = s.take().unwrap();
    s.acknowledge(first.stamp()).unwrap();
    s.offer(frame(1, 2)).unwrap();
    let second = s.take().unwrap();
    assert!(matches!(second, Update::Delta(_)));
    s.offer(frame(1, 3)).unwrap();
    s.reject(second.stamp()).unwrap();
    assert!(matches!(s.take(), Some(Update::Full(_))));
}
#[test]
fn sequence_exhaustion_never_wraps_and_new_epoch_is_explicit() {
    let mut seq = Sequence::after(7, u64::MAX - 1).unwrap();
    assert_eq!(seq.next_revision(), Ok((7, u64::MAX)));
    assert_eq!(seq.next_revision(), Err(publication::Error::Exhausted));
    assert_eq!(seq.next_revision(), Err(publication::Error::Exhausted));
    let mut new = Sequence::new(8).unwrap();
    assert_eq!(new.next_revision(), Ok((8, 1)));
    assert!(Sequence::new(0).is_err());
}

#[test]
fn ack_and_reject_require_exact_layout_and_screen_even_at_same_revision() {
    let mut subscriber = Subscriber::new(1).unwrap();
    subscriber.offer(frame(1, 1)).unwrap();
    let sent = subscriber.take().unwrap().stamp();
    subscriber.offer(frame(1, 2)).unwrap();
    for attempted in [
        Stamp { layout: 2, ..sent },
        Stamp {
            screen: Screen::Alternate,
            ..sent
        },
        Stamp { epoch: 0, ..sent },
        Stamp {
            revision: 0,
            ..sent
        },
        Stamp { layout: 0, ..sent },
    ] {
        assert_eq!(
            subscriber.acknowledge(attempted),
            Err(publication::Error::Ack)
        );
        assert_eq!(subscriber.reject(attempted), Err(publication::Error::Ack));
        assert_eq!(subscriber.retained_frames(), 2);
        assert!(subscriber.take().is_none());
    }
    subscriber.acknowledge(sent).unwrap();
    match subscriber.take().unwrap() {
        Update::Delta(delta) => {
            assert_eq!(delta.base, sent);
            assert_eq!(delta.next.revision, 2);
        }
        _ => panic!("exact ACK should retain the originally applied baseline"),
    }
}

fn sized_frame(revision: u64, layout: u64, cols: usize, rows: usize) -> CellFrame {
    let original = frame(1, revision);
    CellFrame::new(
        Stamp {
            layout,
            ..original.stamp()
        },
        cols,
        vec![
            Row {
                cells: vec![TerminalCell::default(); cols].into(),
                wrapped: false,
            };
            rows
        ]
        .into(),
        original.cursor(),
        Limits {
            max_rows: 2,
            max_cols: 2,
            max_cells: 4,
        },
    )
    .unwrap()
}

#[test]
fn unchanged_layout_rejects_resize_without_losing_pending_frame() {
    for (cols, rows) in [(2, 1), (1, 2)] {
        let mut subscriber = Subscriber::new(1).unwrap();
        let mut receiver = Receiver::new(1);
        let first = frame(1, 1);
        subscriber.offer(first.clone()).unwrap();
        subscriber.take().unwrap();
        receiver.full(first.clone()).unwrap();
        subscriber.acknowledge(first.stamp()).unwrap();
        subscriber.offer(frame(1, 2)).unwrap();
        let invalid = sized_frame(3, 1, cols, rows);
        assert_eq!(receiver.full(invalid.clone()), Err(Error::Layout));
        assert_eq!(subscriber.offer(invalid), Err(publication::Error::Layout));
        assert_eq!(subscriber.retained_frames(), 2);
        let Update::Delta(delta) = subscriber.take().unwrap() else {
            panic!("valid pending publication must survive rejection");
        };
        assert_eq!(delta.next.revision, 2);
        receiver
            .delta(
                &delta,
                Limits {
                    max_rows: 2,
                    max_cols: 2,
                    max_cells: 4,
                },
            )
            .unwrap();
        subscriber.acknowledge(delta.next).unwrap();
        // Rejection must not consume revision 3. A real layout advance is valid.
        let resized = sized_frame(3, 2, cols, rows);
        subscriber.offer(resized.clone()).unwrap();
        let Update::Full(full) = subscriber.take().unwrap() else {
            panic!("resize requires a full frame");
        };
        receiver.full(full).unwrap();
        assert_eq!(receiver.frame(), Some(&resized));
    }
}

#[test]
fn layout_dimensions_survive_flight_rejection_and_coalescing() {
    let mut subscriber = Subscriber::new(1).unwrap();
    subscriber.offer(frame(1, 1)).unwrap();
    let first = subscriber.take().unwrap();
    subscriber.reject(first.stamp()).unwrap();
    assert_eq!(subscriber.retained_frames(), 0);
    assert_eq!(
        subscriber.offer(sized_frame(2, 1, 2, 1)),
        Err(publication::Error::Layout)
    );
    subscriber.offer(sized_frame(2, 2, 2, 1)).unwrap();
    // Compare against the latest offer, even before it is sent or acknowledged.
    assert_eq!(
        subscriber.offer(sized_frame(3, 2, 1, 1)),
        Err(publication::Error::Layout)
    );
    assert_eq!(subscriber.take().unwrap().stamp().revision, 2);
}
