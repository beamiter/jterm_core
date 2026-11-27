//! Ordered DECIC/DECDC and ECMA SL/SR edits across the vertical scroll region.
use crate::{
    grid::WideCellEditPolicy,
    screen::{RowMutation, ScreenState},
    TerminalCell,
};

pub(crate) struct ColumnEdit {
    next_row: usize,
    end_row: usize,
    column: usize,
    count: usize,
    insert: bool,
    blank: TerminalCell,
}

/// Admit only exact DEC column-edit sequences. A matched but invalid request
/// is an empty operation, preventing permissive legacy CSI fallback admission.
pub(crate) fn prepare_column_edit(
    screen: &ScreenState,
    params: &[u16],
    command: char,
    prefix: Option<u8>,
    intermediates: &[u8],
) -> Option<ColumnEdit> {
    if prefix.is_some() {
        return None;
    }
    let horizontal_scroll = intermediates == b" " && matches!(command, '@' | 'A');
    if !horizontal_scroll && !(intermediates == b"'" && matches!(command, '}' | '~')) {
        return None;
    }
    let column = if horizontal_scroll {
        0
    } else {
        screen.cursor_col
    };
    let mut edit = ColumnEdit {
        next_row: 0,
        end_row: 0,
        column,
        count: 0,
        insert: matches!(command, '}' | 'A'),
        blank: if horizontal_scroll {
            TerminalCell {
                background: screen.current_bg,
                ..TerminalCell::default()
            }
        } else {
            TerminalCell::default()
        },
    };
    let (top, bottom) = (screen.scroll_region_top, screen.scroll_region_bottom);
    if params.len() > 1
        || top > bottom
        || bottom >= screen.grid.rows()
        || screen.cursor_row < top
        || screen.cursor_row > bottom
        || screen.cursor_col >= screen.grid.cols()
    {
        return Some(edit);
    }
    edit.next_row = top;
    edit.end_row = bottom + 1;
    edit.count =
        usize::from(params.first().copied().unwrap_or(1).max(1)).min(screen.grid.cols() - column);
    Some(edit)
}

/// XTerm DECBI/DECFI: move inside the page, otherwise shift the scroll
/// region at the edge. Raw column shifts preserve the Last Column Flag.
pub(crate) fn prepare_column_index(
    screen: &mut ScreenState,
    backward: bool,
    protocol: &crate::protocol_state::ProtocolState,
) -> Option<ColumnEdit> {
    let cols = screen.grid.cols();
    if cols == 0 || screen.cursor_col >= cols {
        return None;
    }
    if backward && screen.cursor_col > 0 {
        screen.move_cursor_backward_with_wrap(1, protocol);
        return None;
    }
    if !backward && screen.cursor_col + 1 < cols {
        screen.move_cursor_forward(1);
        return None;
    }
    let (top, bottom) = (screen.scroll_region_top, screen.scroll_region_bottom);
    if top > bottom
        || bottom >= screen.grid.rows()
        || screen.cursor_row < top
        || screen.cursor_row > bottom
    {
        return None;
    }
    Some(ColumnEdit {
        next_row: top,
        end_row: bottom + 1,
        column: 0,
        count: 1,
        insert: backward,
        blank: TerminalCell {
            foreground: screen.current_fg,
            background: screen.current_bg,
            ..TerminalCell::default()
        },
    })
}

