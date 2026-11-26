//! Ordered native alternate-screen transitions. The engine switches the actual
//! grid, drawing state and keyboard stacks. Hosts retain selection, graphics,
//! history and rendering work at the explicit synchronous hook boundaries.
use crate::colors::DynamicColorState;
use crate::grid::Color;
use crate::hyperlink::HyperlinkId;
use crate::protocol_state::ProtocolState;
use crate::screen::{AlternateRestorePolicy, ScreenState};
use std::ops::Range;

pub struct NativeAlternateState<'a> {
    pub screen: &'a mut ScreenState,
    pub protocol: &'a mut ProtocolState,
    pub global_background: &'a mut Color,
}

pub struct FrostAlternateState<'a> {
    pub native: NativeAlternateState<'a>,
    pub disabled: bool,
    pub saved_background: &'a mut Color,
    pub colors: &'a mut DynamicColorState,
    pub saved_colors: &'a mut DynamicColorState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NativeAlternateHook {
    PrepareEntry,
    PrepareExit,
    GridSwapped,
    OwnershipChanged {
        entering: bool,
    },
    EntryCleared {
        rows: Range<usize>,
    },
    ClearSelection,
    /// Frost's original clear includes ordered history admission and paint
    /// invalidation. Its mode bit must remain unchanged until this returns.
    ClearAlternate,
    FinalizeAlternate,
    Repaint {
        rows: Range<usize>,
    },
}

pub trait NativeAlternateHost {
    fn alternate_state(&mut self) -> NativeAlternateState<'_>;
    fn alternate_hook(&mut self, hook: NativeAlternateHook);
}

pub trait FrostAlternateHost: NativeAlternateHost {
    fn frost_alternate_state(&mut self) -> FrostAlternateState<'_>;
}

/// Ember retains its native DEC 47/1047/1049 save/clear policy. A repeated entry
/// has no effects, including no keyboard-stack swap or host notification.
pub fn enter_ember(host: &mut impl NativeAlternateHost, save_cursor: bool, clear: bool) -> bool {
    if host.alternate_state().screen.use_alt_buffer {
        return false;
    }
    host.alternate_hook(NativeAlternateHook::PrepareEntry);
    let entry = {
        let state = host.alternate_state();
        state
            .screen
            .enter_alternate_screen(save_cursor, clear, state.protocol)
            .expect("inactive alternate screen")
    };
    // Ember's graphics ownership follows the grid, before keyboard ownership.
    host.alternate_hook(NativeAlternateHook::GridSwapped);
    host.alternate_state().protocol.switch_screen();
    host.alternate_hook(NativeAlternateHook::OwnershipChanged { entering: true });
    if let Some(rows) = entry.cleared_rows {
        host.alternate_hook(NativeAlternateHook::EntryCleared { rows });
    }
    true
}

pub fn leave_ember(host: &mut impl NativeAlternateHost, restore_cursor: bool) -> bool {
    if !host.alternate_state().screen.use_alt_buffer {
        return false;
    }
    host.alternate_hook(NativeAlternateHook::PrepareExit);
    let exit = host
        .alternate_state()
        .screen
        .leave_alternate_screen(restore_cursor)
        .expect("active alternate screen");
    host.alternate_hook(NativeAlternateHook::GridSwapped);
    host.alternate_state().protocol.switch_screen();
    host.alternate_hook(NativeAlternateHook::OwnershipChanged { entering: false });
    {
        let state = host.alternate_state();
        *state.global_background = exit.global_background;
        if let Some(autowrap) = exit.restore_autowrap {
            if autowrap {
                state.protocol.modes_mut().insert(7);
            } else {
                state.protocol.modes_mut().remove(&7);
            }
        }
    }
    host.alternate_hook(NativeAlternateHook::Repaint {
        rows: exit.dirty_rows,
    });
    true
}

