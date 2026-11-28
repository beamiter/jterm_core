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

/// Physical keys on a numeric keypad, independent of its current layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeypadKey {
    Digit(u8),
    Decimal,
    Divide,
    Multiply,
    Subtract,
    Add,
    Enter,
    Equal,
    Separator,
    Space,
    Tab,
}

impl KeypadKey {
    /// VT220 application-keypad encoding. The input adapter supplies
    /// the physical key after resolving keyboard and shortcut ownership.
    pub fn application_sequence(self) -> Option<[u8; 3]> {
        let final_byte = match self {
            Self::Digit(digit @ 0..=9) => b'p' + digit,
            Self::Digit(_) => return None,
            Self::Decimal => b'n',
            Self::Divide => b'o',
            Self::Multiply => b'j',
            Self::Subtract => b'm',
            Self::Add => b'k',
            Self::Enter => b'M',
            Self::Equal => b'X',
            Self::Separator => b'l',
            Self::Space => b' ',
            Self::Tab => b'I',
        };
        Some([0x1b, b'O', final_byte])
    }
}

/// PC keypad center in numeric mode when the OS reports Begin or Clear.
pub fn keypad_begin_sequence(modifiers: Modifiers) -> Cow<'static, str> {
    let parameter = kitty_modifier_value(modifiers);
    if parameter == 1 {
        Cow::Borrowed("\x1b[E")
    } else {
        Cow::Owned(format!("\x1b[1;{parameter}E"))
    }
}

/// The negotiated flag for disambiguating key presses.
pub const KITTY_DISAMBIGUATE: u8 = 1;
pub const KITTY_REPORT_ALL_KEYS: u16 = 8;
pub const KITTY_REPORT_ASSOCIATED_TEXT: u16 = 16;

/// Layout values associated with a printable physical press.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KittyTextKey {
    pub base: char,
    pub shifted: Option<char>,
    pub base_layout: Option<char>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KeyboardLocks {
    pub caps_lock: bool,
    pub num_lock: bool,
}

/// Report a printable press in all-key mode. If the platform delivered only
/// text, key zero carries the optional associated text without key modifiers.
pub fn encode_kitty_text_press(
    key: Option<KittyTextKey>,
    modifiers: Modifiers,
    locks: KeyboardLocks,
    flags: u16,
    text: Option<&str>,
) -> Option<Vec<u8>> {
    if flags & KITTY_REPORT_ALL_KEYS == 0 {
        return None;
    }
    let associated = text.filter(|_| flags & KITTY_REPORT_ASSOCIATED_TEXT != 0);
    let Some(key) = key else {
        return text
            .filter(|value| value.chars().any(|ch| !ch.is_control()))
            .map(|_| serialize_kitty_press(0, 1, b'u', (None, None), associated));
    };
    if key.base.is_control() {
        return None;
    }
    let alternates = if flags & 4 != 0 {
        (
            key.shifted
                .filter(|ch| modifiers.shift && *ch != key.base && !ch.is_control()),
            key.base_layout
                .filter(|ch| *ch != key.base && !ch.is_control()),
        )
    } else {
        (None, None)
    };
    let parameter = u32::from(kitty_modifier_value(modifiers))
        + 64 * u32::from(locks.caps_lock)
        + 128 * u32::from(locks.num_lock);
    Some(serialize_kitty_press(
        u32::from(key.base),
        parameter,
        b'u',
        alternates,
        associated,
    ))
}

#[cfg(test)]
#[test]
fn kitty_printable_press_negotiates_text_alternates_and_locks() {
    let plain = Modifiers::default();
    let unlocked = KeyboardLocks::default();
    let a = Some(KittyTextKey {
        base: 'a',
        shifted: Some('A'),
        base_layout: Some('a'),
    });
    assert_eq!(
        encode_kitty_text_press(a, plain, unlocked, 1, Some("a")),
        None
    );
    assert_eq!(
        encode_kitty_text_press(a, plain, unlocked, 8, Some("a")).unwrap(),
        b"\x1b[97u"
    );
    assert_eq!(
        encode_kitty_text_press(a, plain, unlocked, 24, Some("a")).unwrap(),
        b"\x1b[97;;97u"
    );
    assert_eq!(
        encode_kitty_text_press(
            a,
            Modifiers {
                shift: true,
                ..plain
            },
            KeyboardLocks {
                caps_lock: true,
                num_lock: true
            },
            28,
            Some("A\r\u{0085}"),
        )
        .unwrap(),
        b"\x1b[97:65;194;65u"
    );
    let cyrillic = Some(KittyTextKey {
        base: 'ф',
        shifted: None,
        base_layout: Some('a'),
    });
    assert_eq!(
        encode_kitty_text_press(cyrillic, plain, unlocked, 28, Some("ф")).unwrap(),
        b"\x1b[1092::97;;1092u"
    );
    assert_eq!(
        encode_kitty_text_press(None, plain, unlocked, 24, Some("e\u{301}")).unwrap(),
        b"\x1b[0;;101:769u"
    );
    assert_eq!(
        encode_kitty_text_press(None, plain, unlocked, 8, Some("é")).unwrap(),
        b"\x1b[0u"
    );
    assert_eq!(
        encode_kitty_text_press(None, plain, unlocked, 24, Some("\n")),
        None
    );
}

