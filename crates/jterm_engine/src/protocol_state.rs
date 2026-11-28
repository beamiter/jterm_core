//! VT mode and keyboard protocol state shared independently of a renderer.
//! The keyboard stacks are screen-local; xterm keyboard settings are global.

/// Recognized DEC private modes; unknown mode numbers remain absent.
#[derive(Clone, Debug, Default)]
pub struct TerminalModes {
    bits: u32,
}

impl TerminalModes {
    // Pixel reporting requires a frontend that can supply measured positions.
    const PIXEL_MOUSE_CAPABILITY: u32 = 1 << 31;

    /// Enable DEC mode 1016 for an adapter that can report measured pixels.
    pub fn enable_pixel_mouse(&mut self) {
        self.bits |= Self::PIXEL_MOUSE_CAPABILITY;
    }

    pub fn supports_pixel_mouse(&self) -> bool {
        self.bits & Self::PIXEL_MOUSE_CAPABILITY != 0
    }

    pub(crate) fn set_mouse_encoding(&mut self, mode: u16, enabled: bool) {
        if !enabled {
            self.remove(&mode);
            return;
        }
        match mode {
            1016 if self.supports_pixel_mouse() => {
                for cell_encoding in [1005, 1006, 1015] {
                    self.remove(&cell_encoding);
                }
            }
            1005 | 1006 | 1015 => self.remove(&1016),
            _ => {}
        }
        self.insert(mode);
    }

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
            9 => Some(24),
            45 => Some(25),
            42 => Some(26),
            1016 => Some(27),
            // DECBKM — send BS on the backarrow key when set.
            67 => Some(28),
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
            if mode != 1016 || self.supports_pixel_mouse() {
                self.bits |= 1 << bit;
            }
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
#[derive(Clone, Default)]
pub struct ProtocolState {
    modes: TerminalModes,
    pub(crate) sgr_stack: crate::sgr_stack::SgrStack,
    attribute_change_extent: u8,
    pub(crate) saved_dec_private_modes: [Option<u8>; 18],
    keyboard_enhancement_flags: u16,
    keyboard_enhancement_stack: Vec<u16>,
    alt_keyboard_enhancement_flags: u16,
    alt_keyboard_enhancement_stack: Vec<u16>,
    keyboard_feature_mask: Option<u16>,
    keyboard_report_generation: u64,
    xterm_modify_other_keys: u16,
    xterm_format_other_keys: u16,
}

impl std::fmt::Debug for ProtocolState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut output = formatter.debug_struct("ProtocolState");
        output
            .field("modes", &self.modes)
            .field(
                "keyboard_enhancement_flags",
                &self.keyboard_enhancement_flags,
            )
            .field(
                "keyboard_enhancement_stack",
                &self.keyboard_enhancement_stack,
            )
            .field(
                "alt_keyboard_enhancement_flags",
                &self.alt_keyboard_enhancement_flags,
            )
            .field(
                "alt_keyboard_enhancement_stack",
                &self.alt_keyboard_enhancement_stack,
            );
        // Frozen production traces describe sessions without an explicit mask.
        if self.keyboard_feature_mask.is_some() {
            output.field("keyboard_feature_mask", &self.keyboard_feature_mask);
        }
        output
            .field("xterm_modify_other_keys", &self.xterm_modify_other_keys)
            .field("xterm_format_other_keys", &self.xterm_format_other_keys)
            .finish()
    }
}

impl ProtocolState {
    pub(crate) fn attribute_change_extent(&self) -> u8 {
        self.attribute_change_extent
    }

    pub(crate) fn rectangular_attribute_extent(&self) -> bool {
        self.attribute_change_extent == 2
    }

    pub(crate) fn set_attribute_change_extent(&mut self, value: u16) {
        if value <= 2 {
            self.attribute_change_extent = value as u8;
        }
    }

    /// Transfer the widget-global XTerm rendition stack when RIS replaces mode
    /// state. DECSTR and primary/alternate screen switches leave it untouched.
    pub fn take_rendition_stack_from(&mut self, previous: &mut Self) {
        self.sgr_stack = std::mem::take(&mut previous.sgr_stack);
    }

    pub fn with_modes(modes: TerminalModes) -> Self {
        Self {
            modes,
            ..Self::default()
        }
    }

    pub(crate) fn clear_saved_dec_modes(&mut self) {
        self.saved_dec_private_modes.fill(None);
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

    pub(crate) fn keyboard_report_generation(&self) -> u64 {
        self.keyboard_report_generation
    }

    fn retire_keyboard_reports(&mut self) {
        self.keyboard_report_generation = self.keyboard_report_generation.wrapping_add(1);
    }

    fn assign_keyboard_enhancements(&mut self, flags: u16) {
        if self.keyboard_enhancement_flags != flags {
            self.keyboard_enhancement_flags = flags;
            self.retire_keyboard_reports();
        }
    }

    pub(crate) fn retire_keyboard_reports_from(&mut self, before: &Self) {
        self.keyboard_report_generation = before.keyboard_report_generation;
        self.retire_keyboard_reports();
    }

    pub(crate) fn limit_keyboard_enhancements(&mut self, mask: u16) {
        self.keyboard_feature_mask = Some(mask);
        self.assign_keyboard_enhancements(self.keyboard_enhancement_flags & mask);
        self.alt_keyboard_enhancement_flags &= mask;
        for flags in self
            .keyboard_enhancement_stack
            .iter_mut()
            .chain(&mut self.alt_keyboard_enhancement_stack)
        {
            *flags &= mask;
        }
    }

    pub(crate) fn keyboard_enhancement_limit(&self) -> Option<u16> {
        self.keyboard_feature_mask
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
        self.retire_keyboard_reports();
    }

    pub fn set_keyboard_enhancement_flags(&mut self, flags: u16, mode: u16) {
        let flags = flags & self.keyboard_feature_mask.unwrap_or(u16::MAX);
        let flags = match mode {
            1 => flags,
            2 => self.keyboard_enhancement_flags | flags,
            3 => self.keyboard_enhancement_flags & !flags,
            _ => return,
        };
        self.assign_keyboard_enhancements(flags);
    }

    pub fn push_keyboard_enhancement_flags(&mut self, flags: u16) {
        if self.keyboard_enhancement_stack.len() >= 32 {
            self.keyboard_enhancement_stack.remove(0);
        }
        self.keyboard_enhancement_stack
            .push(self.keyboard_enhancement_flags);
        self.assign_keyboard_enhancements(flags & self.keyboard_feature_mask.unwrap_or(u16::MAX));
    }

    pub fn pop_keyboard_enhancement_flags(&mut self, count: usize) {
        let mut flags = self.keyboard_enhancement_flags;
        for _ in 0..count.max(1) {
            match self.keyboard_enhancement_stack.pop() {
                Some(saved) => flags = saved,
                None => {
                    flags = 0;
                    break;
                }
            }
        }
        self.assign_keyboard_enhancements(flags);
    }

    /// Shell-prompt recovery clears only the currently active screen.
    /// DECSTR does not invoke this; RIS replaces the complete protocol state.
    pub fn reset_keyboard_enhancement(&mut self) {
        self.assign_keyboard_enhancements(0);
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
            1048, 1005, 1015, 66, 1007, 12, 1036, 1034, 9, 45,
        ];
        let mut modes = TerminalModes::default();
        for (bit, mode) in mapping.into_iter().enumerate() {
            modes.insert(mode);
            assert!(modes.contains(&mode));
            assert_eq!(modes.bits, (1u32 << (bit + 1)) - 1);
        }
        for unknown in [0, 6, 1016, u16::MAX] {
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
