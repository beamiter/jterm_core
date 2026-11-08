use jterm_engine::grid::{Color, RawRowId};
use jterm_engine::protocol_state::ProtocolState;
use jterm_engine::screen::{Charset, RowMutation, ScreenState};
use jterm_engine::HyperlinkId;

fn screen(cols: usize) -> ScreenState {
    let mut s = ScreenState::new(3, cols);
    for (i, id) in s.grid.row_ids_mut().iter_mut().enumerate() {
        *id = RawRowId::new(41 + i as u64);
    }
    s
}
fn cells(s: &ScreenState) -> String {
    format!("{:?}", s.grid.to_vec())
}
fn raw_row(s: &ScreenState) -> String {
    s.grid[0].iter().map(|c| c.character).collect()
}
fn write(s: &mut ScreenState, ch: char, width: usize) -> RowMutation {
    s.write_char_in_row(ch, width, true)
}
fn wrap(s: &mut ScreenState) {
    s.grid.row_wrapped[s.cursor_row] = true;
    s.cursor_col = 0;
    s.cursor_row = (s.cursor_row + 1) % s.grid.rows();
}
fn check_pairs(s: &ScreenState) {
    for row in s.grid.iter() {
        for (col, cell) in row.iter().enumerate() {
            if cell.flags.wide() {
                assert!(
                    col + 1 < row.len() && row[col + 1].flags.wide_continuation(),
                    "{col}: {row:?}"
                );
            }
            if cell.flags.wide_continuation() {
                assert!(col > 0 && row[col - 1].flags.wide(), "{col}: {row:?}");
            }
        }
    }
}

#[test]
fn cursor_save_restore_retains_pen_charsets_origin_protection_and_wrap() {
    let mut s = screen(16);
    let mut protocol = ProtocolState::default();
    protocol.modes_mut().insert(7);
    s.set_scroll_region(2, 3);
    s.origin_mode = true;
    s.set_cursor_position(2, 16);
    s.current_fg = Color::Red;
    s.current_bg = Color::Blue;
    s.current_flags.set_protected(true);
    s.g0_charset = Charset::DecSpecialGraphics;
    s.active_charset = s.g0_charset;
    s.pending_wrap = true;
    s.save_cursor_state(&protocol);
    protocol.modes_mut().remove(&7);
    s.current_fg = Color::Green;
    s.current_bg = Color::Default;
    s.current_flags.set_protected(false);
    s.g0_charset = Charset::Ascii;
    s.active_charset = Charset::Ascii;
    s.origin_mode = false;
    s.set_cursor_position(1, 1);
    s.restore_cursor_state(&mut protocol);
    assert_eq!((s.cursor_row, s.cursor_col), (2, 15));
    assert_eq!((s.current_fg, s.current_bg), (Color::Red, Color::Blue));
    assert!(s.origin_mode && s.pending_wrap && s.current_flags.protected());
    assert_eq!(s.translate_char('q'), '─');
    assert!(protocol.modes().contains(&7));
    // Save with wrapping disabled and restore against the same sole mode owner.
    protocol.modes_mut().remove(&7);
    s.save_cursor_state(&protocol);
    protocol.modes_mut().insert(7);
    s.restore_cursor_state(&mut protocol);
    assert!(!protocol.modes().contains(&7));
}

#[test]
fn restore_without_save_homes_and_saved_positions_clamp_to_resized_grid() {
    let mut s = screen(8);
    let mut protocol = ProtocolState::default();
    s.set_cursor_position(3, 8);
    s.pending_wrap = true;
    s.restore_cursor_state(&mut protocol);
    assert_eq!((s.cursor_row, s.cursor_col, s.pending_wrap), (0, 0, false));
    s.set_cursor_position(3, 8);
    s.save_cursor_state(&protocol);
    s.grid.resize(1, 2, s.create_blank_cell());
    s.restore_cursor_state(&mut protocol);
    assert_eq!((s.cursor_row, s.cursor_col), (0, 1));
}

