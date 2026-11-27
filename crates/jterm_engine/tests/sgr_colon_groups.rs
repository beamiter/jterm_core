use jterm_engine::{
    parser::{Event, Parser, ParserLimits, ParserProfile},
    protocol_state::ProtocolState,
    screen::ScreenState,
    semantics::{dispatch_local, Dispatch},
    terminal::Terminal,
    Color, StyleFlags, TerminalCell, UnderlineStyle,
};
struct Case {
    sequence: &'static [u8],
    foreground: Color,
    background: Color,
    flags: [bool; 6],
    underline: UnderlineStyle,
}
fn assert_rendition(cell: TerminalCell, expected: &Case) {
    assert_eq!(
        (cell.foreground, cell.background),
        (expected.foreground, expected.background),
        "{:?}",
        expected.sequence
    );
    let flags = cell.flags;
    assert_eq!(
        [
            flags.bold(),
            flags.dim(),
            flags.italic(),
            flags.blink(),
            flags.inverse(),
            flags.strikethrough()
        ],
        expected.flags,
        "{:?}",
        expected.sequence
    );
    assert_eq!(
        flags.underline(),
        expected.underline,
        "{:?}",
        expected.sequence
    );
}
#[test]
fn atomic_colon_groups_reach_hosted_paint_and_actual_ember_parser_at_every_split() {
    use Color::*;
    use UnderlineStyle::*;
    let cases = [
        Case {
            sequence: b"38:9:1;32",
            foreground: Green,
            background: Default,
            flags: [false; 6],
            underline: None,
        },
        Case {
            sequence: b"48:2::10:20:30:7",
            foreground: Default,
            background: Rgb(10, 20, 30),
            flags: [false; 6],
            underline: None,
        },
        Case {
            sequence: b"38:2:10;4",
            foreground: Default,
            background: Default,
            flags: [false; 6],
            underline: Single,
        },
        Case {
            sequence: b"38:5:256;3",
            foreground: Default,
            background: Default,
            flags: [false, false, true, false, false, false],
            underline: None,
        },
        Case {
            sequence: b"38:2::256:1:2;7",
            foreground: Default,
            background: Default,
            flags: [false, false, false, false, true, false],
            underline: None,
        },
        Case {
            sequence: b"38;5:123:1;3",
            foreground: Indexed(123),
            background: Default,
            flags: [false, false, true, false, false, false],
            underline: None,
        },
        Case {
            sequence: b"38;2:10:20:30;4:3",
            foreground: Rgb(10, 20, 30),
            background: Default,
            flags: [false; 6],
            underline: Curly,
        },
        Case {
            sequence: b"38;2::10:20:30:9;4:2",
            foreground: Rgb(10, 20, 30),
            background: Default,
            flags: [false; 6],
            underline: Double,
        },
        Case {
            sequence: b"4:3:1;32",
            foreground: Green,
            background: Default,
            flags: [false; 6],
            underline: Curly,
        },
        Case {
            sequence: b"4:9;5",
            foreground: Default,
            background: Default,
            flags: [false, false, false, true, false, false],
            underline: Single,
        },
        Case {
            sequence: b"1:4;32",
            foreground: Green,
            background: Default,
            flags: [true, false, false, false, false, false],
            underline: None,
        },
        Case {
            sequence: b"48:2::1:2;7",
            foreground: Default,
            background: Rgb(0, 1, 2),
            flags: [false, false, false, false, true, false],
            underline: None,
        },
        // Pure semicolon conversion intentionally retains legacy byte wrapping.
        Case {
            sequence: b"38;2;256;257;258",
            foreground: Rgb(0, 1, 2),
            background: Default,
            flags: [false; 6],
            underline: None,
        },
    ];
    for case in &cases {
        let packet = [b"\x1b[".as_slice(), case.sequence, b"m"].concat();
        for split in 0..=packet.len() {
            let mut terminal = Terminal::new(2, 8).unwrap();
            terminal.feed(&packet[..split]).unwrap();
            terminal.feed(&packet[split..]).unwrap();
            terminal.feed(b"X").unwrap();
            assert_rendition(terminal.frame().unwrap().rows()[0].cells[0], case);
            let mut parser = Parser::with_bounded_profile(
                ParserLimits {
                    pending_bytes: 256,
                    kitty_recovery_bytes: 32,
                },
                ParserProfile::EmberV1,
            )
            .unwrap();
            let mut screen = ScreenState::new(2, 8);
            let mut protocol = ProtocolState::default();
            for chunk in [&packet[..split], &packet[split..]] {
                parser.begin_feed();
                let mut offset = 0;
                loop {
                    let step = parser.next(&chunk[offset..]);
                    offset += step.consumed;
                    match step.event {
                        Some(Event::Boundary) => {}
                        Some(event) => {
                            assert!(matches!(
                                dispatch_local(&mut screen, &mut protocol, &event),
                                Dispatch::Handled(_)
                            ));
                        }
                        Option::None => break,
                    }
                }
            }
            assert_rendition(
                TerminalCell {
                    foreground: screen.current_fg,
                    background: screen.current_bg,
                    flags: screen.current_flags,
                    ..TerminalCell::default()
                },
                case,
            );
        }
    }
}
#[test]
fn malformed_colon_groups_cannot_change_existing_style_through_namespaces() {
    for prefix in [b"?".as_slice(), b">", b"=", b"<"] {
        let mut terminal = Terminal::new(2, 8).unwrap();
        terminal
            .feed(&[b"\x1b[".as_slice(), prefix, b"38:9:1;32m"].concat())
            .unwrap();
        terminal.feed(b"X").unwrap();
        let cell = terminal.frame().unwrap().rows()[0].cells[0];
        assert_eq!(
            (cell.foreground, cell.background, cell.flags),
            (Color::Default, Color::Default, StyleFlags::default())
        );
    }
}
