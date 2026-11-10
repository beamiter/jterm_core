//! Independent physical oracle for the published Ember vertical algorithms.
//! LF/RI/wrap control flow is frozen from baseline ember/src/terminal/
//! vertical_motion_frozen.rs (SHA256:
//! bcaccc480b25d4caeee16d80007dea071795c969abfa6e47e64d13fa45f760cb).
//! Region shifts below use literal cell copies and
//! metadata movement, never ScreenState region/vertical helpers.
use jterm_engine::grid::{Color, CursorShape, RawRowId, ScrollbackLine, StyleFlags, TerminalCell};
use jterm_engine::protocol_state::ProtocolState;
use jterm_engine::screen::{
    Charset, RegionDirection, ScreenState, VerticalMotion, VerticalMutation,
};
use jterm_engine::HyperlinkId;

fn seeded(rows: usize, cols: usize, variant: usize) -> (ScreenState, ProtocolState) {
    let mut s = ScreenState::new(rows, cols);
    for (buffer, grid) in [&mut s.grid, &mut s.alt_grid].into_iter().enumerate() {
        for row in 0..rows {
            grid.row_ids_mut()[row] = if row == 0 {
                RawRowId::UNTRACKED
            } else {
                RawRowId::new((buffer * 100 + row) as u64)
            };
            grid.row_wrapped[row] = (buffer + row) % 2 == 0;
            for col in 0..cols {
                let cell = grid.get_mut(row, col);
                cell.character = char::from(b'A' + (buffer * 10 + row) as u8);
                cell.background = Color::Green;
                cell.flags.set_protected(true);
                cell.hyperlink_id = HyperlinkId::from_raw(12);
            }
            if cols > 1 {
                grid[row][cols - 2].flags.set_wide(true);
                grid[row][cols - 1].flags.set_wide_continuation(true);
            }
        }
    }
    s.cursor_row = rows - 1;
    s.cursor_col = cols - 1;
    s.alt_cursor_row = 0;
    s.alt_cursor_col = 0;
    s.saved_cursor_row = 0;
    s.saved_cursor_col = 0;
    s.cursor_shape = CursorShape::Underline;
    s.current_fg = Color::Red;
    s.current_bg = Color::Blue;
    s.current_flags.set_protected(true);
    s.current_hyperlink = HyperlinkId::from_raw(99);
    s.g1_charset = Charset::DecSpecialGraphics;
    s.active_charset = Charset::DecSpecialGraphics;
    s.origin_mode = true;
    s.insert_mode = true;
    s.last_printed_char = Some('界');
    s.pending_wrap = true;
    s.scroll_region_top = 1.min(rows - 1);
    s.scroll_region_bottom = rows - 1;
    s.tab_stops.fill(false);
    let mut p = ProtocolState::default();
    if variant % 2 == 0 {
        p.modes_mut().insert(7);
    }
    p.push_keyboard_enhancement_flags(3);
    p.switch_screen();
    p.push_keyboard_enhancement_flags(8);
    p.switch_screen();
    s.save_cursor_state(&p);
    if variant >= 2 {
        s.saved_primary_screen_state = s.saved_state.clone();
        s.saved_primary_cursor_shape = Some(CursorShape::Block);
        s.use_alt_buffer = variant == 3;
    }
    (s, p)
}

fn snapshot(s: &ScreenState) -> String {
    format!(
        "{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
        (
            s.grid.row_ids(),
            &s.grid.row_wrapped,
            s.alt_grid.row_ids(),
            &s.alt_grid.row_wrapped
        ),
        (
            s.cursor_row,
            s.cursor_col,
            s.saved_cursor_row,
            s.saved_cursor_col,
            s.alt_cursor_row,
            s.alt_cursor_col
        ),
        (s.cursor_shape, s.saved_primary_cursor_shape),
        (&s.saved_state, &s.saved_primary_screen_state),
        (
            s.current_fg,
            s.current_bg,
            s.current_flags,
            s.current_hyperlink,
        ),
        (s.g0_charset, s.g1_charset, s.active_charset),
        (
            s.use_alt_buffer,
            s.origin_mode,
            s.insert_mode,
            s.pending_wrap
        ),
        (s.scroll_region_top, s.scroll_region_bottom),
        (&s.tab_stops, s.last_printed_char),
        (
            s.grid.to_vec(),
            s.alt_grid.to_vec(),
            s.grid.rows(),
            s.grid.cols(),
            s.alt_grid.rows(),
            s.alt_grid.cols()
        )
    )
}

#[derive(Debug)]
struct OriginalRegion {
    direction: RegionDirection,
    top: usize,
    bottom: usize,
    removed_row: usize,
    removed_id: RawRowId,
    captured: Option<ScrollbackLine>,
}

