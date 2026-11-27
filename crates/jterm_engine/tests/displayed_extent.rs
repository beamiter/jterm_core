use jterm_engine::{
    protocol_state::ProtocolState,
    screen::ScreenState,
    semantics::{dispatch_frost_legacy_csi, Dispatch, Effect},
    terminal::Terminal,
};

#[test]
fn displayed_extent_tracks_geometry_without_moving_the_cursor() {
    let query = b"\x1b[\"v";
    for split in 0..=query.len() {
        let mut terminal = Terminal::new(5, 12).unwrap();
        terminal.feed(b"\x1b[2;4r\x1b[?6h\x1b[2;12HX").unwrap();
        let before = terminal.frame().unwrap();
        let mut reply = terminal.feed(&query[..split]).unwrap();
        reply.extend(terminal.feed(&query[split..]).unwrap());
        assert_eq!(reply, b"\x1b[5;12;1;1;1\"w");
        assert_eq!(terminal.frame().unwrap(), before);
        terminal.feed(b"\x1b[?1049h").unwrap();
        terminal.resize(7, 16).unwrap();
        let before = terminal.frame().unwrap();
        assert_eq!(terminal.feed(query).unwrap(), b"\x1b[7;16;1;1;1\"w");
        assert_eq!(terminal.frame().unwrap(), before);
        for invalid in [
            b"\x1b[v".as_slice(),
            b"\x1b[?\"v",
            b"\x1b[0\"v",
            b"\x1b[\" v",
        ] {
            assert!(terminal.feed(invalid).unwrap().is_empty());
            assert_eq!(terminal.frame().unwrap(), before);
        }
    }
}

#[test]
fn frost_displayed_extent_uses_the_live_grid_and_exact_namespace() {
    let mut screen = ScreenState::new(8, 24);
    let mut protocol = ProtocolState::default();
    screen.cursor_row = 3;
    screen.cursor_col = 23;
    screen.pending_wrap = true;
    assert_eq!(
        dispatch_frost_legacy_csi(&mut screen, &mut protocol, b"", &[], b'v', None, b"\""),
        Dispatch::Handled(
            [Effect::Reply(b"\x1b[8;24;1;1;1\"w".to_vec())]
                .into_iter()
                .collect()
        )
    );
    assert_eq!((screen.cursor_row, screen.cursor_col), (3, 23));
    assert!(screen.pending_wrap);
    for (raw, params, prefix, intermediate) in [
        (b"".as_slice(), &[][..], Some(b'?'), b"\"".as_slice()),
        (b"0", &[0][..], None, b"\""),
        (b"", &[][..], None, b""),
    ] {
        assert_eq!(
            dispatch_frost_legacy_csi(
                &mut screen,
                &mut protocol,
                raw,
                params,
                b'v',
                prefix,
                intermediate
            ),
            Dispatch::Unhandled
        );
    }
}
