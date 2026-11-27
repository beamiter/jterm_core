use jterm_engine::{frame::CellFrame, terminal::Terminal, Color, TerminalCell};

fn row_text(frame: &CellFrame, row: usize) -> String {
    frame.rows()[row]
        .cells
        .iter()
        .map(|cell| cell.character)
        .collect()
}

#[test]
fn column_edits_shift_every_scrolling_row_and_its_style_without_moving_the_cursor() {
    let insert = b"\x1b[2'}";
    for split in 0..=insert.len() {
        let mut terminal = Terminal::new(4, 6).unwrap();
        terminal.enable_underline_colors();
        terminal.feed(b"ABCDEF\x1b[2;1Hab\x1b[1;31;4;58:2::8:9:10mcdef\x1b[0m\x1b[3;1H123456\x1b[4;1HUVWXYZ\x1b[2;3r\x1b[2;3H\x1b[44m").unwrap();
        let before = terminal.frame().unwrap();
        terminal.feed(&insert[..split]).unwrap();
        terminal.feed(&insert[split..]).unwrap();
        let inserted = terminal.frame().unwrap();
        assert_eq!(inserted.cursor(), before.cursor());
        assert_eq!(row_text(&inserted, 1), "ab  cd");
        assert_eq!(row_text(&inserted, 2), "12  34");
        assert_eq!(inserted.rows()[1].cells[2], TerminalCell::default());
        assert_eq!(inserted.rows()[1].cells[4].foreground, Color::Red);
        assert_eq!(inserted.underline_color(1, 4), Some(Color::Rgb(8, 9, 10)));
        assert_eq!(inserted.rows()[0], before.rows()[0]);
        assert_eq!(inserted.rows()[3], before.rows()[3]);

        terminal.feed(b"\x1b[0'~").unwrap();
        let deleted = terminal.frame().unwrap();
        assert_eq!(deleted.cursor(), before.cursor());
        assert_eq!(row_text(&deleted, 1), "ab cd ");
        assert_eq!(row_text(&deleted, 2), "12 34 ");
        assert_eq!(deleted.rows()[1].cells[5], TerminalCell::default());
        assert_eq!(deleted.underline_color(1, 3), Some(Color::Rgb(8, 9, 10)));
        assert_eq!(terminal.feed(b"\x1b[6n").unwrap(), b"\x1b[2;3R");
    }
}

#[test]
fn column_edits_are_no_ops_outside_the_region_or_with_malformed_parameters() {
    let mut terminal = Terminal::new(4, 6).unwrap();
    terminal
        .feed(b"ABCDEF\x1b[2;1Habcdef\x1b[3;1H123456\x1b[4;1HUVWXYZ\x1b[2;3r\x1b[1;3H")
        .unwrap();
    let outside = terminal.frame().unwrap();
    terminal.feed(b"\x1b['}\x1b[65535'~").unwrap();
    assert_eq!(terminal.frame().unwrap(), outside);
    terminal.feed(b"\x1b[2;3H").unwrap();
    let inside = terminal.frame().unwrap();
    for malformed in [
        b"\x1b[?1'}".as_slice(),
        b"\x1b[1:1'~",
        b"\x1b[1;1'}",
        b"\x1b[1 ~",
    ] {
        terminal.feed(malformed).unwrap();
        assert_eq!(terminal.frame().unwrap(), inside);
    }
}
