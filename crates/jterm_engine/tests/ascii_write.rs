use jterm_engine::grid::{AsciiOverwritePolicy, Color, RawRowId, TerminalCell, TerminalGrid};
use jterm_engine::screen::{AsciiWriteStep, RowMutation, ScreenState, VerticalMotion};
use jterm_engine::terminal::Terminal;
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
fn ascii_rep_matches_scalar_output_in_live_and_retained_rows() {
    let cases: [(&[u8], u8, &str); 4] = [
        (b"\x1b[3;4H", b'#', "\u{fe0f}\u{20e3}"),
        (b"\x1b[?7l\x1b[2;4H", b' ', ""),
        (b"\x1b[?1049h\x1b[2;3r\x1b[3;4H", b'X', ""),
        (b"\x1b[4h\x1b[2;4H", b'X', ""),
    ];
    for (setup, byte, suffix) in cases {
        let seed = || {
            let mut terminal = Terminal::new(3, 8).unwrap();
            terminal.enable_grapheme_clusters();
            terminal.enable_underline_colors();
            terminal.enable_hyperlinks(|_, _| true);
            terminal.track_printed_spaces();
            terminal.feed("old\r\nxx界tail".as_bytes()).unwrap();
            terminal.feed(setup).unwrap();
            terminal
                .feed(b"\x1b[31;44;4:3;58;5;33m\x1b]8;;https://example.test/rep\x07")
                .unwrap();
            terminal
        };
        let mut literal = seed();
        for _ in 0..21 {
            literal.feed(&[byte]).unwrap();
        }
        literal.feed(suffix.as_bytes()).unwrap();
        let baseline = literal.frame().unwrap();
        let expected_history = literal.history_page(0, 8).unwrap();
        let mut request = vec![byte];
        request.extend_from_slice(b"\x1b[20b");
        request.extend_from_slice(suffix.as_bytes());
        for split in 0..=request.len() {
            let mut repeated = seed();
            repeated.feed(&request[..split]).unwrap();
            repeated.feed(&request[split..]).unwrap();
            let frame = repeated.frame().unwrap();
            assert_eq!(frame.rows(), baseline.rows());
            assert_eq!(frame.cursor(), baseline.cursor());
            assert_eq!(repeated.visible_text(), literal.visible_text());
            assert_eq!(repeated.unsupported_count(), literal.unsupported_count());
            for row in 0..3 {
                assert_eq!(
                    frame.underline_color_runs(row),
                    baseline.underline_color_runs(row)
                );
                for col in 0..8 {
                    assert_eq!(
                        frame.cluster_text(row, col),
                        baseline.cluster_text(row, col)
                    );
                }
            }
            assert_eq!(repeated.history_extent(), literal.history_extent());
            let history = repeated.history_page(0, 8).unwrap();
            assert_eq!(history.text, expected_history.text);
            for row in 0..history.row_count {
                assert_eq!(history.row(row), expected_history.row(row));
                assert_eq!(
                    history.underline_color_runs(row),
                    expected_history.underline_color_runs(row)
                );
            }
        }
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

#[test]
fn frost_row_overwrite_repairs_both_touched_wide_pairs_and_applies_the_pen() {
    let mut grid = TerminalGrid::new(1, 6);
    grid[0][0].character = 'L';
    grid[0][5].character = 'R';
    for col in [1, 3] {
        grid[0][col].character = '界';
        grid[0][col].flags.set_wide(true);
        grid[0][col + 1].flags.set_wide_continuation(true);
    }
    let blank = TerminalCell {
        background: Color::Blue,
        ..TerminalCell::default()
    };
    let mut pen = TerminalCell {
        foreground: Color::Red,
        background: Color::Blue,
        hyperlink_id: HyperlinkId::from_raw(17),
        ..TerminalCell::default()
    };
    pen.flags.set_bold(true);
    pen.flags.set_wide(true);
    pen.flags.set_wide_continuation(true);
    grid.overwrite_ascii(0, 2, b"AB", pen, blank, AsciiOverwritePolicy::FrostLegacy);
    assert_eq!(grid[0][0].character, 'L');
    assert_eq!(grid[0][5].character, 'R');
    assert_eq!(grid[0][1], blank);
    assert_eq!(grid[0][4], blank);
    pen.flags.set_wide(false);
    pen.flags.set_wide_continuation(false);
    assert_eq!(
        grid[0][2],
        TerminalCell {
            character: 'A',
            ..pen
        }
    );
    assert_eq!(
        grid[0][3],
        TerminalCell {
            character: 'B',
            ..pen
        }
    );
}

#[test]
fn legacy_and_shared_overwrites_read_different_edges_of_existing_split_pairs() {
    let mut original = TerminalGrid::new(1, 6);
    original[0][1].character = '界';
    original[0][1].flags.set_wide(true);
    original[0][4].flags.set_wide_continuation(true);
    for (policy, repairs) in [
        (AsciiOverwritePolicy::FrostLegacy, false),
        (AsciiOverwritePolicy::SharedScreen, true),
    ] {
        let mut grid = original.clone();
        let blank = TerminalCell::default();
        grid.overwrite_ascii(0, 2, b"AB", blank, blank, policy);
        assert_eq!(grid[0][1] == blank, repairs);
        assert_eq!(grid[0][4] == blank, repairs);
        assert_eq!(grid[0][2].character, 'A');
        assert_eq!(grid[0][3].character, 'B');
    }
}
