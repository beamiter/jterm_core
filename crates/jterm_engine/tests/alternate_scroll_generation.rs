use jterm_engine::{mouse::MouseInput, terminal::Terminal};
fn wheel(terminal: &Terminal, up: bool) -> Option<Vec<u8>> {
    terminal.encode_mouse(
        if up {
            MouseInput::WheelUp
        } else {
            MouseInput::WheelDown
        },
        0,
        0,
        0,
    )
}
#[test]
fn eligibility_and_current_arrow_encoding_follow_screen_modes_and_tracking() {
    let mut terminal = Terminal::new(3, 8).unwrap();
    assert!(!terminal.alternate_scroll_active());
    terminal.feed(b"\x1b[?1049;1007h").unwrap();
    assert!(terminal.alternate_scroll_active());
    assert_eq!(wheel(&terminal, true), Some(b"\x1b[A".to_vec()));
    assert_eq!(wheel(&terminal, false), Some(b"\x1b[B".to_vec()));
    terminal.feed(b"\x1b[?1h").unwrap();
    assert_eq!(wheel(&terminal, true), Some(b"\x1bOA".to_vec()));
    terminal.feed(b"\x1b[?1000;1006h").unwrap();
    assert!(!terminal.alternate_scroll_active());
    assert_eq!(wheel(&terminal, true), Some(b"\x1b[<64;1;1M".to_vec()));
    terminal.feed(b"\x1b[?1000l").unwrap();
    assert!(terminal.alternate_scroll_active());
    terminal.feed(b"\x1b[?1007l").unwrap();
    assert!(!terminal.alternate_scroll_active());
    assert_eq!(wheel(&terminal, true), None);
    terminal.feed(b"\x1b[?1007h\x1b[?1049l").unwrap();
    assert!(!terminal.alternate_scroll_active());
}
#[test]
fn eligible_mode_round_trips_retire_each_step_without_paint_or_unrelated_churn() {
    let packet = b"\x1b[?1007l\x1b[?1007h\x1b[?1h\x1b[?1l";
    for split in 0..=packet.len() {
        let mut terminal = Terminal::new(3, 8).unwrap();
        terminal.feed(b"\x1b[?1049;1007h").unwrap();
        let before = terminal.frame().unwrap();
        let generation = terminal.mouse_report_generation();
        terminal.feed(&packet[..split]).unwrap();
        terminal.feed(&packet[split..]).unwrap();
        assert_eq!(terminal.mouse_report_generation(), generation + 4);
        assert_eq!(terminal.frame().unwrap(), before);
        let generation = terminal.mouse_report_generation();
        terminal
            .feed(b"\x1b[?1007h\x1b[?1l\x1b[?2004h\x1b[?1005h\x1b[?1006h\x1b[?1007$p\x1b[?u")
            .unwrap();
        assert_eq!(terminal.mouse_report_generation(), generation);
    }
    let mut terminal = Terminal::new(3, 8).unwrap();
    let generation = terminal.mouse_report_generation();
    terminal
        .feed(b"\x1b[?1h\x1b[?1007l\x1b[?1007h\x1b[>1u\x1b[<u")
        .unwrap();
    assert_eq!(terminal.mouse_report_generation(), generation);
}
#[test]
fn keyboard_profile_and_soft_reset_retire_only_eligible_alternate_scroll() {
    let mut terminal = Terminal::new(3, 8).unwrap();
    terminal.enable_keyboard_disambiguation();
    terminal.feed(b"\x1b[?1049;1007h").unwrap();
    let generation = terminal.mouse_report_generation();
    terminal.feed(b"\x1b[>1u\x1b[>1u\x1b[<u\x1b[<u").unwrap();
    assert_eq!(terminal.mouse_report_generation(), generation + 2);
    assert_eq!(wheel(&terminal, true), Some(b"\x1b[A".to_vec()));
    let generation = terminal.mouse_report_generation();
    terminal.feed(b"\x1b[>1 u\x1b[< u").unwrap(); // Rejected intermediates do not change negotiation.
    assert_eq!(terminal.mouse_report_generation(), generation);
    terminal.feed(b"\x1b[?1000h").unwrap();
    let generation = terminal.mouse_report_generation();
    terminal
        .feed(b"\x1b[?1h\x1b[?1007l\x1b[>1u\x1b[<u")
        .unwrap();
    assert_eq!(terminal.mouse_report_generation(), generation);
    terminal.feed(b"\x1b[?1000l").unwrap();
    assert!(!terminal.alternate_scroll_active());
    let generation = terminal.mouse_report_generation();
    terminal.feed(b"\x1b[!p").unwrap();
    assert!(terminal.alternate_scroll_active());
    assert_eq!(terminal.mouse_report_generation(), generation + 1);
}
#[test]
fn tracking_transitions_count_once_and_keyboard_admission_changes_are_visible() {
    let mut terminal = Terminal::new(3, 8).unwrap();
    terminal.feed(b"\x1b[?1049;1007h").unwrap();
    let generation = terminal.mouse_report_generation();
    terminal.feed(b"\x1b[?1000h\x1b[?1000l").unwrap();
    assert_eq!(terminal.mouse_report_generation(), generation + 2);
    let generation = terminal.mouse_report_generation();
    terminal.feed(b"\x1b[>1u").unwrap();
    assert_eq!(terminal.mouse_report_generation(), generation + 1);
    assert!(!terminal.alternate_scroll_active());
    assert_eq!(wheel(&terminal, true), None); // Unsupported non-opted-in keyboard protocol.
    let generation = terminal.mouse_report_generation();
    terminal.enable_keyboard_disambiguation();
    assert_eq!(terminal.mouse_report_generation(), generation + 1);
    assert!(terminal.alternate_scroll_active());
    assert_eq!(wheel(&terminal, true), Some(b"\x1b[A".to_vec()));
    let generation = terminal.mouse_report_generation();
    terminal.enable_keyboard_disambiguation();
    assert_eq!(terminal.mouse_report_generation(), generation);
}

#[test]
fn unsupported_keyboard_profiles_do_not_advertise_or_churn_a_wheel_route() {
    let mut terminal = Terminal::new(3, 8).unwrap();
    terminal.feed(b"\x1b[?1049;1007h\x1b[>8u").unwrap();
    assert!(!terminal.alternate_scroll_active());
    assert_eq!(wheel(&terminal, true), None);
    let generation = terminal.mouse_report_generation();
    terminal
        .feed(b"\x1b[?1h\x1b[?1007l\x1b[?1007h\x1b[>16u\x1b[<u")
        .unwrap();
    assert!(!terminal.alternate_scroll_active());
    assert_eq!(terminal.mouse_report_generation(), generation);
    terminal.feed(b"\x1b[<u").unwrap();
    assert!(terminal.alternate_scroll_active());
    assert_eq!(terminal.mouse_report_generation(), generation + 1);
    assert_eq!(wheel(&terminal, true), Some(b"\x1bOA".to_vec()));
}
