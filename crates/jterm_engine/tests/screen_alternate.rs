use jterm_engine::grid::{Color, CursorShape, RawRowId, StyleFlags, TerminalCell};
use jterm_engine::protocol_state::ProtocolState;
use jterm_engine::screen::{Charset, SavedCursorState, ScreenState};
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

// Independent original physical/state portions of Ember enter_alt_screen and
// exit_alt_screen. Original parser.rs SHA256:
// 6841a4102e291c617017c3436252c19ed41f9f7b8dc0b9f1929130971930c95d.
// Host callbacks are applied by the harness, not inferred from engine methods.
fn original_enter(s: &mut ScreenState, p: &ProtocolState, save: bool, clear: bool) -> bool {
    if s.use_alt_buffer {
        return false;
    }
    if save {
        s.saved_cursor_row = s.cursor_row;
        s.saved_cursor_col = s.cursor_col;
        s.saved_primary_screen_state = Some(SavedCursorState {
            row: s.cursor_row,
            col: s.cursor_col,
            fg: s.current_fg,
            bg: s.current_bg,
            flags: s.current_flags,
            g0: s.g0_charset,
            g1: s.g1_charset,
            active: s.active_charset,
            origin_mode: s.origin_mode,
            autowrap: p.modes().contains(&7),
            pending_wrap: s.pending_wrap,
        });
    }
    s.saved_primary_cursor_shape = Some(s.cursor_shape);
    std::mem::swap(&mut s.grid, &mut s.alt_grid);
    s.alt_cursor_row = s.cursor_row;
    s.alt_cursor_col = s.cursor_col;
    s.use_alt_buffer = true;
    s.current_hyperlink = HyperlinkId::NONE;
    s.scroll_region_top = 0;
    s.scroll_region_bottom = s.grid.rows().saturating_sub(1);
    if clear {
        let bg = s.current_bg;
        for row in s.grid.iter_mut() {
            for cell in row.iter_mut() {
                *cell = TerminalCell {
                    character: ' ',
                    foreground: Color::Default,
                    background: bg,
                    flags: StyleFlags::default(),
                    hyperlink_id: HyperlinkId::NONE,
                };
            }
        }
        s.cursor_row = 0;
        s.cursor_col = 0;
    }
    true
}

fn original_leave(s: &mut ScreenState, restore: bool) -> Option<(Color, Option<bool>)> {
    if !s.use_alt_buffer {
        return None;
    }
    s.alt_cursor_row = s.cursor_row;
    s.alt_cursor_col = s.cursor_col;
    std::mem::swap(&mut s.grid, &mut s.alt_grid);
    if restore {
        s.cursor_row = s.saved_cursor_row;
        s.cursor_col = s.saved_cursor_col;
    }
    s.use_alt_buffer = false;
    if let Some(shape) = s.saved_primary_cursor_shape.take() {
        s.cursor_shape = shape;
    }
    s.current_hyperlink = HyperlinkId::NONE;
    s.scroll_region_top = 0;
    s.scroll_region_bottom = s.grid.rows().saturating_sub(1);
    let autowrap;
    let global_bg;
    if let Some(saved) = s.saved_primary_screen_state.take() {
        s.current_fg = saved.fg;
        s.current_bg = saved.bg;
        global_bg = saved.bg;
        s.current_flags = saved.flags;
        s.g0_charset = saved.g0;
        s.g1_charset = saved.g1;
        s.active_charset = saved.active;
        s.origin_mode = saved.origin_mode;
        autowrap = Some(saved.autowrap);
    } else {
        s.current_fg = Color::Default;
        s.current_bg = Color::Default;
        global_bg = Color::Default;
        s.current_flags = StyleFlags::default();
        autowrap = None;
    }
    s.pending_wrap = false;
    Some((global_bg, autowrap))
}

fn snapshot(s: &ScreenState, p: &ProtocolState, global_bg: Color) -> String {
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
            global_bg
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
        p
    )
}

