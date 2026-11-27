use jterm_engine::{terminal::Terminal, Color, UnderlineStyle};

#[test]
fn stream_attributes_distinguish_drawn_spaces_and_unoccupied_cells_across_splits() {
    let packet = b"\x1b[1;2;3;4;1$r";
    for split in 0..=packet.len() {
        let mut terminal = Terminal::new(4, 8).unwrap();
        terminal
            .feed(b"AB \x1b[1;5HE\x1b[2;1Hab cd\x1b[3;1H123456\x1b[4;8HX")
            .unwrap();
        let before = terminal.frame().unwrap();
        terminal.feed(&packet[..split]).unwrap();
        terminal.feed(&packet[split..]).unwrap();
        let marked = terminal.frame().unwrap();
        assert_eq!(marked.cursor(), before.cursor());
        let bold = |row: usize, col: usize| marked.rows()[row].cells[col].flags.bold();
        assert!(!bold(0, 0));
        assert!(bold(0, 1) && bold(0, 2) && !bold(0, 3) && bold(0, 4));
        assert!((0..5).all(|col| bold(1, col)));
        assert!((0..4).all(|col| bold(2, col)));
        assert!(!bold(2, 4));

        terminal.feed(b"\x1b[2*x\x1b[1;2;3;4;7$t").unwrap();
        let inverted = terminal.frame().unwrap();
        assert!(inverted.rows()[0].cells[3].flags.inverse());
        assert!(!inverted.rows()[0].cells[4].flags.inverse());
        terminal.feed(b"\x1b[0*x\x1b[1;4;1;4;1$r").unwrap();
        let materialized = terminal.frame().unwrap();
        assert!(materialized.rows()[0].cells[3].flags.bold());
        assert_eq!(materialized.cursor(), before.cursor());
    }
}

#[test]
fn rectangle_attributes_preserve_metadata_and_the_current_pen() {
    let mut terminal = Terminal::new(3, 8).unwrap();
    terminal.enable_grapheme_clusters();
    terminal.enable_hyperlinks(|_, _| true);
    terminal.enable_underline_colors();
    terminal.feed("\x1b[1;3;31;44;4:3;58:5:42m\x1b[1\"q\x1b]8;;https://example.invalid\x07q\u{307}界\x1b]8;;\x07\x1b[0\"q\x1b[0;32m\x1b[2*x".as_bytes()).unwrap();
    let before = terminal.frame().unwrap();
    terminal
        .feed(b"\x1b[1;1;1;1;0$r\x1b[1;2;1;2;7;7$t")
        .unwrap();
    let after = terminal.frame().unwrap();
    let cell = after.rows()[0].cells[0];
    assert!(!cell.flags.bold());
    assert!(cell.flags.italic() && cell.flags.protected());
    assert_eq!(cell.flags.underline(), UnderlineStyle::None);
    assert_eq!(after.underline_color(0, 0), Some(Color::Indexed(42)));
    assert_eq!(after.cluster_text(0, 0), Some("q\u{307}"));
    assert_eq!(
        after.hyperlink_target(cell.hyperlink_id),
        Some("https://example.invalid")
    );
    assert_eq!(after.rows()[0].cells[1..3], before.rows()[0].cells[1..3]);
    assert_eq!(after.cursor(), before.cursor());
    terminal.feed(b"\x1b[2;1HP").unwrap();
    assert_eq!(
        terminal.frame().unwrap().rows()[1].cells[0].foreground,
        Color::Green
    );
}

#[test]
fn extent_is_preserved_on_soft_reset_and_cleared_on_ris() {
    let mut terminal = Terminal::new(4, 8).unwrap();
    terminal.feed(b"\x1b[2*x\x1b[!pABCD\x1b[2;1HEFGH").unwrap();
    let before = terminal.frame().unwrap();
    for malformed in [
        b"\x1b[?1*x".as_slice(),
        b"\x1b[1:2*x",
        b"\x1b[1;2*x",
        b"\x1b[9*x",
        b"\x1b[?1;1;1;2;1$r",
        b"\x1b[1;1:2;1;2;1$r",
    ] {
        terminal.feed(malformed).unwrap();
    }
    assert_eq!(terminal.frame().unwrap(), before);
    terminal.feed(b"\x1b[1;2;2;3;1$r").unwrap();
    let rectangle = terminal.frame().unwrap();
    assert!(!rectangle.rows()[0].cells[3].flags.bold());
    assert!(!rectangle.rows()[1].cells[0].flags.bold());
    terminal
        .feed(b"\x1bcABCD\x1b[2;1HEFGH\x1b[1;2;2;3;1$r")
        .unwrap();
    let stream = terminal.frame().unwrap();
    assert!(stream.rows()[0].cells[3].flags.bold());
    assert!(stream.rows()[1].cells[0].flags.bold());
}
