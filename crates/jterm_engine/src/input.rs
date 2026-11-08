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

pub fn kitty_modifier_value(modifiers: Modifiers) -> u8 {
    1 + u8::from(modifiers.shift)
        + 2 * u8::from(modifiers.alt)
        + 4 * u8::from(modifiers.ctrl)
        + 8 * u8::from(modifiers.super_key)
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
