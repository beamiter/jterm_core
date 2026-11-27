use jterm_engine::{frame::CellFrame, terminal::Terminal, Color, TerminalCell};

fn raw_row(frame: &CellFrame, row: usize) -> String {
    frame.rows()[row]
        .cells
        .iter()
        .map(|cell| cell.character)
        .collect()
}

#[test]
fn deccra_copies_overlaps_from_pre_command_cells_at_every_input_split() {
    let packet = b"\x1b[1;1;2;5;1;2;3;1$v";
    for split in 0..=packet.len() {
        let mut terminal = Terminal::new(4, 8).unwrap();
        terminal
            .feed(b"ABCDEFGH\x1b[2;1H12345678\x1b[3;1Habcdefgh\x1b[4;1H87654321")
            .unwrap();
        let before = terminal.frame().unwrap();
        assert!(before.cursor().pending_wrap);
        terminal.feed(&packet[..split]).unwrap();
        terminal.feed(&packet[split..]).unwrap();
        let copied = terminal.frame().unwrap();
        assert_eq!(raw_row(&copied, 0), "ABCDEFGH");
        assert_eq!(raw_row(&copied, 1), "12ABCDE8");
        assert_eq!(raw_row(&copied, 2), "ab12345h");
        assert_eq!(raw_row(&copied, 3), "87654321");
        assert_eq!(copied.cursor(), before.cursor());
        assert_eq!(before.delta_to(&copied).unwrap().rows.len(), 2);
        terminal.feed(b"\x1b[2;3;2;7;1;2;1;1$v").unwrap();
        assert_eq!(raw_row(&terminal.frame().unwrap(), 1), "ABCDEDE8");
    }
}

#[test]
fn deccra_preserves_text_metadata_and_repairs_source_and_target_wide_edges() {
    let mut terminal = Terminal::new(3, 8).unwrap();
    terminal.enable_grapheme_clusters();
    terminal.enable_underline_colors();
    terminal.enable_hyperlinks(|_, _| true);
    terminal.feed("\x1b[1;31;44;4;58:2::1:2:3m\x1b[1\"q\x1b]8;;https://example.invalid\x07q\u{307}界\x1b]8;;\x07\x1b[0\"q\x1b[0m\x1b[2;1Hab界XY\x1b[3;5H".as_bytes()).unwrap();
    let before = terminal.frame().unwrap();
    assert_eq!(before.cluster_text(0, 0), Some("q\u{307}"));
    let link = before.rows()[0].cells[0].hyperlink_id;
    terminal
        .feed(b"\x1b[1;1;1;3;1;2;4;1$v\x1b[1;3;1;3;1;3;2;1$v\x1b[1;1;1;3;1;3;7;1$v")
        .unwrap();
    let after = terminal.frame().unwrap();
    assert_eq!(after.cursor(), before.cursor());
    assert_eq!(after.rows()[1].cells[2], TerminalCell::default());
    assert_eq!(after.cluster_text(1, 3), Some("q\u{307}"));
    assert_eq!(after.rows()[1].cells[3], before.rows()[0].cells[0]);
    assert_eq!(
        after.hyperlink_target(link),
        Some("https://example.invalid")
    );
    assert!(after.rows()[1].cells[4].flags.wide());
    assert!(after.rows()[1].cells[5].flags.wide_continuation());
    assert_eq!(after.underline_color(1, 3), Some(Color::Rgb(1, 2, 3)));
    assert_eq!(after.cluster_text(2, 6), Some("q\u{307}"));
    for (source_col, col) in [(2, 1), (1, 7)] {
        let source = before.rows()[0].cells[source_col];
        let cell = after.rows()[2].cells[col];
        assert_eq!(cell.character, ' ');
        assert!(!cell.flags.wide() && !cell.flags.wide_continuation());
        assert_eq!(
            (cell.foreground, cell.background),
            (source.foreground, source.background)
        );
        assert_eq!(cell.flags.bold(), source.flags.bold());
        assert_eq!(cell.flags.protected(), source.flags.protected());
        assert!(cell.hyperlink_id.is_none());
        assert_eq!(
            after.underline_color(2, col),
            before.underline_color(0, source_col)
        );
    }
}

#[test]
fn deccra_clamps_pages_and_relative_coordinates_and_rejects_malformed_parameters() {
    let mut terminal = Terminal::new(5, 6).unwrap();
    terminal
        .feed(
            b"111111\x1b[2;1H222222\x1b[3;1H333333\x1b[4;1H444444\x1b[5;1H555555\x1b[2;4r\x1b[?6h",
        )
        .unwrap();
    let before = terminal.frame().unwrap();
    for invalid in [
        b"\x1b[?1;1;1;2;1;3;5;1$v".as_slice(),
        b"\x1b[1:1;1;2;1;3;5;1$v",
        b"\x1b[1;1;1;2;1;3;5;1;1$v",
        b"\x1b[1;1;1;2;1;3;5;1 $v",
        b"\x1b[2;1;1;2;1;3;5;1$v",
    ] {
        terminal.feed(invalid).unwrap();
        assert_eq!(terminal.frame().unwrap(), before);
    }
    terminal.feed(b"\x1b[1;1;1;2;99;3;5;99$v").unwrap();
    let after = terminal.frame().unwrap();
    assert_eq!(raw_row(&after, 0), "111111");
    assert_eq!(raw_row(&after, 3), "444422");
    assert_eq!(raw_row(&after, 4), "555555");
    assert_eq!(after.cursor(), before.cursor());
}

#[test]
fn deccra_preflights_the_combined_text_budget_before_any_row_changes() {
    let mut terminal = Terminal::new(4, 128).unwrap();
    terminal.enable_grapheme_clusters();
    let cluster = format!("q{}", "\u{307}".repeat(25));
    let rich_row = cluster.repeat(60);
    let content = format!("{}\x1b[2;1H{rich_row}\x1b[4;65H{rich_row}", "a".repeat(60));
    terminal.feed(content.as_bytes()).unwrap();
    let before = terminal.frame().unwrap();
    assert_eq!(before.cluster_text(1, 59), Some(cluster.as_str()));
    assert_eq!(before.cluster_text(3, 123), Some(cluster.as_str()));
    terminal.feed(b"\x1b[1;1;2;60;1;3;1;1$v").unwrap();
    assert_eq!(terminal.frame().unwrap(), before);
}
