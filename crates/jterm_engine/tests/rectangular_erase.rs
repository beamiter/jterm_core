use jterm_engine::{frame::CellFrame, terminal::Terminal, Color, StyleFlags};

fn row_text(frame: &CellFrame, row: usize) -> String {
    frame.rows()[row]
        .cells
        .iter()
        .map(|cell| cell.character)
        .collect()
}

#[test]
fn decera_is_partition_independent_and_preserves_cursor_and_line_wrapping() {
    let rectangle = b"\x1b[1;2;2;4$z";
    for split in 0..=rectangle.len() {
        let mut terminal = Terminal::new(4, 8).unwrap();
        terminal
            .feed(b"\x1b[31;44;1mABCDEFGHi\x1b[2;1Habcdefgh\x1b[4;8HX")
            .unwrap();
        let before = terminal.frame().unwrap();
        assert!(before.rows()[0].wrapped && before.cursor().pending_wrap);
        terminal.feed(&rectangle[..split]).unwrap();
        terminal.feed(&rectangle[split..]).unwrap();
        let after = terminal.frame().unwrap();
        assert_eq!(row_text(&after, 0), "A   EFGH");
        assert_eq!(row_text(&after, 1), "a   efgh");
        assert_eq!(after.cursor(), before.cursor());
        assert!(after.rows()[0].wrapped);
        assert_eq!(after.rows()[0].cells[1].foreground, Color::Default);
        assert_eq!(after.rows()[0].cells[1].background, Color::Default);
        assert_eq!(after.rows()[0].cells[1].flags, StyleFlags::default());
        assert_eq!(before.delta_to(&after).unwrap().rows.len(), 2);
    }
}

#[test]
fn decsera_preserves_rendition_and_protected_cells_and_repairs_wide_edges() {
    let mut terminal = Terminal::new(3, 8).unwrap();
    terminal.enable_underline_colors();
    terminal.enable_hyperlinks(|_, _| true);
    terminal.feed(b"\x1b[31;44;1;4;58:2::1:2:3m\x1b]8;;https://example.invalid\x07A\x1b[1\"qB\x1b[0\"qC\x1b]8;;\x07").unwrap();
    terminal.feed("\x1b[2;1HA界B".as_bytes()).unwrap();
    let before = terminal.frame().unwrap();
    terminal.feed(b"\x1b[1;1;1;3${\x1b[2;3;2;3${").unwrap();
    let after = terminal.frame().unwrap();
    assert_eq!(row_text(&after, 0), " B      ");
    assert_eq!(row_text(&after, 1), "A  B    ");
    assert_eq!(before.cursor(), after.cursor());
    for col in [0, 2] {
        let cell = after.rows()[0].cells[col];
        assert_eq!(cell.foreground, before.rows()[0].cells[col].foreground);
        assert_eq!(cell.background, before.rows()[0].cells[col].background);
        assert_eq!(cell.flags, before.rows()[0].cells[col].flags);
        assert!(cell.hyperlink_id.is_none());
        assert_eq!(after.underline_color(0, col), Some(Color::Rgb(1, 2, 3)));
    }
    assert_eq!(before.rows()[0].cells[1], after.rows()[0].cells[1]);
    assert!(after
        .hyperlink_target(after.rows()[0].cells[1].hyperlink_id)
        .is_some());
    assert!(!after.rows()[1].cells[1].flags.wide());
    assert!(!after.rows()[1].cells[2].flags.wide_continuation());
}

#[test]
fn rectangular_coordinates_follow_origin_and_reject_malformed_or_reversed_ranges() {
    let mut terminal = Terminal::new(5, 6).unwrap();
    terminal
        .feed(
            b"111111\x1b[2;1H222222\x1b[3;1H333333\x1b[4;1H444444\x1b[5;1H555555\x1b[2;4r\x1b[?6h",
        )
        .unwrap();
    let before = terminal.frame().unwrap();
    for invalid in [
        b"\x1b[?1;2;2;3$z".as_slice(),
        b"\x1b[1:2;2;3$z",
        b"\x1b[1;2;2;3;4$z",
        b"\x1b[2;2;1;3$z",
        b"\x1b[1;2;2;3z",
    ] {
        terminal.feed(invalid).unwrap();
        assert_eq!(terminal.frame().unwrap(), before);
    }
    terminal.feed(b"\x1b[1;2;99;3$z").unwrap();
    let after = terminal.frame().unwrap();
    assert_eq!(row_text(&after, 0), "111111");
    assert_eq!(row_text(&after, 1), "2  222");
    assert_eq!(row_text(&after, 2), "3  333");
    assert_eq!(row_text(&after, 3), "4  444");
    assert_eq!(row_text(&after, 4), "555555");
    assert_eq!(after.cursor(), before.cursor());
}
