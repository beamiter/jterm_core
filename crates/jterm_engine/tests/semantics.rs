#[path = "support/semantic_sgr_reference.rs"]
mod reference;
use jterm_engine::{
    parser::Event,
    protocol_state::ProtocolState,
    screen::ScreenState,
    semantics::{dispatch_local, Dispatch, Effect},
    Color, TerminalGrid,
};

fn screen() -> ScreenState {
    ScreenState::with_grids(TerminalGrid::new(4, 8), TerminalGrid::new(4, 8))
}
fn csi(final_byte: u8, params: &[u16], prefix: Option<u8>, intermediates: &[u8]) -> Event<'static> {
    Event::Csi {
        raw_params: Vec::new(),
        final_byte,
        params: params.to_vec(),
        colon_flags: vec![false; params.len()],
        private_prefix: prefix,
        intermediates: intermediates.to_vec(),
    }
}
fn handled(
    screen: &mut ScreenState,
    protocol: &mut ProtocolState,
    event: &Event<'_>,
) -> Vec<Effect> {
    match dispatch_local(screen, protocol, event) {
        Dispatch::Handled(effects) => effects.into_vec(),
        Dispatch::Unhandled => panic!("unhandled {event:?}"),
    }
}
fn snapshot(s: &ScreenState, p: &ProtocolState) -> String {
    format!(
        "{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
        s.grid.to_vec(),
        s.alt_grid.to_vec(),
        (s.cursor_row, s.cursor_col, s.pending_wrap),
        (s.current_fg, s.current_bg, s.current_flags),
        (s.g0_charset, s.g1_charset, s.active_charset),
        s.saved_state,
        s.tab_stops,
        (s.scroll_region_top, s.scroll_region_bottom),
        (s.origin_mode, s.insert_mode, s.use_alt_buffer),
        s.cursor_shape,
        s.last_printed_char,
        s.current_hyperlink,
        p
    )
}

#[test]
fn unhandled_events_never_mutate_even_mixed_host_hook_modes() {
    let mut s = screen();
    let mut p = ProtocolState::default();
    s.pending_wrap = true;
    s.cursor_row = 2;
    s.current_bg = Color::Red;
    p.modes_mut().insert(7);
    for event in [
        csi(b'h', &[7, 1004, 2026, 47], Some(b'?'), &[]),
        csi(b'l', &[7, 5522], Some(b'?'), &[]),
        csi(b'J', &[2], None, &[]),
        csi(b'n', &[996], Some(b'?'), &[]),
        csi(b'q', &[0], Some(b'>'), &[]),
        csi(b'P', &[1], Some(b'?'), &[]),
        Event::Control(10),
        Event::Print('x'),
        Event::Escape {
            intermediates: vec![],
            final_byte: b'c',
        },
    ] {
        let before = snapshot(&s, &p);
        assert_eq!(dispatch_local(&mut s, &mut p, &event), Dispatch::Unhandled);
        assert_eq!(snapshot(&s, &p), before);
    }
}

#[test]
fn sgr_matches_frozen_original_on_adversarial_sequences() {
    let mut original = reference::Original {
        screen: screen(),
        global_bg: Color::Blue,
    };
    let mut s = screen();
    let mut p = ProtocolState::default();
    let mut bg = Color::Blue;
    original.screen.current_flags.set_protected(true);
    s.current_flags.set_protected(true);
    let fixtures = [
        vec![],
        vec![0],
        vec![49],
        vec![41, 0, 49],
        vec![4, 1],
        vec![4, 3],
        vec![38, 2, 0, 255, 256, 65535],
        vec![58, 2, 1, 2, 3, 1],
        vec![48, 5, 65535],
        vec![38, 2, 1],
        vec![48, 2, 0, 1, 2, 3, 49],
    ];
    for params in fixtures {
        for colons in [false, true] {
            let flags: Vec<_> = (0..params.len()).map(|i| i > 0 && colons).collect();
            original.handle_sgr(&params, &flags);
            let event = Event::Csi {
                raw_params: Vec::new(),
                params: params.clone(),
                colon_flags: flags,
                private_prefix: None,
                intermediates: vec![],
                final_byte: b'm',
            };
            for effect in handled(&mut s, &mut p, &event) {
                match effect {
                    Effect::GlobalBackground(color) => bg = color,
                    _ => panic!("unexpected effect"),
                }
            }
            assert_eq!(
                (s.current_fg, s.current_bg, s.current_flags, bg),
                (
                    original.screen.current_fg,
                    original.screen.current_bg,
                    original.screen.current_flags,
                    original.global_bg
                )
            );
        }
    }
    let alphabet = [
        0, 1, 2, 3, 4, 5, 7, 9, 21, 22, 23, 24, 25, 27, 29, 30, 39, 40, 48, 49, 58, 59, 90, 107,
        255, 256, 65535,
    ];
    let mut random = 1_u64;
    for _ in 0..4000 {
        let mut params = Vec::new();
        let mut flags = Vec::new();
        for i in 0..12 {
            random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
            params.push(alphabet[random as usize % alphabet.len()]);
            flags.push(i > 0 && (random >> 32) & 1 == 1);
        }
        original.handle_sgr(&params, &flags);
        let event = Event::Csi {
            raw_params: Vec::new(),
            params: params.clone(),
            colon_flags: flags,
            private_prefix: None,
            intermediates: vec![],
            final_byte: b'm',
        };
        for effect in handled(&mut s, &mut p, &event) {
            if let Effect::GlobalBackground(color) = effect {
                bg = color;
            }
        }
        assert_eq!(
            (s.current_fg, s.current_bg, s.current_flags, bg),
            (
                original.screen.current_fg,
                original.screen.current_bg,
                original.screen.current_flags,
                original.global_bg
            )
        );
    }
}

