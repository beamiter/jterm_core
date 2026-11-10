//! Ember per-parameter mode mutations. The host retains CSI admission, ordered
//! iteration, observer cadence and all lifecycle hooks. This is not a policy
//! for other terminal profiles and does not dispatch whole CSI sequences.
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
