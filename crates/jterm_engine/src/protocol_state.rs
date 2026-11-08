//! VT mode and keyboard protocol state shared independently of a renderer.
//! The keyboard stacks are screen-local; xterm keyboard settings are global.

/// Recognized DEC private modes; unknown mode numbers remain absent.
#[derive(Clone, Debug, Default)]
pub struct TerminalModes {
    bits: u32,
}

impl TerminalModes {
    const fn bit_index(mode: u16) -> Option<u32> {
        match mode {
            // DECCKM — application cursor keys. Keep this independent from
            // alternate-screen modes: full-screen applications explicitly
            // opt into the SS3 cursor-key sequences with CSI ? 1 h.
            1 => Some(13),
            7 => Some(0),
            25 => Some(1),
            1000 => Some(2),
            1001 => Some(3),
            1002 => Some(4),
            1003 => Some(5),
            1004 => Some(6),
            1006 => Some(7),
            1049 => Some(8),
            2004 => Some(9),
            2026 => Some(10),
            2031 => Some(11),
            5522 => Some(12),
            // Kept as bits so DECRQM can answer them truthfully: 47/1047 enter
            // the alternate screen, 1048 is the DECSC-style save, and 1005/1015
            // select the UTF-8 and urxvt mouse encodings.
            47 => Some(14),
            1047 => Some(15),
            1048 => Some(16),
            1005 => Some(17),
            1015 => Some(18),
            66 => Some(19),
            // Alternate Scroll: wheel on the alt screen becomes cursor keys
            // when mouse reporting is off (xterm 1007 / alternateScroll).
            1007 => Some(20),
            // ATT610 / DECSET 12: cursor blink. Ember stores the request so
            // DECRQM answers truthfully even though the renderer is steady.
            12 => Some(21),
            // 1036 metaSendsEscape: Alt/Meta prefixes ESC. Default on, as in xterm.
            1036 => Some(22),
            // 1034 eightBitInput: we stay UTF-8 7-bit C1, but DECRQM must answer.
            1034 => Some(23),
            _ => None,
        }
    }

    #[inline]
    pub fn contains(&self, mode: &u16) -> bool {
        match Self::bit_index(*mode) {
            Some(bit) => self.bits & (1 << bit) != 0,
            None => false,
        }
    }

    #[inline]
    pub fn insert(&mut self, mode: u16) {
        if let Some(bit) = Self::bit_index(mode) {
            self.bits |= 1 << bit;
        }
    }

    #[inline]
    pub fn remove(&mut self, mode: &u16) {
        if let Some(bit) = Self::bit_index(*mode) {
            self.bits &= !(1 << bit);
        }
    }
}

/// Active/inactive are relative to the displayed screen, not primary/alternate.
/// Switching screens swaps only the Kitty flags and stacks.
#[derive(Clone, Debug, Default)]
pub struct ProtocolState {
    modes: TerminalModes,
    keyboard_enhancement_flags: u16,
    keyboard_enhancement_stack: Vec<u16>,
    alt_keyboard_enhancement_flags: u16,
    alt_keyboard_enhancement_stack: Vec<u16>,
    xterm_modify_other_keys: u16,
    xterm_format_other_keys: u16,
}

impl ProtocolState {
    pub fn with_modes(modes: TerminalModes) -> Self {
        Self {
            modes,
            ..Self::default()
        }
    }

    pub fn modes(&self) -> &TerminalModes {
        &self.modes
    }
    pub fn modes_mut(&mut self) -> &mut TerminalModes {
        &mut self.modes
    }
    pub fn keyboard_enhancement_flags(&self) -> u16 {
        self.keyboard_enhancement_flags
    }
    pub fn keyboard_enhancement_stack(&self) -> &[u16] {
        &self.keyboard_enhancement_stack
    }
    pub fn inactive_keyboard_enhancement_flags(&self) -> u16 {
        self.alt_keyboard_enhancement_flags
    }
    pub fn inactive_keyboard_enhancement_stack(&self) -> &[u16] {
        &self.alt_keyboard_enhancement_stack
    }
    pub fn xterm_modify_other_keys(&self) -> u16 {
        self.xterm_modify_other_keys
    }
    pub fn xterm_format_other_keys(&self) -> u16 {
        self.xterm_format_other_keys
    }
    pub fn set_xterm_modify_other_keys(&mut self, value: u16) {
        self.xterm_modify_other_keys = value;
    }
    pub fn set_xterm_format_other_keys(&mut self, value: u16) {
        self.xterm_format_other_keys = value;
    }

    pub fn switch_screen(&mut self) {
        std::mem::swap(
            &mut self.keyboard_enhancement_flags,
            &mut self.alt_keyboard_enhancement_flags,
        );
        std::mem::swap(
            &mut self.keyboard_enhancement_stack,
            &mut self.alt_keyboard_enhancement_stack,
        );
    }

    pub fn set_keyboard_enhancement_flags(&mut self, flags: u16, mode: u16) {
        match mode {
            1 => self.keyboard_enhancement_flags = flags,
            2 => self.keyboard_enhancement_flags |= flags,
            3 => self.keyboard_enhancement_flags &= !flags,
            _ => {}
        }
    }