/// Frost saves its palette/background and primary cursor for all three modes.
/// Clearing and history callbacks observe the switched keyboard stack, empty
/// hyperlink and the old mode bit; the DEC mode is committed only afterward.
pub fn enter_frost(host: &mut impl FrostAlternateHost, mode: u16) -> bool {
    {
        let state = host.frost_alternate_state();
        if state.disabled || state.native.screen.use_alt_buffer {
            return false;
        }
        *state.saved_background = *state.native.global_background;
        state.native.screen.saved_primary_cursor_shape = Some(state.native.screen.cursor_shape);
        *state.saved_colors = state.colors.clone();
    }
    host.alternate_hook(NativeAlternateHook::PrepareEntry);
    {
        let state = host.alternate_state();
        let autowrap = state.protocol.modes().contains(&7);
        let entered = state
            .screen
            .alternate_screen_controls()
            .enter(true, autowrap);
        debug_assert!(entered);
        state.protocol.switch_screen();
    }
    host.alternate_hook(NativeAlternateHook::OwnershipChanged { entering: true });
    host.alternate_state().screen.current_hyperlink = HyperlinkId::NONE;
    host.alternate_hook(NativeAlternateHook::ClearSelection);
    host.alternate_hook(NativeAlternateHook::ClearAlternate);
    host.alternate_state().protocol.modes_mut().insert(mode);
    true
}

