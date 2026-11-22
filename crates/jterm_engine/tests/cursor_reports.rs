//! Literal user-visible reports plus historical Frost differential, kept separate.
use jterm_engine::{
    parser::{Event, Parser, ParserLimits},
    protocol_state::ProtocolState,
    screen::ScreenState,
    semantics::{dispatch_local, frost_legacy_cursor_position_reply, Dispatch, Effect},
};

fn ember_queries(screen: &mut ScreenState, stream: &[u8]) -> Vec<Vec<u8>> {
    let mut parser = Parser::new(ParserLimits {
        pending_bytes: 128,
        kitty_recovery_bytes: 32,
    });
    let mut protocol = ProtocolState::default();
    let mut replies = vec![];
    // Feed one byte at a time, including each CSI prefix and final byte.
    for byte in stream {
        parser.begin_feed();
        let mut input = std::slice::from_ref(byte);
        let mut drained = false;
        for _ in 0..8 {
            let step = parser.next(input);
            input = &input[step.consumed..];
            match step.event {
                Some(Event::Boundary) => (),
                Some(event) => {
                    let Dispatch::Handled(effects) = dispatch_local(screen, &mut protocol, &event)
                    else {
                        panic!("unhandled status event {event:?}");
                    };
                    for effect in effects {
                        let Effect::Reply(bytes) = effect else {
                            panic!("status query changed state: {effect:?}");
                        };
                        replies.push(bytes);
                    }
                }
                None => {
                    assert!(input.is_empty());
                    drained = true;
                    break;
                }
            }
        }
        assert!(drained, "parser did not drain this byte");
    }
    replies
}

#[test]
fn ember_cursor_report_honors_origin_and_wrap_in_both_namespaces() {
    let mut screen = ScreenState::new(6, 12);
    screen.scroll_region_top = 2;
    screen.scroll_region_bottom = 4;
    screen.cursor_row = 3;
    screen.cursor_col = 11;
    screen.origin_mode = true;
    screen.pending_wrap = true;
    assert_eq!(
        ember_queries(&mut screen, b"\x1b[5n\x1b[?6n\x1b[6n"),
        [b"\x1b[0n".as_slice(), b"\x1b[?2;13R", b"\x1b[2;13R"],
    );
    assert_eq!((screen.cursor_row, screen.cursor_col), (3, 11));
    assert!(screen.origin_mode && screen.pending_wrap);
    screen.origin_mode = false;
    screen.pending_wrap = false;
    assert_eq!(
        ember_queries(&mut screen, b"\x1b[6n"),
        [b"\x1b[4;12R".as_slice()],
    );
}

#[test]
fn frost_absolute_wire_and_legacy_width_are_explicit() {
    assert_eq!(
        frost_legacy_cursor_position_reply(3, 11, false),
        b"\x1b[4;12R"
    );
    assert_eq!(
        frost_legacy_cursor_position_reply(3, 11, true),
        b"\x1b[?4;12R"
    );
    // Out-of-grid inputs deliberately exercise the historic width distinction,
    // not an assertion that Frost can construct a screen of this size.
    assert_eq!(
        frost_legacy_cursor_position_reply(65535, 65536, false),
        b"\x1b[0;1R"
    );
    assert_eq!(
        frost_legacy_cursor_position_reply(65535, 65536, true),
        b"\x1b[?65536;65537R"
    );
}

#[test]
fn frost_wire_matches_original_host_formulas_at_boundaries() {
    // Frost 5635699, src/terminal.rs, DSR 6 and DEC-private DSR 6 arms.
    for row in [0, 1, 511, 65534, 65535, 65536] {
        for col in [0, 1, 1023, 65534, 65535, 65536] {
            assert_eq!(
                frost_legacy_cursor_position_reply(row, col, false),
                format!("\x1b[{};{}R", (row + 1) as u16, (col + 1) as u16).as_bytes()
            );
            assert_eq!(
                frost_legacy_cursor_position_reply(row, col, true),
                format!("\x1b[?{};{}R", row + 1, col + 1).as_bytes()
            );
        }
    }
}
