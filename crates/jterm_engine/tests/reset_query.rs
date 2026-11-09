#[path = "support/reset_query_reference.rs"]
mod reference;
use jterm_engine::{
    parser::{Event, StringKind},
    protocol_state::ProtocolState,
    screen::{Charset, SavedCursorState, ScreenState},
    semantics::{dispatch_local, Dispatch, Effect},
    Color, CursorShape, HyperlinkId, TerminalGrid, UnderlineStyle,
};
use reference::Original;

fn csi(prefix: Option<u8>, intermediates: &[u8], params: &[u16]) -> Event<'static> {
    Event::Csi {
        raw_params: b"65535:0;1".to_vec(),
        params: params.to_vec(),
        colon_flags: vec![true; params.len()],
        private_prefix: prefix,
        intermediates: intermediates.to_vec(),
        final_byte: b'p',
    }
}
fn string(kind: StringKind, payload: &[u8], bel: bool) -> Event<'static> {
    Event::String {
        kind,
        payload: payload.to_vec().into(),
        bel_terminated: bel,
    }
}
fn seeded(seed: usize) -> Original {
    let mut s = ScreenState::with_grids(TerminalGrid::new(5, 9), TerminalGrid::new(5, 9));
    s.cursor_row = 3;
    s.cursor_col = 7;
    s.saved_cursor_row = 1;
    s.saved_cursor_col = 2;
    s.alt_cursor_row = 2;
    s.alt_cursor_col = 4;
    s.cursor_shape = [
        CursorShape::Block,
        CursorShape::Beam,
        CursorShape::Underline,
    ][seed % 3];
    s.saved_primary_cursor_shape = Some(CursorShape::Underline);
    s.current_fg = Color::Rgb(0, 128, 255);
    s.current_bg = Color::Indexed(201);
    s.current_flags.set_bold(true);
    s.current_flags.set_protected(seed & 1 != 0);
    s.current_flags.set_underline(UnderlineStyle::Curly);
    s.g0_charset = Charset::DecSpecialGraphics;
    s.g1_charset = Charset::DecSpecialGraphics;
    s.active_charset = Charset::DecSpecialGraphics;
    s.origin_mode = true;
    s.insert_mode = true;
    s.pending_wrap = true;
    s.scroll_region_top = 1;
    s.scroll_region_bottom = 3;
    s.use_alt_buffer = seed & 1 != 0;
    s.last_printed_char = Some('界');
    s.current_hyperlink = HyperlinkId::from_raw(65535);
    s.tab_stops = (0..9).map(|i| i % 3 == 1).collect();
    let saved = SavedCursorState {
        row: 2,
        col: 6,
        fg: Color::Red,
        bg: Color::Blue,
        flags: s.current_flags,
        g0: s.g0_charset,
        g1: s.g1_charset,
        active: s.active_charset,
        origin_mode: true,
        autowrap: false,
        pending_wrap: true,
    };
    s.saved_state = Some(saved.clone());
    s.saved_primary_screen_state = Some(saved);
    for (n, grid) in [&mut s.grid, &mut s.alt_grid].into_iter().enumerate() {
        for row in 0..grid.rows() {
            grid.row_wrapped[row] = row % 2 == n;
            grid.row_ids_mut()[row] = jterm_engine::RawRowId::new((100 + n * 5 + row) as u64);
            for col in 0..grid.cols() {
                grid[row][col].character =
                    char::from_u32(65 + (n * 45 + row * 9 + col) as u32).unwrap();
                grid[row][col].foreground = Color::Indexed((row * 9 + col) as u8);
                grid[row][col].hyperlink_id = HyperlinkId::from_raw((col + 1) as u16);
                grid[row][col].flags.set_protected(col % 2 == 0);
            }
        }
    }
    let mut p = ProtocolState::default();
    for mode in 0..=u16::MAX {
        if seed & 2 == 0 || mode % 2 == 0 {
            p.modes_mut().insert(mode);
        }
    }
    for value in [0, 0x8000, 0xffff, 0x4017] {
        p.push_keyboard_enhancement_flags(value);
    }
    p.switch_screen();
    for value in [0x5555, 0xaaaa, 0xfffe] {
        p.push_keyboard_enhancement_flags(value);
    }
    p.set_xterm_modify_other_keys(65535);
    p.set_xterm_format_other_keys(32768);
    Original {
        screen: s,
        protocol: p,
        global_bg: Color::Blue,
        output_buffer: b"existing-reply".to_vec(),
    }
}
fn snapshot(t: &Original) -> Vec<String> {
    let s = &t.screen;
    let mut out = Vec::new();
    macro_rules! fields { ($($field:ident),* $(,)?) => { $(out.push(format!("{}={:?}", stringify!($field), s.$field));)* }; }
    fields!(
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
        active_charset
    );
    for grid in [&s.grid, &s.alt_grid] {
        out.push(format!(
            "{:?}",
            (
                grid.rows(),
                grid.cols(),
                grid.to_vec(),
                &grid.row_wrapped,
                grid.row_ids()
            )
        ));
    }
    out.push(format!(
        "{:?}|{:?}|{:?}",
        t.protocol, t.global_bg, t.output_buffer
    ));
    out
}
fn run(t: &mut Original, event: &Event<'_>) -> Vec<Effect> {
    let Dispatch::Handled(effects) = dispatch_local(&mut t.screen, &mut t.protocol, event) else {
        panic!("unhandled {event:?}")
    };
    let effects = effects.into_vec();
    for effect in &effects {
        match effect {
            Effect::GlobalBackground(c) => t.global_bg = *c,
            Effect::Reply(bytes) => t.output_buffer.extend(bytes),
            _ => panic!("unexpected effect {effect:?}"),
        }
    }
    effects
}
#[test]
fn decstr_matches_frozen_host_and_retains_all_unrelated_state() {
    for seed in 0..8 {
        for params in [vec![], vec![0], vec![1, 65535, 0]] {
            let mut actual = seeded(seed);
            let mut expected = seeded(seed);
            expected.frozen_soft_reset();
            assert_eq!(
                run(&mut actual, &csi(None, b"!", &params)),
                [Effect::GlobalBackground(Color::Default)]
            );
            assert_eq!(snapshot(&actual), snapshot(&expected));
            for mode in 0..=u16::MAX {
                assert_eq!(
                    actual.protocol.modes().contains(&mode),
                    expected.protocol.modes().contains(&mode),
                    "mode {mode}"
                );
            }
            // Repeating DECSTR still emits the host background assignment.
            assert_eq!(
                run(&mut actual, &csi(None, b"!", &params)),
                [Effect::GlobalBackground(Color::Default)]
            );
            assert_eq!(snapshot(&actual), snapshot(&expected));
        }
    }
}
#[test]
fn decrqss_all_selectors_reply_exactly_without_state_mutation() {
    for seed in 0..6 {
        for selector in [
            b"m".as_slice(),
            b"r",
            b" q",
            b"\"q",
            b"\"p",
            b"",
            b"unknown",
            b"m\0",
            b"m;",
            b"\xff",
        ] {
            for bel in [false, true] {
                let mut actual = seeded(seed);
                let mut expected = seeded(seed);
                expected.frozen_handle_decrqss(selector);
                let payload = [b"$q".as_slice(), selector].concat();
                let effects = run(&mut actual, &string(StringKind::Dcs, &payload, bel));
                assert_eq!(
                    effects,
                    [Effect::Reply(
                        expected.output_buffer[b"existing-reply".len()..].to_vec()
                    )]
                );
                assert_eq!(snapshot(&actual), snapshot(&expected));
            }
        }
    }
}
#[test]
fn dispatch_requires_exact_decstr_shape_or_dcs_query_prefix() {
    let mut t = seeded(3);
    let mut events = vec![
        csi(Some(b'?'), b"!", &[]),
        csi(Some(b'>'), b"!", &[1]),
        csi(None, b"!!", &[]),
        csi(None, b" !", &[]),
        csi(None, b"", &[]),
        string(StringKind::Dcs, b"x$qm", false),
        string(StringKind::Dcs, b"$", false),
        string(StringKind::Dcs, b"", false),
    ];
    for kind in [
        StringKind::Osc,
        StringKind::Apc,
        StringKind::Sos,
        StringKind::Pm,
    ] {
        events.push(string(kind, b"$qm", false));
    }
    for event in events {
        let before = snapshot(&t);
        assert_eq!(
            dispatch_local(&mut t.screen, &mut t.protocol, &event),
            Dispatch::Unhandled,
            "{event:?}"
        );
        assert_eq!(snapshot(&t), before);
    }
}
#[test]
fn sgr_query_matches_frozen_flags_colors_and_deterministic_adversarial_combinations() {
    let mut colors = vec![
        Color::Default,
        Color::Black,
        Color::Red,
        Color::Green,
        Color::Yellow,
        Color::Blue,
        Color::Magenta,
        Color::Cyan,
        Color::White,
        Color::BrightBlack,
        Color::BrightRed,
        Color::BrightGreen,
        Color::BrightYellow,
        Color::BrightBlue,
        Color::BrightMagenta,
        Color::BrightCyan,
        Color::BrightWhite,
    ];
    colors.extend((0..=255).map(Color::Indexed));
    for r in [0, 1, 127, 128, 254, 255] {
        for g in [0, 128, 255] {
            for b in [0, 128, 255] {
                colors.push(Color::Rgb(r, g, b));
            }
        }
    }
    let mut actual = seeded(0);
    let mut expected = seeded(0);
    let mut random = 0xdeadbeef_u64;
    for case in 0..4096 {
        random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
        for t in [&mut actual, &mut expected] {
            let f = &mut t.screen.current_flags;
            f.set_bold(case & 1 != 0);
            f.set_dim(case & 2 != 0);
            f.set_italic(case & 4 != 0);
            f.set_blink(case & 8 != 0);
            f.set_inverse(case & 16 != 0);
            f.set_strikethrough(case & 32 != 0);
            f.set_protected(case & 64 != 0);
            f.set_wide(case & 128 != 0);
            f.set_wide_continuation(case & 256 != 0);
            f.set_underline(
                [
                    UnderlineStyle::None,
                    UnderlineStyle::Single,
                    UnderlineStyle::Double,
                    UnderlineStyle::Curly,
                    UnderlineStyle::Dotted,
                    UnderlineStyle::Dashed,
                ][case % 6],
            );
            t.screen.current_fg = colors[case % colors.len()];
            t.screen.current_bg = colors[if case < colors.len() {
                colors.len() - 1 - case
            } else {
                (random >> 32) as usize % colors.len()
            }];
            t.output_buffer.clear();
        }
        expected.frozen_handle_decrqss(b"m");
        assert_eq!(
            run(&mut actual, &string(StringKind::Dcs, b"$qm", false)),
            [Effect::Reply(expected.output_buffer.clone())],
            "case {case}"
        );
        assert_eq!(snapshot(&actual), snapshot(&expected), "case {case}");
    }
}

#[test]
fn default_and_protected_only_sgr_reply_remains_zero_m() {
    for protected in [false, true] {
        let mut actual = seeded(0);
        let mut expected = seeded(0);
        for t in [&mut actual, &mut expected] {
            t.screen.current_fg = Color::Default;
            t.screen.current_bg = Color::Default;
            t.screen.current_flags = Default::default();
            t.screen.current_flags.set_protected(protected);
            t.output_buffer.clear();
        }
        expected.frozen_handle_decrqss(b"m");
        assert_eq!(expected.output_buffer, b"\x1bP1$r0m\x1b\\");
        assert_eq!(
            run(&mut actual, &string(StringKind::Dcs, b"$qm", false)),
            [Effect::Reply(expected.output_buffer.clone())]
        );
        assert_eq!(snapshot(&actual), snapshot(&expected));
    }
}
