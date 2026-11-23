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

#[test]
fn fragmented_nonempty_dsr_intermediates_emit_no_reply_or_grid_change() {
    use jterm_engine::terminal::Terminal;

    let setup = b"\x1b[?2004h\x1b[2;4r\x1b[?6h\x1b[2;10HX";
    let valid = b"\x1b[5n\x1b[6n\x1b[?6n\x1b[?2004$p";
    let valid_replies = b"\x1b[0n\x1b[2;11R\x1b[?2;11R\x1b[?2004;1$y";
    for introduction in [b"5".as_slice(), b"6", b"?6"] {
        for intermediate in 0x20..=0x2f {
            let mut query = b"\x1b[".to_vec();
            query.extend_from_slice(introduction);
            query.extend_from_slice(&[intermediate, b'n']);
            for cut in 0..=query.len() {
                let mut terminal = Terminal::new(5, 10).unwrap();
                terminal.feed(setup).unwrap();
                let before = terminal.frame().unwrap();
                assert_eq!(before.cursor().paint_position(), (2, 9));
                assert!(before.cursor().pending_wrap);

                let mut reply = terminal.feed(&query[..cut]).unwrap();
                reply.extend(terminal.feed(&query[cut..]).unwrap());
                assert!(reply.is_empty(), "query {query:?} cut {cut}: {reply:?}");
                let after = terminal.frame().unwrap();
                assert_eq!(after.cursor(), before.cursor());
                assert_eq!(after.rows(), before.rows());
                assert_eq!(after.stamp().screen, before.stamp().screen);
                assert_eq!(after.stamp().layout, before.stamp().layout);

                assert_eq!(terminal.feed(valid).unwrap(), valid_replies);
                // Retain the first-parameter compatibility policy in the
                // original no-intermediate request family.
                assert_eq!(
                    terminal.feed(b"\x1b[6;0n\x1b[?6;0n").unwrap(),
                    b"\x1b[2;11R\x1b[?2;11R"
                );
            }
        }
    }
}
