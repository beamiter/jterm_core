use jterm_engine::terminal::Terminal;

#[test]
fn g2_g3_locking_and_single_shifts_survive_all_feed_partitions() {
    let packet = b"\x1b*0\x1bnq\x1b+B\x1boq\x1bNqq\x1b*B\x1b+0\x0f\x1bOqq";
    for split in 0..=packet.len() {
        let mut terminal = Terminal::new(3, 12).unwrap();
        terminal.feed(&packet[..split]).unwrap();
        terminal.feed(&packet[split..]).unwrap();
        assert!(
            terminal.visible_text().starts_with("─q─q─q"),
            "split {split}"
        );
        assert_eq!(terminal.frame().unwrap().cursor().col, 6);
    }
}

#[test]
fn cursor_report_restores_pending_shift_without_consuming_it_on_unicode_or_controls() {
    let mut terminal = Terminal::new(3, 12).unwrap();
    terminal
        .feed("\x1b*0\x1b+B\x1bo\x1bN\x1b[31mλ".as_bytes())
        .unwrap();
    let before = terminal.frame().unwrap();
    let report = terminal.feed(b"\x1b[1$w").unwrap();
    assert_eq!(report, b"\x1bP1$u1;2;1;@;@;B;3;2;@;BB0B\x1b\\");
    assert_eq!(terminal.frame().unwrap(), before);

    terminal.feed(b"\x1b*B\x1b+0\x1bo\x1bO").unwrap();
    let mut restore = report.clone();
    restore[4] = b't';
    terminal.feed(&restore).unwrap();
    assert_eq!(terminal.feed(b"\x1b[1$w").unwrap(), report);
    terminal.feed(b"qq").unwrap();
    assert!(terminal.visible_text().starts_with("λ─q"));
    assert_eq!(
        terminal.feed(b"\x1b[1$w").unwrap(),
        b"\x1bP1$u1;4;1;@;@;@;3;2;@;BB0B\x1b\\"
    );
}

#[test]
fn saved_single_shift_is_consumed_by_one_graphic_space_after_cursor_restore() {
    let mut terminal = Terminal::new(3, 12).unwrap();
    terminal.feed(b"\x1b+0\x1bO\x1b7q\x1b8 q").unwrap();
    assert!(terminal.visible_text().starts_with(" q"));
    assert_eq!(
        terminal.feed(b"\x1b[1$w").unwrap(),
        b"\x1bP1$u1;3;1;@;@;@;0;2;@;BBB0\x1b\\"
    );
}

#[test]
fn dec_special_control_pictures_survive_all_feed_partitions() {
    for packet in [
        b"\x1b(0bcdehi\x1b(Bbci".as_slice(),
        b"\x1b)0\x0ebcdehi\x0fbci",
        b"\x1b*0\x1bNb\x1bNc\x1bNd\x1bNe\x1bNh\x1bNibci",
        b"\x1b+0\x1bOb\x1bOc\x1bOd\x1bOe\x1bOh\x1bOibci",
    ] {
        for split in 0..=packet.len() {
            let mut terminal = Terminal::new(3, 12).unwrap();
            terminal.feed(&packet[..split]).unwrap();
            terminal.feed(&packet[split..]).unwrap();
            assert!(
                terminal.visible_text().starts_with("␉␌␍␊␤␋bci"),
                "split {split}"
            );
            let frame = terminal.frame().unwrap();
            assert_eq!((frame.cursor().row, frame.cursor().col), (0, 9));
            assert_eq!(terminal.unsupported_count(), 0);
        }
    }
}

#[test]
fn dec_special_control_pictures_are_single_column_graphics() {
    use unicode_width::UnicodeWidthChar;
    for (input, expected) in "bcdehi".chars().zip("␉␌␍␊␤␋".chars()) {
        let translated = jterm_engine::screen::Charset::DecSpecialGraphics.translate(input);
        assert_eq!(translated, expected);
        assert_eq!(translated.width(), Some(1));
        let mut terminal = Terminal::new(2, 12).unwrap();
        terminal
            .feed(format!("\x1b(0{input}\x1b[2b").as_bytes())
            .unwrap();
        assert!(terminal
            .visible_text()
            .starts_with(&expected.to_string().repeat(3)));
        let frame = terminal.frame().unwrap();
        assert_eq!((frame.cursor().row, frame.cursor().col), (0, 3));
    }
}
