use jterm_engine::{terminal::Terminal, Color};

#[test]
fn hosted_stack_survives_fragmentation_screen_switch_soft_and_hard_reset() {
    let packet = b"\x1b[1;31;44m\x1b[#{\x1b[0;32m\x1b[?1049h\x1b[?1049l\x1b[!p\x1bc\x1b[#}X";
    for split in 0..=packet.len() {
        let mut terminal = Terminal::new(2, 8).unwrap();
        terminal.feed(&packet[..split]).unwrap();
        terminal.feed(&packet[split..]).unwrap();
        let frame = terminal.frame().unwrap();
        let cell = frame.rows()[0].cells[0];
        assert_eq!(
            (cell.foreground, cell.background),
            (Color::Red, Color::Blue),
            "split={split}"
        );
        assert!(cell.flags.bold());
    }
}

#[test]
fn alias_stack_survives_same_feed_ris() {
    let packet = b"\x1b[1;31;44m\x1b[#p\x1b[0;32m\x1bc\x1b[#qX";
    for split in 0..=packet.len() {
        let mut terminal = Terminal::new(2, 8).unwrap();
        terminal.feed(&packet[..split]).unwrap();
        terminal.feed(&packet[split..]).unwrap();
        let frame = terminal.frame().unwrap();
        let cell = frame.rows()[0].cells[0];
        assert_eq!(
            (cell.foreground, cell.background),
            (Color::Red, Color::Blue)
        );
        assert!(cell.flags.bold());
    }
}
