//! Bounded DEC cursor presentation reports and atomic supported-state restore.
use crate::{
    screen::{Charset, ScreenState},
    UnderlineStyle,
};

/// DECCIR uses absolute page coordinates even when origin mode is enabled.
pub(crate) fn cursor_information_report(screen: &ScreenState) -> Vec<u8> {
    let flags = screen.current_flags;
    let rendition = 0x40
        | u8::from(flags.bold())
        | (u8::from(flags.underline() != UnderlineStyle::None) << 1)
        | (u8::from(flags.blink()) << 2)
        | (u8::from(flags.inverse()) << 3);
    let protection = 0x40 | u8::from(flags.protected());
    let state = 0x40
        | u8::from(screen.origin_mode)
        | (u8::from(screen.pending_wrap) << 3)
        | match screen.single_shift_bank() {
            Some(2) => 2,
            Some(3) => 4,
            _ => 0,
        };
    let designation = |index| {
        if screen.charset_is_latin1(index) || screen.charset_is_british(index) {
            'A'
        } else {
            match screen.charset_bank(index) {
                Charset::Ascii => 'B',
                Charset::DecSpecialGraphics => '0',
            }
        }
    };
    let sizes = (0..4).fold(0x40, |bits, index| {
        bits | (u8::from(screen.charset_is_latin1(index)) << index)
    });
    format!(
        "\x1bP1$u{};{};1;{};{};{};{};{};{};{}{}{}{}\x1b\\",
        screen.cursor_row + 1,
        screen.cursor_col + 1,
        char::from(rendition),
        char::from(protection),
        char::from(state),
        screen.invoked_charset_bank(),
        screen.invoked_gr_charset_bank(),
        char::from(sizes),
        designation(0),
        designation(1),
        designation(2),
        designation(3)
    )
    .into_bytes()
}

struct RestoredCursor {
    row: usize,
    column: usize,
    rendition: u8,
    protection: bool,
    origin: bool,
    pending_wrap: bool,
    banks: [Charset; 4],
    latin1: u8,
    british: u8,
    invoked: u8,
    invoked_gr: u8,
    single_shift: Option<u8>,
}

fn parse_coordinate(bytes: &[u8], limit: usize) -> Option<usize> {
    if bytes.is_empty() {
        return None;
    }
    let mut number = 0usize;
    for &byte in bytes {
        if !byte.is_ascii_digit() {
            return None;
        }
        number = number
            .checked_mul(10)?
            .checked_add(usize::from(byte - b'0'))?;
        if number > limit {
            return None;
        }
    }
    number.checked_sub(1)
}
fn parse_report(screen: &ScreenState, payload: &[u8]) -> Option<RestoredCursor> {
    let mut fields = payload.split(|&byte| byte == b';');
    let row = parse_coordinate(fields.next()?, screen.grid.rows())?;
    let column = parse_coordinate(fields.next()?, screen.grid.cols())?;
    if fields.next()? != b"1" {
        return None;
    }
    let rendition = match fields.next()? {
        [value] if value & 0xf0 == 0x40 => *value,
        _ => return None,
    };
    let protection = match fields.next()? {
        b"@" => false,
        b"A" => true,
        _ => return None,
    };
    let state = match fields.next()? {
        [value] if value & 0xf0 == 0x40 && value & 6 != 6 => *value,
        _ => return None,
    };
    let invoked = match fields.next()? {
        [value @ b'0'..=b'3'] => *value - b'0',
        _ => return None,
    };
    let invoked_gr = match fields.next()? {
        [value @ b'0'..=b'3'] => *value - b'0',
        _ => return None,
    };
    let sizes = match fields.next()? {
        [value] if value & 0xf0 == 0x40 => *value & 0x0f,
        _ => return None,
    };
    let designators = fields.next()?;
    if fields.next().is_some() || designators.len() != 4 {
        return None;
    }
    let mut banks = [Charset::Ascii; 4];
    let mut british = 0;
    for (index, &designator) in designators.iter().enumerate() {
        banks[index] = match (sizes & (1 << index) != 0, designator) {
            (true, b'A') | (false, b'B') => Charset::Ascii,
            (false, b'A') => {
                british |= 1 << index;
                Charset::Ascii
            }
            (false, b'0') => Charset::DecSpecialGraphics,
            _ => return None,
        };
    }
    Some(RestoredCursor {
        row,
        column,
        rendition,
        protection,
        origin: state & 1 != 0,
        pending_wrap: state & 8 != 0,
        banks,
        latin1: sizes,
        british,
        invoked,
        invoked_gr,
        single_shift: if state & 2 != 0 {
            Some(2)
        } else if state & 4 != 0 {
            Some(3)
        } else {
            None
        },
    })
}

