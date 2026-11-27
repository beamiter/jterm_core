//! Platform-neutral keyboard wire encoding. Hosts own shortcut routing, IME,
//! text/key pairing, viewport gestures and protocol capability negotiation.
use std::borrow::Cow;

/// A logical key, independent of GUI key enums and physical keyboard layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    ArrowUp,
    ArrowDown,
    ArrowRight,
    ArrowLeft,
    Home,
    End,
    Insert,
    Delete,
    PageUp,
    PageDown,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
    Enter,
    Escape,
    Backspace,
    Tab,
    Character(char),
    Unidentified,
}

/// Wire modifiers. Hosts resolve platform command aliases before encoding.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
    pub super_key: bool,
}

/// The negotiated flag for disambiguating key presses.
pub const KITTY_DISAMBIGUATE: u8 = 1;

/// Toolkit-independent keys in the disambiguation profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KittyKey {
    Unicode(char),
    /// Keep the toolkit's text for matching the corresponding legacy commit.
    UnicodeWithText {
        base: char,
        text: char,
    },
    Escape,
    Enter,
    Tab,
    Backspace,
    Space,
    Functional,
}

/// Preserve the base key and the toolkit's text when the layout changes it.
pub fn unicode_key(base: char, text: Option<char>) -> KittyKey {
    if let Some(text) = text.filter(|ch| !ch.is_control()) {
        let mut lower = text.to_lowercase();
        if (lower.next(), lower.next()) != (Some(base), None) {
            return KittyKey::UnicodeWithText { base, text };
        }
    }
    KittyKey::Unicode(base)
}

pub fn kitty_modifier_value(modifiers: Modifiers) -> u8 {
    1 + u8::from(modifiers.shift)
        + 2 * u8::from(modifiers.alt)
        + 4 * u8::from(modifiers.ctrl)
        + 8 * u8::from(modifiers.super_key)
}

/// Encode keys that the disambiguation flag changes. A None result indicates
/// that the terminal should use its regular key or committed-text encoding.
pub fn encode_kitty_disambiguated_key(
    key: KittyKey,
    modifiers: Modifiers,
    flags: u8,
) -> Option<Vec<u8>> {
    if flags & KITTY_DISAMBIGUATE == 0 {
        return None;
    }
    let non_shift = modifiers.alt || modifiers.ctrl || modifiers.super_key;
    let modified = modifiers.shift || non_shift;
    let code = match key {
        KittyKey::Escape => 27,
        KittyKey::Enter if modified => 13,
        KittyKey::Tab if modified => 9,
        KittyKey::Backspace if modified => 127,
        KittyKey::Space if non_shift => 32,
        KittyKey::Unicode(ch) if non_shift => u32::from(ch),
        KittyKey::UnicodeWithText { base, .. } if non_shift => u32::from(base),
        _ => return None,
    };
    let parameter = kitty_modifier_value(modifiers);
    // Maximum wire length: CSI, seven Unicode decimal digits, semicolon,
    // two modifier digits and the final byte.
    let mut output = Vec::with_capacity(13);
    output.extend_from_slice(b"\x1b[");
    append_key_decimal(&mut output, code);
    if parameter != 1 {
        output.push(b';');
        append_key_decimal(&mut output, u32::from(parameter));
    }
    output.push(b'u');
    Some(output)
}

fn append_key_decimal(output: &mut Vec<u8>, mut value: u32) {
    let mut digits = [0; 10];
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

// Preserve Ember's existing logical-key repertoire. Arbitrary OS-produced
// Unicode is supplied explicitly to modifyOtherKeys via text_character.
fn kitty_text_key_code(key: Key) -> Option<u32> {
    match key {
        Key::Character(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => Some(c as u32),
        _ => None,
    }
}

fn text_key_code(key: Key, modifiers: Modifiers) -> Option<u32> {
    let code = kitty_text_key_code(key)?;
    Some(if modifiers.shift {
        (code as u8).to_ascii_uppercase() as u32
    } else {
        code
    })
}

/// Encode cursor, editing and F1–F12 keys using the xterm/terminfo family.
/// Hosts must withhold keys reserved for viewport navigation before calling.
pub fn legacy_function_key_sequence(
    key: Key,
    modifiers: Modifiers,
    application_cursor_keys: bool,
) -> Option<Cow<'static, str>> {
    let modifier = kitty_modifier_value(modifiers);
    // 1 is the "no modifier" parameter value; xterm omits it entirely.
    let modified = modifier > 1;

    // Cursor keys and Home/End follow DECCKM only while unmodified: the
    // parameterized form is always CSI, never SS3.
    let cursor =
        |normal: &'static str, application: &'static str, final_byte: char| -> Cow<'static, str> {
            if modified {
                Cow::Owned(format!("\x1b[1;{modifier}{final_byte}"))
            } else if application_cursor_keys {
                Cow::Borrowed(application)
            } else {
                Cow::Borrowed(normal)
            }
        };
    let tilde = |plain: &'static str, code: u8| -> Cow<'static, str> {
        if modified {
            Cow::Owned(format!("\x1b[{code};{modifier}~"))
        } else {
            Cow::Borrowed(plain)
        }
    };
    // F1-F4 are SS3 unmodified but join the CSI 1;<mod> form once modified;
    // they are not affected by DECCKM.
    let function = |plain: &'static str, final_byte: char| -> Cow<'static, str> {
        if modified {
            Cow::Owned(format!("\x1b[1;{modifier}{final_byte}"))
        } else {
            Cow::Borrowed(plain)
        }
    };

    Some(match key {
        Key::ArrowUp => cursor("\x1b[A", "\x1bOA", 'A'),
        Key::ArrowDown => cursor("\x1b[B", "\x1bOB", 'B'),
        Key::ArrowRight => cursor("\x1b[C", "\x1bOC", 'C'),
        Key::ArrowLeft => cursor("\x1b[D", "\x1bOD", 'D'),
        Key::Home => cursor("\x1b[H", "\x1bOH", 'H'),
        Key::End => cursor("\x1b[F", "\x1bOF", 'F'),
        Key::Insert => tilde("\x1b[2~", 2),
        Key::Delete => tilde("\x1b[3~", 3),
        Key::PageUp => tilde("\x1b[5~", 5),
        Key::PageDown => tilde("\x1b[6~", 6),
        Key::F1 => function("\x1bOP", 'P'),
        Key::F2 => function("\x1bOQ", 'Q'),
        Key::F3 => function("\x1bOR", 'R'),
        Key::F4 => function("\x1bOS", 'S'),
        Key::F5 => tilde("\x1b[15~", 15),
        Key::F6 => tilde("\x1b[17~", 17),
        Key::F7 => tilde("\x1b[18~", 18),
        Key::F8 => tilde("\x1b[19~", 19),
        Key::F9 => tilde("\x1b[20~", 20),
        Key::F10 => tilde("\x1b[21~", 21),
        Key::F11 => tilde("\x1b[23~", 23),
        Key::F12 => tilde("\x1b[24~", 24),
        _ => return None,
    })
}

