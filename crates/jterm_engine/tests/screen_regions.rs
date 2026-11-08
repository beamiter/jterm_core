use jterm_engine::grid::{Color, RawRowId, ScrollbackLine, TerminalCell};
use jterm_engine::screen::{RegionDirection, ScreenState};
use jterm_engine::HyperlinkId;

fn populated(rows: usize, cols: usize) -> ScreenState {
    let mut s = ScreenState::new(rows, cols);
    for row in 0..rows {
        s.grid.row_ids_mut()[row] = RawRowId::new(row as u64 + 11);
        s.alt_grid.row_ids_mut()[row] = RawRowId::new(row as u64 + 101);
        s.grid.row_wrapped[row] = row % 2 == 0;
        s.alt_grid.row_wrapped[row] = row % 2 != 0;
        for col in 0..cols {
            let cell = s.grid.get_mut(row, col);
            cell.character = char::from(b'A' + row as u8);
            cell.foreground = Color::Red;
            cell.background = Color::Green;
            cell.flags.set_bold(true);
            cell.hyperlink_id = HyperlinkId::from_raw(row as u16 + 1);
            s.alt_grid.get_mut(row, col).character = 'H';
        }
    }
    s.current_bg = Color::Blue;
    s.current_fg = Color::Magenta;
    s.current_flags.set_protected(true);
    s.current_hyperlink = HyperlinkId::from_raw(99);
    s.cursor_row = rows - 1;
    s.cursor_col = cols - 1;
    s.pending_wrap = true;
    s
}

#[test]
fn scroll_captures_original_content_identity_and_wrap_before_moving() {
    for direction in [RegionDirection::Up, RegionDirection::Down] {
        let mut s = populated(5, 4);
        let removed = if direction == RegionDirection::Up {
            1
        } else {
            3
        };
        let old_id = s.grid.row_id(removed);
        let expected = ScrollbackLine::compress(&s.grid[removed], s.grid.row_wrapped[removed]);
        let mut calls = 0;
        let effect = s
            .scroll_region_one(direction, 1, 3, true, || {
                calls += 1;
                RawRowId::new(1000)
            })
            .unwrap();
        assert_eq!(calls, 1);
        assert_eq!(
            (effect.direction, effect.top, effect.bottom, effect.count),
            (direction, 1, 3, 1)
        );
        assert_eq!(effect.dirty_rows, 1..4);
        assert_eq!(effect.removed.len(), 1);
        let outgoing = &effect.removed[0];
        assert_eq!((outgoing.original_row, outgoing.row_id), (removed, old_id));
        let line = outgoing.captured.as_ref().unwrap();
        assert_eq!(line.raw_row_id(), old_id);
        assert_eq!(line.is_wrapped, expected.is_wrapped);
        assert_eq!(line.decompress(), expected.decompress());
        assert!(!s.grid.row_ids().contains(&old_id));
    }
}

#[test]
fn scroll_without_capture_discards_only_ids_without_compressing_a_snapshot() {
    let mut s = populated(4, 3);
    let effect = s
        .scroll_region_one(RegionDirection::Up, 0, 2, false, || RawRowId::new(1000))
        .unwrap();
    assert_eq!(effect.removed[0].row_id, RawRowId::new(11));
    assert!(effect.removed[0].captured.is_none());
    assert_eq!(s.grid.row_id(3), RawRowId::new(14));
}

#[test]
fn line_edits_keep_legacy_wrap_flags_while_scroll_moves_them() {
    for direction in [RegionDirection::Up, RegionDirection::Down] {
        let mut scroll = populated(5, 3);
        let mut lines = populated(5, 3);
        let wraps = lines.grid.row_wrapped.clone();
        scroll.scroll_region_one(direction, 1, 3, false, || RawRowId::new(1000));
        lines.edit_lines(direction, 1, 3, 1, || RawRowId::new(1000));
        assert_eq!(scroll.grid.to_vec(), lines.grid.to_vec());
        assert_eq!(scroll.grid.row_ids(), lines.grid.row_ids());
        assert_eq!(lines.grid.row_wrapped, wraps);
        assert_ne!(scroll.grid.row_wrapped, wraps);
        assert_eq!(scroll.grid.row_wrapped[0], wraps[0]);
        assert_eq!(scroll.grid.row_wrapped[4], wraps[4]);
    }
}

