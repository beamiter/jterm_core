use jterm_engine::{terminal::Terminal, Color};

#[test]
fn xtreportsgr_returns_the_intersection_at_every_input_split() {
    let packet = b"\x1b[1;1;1;2#|";
    for split in 0..=packet.len() {
        let mut terminal = Terminal::new(3, 8).unwrap();
        terminal.enable_hyperlinks(|_, _| true);
        terminal.enable_grapheme_clusters();
        terminal.feed("\x1b[1;31;44m\x1b]8;;https://example.invalid\x07q\u{307}\x1b[3;38:5:1mY\x1b]8;;\x07\x1b[3;8HX\x1b[0;32m".as_bytes()).unwrap();
        let before = terminal.frame().unwrap();
        assert!(before.cursor().pending_wrap);
        assert_eq!(before.cluster_text(0, 0), Some("q\u{307}"));
        let mut reply = terminal.feed(&packet[..split]).unwrap();
        reply.extend(terminal.feed(&packet[split..]).unwrap());
        assert_eq!(reply, b"\x1b[0;1;31;44m");
        assert_eq!(terminal.frame().unwrap(), before);
        assert_eq!(terminal.feed(b"\x1b[1;1;1;3#|").unwrap(), b"\x1b[0m");
        terminal.feed(b"\x1b[2;1HP").unwrap();
        assert_eq!(
            terminal.frame().unwrap().rows()[1].cells[0].foreground,
            Color::Green
        );
    }
}

#[test]
fn xtreportsgr_reports_common_extended_rendition_in_relative_coordinates() {
    let mut terminal = Terminal::new(5, 8).unwrap();
    terminal.enable_underline_colors();
    terminal
        .feed(b"\x1b[2;1H\x1b[4:3;38:2::1:2:3;48:5:200;58:5:42mAB\x1b[59mC\x1b[2;4r\x1b[?6h")
        .unwrap();
    let before = terminal.frame().unwrap();
    assert_eq!(
        terminal.feed(b"\x1b[1;1;1;2#|").unwrap(),
        b"\x1b[0;4:3;38:2::1:2:3;48:5:200;58:5:42m"
    );
    assert_eq!(
        terminal.feed(b"\x1b[1;1;1;3#|").unwrap(),
        b"\x1b[0;4:3;38:2::1:2:3;48:5:200m"
    );
    assert_eq!(terminal.feed(b"\x1b[2;1;1;3#|").unwrap(), b"\x1b[0m");
    for invalid in [
        b"\x1b[?1;1;1;2#|".as_slice(),
        b"\x1b[1:1;1;2#|",
        b"\x1b[1;1;1;2;1#|",
        b"\x1b[1;1;1;2 #|",
    ] {
        assert!(terminal.feed(invalid).unwrap().is_empty());
    }
    assert_eq!(terminal.frame().unwrap(), before);
}
