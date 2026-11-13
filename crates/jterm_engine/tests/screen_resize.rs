use jterm_engine::grid::{Color, CursorShape, RawRowId, ScrollbackLine, TerminalCell};
use jterm_engine::protocol_state::ProtocolState;
use jterm_engine::screen::{Charset, ScreenState};
use jterm_engine::HyperlinkId;

fn seeded(rows: usize, cols: usize, variant: usize) -> ScreenState {
    let mut s = ScreenState::new(rows, cols);
    for (buffer, grid) in [&mut s.grid, &mut s.alt_grid].into_iter().enumerate() {
        for row in 0..rows {
            grid.row_ids_mut()[row] = if (row + variant) % 3 == 0 {
                RawRowId::UNTRACKED
            } else {
                RawRowId::new((100 * buffer + row + 1) as u64)
            };
            grid.row_wrapped[row] = (row + buffer) % 2 == 0;
            for col in 0..cols {
                let cell = grid.get_mut(row, col);
                cell.character = char::from(b'A' + (row + buffer * 10) as u8);
                cell.foreground = Color::Red;
                cell.background = Color::Green;
                cell.flags.set_bold(true);
                cell.flags.set_protected(row % 2 == 0);
                cell.hyperlink_id = HyperlinkId::from_raw(row as u16 + 1);
            }
            // Include valid pairs and deliberately dangling leads at the last
            // column: height-only resize repairs the latter in the old path.
            if cols > 1 {
                grid.get_mut(row, cols - 2).flags.set_wide(true);
                grid.get_mut(row, cols - 1)
                    .flags
                    .set_wide_continuation(true);
            }
            if variant % 2 == 0 {
                grid.get_mut(row, cols - 1).flags.set_wide(true);
            }
        }
    }
    s.current_bg = Color::Blue;
    s.current_fg = Color::Magenta;
    s.current_flags.set_protected(true);
    s.current_hyperlink = HyperlinkId::from_raw(99);
    s.cursor_row = variant % rows;
    s.cursor_col = cols - 1;
    s.saved_cursor_row = rows + 3;
    s.saved_cursor_col = cols + 3;
    s.alt_cursor_row = rows + 4;
    s.alt_cursor_col = cols + 4;
    s.pending_wrap = true;
    s.use_alt_buffer = variant % 2 != 0;
    s.insert_mode = true;
    s.origin_mode = true;
    s.g1_charset = Charset::DecSpecialGraphics;
    s.active_charset = Charset::DecSpecialGraphics;
    s.last_printed_char = Some('界');
    s.saved_primary_cursor_shape = Some(CursorShape::default());
    s.tab_stops.fill(false);
    s.tab_stops[cols - 1] = true;
    s.save_cursor_state(&ProtocolState::default());
    s.saved_state.as_mut().unwrap().row = rows + 7;
    s.saved_state.as_mut().unwrap().col = cols + 7;
    s.saved_primary_screen_state = s.saved_state.clone();
    match variant % 4 {
        1 => {
            s.scroll_region_top = 1.min(rows - 1);
            s.scroll_region_bottom = rows - 1;
        }
        2 => {
            s.scroll_region_top = rows + 2;
            s.scroll_region_bottom = 0;
        }
        3 => {
            s.scroll_region_top = 0;
            s.scroll_region_bottom = rows + 2;
        }
        _ => {}
    }
    s
}

fn allocate(next: &mut u64) -> RawRowId {
    if *next == 0 {
        return RawRowId::UNTRACKED;
    }
    let id = RawRowId::new(*next);
    *next = next.checked_add(1).unwrap_or(0);
    id
}

