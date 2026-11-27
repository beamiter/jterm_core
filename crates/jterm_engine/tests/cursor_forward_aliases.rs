use jterm_engine::{
    protocol_state::ProtocolState,
    screen::ScreenState,
    semantics::{dispatch_frost_legacy_csi, Dispatch},
    terminal::Terminal,
};

#[test]
fn hosted_forward_aliases_report_positions_across_all_partitions() {
    let packet = b"\x1b[3;4H\x1b[2a\x1b[6n\x1b[1e\x1b[6n\x1b[0a\x1b[0e\x1b[6n\x1b[a\x1b[e\x1b[6n";
    for split in 0..=packet.len() {
        let mut terminal = Terminal::new(6, 12).unwrap();
        terminal.feed(b"UNCHANGED").unwrap();
        let text = terminal.visible_text();
        let mut replies = terminal.feed(&packet[..split]).unwrap();
        replies.extend(terminal.feed(&packet[split..]).unwrap());
        assert_eq!(
            replies, b"\x1b[3;6R\x1b[4;6R\x1b[5;7R\x1b[6;8R",
            "split {split}"
        );
        assert_eq!(terminal.visible_text(), text);
    }
}

#[test]
fn frost_forward_aliases_clip_to_page_and_margins_and_reject_namespaces() {
    let mut screen = ScreenState::new(6, 12);
    let mut protocol = ProtocolState::default();
    screen.cursor_row = 2;
    screen.cursor_col = 3;
    screen.pending_wrap = true;
    assert!(matches!(
        dispatch_frost_legacy_csi(
            &mut screen,
            &mut protocol,
            b"65535",
            &[65535],
            b'a',
            None,
            b""
        ),
        Dispatch::Handled(_)
    ));
    assert_eq!(
        (screen.cursor_row, screen.cursor_col, screen.pending_wrap),
        (2, 11, false)
    );
    screen.set_scroll_region(2, 5);
    screen.origin_mode = true;
    screen.cursor_row = 2;
    screen.cursor_col = 11;
    assert!(matches!(
        dispatch_frost_legacy_csi(
            &mut screen,
            &mut protocol,
            b"65535",
            &[65535],
            b'e',
            None,
            b""
        ),
        Dispatch::Handled(_)
    ));
    assert_eq!((screen.cursor_row, screen.cursor_col), (4, 11));
    for command in *b"ae" {
        for (prefix, intermediate) in [
            (Some(b'?'), b"".as_slice()),
            (Some(b'>'), b""),
            (None, b"$"),
            (None, b" "),
        ] {
            screen.pending_wrap = true;
            let before = (screen.cursor_row, screen.cursor_col, screen.pending_wrap);
            assert_eq!(
                dispatch_frost_legacy_csi(
                    &mut screen,
                    &mut protocol,
                    b"2",
                    &[2],
                    command,
                    prefix,
                    intermediate
                ),
                Dispatch::Unhandled
            );
            assert_eq!(
                (screen.cursor_row, screen.cursor_col, screen.pending_wrap),
                before
            );
        }
    }
}
