use jterm_engine::grid::{Color, CursorShape, RawRowId, StyleFlags, TerminalCell, UnderlineStyle};
use jterm_engine::screen::{Charset, SavedCursorState, ScreenState};
use jterm_engine::HyperlinkId;

fn flags(style: usize) -> StyleFlags {
    let mut flags = StyleFlags::default();
    flags.set_bold(style != 0);
    flags.set_italic(style % 2 == 1);
    flags.set_underline(
        [
            UnderlineStyle::None,
            UnderlineStyle::Single,
            UnderlineStyle::Double,
            UnderlineStyle::Curly,
            UnderlineStyle::Dotted,
            UnderlineStyle::Dashed,
        ][style],
    );
    flags.set_inverse(style > 1);
    flags.set_dim(style > 2);
    flags.set_blink(style > 3);
    flags.set_strikethrough(style > 4);
    flags.set_protected(style != 0);
    flags
}

fn saved(rows: usize, cols: usize, style: usize) -> SavedCursorState {
    SavedCursorState {
        row: rows - 1,
        col: cols - 1,
        fg: Color::BrightCyan,
        bg: Color::Indexed(231),
        flags: flags(style),
        g0: Charset::DecSpecialGraphics,
        g1: Charset::Ascii,
        active: Charset::DecSpecialGraphics,
        origin_mode: true,
        autowrap: style % 2 == 0,
        pending_wrap: true,
    }
}

fn seeded(rows: usize, cols: usize, style: usize, alternate: bool) -> ScreenState {
    let mut s = ScreenState::new(rows, cols);
    for (buffer, grid) in [&mut s.grid, &mut s.alt_grid].into_iter().enumerate() {
        for row in 0..rows {
            grid.row_ids_mut()[row] = if (row + buffer) % 3 == 0 {
                RawRowId::UNTRACKED
            } else {
                RawRowId::new((100 * buffer + row + 1) as u64)
            };
            grid.row_wrapped[row] = (row + buffer) % 2 == 0;
            for col in 0..cols {
                grid[row][col] = TerminalCell {
                    character: ['界', 'é', '\0', ' ', 'X'][(row + col + buffer) % 5],
                    foreground: Color::Rgb(11, 22, 33),
                    background: Color::Indexed((row + col + 100 * buffer) as u8),
                    flags: flags((style + row + col + buffer) % 6),
                    hyperlink_id: HyperlinkId::from_raw((buffer + row + col + 1) as u16),
                };
            }
            // Include real wide pairs and a truncated wide lead at the edge,
            // including the historical one-column truncated-wide case.
            for col in (0..cols.saturating_sub(1)).step_by(2) {
                grid[row][col].character = '界';
                grid[row][col].flags.set_wide(true);
                grid[row][col + 1].character = ' ';
                grid[row][col + 1].flags.set_wide_continuation(true);
            }
            if cols % 2 == 1 {
                grid[row][cols - 1].flags.set_wide(true);
            }
        }
    }
    s.use_alt_buffer = alternate;
    s.saved_cursor_row = rows - 1;
    s.saved_cursor_col = cols - 1;
    s.alt_cursor_row = rows / 2;
    s.alt_cursor_col = cols / 2;
    s.cursor_shape = [
        CursorShape::Block,
        CursorShape::Underline,
        CursorShape::Beam,
    ][style % 3];
    s.saved_primary_cursor_shape = (style != 0).then_some(CursorShape::Beam);
    s.saved_state = (style % 2 == 0).then(|| saved(rows, cols, style));
    s.saved_primary_screen_state = (style != 0).then(|| saved(rows, cols, 5 - style));
    s.insert_mode = style % 2 == 0;
    s.last_printed_char = (style != 0).then_some('界');
    s.tab_stops = (0..cols).map(|col| (col + style) % 2 == 0).collect();
    s.current_fg = Color::Rgb(23, 67, 89);
    s.current_bg = [
        Color::Default,
        Color::Black,
        Color::Blue,
        Color::BrightMagenta,
        Color::Indexed(255),
        Color::Rgb(17, 83, 191),
    ][style];
    s.current_flags = flags(style);
    s.current_hyperlink = HyperlinkId::from_raw(77);
    s.g0_charset = Charset::DecSpecialGraphics;
    s.g1_charset = if style % 2 == 0 {
        Charset::Ascii
    } else {
        Charset::DecSpecialGraphics
    };
    s.active_charset = s.g1_charset;
    s
}

