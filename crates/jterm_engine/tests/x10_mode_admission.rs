use jterm_engine::{
    mode_mutation::{apply_ember_mode, apply_frost_mode, FrostModeEffect, ModeNamespace},
    mouse::MouseInput,
    parser::Event,
    protocol_state::ProtocolState,
    screen::ScreenState,
    semantics::{dispatch_frost_legacy_csi, dispatch_local, Dispatch, Effect},
    terminal::Terminal,
};

#[test]
fn hosted_x10_reports_only_unmodified_button_presses() {
    for chunk in 1..=8 {
        let mut terminal = Terminal::new(4, 12).unwrap();
        let before = terminal.frame().unwrap();
        let generation = terminal.mouse_report_generation();
        let mut reply = Vec::new();
        for bytes in b"\x1b[?9h\x1b[?9$p".chunks(chunk) {
            reply.extend(terminal.feed(bytes).unwrap());
        }
        assert_eq!(reply, b"\x1b[?9;1$y");
        assert_eq!(terminal.frame().unwrap(), before);
        assert!(terminal.mouse_report_generation() > generation);
        assert_eq!(
            terminal.encode_mouse(MouseInput::Press(0), 2, 1, 28),
            Some(b"\x1b[M #\"".to_vec())
        );
        assert!(terminal
            .encode_mouse(MouseInput::Release(0), 2, 1, 0)
            .is_none());
        assert!(terminal
            .encode_mouse(MouseInput::Motion(Some(0)), 2, 1, 0)
            .is_none());
        assert!(terminal
            .encode_mouse(MouseInput::WheelUp, 2, 1, 0)
            .is_none());
        let active = terminal.mouse_report_generation();
        terminal.feed(b"\x1b[?9h").unwrap();
        assert_eq!(terminal.mouse_report_generation(), active);
        terminal.feed(b"\x1b[?1000h").unwrap();
        assert!(terminal
            .encode_mouse(MouseInput::Release(0), 2, 1, 0)
            .is_some());
        terminal.feed(b"\x1b[?1000l").unwrap();
        assert!(terminal
            .encode_mouse(MouseInput::Release(0), 2, 1, 0)
            .is_none());
        terminal.feed(b"\x1b[?9l").unwrap();
        assert_eq!(terminal.feed(b"\x1b[?9$p").unwrap(), b"\x1b[?9;2$y");
        assert!(terminal.mouse_report_profile().is_none());
    }
}

#[test]
fn native_reducers_admit_x10_and_report_its_stored_mode() {
    for frost in [false, true] {
        let mut screen = ScreenState::new(4, 12);
        let mut protocol = ProtocolState::default();
        if frost {
            assert_eq!(
                apply_frost_mode(
                    &mut screen,
                    &mut protocol,
                    ModeNamespace::DecPrivate,
                    9,
                    true
                ),
                FrostModeEffect::MouseReportingChanged
            );
        } else {
            assert!(apply_ember_mode(
                &mut screen,
                &mut protocol,
                ModeNamespace::DecPrivate,
                9,
                true
            ));
        }
        assert!(protocol.modes().is_mouse_enabled());
        assert_eq!(
            protocol.modes().get_mouse_report(28, 2, 1),
            Some(b"\x1b[M #\"".to_vec())
        );
        let query = Event::Csi {
            raw_params: b"9".to_vec(),
            params: vec![9],
            colon_flags: vec![false],
            private_prefix: Some(b'?'),
            intermediates: b"$".to_vec(),
            final_byte: b'p',
        };
        let actual = if frost {
            dispatch_frost_legacy_csi(
                &mut screen,
                &mut protocol,
                b"9",
                &[9],
                b'p',
                Some(b'?'),
                b"$",
            )
        } else {
            dispatch_local(&mut screen, &mut protocol, &query)
        };
        assert_eq!(
            actual,
            Dispatch::Handled(
                [Effect::Reply(b"\x1b[?9;1$y".to_vec())]
                    .into_iter()
                    .collect()
            )
        );
    }
}
