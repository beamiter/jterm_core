use jterm_engine::grid::{Color, RawRowId};
use jterm_engine::screen::{AsciiWriteStep, RowMutation, ScreenState, VerticalMotion};
use jterm_engine::HyperlinkId;

fn seed(cols: usize, row: usize, col: usize, pending: bool, wide: bool) -> ScreenState {
    let mut s = ScreenState::new(3, cols);
    for (i, id) in s.grid.row_ids_mut().iter_mut().enumerate() {
        *id = RawRowId::new(i as u64 + 10);
    }
    s.current_fg = Color::Red;
    s.current_bg = Color::Blue;
    s.current_flags.set_bold(true);
    s.current_flags.set_protected(true);
    s.current_hyperlink = HyperlinkId::from_raw(17);
    if wide && cols >= 2 {
        s.write_char_in_row('界', 2, true);
    }
    s.cursor_row = row;
    s.cursor_col = col;
    s.pending_wrap = pending;
    s.last_printed_char = Some('Q');
    s
}
fn state(s: &ScreenState) -> String {
    format!(
        "{:?}",
        (
            s.grid.to_vec(),
            s.alt_grid.to_vec(),
            s.grid.row_ids(),
            &s.grid.row_wrapped,
            (
                s.cursor_row,
                s.cursor_col,
                s.pending_wrap,
                s.last_printed_char
            ),
            (
                s.current_fg,
                s.current_bg,
                s.current_flags,
                s.current_hyperlink
            ),
        )
    )
}
fn host_wrap(s: &mut ScreenState, next: &mut u64, trace: &mut Vec<String>) {
    let effect = s.vertical_motion(VerticalMotion::Wrap, true, || {
        let id = RawRowId::new(*next);
        *next += 1;
        id
    });
    trace.push(format!("wrap:{effect:?}:{}", state(s)));
}
fn host_row(s: &ScreenState, effect: RowMutation, trace: &mut Vec<String>) {
    trace.push(format!("row:{effect:?}:{}", state(s)));
}
// Explicit original orchestration, independent of the new step dispatcher.
fn old(s: &mut ScreenState, bytes: &[u8], awm: bool) -> Vec<String> {
    let (mut pos, mut next, mut trace) = (0, 100, Vec::new());
    while pos < bytes.len() {
        if s.take_pending_wrap(awm) {
            host_wrap(s, &mut next, &mut trace);
        }
        let Some((count, mutation)) = s.write_ascii_in_row(&bytes[pos..], awm) else {
            break;
        };
        host_row(s, mutation, &mut trace);
        pos += count;
    }
    trace.push(format!("done:{pos}:{}", state(s)));
    trace
}
fn candidate(s: &mut ScreenState, bytes: &[u8], awm: bool) -> Vec<String> {
    let (mut pos, mut next, mut trace) = (0, 100, Vec::new());
    for _ in 0..=(bytes.len() * 2 + 1) {
        match s.step_ascii_write(&bytes[pos..], awm) {
            AsciiWriteStep::NeedsWrap => host_wrap(s, &mut next, &mut trace),
            AsciiWriteStep::Written { consumed, mutation } => {
                assert!(consumed > 0 && consumed <= bytes.len() - pos);
                host_row(s, mutation, &mut trace);
                pos += consumed;
            }
            AsciiWriteStep::Stopped => {
                trace.push(format!("done:{pos}:{}", state(s)));
                return trace;
            }
        }
    }
    panic!("no progress");
}
#[test]
fn explicit_original_orchestration_matches_each_immediate_host_checkpoint() {
    for cols in [1, 2, 6, 17, 80] {
        for row in [0, 2] {
            for col in [0, cols - 1] {
                for pending in [false, true] {
                    for awm in [false, true] {
                        for n in [0, 1, cols - 1, cols, cols + 1, 2 * cols, 3 * cols + 1] {
                            let bytes: Vec<_> = (0..n).map(|i| 0x20 + (i % 95) as u8).collect();
                            let mut a = seed(cols, row, col, pending, true);
                            let mut b = seed(cols, row, col, pending, true);
                            assert_eq!(
                                old(&mut a, &bytes, awm),
                                candidate(&mut b, &bytes, awm),
                                "cols={cols} row={row} col={col} pending={pending} awm={awm} n={n}"
                            );
                        }
                    }
                }
            }
        }
    }
}
#[test]
fn empty_and_nonprogress_preserve_original_latch_contract() {
    for awm in [false, true] {
        let mut s = seed(2, 0, 0, true, false);
        let before = state(&s);
        assert_eq!(s.step_ascii_write(b"", awm), AsciiWriteStep::Stopped);
        assert_eq!(state(&s), before);
    }
    // Do not clamp an out-of-range column into a new write: retain original None.
    let mut s = seed(2, 0, 2, true, false);
    assert_eq!(s.step_ascii_write(b"A", false), AsciiWriteStep::Stopped);
    assert!(!s.pending_wrap);
    assert_eq!(s.cursor_col, 2);
    assert_eq!(s.last_printed_char, Some('Q'));
    s.pending_wrap = true;
    assert_eq!(s.step_ascii_write(b"A", true), AsciiWriteStep::NeedsWrap);
    assert!(!s.pending_wrap);
    assert_eq!(s.last_printed_char, Some('Q'));
}
#[test]
fn off_at_last_column_keeps_one_byte_batches_and_rep_source() {
    let mut s = seed(1, 0, 0, true, false);
    for byte in b"ABC" {
        let AsciiWriteStep::Written { consumed, mutation } =
            s.step_ascii_write(&[*byte, b'Z'], false)
        else {
            panic!()
        };
        assert_eq!(consumed, 1);
        assert_eq!(mutation.printed, Some(0..1));
        assert_eq!(s.last_printed_char, Some(*byte as char));
        assert_eq!(s.cursor_col, 0);
        assert!(!s.pending_wrap);
    }
}
#[test]
fn wide_collateral_erasure_is_not_printed_output() {
    let mut s = seed(6, 0, 1, false, true);
    let AsciiWriteStep::Written { consumed, mutation } = s.step_ascii_write(b"A", true) else {
        panic!()
    };
    assert_eq!(consumed, 1);
    assert_eq!(mutation.changed, 0..2);
    assert_eq!(mutation.printed, Some(1..2));
    assert_eq!(s.grid[0][0].character, ' ');
    assert_eq!(s.grid[0][1].character, 'A');
    assert!(!s.grid[0][1].flags.wide_continuation());
    assert!(s.grid[0][1].flags.protected());
    assert_eq!(s.grid[0][1].hyperlink_id, HyperlinkId::from_raw(17));
    assert_eq!(s.last_printed_char, Some('A'));
}