    pub fn push_keyboard_enhancement_flags(&mut self, flags: u16) {
        if self.keyboard_enhancement_stack.len() >= 32 {
            self.keyboard_enhancement_stack.remove(0);
        }
        self.keyboard_enhancement_stack
            .push(self.keyboard_enhancement_flags);
        self.keyboard_enhancement_flags = flags;
    }

    pub fn pop_keyboard_enhancement_flags(&mut self, count: usize) {
        for _ in 0..count.max(1) {
            match self.keyboard_enhancement_stack.pop() {
                Some(flags) => self.keyboard_enhancement_flags = flags,
                None => {
                    self.keyboard_enhancement_flags = 0;
                    break;
                }
            }
        }
    }

    /// Shell-prompt recovery clears only the currently active screen.
    /// DECSTR does not invoke this; RIS replaces the complete protocol state.
    pub fn reset_keyboard_enhancement(&mut self) {
        self.keyboard_enhancement_flags = 0;
        self.keyboard_enhancement_stack.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_mode_mapping_and_unknown_modes() {
        let mapping = [
            7, 25, 1000, 1001, 1002, 1003, 1004, 1006, 1049, 2004, 2026, 2031, 5522, 1, 47, 1047,
            1048, 1005, 1015, 66, 1007, 12, 1036, 1034,
        ];
        let mut modes = TerminalModes::default();
        for (bit, mode) in mapping.into_iter().enumerate() {
            modes.insert(mode);
            assert!(modes.contains(&mode));
            assert_eq!(modes.bits, (1u32 << (bit + 1)) - 1);
        }
        for unknown in [0, 6, 9, 1016, u16::MAX] {
            let before = modes.bits;
            modes.insert(unknown);
            modes.remove(&unknown);
            assert!(!modes.contains(&unknown));
            assert_eq!(modes.bits, before);
        }
        for mode in mapping {
            modes.remove(&mode);
            assert!(!modes.contains(&mode));
        }
        assert_eq!(modes.bits, 0);
    }

    #[test]
    fn keyboard_operations_preserve_all_bits_and_ignore_unknown_operations() {
        let mut state = ProtocolState::default();
        state.set_keyboard_enhancement_flags(0x8008, 1);
        state.set_keyboard_enhancement_flags(0x4017, 2);
        assert_eq!(state.keyboard_enhancement_flags(), 0xc01f);
        state.set_keyboard_enhancement_flags(0x8004, 3);
        assert_eq!(state.keyboard_enhancement_flags(), 0x401b);
        for operation in [0, 4, u16::MAX] {
            state.set_keyboard_enhancement_flags(0, operation);
        }
        assert_eq!(state.keyboard_enhancement_flags(), 0x401b);
    }

    #[test]
    fn bounded_stack_drops_oldest_and_pop_zero_means_one() {
        let mut state = ProtocolState::default();
        for flags in 1..=40 {
            state.push_keyboard_enhancement_flags(flags);
        }
        assert_eq!(
            state.keyboard_enhancement_stack(),
            &(8..40).collect::<Vec<_>>()
        );
        state.pop_keyboard_enhancement_flags(0);
        assert_eq!(state.keyboard_enhancement_flags(), 39);
        state.pop_keyboard_enhancement_flags(31);
        assert_eq!(state.keyboard_enhancement_flags(), 8);
        assert!(state.keyboard_enhancement_stack().is_empty());
        state.pop_keyboard_enhancement_flags(usize::MAX);
        assert_eq!(state.keyboard_enhancement_flags(), 0);
    }

    #[test]
    fn screen_swap_and_prompt_reset_are_local_but_xterm_and_modes_are_global() {
        let mut state = ProtocolState::default();
        state.modes_mut().insert(2004);
        state.set_xterm_modify_other_keys(u16::MAX);
        state.set_xterm_format_other_keys(31);
        state.push_keyboard_enhancement_flags(17);
        state.push_keyboard_enhancement_flags(31);
        state.switch_screen();
        assert_eq!(state.keyboard_enhancement_flags(), 0);
        assert!(state.keyboard_enhancement_stack().is_empty());
        assert_eq!(state.inactive_keyboard_enhancement_flags(), 31);
        assert_eq!(state.inactive_keyboard_enhancement_stack(), &[0, 17]);
        state.push_keyboard_enhancement_flags(8);
        state.reset_keyboard_enhancement();
        state.switch_screen();
        assert_eq!(state.keyboard_enhancement_flags(), 31);
        assert_eq!(state.keyboard_enhancement_stack(), &[0, 17]);
        assert_eq!(state.inactive_keyboard_enhancement_flags(), 0);
        assert!(state.inactive_keyboard_enhancement_stack().is_empty());
        assert_eq!(state.xterm_modify_other_keys(), u16::MAX);
        assert_eq!(state.xterm_format_other_keys(), 31);
        assert!(state.modes().contains(&2004));
    }
}
