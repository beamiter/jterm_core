//! Independent all-u16 differential against pre-extraction branches.
#[path = "support/mode_mutation_reference.rs"]
mod reference;
use jterm_engine::{
    mode_mutation::{apply_ember_mode, ModeNamespace},
    protocol_state::ProtocolState,
    screen::ScreenState,
};
const KNOWN: [u16; 24] = [
    1, 7, 25, 1000, 1001, 1002, 1003, 1004, 1006, 1049, 2004, 2026, 2031, 5522, 47, 1047, 1048,
    1005, 1015, 66, 1007, 12, 1036, 1034,
];
const HOST: [u16; 7] = [1004, 47, 1047, 1048, 1049, 2026, 5522];
fn seeded(initial: bool) -> reference::Frozen {
    let mut screen = ScreenState::new(4, 5);
    screen.cursor_row = 3;
    screen.cursor_col = 4;
    screen.scroll_region_top = 2;
    screen.pending_wrap = initial;
    screen.origin_mode = initial;
    screen.insert_mode = initial;
    let mut protocol = ProtocolState::default();
    if initial {
        for mode in KNOWN {
            protocol.modes_mut().insert(mode);
        }
    }
    protocol.set_keyboard_enhancement_flags(19, 1);
    protocol.push_keyboard_enhancement_flags(23);
    protocol.switch_screen();
    protocol.set_xterm_modify_other_keys(2);
    protocol.set_xterm_format_other_keys(1);
    reference::Frozen { screen, protocol }
}
#[test]
fn mode_mutation_exhaustive_frozen_reference() {
    for initial in [false, true] {
        for namespace in [ModeNamespace::Ansi, ModeNamespace::DecPrivate] {
            for enabled in [false, true] {
                for mode in 0..=u16::MAX {
                    let mut actual = seeded(initial);
                    let mut expected = seeded(initial);
                    let expected_handled = match (namespace, enabled) {
                        (ModeNamespace::Ansi, true) => {
                            expected.set_ansi_mode(mode);
                            true
                        }
                        (ModeNamespace::Ansi, false) => {
                            expected.reset_ansi_mode(mode);
                            true
                        }
                        (ModeNamespace::DecPrivate, true) => expected.set_mode(mode),
                        (ModeNamespace::DecPrivate, false) => expected.reset_mode(mode),
                    };
                    let handled = apply_ember_mode(
                        &mut actual.screen,
                        &mut actual.protocol,
                        namespace,
                        mode,
                        enabled,
                    );
                    let private = namespace == ModeNamespace::DecPrivate;
                    let host = private && HOST.contains(&mode);
                    assert_eq!(handled, expected_handled, "{namespace:?} {mode} {enabled}");
                    assert_eq!(handled, !host);
                    let a = &actual.screen;
                    let e = &expected.screen;
                    assert_eq!(
                        (
                            a.cursor_row,
                            a.cursor_col,
                            a.origin_mode,
                            a.insert_mode,
                            a.pending_wrap
                        ),
                        (
                            e.cursor_row,
                            e.cursor_col,
                            e.origin_mode,
                            e.insert_mode,
                            e.pending_wrap
                        )
                    );
                    assert_eq!(
                        format!("{:?}", actual.protocol),
                        format!("{:?}", expected.protocol)
                    );
                    // Explicit slot oracle also guards against coupling through TerminalModes.
                    for known in KNOWN {
                        assert_eq!(
                            actual.protocol.modes().contains(&known),
                            if private && !host && mode == known {
                                enabled
                            } else {
                                initial
                            },
                            "{namespace:?} {mode} {enabled} slot {known}"
                        );
                    }
                    if !KNOWN.contains(&mode) {
                        assert!(!actual.protocol.modes().contains(&mode));
                    }
                }
            }
        }
    }
}
#[test]
fn mode_mutation_host_hooks_are_completely_transactional() {
    for initial in [false, true] {
        for mode in HOST {
            for enabled in [false, true] {
                let mut actual = seeded(initial);
                let before = snapshot(&actual.screen, &actual.protocol);
                assert!(!apply_ember_mode(
                    &mut actual.screen,
                    &mut actual.protocol,
                    ModeNamespace::DecPrivate,
                    mode,
                    enabled
                ));
                assert_eq!(snapshot(&actual.screen, &actual.protocol), before);
            }
        }
    }
}

fn snapshot(s: &ScreenState, protocol: &ProtocolState) -> Vec<String> {
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
    out.push(format!("{protocol:?}"));
    out
}
