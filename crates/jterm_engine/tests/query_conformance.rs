//! Independent wire expectations, not the frozen Ember differential oracle.
//! dispatch_local is Ember policy; this suite does not enable it for other hosts.
use jterm_engine::{
    parser::{Event, Parser, ParserLimits, ParserProfile, PendingKind},
    protocol_state::ProtocolState,
    screen::ScreenState,
    semantics::{dispatch_local, Dispatch, Effect},
    TerminalGrid,
};

// Check live owners, cursor, current drawing state, both grids and protocol.
// This is a sentinel snapshot, not a claim to cover every host-owned field.
fn snapshot(screen: &ScreenState, protocol: &ProtocolState) -> String {
    let mut out = format!(
        "{:?}",
        (
            screen.origin_mode,
            screen.insert_mode,
            screen.cursor_row,
            screen.cursor_col,
            screen.pending_wrap,
            screen.use_alt_buffer,
            screen.current_fg,
            screen.current_bg,
            screen.current_flags,
            screen.cursor_shape,
            protocol,
        )
    );
    for grid in [&screen.grid, &screen.alt_grid] {
        for row in 0..grid.rows() {
            out.push_str(&format!("{:?}", &grid[row]));
        }
    }
    out
}

fn run(bytes: &[u8], cuts: &[usize], enabled: bool, handled: bool) -> Vec<Vec<u8>> {
    let mut screen = ScreenState::with_grids(TerminalGrid::new(5, 9), TerminalGrid::new(5, 9));
    screen.origin_mode = enabled;
    screen.insert_mode = enabled;
    screen.cursor_row = 2;
    screen.cursor_col = 3;
    screen.grid[2][3].character = '界';
    let mut protocol = ProtocolState::default();
    // Deliberately contradict actual mode owners. Query must use live screen state.
    if !enabled {
        protocol.modes_mut().insert(4);
        protocol.modes_mut().insert(6);
    }
    if enabled {
        protocol.modes_mut().insert(25);
    }
    // Storage presence alone is not capability support.
    for mode in [20, 1016, 65000] {
        protocol.modes_mut().insert(mode);
    }
    let before = snapshot(&screen, &protocol);
    let mut parser = Parser::with_bounded_profile(
        ParserLimits {
            pending_bytes: 128,
            kitty_recovery_bytes: 32,
        },
        ParserProfile::EmberV1,
    )
    .unwrap();
    let mut replies = vec![];
    let mut start = 0;
    let mut events = 0;
    for end in cuts.iter().copied().chain(std::iter::once(bytes.len())) {
        parser.begin_feed();
        let input = &bytes[start..end];
        let mut at = 0;
        let mut drained = false;
        for _ in 0..input.len() * 4 + 32 {
            let step = parser.next(&input[at..]);
            assert!(step.consumed <= input.len() - at);
            at += step.consumed;
            match step.event {
                Some(Event::Boundary) => (),
                Some(event) => {
                    events += 1;
                    let result = dispatch_local(&mut screen, &mut protocol, &event);
                    if handled {
                        let Dispatch::Handled(effects) = result else {
                            panic!("query unhandled")
                        };
                        assert_eq!(effects.len(), 1, "query has extra side effects");
                        let Effect::Reply(reply) = &effects[0] else {
                            panic!("not a reply")
                        };
                        replies.push(reply.clone());
                    } else {
                        assert_eq!(result, Dispatch::Unhandled);
                    }
                    assert_eq!(snapshot(&screen, &protocol), before, "query mutated state");
                }
                None => {
                    assert_eq!(at, input.len());
                    drained = true;
                    break;
                }
            }
        }
        assert!(drained, "parser did not drain");
        start = end;
    }
    assert!(events > 0, "test must reach dispatcher");
    assert_eq!(parser.pending_kind(), PendingKind::None);
    assert_eq!(parser.rejected_count(), 0);
    replies
}
fn check(bytes: &[u8], enabled: bool, handled: bool, expected: &[&[u8]]) {
    let expected: Vec<Vec<u8>> = expected.iter().map(|b| b.to_vec()).collect();
    for cut in 0..=bytes.len() {
        assert_eq!(run(bytes, &[cut], enabled, handled), expected, "cut={cut}");
    }
    let cuts: Vec<_> = (0..=bytes.len()).flat_map(|n| [n, n]).collect();
    assert_eq!(run(bytes, &cuts, enabled, handled), expected);
}
#[test]
fn ansi_irm_and_dec_origin_report_live_state_with_exact_wire_form() {
    check(
        b"\x1b[4$p\x1b[?6$p",
        false,
        true,
        &[b"\x1b[4;2$y", b"\x1b[?6;2$y"],
    );
    check(
        b"\x1b[4$p\x1b[?6$p",
        true,
        true,
        &[b"\x1b[4;1$y", b"\x1b[?6;1$y"],
    );
}
#[test]
fn recognized_dec_cursor_visibility_reports_set_and_reset() {
    check(b"\x1b[?25$p", false, true, &[b"\x1b[?25;2$y"]);
    check(b"\x1b[?25$p", true, true, &[b"\x1b[?25;1$y"]);
}
#[test]
fn local_capability_policy_does_not_advertise_stored_unsupported_modes() {
    // 20 permanently reset and 1016 unsupported are Ember policy, not DEC mandates.
    for enabled in [false, true] {
        check(
            b"\x1b[20$p\x1b[?1016$p\x1b[?65000$p\x1b[65000$p",
            enabled,
            true,
            &[
                b"\x1b[20;4$y",
                b"\x1b[?1016;0$y",
                b"\x1b[?65000;0$y",
                b"\x1b[65000;0$y",
            ],
        );
    }
}
#[test]
fn lookalike_queries_are_not_claimed_as_decrqm() {
    for bytes in [b"\x1b[>4$p".as_slice(), b"\x1b[?6p", b"\x1b[?6$z"] {
        check(bytes, true, false, &[]);
    }
}