#[test]
fn cursor_origin_margins_counts_and_tabs_are_bounded() {
    let mut s = ScreenState::new(6, 25);
    s.set_scroll_region(2, 5);
    s.origin_mode = true;
    s.set_cursor_position(usize::MAX, usize::MAX);
    assert_eq!((s.cursor_row, s.cursor_col), (4, 24));
    s.move_cursor_up(usize::MAX);
    assert_eq!(s.cursor_row, 1);
    s.move_cursor_down(usize::MAX);
    assert_eq!(s.cursor_row, 4);
    s.origin_mode = false;
    s.set_cursor_position(1, 1);
    s.move_cursor_up(usize::MAX);
    assert_eq!(s.cursor_row, 0);
    s.set_cursor_row(6);
    s.move_cursor_down(usize::MAX);
    assert_eq!(s.cursor_row, 5);
    s.move_cursor_forward(usize::MAX);
    assert_eq!(s.cursor_col, 24);
    s.move_cursor_backward(usize::MAX);
    assert_eq!(s.cursor_col, 0);
    s.tab_forward(2);
    assert_eq!(s.cursor_col, 16);
    s.tab_backward(1);
    assert_eq!(s.cursor_col, 8);
    s.clear_all_tab_stops();
    s.set_cursor_column(4);
    s.set_tab_stop();
    assert_eq!(s.next_tab_stop(0), 3);
    s.clear_tab_stop();
    assert_eq!(s.next_tab_stop(usize::MAX), 24);
    assert_eq!(s.prev_tab_stop(usize::MAX), 0);
    s.tab_backward(usize::MAX);
    assert_eq!(s.cursor_col, 0);
    s.tab_forward(usize::MAX);
    assert_eq!(s.cursor_col, 24);
    s.set_scroll_region(6, 2);
    assert_eq!((s.scroll_region_top, s.scroll_region_bottom), (0, 5));
}

#[test]
fn changed_span_includes_wide_collateral_but_printed_span_does_not() {
    let mut s = screen(8);
    write(&mut s, '界', 2);
    s.set_cursor_column(2);
    let effect = write(&mut s, 'x', 1);
    assert_eq!(effect.row_id, RawRowId::new(41));
    assert_eq!(effect.changed, 0..2);
    assert_eq!(effect.printed, Some(1..2));
    assert_eq!(raw_row(&s), " x      ");
    s.set_cursor_column(4);
    write(&mut s, '界', 2);
    s.set_cursor_column(3);
    let effect = write(&mut s, '文', 2);
    assert_eq!(effect.changed, 2..5);
    assert_eq!(effect.printed, Some(2..4));
    check_pairs(&s);
}

#[test]
fn insert_and_delete_expand_original_span_and_never_report_printed_output() {
    for insert in [true, false] {
        let mut s = screen(8);
        write(&mut s, 'a', 1);
        write(&mut s, '界', 2);
        write(&mut s, 'b', 1);
        s.set_cursor_column(3);
        s.pending_wrap = true;
        let effect = if insert {
            s.insert_characters(1)
        } else {
            s.delete_characters(1)
        }
        .unwrap();
        assert_eq!(effect.changed, 1..8);
        assert_eq!(effect.printed, None);
        assert_eq!(effect.row_id, RawRowId::new(41));
        assert!(!s.pending_wrap);
        check_pairs(&s);
    }
}

#[test]
fn irm_inside_wide_pair_repairs_both_halves_after_shifting() {
    for width in [1, 2] {
        let mut s = screen(8);
        write(&mut s, 'a', 1);
        write(&mut s, '界', 2);
        write(&mut s, 'b', 1);
        s.set_cursor_column(3);
        s.insert_mode = true;
        let effect = write(&mut s, if width == 1 { 'x' } else { '文' }, width);
        assert_eq!(effect.changed, 1..8);
        assert_eq!(effect.printed, Some(2..2 + width));
        check_pairs(&s);
        assert_eq!(s.grid[0][1].character, ' ');
        assert_eq!(s.grid[0][3 + width].character, 'b');
    }
}

