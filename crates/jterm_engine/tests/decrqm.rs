use jterm_engine::{
    parser::Event,
    protocol_state::ProtocolState,
    screen::{Charset, SavedCursorState, ScreenState},
    semantics::{dispatch_local, Dispatch, Effect},
    Color, CursorShape, HyperlinkId, TerminalGrid, UnderlineStyle,
};
// Original Ember 4f8f829e5d22a746f9654be44282989b394486ef method bodies below.
struct Original {
    screen: ScreenState,
    protocol: ProtocolState,
    global_bg: Color,
    output_buffer: Vec<u8>,
}
// Verbatim baseline method bodies; not a replacement implementation.
impl Original {
    fn decrqm_private_mode_state(&self, mode: u16) -> u8 {
        let set = match mode {
            6 => self.screen.origin_mode,
            1
            | 7
            | 12
            | 25
            | 47
            | 66
            | 1000..=1007
            | 1015
            | 1034
            | 1036
            | 1047..=1049
            | 2004
            | 2026
            | 2031
            | 5522 => self.protocol.modes().contains(&mode),
            _ => return 0,
        };
        if set {
            1
        } else {
            2
        }
    }

    fn decrqm_ansi_mode_state(&self, mode: u16) -> u8 {
        match mode {
            4 if self.screen.insert_mode => 1,
            4 => 2,
            20 => 4,
            _ => 0,
        }
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
    s.origin_mode = seed & 1 != 0;
    s.insert_mode = seed & 2 != 0;
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

fn event(params: &[u16], prefix: Option<u8>, intermediates: &[u8]) -> Event<'static> {
    Event::Csi {
        raw_params: b"999:888;777".to_vec(),
        params: params.to_vec(),
        colon_flags: vec![true; params.len()],
        private_prefix: prefix,
        intermediates: intermediates.to_vec(),
        final_byte: b'p',
    }
}
fn check(t: &mut Original, params: &[u16], prefix: Option<u8>, retention: bool) {
    let mode = params.first().copied().unwrap_or(0);
    let status = if prefix.is_some() {
        t.decrqm_private_mode_state(mode)
    } else {
        t.decrqm_ansi_mode_state(mode)
    };
    let expected = format!(
        "\x1b[{}{};{}$y",
        if prefix.is_some() { "?" } else { "" },
        mode,
        status
    )
    .into_bytes();
    let before = retention.then(|| snapshot(t));
    let got = dispatch_local(&mut t.screen, &mut t.protocol, &event(params, prefix, b"$"));
    assert_eq!(
        got,
        Dispatch::Handled(vec![Effect::Reply(expected)].into()),
        "mode={mode},prefix={prefix:?}"
    );
    if let Some(before) = before {
        assert_eq!(snapshot(t), before, "query mutated screen/protocol");
    }
}
#[test]
fn exhaustive_decrqm_matches_frozen_ember() {
    for flags in 0..4 {
        let mut t = seeded(flags);
        for mode in 0..=u16::MAX {
            for stored in [false, true] {
                if stored {
                    t.protocol.modes_mut().insert(mode)
                } else {
                    t.protocol.modes_mut().remove(&mode)
                };
                // Query only current mode, independent of unrelated accumulated mode bits.
                check(&mut t, &[mode], None, false);
                check(&mut t, &[mode], Some(b'?'), false);
            }
        }
    }
}
#[test]
fn defaults_first_parameter_and_ignored_raw_colon_data() {
    for flags in 0..4 {
        let mut t = seeded(flags);
        for params in [
            vec![],
            vec![0],
            vec![4, 20],
            vec![6, 7],
            vec![65535, 1],
            vec![20, 4],
            vec![1],
            vec![7],
            vec![12],
            vec![25],
            vec![47],
            vec![66],
            vec![1000],
            vec![1001],
            vec![1002],
            vec![1003],
            vec![1004],
            vec![1005],
            vec![1006],
            vec![1007],
            vec![1015],
            vec![1016],
            vec![1034],
            vec![1036],
            vec![1047],
            vec![1048],
            vec![1049],
            vec![2004],
            vec![2026],
            vec![2031],
            vec![5522],
        ] {
            for prefix in [None, Some(b'?')] {
                check(&mut t, &params, prefix, true);
            }
        }
    }
}
#[test]
fn malformed_decrqm_is_transactionally_unhandled() {
    let mut t = seeded(3);
    for prefix in [None, Some(b'?'), Some(b'>'), Some(b'<'), Some(b'=')] {
        for intermediates in [b"".as_slice(), b"$", b"$$", b" $", b"$ ", b"!"] {
            // CSI ! p without prefix is DECSTR, not malformed DECRQM.
            if (intermediates == b"$" && matches!(prefix, None | Some(b'?')))
                || (intermediates == b"!" && prefix.is_none())
            {
                continue;
            }
            let before = snapshot(&t);
            assert_eq!(
                dispatch_local(
                    &mut t.screen,
                    &mut t.protocol,
                    &event(&[6], prefix, intermediates)
                ),
                Dispatch::Unhandled
            );
            assert_eq!(snapshot(&t), before);
        }
    }
}

#[test]
fn decrqm_wrong_final_is_not_claimed() {
    let mut t = seeded(3);
    for prefix in [None, Some(b'?')] {
        let mut query = event(&[6], prefix, b"$");
        if let Event::Csi { final_byte, .. } = &mut query {
            *final_byte = b'q';
        }
        let before = snapshot(&t);
        assert_eq!(
            dispatch_local(&mut t.screen, &mut t.protocol, &query),
            Dispatch::Unhandled
        );
        assert_eq!(snapshot(&t), before);
    }
}
