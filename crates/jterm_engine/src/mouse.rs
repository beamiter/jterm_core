//! Mouse reporting encoders extracted from Ember, including legacy limits.
use crate::protocol_state::TerminalModes;

/// Pointer input addressed to a terminal cell, before encoding for the PTY.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MouseInput {
    Press(u8),
    Release(u8),
    Motion(Option<u8>),
    WheelUp,
    WheelDown,
}

/// Enabled pointer tracking policy, independent of its wire encoding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MouseTracking {
    PressOnly,
    Button,
    Drag,
    Any,
}

/// The effective encoding selected by the negotiated mouse modes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MouseEncoding {
    Legacy,
    Utf8,
    Urxvt,
    Sgr,
}

/// Immutable view of the modes that determine pointer reporting.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MouseReportProfile {
    pub tracking: MouseTracking,
    pub encoding: MouseEncoding,
}

fn append_decimal(output: &mut Vec<u8>, mut value: usize) {
    // Every supported usize fits in at most this many decimal digits.
    let mut digits = [0; usize::BITS as usize];
    let mut start = digits.len();
    loop {
        start -= 1;
        digits[start] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    output.extend_from_slice(&digits[start..]);
}

fn decimal_mouse_report(
    prefix: &[u8],
    button: usize,
    col: usize,
    row: usize,
    final_byte: u8,
) -> Vec<u8> {
    // CSI prefix, three decimal fields, two separators, and final byte.
    let coordinate_digits = usize::MAX.ilog10() as usize + 1;
    let mut output = Vec::with_capacity(prefix.len() + 3 + 2 * coordinate_digits + 3);
    output.extend_from_slice(prefix);
    append_decimal(&mut output, button);
    output.push(b';');
    append_decimal(&mut output, col.saturating_add(1));
    output.push(b';');
    append_decimal(&mut output, row.saturating_add(1));
    output.push(final_byte);
    output
}

impl TerminalModes {
    /// Read effective pointer negotiation without encoding a speculative event.
    pub fn mouse_report_profile(&self) -> Option<MouseReportProfile> {
        Some(MouseReportProfile {
            tracking: self.mouse_tracking()?,
            encoding: self.mouse_encoding(),
        })
    }

    fn mouse_tracking(&self) -> Option<MouseTracking> {
        if self.contains(&1003) {
            Some(MouseTracking::Any)
        } else if self.contains(&1002) {
            Some(MouseTracking::Drag)
        } else if self.contains(&1000) {
            Some(MouseTracking::Button)
        } else if self.contains(&9) {
            Some(MouseTracking::PressOnly)
        } else {
            None
        }
    }

    fn mouse_encoding(&self) -> MouseEncoding {
        if self.contains(&1006) {
            MouseEncoding::Sgr
        } else if self.contains(&1015) {
            MouseEncoding::Urxvt
        } else if self.contains(&1005) {
            MouseEncoding::Utf8
        } else {
            MouseEncoding::Legacy
        }
    }

    pub fn get_mouse_report(&self, button: u8, col: usize, row: usize) -> Option<Vec<u8>> {
        let tracking = self.mouse_tracking()?;
        let button = if tracking == MouseTracking::PressOnly {
            let base = button & !0x1c;
            if base > 2 {
                return None;
            }
            base
        } else {
            button
        };
        Some(match self.mouse_encoding() {
            MouseEncoding::Sgr => {
                decimal_mouse_report(b"\x1b[<", usize::from(button), col, row, b'M')
            }
            MouseEncoding::Urxvt => Self::urxvt_mouse_report(button, col, row),
            MouseEncoding::Utf8 => Self::utf8_mouse_report(button, col, row),
            MouseEncoding::Legacy => Self::x10_mouse_report(button, col, row),
        })
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

    /// Mode 1005 keeps the X10 frame but writes button+32 and each coordinate+32
    /// as UTF-8 scalars, lifting the 223-column cap to xterm's 2015.
    fn utf8_mouse_report(button: u8, col: usize, row: usize) -> Vec<u8> {
        let mut output = Vec::with_capacity(12);
        output.extend_from_slice(b"\x1b[M");
        // Mode 1005 encodes Cb as UTF-8 too, not just the coordinates.
        // Extended buttons can exceed ASCII; a raw byte would be invalid UTF-8.
        // Widen before adding the protocol offset so high button codes do not
        // saturate or wrap and silently alias a different event.
        for codepoint in [
            u32::from(button) + 32,
            32 + col.saturating_add(1).min(2015) as u32,
            32 + row.saturating_add(1).min(2015) as u32,
        ] {
            if let Some(ch) = char::from_u32(codepoint) {
                let mut buf = [0u8; 4];
                output.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
            }
        }
        output
    }

    /// Mode 1015 (urxvt): decimal fields, button still offset by 32.
    fn urxvt_mouse_report(button: u8, col: usize, row: usize) -> Vec<u8> {
        decimal_mouse_report(b"\x1b[", usize::from(button) + 32, col, row, b'M')
    }

    /// `button` is the encoded Cb value, including Shift/Meta/Control bits.
    /// Legacy releases discard button/motion/extension bits, retaining modifiers.
    pub fn get_mouse_release_report(&self, button: u8, col: usize, row: usize) -> Option<Vec<u8>> {
        if self.mouse_tracking()? == MouseTracking::PressOnly {
            return None;
        }
        let legacy_release = 3 | (button & 0x1c);
        Some(match self.mouse_encoding() {
            MouseEncoding::Sgr => {
                decimal_mouse_report(b"\x1b[<", usize::from(button), col, row, b'm')
            }
            MouseEncoding::Urxvt => Self::urxvt_mouse_report(legacy_release, col, row),
            MouseEncoding::Utf8 => Self::utf8_mouse_report(legacy_release, col, row),
            MouseEncoding::Legacy => Self::x10_mouse_report(legacy_release, col, row),
        })
    }

    pub fn is_mouse_enabled(&self) -> bool {
        self.mouse_tracking().is_some()
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
    fn press_only_profile_strips_modifiers_and_suppresses_non_press_events() {
        for encoding in [0, 1005, 1015, 1006] {
            let mut modes = TerminalModes::default();
            modes.insert(9);
            modes.insert(encoding);
            assert_eq!(
                modes.mouse_report_profile().unwrap().tracking,
                MouseTracking::PressOnly
            );
            for button in 0..=255 {
                let base = button & !0x1c;
                let report = modes.get_mouse_report(button, 2, 1);
                if base <= 2 {
                    let mut reference = modes.clone();
                    reference.remove(&9);
                    reference.insert(1000);
                    assert_eq!(report, reference.get_mouse_report(base, 2, 1));
                } else {
                    assert!(report.is_none());
                }
                assert!(modes.get_mouse_release_report(button, 2, 1).is_none());
            }
            assert!(!modes.should_report_mouse_motion(false));
            assert!(!modes.should_report_mouse_motion(true));
            for stronger in [1000, 1002, 1003] {
                modes.insert(stronger);
                assert_ne!(
                    modes.mouse_report_profile().unwrap().tracking,
                    MouseTracking::PressOnly
                );
                assert!(modes.get_mouse_release_report(1, 2, 1).is_some());
                assert!(modes.get_mouse_report(64, 2, 1).is_some());
                modes.remove(&stronger);
            }
        }
    }

    #[test]
    fn typed_profile_matches_independent_reporting_and_encoding_negotiation() {
        for reporting in 0..8 {
            for encoding in 0..8 {
                let mut modes = TerminalModes::default();
                for (bit, mode) in [1000, 1002, 1003].into_iter().enumerate() {
                    if reporting & (1 << bit) != 0 {
                        modes.insert(mode);
                    }
                }
                for (bit, mode) in [1005, 1015, 1006].into_iter().enumerate() {
                    if encoding & (1 << bit) != 0 {
                        modes.insert(mode);
                    }
                }
                let actual = modes.mouse_report_profile();
                assert_eq!(actual.is_some(), modes.is_mouse_enabled());
                let Some(profile) = actual else { continue };
                assert_eq!(
                    profile.tracking,
                    if modes.should_report_mouse_motion(false) {
                        MouseTracking::Any
                    } else if modes.should_report_mouse_motion(true) {
                        MouseTracking::Drag
                    } else {
                        MouseTracking::Button
                    }
                );
                let probe = modes.get_mouse_report(96, 0, 0).unwrap();
                let expected = if probe.starts_with(b"\x1b[<") {
                    MouseEncoding::Sgr
                } else if probe.starts_with(b"\x1b[M") {
                    if probe[3] == 0xc2 {
                        MouseEncoding::Utf8
                    } else {
                        MouseEncoding::Legacy
                    }
                } else {
                    MouseEncoding::Urxvt
                };
                assert_eq!(profile.encoding, expected);
            }
        }
    }

    #[test]
    fn decimal_reports_match_wire_formulas_at_all_button_and_coordinate_boundaries() {
        for encoding in [1006, 1015] {
            let mut modes = TerminalModes::default();
            modes.insert(1003);
            modes.insert(encoding);
            for button in u8::MIN..=u8::MAX {
                for col in [0, 8, 9, 98, 99, 998, 999, 9999, usize::MAX - 1, usize::MAX] {
                    for row in [0, 99, 999, usize::MAX] {
                        let x = col.saturating_add(1);
                        let y = row.saturating_add(1);
                        let press = if encoding == 1006 {
                            format!("\x1b[<{button};{x};{y}M")
                        } else {
                            format!("\x1b[{};{x};{y}M", u16::from(button) + 32)
                        };
                        let release = if encoding == 1006 {
                            format!("\x1b[<{button};{x};{y}m")
                        } else {
                            format!("\x1b[{};{x};{y}M", u16::from(3 | (button & 0x1c)) + 32)
                        };
                        assert_eq!(
                            modes.get_mouse_report(button, col, row).unwrap(),
                            press.as_bytes()
                        );
                        assert_eq!(
                            modes.get_mouse_release_report(button, col, row).unwrap(),
                            release.as_bytes()
                        );
                    }
                }
            }
        }
    }

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
                    35 + (button & 0x1c)
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
            Some("\x1b[M\u{11f}\u{7ff}!".as_bytes().to_vec())
        );
        assert_eq!(
            modes.get_mouse_release_report(255, usize::MAX, 0),
            Some("\x1b[M?\u{7ff}!".as_bytes().to_vec())
        );
        modes.insert(1015);
        assert_eq!(
            modes.get_mouse_report(255, usize::MAX, 300),
            Some(format!("\x1b[287;{};301M", usize::MAX).into_bytes())
        );
        assert_eq!(
            modes.get_mouse_release_report(255, usize::MAX, 300),
            Some(format!("\x1b[63;{};301M", usize::MAX).into_bytes())
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