/// Restore the DECCIR data body after DCS `1$t`. Unsupported or malformed
/// reports are ignored atomically, leaving all terminal state untouched.
pub(crate) fn restore_cursor_information(screen: &mut ScreenState, payload: &[u8]) -> bool {
    let Some(state) = parse_report(screen, payload) else {
        return false;
    };
    screen.cursor_row = state.row;
    screen.cursor_col = state.column;
    screen.current_flags.set_bold(state.rendition & 1 != 0);
    screen
        .current_flags
        .set_underline(if state.rendition & 2 != 0 {
            UnderlineStyle::Single
        } else {
            UnderlineStyle::None
        });
    screen.current_flags.set_blink(state.rendition & 4 != 0);
    screen.current_flags.set_inverse(state.rendition & 8 != 0);
    screen.current_flags.set_protected(state.protection);
    screen.origin_mode = state.origin;
    screen.pending_wrap = state.pending_wrap;
    screen.reset_charset_banks();
    for (index, charset) in state.banks.into_iter().enumerate() {
        if state.latin1 & (1 << index) != 0 {
            screen.designate_latin1(index as u8);
        } else if state.british & (1 << index) != 0 {
            screen.designate_british(index as u8);
        } else {
            screen.designate_charset_bank(index as u8, charset);
        }
    }
    screen.invoke_charset_bank(state.invoked);
    screen.invoke_gr_charset_bank(state.invoked_gr);
    if let Some(index) = state.single_shift {
        screen.single_shift(index);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{protocol_state::ProtocolState, Color};
    fn body(reply: &[u8]) -> &[u8] {
        reply
            .strip_prefix(b"\x1bP1$u")
            .unwrap()
            .strip_suffix(b"\x1b\\")
            .unwrap()
    }
    #[test]
    fn report_roundtrips_supported_state_with_absolute_origin_coordinates() {
        let mut screen = ScreenState::new(5, 8);
        screen.cursor_row = 2;
        screen.cursor_col = 4;
        screen.scroll_region_top = 1;
        screen.scroll_region_bottom = 3;
        screen.origin_mode = true;
        screen.pending_wrap = true;
        screen.current_flags.set_bold(true);
        screen.current_flags.set_blink(true);
        screen.current_flags.set_inverse(true);
        screen.current_flags.set_underline(UnderlineStyle::Curly);
        screen.current_flags.set_protected(true);
        screen.g1_charset = Charset::DecSpecialGraphics;
        screen.invoke_charset(true);
        screen.grid.begin_capture_revision(9);
        screen.grid.begin_capture_revision(10);
        let report = cursor_information_report(&screen);
        assert_eq!(report, b"\x1bP1$u3;5;1;O;A;I;1;2;@;B0BB\x1b\\");
        assert!(!screen.grid.row_changed_after(0, 9));
        screen.cursor_row = 0;
        screen.cursor_col = 0;
        screen.origin_mode = false;
        screen.pending_wrap = false;
        screen.current_flags.set_bold(false);
        screen.current_flags.set_protected(false);
        screen.g1_charset = Charset::Ascii;
        screen.invoke_charset(false);
        assert!(restore_cursor_information(&mut screen, body(&report)));
        assert_eq!(
            (
                screen.cursor_row,
                screen.cursor_col,
                screen.origin_mode,
                screen.pending_wrap
            ),
            (2, 4, true, true)
        );
        assert_eq!(screen.current_flags.underline(), UnderlineStyle::Single);
        assert!(
            screen.current_flags.bold()
                && screen.current_flags.blink()
                && screen.current_flags.inverse()
                && screen.current_flags.protected()
        );
        assert!(screen.invoked_g1());
        assert_eq!(screen.translate_char('q'), '─');
        assert_eq!(cursor_information_report(&screen), report);
    }
    #[test]
    fn restoring_preserves_unrepresented_pen_modes_and_saved_cursor() {
        let mut screen = ScreenState::new(3, 4);
        screen.current_fg = Color::Rgb(2, 3, 4);
        screen.current_bg = Color::Blue;
        screen.current_flags.set_dim(true);
        screen.current_flags.set_italic(true);
        screen.current_flags.set_strikethrough(true);
        screen.current_flags.set_overline(true);
        screen.current_flags.set_concealed(true);
        screen.insert_mode = true;
        screen.current_hyperlink = crate::HyperlinkId::from_raw(7);
        screen.save_cursor_state(&ProtocolState::default());
        let saved = format!("{:?}", screen.saved_state);
        let tabs = screen.tab_stops.clone();
        assert!(restore_cursor_information(
            &mut screen,
            b"2;3;1;@;@;H;1;2;@;BBBB"
        ));
        assert!(screen.invoked_g1()); // Preserve invocation identity even if both sets match.
        assert_eq!(
            cursor_information_report(&screen),
            b"\x1bP1$u2;3;1;@;@;H;1;2;@;BBBB\x1b\\"
        );
        assert_eq!(
            (screen.current_fg, screen.current_bg),
            (Color::Rgb(2, 3, 4), Color::Blue)
        );
        assert!(
            screen.current_flags.dim()
                && screen.current_flags.italic()
                && screen.current_flags.strikethrough()
                && screen.current_flags.overline()
                && screen.current_flags.concealed()
        );
        assert!(screen.insert_mode);
        assert_eq!(screen.current_hyperlink, crate::HyperlinkId::from_raw(7));
        assert_eq!(screen.tab_stops, tabs);
        assert_eq!(format!("{:?}", screen.saved_state), saved);
    }
    #[test]
    fn invalid_and_unsupported_fields_never_partially_restore() {
        let mut screen = ScreenState::new(3, 4);
        screen.cursor_row = 2;
        screen.cursor_col = 3;
        screen.pending_wrap = true;
        screen.current_flags.set_bold(true);
        screen.current_flags.set_protected(true);
        let before = cursor_information_report(&screen);
        let valid = b"1;1;1;@;@;@;0;2;@;BBBB";
        for length in 0..valid.len() {
            assert!(!restore_cursor_information(&mut screen, &valid[..length]));
            assert_eq!(cursor_information_report(&screen), before);
        }
        for invalid in [
            "0;1;1;@;@;@;0;2;@;BBBB",
            "4;1;1;@;@;@;0;2;@;BBBB",
            "1;5;1;@;@;@;0;2;@;BBBB",
            "1;1;2;@;@;@;0;2;@;BBBB",
            "1;1;1;P;@;@;0;2;@;BBBB",
            "1;1;1;@;B;@;0;2;@;BBBB",
            "1;1;1;@;@;F;0;2;@;BBBB",
            "1;1;1;@;@;@;4;2;@;BBBB",
            "1;1;1;@;@;@;0;4;@;BBBB",
            "1;1;1;@;@;@;0;2;L;BBBB",
            "1;1;1;@;@;@;0;2;@;BBZZ",
            "1;1;1;@;@;@;0;2;@;B%5BB",
            "1;1;1;@;@;@;0;2;@;BBBB;",
            "99999999999999999999999;1;1;@;@;@;0;2;@;BBBB",
        ] {
            assert!(
                !restore_cursor_information(&mut screen, invalid.as_bytes()),
                "{invalid}"
            );
            assert_eq!(cursor_information_report(&screen), before, "{invalid}");
        }
    }
    #[test]
    fn four_banks_locking_and_single_shift_report_roundtrip() {
        let mut screen = ScreenState::new(3, 6);
        screen.designate_charset_bank(0, Charset::DecSpecialGraphics);
        screen.designate_charset_bank(1, Charset::Ascii);
        screen.designate_charset_bank(2, Charset::DecSpecialGraphics);
        screen.designate_charset_bank(3, Charset::DecSpecialGraphics);
        for invoked in 0..4 {
            for shift in [2, 3] {
                screen.invoke_charset_bank(invoked);
                screen.single_shift(shift);
                let report = cursor_information_report(&screen);
                let mut restored = ScreenState::new(3, 6);
                assert!(restore_cursor_information(&mut restored, body(&report)));
                assert_eq!(restored.invoked_charset_bank(), invoked);
                assert_eq!(restored.single_shift_bank(), Some(shift));
                for index in 0..4 {
                    assert_eq!(restored.charset_bank(index), screen.charset_bank(index));
                }
                assert_eq!(cursor_information_report(&restored), report);
                assert_eq!(restored.take_graphic_charset('q').translate('q'), '─');
                assert!(!restored.single_shift_pending());
                assert_eq!(restored.invoked_charset_bank(), invoked);
            }
        }
        let before = cursor_information_report(&screen);
        assert!(!restore_cursor_information(
            &mut screen,
            b"1;1;1;@;@;F;3;2;@;B0B0"
        ));
        assert_eq!(cursor_information_report(&screen), before);
    }
    #[test]
    fn configurable_gr_roundtrips_all_banks_and_rejects_out_of_range_atomically() {
        let mut screen = ScreenState::new(3, 6);
        screen.designate_charset_bank(0, Charset::DecSpecialGraphics);
        screen.designate_charset_bank(3, Charset::DecSpecialGraphics);
        screen.invoke_charset_bank(3);
        screen.single_shift(2);
        for gr in 0..4 {
            screen.invoke_gr_charset_bank(gr);
            let report = cursor_information_report(&screen);
            assert_eq!(
                body(&report).split(|&byte| byte == b';').nth(7),
                Some([b'0' + gr].as_slice())
            );
            let mut restored = ScreenState::new(3, 6);
            assert!(restore_cursor_information(&mut restored, body(&report)));
            assert_eq!(restored.invoked_gr_charset_bank(), gr);
            assert_eq!(restored.invoked_charset_bank(), 3);
            assert_eq!(restored.single_shift_bank(), Some(2));
            assert_eq!(cursor_information_report(&restored), report);
        }
        let before = cursor_information_report(&screen);
        assert!(!restore_cursor_information(
            &mut screen,
            b"1;1;1;@;@;@;0;4;@;BBBB"
        ));
        assert_eq!(cursor_information_report(&screen), before);
        assert!(restore_cursor_information(
            &mut screen,
            b"1;1;1;@;@;@;0;0;@;0BBB"
        ));
        assert_eq!(screen.invoked_gr_charset_bank(), 0);
        assert_eq!(screen.translate_fill_character(0xf1), '─');
    }
    #[test]
    fn latin1_sizes_and_designators_roundtrip_every_consistent_mask() {
        for mask in 0u8..16 {
            let mut screen = ScreenState::new(2, 6);
            for index in 0..4 {
                if mask & (1 << index) != 0 {
                    screen.designate_latin1(index);
                } else {
                    screen.designate_charset_bank(index, Charset::DecSpecialGraphics);
                }
            }
            screen.invoke_charset_bank(3);
            screen.invoke_gr_charset_bank(1);
            screen.single_shift(2);
            let report = cursor_information_report(&screen);
            let fields: Vec<_> = body(&report).split(|&b| b == b';').collect();
            assert_eq!(fields[8], [0x40 | mask]);
            for (index, &designator) in fields[9].iter().enumerate() {
                assert_eq!(
                    designator,
                    if mask & (1 << index) != 0 { b'A' } else { b'0' }
                );
            }
            let mut restored = ScreenState::new(2, 6);
            assert!(restore_cursor_information(&mut restored, body(&report)));
            assert_eq!(cursor_information_report(&restored), report);
            for index in 0..4 {
                assert_eq!(restored.charset_is_latin1(index), mask & (1 << index) != 0);
            }
        }
    }
    #[test]
    fn inconsistent_latin1_size_and_designator_is_atomic_noop() {
        let mut screen = ScreenState::new(2, 6);
        screen.designate_latin1(2);
        let before = cursor_information_report(&screen);
        for invalid in [
            b"1;1;1;@;@;@;0;2;@;BBZB".as_slice(),
            b"1;1;1;@;@;@;0;2;D;BBBB",
            b"1;1;1;@;@;@;0;2;D;BB0B",
            b"1;1;1;@;@;@;0;2;P;AAAA",
            b"1;1;1;@;@;@;0;2;O;AAAZ",
        ] {
            assert!(!restore_cursor_information(&mut screen, invalid));
            assert_eq!(cursor_information_report(&screen), before);
        }
        assert!(restore_cursor_information(
            &mut screen,
            b"1;1;1;@;@;@;0;2;A;ABBB"
        ));
        assert!(screen.charset_is_latin1(0)); // DECCIR defines G0 size even without an ESC designation route.
    }
}
