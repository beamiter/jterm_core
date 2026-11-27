use jterm_engine::{
    mouse::{MouseInput, MouseTracking},
    terminal::Terminal,
};

fn up(terminal: &Terminal) -> Vec<u8> {
    terminal.encode_key("ArrowUp", false, false, false).unwrap()
}

#[test]
fn hosted_save_restore_preserves_independent_modes_at_every_input_split() {
    let packet = b"\x1b[?1;2004h\x1b[?25l\x1b[?1;25;2004s\x1b[?1;2004l\x1b[?25h\x1b[?1;25r";
    for split in 0..=packet.len() {
        let mut terminal = Terminal::new(4, 12).unwrap();
        terminal.feed(&packet[..split]).unwrap();
        terminal.feed(&packet[split..]).unwrap();
        assert_eq!(up(&terminal), b"\x1bOA");
        assert!(!terminal.frame().unwrap().cursor().visible);
        assert_eq!(terminal.encode_paste("test").unwrap(), b"test");
        terminal.feed(b"\x1b[?2004r").unwrap();
        assert_eq!(
            terminal.encode_paste("test").unwrap(),
            b"\x1b[200~test\x1b[201~"
        );
    }
}

#[test]
fn saved_values_survive_soft_reset_but_not_ris_or_overwriting_a_slot() {
    let mut terminal = Terminal::new(4, 12).unwrap();
    terminal.feed(b"\x1b[?1h\x1b[?1r").unwrap();
    assert_eq!(up(&terminal), b"\x1bOA");
    terminal.feed(b"\x1b[?1s\x1b[!p\x1b[?1r").unwrap();
    assert_eq!(up(&terminal), b"\x1bOA");
    terminal.feed(b"\x1b[?1l\x1b[?1:7s\x1b[?1r").unwrap();
    assert_eq!(up(&terminal), b"\x1bOA");
    terminal.feed(b"\x1b[?1l\x1b[?1s\x1b[?1h\x1b[?1r").unwrap();
    assert_eq!(up(&terminal), b"\x1b[A");
    terminal.feed(b"\x1b[?1h\x1b[?1s\x1bc\x1b[?1r").unwrap();
    assert_eq!(up(&terminal), b"\x1b[A");
}

#[test]
fn saved_mouse_groups_and_alternate_aliases_restore_using_live_transitions() {
    let mut terminal = Terminal::new(4, 12).unwrap();
    terminal
        .feed(b"primary\x1b[?47halt\x1b[?1049s\x1b[?47l")
        .unwrap();
    assert!(terminal.visible_text().contains("primary"));
    terminal.feed(b"\x1b[?1047r").unwrap();
    assert!(terminal.alternate_screen_active());
    assert!(terminal.visible_text().contains("alt"));
    terminal
        .feed(b"\x1b[?47l\x1b[?47s\x1b[?47h\x1b[?1049r")
        .unwrap();
    assert!(!terminal.alternate_screen_active());
    assert!(terminal.visible_text().contains("primary"));

    terminal
        .feed(b"\x1b[?1000;1002;1006h\x1b[?1000;1005s\x1b[?1000;1002;1006l\x1b[?1003;1015h")
        .unwrap();
    let before = terminal.mouse_report_generation();
    let unsupported = terminal.unsupported_count();
    terminal.feed(b"\x1b[?1003;1015r").unwrap();
    assert_eq!(terminal.unsupported_count(), unsupported);
    assert!(terminal.mouse_report_generation() > before);
    assert!(terminal
        .encode_mouse(MouseInput::Motion(None), 2, 1, 0)
        .is_none());
    assert_eq!(
        terminal
            .encode_mouse(MouseInput::Motion(Some(0)), 2, 1, 0)
            .unwrap(),
        b"\x1b[<32;3;2M"
    );
    terminal.feed(b"\x1b[?1002l").unwrap();
    assert_eq!(
        terminal
            .encode_mouse(MouseInput::Release(0), 2, 1, 0)
            .unwrap(),
        b"\x1b[<0;3;2m"
    );
}

#[test]
fn x10_and_vt200_tracking_have_independent_save_slots() {
    let mut terminal = Terminal::new(4, 12).unwrap();
    terminal
        .feed(b"\x1b[?9h\x1b[?9s\x1b[?1002h\x1b[?1000s\x1b[?9;1002l\x1b[?9r")
        .unwrap();
    assert_eq!(
        terminal.mouse_report_profile().unwrap().tracking,
        MouseTracking::PressOnly
    );
    terminal.feed(b"\x1b[?1003r").unwrap();
    assert_eq!(
        terminal.mouse_report_profile().unwrap().tracking,
        MouseTracking::Drag
    );
    terminal.feed(b"\x1b[?1002l").unwrap();
    assert_eq!(
        terminal.mouse_report_profile().unwrap().tracking,
        MouseTracking::PressOnly
    );
}