#[test]
fn edits_preserve_bce_and_remove_old_pen_hyperlink_protection() {
    let mut s = screen(8);
    s.current_bg = Color::Blue;
    s.current_fg = Color::Red;
    s.current_flags.set_bold(true);
    s.current_flags.set_protected(true);
    s.current_hyperlink = HyperlinkId::from_raw(7);
    write(&mut s, '界', 2);
    let effects = s.erase_span(0, 1, 2, false);
    assert_eq!(effects[0].changed, 0..2);
    assert_eq!(effects[0].printed, None);
    for cell in &s.grid[0][..2] {
        assert_eq!(
            (cell.character, cell.foreground, cell.background),
            (' ', Color::Default, Color::Blue)
        );
        assert!(
            !cell.flags.wide()
                && !cell.flags.wide_continuation()
                && !cell.flags.bold()
                && !cell.flags.protected()
        );
        assert!(cell.hyperlink_id.is_none());
    }
}

#[test]
fn selective_erase_reports_only_eligible_runs_and_keeps_protected_wrap() {
    let mut s = screen(8);
    s.current_flags.set_protected(true);
    s.write_ascii_in_row(b"ABCDEFGH", true);
    assert!(s.pending_wrap);
    assert!(s.erase_span(0, 0, usize::MAX, true).is_empty());
    assert!(s.pending_wrap);
    for col in [1, 3, 4, 7] {
        s.grid[0][col].flags.set_protected(false);
    }
    let effects = s.erase_span(0, 0, usize::MAX, true);
    assert_eq!(
        effects
            .iter()
            .map(|e| e.changed.clone())
            .collect::<Vec<_>>(),
        vec![1..2, 3..5, 7..8]
    );
    assert!(effects
        .iter()
        .all(|e| e.printed.is_none() && e.row_id == RawRowId::new(41)));
    assert_eq!(raw_row(&s), "A C  FG ");
    assert!(!s.pending_wrap);
    assert!(s.erase_span(usize::MAX, 0, usize::MAX, false).is_empty());
}

#[test]
fn combining_changes_only_the_base_and_retains_cursor_and_wrap() {
    let mut s = screen(2);
    s.write_ascii_in_row(b"ae", true);
    assert_eq!(s.combining_target(), Some((1, 'e')));
    let effect = s.replace_combining_target(1, 'é');
    assert_eq!(effect.changed, 1..2);
    assert_eq!(effect.printed, Some(1..2));
    assert_eq!(effect.row_id, RawRowId::new(41));
    assert_eq!((s.cursor_row, s.cursor_col, s.pending_wrap), (0, 1, true));
    s.set_cursor_column(1);
    assert_eq!(s.combining_target(), None);
}

#[test]
fn ascii_and_scalar_writes_match_across_boundaries_and_autowrap_modes() {
    for cols in 1..=12 {
        for autowrap in [false, true] {
            for len in 0..=64 {
                let bytes: Vec<_> = (0..len).map(|i| b'a' + (i % 26) as u8).collect();
                let mut ascii = screen(cols);
                let mut scalar = screen(cols);
                let mut offset = 0;
                while offset < bytes.len() {
                    if ascii.take_pending_wrap(autowrap) {
                        wrap(&mut ascii);
                    }
                    let (n, _) = ascii
                        .write_ascii_in_row(&bytes[offset..], autowrap)
                        .unwrap();
                    offset += n;
                }
                for &byte in &bytes {
                    if scalar.take_pending_wrap(autowrap) {
                        wrap(&mut scalar);
                    }
                    assert!(!scalar.prepare_character_width(1, autowrap));
                    scalar.write_char_in_row(byte as char, 1, autowrap);
                    scalar.last_printed_char = Some(byte as char);
                }
                assert_eq!(
                    cells(&ascii),
                    cells(&scalar),
                    "cols={cols}, len={len}, wrap={autowrap}"
                );
                assert_eq!(
                    (
                        ascii.cursor_row,
                        ascii.cursor_col,
                        ascii.pending_wrap,
                        ascii.last_printed_char
                    ),
                    (
                        scalar.cursor_row,
                        scalar.cursor_col,
                        scalar.pending_wrap,
                        scalar.last_printed_char
                    )
                );
                assert_eq!(ascii.grid.row_wrapped, scalar.grid.row_wrapped);
            }
        }
    }
}

