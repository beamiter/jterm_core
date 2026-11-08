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