#[test]
fn structural_operations_leave_cursor_pen_pending_wrap_and_hidden_grid_unchanged() {
    for direction in [RegionDirection::Up, RegionDirection::Down] {
        for scroll in [false, true] {
            let mut s = populated(5, 3);
            s.use_alt_buffer = true;
            let hidden = s.alt_grid.to_vec();
            let hidden_ids = s.alt_grid.row_ids().to_vec();
            let hidden_wraps = s.alt_grid.row_wrapped.clone();
            let cursor = (s.cursor_row, s.cursor_col, s.pending_wrap);
            let pen = (
                s.current_fg,
                s.current_bg,
                s.current_flags,
                s.current_hyperlink,
            );
            if scroll {
                s.scroll_region_one(direction, 1, 3, true, || RawRowId::new(1000));
            } else {
                s.edit_lines(direction, 1, 3, 2, || RawRowId::new(1000));
            }
            assert_eq!((s.cursor_row, s.cursor_col, s.pending_wrap), cursor);
            assert_eq!(
                (
                    s.current_fg,
                    s.current_bg,
                    s.current_flags,
                    s.current_hyperlink
                ),
                pen
            );
            assert_eq!(s.alt_grid.to_vec(), hidden);
            assert_eq!(s.alt_grid.row_ids(), hidden_ids);
            assert_eq!(s.alt_grid.row_wrapped, hidden_wraps);
            assert!(s.use_alt_buffer);
        }
    }
}

#[test]
fn new_rows_use_bce_without_current_style_or_link() {
    for direction in [RegionDirection::Up, RegionDirection::Down] {
        let mut s = populated(5, 4);
        let mut next = 1000;
        let effect = s
            .edit_lines(direction, 1, 4, 2, || {
                let id = RawRowId::new(next);
                next += 1;
                id
            })
            .unwrap();
        assert_eq!(next, 1002);
        assert_eq!(effect.count, 2);
        let blank_rows = if direction == RegionDirection::Up {
            3..5
        } else {
            1..3
        };
        for (offset, row) in blank_rows.enumerate() {
            assert_eq!(s.grid.row_id(row), RawRowId::new(1000 + offset as u64));
            for cell in &s.grid[row] {
                assert_eq!(
                    *cell,
                    TerminalCell {
                        background: Color::Blue,
                        ..TerminalCell::default()
                    }
                );
            }
        }
        assert!(effect.removed.iter().all(|row| row.captured.is_none()));
    }
}

#[test]
fn invalid_regions_and_zero_counts_do_not_mutate_or_allocate() {
    for direction in [RegionDirection::Up, RegionDirection::Down] {
        for (top, bottom) in [
            (3, 1),
            (0, 3),
            (3, 3),
            (0, usize::MAX),
            (usize::MAX, usize::MAX),
        ] {
            let mut s = populated(3, 4);
            let before = s.grid.to_vec();
            let ids = s.grid.row_ids().to_vec();
            let wraps = s.grid.row_wrapped.clone();
            assert!(s
                .scroll_region_one(direction, top, bottom, true, || panic!(
                    "invalid allocation"
                ))
                .is_none());
            assert!(s
                .edit_lines(direction, top, bottom, usize::MAX, || panic!(
                    "invalid allocation"
                ))
                .is_none());
            assert_eq!(s.grid.to_vec(), before);
            assert_eq!(s.grid.row_ids(), ids);
            assert_eq!(s.grid.row_wrapped, wraps);
        }
        let mut s = populated(3, 4);
        let before = s.grid.to_vec();
        assert!(s
            .edit_lines(direction, 0, 2, 0, || panic!("zero allocation"))
            .is_none());
        assert_eq!(s.grid.to_vec(), before);
    }
}

#[test]
fn untracked_removed_and_retained_ids_are_not_reminted() {
    let mut s = populated(3, 2);
    s.grid.row_ids_mut()[0] = RawRowId::UNTRACKED;
    s.grid.row_ids_mut()[1] = RawRowId::UNTRACKED;
    let mut next = u64::MAX;
    let effect = s
        .scroll_region_one(RegionDirection::Up, 0, 2, true, || {
            let id = RawRowId::new(next);
            next = next.checked_add(1).unwrap_or(0);
            id
        })
        .unwrap();
    assert_eq!(effect.removed[0].row_id, RawRowId::UNTRACKED);
    assert_eq!(
        effect.removed[0].captured.as_ref().unwrap().raw_row_id(),
        RawRowId::UNTRACKED
    );
    assert_eq!(s.grid.row_id(0), RawRowId::UNTRACKED);
    assert_eq!(s.grid.row_id(2), RawRowId::new(u64::MAX));
    assert_eq!(next, 0);
    let mut calls = 0;
    s.edit_lines(RegionDirection::Down, 0, 2, 2, || {
        calls += 1;
        RawRowId::UNTRACKED
    });
    assert_eq!(calls, 2);
    assert_eq!(s.grid.row_ids(), &[RawRowId::UNTRACKED; 3]);
}

