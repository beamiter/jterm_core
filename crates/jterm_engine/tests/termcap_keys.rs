use jterm_engine::terminal::Terminal;
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02X}")).collect()
}
#[test]
fn hosted_key_capabilities_match_actual_unmodified_input_and_preserve_modes() {
    let keys = [
        ("ku", "kcuu1", "ArrowUp"),
        ("kd", "kcud1", "ArrowDown"),
        ("kr", "kcuf1", "ArrowRight"),
        ("kl", "kcub1", "ArrowLeft"),
        ("kh", "khome", "Home"),
        ("@7", "kend", "End"),
        ("kI", "kich1", "Insert"),
        ("kD", "kdch1", "Delete"),
        ("kP", "kpp", "PageUp"),
        ("kN", "knp", "PageDown"),
        ("k1", "kf1", "F1"),
        ("k2", "kf2", "F2"),
        ("k3", "kf3", "F3"),
        ("k4", "kf4", "F4"),
        ("k5", "kf5", "F5"),
        ("k6", "kf6", "F6"),
        ("k7", "kf7", "F7"),
        ("k8", "kf8", "F8"),
        ("k9", "kf9", "F9"),
        ("k;", "kf10", "F10"),
        ("F1", "kf11", "F11"),
        ("F2", "kf12", "F12"),
    ];
    for application in [false, true] {
        let mut terminal = Terminal::new(2, 8).unwrap();
        if application {
            terminal.feed(b"\x1b[?1h").unwrap();
        }
        terminal.feed(b"\x1b[>4;2m\x1b[>4;1f\x1b[?66h").unwrap();
        let before = terminal.frame().unwrap();
        for (short, long, key) in keys {
            let actual = terminal.encode_key(key, false, false, false).unwrap();
            for name in [short, long] {
                let encoded_name = hex(name.as_bytes());
                let query = format!("\x1bP+q{encoded_name}\x1b\\");
                let expected = format!("\x1bP1+r{encoded_name}={}\x1b\\", hex(&actual));
                for split in 0..=query.len() {
                    let mut reply = terminal.feed(&query.as_bytes()[..split]).unwrap();
                    reply.extend(terminal.feed(&query.as_bytes()[split..]).unwrap());
                    assert_eq!(
                        reply,
                        expected.as_bytes(),
                        "{name} app={application} split={split}"
                    );
                }
            }
        }
        assert_eq!(terminal.frame().unwrap(), before);
    }
}
#[test]
fn unknown_key_names_and_modified_aliases_keep_fixed_negative_policy() {
    let mut terminal = Terminal::new(2, 8).unwrap();
    for name in [
        "TN", "name", "kbs", "kb", "kent", "kcbt", "kUP", "kLFT", "kf13",
    ] {
        let query = format!("\x1bP+q{}\x1b\\", hex(name.as_bytes()));
        assert_eq!(
            terminal.feed(query.as_bytes()).unwrap(),
            b"\x1bP0+r\x1b\\",
            "{name}"
        );
    }
}