impl ColumnEdit {
    /// The caller applies each row's provenance/dirty notification before
    /// requesting another step. Geometry remains fixed during this command.
    pub(crate) fn step(&mut self, screen: &mut ScreenState) -> Option<RowMutation> {
        if self.next_row >= self.end_row {
            return None;
        }
        let row = self.next_row;
        self.next_row += 1;
        let mut start = self.column;
        if start > 0
            && (screen.grid.get(row, start).flags.wide_continuation()
                || (!self.insert && screen.grid.get(row, start - 1).flags.wide()))
        {
            start -= 1;
        }
        if self.insert {
            screen.grid.insert_blank_cells(
                row,
                self.column,
                self.count,
                self.blank,
                WideCellEditPolicy::RepairSplitPairs,
            );
        } else {
            screen.grid.delete_cells(
                row,
                self.column,
                self.count,
                self.blank,
                WideCellEditPolicy::RepairSplitPairs,
            );
            // XTerm's raw column deletion invalidates the soft-wrap relation;
            // insertion preserves the destination row's existing line flag.
            screen.grid.row_wrapped[row] = false;
        }
        Some(RowMutation {
            row,
            row_id: screen.grid.row_id(row),
            changed: start..screen.grid.cols(),
            printed: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{terminal::Terminal, Color, HyperlinkId, StyleFlags};
    fn apply(screen: &mut ScreenState, params: &[u16], insert: bool) -> Vec<RowMutation> {
        let mut operation =
            prepare_column_edit(screen, params, if insert { '}' } else { '~' }, None, b"'")
                .unwrap();
        let mut mutations = Vec::new();
        while let Some(row) = operation.step(screen) {
            mutations.push(row);
        }
        mutations
    }
    #[test]
    fn index_matches_snapshot_reference_and_bce_without_cursor_or_identity_changes() {
        for backward in [false, true] {
            for cursor_row in 0..4 {
                for cursor_col in 0..5 {
                    let mut screen = ScreenState::new(4, 5);
                    for r in 0..4 {
                        for c in 0..5 {
                            screen.grid.get_mut(r, c).character =
                                char::from(b'A' + (r * 5 + c) as u8);
                        }
                    }
                    screen.scroll_region_top = 1;
                    screen.scroll_region_bottom = 2;
                    screen.cursor_row = cursor_row;
                    screen.cursor_col = cursor_col;
                    screen.pending_wrap = true;
                    screen.current_fg = Color::Red;
                    screen.current_bg = Color::Blue;
                    screen.current_flags.set_bold(true);
                    screen.current_flags.set_protected(true);
                    let ids = screen.grid.row_ids().to_vec();
                    let mut rows = Vec::new();
                    if let Some(mut edit) = prepare_column_index(
                        &mut screen,
                        backward,
                        &crate::protocol_state::ProtocolState::default(),
                    ) {
                        while let Some(effect) = edit.step(&mut screen) {
                            rows.push(effect.row);
                        }
                    }
                    let edge = if backward {
                        cursor_col == 0
                    } else {
                        cursor_col == 4
                    };
                    let shifts = edge && (1..=2).contains(&cursor_row);
                    assert_eq!(rows, if shifts { vec![1, 2] } else { vec![] });
                    assert_eq!(
                        screen.cursor_col,
                        if edge {
                            cursor_col
                        } else if backward {
                            cursor_col - 1
                        } else {
                            cursor_col + 1
                        }
                    );
                    assert_eq!(screen.pending_wrap, edge);
                    assert_eq!(screen.grid.row_ids(), ids.as_slice());
                    for r in 0..4 {
                        for c in 0..5 {
                            let shifted = shifts && (1..=2).contains(&r);
                            let blank = shifted && if backward { c == 0 } else { c == 4 };
                            let cell = screen.grid.get(r, c);
                            let source = if shifted && !blank {
                                if backward {
                                    c - 1
                                } else {
                                    c + 1
                                }
                            } else {
                                c
                            };
                            assert_eq!(
                                cell.character,
                                if blank {
                                    ' '
                                } else {
                                    char::from(b'A' + (r * 5 + source) as u8)
                                }
                            );
                            if blank {
                                assert_eq!(
                                    (cell.foreground, cell.background),
                                    (Color::Red, Color::Blue)
                                );
                                assert_eq!(cell.flags, StyleFlags::default());
                                assert_eq!(cell.hyperlink_id, HyperlinkId::NONE);
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn columns_match_independent_snapshot_shift_reference_across_counts_and_regions() {
        for insert in [false, true] {
            for column in 0..5 {
                for count in [0, 1, 2, 5, u16::MAX] {
                    let mut screen = ScreenState::new(4, 5);
                    for row in 0..4 {
                        for col in 0..5 {
                            let mut flags = StyleFlags::default();
                            flags.set_bold(col % 2 == 0);
                            flags.set_protected(row == 1);
                            *screen.grid.get_mut(row, col) = TerminalCell {
                                character: char::from(b'A' + (row * 5 + col) as u8),
                                foreground: Color::Red,
                                background: Color::Blue,
                                flags,
                                hyperlink_id: HyperlinkId::from_raw((col + 1) as u16),
                            };
                            screen.alt_grid.get_mut(row, col).character = 'Z';
                        }
                        screen.grid.row_wrapped[row] = true;
                    }
                    screen.scroll_region_top = 1;
                    screen.scroll_region_bottom = 2;
                    screen.cursor_row = 2;
                    screen.cursor_col = column;
                    screen.pending_wrap = true;
                    screen.current_bg = Color::Green;
                    screen.current_flags.set_italic(true);
                    screen.origin_mode = true;
                    let before = screen.grid.to_vec();
                    let hidden = screen.alt_grid.to_vec();
                    let ids: Vec<_> = (0..4).map(|r| screen.grid.row_id(r)).collect();
                    let mut expected = before.clone();
                    let count_usize = usize::from(count.max(1)).min(5 - column);
                    for row in 1..=2 {
                        for col in column..5 {
                            expected[row][col] = if insert {
                                if col < column + count_usize {
                                    TerminalCell::default()
                                } else {
                                    before[row][col - count_usize]
                                }
                            } else if col + count_usize < 5 {
                                before[row][col + count_usize]
                            } else {
                                TerminalCell::default()
                            };
                        }
                    }
                    let effects = apply(&mut screen, &[count], insert);
                    assert_eq!(
                        screen.grid.to_vec(),
                        expected,
                        "insert={insert} col={column} count={count}"
                    );
                    assert_eq!(effects.iter().map(|m| m.row).collect::<Vec<_>>(), [1, 2]);
                    assert!(effects
                        .iter()
                        .all(|m| m.printed.is_none() && m.changed == (column..5)));
                    assert_eq!(screen.grid.row_wrapped, [true, insert, insert, true]);
                    assert_eq!(
                        (screen.cursor_row, screen.cursor_col, screen.pending_wrap),
                        (2, column, true)
                    );
                    assert_eq!(
                        (0..4).map(|r| screen.grid.row_id(r)).collect::<Vec<_>>(),
                        ids
                    );
                    assert_eq!(screen.alt_grid.to_vec(), hidden);
                    assert_eq!(screen.current_bg, Color::Green);
                    assert!(screen.current_flags.italic() && screen.origin_mode);
                }
            }
        }
    }
    #[test]
    fn admission_is_exact_and_cursor_outside_region_is_ignored() {
        let mut screen = ScreenState::new(4, 6);
        screen.scroll_region_top = 1;
        screen.scroll_region_bottom = 2;
        for row in [0, 3] {
            screen.cursor_row = row;
            assert!(apply(&mut screen, &[], true).is_empty());
            assert!(apply(&mut screen, &[], false).is_empty());
        }
        screen.cursor_row = 1;
        assert!(apply(&mut screen, &[1, 2], true).is_empty());
        for (prefix, intermediate) in [(Some(b'?'), b"'".as_slice()), (None, b""), (None, b" '")] {
            assert!(prepare_column_edit(&screen, &[1], '}', prefix, intermediate).is_none());
        }
        screen.grid.get_mut(1, 0).character = 'X';
        screen.grid.get_mut(2, 0).character = 'Y';
        let mut operation = prepare_column_edit(&screen, &[], '}', None, b"'").unwrap();
        assert_eq!(operation.step(&mut screen).unwrap().row, 1);
        assert_eq!(screen.grid.get(1, 1).character, 'X');
        assert_eq!(screen.grid.get(2, 0).character, 'Y'); // The second row has not stepped yet.
    }
    #[test]
    fn underline_links_graphemes_and_image_annotations_move_with_their_cells() {
        let mut terminal = Terminal::new(1, 8).unwrap();
        let prepared = terminal.prepare_inline_image((0, 0), 1, 1, false).unwrap();
        terminal.place_inline_image(prepared).unwrap();
        let frame = terminal.frame().unwrap();
        let image = frame
            .row_annotations(0)
            .unwrap()
            .image_at(0)
            .unwrap()
            .clone();
        for insert in [false, true] {
            let mut screen = ScreenState::new(1, 8);
            for col in 0..8 {
                *screen.grid.get_mut(0, col) = TerminalCell {
                    character: char::from(b'a' + col as u8),
                    hyperlink_id: HyperlinkId::from_raw((col + 1) as u16),
                    ..TerminalCell::default()
                };
            }
            screen.grid.get_mut(0, 2).character = ' ';
            screen.grid.set_image(0, 2, image.clone());
            screen.grid.set_cluster_text(0, 3, "d\u{301}").unwrap();
            screen.grid.set_underline_color(0, 0..8, Color::Red);
            screen.cursor_col = 2;
            apply(&mut screen, &[1], insert);
            let cluster_col = if insert { 4 } else { 2 };
            assert_eq!(screen.grid.cluster_text(0, cluster_col), Some("d\u{301}"));
            assert_eq!(
                screen.grid.get(0, cluster_col).hyperlink_id,
                HyperlinkId::from_raw(4)
            );
            assert_eq!(screen.grid.underline_colors(0)[cluster_col], Color::Red);
            let annotations = screen.grid.row_annotations(0).unwrap();
            if insert {
                assert_eq!(annotations.image_at(3), Some(&image));
            } else {
                assert!(annotations.image_at(2).is_none());
            }
            let blank_col = if insert { 2 } else { 7 };
            assert_eq!(*screen.grid.get(0, blank_col), TerminalCell::default());
            assert_eq!(screen.grid.underline_colors(0)[blank_col], Color::Default);
        }
    }
    #[test]
    fn split_wide_pairs_are_repaired_and_included_in_changed_span() {
        for insert in [false, true] {
            let mut screen = ScreenState::new(1, 6);
            screen.grid.get_mut(0, 1).character = '界';
            screen.grid.get_mut(0, 1).flags.set_wide(true);
            screen.grid.get_mut(0, 2).flags.set_wide_continuation(true);
            screen.grid.set_cluster_text(0, 1, "界\u{301}").unwrap();
            screen.cursor_col = 2;
            let effects = apply(&mut screen, &[1], insert);
            assert_eq!(effects[0].changed, 1..6);
            assert_eq!(*screen.grid.get(0, 1), TerminalCell::default());
            assert!(screen.grid.cluster_text(0, 1).is_none());
            for col in 0..6 {
                assert!(
                    !screen.grid.get(0, col).flags.wide()
                        && !screen.grid.get(0, col).flags.wide_continuation()
                );
            }
        }
        let mut screen = ScreenState::new(1, 6);
        screen.grid.get_mut(0, 4).character = '界';
        screen.grid.get_mut(0, 4).flags.set_wide(true);
        screen.grid.get_mut(0, 5).flags.set_wide_continuation(true);
        apply(&mut screen, &[1], true);
        assert!(!screen.grid.get(0, 5).flags.wide());
    }
    #[test]
    fn horizontal_scroll_uses_full_width_bce_and_preserves_cursor_at_nonzero_column() {
        for insert in [false, true] {
            for column in 1..5 {
                for count in [0, 1, 3, u16::MAX] {
                    let mut screen = ScreenState::new(4, 5);
                    for row in 0..4 {
                        for col in 0..5 {
                            *screen.grid.get_mut(row, col) = TerminalCell {
                                character: char::from(b'A' + (row * 5 + col) as u8),
                                foreground: Color::Red,
                                hyperlink_id: HyperlinkId::from_raw((col + 1) as u16),
                                ..TerminalCell::default()
                            };
                        }
                        screen.grid.row_wrapped[row] = true;
                    }
                    screen.cursor_row = 2;
                    screen.cursor_col = column;
                    screen.pending_wrap = true;
                    screen.scroll_region_top = 1;
                    screen.scroll_region_bottom = 2;
                    screen.current_bg = Color::Blue;
                    screen.current_flags.set_bold(true);
                    let before = screen.grid.to_vec();
                    let ids: Vec<_> = (0..4).map(|r| screen.grid.row_id(r)).collect();
                    let mut expected = before.clone();
                    let amount = usize::from(count.max(1)).min(5);
                    let blank = TerminalCell {
                        background: Color::Blue,
                        ..TerminalCell::default()
                    };
                    for row in 1..=2 {
                        for col in 0..5 {
                            expected[row][col] = if insert {
                                if col < amount {
                                    blank
                                } else {
                                    before[row][col - amount]
                                }
                            } else if col + amount < 5 {
                                before[row][col + amount]
                            } else {
                                blank
                            };
                        }
                    }
                    let mut operation = prepare_column_edit(
                        &screen,
                        &[count],
                        if insert { 'A' } else { '@' },
                        None,
                        b" ",
                    )
                    .unwrap();
                    let mut effects = Vec::new();
                    while let Some(row) = operation.step(&mut screen) {
                        effects.push(row);
                    }
                    assert_eq!(
                        screen.grid.to_vec(),
                        expected,
                        "insert={insert} col={column} count={count}"
                    );
                    assert!(effects
                        .iter()
                        .all(|m| m.changed == (0..5) && m.printed.is_none()));
                    assert_eq!(
                        (screen.cursor_row, screen.cursor_col, screen.pending_wrap),
                        (2, column, true)
                    );
                    assert_eq!(screen.grid.row_wrapped, [true, insert, insert, true]);
                    assert_eq!(
                        (0..4).map(|r| screen.grid.row_id(r)).collect::<Vec<_>>(),
                        ids
                    );
                }
            }
        }
    }
    #[test]
    fn horizontal_scroll_admission_does_not_claim_plain_commands_or_outside_rows() {
        let mut screen = ScreenState::new(3, 6);
        screen.scroll_region_top = 1;
        screen.scroll_region_bottom = 2;
        for command in ['@', 'A'] {
            assert!(prepare_column_edit(&screen, &[], command, None, b"").is_none());
            assert!(prepare_column_edit(&screen, &[], command, Some(b'?'), b" ").is_none());
            assert!(prepare_column_edit(&screen, &[], command, None, b"  ").is_none());
            assert!(prepare_column_edit(&screen, &[], command, None, b" ")
                .unwrap()
                .step(&mut screen)
                .is_none());
            screen.cursor_row = 1;
            assert!(prepare_column_edit(&screen, &[1, 2], command, None, b" ")
                .unwrap()
                .step(&mut screen)
                .is_none());
            screen.cursor_row = 0;
        }
    }
}
