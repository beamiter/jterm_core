use jterm_engine::terminal::Terminal;
#[test]
fn reverse_wrap_mode_negotiation_saved_cache_and_resets_are_observable() {
    let mut terminal = Terminal::new(3, 4).unwrap();
    assert_eq!(terminal.feed(b"\x1b[?45$p").unwrap(), b"\x1b[?45;2$y");
    assert_eq!(
        terminal
            .feed(b"\x1b[?45h\x1b[?45s\x1b[?45l\x1b[?45$p\x1b[?45r\x1b[?45$p")
            .unwrap(),
        b"\x1b[?45;2$y\x1b[?45;1$y"
    );
    assert_eq!(
        terminal
            .feed(b"\x1b[!p\x1b[?45$p\x1b[?45r\x1b[?45$p")
            .unwrap(),
        b"\x1b[?45;2$y\x1b[?45;1$y"
    );
    assert_eq!(
        terminal.feed(b"\x1bc\x1b[?45r\x1b[?45$p").unwrap(),
        b"\x1b[?45;2$y"
    );
}
#[test]
fn actual_soft_wrapping_and_cursor_back_survive_every_feed_partition() {
    for backward in [b"\x08".as_slice(), b"\x1b[D", b"\x1b[j"] {
        let packet = [b"\x1b[?45habcde\x1b[2;1H".as_slice(), backward, b"\x1b[6n"].concat();
        for split in 0..=packet.len() {
            let mut terminal = Terminal::new(3, 4).unwrap();
            let mut replies = terminal.feed(&packet[..split]).unwrap();
            replies.extend(terminal.feed(&packet[split..]).unwrap());
            assert_eq!(replies, b"\x1b[1;4R");
            let frame = terminal.frame().unwrap();
            assert!(frame.rows()[0].wrapped);
            assert_eq!(
                frame.rows()[0]
                    .cells
                    .iter()
                    .map(|c| c.character)
                    .collect::<String>(),
                "abcd"
            );
            assert_eq!(frame.rows()[1].cells[0].character, 'e');
        }
    }
}
#[test]
fn pending_wrap_hard_lines_off_modes_and_namespace_guards_keep_exact_positions() {
    let cases: &[(&[u8], (usize, usize))] = &[
        (b"\x1b[?45habcd\x08", (0, 3)),
        (b"\x1b[?45habcd\x1b[2D", (0, 2)),
        (b"abcd\x08", (0, 2)),
        (b"\x1b[?45h\x1b[?7labcd\x08", (0, 2)),
        (b"\x1b[?45habc\r\n\x08", (1, 0)),
        (b"\x1b[?45habcde\x1b[2;1H\x1b[?2D", (1, 0)),
        (b"\x1b[?45habcde\x1b[2;1H\x1b[2$D", (1, 0)),
        (b"\x1b[?45habcde\x1b[2;1H\x1b[65535D", (0, 0)),
        (b"\x1b[?45habcde\x1b[2;3r\x1b[2;1H\x08", (0, 3)),
    ];
    for &(packet, position) in cases {
        let mut terminal = Terminal::new(3, 4).unwrap();
        terminal.feed(packet).unwrap();
        let cursor = terminal.frame().unwrap().cursor();
        assert_eq!((cursor.row, cursor.col), position, "{packet:?}");
        assert!(!cursor.pending_wrap);
    }
}