// Frozen physical scroll: the host formerly copied cells/wrap flags and moved
// raw IDs, then blanked the vacancy. Capture is before any movement, including
// the freshly set wrapped bit when a one-row wrap removes its own departure.
fn original_scroll(
    s: &mut ScreenState,
    up: bool,
    capture: bool,
    calls: &mut usize,
) -> Option<OriginalRegion> {
    let top = s.scroll_region_top;
    let bottom = s.scroll_region_bottom;
    if bottom >= s.grid.rows() || top > bottom {
        return None;
    }
    let removed_row = if up { top } else { bottom };
    let effect = OriginalRegion {
        direction: if up {
            RegionDirection::Up
        } else {
            RegionDirection::Down
        },
        top,
        bottom,
        removed_row,
        removed_id: s.grid.row_id(removed_row),
        captured: capture.then(|| {
            // Match the original history storage contract, not raw live cells:
            // legacy compression drops protected and blink flags. It retains
            // wide-cell metadata and reconstructs trimmed trailing blanks.
            let mut line =
                ScrollbackLine::compress(&s.grid[removed_row], s.grid.row_wrapped[removed_row]);
            line.set_raw_row_id(s.grid.row_id(removed_row));
            line
        }),
    };
    if up {
        for row in top..bottom {
            for col in 0..s.grid.cols() {
                let cell = *s.grid.get(row + 1, col);
                *s.grid.get_mut(row, col) = cell;
            }
            s.grid.row_wrapped[row] = s.grid.row_wrapped[row + 1];
            s.grid.row_ids_mut()[row] = s.grid.row_id(row + 1);
        }
    } else {
        for row in (top + 1..=bottom).rev() {
            for col in 0..s.grid.cols() {
                let cell = *s.grid.get(row - 1, col);
                *s.grid.get_mut(row, col) = cell;
            }
            s.grid.row_wrapped[row] = s.grid.row_wrapped[row - 1];
            s.grid.row_ids_mut()[row] = s.grid.row_id(row - 1);
        }
    }
    let vacancy = if up { bottom } else { top };
    for col in 0..s.grid.cols() {
        *s.grid.get_mut(vacancy, col) = TerminalCell {
            character: ' ',
            foreground: Color::Default,
            background: s.current_bg,
            flags: StyleFlags::default(),
            hyperlink_id: HyperlinkId::NONE,
        };
    }
    s.grid.row_wrapped[vacancy] = false;
    s.grid.row_ids_mut()[vacancy] = RawRowId::new(1000 + *calls as u64);
    *calls += 1;
    Some(effect)
}

fn original_lf(s: &mut ScreenState, capture: bool, calls: &mut usize) -> Option<OriginalRegion> {
    s.pending_wrap = false;
    if s.cursor_row == s.scroll_region_bottom {
        original_scroll(s, true, capture, calls)
    } else {
        if s.cursor_row + 1 < s.grid.rows() {
            s.cursor_row += 1;
        }
        None
    }
}

fn original_wrap(s: &mut ScreenState, capture: bool, calls: &mut usize) -> Option<OriginalRegion> {
    s.grid.row_wrapped[s.cursor_row] = true;
    s.cursor_col = 0;
    if s.cursor_row == s.scroll_region_bottom {
        original_scroll(s, true, capture, calls)
    } else {
        if s.cursor_row + 1 < s.grid.rows() {
            s.cursor_row += 1;
        }
        None
    }
}

fn original_ri(s: &mut ScreenState, calls: &mut usize) -> Option<OriginalRegion> {
    s.pending_wrap = false;
    if s.cursor_row == s.scroll_region_top {
        if s.scroll_region_bottom < s.grid.rows() && s.scroll_region_top <= s.scroll_region_bottom {
            return original_scroll(s, false, false, calls);
        }
    } else {
        s.cursor_row = s.cursor_row.saturating_sub(1);
    }
    None
}

fn assert_effect(
    actual: &VerticalMutation,
    expected: &Option<OriginalRegion>,
    departure: Option<(RawRowId, usize, usize)>,
    context: &str,
) {
    assert_eq!(
        actual.departure.map(|d| (d.row_id, d.row, d.width)),
        departure,
        "departure: {context}"
    );
    match (&actual.region, expected) {
        (None, None) => (),
        (Some(a), Some(e)) => {
            assert_eq!(
                (a.direction, a.top, a.bottom, a.count),
                (e.direction, e.top, e.bottom, 1),
                "region: {context}"
            );
            assert_eq!(a.dirty_rows, e.top..e.bottom + 1, "dirty: {context}");
            assert_eq!(a.removed.len(), 1, "removed count: {context}");
            let removed = &a.removed[0];
            assert_eq!(
                (removed.original_row, removed.row_id),
                (e.removed_row, e.removed_id),
                "identity: {context}"
            );
            match (&removed.captured, &e.captured) {
                (None, None) => (),
                (Some(a), Some(line)) => {
                    assert_eq!(a.raw_row_id(), e.removed_id, "captured ID: {context}");
                    assert_eq!(a.columns(), line.columns(), "captured width: {context}");
                    assert_eq!(
                        a.decompress(),
                        line.decompress(),
                        "captured cells: {context}"
                    );
                    assert_eq!(a.is_wrapped, line.is_wrapped, "captured wrap: {context}");
                    assert_eq!(
                        a.reflow_content_len(),
                        line.reflow_content_len(),
                        "captured reflow length: {context}"
                    );
                    assert_eq!(
                        a.reflow_wide_continuations(),
                        line.reflow_wide_continuations(),
                        "captured wide metadata: {context}"
                    );
                }
                _ => panic!("capture mismatch: {context}"),
            }
        }
        _ => panic!("region presence mismatch: {context}"),
    }
}