fn step(
    s: &mut ScreenState,
    p: &mut ProtocolState,
    global_bg: &mut Color,
    action: usize,
    original: bool,
) {
    match action {
        0..=2 => {
            let save = action == 2;
            let clear = action != 0;
            let changed = if original {
                original_enter(s, p, save, clear)
            } else {
                let effect = s.enter_alternate_screen(save, clear, p);
                if let Some(ref effect) = effect {
                    assert_eq!(effect.cleared_rows, clear.then_some(0..s.grid.rows()));
                }
                effect.is_some()
            };
            if changed {
                p.switch_screen();
            }
            p.modes_mut().insert([47, 1047, 1049][action]);
        }
        3..=5 => {
            let effect = if original {
                original_leave(s, action == 5)
            } else {
                s.leave_alternate_screen(action == 5).map(|effect| {
                    assert_eq!(effect.dirty_rows, 0..s.grid.rows());
                    (effect.global_background, effect.restore_autowrap)
                })
            };
            if let Some((bg, autowrap)) = effect {
                p.switch_screen();
                *global_bg = bg;
                if let Some(autowrap) = autowrap {
                    if autowrap {
                        p.modes_mut().insert(7);
                    } else {
                        p.modes_mut().remove(&7);
                    }
                }
            }
            p.modes_mut().remove(&[47, 1047, 1049][action - 3]);
        }
        6 => s.save_cursor_state(p),
        7 => s.restore_cursor_state(p),
        8 => {
            s.cursor_row = s.grid.rows() - 1;
            s.cursor_col = 0;
            s.cursor_shape = CursorShape::Beam;
            s.current_fg = Color::Green;
            s.current_bg = Color::Magenta;
            s.current_flags.set_italic(true);
            s.current_hyperlink = HyperlinkId::from_raw(44);
            s.g0_charset = Charset::DecSpecialGraphics;
            s.active_charset = Charset::Ascii;
            s.origin_mode = false;
            s.pending_wrap = true;
            s.grid[0][0].character = 'X';
            if p.modes().contains(&7) {
                p.modes_mut().remove(&7);
            } else {
                p.modes_mut().insert(7);
            }
            p.push_keyboard_enhancement_flags(16);
        }
        9 => {
            s.resize_geometry(2, 3, 0, || RawRowId::UNTRACKED);
        }
        _ => unreachable!(),
    }
}

#[test]
fn alternate_transitions_match_original_algorithm_sequences() {
    let mut cases = 0;
    for (rows, cols) in [(1, 1), (3, 4), (5, 9)] {
        for variant in 0..4 {
            for a in 0..10 {
                for b in 0..10 {
                    for c in 0..10 {
                        let (mut actual, mut ap) = seeded(rows, cols, variant);
                        let (mut expected, mut ep) = seeded(rows, cols, variant);
                        let (mut abg, mut ebg) = (Color::Yellow, Color::Yellow);
                        for action in [a, b, c] {
                            step(&mut actual, &mut ap, &mut abg, action, false);
                            step(&mut expected, &mut ep, &mut ebg, action, true);
                            let context = format!("{rows}x{cols} variant {variant}, actions {a},{b},{c}, step {action}");
                            assert_eq!(
                                snapshot(&actual, &ap, abg),
                                snapshot(&expected, &ep, ebg),
                                "{context}"
                            );
                            assert_eq!(actual.grid.to_vec(), expected.grid.to_vec(), "{context}");
                            assert_eq!(
                                actual.alt_grid.to_vec(),
                                expected.alt_grid.to_vec(),
                                "{context}"
                            );
                        }
                        cases += 1;
                    }
                }
            }
        }
    }
    assert_eq!(cases, 12000);
}