/// Encode the selected keypad key when the application requested Kitty
/// disambiguation or reporting of all keys. A plain printable keypad press
/// can still take the text path under disambiguation alone.
pub fn encode_kitty_keypad_press(
    position: KeypadKey,
    logical: Key,
    modifiers: Modifiers,
    num_locked: bool,
    flags: u16,
    text: Option<&str>,
) -> Option<Vec<u8>> {
    let report_all = flags & KITTY_REPORT_ALL_KEYS != 0;
    if !report_all && flags & u16::from(KITTY_DISAMBIGUATE) == 0 {
        return None;
    }
    if !report_all
        && matches!(logical, Key::Character(ch) if !ch.is_control())
        && !(modifiers.ctrl || modifiers.alt || modifiers.super_key)
    {
        return None;
    }

    let physical_code = match position {
        KeypadKey::Digit(digit @ 0..=9) => 57399 + u32::from(digit),
        KeypadKey::Decimal => 57409,
        KeypadKey::Divide => 57410,
        KeypadKey::Multiply => 57411,
        KeypadKey::Subtract => 57412,
        KeypadKey::Add => 57413,
        KeypadKey::Enter => 57414,
        KeypadKey::Equal => 57415,
        KeypadKey::Separator => 57416,
        KeypadKey::Digit(_) | KeypadKey::Space | KeypadKey::Tab => return None,
    };
    let (code, final_byte) = match logical {
        Key::ArrowLeft => (57417, b'u'),
        Key::ArrowRight => (57418, b'u'),
        Key::ArrowUp => (57419, b'u'),
        Key::ArrowDown => (57420, b'u'),
        Key::PageUp => (57421, b'u'),
        Key::PageDown => (57422, b'u'),
        Key::Home => (57423, b'u'),
        Key::End => (57424, b'u'),
        Key::Insert => (57425, b'u'),
        Key::Delete => (57426, b'u'),
        Key::Unidentified if position == KeypadKey::Digit(5) => (1, b'E'),
        _ => (physical_code, b'u'),
    };
    let parameter = u32::from(kitty_modifier_value(modifiers)) + 128 * u32::from(num_locked);
    let associated = text.filter(|_| report_all && flags & KITTY_REPORT_ASSOCIATED_TEXT != 0);
    Some(serialize_kitty_press(
        code,
        parameter,
        final_byte,
        (None, None),
        associated,
    ))
}

fn serialize_kitty_press(
    code: u32,
    parameter: u32,
    final_byte: u8,
    (shifted, base_layout): (Option<char>, Option<char>),
    text: Option<&str>,
) -> Vec<u8> {
    let mut characters = text
        .into_iter()
        .flat_map(str::chars)
        .filter(|ch| !ch.is_control())
        .peekable();
    let has_text = characters.peek().is_some();
    let has_alternates = shifted.is_some() || base_layout.is_some();
    let mut output = Vec::with_capacity(13 + 16 * usize::from(has_alternates));
    output.extend_from_slice(b"\x1b[");
    if code != 1 || final_byte == b'u' || parameter != 1 || has_text || has_alternates {
        append_key_decimal(&mut output, code);
    }
    if has_alternates {
        output.push(b':');
        if let Some(shifted) = shifted {
            append_key_decimal(&mut output, u32::from(shifted));
        }
        if let Some(base_layout) = base_layout {
            output.push(b':');
            append_key_decimal(&mut output, u32::from(base_layout));
        }
    }
    if parameter != 1 || has_text {
        output.push(b';');
        if parameter != 1 {
            append_key_decimal(&mut output, parameter);
        }
    }
    if let Some(first) = characters.next() {
        output.push(b';');
        append_key_decimal(&mut output, u32::from(first));
        for character in characters {
            output.push(b':');
            append_key_decimal(&mut output, u32::from(character));
        }
    }
    output.push(final_byte);
    output
}

