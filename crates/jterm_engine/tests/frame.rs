use jterm_engine::{
    frame::*,
    grid::{CursorShape, TerminalCell},
};
const LIMITS: Limits = Limits {
    max_rows: 20,
    max_cols: 80,
    max_cells: 1600,
};
fn frame(revision: u64) -> CellFrame {
    CellFrame::new(
        Stamp {
            epoch: 1,
            revision,
            layout: 1,
            screen: Screen::Primary,
        },
        4,
        vec![
            Row {
                cells: vec![TerminalCell::default(); 4].into(),
                wrapped: false
            };
            3
        ]
        .into(),
        Cursor {
            row: 0,
            col: 0,
            pending_wrap: false,
            visible: true,
            shape: CursorShape::Block,
        },
        LIMITS,
    )
    .unwrap()
}
fn modify(base: &CellFrame, revision: u64, rows: Vec<Row>, cursor: Cursor) -> CellFrame {
    CellFrame::new(
        Stamp {
            revision,
            ..base.stamp()
        },
        base.cols(),
        rows.into(),
        cursor,
        LIMITS,
    )
    .unwrap()
}
#[test]
fn full_and_delta_equal_with_shared_unchanged_rows() {
    let a = frame(1);
    let mut rows = a.rows().to_vec();
    let mut cells = rows[1].cells.to_vec();
    cells[0].character = '字';
    rows[1].cells = cells.into();
    rows[1].wrapped = true;
    let b = modify(&a, 2, rows, a.cursor());
    let delta = a.delta_to(&b).unwrap();
    assert_eq!(delta.rows.len(), 1);
    let actual = a.apply(&delta, LIMITS).unwrap();
    assert_eq!(actual, b);
    assert!(std::sync::Arc::ptr_eq(
        &a.rows()[0].cells,
        &actual.rows()[0].cells
    ));
}
#[test]
fn cursor_only_and_pending_wrap_preserved() {
    let a = frame(1);
    let cursor = Cursor {
        col: 3,
        pending_wrap: true,
        shape: CursorShape::Beam,
        ..a.cursor()
    };
    let b = modify(&a, 2, a.rows().to_vec(), cursor);
    let d = a.delta_to(&b).unwrap();
    assert!(d.rows.is_empty());
    assert_eq!(a.apply(&d, LIMITS).unwrap(), b);
    assert_eq!(cursor.paint_position(), (0, 3));
}
#[test]
fn dropped_reordered_and_duplicate_deltas_do_not_mutate() {
    let a = frame(1);
    let b = frame(2);
    let c = frame(3);
    let ab = a.delta_to(&b).unwrap();
    let bc = b.delta_to(&c).unwrap();
    let mut r = Receiver::new(1);
    r.full(a.clone()).unwrap();
    assert_eq!(r.delta(&bc, LIMITS), Err(Error::Base));
    assert_eq!(r.frame(), Some(&a));
    r.delta(&ab, LIMITS).unwrap();
    assert_eq!(r.delta(&ab, LIMITS), Err(Error::Base));
    r.delta(&bc, LIMITS).unwrap();
    assert_eq!(r.frame(), Some(&c));
    assert_eq!(r.full(b), Err(Error::Revision));
}
#[test]
fn malformed_patch_rejected_atomically() {
    let a = frame(1);
    let mut d = a.delta_to(&frame(2)).unwrap();
    d.rows = vec![(0, a.rows()[0].clone()), (0, a.rows()[1].clone())];
    assert_eq!(a.apply(&d, LIMITS), Err(Error::Duplicate));
    d.rows = vec![(99, a.rows()[0].clone())];
    assert_eq!(a.apply(&d, LIMITS), Err(Error::Bounds));
    d.rows.clear();
    d.cursor.col = 4;
    assert_eq!(a.apply(&d, LIMITS), Err(Error::Cursor));
    assert_eq!(a, frame(1));
}
#[test]
fn resize_and_screen_transition_require_full() {
    let a = frame(1);
    let resized = CellFrame::new(
        Stamp {
            revision: 2,
            layout: 2,
            ..a.stamp()
        },
        4,
        vec![a.rows()[0].clone()].into(),
        a.cursor(),
        LIMITS,
    )
    .unwrap();
    assert_eq!(a.delta_to(&resized).unwrap_err(), Error::Layout);
    let alt = CellFrame::new(
        Stamp {
            revision: 3,
            screen: Screen::Alternate,
            ..resized.stamp()
        },
        4,
        resized.rows().to_vec().into(),
        a.cursor(),
        LIMITS,
    )
    .unwrap();
    assert_eq!(resized.delta_to(&alt).unwrap_err(), Error::Layout);
    let mut r = Receiver::new(1);
    r.full(a).unwrap();
    r.full(resized).unwrap();
    r.full(alt).unwrap();
}
#[test]
fn rollover_requires_explicit_new_epoch() {
    let a = frame(u64::MAX);
    assert_eq!(a.delta_to(&frame(1)).unwrap_err(), Error::Revision);
    let mut r = Receiver::new(2);
    assert_eq!(r.full(frame(1)), Err(Error::Revision));
    let next = CellFrame::new(
        Stamp {
            epoch: 2,
            revision: 1,
            ..a.stamp()
        },
        4,
        a.rows().to_vec().into(),
        a.cursor(),
        LIMITS,
    )
    .unwrap();
    r.full(next).unwrap();
    assert_eq!(r.frame().unwrap().stamp().revision, 1);
}
#[test]
fn limits_reject_invalid_dimensions_and_widths() {
    let a = frame(1);
    assert_eq!(
        CellFrame::new(a.stamp(), 0, a.rows().to_vec().into(), a.cursor(), LIMITS),
        Err(Error::Bounds)
    );
    assert_eq!(
        CellFrame::new(a.stamp(), 5, a.rows().to_vec().into(), a.cursor(), LIMITS),
        Err(Error::Bounds)
    );
    assert_eq!(
        CellFrame::new(
            a.stamp(),
            4,
            a.rows().to_vec().into(),
            a.cursor(),
            Limits {
                max_cells: 11,
                ..LIMITS
            }
        ),
        Err(Error::Bounds)
    );
}
#[test]
fn blank_row_replacement_clears_wide_cells_exactly() {
    let a = frame(1);
    let mut rows = a.rows().to_vec();
    let mut cells = rows[0].cells.to_vec();
    cells[0].character = '界';
    cells[0].flags.set_wide(true);
    cells[1].flags.set_wide_continuation(true);
    rows[0].cells = cells.into();
    let wide = modify(&a, 2, rows, a.cursor());
    let blank = frame(3);
    let d = wide.delta_to(&blank).unwrap();
    assert_eq!(wide.apply(&d, LIMITS).unwrap(), blank);
}
