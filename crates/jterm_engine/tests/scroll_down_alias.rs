use jterm_engine::{terminal::Terminal, Color, StyleFlags};

#[test]
fn ecma_scroll_down_alias_obeys_margins_cursor_bce_and_count() {
    for parameter in ["", "0", "1", "2", "65535"] {
        let query = format!("\x1b[{parameter}^");
        for split in 0..=query.len() {
            let mut terminal = Terminal::new(4, 8).unwrap();
            terminal.feed(b"\x1b[1;1HAAAA\x1b[2;1HBBBB\x1b[3;1HCCCC\x1b[4;1HDDDD\x1b[2;3r\x1b[3;2H\x1b[1;31;44m").unwrap();
            let history = terminal.history_rows();
            terminal.feed(&query.as_bytes()[..split]).unwrap();
            terminal.feed(&query.as_bytes()[split..]).unwrap();
            let frame = terminal.frame().unwrap();
            let text: Vec<String> = frame
                .rows()
                .iter()
                .map(|row| row.cells.iter().map(|cell| cell.character).collect())
                .collect();
            let single = matches!(parameter, "" | "0" | "1");
            assert_eq!(
                text,
                [
                    "AAAA    ",
                    "        ",
                    if single { "BBBB    " } else { "        " },
                    "DDDD    "
                ]
            );
            assert_eq!((frame.cursor().row, frame.cursor().col), (2, 1));
            assert_eq!(terminal.history_rows(), history);
            assert_eq!(terminal.unsupported_count(), 0);
            let blank = frame.rows()[1].cells[0];
            assert_eq!(
                (blank.foreground, blank.background),
                (Color::Default, Color::Blue)
            );
            assert_eq!(blank.flags, StyleFlags::default());
        }
    }
}

#[test]
fn scroll_down_alias_rejects_other_control_namespaces() {
    for query in [
        b"\x1b[?1^".as_slice(),
        b"\x1b[>1^",
        b"\x1b[1 ^",
        b"\x1b[1$^",
    ] {
        let mut terminal = Terminal::new(3, 8).unwrap();
        terminal.feed(b"A\r\nB\r\nC").unwrap();
        let before = terminal.frame().unwrap();
        terminal.feed(query).unwrap();
        assert_eq!(terminal.frame().unwrap(), before);
        assert_eq!(terminal.unsupported_count(), 1);
    }
}