#[cfg(test)]
#[test]
fn kitty_keypad_reports_navigation_and_keeps_plain_text_in_disambiguation_mode() {
    use KeypadKey as Pad;

    let modifiers = Modifiers::default();
    for (position, logical, expected) in [
        (Pad::Digit(4), Key::ArrowLeft, b"\x1b[57417u".as_slice()),
        (Pad::Digit(6), Key::ArrowRight, b"\x1b[57418u".as_slice()),
        (Pad::Digit(8), Key::ArrowUp, b"\x1b[57419u".as_slice()),
        (Pad::Digit(2), Key::ArrowDown, b"\x1b[57420u".as_slice()),
        (Pad::Digit(9), Key::PageUp, b"\x1b[57421u".as_slice()),
        (Pad::Digit(3), Key::PageDown, b"\x1b[57422u".as_slice()),
        (Pad::Digit(7), Key::Home, b"\x1b[57423u".as_slice()),
        (Pad::Digit(1), Key::End, b"\x1b[57424u".as_slice()),
        (Pad::Digit(0), Key::Insert, b"\x1b[57425u".as_slice()),
        (Pad::Decimal, Key::Delete, b"\x1b[57426u".as_slice()),
        (Pad::Digit(5), Key::Unidentified, b"\x1b[E".as_slice()),
    ] {
        assert_eq!(
            encode_kitty_keypad_press(position, logical, modifiers, false, 1, None).as_deref(),
            Some(expected),
        );
    }
    assert_eq!(
        encode_kitty_keypad_press(
            Pad::Digit(7),
            Key::Character('7'),
            modifiers,
            true,
            1,
            Some("7")
        ),
        None,
    );
    assert_eq!(
        encode_kitty_keypad_press(Pad::Enter, Key::Enter, modifiers, false, 4, None),
        None,
    );
    assert_eq!(
        encode_kitty_keypad_press(
            Pad::Add,
            Key::Character('+'),
            Modifiers {
                ctrl: true,
                ..modifiers
            },
            false,
            1,
            None,
        )
        .unwrap(),
        b"\x1b[57413;5u",
    );
}

#[cfg(test)]
#[test]
fn kitty_keypad_report_all_keeps_locks_and_associated_text() {
    let modifiers = Modifiers::default();
    assert_eq!(
        encode_kitty_keypad_press(
            KeypadKey::Digit(7),
            Key::Character('7'),
            modifiers,
            true,
            8,
            Some("7"),
        )
        .unwrap(),
        b"\x1b[57406;129u",
    );
    assert_eq!(
        encode_kitty_keypad_press(
            KeypadKey::Decimal,
            Key::Character(','),
            modifiers,
            true,
            24,
            Some("\r,\u{85}"),
        )
        .unwrap(),
        b"\x1b[57409;129;44u",
    );
    assert_eq!(
        encode_kitty_keypad_press(
            KeypadKey::Multiply,
            Key::Character('*'),
            modifiers,
            false,
            24,
            Some("*"),
        )
        .unwrap(),
        b"\x1b[57411;;42u",
    );
    assert_eq!(
        encode_kitty_keypad_press(
            KeypadKey::Enter,
            Key::Enter,
            modifiers,
            false,
            24,
            Some("\r"),
        )
        .unwrap(),
        b"\x1b[57414u",
    );
}

/// DECBKM selects the legacy backarrow byte. Xterm's Control modifier toggles
/// the selection for hosts that forward Control+Backspace to this encoder.
pub fn legacy_backspace_sequence(backarrow_mode: bool, ctrl: bool) -> &'static str {
    if backarrow_mode ^ ctrl {
        "\x08"
    } else {
        "\x7f"
    }
}

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

