use jterm_engine::terminal::Terminal;

#[test]
fn geometry_changes_retire_pointer_generation_only_after_success() {
    for reflow in [false, true] {
        let mut terminal = Terminal::new(3, 12).unwrap();
        if reflow {
            terminal.enable_primary_reflow();
        }
        terminal.feed(b"\x1b[?1002;1006h").unwrap();
        let initial = terminal.mouse_report_generation();
        terminal.resize(3, 12).unwrap();
        assert_eq!(terminal.mouse_report_generation(), initial);
        assert!(terminal
            .resize_with(4, 16, || Err("ioctl refused".into()))
            .is_err());
        assert_eq!(terminal.mouse_report_generation(), initial);
        terminal.resize(4, 16).unwrap();
        assert_eq!(terminal.mouse_report_generation(), initial + 1);
        terminal.resize(3, 12).unwrap();
        assert_eq!(terminal.mouse_report_generation(), initial + 2);
        assert!(terminal.mouse_report_profile().is_some());
    }
}

#[test]
fn pointer_generation_records_hidden_profile_transitions_without_paint() {
    let packet = b"\x1b[?1000h\x1b[?1000l\x1b[?1000h";
    for split in 0..=packet.len() {
        let mut terminal = Terminal::new(3, 12).unwrap();
        let before = terminal.frame().unwrap();
        let generation = terminal.mouse_report_generation();
        terminal.feed(&packet[..split]).unwrap();
        terminal.feed(&packet[split..]).unwrap();
        assert_eq!(terminal.mouse_report_generation(), generation + 3);
        assert!(terminal.mouse_report_profile().is_some());
        assert_eq!(terminal.frame().unwrap(), before);
    }
}

#[test]
fn pointer_generation_follows_effective_precedence_and_reset_lifetimes() {
    let mut terminal = Terminal::new(3, 12).unwrap();
    let initial = terminal.mouse_report_generation();
    terminal.feed(b"\x1b[?1006h\x1b[?2004h").unwrap();
    assert_eq!(terminal.mouse_report_generation(), initial);
    terminal.feed(b"\x1b[?1002h").unwrap();
    let drag = terminal.mouse_report_generation();
    assert_eq!(drag, initial + 1);
    terminal
        .feed(b"\x1b[?1002h\x1b[?1000h\x1b[?1005h\x1b[?1015h")
        .unwrap();
    assert_eq!(terminal.mouse_report_generation(), drag);
    terminal.feed(b"\x1b[?1006l").unwrap();
    assert_eq!(terminal.mouse_report_generation(), drag + 1);
    let profile = terminal.mouse_report_profile();
    terminal.feed(b"\x1bc\x1b[?1000;1002;1015h").unwrap();
    assert_eq!(terminal.mouse_report_profile(), profile);
    assert!(terminal.mouse_report_generation() > drag + 1);
    terminal.feed(b"\x1bc").unwrap();
    let reset = terminal.mouse_report_generation();
    terminal.feed(b"\x1bc").unwrap();
    assert_eq!(terminal.mouse_report_generation(), reset + 1);
}

#[test]
fn pointer_generation_retires_gestures_when_the_screen_switches_back_inside_one_feed() {
    let mut terminal = Terminal::new(3, 12).unwrap();
    terminal.feed(b"\x1b[?1002;1006h").unwrap();
    let before = terminal.frame().unwrap();
    let generation = terminal.mouse_report_generation();
    terminal
        .feed(b"\x1b[?1049h\x1b[?1049h\x1b[?1049l\x1b[?1049l")
        .unwrap();
    assert_eq!(
        terminal.frame().unwrap().stamp().screen,
        before.stamp().screen
    );
    assert_eq!(terminal.mouse_report_generation(), generation + 2);
}
