//! Profile-specific mode mutations. Shared native commands commit parameters
//! in wire order; hosts consume ordered window, archive and sync effects.
use crate::{protocol_state::ProtocolState, screen::ScreenState};

/// Profiles admit only Boolean modes their ordinary DECSET path supports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SavedDecModeProfile {
    Hosted,
    Ember,
    FrostLegacy,
}
const TRACKING_MODES: &[u16] = &[9, 1000, 1002, 1003];
const ENCODING_MODES: &[u16] = &[1005, 1006, 1015];
const PIXEL_ENCODING_MODES: &[u16] = &[1005, 1006, 1015, 1016];
fn saved_mode_slot(
    profile: SavedDecModeProfile,
    mode: u16,
    pixel_mouse: bool,
) -> Option<(usize, &'static [u16])> {
    let result = match mode {
        1 => (0, &[1][..]),
        6 => (1, &[6][..]),
        7 => (2, &[7][..]),
        12 => (3, &[12][..]),
        25 => (4, &[25][..]),
        66 => (5, &[66][..]),
        1004 => (6, &[1004][..]),
        1007 => (7, &[1007][..]),
        1034 => (8, &[1034][..]),
        1036 => (9, &[1036][..]),
        2004 => (10, &[2004][..]),
        9 => (11, TRACKING_MODES),
        1000 | 1002 | 1003 => (12, TRACKING_MODES),
        1005 | 1006 | 1015 | 1016 if pixel_mouse => (13, PIXEL_ENCODING_MODES),
        1005 | 1006 | 1015 => (13, ENCODING_MODES),
        47 | 1047 | 1049 => (14, &[47][..]),
        45 => (15, &[45][..]),
        42 => (16, &[42][..]),
        _ => return None,
    };
    if profile != SavedDecModeProfile::Ember && matches!(mode, 12 | 1034 | 1036) {
        return None;
    }
    Some(result)
}
/// Save requested one-level slots, preserving independent mouse bits.
pub(crate) fn save_dec_private_modes(
    screen: &ScreenState,
    protocol: &mut ProtocolState,
    profile: SavedDecModeProfile,
    params: &[u16],
) {
    for &mode in params {
        let Some((slot, modes)) =
            saved_mode_slot(profile, mode, protocol.modes().supports_pixel_mouse())
        else {
            continue;
        };
        let mut bits = 0;
        for (index, &mode) in modes.iter().enumerate() {
            let enabled = match mode {
                6 => screen.origin_mode,
                47 => screen.use_alt_buffer,
                _ => protocol.modes().contains(&mode),
            };
            bits |= u8::from(enabled) << index;
        }
        protocol.saved_dec_private_modes[slot] = Some(bits);
    }
}
/// Ordinary ordered DECSET/DECRST operations, preserving caller host effects.
pub(crate) fn restore_dec_private_modes(
    protocol: &ProtocolState,
    profile: SavedDecModeProfile,
    params: &[u16],
) -> smallvec::SmallVec<[(u16, bool); 16]> {
    let mut result = smallvec::SmallVec::new();
    for &mode in params {
        let Some((slot, modes)) =
            saved_mode_slot(profile, mode, protocol.modes().supports_pixel_mouse())
        else {
            continue;
        };
        let Some(bits) = protocol.saved_dec_private_modes[slot] else {
            continue;
        };
        for (index, &mode) in modes.iter().enumerate() {
            result.push((mode, bits & (1 << index) != 0));
        }
    }
    result
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModeNamespace {
    Ansi,
    DecPrivate,
}

/// Apply one Ember mode parameter. `false` means a host hook is required and
/// guarantees that both arguments remain completely untouched. In particular,
/// 5522 is host-owned in both directions, even though enabling it is just a bit.
/// Unknown pure modes are handled no-ops, as TerminalModes stores only known bits.
/// DECOM homes the cursor but deliberately preserves pending wrap.
pub fn apply_ember_mode(
    screen: &mut ScreenState,
    protocol: &mut ProtocolState,
    namespace: ModeNamespace,
    mode: u16,
    enabled: bool,
) -> bool {
    match namespace {
        ModeNamespace::Ansi => {
            if mode == 4 {
                screen.insert_mode = enabled;
            }
        }
        ModeNamespace::DecPrivate => match mode {
            1004 | 47 | 1047 | 1048 | 1049 | 2026 | 5522 => return false,
            6 => {
                screen.origin_mode = enabled;
                screen.cursor_row = if enabled { screen.scroll_region_top } else { 0 };
                screen.cursor_col = 0;
            }
            42 => {
                screen.national_replacement_mode = enabled;
                if enabled {
                    protocol.modes_mut().insert(42);
                } else {
                    protocol.modes_mut().remove(&42);
                }
            }
            1005 | 1006 | 1015 | 1016 => {
                protocol.modes_mut().set_mouse_encoding(mode, enabled);
            }
            _ => {
                if enabled {
                    protocol.modes_mut().insert(mode);
                } else {
                    protocol.modes_mut().remove(&mode);
                }
            }
        },
    }
    true
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrostModeEffect {
    None,
    MouseReportingChanged,
    AlternateScreen,
    SynchronizedOutput,
}

pub(crate) fn native_alternate_scroll_profile(
    screen: &ScreenState,
    protocol: &ProtocolState,
) -> Option<(bool, u16)> {
    (screen.use_alt_buffer
        && protocol.modes().contains(&1007)
        && !protocol.modes().is_mouse_enabled())
    .then(|| {
        (
            protocol.modes().contains(&1),
            protocol.keyboard_enhancement_flags(),
        )
    })
}

/// Apply one FrostLegacyV1 mode parameter. A mouse or alternate-scroll change requires the host
/// to revoke queued scroll input before proceeding to the next parameter.
/// Alternate screen and sync leave screen and protocol state to the host
/// until it can take the required snapshots and apply the transition.
pub fn apply_frost_mode(
    screen: &mut ScreenState,
    protocol: &mut ProtocolState,
    namespace: ModeNamespace,
    mode: u16,
    enabled: bool,
) -> FrostModeEffect {
    use FrostModeEffect::{AlternateScreen, MouseReportingChanged, None, SynchronizedOutput};

    if namespace == ModeNamespace::Ansi {
        if mode == 4 {
            screen.insert_mode = enabled;
        }
        return None;
    }

    if matches!(mode, 1005 | 1006 | 1015 | 1016) {
        let modes = protocol.modes_mut();
        let old = modes.contains(&mode);
        let was_pixel = modes.contains(&1016);
        modes.set_mouse_encoding(mode, enabled);
        return if old != modes.contains(&mode) || was_pixel != modes.contains(&1016) {
            MouseReportingChanged
        } else {
            None
        };
    }

    let effect = match mode {
        1 if protocol.modes().contains(&1) != enabled
            && native_alternate_scroll_profile(screen, protocol).is_some() =>
        {
            MouseReportingChanged
        }
        47 | 1047 | 1049 => return AlternateScreen,
        2026 => return SynchronizedOutput,
        6 => {
            screen.cursor_controls().set_origin_mode(enabled);
            return None;
        }
        1048 => {
            if enabled {
                screen.save_cursor_state(protocol);
            } else {
                // Frost restores the saved drawing state with live DECAWM.
                let _ = screen.restore_cursor();
            }
            None
        }
        9 | 1000..=1003 | 1007 if protocol.modes().contains(&mode) != enabled => {
            MouseReportingChanged
        }
        42 => {
            screen.national_replacement_mode = enabled;
            None
        }
        1 | 7 | 25 | 45 | 66 | 1004 | 1007 | 2004 | 2031 | 5522 => None,
        _ => return None,
    };

    if enabled {
        protocol.modes_mut().insert(mode);
    } else {
        protocol.modes_mut().remove(&mode);
    }
    effect
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeModeProfile {
    Ember,
    FrostLegacy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeModeEffect {
    None,
    Reply(&'static [u8]),
    RevokePasteGrant,
    MouseReportingChanged,
    AlternateScreen { mode: u16, enabled: bool },
    SynchronizedOutput { enabled: bool },
}

/// One parameter is committed before its effect is returned. The caller must
/// consume that effect before advancing to another parameter or input event.
pub struct NativeModeCommand<'a> {
    profile: NativeModeProfile,
    namespace: ModeNamespace,
    params: std::slice::Iter<'a, u16>,
    enabled: bool,
    track_alternate_scroll_changes: bool,
}

/// Both legacy profiles ignore intermediates for h/l. Ember admits ANSI and
/// DEC-private only; Frost historically treats every other namespace as ANSI.
pub fn prepare_native_mode_command<'a>(
    profile: NativeModeProfile,
    params: &'a [u16],
    cmd: char,
    prefix: Option<u8>,
) -> Option<NativeModeCommand<'a>> {
    let enabled = match cmd {
        'h' => true,
        'l' => false,
        _ => return None,
    };
    let namespace = match prefix {
        Some(b'?') => ModeNamespace::DecPrivate,
        None => ModeNamespace::Ansi,
        _ if profile == NativeModeProfile::FrostLegacy => ModeNamespace::Ansi,
        _ => return None,
    };
    Some(NativeModeCommand {
        profile,
        namespace,
        params: params.iter(),
        enabled,
        track_alternate_scroll_changes: false,
    })
}

impl NativeModeCommand<'_> {
    pub(crate) fn track_alternate_scroll_changes(&mut self) {
        self.track_alternate_scroll_changes = self.profile == NativeModeProfile::Ember;
    }
    pub fn step(
        &mut self,
        screen: &mut ScreenState,
        protocol: &mut ProtocolState,
        focused: bool,
    ) -> Option<NativeModeEffect> {
        let mode = *self.params.next()?;
        let previous = (self.track_alternate_scroll_changes
            && self.namespace == ModeNamespace::DecPrivate
            && matches!(mode, 1 | 1007 | 9 | 1000 | 1002 | 1003))
        .then(|| native_alternate_scroll_profile(screen, protocol));
        let effect = apply_native_mode_parameter(
            self.profile,
            screen,
            protocol,
            self.namespace,
            mode,
            self.enabled,
            focused,
        );
        Some(
            if matches!(effect, NativeModeEffect::None)
                && previous
                    .is_some_and(|old| native_alternate_scroll_profile(screen, protocol) != old)
            {
                NativeModeEffect::MouseReportingChanged
            } else {
                effect
            },
        )
    }
}

/// Apply a single native mode parameter, including the previously host-owned
/// focus/paste bits and Ember's DECSC/DECRC mode. Snapshot/archive and native
/// window clock operations remain ordered callbacks with unchanged policies.
pub fn apply_native_mode_parameter(
    profile: NativeModeProfile,
    screen: &mut ScreenState,
    protocol: &mut ProtocolState,
    namespace: ModeNamespace,
    mode: u16,
    enabled: bool,
    focused: bool,
) -> NativeModeEffect {
    if namespace == ModeNamespace::DecPrivate && mode == 1004 {
        let newly = !protocol.modes().contains(&1004);
        if enabled {
            protocol.modes_mut().insert(1004);
        } else {
            protocol.modes_mut().remove(&1004);
        }
        return if enabled && newly && focused {
            NativeModeEffect::Reply(b"\x1b[I")
        } else {
            NativeModeEffect::None
        };
    }
    if profile == NativeModeProfile::FrostLegacy {
        return match apply_frost_mode(screen, protocol, namespace, mode, enabled) {
            FrostModeEffect::None => NativeModeEffect::None,
            FrostModeEffect::MouseReportingChanged => NativeModeEffect::MouseReportingChanged,
            FrostModeEffect::AlternateScreen => NativeModeEffect::AlternateScreen { mode, enabled },
            FrostModeEffect::SynchronizedOutput => NativeModeEffect::SynchronizedOutput { enabled },
        };
    }
    let was_pixel = (namespace == ModeNamespace::DecPrivate
        && matches!(mode, 1005 | 1006 | 1015 | 1016))
    .then(|| protocol.modes().contains(&1016));
    if apply_ember_mode(screen, protocol, namespace, mode, enabled) {
        return if was_pixel.is_some_and(|old| old != protocol.modes().contains(&1016)) {
            NativeModeEffect::MouseReportingChanged
        } else {
            NativeModeEffect::None
        };
    }
    match mode {
        5522 => {
            if enabled {
                protocol.modes_mut().insert(5522);
                NativeModeEffect::None
            } else {
                protocol.modes_mut().remove(&5522);
                NativeModeEffect::RevokePasteGrant
            }
        }
        1048 => {
            if enabled {
                screen.save_cursor_state(protocol);
                protocol.modes_mut().insert(1048);
            } else {
                screen.restore_cursor_state(protocol);
                protocol.modes_mut().remove(&1048);
            }
            NativeModeEffect::None
        }
        47 | 1047 | 1049 => NativeModeEffect::AlternateScreen { mode, enabled },
        2026 => NativeModeEffect::SynchronizedOutput { enabled },
        _ => unreachable!("legacy pure mutation left an unknown host mode"),
    }
}

#[cfg(test)]
mod command_tests {
    use super::*;
    #[test]
    fn admission_and_focus_reply_are_profiled_and_parameter_ordered() {
        let params = [1004, 1004, 5522];
        let mut screen = ScreenState::new(2, 3);
        let mut protocol = ProtocolState::default();
        let mut command =
            prepare_native_mode_command(NativeModeProfile::Ember, &params, 'h', Some(b'?'))
                .unwrap();
        assert_eq!(
            command.step(&mut screen, &mut protocol, true),
            Some(NativeModeEffect::Reply(b"\x1b[I"))
        );
        assert!(!protocol.modes().contains(&5522));
        assert_eq!(
            command.step(&mut screen, &mut protocol, true),
            Some(NativeModeEffect::None)
        );
        assert_eq!(
            command.step(&mut screen, &mut protocol, true),
            Some(NativeModeEffect::None)
        );
        assert!(protocol.modes().contains(&5522));
        let mut command =
            prepare_native_mode_command(NativeModeProfile::Ember, &[5522], 'l', Some(b'?'))
                .unwrap();
        assert_eq!(
            command.step(&mut screen, &mut protocol, false),
            Some(NativeModeEffect::RevokePasteGrant)
        );
        assert!(!protocol.modes().contains(&5522));
        assert!(
            prepare_native_mode_command(NativeModeProfile::Ember, &params, 'h', Some(b'>'))
                .is_none()
        );
        assert!(prepare_native_mode_command(
            NativeModeProfile::FrostLegacy,
            &params,
            'h',
            Some(b'>')
        )
        .is_some());
    }

    #[test]
    fn cursor_save_restore_mode_is_owned_by_the_engine() {
        let mut screen = ScreenState::new(3, 4);
        let mut protocol = ProtocolState::default();
        screen.cursor_row = 1;
        screen.cursor_col = 2;
        apply_native_mode_parameter(
            NativeModeProfile::Ember,
            &mut screen,
            &mut protocol,
            ModeNamespace::DecPrivate,
            1048,
            true,
            false,
        );
        screen.cursor_row = 0;
        screen.cursor_col = 0;
        apply_native_mode_parameter(
            NativeModeProfile::Ember,
            &mut screen,
            &mut protocol,
            ModeNamespace::DecPrivate,
            1048,
            false,
            false,
        );
        assert_eq!((screen.cursor_row, screen.cursor_col), (1, 2));
    }
}

#[cfg(test)]
mod saved_mode_tests {
    use super::*;
    #[test]
    fn saved_slots_are_independent_grouped_and_exclude_actions() {
        let mut screen = ScreenState::new(3, 4);
        let mut protocol = ProtocolState::default();
        protocol.modes_mut().insert(1);
        protocol.modes_mut().insert(9);
        protocol.modes_mut().insert(1002);
        protocol.modes_mut().insert(1006);
        screen.origin_mode = true;
        screen.use_alt_buffer = true;
        save_dec_private_modes(
            &screen,
            &mut protocol,
            SavedDecModeProfile::Ember,
            &[1, 6, 1000, 1015, 1049, 1048, 2026, 2031, 5522],
        );
        protocol.modes_mut().remove(&1);
        screen.origin_mode = false;
        save_dec_private_modes(&screen, &mut protocol, SavedDecModeProfile::Ember, &[6]);
        assert_eq!(
            &restore_dec_private_modes(
                &protocol,
                SavedDecModeProfile::Ember,
                &[1, 6, 1003, 1005, 47, 1048, 2026, 2031, 5522]
            )[..],
            &[
                (1, true),
                (6, false),
                (9, true),
                (1000, false),
                (1002, true),
                (1003, false),
                (1005, false),
                (1006, true),
                (1015, false),
                (47, true)
            ]
        );
        assert!(restore_dec_private_modes(
            &ProtocolState::default(),
            SavedDecModeProfile::Ember,
            &[1, 47, 1000]
        )
        .is_empty());
        assert!(restore_dec_private_modes(
            &protocol,
            SavedDecModeProfile::FrostLegacy,
            &[12, 1034, 1036]
        )
        .is_empty());
    }
}

#[cfg(test)]
mod saved_reset_tests {
    use super::*;
    #[test]
    fn frost_ris_discards_saved_slots() {
        let mut screen = ScreenState::new(2, 3);
        let mut protocol = ProtocolState::default();
        protocol.modes_mut().insert(1);
        save_dec_private_modes(
            &screen,
            &mut protocol,
            SavedDecModeProfile::FrostLegacy,
            &[1],
        );
        assert_eq!(
            &restore_dec_private_modes(&protocol, SavedDecModeProfile::FrostLegacy, &[1])[..],
            &[(1, true)]
        );
        crate::native_reset::reset_frost_attributes(&mut screen, &mut protocol);
        assert!(
            restore_dec_private_modes(&protocol, SavedDecModeProfile::FrostLegacy, &[1]).is_empty()
        );
    }
}