// Independently retained physical portion of Ember TerminalState::on_resize
// before extraction. Source state.rs SHA256:
// 56f1829fe8641cf543a7398f024087bfe0e408bb97e1017a384e972b7c271cea.
// Host policy supplies `from_top`; callbacks stand in only for history and ID
// allocation. Do not implement this oracle with ScreenState::resize_geometry.
fn original_resize(
    s: &mut ScreenState,
    rows: usize,
    cols: usize,
    from_top: usize,
    next: &mut u64,
) -> Option<Vec<ScrollbackLine>> {
    if cols == 0 || rows == 0 {
        return None;
    }
    if cols == s.grid.row_len() && rows == s.grid.rows() {
        return None;
    }
    let old_rows = s.grid.rows();
    let had_full_screen_region =
        old_rows == 0 || (s.scroll_region_top == 0 && s.scroll_region_bottom + 1 >= old_rows);
    let blank = s.create_blank_cell();
    let inactive_blank = TerminalCell::default();
    let mut transferred = Vec::new();
    if from_top > 0 {
        for r in 0..from_top {
            let mut line = ScrollbackLine::compress(&s.grid[r], s.grid.row_wrapped[r]);
            line.set_raw_row_id(s.grid.row_id(r));
            transferred.push(line);
        }
        s.grid.scroll_up_by(from_top, blank);
        s.cursor_row -= from_top;
    }
    s.grid.resize(rows, cols, blank);
    s.alt_grid.resize(rows, cols, inactive_blank);
    for row in 0..s.grid.rows() {
        if !s.grid.row_id(row).is_tracked() {
            s.grid.row_ids_mut()[row] = allocate(next);
        }
    }
    for row in 0..s.alt_grid.rows() {
        if !s.alt_grid.row_id(row).is_tracked() {
            s.alt_grid.row_ids_mut()[row] = allocate(next);
        }
    }
    for row in 0..rows {
        s.grid.clear_dangling_wide_at_row_end(row, blank);
        s.alt_grid
            .clear_dangling_wide_at_row_end(row, inactive_blank);
    }
    s.pending_wrap = false;
    s.cursor_row = s.cursor_row.min(rows.saturating_sub(1));
    s.cursor_col = s.cursor_col.min(cols.saturating_sub(1));
    s.saved_cursor_row = s.saved_cursor_row.min(rows.saturating_sub(1));
    s.saved_cursor_col = s.saved_cursor_col.min(cols.saturating_sub(1));
    if cols != s.tab_stops.len() {
        let old_len = s.tab_stops.len();
        s.tab_stops.resize(cols, false);
        let mut c = old_len;
        while c < cols {
            s.tab_stops[c] = c % 8 == 0;
            c += 1;
        }
    }
    if let Some(saved) = s.saved_state.as_mut() {
        saved.row = saved.row.min(rows.saturating_sub(1));
        saved.col = saved.col.min(cols.saturating_sub(1));
    }
    s.alt_cursor_row = s.alt_cursor_row.min(rows.saturating_sub(1));
    s.alt_cursor_col = s.alt_cursor_col.min(cols.saturating_sub(1));
    if had_full_screen_region {
        s.scroll_region_top = 0;
        s.scroll_region_bottom = rows.saturating_sub(1);
    } else {
        s.scroll_region_top = s.scroll_region_top.min(rows.saturating_sub(1));
        s.scroll_region_bottom = s.scroll_region_bottom.min(rows.saturating_sub(1));
        if s.scroll_region_top > s.scroll_region_bottom {
            s.scroll_region_top = 0;
            s.scroll_region_bottom = rows.saturating_sub(1);
        }
    }
    Some(transferred)
}

