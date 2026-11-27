use jterm_engine::terminal::Terminal;

#[test]
fn hosted_sgr_status_restores_the_complete_pen_after_temporary_changes() {
    let mut terminal = Terminal::new(3, 16).unwrap();
    terminal.enable_underline_colors();
    terminal.feed(b"\x1b[31;4:3;58;5;9mA").unwrap();
    let original = terminal.frame().unwrap();
    let query = b"\x1bP$qm\x1b\\";
    for split in 0..=query.len() {
        let mut reply = terminal.feed(&query[..split]).unwrap();
        reply.extend(terminal.feed(&query[split..]).unwrap());
        assert_eq!(reply, b"\x1bP1$r0;4:3;31;58;5;9m\x1b\\");
        terminal.feed(b"\x1b[1;2;3;5;7;9;44;58;5;7m").unwrap();
        let payload = reply
            .strip_prefix(b"\x1bP1$r")
            .unwrap()
            .strip_suffix(b"\x1b\\")
            .unwrap();
        terminal
            .feed(&[b"\x1b[".as_slice(), payload, b"B"].concat())
            .unwrap();
        let restored = terminal.frame().unwrap();
        let reference = original.rows()[0].cells[0];
        let current = restored.rows()[0].cells[split + 1];
        assert_eq!(current.foreground, reference.foreground);
        assert_eq!(current.background, reference.background);
        assert_eq!(current.flags, reference.flags);
        assert_eq!(
            restored.underline_color(0, split + 1),
            original.underline_color(0, 0)
        );
    }
}

#[test]
fn keyboard_status_queries_restore_negotiated_wire_encoding() {
    for chunk in 1..=8 {
        let mut terminal = Terminal::new(3, 16).unwrap();
        terminal.feed(b"\x1b[>4;2m\x1b[>4;1f").unwrap();
        let mut replies = Vec::new();
        for bytes in b"\x1bP$q>4m\x1b\\\x1bP$q>04f\x1b\\".chunks(chunk) {
            replies.extend(terminal.feed(bytes).unwrap());
        }
        assert_eq!(replies, b"\x1bP1$r>4;2m\x1b\\\x1bP1$r>4;1f\x1b\\");
        terminal.feed(b"\x1b[>4;0m\x1b[>4;0f").unwrap();
        let split = b"\x1bP1$r>4;2m\x1b\\".len();
        for report in [&replies[..split], &replies[split..]] {
            let payload = report
                .strip_prefix(b"\x1bP1$r")
                .unwrap()
                .strip_suffix(b"\x1b\\")
                .unwrap();
            terminal
                .feed(&[b"\x1b[".as_slice(), payload].concat())
                .unwrap();
        }
        assert_eq!(
            terminal.encode_key("a", false, false, true).unwrap(),
            b"\x1b[97;5u"
        );
        for selector in [b">3m".as_slice(), b">4;1m", b">4 m", b">40m"] {
            let query = [b"\x1bP$q".as_slice(), selector, b"\x1b\\"].concat();
            assert_eq!(terminal.feed(&query).unwrap(), b"\x1bP0$r\x1b\\");
        }
    }
}
