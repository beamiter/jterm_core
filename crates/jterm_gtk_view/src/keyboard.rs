//! Project physical GTK key transitions and inline text into Kitty reports.
use gtk4::{gdk, prelude::*};
use jterm_engine::input::{
    kitty_keypad_report, kitty_modifier_report, kitty_named_report, kitty_text_report,
    Key as EngineKey, KeyboardLocks, KeypadKey, KittyKeyReport, KittyModifierKey, KittyTextKey,
    Modifiers, KITTY_REPORT_ALL_KEYS,
};

pub(crate) type ReportState = (u16, u64);

#[derive(Default)]
pub(crate) struct NativeKeyboard {
    context: ReportState,
    held: Vec<(u32, KittyKeyReport)>,
    modifier_sides: u8,
}

impl NativeKeyboard {
    pub(crate) fn clear(&mut self) {
        self.held.clear();
        self.modifier_sides = 0;
    }

    pub(crate) fn sync(&mut self, context: ReportState) {
        if self.context != context {
            self.held.clear();
            self.context = context;
        }
    }

    pub(crate) fn tracks(&self, code: u32) -> Option<KittyKeyReport> {
        self.held
            .iter()
            .find(|(held, _)| *held == code)
            .map(|(_, report)| *report)
    }

    pub(crate) fn release(&mut self, code: u32) -> Option<KittyKeyReport> {
        let index = self.held.iter().position(|(held, _)| *held == code)?;
        Some(self.held.swap_remove(index).1)
    }

    pub(crate) fn admit(&mut self, context: ReportState, code: u32, report: KittyKeyReport) {
        if code != 0 && context == self.context {
            self.held.push((code, report));
        }
    }

    pub(crate) fn modifiers(
        &mut self,
        key: gdk::Key,
        state: gdk::ModifierType,
        down: bool,
    ) -> Modifiers {
        use gdk::Key;
        let mut modifiers = key_modifiers(key, state);
        modifiers.super_key = state.contains(gdk::ModifierType::SUPER_MASK);
        let (slot, output) = match key {
            Key::Shift_L => (0, &mut modifiers.shift),
            Key::Shift_R => (1, &mut modifiers.shift),
            Key::Alt_L => (2, &mut modifiers.alt),
            Key::Alt_R => (3, &mut modifiers.alt),
            Key::Control_L => (4, &mut modifiers.ctrl),
            Key::Control_R => (5, &mut modifiers.ctrl),
            Key::Super_L => (6, &mut modifiers.super_key),
            Key::Super_R => (7, &mut modifiers.super_key),
            _ => return modifiers,
        };
        let side = 1 << slot;
        if down {
            self.modifier_sides |= side;
        } else {
            self.modifier_sides &= !side;
        }
        *output = self.modifier_sides & (3 << (slot & !1)) != 0;
        modifiers
    }
}

pub(crate) fn locks(event: &gdk::KeyEvent, display: &gdk::Display, flags: u16) -> KeyboardLocks {
    if flags & KITTY_REPORT_ALL_KEYS == 0 {
        return KeyboardLocks::default();
    }
    let state = event.modifier_state();
    let key = event.keyval();
    let num_lock = match keypad_number_layer(key) {
        Some(numeric)
            if !state.contains(gdk::ModifierType::SHIFT_MASK)
                && display
                    .translate_key(
                        event.keycode(),
                        gdk::ModifierType::empty(),
                        event.layout() as i32,
                    )
                    .is_some_and(|(base, _, _, _)| keypad_number_layer(base) == Some(false)) =>
        {
            numeric
        }
        _ => event.device().is_some_and(|device| device.is_num_locked()),
    };
    KeyboardLocks {
        caps_lock: state.contains(gdk::ModifierType::LOCK_MASK),
        num_lock,
    }
}

// A dual-layer keypad key retains the transition's Num Lock evidence in its
// keyval even when GTK's device state has advanced to a newer event.
fn keypad_number_layer(key: gdk::Key) -> Option<bool> {
    use gdk::Key;
    match key {
        Key::KP_0
        | Key::KP_1
        | Key::KP_2
        | Key::KP_3
        | Key::KP_4
        | Key::KP_5
        | Key::KP_6
        | Key::KP_7
        | Key::KP_8
        | Key::KP_9
        | Key::KP_Decimal => Some(true),
        Key::KP_Insert
        | Key::KP_End
        | Key::KP_Down
        | Key::KP_Page_Down
        | Key::KP_Left
        | Key::KP_Begin
        | Key::KP_Right
        | Key::KP_Home
        | Key::KP_Up
        | Key::KP_Page_Up
        | Key::KP_Delete => Some(false),
        _ => None,
    }
}