pub fn leave_frost(host: &mut impl FrostAlternateHost, mode: u16) -> bool {
    let Some(exit) = host
        .alternate_state()
        .screen
        .alternate_screen_controls()
        .leave(AlternateRestorePolicy::Frost)
    else {
        return false;
    };
    host.alternate_state().protocol.switch_screen();
    host.alternate_hook(NativeAlternateHook::OwnershipChanged { entering: false });
    host.alternate_hook(NativeAlternateHook::FinalizeAlternate);
    {
        let state = host.alternate_state();
        state.protocol.modes_mut().remove(&mode);
        state.screen.current_hyperlink = HyperlinkId::NONE;
    }
    host.alternate_hook(NativeAlternateHook::ClearSelection);
    {
        let state = host.frost_alternate_state();
        *state.native.global_background = *state.saved_background;
        *state.saved_background = Color::Default;
        if let Some(shape) = state.native.screen.saved_primary_cursor_shape {
            state.native.screen.cursor_shape = shape;
        }
        *state.colors = state.saved_colors.clone();
        state.saved_colors.reset_dynamic();
    }
    host.alternate_hook(NativeAlternateHook::Repaint {
        rows: exit.dirty_rows,
    });
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::CursorShape;

    #[derive(Debug)]
    struct Observation {
        hook: NativeAlternateHook,
        alternate: bool,
        keyboard: u16,
        hyperlink: HyperlinkId,
        modes: [bool; 3],
        background: Color,
        pen_background: Color,
        autowrap: bool,
        foreground_override: Option<(u8, u8, u8)>,
    }
    struct Host {
        screen: ScreenState,
        protocol: ProtocolState,
        background: Color,
        saved_background: Color,
        colors: DynamicColorState,
        saved_colors: DynamicColorState,
        disabled: bool,
        observations: Vec<Observation>,
    }
    impl Host {
        fn seeded() -> Self {
            let mut screen = ScreenState::new(3, 6);
            screen.current_fg = Color::Indexed(3);
            screen.current_bg = Color::Indexed(4);
            screen.cursor_row = 1;
            screen.cursor_col = 5;
            screen.pending_wrap = true;
            screen.cursor_shape = CursorShape::Underline;
            screen.current_hyperlink = HyperlinkId::from_raw(17);
            let mut protocol = ProtocolState::default();
            protocol.modes_mut().insert(7);
            protocol.push_keyboard_enhancement_flags(17);
            protocol.switch_screen();
            protocol.push_keyboard_enhancement_flags(8);
            protocol.switch_screen();
            let colors = DynamicColorState {
                dynamic_fg: Some((1, 2, 3)),
                ..DynamicColorState::default()
            };
            Self {
                screen,
                protocol,
                background: Color::Indexed(11),
                saved_background: Color::Default,
                colors,
                saved_colors: DynamicColorState::default(),
                disabled: false,
                observations: Vec::new(),
            }
        }
    }
    impl NativeAlternateHost for Host {
        fn alternate_state(&mut self) -> NativeAlternateState<'_> {
            NativeAlternateState {
                screen: &mut self.screen,
                protocol: &mut self.protocol,
                global_background: &mut self.background,
            }
        }
        fn alternate_hook(&mut self, hook: NativeAlternateHook) {
            self.observations.push(Observation {
                hook: hook.clone(),
                alternate: self.screen.use_alt_buffer,
                keyboard: self.protocol.keyboard_enhancement_flags(),
                hyperlink: self.screen.current_hyperlink,
                modes: [47, 1047, 1049].map(|mode| self.protocol.modes().contains(&mode)),
                background: self.background,
                pen_background: self.screen.current_bg,
                autowrap: self.protocol.modes().contains(&7),
                foreground_override: self.colors.dynamic_fg,
            });
            if hook == NativeAlternateHook::ClearAlternate {
                self.screen.erase_screen();
                self.screen.cursor_row = 0;
                self.screen.cursor_col = 0;
            }
        }
    }
    impl FrostAlternateHost for Host {
        fn frost_alternate_state(&mut self) -> FrostAlternateState<'_> {
            FrostAlternateState {
                native: NativeAlternateState {
                    screen: &mut self.screen,
                    protocol: &mut self.protocol,
                    global_background: &mut self.background,
                },
                disabled: self.disabled,
                saved_background: &mut self.saved_background,
                colors: &mut self.colors,
                saved_colors: &mut self.saved_colors,
            }
        }
    }
    #[test]
    fn ember_graphics_precedes_keyboard_and_repaint_observes_restored_pen() {
        let mut host = Host::seeded();
        assert!(enter_ember(&mut host, true, true));
        assert_eq!(
            host.observations
                .iter()
                .map(|s| s.hook.clone())
                .collect::<Vec<_>>(),
            [
                NativeAlternateHook::PrepareEntry,
                NativeAlternateHook::GridSwapped,
                NativeAlternateHook::OwnershipChanged { entering: true },
                NativeAlternateHook::EntryCleared { rows: 0..3 }
            ]
        );
        assert_eq!(host.observations[1].keyboard, 17);
        assert!(host.observations[1].alternate);
        assert_eq!(host.observations[1].hyperlink, HyperlinkId::NONE);
        assert_eq!(host.observations[2].keyboard, 8);
        host.observations.clear();
        host.protocol.modes_mut().remove(&7);
        host.screen.current_bg = Color::Indexed(9);
        host.background = Color::Indexed(9);
        assert!(leave_ember(&mut host, true));
        assert_eq!(
            host.observations
                .iter()
                .map(|s| s.hook.clone())
                .collect::<Vec<_>>(),
            [
                NativeAlternateHook::PrepareExit,
                NativeAlternateHook::GridSwapped,
                NativeAlternateHook::OwnershipChanged { entering: false },
                NativeAlternateHook::Repaint { rows: 0..3 }
            ]
        );
        assert_eq!(host.observations[1].keyboard, 8);
        assert_eq!(host.observations[2].keyboard, 17);
        assert_eq!(host.observations[2].background, Color::Indexed(9));
        assert_eq!(host.observations[2].pen_background, Color::Indexed(4));
        assert!(!host.observations[2].autowrap);
        assert!(host.observations[3].autowrap);
        assert_eq!(host.observations[3].background, Color::Indexed(4));
        assert!(host.protocol.modes().contains(&7));
        assert!(!host.screen.pending_wrap);
        assert_eq!(host.screen.saved_primary_cursor_shape, None);
    }
    #[test]
    fn frost_clear_and_retirement_observe_ordered_modes_links_and_palette() {
        for mode in [47, 1047, 1049] {
            let mut host = Host::seeded();
            assert!(enter_frost(&mut host, mode));
            assert_eq!(
                host.observations
                    .iter()
                    .map(|s| s.hook.clone())
                    .collect::<Vec<_>>(),
                [
                    NativeAlternateHook::PrepareEntry,
                    NativeAlternateHook::OwnershipChanged { entering: true },
                    NativeAlternateHook::ClearSelection,
                    NativeAlternateHook::ClearAlternate
                ]
            );
            assert_eq!(host.observations[1].keyboard, 8);
            assert_eq!(host.observations[1].hyperlink, HyperlinkId::from_raw(17));
            assert_eq!(host.observations[2].hyperlink, HyperlinkId::NONE);
            assert_eq!(host.observations[3].modes, [false; 3]);
            assert!(host.protocol.modes().contains(&mode));
            host.observations.clear();
            host.colors.dynamic_fg = Some((4, 5, 6));
            host.background = Color::Indexed(12);
            host.screen.current_hyperlink = HyperlinkId::from_raw(23);
            host.screen.cursor_shape = CursorShape::Beam;
            host.protocol.modes_mut().remove(&7);
            assert!(leave_frost(&mut host, mode));
            assert_eq!(
                host.observations
                    .iter()
                    .map(|s| s.hook.clone())
                    .collect::<Vec<_>>(),
                [
                    NativeAlternateHook::OwnershipChanged { entering: false },
                    NativeAlternateHook::FinalizeAlternate,
                    NativeAlternateHook::ClearSelection,
                    NativeAlternateHook::Repaint { rows: 0..3 }
                ]
            );
            assert_eq!(host.observations[0].keyboard, 17);
            assert!(!host.observations[0].alternate);
            assert_eq!(host.observations[1].hyperlink, HyperlinkId::from_raw(23));
            let index = [47, 1047, 1049]
                .iter()
                .position(|value| *value == mode)
                .unwrap();
            assert!(host.observations[1].modes[index]);
            assert_eq!(host.observations[2].modes, [false; 3]);
            assert_eq!(host.observations[2].hyperlink, HyperlinkId::NONE);
            assert_eq!(host.observations[2].foreground_override, Some((4, 5, 6)));
            assert_eq!(host.observations[3].foreground_override, Some((1, 2, 3)));
            assert_eq!(host.background, Color::Indexed(11));
            assert_eq!(host.saved_background, Color::Default);
            assert_eq!(host.saved_colors, DynamicColorState::default());
            assert_eq!(host.screen.cursor_shape, CursorShape::Underline);
            assert_eq!(
                host.screen.saved_primary_cursor_shape,
                Some(CursorShape::Underline)
            );
            assert!(host.screen.pending_wrap);
            assert!(!host.protocol.modes().contains(&7));
        }
    }
    #[test]
    fn repeated_and_disabled_transitions_do_not_touch_stacks_saves_or_hooks() {
        let mut host = Host::seeded();
        host.disabled = true;
        assert!(!enter_frost(&mut host, 1049));
        assert!(!leave_frost(&mut host, 1049));
        assert!(host.observations.is_empty());
        assert_eq!(host.protocol.keyboard_enhancement_flags(), 17);
        host.disabled = false;
        assert!(enter_frost(&mut host, 47));
        host.observations.clear();
        let saved = format!("{:?}", host.screen.saved_primary_screen_state);
        assert!(!enter_frost(&mut host, 1049));
        assert!(!enter_ember(&mut host, true, true));
        assert!(host.observations.is_empty());
        assert_eq!(
            format!("{:?}", host.screen.saved_primary_screen_state),
            saved
        );
        assert_eq!(host.protocol.keyboard_enhancement_flags(), 8);
        assert!(!host.protocol.modes().contains(&1049));
        host.disabled = true; // Disabling future entry must not trap an active TUI.
        assert!(leave_frost(&mut host, 47));
        host.observations.clear();
        assert!(!leave_frost(&mut host, 47));
        assert!(!leave_ember(&mut host, true));
        assert!(host.observations.is_empty());
    }
}