// Independent physical/state portions of the original Ember parser.rs methods
// in fullscreen-fill-baseline, SHA256:
// 34631e6e44ecd614fbf9f9f1638db3167be00adb5c479dfd558caf99b0fe5c2b.
// Deliberately do not call any engine erase/fill, blank-cell, cursor or margin
// helper. Host dirty/provenance/graphics callbacks are outside ScreenState.
fn original(s: &mut ScreenState, alignment: bool) {
    if alignment {
        let rows = s.grid.rows();
        let filled = TerminalCell {
            character: 'E',
            ..TerminalCell::default()
        };
        for row in s.grid.iter_mut() {
            row.fill(filled);
        }
        s.scroll_region_top = 0;
        s.scroll_region_bottom = rows.saturating_sub(1);
        s.cursor_row = 0;
        s.cursor_col = 0;
        s.pending_wrap = false;
    } else {
        s.pending_wrap = false;
        let bg_color = s.current_bg;
        for row in s.grid.iter_mut() {
            for cell in row.iter_mut() {
                *cell = TerminalCell {
                    character: ' ',
                    foreground: Color::Default,
                    background: bg_color,
                    flags: StyleFlags::default(),
                    hyperlink_id: HyperlinkId::NONE,
                };
            }
        }
    }
}

fn assert_saved(actual: &Option<SavedCursorState>, expected: &Option<SavedCursorState>) {
    match (actual, expected) {
        (Some(a), Some(b)) => {
            // Exhaustive destructuring catches newly added state. Typed flags
            // equality matters: StyleFlags Debug omits the two wide bits.
            let SavedCursorState {
                row,
                col,
                fg,
                bg,
                flags,
                g0,
                g1,
                active,
                origin_mode,
                autowrap,
                pending_wrap,
            } = a;
            assert_eq!(
                (
                    *row,
                    *col,
                    *fg,
                    *bg,
                    *flags,
                    *g0,
                    *g1,
                    *active,
                    *origin_mode,
                    *autowrap,
                    *pending_wrap
                ),
                (
                    b.row,
                    b.col,
                    b.fg,
                    b.bg,
                    b.flags,
                    b.g0,
                    b.g1,
                    b.active,
                    b.origin_mode,
                    b.autowrap,
                    b.pending_wrap
                )
            );
        }
        (None, None) => {}
        _ => panic!("saved-state presence changed"),
    }
}

fn assert_screen(a: &ScreenState, b: &ScreenState) {
    // No `..`: adding a ScreenState field requires updating this comparison.
    let ScreenState {
        grid,
        alt_grid,
        use_alt_buffer,
        cursor_row,
        cursor_col,
        saved_cursor_row,
        saved_cursor_col,
        alt_cursor_row,
        alt_cursor_col,
        cursor_shape,
        saved_primary_cursor_shape,
        saved_state,
        saved_primary_screen_state,
        insert_mode,
        last_printed_char,
        origin_mode,
        tab_stops,
        pending_wrap,
        current_fg,
        current_bg,
        current_flags,
        current_hyperlink,
        scroll_region_top,
        scroll_region_bottom,
        g0_charset,
        g1_charset,
        active_charset,
    } = a;
    for (actual, expected) in [(grid, &b.grid), (alt_grid, &b.alt_grid)] {
        assert_eq!(
            (actual.rows(), actual.cols()),
            (expected.rows(), expected.cols())
        );
        assert_eq!(actual.row_ids(), expected.row_ids());
        assert_eq!(actual.row_wrapped, expected.row_wrapped);
        assert_eq!(actual.to_vec(), expected.to_vec());
    }
    macro_rules! equal {
        ($($field:ident),+ $(,)?) => { $(assert_eq!($field, &b.$field, stringify!($field));)+ };
    }
    equal!(
        use_alt_buffer,
        cursor_row,
        cursor_col,
        saved_cursor_row,
        saved_cursor_col,
        alt_cursor_row,
        alt_cursor_col,
        cursor_shape,
        saved_primary_cursor_shape,
        insert_mode,
        last_printed_char,
        origin_mode,
        tab_stops,
        pending_wrap,
        current_fg,
        current_bg,
        current_flags,
        current_hyperlink,
        scroll_region_top,
        scroll_region_bottom,
        g0_charset,
        g1_charset,
        active_charset
    );
    assert_saved(saved_state, &b.saved_state);
    assert_saved(saved_primary_screen_state, &b.saved_primary_screen_state);
}