#[test]
fn clear_entry_preserves_pending_wrap_row_ids_wrap_flags_and_shared_decsc() {
    let (mut s, p) = seeded(3, 4, 0);
    let hidden_ids = s.alt_grid.row_ids().to_vec();
    let hidden_wraps = s.alt_grid.row_wrapped.clone();
    let decsc = format!("{:?}", s.saved_state);
    let effect = s.enter_alternate_screen(true, true, &p).unwrap();
    assert_eq!(effect.cleared_rows, Some(0..3));
    assert_eq!(s.grid.row_ids(), hidden_ids);
    assert_eq!(s.grid.row_wrapped, hidden_wraps);
    assert_eq!(format!("{:?}", s.saved_state), decsc);
    assert!(s.pending_wrap);
    assert_eq!((s.cursor_row, s.cursor_col), (0, 0));
    assert_eq!((s.alt_cursor_row, s.alt_cursor_col), (2, 3));
    assert!(s
        .grid
        .iter()
        .flatten()
        .all(|cell| *cell == s.create_blank_cell()));
}

#[test]
fn mode47_does_not_load_saved_alternate_cursor_and_unsaved_exit_retains_modes() {
    let (mut s, mut p) = seeded(3, 4, 0);
    let cells = s.alt_grid.to_vec();
    let before_protocol = format!("{p:?}");
    assert_eq!(
        s.enter_alternate_screen(false, false, &p)
            .unwrap()
            .cleared_rows,
        None
    );
    assert_eq!(s.grid.to_vec(), cells);
    assert_eq!((s.cursor_row, s.cursor_col), (2, 3));
    assert_eq!(format!("{p:?}"), before_protocol); // host owns the stack switch
    s.g0_charset = Charset::DecSpecialGraphics;
    s.origin_mode = false;
    s.cursor_shape = CursorShape::Beam;
    p.modes_mut().remove(&7);
    let effect = s.leave_alternate_screen(false).unwrap();
    assert_eq!(effect.global_background, Color::Default);
    assert_eq!(effect.restore_autowrap, None);
    assert_eq!(s.g0_charset, Charset::DecSpecialGraphics);
    assert!(!s.origin_mode);
    assert!(!p.modes().contains(&7));
    assert_eq!(s.cursor_shape, CursorShape::Underline);
    assert_eq!(
        (s.current_fg, s.current_bg, s.current_flags),
        (Color::Default, Color::Default, StyleFlags::default())
    );
}

#[test]
fn restore_drawing_state_is_independent_of_restore_cursor_and_cancels_wrap() {
    let (mut s, p) = seeded(3, 4, 0);
    s.enter_alternate_screen(true, true, &p).unwrap();
    s.cursor_row = 1;
    s.cursor_col = 1;
    s.current_bg = Color::Green;
    let effect = s.leave_alternate_screen(false).unwrap();
    assert_eq!((s.cursor_row, s.cursor_col), (1, 1));
    assert_eq!((s.alt_cursor_row, s.alt_cursor_col), (1, 1));
    assert_eq!(s.current_bg, Color::Blue);
    assert_eq!(effect.global_background, Color::Blue);
    assert_eq!(effect.restore_autowrap, Some(true));
    assert!(!s.pending_wrap);
    assert!(s.saved_primary_screen_state.is_none());
    assert!(s.saved_primary_cursor_shape.is_none());
}

#[test]
fn repeated_transitions_do_not_clear_swap_or_overwrite_saved_slots() {
    let (mut s, p) = seeded(3, 4, 0);
    assert!(s.leave_alternate_screen(true).is_none());
    s.enter_alternate_screen(false, false, &p).unwrap();
    let before = snapshot(&s, &p, Color::Default);
    let cells = s.grid.to_vec();
    assert!(s.enter_alternate_screen(true, true, &p).is_none());
    assert_eq!(snapshot(&s, &p, Color::Default), before);
    assert_eq!(s.grid.to_vec(), cells);
    s.leave_alternate_screen(false).unwrap();
    let before = snapshot(&s, &p, Color::Default);
    assert!(s.leave_alternate_screen(true).is_none());
    assert_eq!(snapshot(&s, &p, Color::Default), before);
}
