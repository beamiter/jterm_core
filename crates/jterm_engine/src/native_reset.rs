//! Native RIS state ownership. Hosts retain ordered history, graphics and
//! completion effects around these phases. Soft reset is a separate operation.
use crate::grid::{Color, CursorShape, StyleFlags};
use crate::hyperlink::HyperlinkId;
use crate::protocol_state::{ProtocolState, TerminalModes};
use crate::screen::{default_tab_stops, ScreenState, TabStopOrigin};

/// Ember RIS reconstructs both screens and all protocol state. Row identity
/// allocation belongs to the host and must run before publishing either grid.
pub fn fresh_ember_state(rows: usize, cols: usize) -> (ScreenState, ProtocolState) {
    let mut modes = frost_initial_modes();
    modes.insert(1007);
    modes.insert(1036);
    (
        ScreenState::new(rows, cols),
        ProtocolState::with_modes(modes),
    )
}

/// Frost construction and RIS share the same native mode defaults.
pub fn frost_initial_modes() -> TerminalModes {
    let mut modes = TerminalModes::default();
    modes.insert(25);
    modes.insert(7);
    modes
}

/// Frost first exits alternate screen and records interruption. This phase
/// precedes synchronous host history/provenance retirement and keeps both grids.
pub fn reset_frost_attributes(screen: &mut ScreenState, protocol: &mut ProtocolState) {
    screen.current_fg = Color::Default;
    screen.current_bg = Color::Default;
    screen.current_flags = StyleFlags::default();
    screen.reset_charset_banks();
    screen.scroll_region_top = 0;
    screen.scroll_region_bottom = screen.grid.rows().saturating_sub(1);
    screen.tab_stops = default_tab_stops(screen.grid.row_len(), TabStopOrigin::Clear);
    screen.saved_state = None;
    *protocol.modes_mut() = frost_initial_modes();
    protocol.clear_saved_dec_modes();
    protocol.set_attribute_change_extent(0);
    screen.insert_mode = false;
    screen.origin_mode = false;
    screen.pending_wrap = false;
}

/// After history retirement Frost resets both screen-local Kitty stacks while
/// retaining global xterm settings. Host selection/link registries and final
/// screen erasure follow this phase, with their original archive policy.
pub fn reset_frost_protocol_and_cursor(screen: &mut ScreenState, protocol: &mut ProtocolState) {
    screen.cursor_shape = CursorShape::default();
    protocol.reset_keyboard_enhancement();
    protocol.switch_screen();
    protocol.reset_keyboard_enhancement();
    protocol.switch_screen();
    screen.current_hyperlink = HyperlinkId::NONE;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_ris_profiles_keep_distinct_keyboard_and_tab_policies() {
        let (ember, protocol) = fresh_ember_state(3, 17);
        assert!(ember.tab_stops[0]);
        assert!(ember.tab_stops[8]);
        assert!(protocol.modes().contains(&1007));
        let mut frost = ScreenState::new_frost_legacy(3, 17);
        let mut protocol = ProtocolState::default();
        protocol.set_xterm_modify_other_keys(2);
        protocol.push_keyboard_enhancement_flags(17);
        protocol.switch_screen();
        protocol.push_keyboard_enhancement_flags(8);
        reset_frost_attributes(&mut frost, &mut protocol);
        reset_frost_protocol_and_cursor(&mut frost, &mut protocol);
        assert!(!frost.tab_stops[0]);
        assert!(frost.tab_stops[8]);
        assert!(!protocol.modes().contains(&1007));
        assert_eq!(protocol.xterm_modify_other_keys(), 2);
        assert_eq!(protocol.keyboard_enhancement_flags(), 0);
        assert_eq!(protocol.inactive_keyboard_enhancement_flags(), 0);
    }
}