fn snapshot(s: &ScreenState) -> String {
    format!(
        "{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
        (s.grid.to_vec(), s.grid.row_ids(), &s.grid.row_wrapped),
        (
            s.alt_grid.to_vec(),
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
        (
            s.current_fg,
            s.current_bg,
            s.current_flags,
            s.current_hyperlink
        ),
        (
            s.use_alt_buffer,
            s.pending_wrap,
            s.origin_mode,
            s.insert_mode
        ),
        (s.g0_charset, s.g1_charset, s.active_charset),
        (&s.saved_state, &s.saved_primary_screen_state),
        (s.cursor_shape, s.saved_primary_cursor_shape),
        s.tab_stops,
        (s.scroll_region_top, s.scroll_region_bottom),
        s.last_printed_char
    )
}
fn lines(lines: &[ScrollbackLine]) -> String {
    format!(
        "{:?}",
        lines
            .iter()
            .map(|line| (line.raw_row_id(), line.is_wrapped, line.decompress()))
            .collect::<Vec<_>>()
    )
}

#[test]
fn physical_resize_matches_original_algorithm_matrix() {
    let mut cases = 0;
    for old_rows in [1_usize, 2, 5] {
        for old_cols in [1, 2, 5, 9, 17] {
            for rows in [1, 2, 3, 6] {
                for cols in [1, 2, 4, 8, 18] {
                    for variant in 0..8 {
                        for from_top in 0..=old_rows.saturating_sub(rows).min(variant % old_rows) {
                            for start_id in [1000, u64::MAX - 1, 0] {
                                let mut actual = seeded(old_rows, old_cols, variant);
                                let mut expected = seeded(old_rows, old_cols, variant);
                                let mut next_actual = start_id;
                                let mut next_expected = start_id;
                                let old = original_resize(
                                    &mut expected,
                                    rows,
                                    cols,
                                    from_top,
                                    &mut next_expected,
                                );
                                let new = actual.resize_geometry(rows, cols, from_top, || {
                                    allocate(&mut next_actual)
                                });
                                let context = format!("{old_rows}x{old_cols} -> {rows}x{cols}, variant {variant}, transfer {from_top}, id {start_id}");
                                assert_eq!(snapshot(&actual), snapshot(&expected), "{context}");
                                assert_eq!(
                                    actual.grid.to_vec(),
                                    expected.grid.to_vec(),
                                    "{context}"
                                );
                                assert_eq!(
                                    actual.alt_grid.to_vec(),
                                    expected.alt_grid.to_vec(),
                                    "{context}"
                                );
                                assert_eq!(next_actual, next_expected, "{context}");
                                match (old, new) {
                                    (None, None) => {}
                                    (Some(old), Some(new)) => {
                                        assert_eq!(
                                            lines(&old),
                                            lines(&new.transferred),
                                            "{context}"
                                        );
                                        assert_eq!(
                                            (new.old_rows, new.old_cols, new.rows, new.cols),
                                            (old_rows, old_cols, rows, cols)
                                        );
                                        for (old, new) in old.iter().zip(new.transferred.iter()) {
                                            assert_eq!(
                                                old.decompress(),
                                                new.decompress(),
                                                "{context}"
                                            );
                                        }
                                        assert_eq!(new.dirty_rows, 0..rows);
                                    }
                                    _ => panic!("mismatched no-op: {context}"),
                                }
                                cases += 1;
                            }
                        }
                    }
                }
            }
        }
    }
    assert_eq!(cases, 10125);
}

#[test]
fn invalid_resize_is_noop_and_consumes_no_ids() {
    for (rows, cols, transfer) in [
        (0, 4, 0),
        (4, 0, 0),
        (5, 5, 0),
        (4, 5, 2),
        (6, 5, 1),
        (1, 1, usize::MAX),
    ] {
        let mut s = seeded(5, 5, 4);
        let before = snapshot(&s);
        let effect = s.resize_geometry(rows, cols, transfer, || {
            panic!("invalid request allocated an ID")
        });
        assert!(effect.is_none());
        assert_eq!(snapshot(&s), before);
    }
}

#[test]
fn shrinking_allocates_no_temporary_ids_for_discarded_bottom_vacancies() {
    let mut s = seeded(5, 5, 4);
    for (i, id) in s.grid.row_ids_mut().iter_mut().enumerate() {
        *id = RawRowId::new(i as u64 + 1);
    }
    for (i, id) in s.alt_grid.row_ids_mut().iter_mut().enumerate() {
        *id = RawRowId::new(i as u64 + 11);
    }
    let effect = s
        .resize_geometry(2, 3, 3, || {
            panic!("compaction must not allocate temporary IDs")
        })
        .unwrap();
    assert_eq!(effect.transferred.len(), 3);
    assert_eq!(s.grid.row_ids(), &[RawRowId::new(4), RawRowId::new(5)]);
    assert_eq!(
        s.alt_grid.row_ids(),
        &[RawRowId::new(11), RawRowId::new(12)]
    );
    assert_eq!(s.cursor_row, 1);
    assert_eq!(effect.transferred[0].decompress().len(), 5);
}

#[test]
fn growth_uses_bce_only_for_active_and_allocates_active_before_hidden() {
    let mut s = seeded(1, 2, 1);
    s.grid.row_ids_mut()[0] = RawRowId::new(10);
    s.alt_grid.row_ids_mut()[0] = RawRowId::new(20);
    let mut next = 100;
    s.resize_geometry(2, 3, 0, || allocate(&mut next)).unwrap();
    assert_eq!(next, 102);
    assert_eq!(s.grid.row_id(1), RawRowId::new(100));
    assert_eq!(s.alt_grid.row_id(1), RawRowId::new(101));
    assert_eq!(s.grid[0][2], s.create_blank_cell());
    assert_eq!(s.grid[1], [s.create_blank_cell(); 3]);
    assert_eq!(s.alt_grid[0][2], TerminalCell::default());
    assert_eq!(s.alt_grid[1], [TerminalCell::default(); 3]);
}

#[test]
fn resize_clips_decsc_but_retains_saved_primary_and_saved_pending_wrap() {
    let mut s = seeded(5, 17, 4);
    let primary = format!("{:?}", s.saved_primary_screen_state);
    s.resize_geometry(2, 4, 0, || RawRowId::UNTRACKED).unwrap();
    assert_eq!(
        (
            s.saved_state.as_ref().unwrap().row,
            s.saved_state.as_ref().unwrap().col
        ),
        (1, 3)
    );
    assert!(s.saved_state.as_ref().unwrap().pending_wrap);
    assert!(!s.pending_wrap);
    assert_eq!(format!("{:?}", s.saved_primary_screen_state), primary);
    s.resize_geometry(2, 18, 0, || RawRowId::UNTRACKED).unwrap();
    assert_eq!(
        s.tab_stops
            .iter()
            .enumerate()
            .filter_map(|(i, set)| set.then_some(i))
            .collect::<Vec<_>>(),
        vec![8, 16]
    );
}
