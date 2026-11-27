use jterm_engine::{screen::ScreenState, terminal::Terminal, Color, StyleFlags};

#[test]
fn hosted_alignment_resets_origin_before_homing_at_every_feed_partition() {
    let packet = b"\x1b[2;4r\x1b[?6h\x1b[2;3H\x1b#8\x1b[?6$p\x1bP$qr\x1b\\\x1b[6n";
    for alternate in [false, true] {
        for split in 0..=packet.len() {
            let mut terminal = Terminal::new(8, 12).unwrap();
            if alternate {
                terminal.feed(b"\x1b[?1049h").unwrap();
            }
            let mut reply = terminal.feed(&packet[..split]).unwrap();
            reply.extend(terminal.feed(&packet[split..]).unwrap());
            assert_eq!(
                reply, b"\x1b[?6;2$y\x1bP1$r1;8r\x1b\\\x1b[1;1R",
                "alternate {alternate}, split {split}"
            );
            let frame = terminal.frame().unwrap();
            assert_eq!((frame.cursor().row, frame.cursor().col), (0, 0));
            for row in frame.rows() {
                assert!(row.cells.iter().all(|cell| cell.character == 'E'));
            }
            // DECOM was also removed from the protocol's stored mode set.
            terminal.feed(b"\x1b[?6s\x1b[?6h\x1b[?6r").unwrap();
            assert_eq!(terminal.feed(b"\x1b[?6$p").unwrap(), b"\x1b[?6;2$y");
            terminal.feed(b"\x1b[3;5H").unwrap();
            assert_eq!(terminal.feed(b"\x1b[6n").unwrap(), b"\x1b[3;5R");

            terminal.feed(b"\x1b[3;6r\x1b[1;1H").unwrap();
            assert_eq!(terminal.frame().unwrap().cursor().row, 0);
            terminal.feed(b"\x1b[?6h\x1b[1;1H").unwrap();
            assert_eq!(terminal.frame().unwrap().cursor().row, 2);
        }
    }
}

#[test]
fn shared_alignment_fill_retains_native_origin_and_drawing_state() {
    let mut screen = ScreenState::new(8, 12);
    screen.origin_mode = true;
    screen.current_fg = Color::Red;
    screen.current_bg = Color::Blue;
    let mut flags = StyleFlags::default();
    flags.set_bold(true);
    flags.set_protected(true);
    screen.current_flags = flags;
    screen.scroll_region_top = 1;
    screen.scroll_region_bottom = 3;
    screen.screen_alignment_test();
    assert!(screen.origin_mode);
    assert_eq!(
        (screen.current_fg, screen.current_bg),
        (Color::Red, Color::Blue)
    );
    assert_eq!(screen.current_flags, flags);
    assert_eq!(
        (screen.scroll_region_top, screen.scroll_region_bottom),
        (0, 7)
    );
    assert_eq!((screen.cursor_row, screen.cursor_col), (0, 0));
}

#[test]
fn hosted_alignment_resets_visual_flags_and_retains_colors_and_protection() {
    let setup = b"\x1b[1;2;3;4;5;7;9;31;44m\x1b[1\"q";
    let packet = b"\x1b#8\x1bP$qm\x1b\\\x1bP$q\"q\x1b\\X";
    for alternate in [false, true] {
        for split in 0..=packet.len() {
            let mut terminal = Terminal::new(4, 8).unwrap();
            if alternate {
                terminal.feed(b"\x1b[?1049h").unwrap();
            }
            terminal.feed(setup).unwrap();
            let mut reply = terminal.feed(&packet[..split]).unwrap();
            reply.extend(terminal.feed(&packet[split..]).unwrap());
            assert_eq!(
                reply, b"\x1bP1$r0;31;44m\x1b\\\x1bP1$r1\"q\x1b\\",
                "alternate {alternate}, split {split}"
            );
            let frame = terminal.frame().unwrap();
            let cell = frame.rows()[0].cells[0];
            assert_eq!(cell.character, 'X');
            assert_eq!(
                (cell.foreground, cell.background),
                (Color::Red, Color::Blue)
            );
            let mut expected = StyleFlags::default();
            expected.set_protected(true);
            assert_eq!(cell.flags, expected);
            assert!(frame.rows()[1].cells.iter().all(|cell| {
                cell.character == 'E'
                    && cell.foreground == Color::Default
                    && cell.background == Color::Default
                    && cell.flags == StyleFlags::default()
            }));
        }
    }
}
