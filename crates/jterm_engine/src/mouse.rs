//! Mouse reporting encoders extracted from Ember, including legacy limits.
use crate::protocol_state::TerminalModes;

impl TerminalModes {
    pub fn get_mouse_report(&self, button: u8, col: usize, row: usize) -> Option<Vec<u8>> {
        // Check if any mouse reporting mode is enabled
        if !self.contains(&1000) && !self.contains(&1002) && !self.contains(&1003) {
            return None;
        }

        // SGR format (mode 1006) is preferred: CSI < button ; col ; row M/m
        // urxvt format (mode 1015): CSI button+32 ; col ; row M (decimal)
        // UTF-8 format (mode 1005): X10 frame with UTF-8 encoded coordinates
        // Standard format (mode 1000/1002): CSI M button col row (3 bytes)

        if self.contains(&1006) {
            // SGR format: CSI < button ; x ; y M (button press) or m (button release)
            // Releases use get_mouse_release_report instead.
            // Decimal SGR coordinates are not subject to the one-byte legacy
            // protocol limit. Keep them 1-indexed without truncating at 255.
            let x = col.saturating_add(1);
            let y = row.saturating_add(1);
            Some(format!("\x1b[<{};{};{}M", button, x, y).into_bytes())
        } else if self.contains(&1015) {
            Some(Self::urxvt_mouse_report(button, col, row))
        } else if self.contains(&1005) {
            Some(Self::utf8_mouse_report(button, col, row))
        } else {
            Some(Self::x10_mouse_report(button, col, row))
        }
    }

    /// Standard xterm format: CSI M button col row (raw bytes).
    /// Coordinates are 1-indexed, offset by 32, and capped at 223 so the
    /// encoded value fits in one byte. Clamp before narrowing to u8; casting
    /// first makes coordinates >= 256 wrap around.
    fn x10_mouse_report(button: u8, col: usize, row: usize) -> Vec<u8> {
        let button_byte = button.saturating_add(32);
        let col_byte = 32 + col.saturating_add(1).min(223) as u8;
        let row_byte = 32 + row.saturating_add(1).min(223) as u8;
        vec![b'\x1b', b'[', b'M', button_byte, col_byte, row_byte]
    }

    /// Mode 1005 keeps the X10 frame but writes each coordinate+32 as a UTF-8
    /// scalar, which lifts the 223-column cap to xterm's 2015.
    fn utf8_mouse_report(button: u8, col: usize, row: usize) -> Vec<u8> {
        let mut output = Vec::with_capacity(12);
        output.extend_from_slice(b"\x1b[M");
        output.push(button.saturating_add(32));
        for value in [col, row] {
            let codepoint = 32 + value.saturating_add(1).min(2015) as u32;
            if let Some(ch) = char::from_u32(codepoint) {
                let mut buf = [0u8; 4];
                output.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
            }
        }
        output
    }

    /// Mode 1015 (urxvt): decimal fields, button still offset by 32.
    fn urxvt_mouse_report(button: u8, col: usize, row: usize) -> Vec<u8> {
        format!(
            "\x1b[{};{};{}M",
            u16::from(button) + 32,
            col.saturating_add(1),
            row.saturating_add(1)
        )
        .into_bytes()
    }

    pub fn get_mouse_release_report(&self, button: u8, col: usize, row: usize) -> Option<Vec<u8>> {
        if !self.contains(&1000) && !self.contains(&1002) && !self.contains(&1003) {
            return None;
        }

        if self.contains(&1006) {
            // SGR format: lowercase 'm' for release
            let x = col.saturating_add(1);
            let y = row.saturating_add(1);
            Some(format!("\x1b[<{};{};{}m", button, x, y).into_bytes())
        } else if self.contains(&1015) {
            // Legacy encodings cannot name the released button: it is 3.
            Some(Self::urxvt_mouse_report(3, col, row))
        } else if self.contains(&1005) {
            Some(Self::utf8_mouse_report(3, col, row))
        } else {
            Some(Self::x10_mouse_report(3, col, row))
        }
    }

    pub fn is_mouse_enabled(&self) -> bool {
        self.contains(&1000) || self.contains(&1002) || self.contains(&1003)
    }

    /// 1002 reports motion only while a button is held; 1003 reports all
    /// pointer motion. Mode 1000 is press/release only.
    pub fn should_report_mouse_motion(&self, button_down: bool) -> bool {
        self.contains(&1003) || (button_down && self.contains(&1002))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reporting_enablement_and_motion_are_independent_of_encoding() {
        for encoding in [0, 1005, 1006, 1015] {
            let mut modes = TerminalModes::default();
            modes.insert(encoding);
            modes.insert(1001);
            assert_eq!(modes.get_mouse_report(0, 0, 0), None);
            assert_eq!(modes.get_mouse_release_report(0, 0, 0), None);
            assert!(!modes.is_mouse_enabled());
            for reporting in [1000, 1002, 1003] {
                modes.insert(reporting);
                assert!(modes.get_mouse_report(0, 0, 0).is_some());
                assert!(modes.get_mouse_release_report(0, 0, 0).is_some());
                assert!(modes.is_mouse_enabled());
                assert_eq!(modes.should_report_mouse_motion(false), reporting == 1003);
                assert_eq!(modes.should_report_mouse_motion(true), reporting != 1000);
                modes.remove(&reporting);
            }
        }
    }

    #[test]
    fn legacy_coordinates_and_button_bytes_saturate_before_narrowing() {
        let mut modes = TerminalModes::default();
        modes.insert(1000);
        for button in [0, 1, 2, 3, 64, 223, 224, 255] {
            for coord in [0, 221, 222, 223, 255, 256, 2015, usize::MAX] {
                assert_eq!(
                    modes.get_mouse_report(button, coord, coord),
                    Some(vec![
                        27,
                        91,
                        77,
                        button.saturating_add(32),
                        32 + coord.saturating_add(1).min(223) as u8,
                        32 + coord.saturating_add(1).min(223) as u8
                    ])
                );
                assert_eq!(
                    modes
                        .get_mouse_release_report(button, coord, coord)
                        .unwrap()[3],
                    35
                );
            }
        }
    }

    #[test]
    fn encoding_precedence_release_and_large_coordinates() {
        let mut modes = TerminalModes::default();
        modes.insert(1003);
        modes.insert(1005);
        assert_eq!(
            modes.get_mouse_report(255, usize::MAX, 0),
            Some([b"\x1b[M\xff".as_slice(), "\u{7ff}!".as_bytes()].concat())
        );
        assert_eq!(
            modes.get_mouse_release_report(255, usize::MAX, 0),
            Some("\x1b[M#\u{7ff}!".as_bytes().to_vec())
        );
        modes.insert(1015);
        assert_eq!(
            modes.get_mouse_report(255, usize::MAX, 300),
            Some(format!("\x1b[287;{};301M", usize::MAX).into_bytes())
        );
        assert_eq!(
            modes.get_mouse_release_report(255, usize::MAX, 300),
            Some(format!("\x1b[35;{};301M", usize::MAX).into_bytes())
        );
        modes.insert(1006);
        assert_eq!(
            modes.get_mouse_report(255, usize::MAX, 300),
            Some(format!("\x1b[<255;{};301M", usize::MAX).into_bytes())
        );
        assert_eq!(
            modes.get_mouse_release_report(255, usize::MAX, 300),
            Some(format!("\x1b[<255;{};301m", usize::MAX).into_bytes())
        );
    }
}
