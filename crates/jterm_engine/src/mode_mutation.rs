//! Profile-specific mode mutations. The host applies each parameter and its
//! effects in wire order, including the alternate-screen and sync lifecycle.
use crate::{protocol_state::ProtocolState, screen::ScreenState};

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

/// Apply one FrostLegacyV1 mode parameter. A mouse change requires the host
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

    let effect = match mode {
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
        1000..=1003 | 1005 | 1006 | 1015 if protocol.modes().contains(&mode) != enabled => {
            MouseReportingChanged
        }
        1 | 7 | 25 | 66 | 1004 | 2004 | 2031 | 5522 => None,
        _ => return None,
    };

    if enabled {
        protocol.modes_mut().insert(mode);
    } else {
        protocol.modes_mut().remove(&mode);
    }
    effect
}
