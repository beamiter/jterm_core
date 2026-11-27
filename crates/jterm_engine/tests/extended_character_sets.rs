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