#[test]
fn repeated_scroll_preserves_one_row_effect_and_allocation_cadence() {
    let mut s = populated(4, 2);
    let mut next = 1000;
    let mut removed = Vec::new();
    for _ in 0..3 {
        let effect = s
            .scroll_region_one(RegionDirection::Up, 0, 3, true, || {
                let id = RawRowId::new(next);
                next += 1;
                id
            })
            .unwrap();
        assert_eq!(effect.count, 1);
        assert_eq!(effect.removed.len(), 1);
        removed.push(effect.removed[0].row_id);
    }
    assert_eq!(
        removed,
        [RawRowId::new(11), RawRowId::new(12), RawRowId::new(13)]
    );
    assert_eq!(
        s.grid.row_ids(),
        &[
            RawRowId::new(14),
            RawRowId::new(1000),
            RawRowId::new(1001),
            RawRowId::new(1002)
        ]
    );
    assert_eq!(next, 1003);
}

#[test]
fn all_small_regions_directions_and_counts_match_row_mapping_contract() {
    for rows in 1..=6 {
        for cols in 1..=4 {
            for top in 0..rows {
                for bottom in top..rows {
                    let height = bottom - top + 1;
                    for direction in [RegionDirection::Up, RegionDirection::Down] {
                        for scroll in [false, true] {
                            for requested in [1, 2, height, usize::MAX] {
                                let count = if scroll { 1 } else { requested.min(height) };
                                let mut s = populated(rows, cols);
                                let old = s.grid.to_vec();
                                let ids = s.grid.row_ids().to_vec();
                                let wraps = s.grid.row_wrapped.clone();
                                let blank = s.create_blank_cell();
                                let mut next = 1000;
                                let allocate = || {
                                    let id = RawRowId::new(next);
                                    next += 1;
                                    id
                                };
                                let effect = if scroll {
                                    s.scroll_region_one(direction, top, bottom, true, allocate)
                                } else {
                                    s.edit_lines(direction, top, bottom, requested, allocate)
                                }
                                .unwrap();
                                assert_eq!(effect.count, count);
                                assert_eq!(next, 1000 + count as u64);
                                assert_eq!(effect.dirty_rows, top..bottom + 1);
                                let mut blank_offset = 0;
                                for row in 0..rows {
                                    let source = if row < top || row > bottom {
                                        Some(row)
                                    } else {
                                        match direction {
                                            RegionDirection::Up => {
                                                (row + count <= bottom).then_some(row + count)
                                            }
                                            RegionDirection::Down => {
                                                row.checked_sub(count).filter(|&old| old >= top)
                                            }
                                        }
                                    };
                                    match source {
                                        Some(source) => {
                                            assert_eq!(s.grid[row], old[source]);
                                            assert_eq!(s.grid.row_id(row), ids[source]);
                                            assert_eq!(
                                                s.grid.row_wrapped[row],
                                                if scroll { wraps[source] } else { wraps[row] }
                                            );
                                        }
                                        None => {
                                            assert!(s.grid[row].iter().all(|cell| *cell == blank));
                                            assert_eq!(
                                                s.grid.row_id(row),
                                                RawRowId::new(1000 + blank_offset)
                                            );
                                            assert_eq!(
                                                s.grid.row_wrapped[row],
                                                !scroll && wraps[row]
                                            );
                                            blank_offset += 1;
                                        }
                                    }
                                }
                                let removed = if direction == RegionDirection::Up {
                                    top..top + count
                                } else {
                                    bottom + 1 - count..bottom + 1
                                };
                                assert_eq!(
                                    effect
                                        .removed
                                        .iter()
                                        .map(|r| r.original_row)
                                        .collect::<Vec<_>>(),
                                    removed.clone().collect::<Vec<_>>()
                                );
                                assert_eq!(
                                    effect.removed.iter().map(|r| r.row_id).collect::<Vec<_>>(),
                                    ids[removed]
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}