pub(crate) fn report(
    event: &gdk::KeyEvent,
    display: &gdk::Display,
    key: gdk::Key,
    modifiers: Modifiers,
    flags: u16,
) -> Option<KittyKeyReport> {
    let named = named_key(key).map(|(key, _)| key);
    if let Some(position) = keypad_key(key) {
        let logical = named.or_else(|| printable(key).map(EngineKey::Character))?;
        return kitty_keypad_report(position, logical, modifiers, flags);
    }
    if let Some(key) = named {
        return kitty_named_report(key, modifiers, flags);
    }
    if let Some(key) = modifier_key(key) {
        return kitty_modifier_report(key, flags);
    }
    let actual = printable(key)?;
    let base = base_character(event, display)
        .or_else(|| actual.to_lowercase().next())
        .unwrap_or(actual);
    kitty_text_report(
        KittyTextKey {
            base,
            shifted: modifiers.shift.then_some(actual),
            base_layout: pc101_key(event.keycode()),
        },
        modifiers,
        flags,
    )
}

pub(crate) fn base_character(event: &gdk::KeyEvent, display: &gdk::Display) -> Option<char> {
    display
        .translate_key(
            event.keycode(),
            gdk::ModifierType::empty(),
            event.layout() as i32,
        )
        .and_then(|(key, _, _, _)| printable(key))
}

pub(crate) fn printable(key: gdk::Key) -> Option<char> {
    key.to_unicode().filter(|ch| !ch.is_control())
}

fn pc101_key(code: u32) -> Option<char> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    // GDK's Linux X11 and Wayland backends use XKB keycodes.
    let byte = match code {
        10..=21 => b"1234567890-="[(code - 10) as usize],
        24..=35 => b"qwertyuiop[]"[(code - 24) as usize],
        38..=49 => b"asdfghjkl;'`"[(code - 38) as usize],
        51 => b'\\',
        52..=61 => b"zxcvbnm,./"[(code - 52) as usize],
        65 => b' ',
        _ => return None,
    };
    Some(char::from(byte))
}

fn modifier_key(key: gdk::Key) -> Option<KittyModifierKey> {
    use gdk::Key;
    use KittyModifierKey as Modifier;
    Some(match key {
        Key::Shift_L => Modifier::ShiftLeft,
        Key::Shift_R => Modifier::ShiftRight,
        Key::Control_L => Modifier::ControlLeft,
        Key::Control_R => Modifier::ControlRight,
        Key::Alt_L => Modifier::AltLeft,
        Key::Alt_R | Key::ISO_Level3_Shift => Modifier::AltRight,
        Key::Super_L => Modifier::SuperLeft,
        Key::Super_R => Modifier::SuperRight,
        Key::Caps_Lock => Modifier::CapsLock,
        Key::Scroll_Lock => Modifier::ScrollLock,
        Key::Num_Lock => Modifier::NumLock,
        _ => return None,
    })
}

pub(crate) fn key_modifiers(key: gdk::Key, state: gdk::ModifierType) -> Modifiers {
    Modifiers {
        shift: key == gdk::Key::ISO_Left_Tab || state.contains(gdk::ModifierType::SHIFT_MASK),
        alt: state.contains(gdk::ModifierType::ALT_MASK),
        ctrl: state.contains(gdk::ModifierType::CONTROL_MASK),
        super_key: false,
    }
}

pub(crate) fn logical_key(key: gdk::Key) -> Option<std::borrow::Cow<'static, str>> {
    if let Some((_, name)) = named_key(key) {
        return Some(name.into());
    }
    printable(key).map(|ch| ch.to_string().into())
}

