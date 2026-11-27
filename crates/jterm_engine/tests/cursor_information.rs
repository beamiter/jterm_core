use jterm_engine::{terminal::Terminal, Color, UnderlineStyle};

#[test]
fn deccir_round_trip_restores_position_wrap_rendition_and_gl_at_every_input_split() {
    let expected = b"\x1bP1$u2;6;1;O;A;I;1;2;@;B0BB\x1b\\";
    for split in 0..=expected.len() {
        let mut terminal = Terminal::new(4, 6).unwrap();
        terminal
            .feed(b"\x1b[2;3r\x1b[?6h\x1b)0\x0e\x1b[1;4;5;7m\x1b[1\"q\x1b[1;6H\x1bHX")
            .unwrap();
        let before = terminal.frame().unwrap();
        let tabs = terminal.feed(b"\x1b[2$w").unwrap();
        let report = terminal.feed(b"\x1b[1$w").unwrap();
        assert_eq!(report, expected);
        assert_eq!(terminal.frame().unwrap(), before);

        terminal
            .feed(b"\x1b[?6l\x0f\x1b)B\x1b[0;32m\x1b[0\"q")
            .unwrap();
        let mut restore = report.clone();
        restore[4] = b't';
        terminal.feed(&restore[..split]).unwrap();
        terminal.feed(&restore[split..]).unwrap();
        assert_eq!(terminal.frame().unwrap().cursor(), before.cursor());
        assert_eq!(terminal.feed(b"\x1b[1$w").unwrap(), report);
        assert_eq!(terminal.feed(b"\x1b[2$w").unwrap(), tabs);

        terminal.feed(b"q").unwrap();
        let frame = terminal.frame().unwrap();
        let cell = frame.rows()[2].cells[0];
        assert_eq!(cell.character, '─');
        assert_eq!(cell.foreground, Color::Green);
        assert!(cell.flags.bold() && cell.flags.blink() && cell.flags.inverse());
        assert!(cell.flags.protected());
        assert_eq!(cell.flags.underline(), UnderlineStyle::Single);
    }
}

#[test]
fn deccir_preserves_the_invoked_bank_when_designations_are_equal() {
    let mut terminal = Terminal::new(4, 8).unwrap();
    terminal.feed(b"\x0e\x1b7\x0f\x1b8").unwrap();
    assert_eq!(
        terminal.feed(b"\x1b[1$w").unwrap(),
        b"\x1bP1$u1;1;1;@;@;@;1;2;@;BBBB\x1b\\"
    );
    terminal.feed(b"\x1b[?1049h\x0f\x1b[?1049l\x1b)0q").unwrap();
    assert!(terminal.visible_text().starts_with('─'));
}

#[test]
fn invalid_or_cancelled_decrsps_keeps_cursor_rendition_and_tabs_unchanged() {
    let mut terminal = Terminal::new(4, 8).unwrap();
    terminal.feed(b"\x1b[2;3H\x1b[31;1m\x1bH").unwrap();
    let before = terminal.frame().unwrap();
    let report = terminal.feed(b"\x1b[1$w\x1b[2$w").unwrap();
    for packet in [
        b"\x1bP1$t1;1;1;@;@;@;0;2;@;B0B?\x1b\\".as_slice(),
        b"\x1bP1$t1;1;1;@;@;@;0;2;@;BBBB\x18",
        b"\x1bP1$t99;1;1;@;@;@;0;2;@;BBBB\x1b\\",
        b"\x1bP1$t1;1;1;@;@;F;0;2;@;BBBB\x1b\\",
    ] {
        assert!(terminal.feed(packet).unwrap().is_empty());
        assert_eq!(terminal.frame().unwrap(), before);
        assert_eq!(terminal.feed(b"\x1b[1$w\x1b[2$w").unwrap(), report);
    }
}

#[test]
fn deccir_round_trips_latin1_designations_without_confusing_94_character_selectors() {
    let mut terminal = Terminal::new(4, 8).unwrap();
    terminal
        .feed(b"\x1b-A\x1b.A\x1b/A\x1bo\x1b|\x1bN\x1b[2;3H")
        .unwrap();
    // Positioning consumes no pending single-shift character.
    let report = terminal.feed(b"\x1b[1$w").unwrap();
    assert_eq!(report, b"\x1bP1$u2;3;1;@;@;B;3;3;N;BAAA\x1b\\");
    terminal.feed(b"\x1b)A").unwrap();
    assert_eq!(
        terminal.feed(b"\x1b[1$w").unwrap(),
        b"\x1bP1$u2;3;1;@;@;B;3;3;L;BBAA\x1b\\"
    );
    terminal
        .feed(b"\x1b*0\x1b+B\x1b -A\x1b.B\x1b/B\x1b(0")
        .unwrap();
    assert_eq!(
        terminal.feed(b"\x1b[1$w").unwrap(),
        b"\x1bP1$u2;3;1;@;@;B;0;3;@;0B0B\x1b\\"
    );

    let mut restore = report.clone();
    restore[4] = b't';
    terminal.feed(&restore).unwrap();
    assert_eq!(terminal.feed(b"\x1b[1$w").unwrap(), report);
}
