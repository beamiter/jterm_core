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

#[test]
fn attribute_extent_status_is_replayable_and_never_changes_the_visible_frame() {
    let query = b"\x1bP$q*x\x1b\\";
    for split in 0..=query.len() {
        for (setting, expected) in [
            (b"\x1b[0*x".as_slice(), b"\x1bP1$r0*x\x1b\\".as_slice()),
            (b"\x1b[1*x", b"\x1bP1$r1*x\x1b\\"),
            (b"\x1b[2*x", b"\x1bP1$r2*x\x1b\\"),
        ] {
            let mut terminal = Terminal::new(3, 8).unwrap();
            terminal.feed(b"ABCD\r\nEFGH").unwrap();
            terminal.feed(setting).unwrap();
            let before = terminal.frame().unwrap();
            let mut reply = terminal.feed(&query[..split]).unwrap();
            reply.extend(terminal.feed(&query[split..]).unwrap());
            assert_eq!(reply, expected, "setting {setting:?}, split {split}");
            assert_eq!(terminal.frame().unwrap(), before);

            terminal.feed(b"\x1b[2*x").unwrap();
            let payload = reply
                .strip_prefix(b"\x1bP1$r")
                .unwrap()
                .strip_suffix(b"\x1b\\")
                .unwrap();
            terminal
                .feed(&[b"\x1b[".as_slice(), payload].concat())
                .unwrap();
            terminal.feed(b"\x1b[1;2;2;3;1$r").unwrap();
            let frame = terminal.frame().unwrap();
            assert_eq!(
                frame.rows()[0].cells[3].flags.bold(),
                expected != b"\x1bP1$r2*x\x1b\\"
            );
            assert_eq!(
                frame.rows()[1].cells[0].flags.bold(),
                expected != b"\x1bP1$r2*x\x1b\\"
            );
        }
    }
}

#[test]
fn extent_status_follows_soft_and_hard_reset_and_rejects_selector_lookalikes() {
    let mut terminal = Terminal::new(3, 8).unwrap();
    terminal.feed(b"\x1b[2*x\x1b[!p").unwrap();
    assert_eq!(
        terminal.feed(b"\x1bP$q*x\x1b\\").unwrap(),
        b"\x1bP1$r2*x\x1b\\"
    );
    for selector in [b"*x ".as_slice(), b" *x", b"2*x", b"*X", b"**x"] {
        let packet = [b"\x1bP$q".as_slice(), selector, b"\x1b\\"].concat();
        assert_eq!(terminal.feed(&packet).unwrap(), b"\x1bP0$r\x1b\\");
    }
    for invalid in [
        b"\x1b[9*x".as_slice(),
        b"\x1b[1:2*x",
        b"\x1b[1;2*x",
        b"\x1b[?1*x",
    ] {
        terminal.feed(invalid).unwrap();
        assert_eq!(
            terminal.feed(b"\x1bP$q*x\x1b\\").unwrap(),
            b"\x1bP1$r2*x\x1b\\"
        );
    }
    terminal.feed(b"\x1b[*x").unwrap();
    assert_eq!(
        terminal.feed(b"\x1bP$q*x\x1b\\").unwrap(),
        b"\x1bP1$r0*x\x1b\\"
    );
    terminal.feed(b"\x1b[2*x\x1bc").unwrap();
    assert_eq!(
        terminal.feed(b"\x1bP$q*x\x1b\\").unwrap(),
        b"\x1bP1$r0*x\x1b\\"
    );
}

#[test]
fn screen_line_status_uses_live_geometry_and_never_changes_visible_state() {
    let query = b"\x1bP$q*|\x1b\\";
    for (rows, cols) in [(1, 5), (12, 42), (31, 96)] {
        for split in 0..=query.len() {
            let mut terminal = Terminal::new(rows, cols).unwrap();
            terminal.feed(b"AB\x1b[1;1H\x1b[31m").unwrap();
            let before = terminal.frame().unwrap();
            let mut replies = terminal.feed(&query[..split]).unwrap();
            replies.extend(terminal.feed(&query[split..]).unwrap());
            assert_eq!(replies, format!("\x1bP1$r{rows}*|\x1b\\").as_bytes());
            assert_eq!(terminal.frame().unwrap(), before);
            terminal.resize(rows + 2, cols + 3).unwrap();
            assert_eq!(
                terminal.feed(query).unwrap(),
                format!("\x1bP1$r{}*|\x1b\\", rows + 2).as_bytes()
            );
        }
    }
}

#[test]
fn screen_line_status_ignores_margins_and_rejects_selector_lookalikes() {
    let mut terminal = Terminal::new(12, 42).unwrap();
    terminal
        .feed(b"\x1b[2;4r\x1b[?6h\x1b[2;3H\x1b[?1049h")
        .unwrap();
    assert_eq!(
        terminal.feed(b"\x1bP$q*|\x1b\\").unwrap(),
        b"\x1bP1$r12*|\x1b\\"
    );
    for selector in [b"*| ".as_slice(), b" *|", b"2*|", b"**|", b"*x|"] {
        let query = [b"\x1bP$q".as_slice(), selector, b"\x1b\\"].concat();
        assert_eq!(terminal.feed(&query).unwrap(), b"\x1bP0$r\x1b\\");
    }
}