#[test]
fn vertical_motion_matches_frozen_physical_algorithms_exhaustively() {
    for rows in 1..=6 {
        for cols in 1..=4 {
            // Full Cartesian margins include valid, inverted and just-outside
            // bounds, with MAX pairs added to catch overflow-prone guards.
            let margins = (0..=rows)
                .flat_map(|top| (0..=rows).map(move |bottom| (top, bottom)))
                .chain([(0, usize::MAX), (usize::MAX, 0), (usize::MAX, usize::MAX)]);
            for (top, bottom) in margins {
                for row in 0..rows {
                    for pending in [false, true] {
                        for capture in [false, true] {
                            for motion in [
                                VerticalMotion::LineFeed,
                                VerticalMotion::ReverseIndex,
                                VerticalMotion::Wrap,
                            ] {
                                // Alternate and primary states both retain their
                                // inactive grid and all saved/drawing metadata.
                                for variant in [0, 3] {
                                    let (mut actual, _) = seeded(rows, cols, variant);
                                    let (mut expected, _) = seeded(rows, cols, variant);
                                    for s in [&mut actual, &mut expected] {
                                        s.cursor_row = row;
                                        s.pending_wrap = pending;
                                        s.scroll_region_top = top;
                                        s.scroll_region_bottom = bottom;
                                    }
                                    let departure = if motion == VerticalMotion::ReverseIndex {
                                        None
                                    } else {
                                        Some((expected.grid.row_id(row), row, cols))
                                    };
                                    let context = format!("{rows}x{cols} row={row} margins={top}..{bottom} pending={pending} capture={capture} {motion:?} variant={variant}");
                                    let mut expected_calls = 0;
                                    let region = match motion {
                                        VerticalMotion::LineFeed => {
                                            original_lf(&mut expected, capture, &mut expected_calls)
                                        }
                                        VerticalMotion::ReverseIndex => {
                                            original_ri(&mut expected, &mut expected_calls)
                                        }
                                        VerticalMotion::Wrap => original_wrap(
                                            &mut expected,
                                            capture,
                                            &mut expected_calls,
                                        ),
                                    };
                                    let mut actual_calls = 0;
                                    let effect = actual.vertical_motion(motion, capture, || {
                                        let id = RawRowId::new(1000 + actual_calls as u64);
                                        actual_calls += 1;
                                        id
                                    });
                                    assert_eq!(
                                        actual_calls, expected_calls,
                                        "allocation: {context}"
                                    );
                                    assert_effect(&effect, &region, departure, &context);
                                    assert_eq!(
                                        snapshot(&actual),
                                        snapshot(&expected),
                                        "state: {context}"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn nel_pre_hook_sees_only_column_reset_before_lf() {
    for rows in 1..=6 {
        for cols in 1..=4 {
            for row in 0..rows {
                for col in 0..cols {
                    for pending in [false, true] {
                        let (mut actual, _) = seeded(rows, cols, 3);
                        let (mut expected, _) = seeded(rows, cols, 3);
                        for s in [&mut actual, &mut expected] {
                            s.cursor_row = row;
                            s.cursor_col = col;
                            s.pending_wrap = pending;
                        }
                        expected.cursor_col = 0;
                        actual.begin_next_line();
                        // This is exactly the host reposition-hook observation:
                        // no LF, wrap clearing, departure or row shift yet.
                        assert_eq!(snapshot(&actual), snapshot(&expected));
                        let departure = Some((expected.grid.row_id(row), row, cols));
                        let mut expected_calls = 0;
                        let region = original_lf(&mut expected, true, &mut expected_calls);
                        let mut calls = 0;
                        let effect = actual.vertical_motion(VerticalMotion::LineFeed, true, || {
                            let id = RawRowId::new(1000 + calls as u64);
                            calls += 1;
                            id
                        });
                        assert_eq!(calls, expected_calls);
                        assert_effect(&effect, &region, departure, "NEL second stage");
                        assert_eq!(snapshot(&actual), snapshot(&expected));
                    }
                }
            }
        }
    }
}