#[test]
fn background_effects_preserve_original_order_and_do_not_infer_resets() {
    let mut s = screen();
    let mut p = ProtocolState::default();
    assert_eq!(
        handled(&mut s, &mut p, &csi(b'm', &[41, 44, 49, 0], None, &[])),
        [
            Effect::GlobalBackground(Color::Red),
            Effect::GlobalBackground(Color::Blue)
        ]
    );
    assert_eq!(s.current_bg, Color::Default);
    assert!(handled(&mut s, &mut p, &csi(b'm', &[], None, &[])).is_empty());
}

#[test]
fn cursor_queries_keep_origin_and_last_column_flag_semantics() {
    let mut s = screen();
    let mut p = ProtocolState::default();
    s.origin_mode = true;
    s.scroll_region_top = 1;
    s.cursor_row = 2;
    s.cursor_col = 7;
    s.pending_wrap = true;
    assert_eq!(
        handled(&mut s, &mut p, &csi(b'n', &[6], None, &[])),
        [Effect::Reply(b"\x1b[2;9R".to_vec())]
    );
    assert_eq!(
        handled(&mut s, &mut p, &csi(b'n', &[6], Some(b'?'), &[])),
        [Effect::Reply(b"\x1b[?2;9R".to_vec())]
    );
    assert!(s.pending_wrap);
    assert_eq!(
        handled(&mut s, &mut p, &csi(b'n', &[5], None, &[])),
        [Effect::Reply(b"\x1b[0n".to_vec())]
    );
}

#[test]
fn cursor_prelude_also_applies_to_existing_private_format_keys_branch() {
    let mut s = screen();
    let mut p = ProtocolState::default();
    s.pending_wrap = true;
    assert_eq!(
        handled(&mut s, &mut p, &csi(b'f', &[4, 1], Some(b'>'), &[])),
        [Effect::CursorReposition { non_linear: true }]
    );
    assert!(!s.pending_wrap);
    assert_eq!(p.xterm_format_other_keys(), 1);
    s.cursor_col = 4;
    assert_eq!(
        handled(&mut s, &mut p, &csi(b'D', &[0], None, &[])),
        [Effect::CursorReposition { non_linear: false }]
    );
    assert_eq!(s.cursor_col, 3);
}

#[test]
fn protected_only_el_zero_cancels_wrap_without_claiming_mutation() {
    let mut s = screen();
    let mut p = ProtocolState::default();
    for cell in s.grid[0].iter_mut() {
        cell.flags.set_protected(true);
        cell.character = 'x';
    }
    s.pending_wrap = true;
    assert!(handled(&mut s, &mut p, &csi(b'K', &[0], Some(b'?'), &[])).is_empty());
    assert!(!s.pending_wrap);
    assert_eq!(s.grid[0][0].character, 'x');
    let effects = handled(&mut s, &mut p, &csi(b'X', &[1], None, &[]));
    assert!(matches!(&effects[0],Effect::Row(effect) if effect.printed.is_none()));
}

#[test]
fn keyboard_query_replies_and_saved_cursor_are_ordered() {
    let mut s = screen();
    let mut p = ProtocolState::default();
    s.cursor_col = 5;
    handled(
        &mut s,
        &mut p,
        &Event::Escape {
            intermediates: vec![],
            final_byte: b'7',
        },
    );
    handled(&mut s, &mut p, &csi(b'G', &[1], None, &[]));
    handled(
        &mut s,
        &mut p,
        &Event::Escape {
            intermediates: vec![],
            final_byte: b'8',
        },
    );
    assert_eq!(s.cursor_col, 5);
    handled(&mut s, &mut p, &csi(b'u', &[17], Some(b'='), &[]));
    assert_eq!(
        handled(&mut s, &mut p, &csi(b'u', &[], Some(b'?'), &[])),
        [Effect::Reply(b"\x1b[?17u".to_vec())]
    );
}