#[test]
fn empty_ascii_and_zero_edits_are_noops_and_counts_are_bounded() {
    let mut s = screen(8);
    s.pending_wrap = true;
    assert!(s.write_ascii_in_row(b"", true).is_none());
    assert!(s.insert_characters(0).is_none());
    assert!(s.delete_characters(0).is_none());
    assert!(s.pending_wrap);
    assert_eq!(s.insert_characters(usize::MAX).unwrap().changed, 0..8);
    assert_eq!(s.delete_characters(usize::MAX).unwrap().changed, 0..8);
}

#[test]
fn one_column_wide_write_retains_existing_truncated_lead_semantics() {
    for autowrap in [false, true] {
        let mut s = screen(1);
        assert_eq!(s.prepare_character_width(2, autowrap), autowrap);
        let effect = s.write_char_in_row('界', 2, autowrap);
        assert_eq!((s.cursor_col, s.pending_wrap), (0, autowrap));
        assert_eq!(effect.changed, 0..1);
        assert_eq!(effect.printed, Some(0..1));
        assert_eq!(s.grid[0][0].character, '界');
        assert!(s.grid[0][0].flags.wide());
        assert!(!s.grid[0][0].flags.wide_continuation());
    }
    let mut s = screen(4);
    s.set_cursor_column(4);
    assert!(!s.prepare_character_width(2, false));
    assert_eq!(s.cursor_col, 2);
    s.write_char_in_row('界', 2, false);
    assert_eq!((s.cursor_col, s.pending_wrap), (2, false));
    check_pairs(&s);
}

#[test]
fn deterministic_width_aware_edit_sweep_keeps_pairs_and_row_identity() {
    let mut s = screen(10);
    let ids = s.grid.row_ids().to_vec();
    let mut seed = 7u64;
    for _ in 0..3000 {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        s.set_cursor_column(((seed >> 8) as usize % 9) + 1);
        match seed % 6 {
            0 => {
                write(&mut s, '界', 2);
            }
            1 => {
                write(&mut s, 'x', 1);
            }
            2 => {
                s.insert_characters((seed as usize >> 16) % 4);
            }
            3 => {
                s.delete_characters((seed as usize >> 16) % 4);
            }
            4 => {
                s.erase_span(0, s.cursor_col, s.cursor_col + 2, false);
            }
            _ => {
                s.insert_mode = true;
                write(&mut s, '文', 2);
                s.insert_mode = false;
            }
        }
        check_pairs(&s);
        assert_eq!(s.grid.row_ids(), ids);
    }
}

#[test]
fn row_writes_match_captured_pre_extraction_edge_cases() {
    // Captured by executing the original put_char body before removal, using
    // the same grid implementation. No second emulator is retained as an oracle.
    for line in include_str!("fixtures/legacy_screen_rows.tsv")
        .lines()
        .filter(|line| !line.starts_with('#'))
    {
        let columns: Vec<_> = line.split('\t').collect();
        let width: usize = columns[0].parse().unwrap();
        let col: usize = columns[1].parse().unwrap();
        let mut s = ScreenState::new(3, 8);
        for (ch, width) in [('a', 1), ('界', 2), ('b', 1), ('文', 2), ('c', 1)] {
            s.write_char_in_row(ch, width, true);
        }
        s.cursor_col = col;
        s.pending_wrap = false;
        s.insert_mode = columns[2].parse().unwrap();
        if s.prepare_character_width(width, true) {
            wrap(&mut s);
        }
        s.write_char_in_row(if width == 1 { 'x' } else { '文' }, width, true);
        assert_eq!(s.cursor_row.to_string(), columns[3], "{line}");
        assert_eq!(s.cursor_col.to_string(), columns[4], "{line}");
        assert_eq!(s.pending_wrap.to_string(), columns[5], "{line}");
        let grid = s
            .grid
            .iter()
            .map(|row| {
                row.iter()
                    .map(|c| {
                        format!(
                            "{:x}:{}{}",
                            c.character as u32,
                            u8::from(c.flags.wide()),
                            u8::from(c.flags.wide_continuation())
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .collect::<Vec<_>>()
            .join("|");
        assert_eq!(grid, columns[6], "{line}");
    }
}