/// Encode navigation, editing, F1–F12 and named control keys when Kitty
/// disambiguation or all-key reporting has been negotiated.
pub fn encode_kitty_named_press(key: Key, modifiers: Modifiers, flags: u16) -> Option<Vec<u8>> {
    let report_all = flags & 8 != 0;
    if !report_all && flags & u16::from(KITTY_DISAMBIGUATE) == 0 {
        return None;
    }
    let parameter = u32::from(kitty_modifier_value(modifiers));
    let (code, final_byte) = match key {
        Key::Escape => (27, b'u'),
        Key::Enter if report_all || parameter != 1 => (13, b'u'),
        Key::Tab if report_all || parameter != 1 => (9, b'u'),
        Key::Backspace if report_all || parameter != 1 => (127, b'u'),
        Key::ArrowUp => (1, b'A'),
        Key::ArrowDown => (1, b'B'),
        Key::ArrowRight => (1, b'C'),
        Key::ArrowLeft => (1, b'D'),
        Key::Home => (1, b'H'),
        Key::End => (1, b'F'),
        Key::Insert => (2, b'~'),
        Key::Delete => (3, b'~'),
        Key::PageUp => (5, b'~'),
        Key::PageDown => (6, b'~'),
        Key::F1 => (1, b'P'),
        Key::F2 => (1, b'Q'),
        // The old CSI R form overlaps the cursor position reply.
        Key::F3 => (13, b'~'),
        Key::F4 => (1, b'S'),
        Key::F5 => (15, b'~'),
        Key::F6 => (17, b'~'),
        Key::F7 => (18, b'~'),
        Key::F8 => (19, b'~'),
        Key::F9 => (20, b'~'),
        Key::F10 => (21, b'~'),
        Key::F11 => (23, b'~'),
        Key::F12 => (24, b'~'),
        _ => return None,
    };
    Some(serialize_kitty_press(
        code,
        parameter,
        final_byte,
        (None, None),
        None,
    ))
}

#[cfg(test)]
#[test]
fn kitty_named_press_uses_unambiguous_function_keys_and_negotiated_controls() {
    let plain = Modifiers::default();
    let ctrl = Modifiers {
        ctrl: true,
        ..plain
    };
    for (key, expected) in [
        (Key::F1, b"\x1b[P".as_slice()),
        (Key::F2, b"\x1b[Q".as_slice()),
        (Key::F3, b"\x1b[13~".as_slice()),
        (Key::F4, b"\x1b[S".as_slice()),
        (Key::F5, b"\x1b[15~".as_slice()),
        (Key::F6, b"\x1b[17~".as_slice()),
        (Key::F7, b"\x1b[18~".as_slice()),
        (Key::F8, b"\x1b[19~".as_slice()),
        (Key::F9, b"\x1b[20~".as_slice()),
        (Key::F10, b"\x1b[21~".as_slice()),
        (Key::F11, b"\x1b[23~".as_slice()),
        (Key::F12, b"\x1b[24~".as_slice()),
        (Key::ArrowUp, b"\x1b[A".as_slice()),
        (Key::Home, b"\x1b[H".as_slice()),
        (Key::Insert, b"\x1b[2~".as_slice()),
    ] {
        assert_eq!(encode_kitty_named_press(key, plain, 0), None);
        assert_eq!(encode_kitty_named_press(key, plain, 4), None);
        for flags in [1, 8, 9] {
            assert_eq!(
                encode_kitty_named_press(key, plain, flags).as_deref(),
                Some(expected)
            );
        }
    }
    assert_eq!(
        encode_kitty_named_press(Key::F3, ctrl, 1).unwrap(),
        b"\x1b[13;5~"
    );
    assert_eq!(
        encode_kitty_named_press(Key::ArrowUp, ctrl, 1).unwrap(),
        b"\x1b[1;5A"
    );
    assert_eq!(
        encode_kitty_named_press(Key::Escape, plain, 1).unwrap(),
        b"\x1b[27u"
    );
    for (key, expected, modified) in [
        (Key::Enter, b"\x1b[13u".as_slice(), b"\x1b[13;5u".as_slice()),
        (Key::Tab, b"\x1b[9u".as_slice(), b"\x1b[9;5u".as_slice()),
        (
            Key::Backspace,
            b"\x1b[127u".as_slice(),
            b"\x1b[127;5u".as_slice(),
        ),
    ] {
        assert_eq!(encode_kitty_named_press(key, plain, 1), None);
        assert_eq!(
            encode_kitty_named_press(key, ctrl, 1).as_deref(),
            Some(modified)
        );
        assert_eq!(
            encode_kitty_named_press(key, plain, 8).as_deref(),
            Some(expected)
        );
    }
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
    Some(serialize_kitty_press(
        code,
        u32::from(kitty_modifier_value(modifiers)),
        b'u',
        (None, None),
        None,
    ))
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
