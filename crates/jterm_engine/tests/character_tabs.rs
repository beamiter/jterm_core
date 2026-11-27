use jterm_engine::terminal::Terminal;

#[test]
fn character_tab_controls_match_hts_and_tbc_at_every_partition() {
    let ctc = b"\x1b[5W\x1b[1;5H\x1b[W\x1b[H\t\x1b[6n\x1b[2W\x1b[H\t\x1b[6n\x1b[1;7H\x1b[0W\x1b[H\t\x1b[6n";
    let legacy =
        b"\x1b[3g\x1b[1;5H\x1bH\x1b[H\t\x1b[6n\x1b[g\x1b[H\t\x1b[6n\x1b[1;7H\x1bH\x1b[H\t\x1b[6n";
    let mut reference = Terminal::new(3, 12).unwrap();
    let expected = reference.feed(legacy).unwrap();
    assert_eq!(expected, b"\x1b[1;5R\x1b[1;12R\x1b[1;7R");
    for split in 0..=ctc.len() {
        let mut terminal = Terminal::new(3, 12).unwrap();
        let mut replies = terminal.feed(&ctc[..split]).unwrap();
        replies.extend(terminal.feed(&ctc[split..]).unwrap());
        assert_eq!(replies, expected, "split {split}");
        assert_eq!(terminal.visible_text(), reference.visible_text());
        assert_eq!(terminal.unsupported_count(), 0);
    }
}

#[test]
fn character_tab_lists_execute_in_order_without_changing_painted_state() {
    let mut terminal = Terminal::new(3, 12).unwrap();
    terminal.feed(b"TEXT\x1b[2;5H").unwrap();
    let before = terminal.frame().unwrap();
    terminal.feed(b"\x1b[5;0W").unwrap();
    assert_eq!(terminal.frame().unwrap(), before);
    assert_eq!(terminal.feed(b"\x1b[2$w").unwrap(), b"\x1bP2$u5\x1b\\");
    terminal.feed(b"\x1b[0;5W").unwrap();
    assert_eq!(terminal.feed(b"\x1b[2$w").unwrap(), b"\x1bP2$u\x1b\\");
    terminal.feed(b"\x1b[0;2;0W").unwrap();
    assert_eq!(terminal.feed(b"\x1b[2$w").unwrap(), b"\x1bP2$u5\x1b\\");
    // Line tabulation and per-line clears have no meaning in the global map.
    terminal.feed(b"\x1b[1;3;4;6;65535W").unwrap();
    assert_eq!(terminal.feed(b"\x1b[2$w").unwrap(), b"\x1bP2$u5\x1b\\");
    terminal.feed(b"\x1b[?5W").unwrap();
    assert_eq!(terminal.feed(b"\x1b[2$w").unwrap(), b"\x1bP2$u9\x1b\\");
}

#[test]
fn character_tab_controls_reject_colons_and_other_namespaces_without_partial_effects() {
    let mut terminal = Terminal::new(3, 12).unwrap();
    terminal.feed(b"\x1b[3g\x1b[1;5H\x1bH").unwrap();
    for invalid in [
        b"\x1b[0:5W".as_slice(),
        b"\x1b[5;0:2W",
        b"\x1b[>5W",
        b"\x1b[5 W",
        b"\x1b[5$W",
    ] {
        terminal.feed(invalid).unwrap();
        assert_eq!(terminal.feed(b"\x1b[2$w").unwrap(), b"\x1bP2$u5\x1b\\");
    }
}