fn named_key(key: gdk::Key) -> Option<(EngineKey, &'static str)> {
    use gdk::Key;
    Some(match key {
        Key::Up | Key::KP_Up => (EngineKey::ArrowUp, "ArrowUp"),
        Key::Down | Key::KP_Down => (EngineKey::ArrowDown, "ArrowDown"),
        Key::Left | Key::KP_Left => (EngineKey::ArrowLeft, "ArrowLeft"),
        Key::Right | Key::KP_Right => (EngineKey::ArrowRight, "ArrowRight"),
        Key::Home | Key::KP_Home => (EngineKey::Home, "Home"),
        Key::End | Key::KP_End => (EngineKey::End, "End"),
        Key::Insert | Key::KP_Insert => (EngineKey::Insert, "Insert"),
        Key::Delete | Key::KP_Delete => (EngineKey::Delete, "Delete"),
        Key::KP_Begin => (EngineKey::Unidentified, "Clear"),
        Key::Page_Up | Key::KP_Page_Up => (EngineKey::PageUp, "PageUp"),
        Key::Page_Down | Key::KP_Page_Down => (EngineKey::PageDown, "PageDown"),
        Key::F1 => (EngineKey::F1, "F1"),
        Key::F2 => (EngineKey::F2, "F2"),
        Key::F3 => (EngineKey::F3, "F3"),
        Key::F4 => (EngineKey::F4, "F4"),
        Key::F5 => (EngineKey::F5, "F5"),
        Key::F6 => (EngineKey::F6, "F6"),
        Key::F7 => (EngineKey::F7, "F7"),
        Key::F8 => (EngineKey::F8, "F8"),
        Key::F9 => (EngineKey::F9, "F9"),
        Key::F10 => (EngineKey::F10, "F10"),
        Key::F11 => (EngineKey::F11, "F11"),
        Key::F12 => (EngineKey::F12, "F12"),
        Key::Return | Key::KP_Enter => (EngineKey::Enter, "Enter"),
        Key::Escape => (EngineKey::Escape, "Escape"),
        Key::BackSpace => (EngineKey::Backspace, "Backspace"),
        Key::Tab | Key::ISO_Left_Tab | Key::KP_Tab => (EngineKey::Tab, "Tab"),
        _ => return None,
    })
}

pub(crate) fn keypad_key(key: gdk::Key) -> Option<KeypadKey> {
    use gdk::Key;
    Some(match key {
        Key::KP_0 | Key::KP_Insert => KeypadKey::Digit(0),
        Key::KP_1 | Key::KP_End => KeypadKey::Digit(1),
        Key::KP_2 | Key::KP_Down => KeypadKey::Digit(2),
        Key::KP_3 | Key::KP_Page_Down => KeypadKey::Digit(3),
        Key::KP_4 | Key::KP_Left => KeypadKey::Digit(4),
        Key::KP_5 | Key::KP_Begin => KeypadKey::Digit(5),
        Key::KP_6 | Key::KP_Right => KeypadKey::Digit(6),
        Key::KP_7 | Key::KP_Home => KeypadKey::Digit(7),
        Key::KP_8 | Key::KP_Up => KeypadKey::Digit(8),
        Key::KP_9 | Key::KP_Page_Up => KeypadKey::Digit(9),
        Key::KP_Decimal | Key::KP_Delete => KeypadKey::Decimal,
        Key::KP_Divide => KeypadKey::Divide,
        Key::KP_Multiply => KeypadKey::Multiply,
        Key::KP_Subtract => KeypadKey::Subtract,
        Key::KP_Add => KeypadKey::Add,
        Key::KP_Enter => KeypadKey::Enter,
        Key::KP_Equal => KeypadKey::Equal,
        Key::KP_Separator => KeypadKey::Separator,
        Key::KP_Space => KeypadKey::Space,
        Key::KP_Tab => KeypadKey::Tab,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_reports_only_survive_in_their_admitted_context() {
        let mut keyboard = NativeKeyboard::default();
        let state = (26, 3);
        let f3 = kitty_named_report(EngineKey::F3, Modifiers::default(), state.0).unwrap();
        keyboard.sync(state);
        keyboard.admit(state, 69, f3);
        assert_eq!(keyboard.tracks(69), Some(f3));
        keyboard.sync((26, 5));
        assert_eq!(keyboard.release(69), None);
        keyboard.admit(state, 69, f3);
        assert_eq!(keyboard.tracks(69), None);
        keyboard.admit((26, 5), 69, f3);
        assert_eq!(keyboard.release(69), Some(f3));
        assert_eq!(keyboard.release(69), None);
    }
}