/// Encode the printable-key subset used by Ember's Kitty input path.
/// This is not a capability negotiator: hosts retain their supported flag masks
/// and stack limits. Event releases, IME commits and key/text deduplication stay
/// with the host's event stream, rather than being inferred here.
pub fn kitty_encode_key_event(
    key: Key,
    modifiers: Modifiers,
    keyboard_flags: u16,
) -> Option<String> {
    let disambiguate = (keyboard_flags & 0b1) != 0;
    let report_all_keys = (keyboard_flags & 0b1000) != 0;
    if !disambiguate && !report_all_keys {
        return None;
    }

    let codepoint = kitty_text_key_code(key)?;
    let should_encode = report_all_keys || modifiers.ctrl || modifiers.alt || modifiers.super_key;
    if !should_encode {
        return None;
    }

    Some(format!(
        "\x1b[{};{}u",
        codepoint,
        kitty_modifier_value(modifiers)
    ))
}

/// Encode a printable key under modifyOtherKeys / formatOtherKeys.
/// `text_character` is the host-validated, layout-correct OS character when
/// available. It takes priority over the legacy ASCII key fallback.
pub fn xterm_encode_modify_other_keys(
    key: Key,
    modifiers: Modifiers,
    modify_other_keys: u16,
    format_other_keys: u16,
    report_all_keys: bool,
    text_character: Option<char>,
) -> Option<String> {
    let codepoint = text_character
        .map(u32::from)
        .or_else(|| text_key_code(key, modifiers))?;
    let modifier_value = kitty_modifier_value(modifiers);
    let has_non_shift_modifier = modifiers.ctrl || modifiers.alt || modifiers.super_key;
    let should_encode = if report_all_keys {
        modifier_value > 1
    } else {
        match modify_other_keys {
            0 => false,
            1 => modifiers.alt || modifiers.super_key,
            _ => has_non_shift_modifier || modifiers.shift,
        }
    };

    if !should_encode {
        return None;
    }

    if format_other_keys == 1 || report_all_keys {
        Some(format!("\x1b[{};{}u", codepoint, modifier_value))
    } else {
        Some(format!("\x1b[27;{};{}~", modifier_value, codepoint))
    }
}

#[cfg(test)]
mod decimal_key_tests {
    use super::*;

    #[test]
    fn disambiguated_wire_matches_formatted_reference() {
        let keys = [
            KittyKey::Escape,
            KittyKey::Enter,
            KittyKey::Tab,
            KittyKey::Backspace,
            KittyKey::Space,
            KittyKey::Functional,
        ];
        let unicode = [
            '\0',
            '9',
            'a',
            '\u{7f}',
            '\u{ff}',
            '\u{3e8}',
            '界',
            '\u{ffff}',
            '\u{10000}',
            '\u{10ffff}',
        ];
        for bits in 0..16 {
            let mods = Modifiers {
                shift: bits & 1 != 0,
                alt: bits & 2 != 0,
                ctrl: bits & 4 != 0,
                super_key: bits & 8 != 0,
            };
            for flags in [0, 1, 2, 3, 255] {
                for key in keys.into_iter().chain(unicode.into_iter().flat_map(|ch| {
                    [
                        KittyKey::Unicode(ch),
                        KittyKey::UnicodeWithText {
                            base: ch,
                            text: 'Z',
                        },
                    ]
                })) {
                    let non_shift = mods.alt || mods.ctrl || mods.super_key;
                    let modified = mods.shift || non_shift;
                    let code = match key {
                        KittyKey::Escape => Some(27),
                        KittyKey::Enter if modified => Some(13),
                        KittyKey::Tab if modified => Some(9),
                        KittyKey::Backspace if modified => Some(127),
                        KittyKey::Space if non_shift => Some(32),
                        KittyKey::Unicode(ch) if non_shift => Some(u32::from(ch)),
                        KittyKey::UnicodeWithText { base, .. } if non_shift => {
                            Some(u32::from(base))
                        }
                        _ => None,
                    };
                    let expected = code.filter(|_| flags & 1 != 0).map(|code| {
                        let parameter = kitty_modifier_value(mods);
                        if parameter == 1 {
                            format!("\x1b[{code}u").into_bytes()
                        } else {
                            format!("\x1b[{code};{parameter}u").into_bytes()
                        }
                    });
                    assert_eq!(encode_kitty_disambiguated_key(key, mods, flags), expected);
                }
            }
        }
    }
}
