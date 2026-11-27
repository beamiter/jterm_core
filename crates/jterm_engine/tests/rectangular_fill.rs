use jterm_engine::{frame::CellFrame, terminal::Terminal, Color, TerminalCell, UnderlineStyle};

fn row_text(frame: &CellFrame, row: usize) -> String {
    frame.rows()[row]
        .cells
        .iter()
        .map(|cell| cell.character)
        .collect()
}

#[test]
fn decfra_uses_current_rendition_and_charset_without_moving_the_cursor() {
    let packet = b"\x1b[113;1;2;2;5$x";
    for split in 0..=packet.len() {
        let mut terminal = Terminal::new(4, 8).unwrap();
        terminal.enable_underline_colors();
        terminal.enable_hyperlinks(|_, _| true);
        terminal.feed(b"12345678x\x1b[2;1Habcdefgh\x1b[4;8HX\x1b[32;45;1;4;58:2::4:5:6m\x1b[1\"q\x1b(0\x1b]8;;https://example.invalid\x07").unwrap();
        let before = terminal.frame().unwrap();
        assert!(before.rows()[0].wrapped && before.cursor().pending_wrap);
        terminal.feed(&packet[..split]).unwrap();
        terminal.feed(&packet[split..]).unwrap();
        let after = terminal.frame().unwrap();
        assert_eq!(row_text(&after, 0), "1────678");
        assert_eq!(row_text(&after, 1), "a────fgh");
        assert!(after.rows()[0].wrapped);
        assert_eq!(after.cursor(), before.cursor());
        assert_eq!(before.delta_to(&after).unwrap().rows.len(), 2);
        for row in 0..2 {
            for col in 1..5 {
                let cell = after.rows()[row].cells[col];
                assert_eq!(
                    (cell.foreground, cell.background),
                    (Color::Green, Color::Magenta)
                );
                assert!(cell.flags.bold() && cell.flags.protected());
                assert_eq!(cell.flags.underline(), UnderlineStyle::Single);
                assert_eq!(after.underline_color(row, col), Some(Color::Rgb(4, 5, 6)));
                assert!(cell.hyperlink_id.is_none());
            }
        }
        terminal.feed(b"\x1b[1;2;2;5${").unwrap();
        assert_eq!(terminal.frame().unwrap(), after);
    }
}

#[test]
fn decfra_repairs_clipped_wide_halves_and_overwrites_old_protection() {
    let mut terminal = Terminal::new(3, 8).unwrap();
    terminal
        .feed("\x1b[31m\x1b[1\"qA界B\x1b[2;1HA界B\x1b[0\"q\x1b[32m".as_bytes())
        .unwrap();
    terminal
        .feed(b"\x1b[233;1;3;1;3$x\x1b[35;2;2;2;2$x")
        .unwrap();
    let frame = terminal.frame().unwrap();
    assert_eq!(row_text(&frame, 0), "A éB    ");
    assert_eq!(row_text(&frame, 1), "A# B    ");
    assert_eq!(frame.rows()[0].cells[1], TerminalCell::default());
    assert_eq!(frame.rows()[1].cells[2], TerminalCell::default());
    assert!(!frame.rows()[0].cells[2].flags.protected());
    assert_eq!(frame.rows()[0].cells[2].foreground, Color::Green);
}

#[test]
fn decfra_checks_parameter_namespace_and_clamps_relative_origin() {
    let mut terminal = Terminal::new(5, 6).unwrap();
    terminal
        .feed(
            b"111111\x1b[2;1H222222\x1b[3;1H333333\x1b[4;1H444444\x1b[5;1H555555\x1b[2;4r\x1b[?6h",
        )
        .unwrap();
    let before = terminal.frame().unwrap();
    for invalid in [
        b"\x1b[?65;1;1;1;1$x".as_slice(),
        b"\x1b[65:1;1;1;1$x",
        b"\x1b[65;1;1;1;1;1$x",
        b"\x1b[65;1;1;1;1 $x",
        b"\x1b[31;1;1;1;1$x",
        b"\x1b[127;1;1;1;1$x",
        b"\x1b[159;1;1;1;1$x",
        b"\x1b[256;1;1;1;1$x",
    ] {
        terminal.feed(invalid).unwrap();
        assert_eq!(terminal.frame().unwrap(), before);
    }
    terminal.feed(b"\x1b[;1;2;99;3$x").unwrap();
    let after = terminal.frame().unwrap();
    assert_eq!(row_text(&after, 0), "111111");
    assert_eq!(row_text(&after, 1), "2  222");
    assert_eq!(row_text(&after, 2), "3  333");
    assert_eq!(row_text(&after, 3), "4  444");
    assert_eq!(row_text(&after, 4), "555555");
    assert_eq!(after.cursor(), before.cursor());
}

#[test]
fn decfra_uses_each_gr_lock_with_gl_and_utf8_unchanged_at_every_input_split() {
    let packet = concat!(
        "\x1b)0\x1b*B\x1b+0\x1b|qñ",
        "\x1b[241;2;1;2;1$x",
        "\x1b}\x1b[241;2;2;2;2$x",
        "\x1b~\x1b[241;2;3;2;3$x",
        "\x1b[160;2;4;2;4$x\x1b[255;2;5;2;5$x",
        "\x1b[223;2;6;2;6$x",
    )
    .as_bytes();
    for split in 0..=packet.len() {
        let mut terminal = Terminal::new(3, 6).unwrap();
        terminal.feed(&packet[..split]).unwrap();
        terminal.feed(&packet[split..]).unwrap();
        let after = terminal.frame().unwrap();
        assert_eq!(row_text(&after, 0), "qñ    ", "split {split}");
        assert_eq!(row_text(&after, 1), "─ñ─  ß", "split {split}");
        assert_eq!(terminal.feed(b"\x1b[6n").unwrap(), b"\x1b[1;3R");
    }
}

#[test]
fn decfra_reads_the_pending_gl_shift_without_consuming_it_or_using_it_for_gr() {
    let mut terminal = Terminal::new(3, 6).unwrap();
    terminal
        .feed(b"\x1b*0\x1b+B\x1b}\x1bO\x1b[241;2;1;2;1$x")
        .unwrap();
    assert_eq!(
        terminal.feed(b"\x1b[1$w").unwrap(),
        b"\x1bP1$u1;1;1;@;@;D;0;2;@;BB0B\x1b\\"
    );
    terminal.feed(b"\x1bN\x1b[113;2;2;2;2$xqq").unwrap();
    let after = terminal.frame().unwrap();
    assert_eq!(row_text(&after, 0), "─q    ");
    assert_eq!(row_text(&after, 1), "──    ");
}