fn apply(s: &mut ScreenState, alignment: bool) {
    if alignment {
        s.screen_alignment_test();
    } else {
        s.erase_screen();
    }
}

#[test]
fn fullscreen_fills_match_original_for_small_geometry_and_repeated_sequences() {
    let mut cases = 0;
    for rows in 1..=3 {
        for cols in 1..=4 {
            for row in 0..rows {
                for col in 0..cols {
                    for top in 0..rows {
                        for bottom in top..rows {
                            for style in 0..6 {
                                for alternate in [false, true] {
                                    for pending in [false, true] {
                                        for origin in [false, true] {
                                            for sequence in 0..4 {
                                                let mut a = seeded(rows, cols, style, alternate);
                                                let mut b = seeded(rows, cols, style, alternate);
                                                for s in [&mut a, &mut b] {
                                                    s.cursor_row = row;
                                                    s.cursor_col = col;
                                                    s.scroll_region_top = top;
                                                    s.scroll_region_bottom = bottom;
                                                    s.pending_wrap = pending;
                                                    s.origin_mode = origin;
                                                }
                                                for alignment in
                                                    [sequence & 1 != 0, sequence & 2 != 0]
                                                {
                                                    apply(&mut a, alignment);
                                                    original(&mut b, alignment);
                                                    assert_screen(&a, &b);
                                                }
                                                cases += 1;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    assert_eq!(cases, 48_000);
}

#[test]
fn fullscreen_fills_erase_protected_wide_cells_outside_margins_without_replacing_rows() {
    for alternate in [false, true] {
        for alignment in [false, true] {
            for cols in [1, 2, 5, 9] {
                let mut a = seeded(5, cols, 5, alternate);
                let mut b = seeded(5, cols, 5, alternate);
                for s in [&mut a, &mut b] {
                    s.cursor_row = 4;
                    s.cursor_col = cols - 1;
                    s.scroll_region_top = 1;
                    s.scroll_region_bottom = 3;
                    s.pending_wrap = true;
                    s.origin_mode = true;
                    for cell in s.grid.cells_mut() {
                        cell.flags.set_protected(true);
                    }
                }
                for _ in 0..3 {
                    apply(&mut a, alignment);
                    original(&mut b, alignment);
                    assert_screen(&a, &b);
                    let fill = TerminalCell {
                        character: if alignment { 'E' } else { ' ' },
                        foreground: Color::Default,
                        background: if alignment {
                            Color::Default
                        } else {
                            Color::Rgb(17, 83, 191)
                        },
                        flags: StyleFlags::default(),
                        hyperlink_id: HyperlinkId::NONE,
                    };
                    assert!(a.grid.iter().flatten().all(|cell| *cell == fill));
                    assert!(!a.pending_wrap);
                    assert!(a.origin_mode);
                    assert_eq!(a.last_printed_char, Some('界'));
                    assert_eq!(
                        (a.cursor_row, a.cursor_col),
                        if alignment { (0, 0) } else { (4, cols - 1) }
                    );
                    assert_eq!(
                        (a.scroll_region_top, a.scroll_region_bottom),
                        if alignment { (0, 4) } else { (1, 3) }
                    );
                }
            }
        }
    }
}
