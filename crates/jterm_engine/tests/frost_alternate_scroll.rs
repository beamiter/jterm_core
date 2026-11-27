use jterm_engine::{
    input::{legacy_function_key_sequence, Key, Modifiers},
    mode_mutation::{apply_frost_mode, FrostModeEffect, ModeNamespace},
    native_reset::{frost_initial_modes, reset_frost_attributes},
    protocol_state::ProtocolState,
    screen::ScreenState,
    semantics::{dispatch_frost_legacy_csi, Dispatch, Effect},
};
fn mode_query(screen: &mut ScreenState, protocol: &mut ProtocolState) -> Dispatch {
    dispatch_frost_legacy_csi(screen, protocol, b"1007", &[1007], b'p', Some(b'?'), b"$")
}
#[test]
fn frost_alternate_scroll_is_opt_in_truthful_and_revokes_only_real_transitions() {
    let mut screen = ScreenState::new_frost_legacy(3, 8);
    let mut protocol = ProtocolState::with_modes(frost_initial_modes());
    assert!(!protocol.modes().contains(&1007));
    assert_eq!(
        mode_query(&mut screen, &mut protocol),
        Dispatch::Handled(
            [Effect::Reply(b"\x1b[?1007;2$y".to_vec())]
                .into_iter()
                .collect()
        )
    );
    for (enabled, effect) in [
        (true, FrostModeEffect::MouseReportingChanged),
        (true, FrostModeEffect::None),
        (false, FrostModeEffect::MouseReportingChanged),
        (false, FrostModeEffect::None),
    ] {
        assert_eq!(
            apply_frost_mode(
                &mut screen,
                &mut protocol,
                ModeNamespace::DecPrivate,
                1007,
                enabled
            ),
            effect
        );
        assert_eq!(protocol.modes().contains(&1007), enabled);
    }
    apply_frost_mode(
        &mut screen,
        &mut protocol,
        ModeNamespace::DecPrivate,
        1007,
        true,
    );
    assert_eq!(
        mode_query(&mut screen, &mut protocol),
        Dispatch::Handled(
            [Effect::Reply(b"\x1b[?1007;1$y".to_vec())]
                .into_iter()
                .collect()
        )
    );
    assert_eq!(
        apply_frost_mode(&mut screen, &mut protocol, ModeNamespace::Ansi, 1007, false),
        FrostModeEffect::None
    );
    assert!(protocol.modes().contains(&1007));
    assert!(matches!(
        dispatch_frost_legacy_csi(&mut screen, &mut protocol, b"", &[], b'p', None, b"!"),
        Dispatch::Handled(_)
    ));
    assert!(!protocol.modes().contains(&1007));
    apply_frost_mode(
        &mut screen,
        &mut protocol,
        ModeNamespace::DecPrivate,
        1007,
        true,
    );
    reset_frost_attributes(&mut screen, &mut protocol);
    assert!(!protocol.modes().contains(&1007));
}
#[test]
fn legacy_wheel_arrow_helper_follows_decckm_without_modifiers_or_keypad_changes() {
    for application in [false, true] {
        for (key, normal, app) in [
            (Key::ArrowUp, "\x1b[A", "\x1bOA"),
            (Key::ArrowDown, "\x1b[B", "\x1bOB"),
        ] {
            assert_eq!(
                legacy_function_key_sequence(key, Modifiers::default(), application).unwrap(),
                if application { app } else { normal }
            );
        }
    }
}
