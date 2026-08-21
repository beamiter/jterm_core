//! Kitty keyboard protocol: the progressive-enhancement flag stacks an
//! application drives with `CSI > flags u` / `CSI < n u` / `CSI = flags ; mode u`,
//! and the key encoding the terminal owes it while those flags are in effect.
//!
//! The terminals this crate serves render through libvte, which neither
//! tracks these flags nor encodes keys for them. Until this module, the live
//! surface answered the `CSI ? u` query with `CSI ? 0 u` — which crossterm and
//! every other client reads as "the protocol is available" — and then kept
//! sending legacy key bytes. Shift+Enter therefore reached codex and kimi as a
//! plain Enter and submitted the composer instead of inserting a newline, and
//! Esc stayed ambiguous with the start of an Alt sequence.
//!
//! Only the first flag, *disambiguate escape codes*, is implemented: it is the
//! bit that makes Esc, Shift+Enter, Ctrl+letter and Alt+letter unambiguous, and
//! it is what inline TUIs (codex, kimi, anything on crossterm/ratatui) actually
//! use. Pushes of the other bits are accepted and masked off, and the query
//! reports what is really in effect, so a client never assumes release events
//! or alternate-key reports it will not get.

/// Disambiguate escape codes (`0b1`). Implemented.
pub const DISAMBIGUATE: u8 = 0b1;
/// Report event types (`0b10`). Accepted, not implemented.
pub const REPORT_EVENT_TYPES: u8 = 0b10;
/// Report alternate keys (`0b100`). Accepted, not implemented.
pub const REPORT_ALTERNATE_KEYS: u8 = 0b100;
/// Report all keys as escape codes (`0b1000`). Accepted, not implemented.
pub const REPORT_ALL_KEYS_AS_ESCAPE_CODES: u8 = 0b1000;
/// Report associated text (`0b10000`). Accepted, not implemented.
pub const REPORT_ASSOCIATED_TEXT: u8 = 0b10000;
/// Every flag the protocol defines.
pub const ALL_FLAGS: u8 = 0b11111;
/// The flags this implementation honours. Everything stored, reported and
/// acted on is masked to this.
pub const SUPPORTED_FLAGS: u8 = DISAMBIGUATE;
/// kitty caps each screen's stack; a push onto a full stack discards the
/// oldest entry so a client that pushes per frame cannot lock itself out.
pub const MAX_STACK_DEPTH: usize = 16;

/// One flag-stack operation, as parsed from the byte stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KittyKeyboardOp {
    /// `CSI > flags u` — push `flags` onto the active screen's stack.
    Push(u8),
    /// `CSI < n u` — pop `n` entries (a missing or zero `n` pops one).
    Pop(u8),
    /// `CSI = flags ; mode u` — edit the top entry: mode 1 replaces it,
    /// 2 sets the given bits, 3 clears them. A missing mode is 1.
    Set { flags: u8, mode: u8 },
}

/// The main and alternate screens each keep their own stack.
///
/// The terminal, not the application, decides when a stack is forgotten:
/// on RIS, when the alternate screen is left, and — because a client that
/// crashes never pops — whenever shell integration says the shell is back at
/// its prompt. The host resets at PromptStart and CommandEnd for that reason.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KittyKeyboardStacks {
    main: Vec<u8>,
    alt: Vec<u8>,
    on_alt: bool,
}

impl KittyKeyboardStacks {
    pub fn new() -> Self {
        Self::default()
    }

    /// The flags in effect for the active screen, masked to what is honoured.
    pub fn flags(&self) -> u8 {
        self.active().last().copied().unwrap_or(0) & SUPPORTED_FLAGS
    }

    /// Whether any honoured flag is in effect.
    pub fn active_flags(&self) -> bool {
        self.flags() != 0
    }

    pub fn apply(&mut self, op: KittyKeyboardOp) {
        let stack = self.active_mut();
        match op {
            KittyKeyboardOp::Push(flags) => {
                if stack.len() >= MAX_STACK_DEPTH {
                    stack.remove(0);
                }
                stack.push(flags & ALL_FLAGS);
            }
            KittyKeyboardOp::Pop(count) => {
                let count = usize::from(count.max(1)).min(stack.len());
                stack.truncate(stack.len() - count);
            }
            KittyKeyboardOp::Set { flags, mode } => {
                let flags = flags & ALL_FLAGS;
                let top = match stack.last_mut() {
                    Some(top) => top,
                    None => {
                        stack.push(0);
                        stack.last_mut().expect("just pushed")
                    }
                };
                match mode {
                    2 => *top |= flags,
                    3 => *top &= !flags,
                    _ => *top = flags,
                }
            }
        }
    }

    /// The application switched to the alternate screen; its stack takes over.
    pub fn enter_alt_screen(&mut self) {
        self.on_alt = true;
    }

    /// Back to the main screen. The alternate stack is forgotten with it, as
    /// kitty does, so a full-screen app that exits without popping leaves the
    /// shell's keys alone.
    pub fn leave_alt_screen(&mut self) {
        self.alt.clear();
        self.on_alt = false;
    }

    /// RIS, or the shell reclaiming the terminal: everything is forgotten.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    fn active(&self) -> &Vec<u8> {
        if self.on_alt {
            &self.alt
        } else {
            &self.main
        }
    }

    fn active_mut(&mut self) -> &mut Vec<u8> {
        if self.on_alt {
            &mut self.alt
        } else {
            &mut self.main
        }
    }
}

/// The reply to `CSI ? u`: the honoured flags currently in effect.
pub fn query_reply(flags: u8) -> Vec<u8> {
    format!("\x1b[?{}u", flags & SUPPORTED_FLAGS).into_bytes()
}

/// A key as the encoder needs to see it, already freed of toolkit detail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KittyKey {
    /// A key with a Unicode value: the codepoint of the key's *base* form —
    /// lowercase, unshifted — which is what the protocol reports regardless of
    /// the modifiers held.
    Unicode(char),
    Escape,
    Enter,
    Tab,
    Backspace,
    Space,
    /// Arrows, function keys, navigation and keypad keys, lone modifiers: the
    /// legacy encoding the terminal already produces (with xterm-style
    /// modifier parameters) is what the protocol specifies for them under the
    /// disambiguate flag, so the encoder leaves them alone.
    Functional,
}

/// Modifier state of a key event. Lock modifiers are deliberately absent: the
/// protocol reports them only with flags this implementation does not honour.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
    pub super_key: bool,
}

impl Modifiers {
    /// Any modifier other than shift — the ones that turn a text key into an
    /// escape code.
    fn non_shift(&self) -> bool {
        self.alt || self.ctrl || self.super_key
    }

    fn any(&self) -> bool {
        self.shift || self.non_shift()
    }

    /// The protocol's modifier parameter: `1 + bitmask`, shift=1, alt=2,
    /// ctrl=4, super=8.
    fn parameter(&self) -> u8 {
        1 + u8::from(self.shift)
            + (u8::from(self.alt) << 1)
            + (u8::from(self.ctrl) << 2)
            + (u8::from(self.super_key) << 3)
    }
}

/// What the terminal must send for this key while `flags` are in effect, or
/// `None` when the legacy bytes the VTE produces on its own are already the
/// right answer.
///
/// Under the disambiguate flag:
/// - Esc is always `CSI 27 u` (with its modifier parameter when held).
/// - Enter, Tab and Backspace stay legacy only with NO modifier at all;
///   Shift+Enter is `CSI 13 ; 2 u`, which is the whole reason composers can
///   tell "newline" from "submit".
/// - Space and text keys stay text with shift alone; with Ctrl, Alt or Super
///   they become `CSI codepoint ; mods u`, so Ctrl+I is no longer Tab and
///   Alt+x is no longer Esc followed by x.
/// - Functional keys keep their legacy form.
pub fn encode_key(key: KittyKey, mods: Modifiers, flags: u8) -> Option<Vec<u8>> {
    if flags & DISAMBIGUATE == 0 {
        return None;
    }
    let code = match key {
        KittyKey::Escape => 27,
        KittyKey::Enter if mods.any() => 13,
        KittyKey::Tab if mods.any() => 9,
        KittyKey::Backspace if mods.any() => 127,
        KittyKey::Space if mods.non_shift() => 32,
        KittyKey::Unicode(ch) if mods.non_shift() => u32::from(ch),
        _ => return None,
    };
    Some(csi_u(code, mods))
}

/// What the terminal should send *instead of* `legacy` — the bytes its VTE
/// just produced for the key press `key` + `mods` — or `None` to let the
/// legacy bytes through unchanged.
///
/// Rewriting at the commit rather than at the key press is what keeps input
/// methods whole. A capture-phase handler that encoded keys itself would run
/// ahead of the IME: Esc would no longer cancel a preedit, Ctrl+Space would no
/// longer toggle the method, and Enter would submit the composer instead of
/// choosing a candidate. At the commit, a key the IME consumed never arrives,
/// and composed text never matches a legacy form, so both stay untouched —
/// only the bytes VTE emitted *for that key* are replaced.
pub fn rewrite_commit(
    key: KittyKey,
    mods: Modifiers,
    legacy: &[u8],
    flags: u8,
) -> Option<Vec<u8>> {
    let encoded = encode_key(key, mods, flags)?;
    legacy_form_matches(key, legacy).then_some(encoded)
}

/// Whether `legacy` is a form the VTE emits for `key` — including the forms it
/// emits when the modifiers the protocol would report were held, since those
/// are exactly the presses being rewritten.
fn legacy_form_matches(key: KittyKey, legacy: &[u8]) -> bool {
    match key {
        KittyKey::Escape => legacy == b"\x1b",
        KittyKey::Enter => matches!(legacy, b"\r" | b"\n" | b"\r\n"),
        // Shift+Tab is CSI Z (back-tab) in the legacy encoding.
        KittyKey::Tab => matches!(legacy, b"\t" | b"\x1b[Z"),
        KittyKey::Backspace => matches!(legacy, b"\x7f" | b"\x08"),
        // Ctrl+Space is NUL.
        KittyKey::Space => matches!(legacy, b" " | b"\0"),
        KittyKey::Unicode(ch) => {
            let mut buf = [0u8; 4];
            let text = ch.encode_utf8(&mut buf).as_bytes();
            let is_c0 = |bytes: &[u8]| bytes.len() == 1 && bytes[0] < 0x20;
            let same_text = |bytes: &[u8]| {
                std::str::from_utf8(bytes)
                    .map(|s| s.to_lowercase().as_bytes() == text)
                    .unwrap_or(false)
            };
            // Ctrl: a lone C0 byte. Alt: ESC then the text (or the C0 byte
            // when Ctrl is held too). Super: VTE ignores it and sends the text.
            is_c0(legacy)
                || same_text(legacy)
                || (legacy.first() == Some(&0x1b)
                    && legacy.len() > 1
                    && (is_c0(&legacy[1..]) || same_text(&legacy[1..])))
        }
        KittyKey::Functional => false,
    }
}

fn csi_u(code: u32, mods: Modifiers) -> Vec<u8> {
    let parameter = mods.parameter();
    if parameter == 1 {
        format!("\x1b[{code}u").into_bytes()
    } else {
        format!("\x1b[{code};{parameter}u").into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mods(shift: bool, alt: bool, ctrl: bool) -> Modifiers {
        Modifiers {
            shift,
            alt,
            ctrl,
            super_key: false,
        }
    }

    #[test]
    fn push_pop_set_track_the_active_screen_and_mask_to_supported_bits() {
        let mut stacks = KittyKeyboardStacks::new();
        assert_eq!(stacks.flags(), 0);
        // codex and kimi push 7; only the disambiguate bit is honoured.
        stacks.apply(KittyKeyboardOp::Push(7));
        assert_eq!(stacks.flags(), DISAMBIGUATE);
        assert_eq!(query_reply(stacks.flags()), b"\x1b[?1u");
        stacks.apply(KittyKeyboardOp::Push(0));
        assert_eq!(stacks.flags(), 0);
        stacks.apply(KittyKeyboardOp::Pop(0));
        assert_eq!(stacks.flags(), DISAMBIGUATE, "a zero count pops one");
        // Set replaces, ors, and clears the top entry.
        stacks.apply(KittyKeyboardOp::Set { flags: 0, mode: 1 });
        assert_eq!(stacks.flags(), 0);
        stacks.apply(KittyKeyboardOp::Set { flags: 1, mode: 2 });
        assert_eq!(stacks.flags(), 1);
        stacks.apply(KittyKeyboardOp::Set { flags: 1, mode: 3 });
        assert_eq!(stacks.flags(), 0);
        // Popping past the bottom is a no-op, and Set on an empty stack pushes.
        stacks.apply(KittyKeyboardOp::Pop(99));
        assert_eq!(stacks, KittyKeyboardStacks::new());
        stacks.apply(KittyKeyboardOp::Set { flags: 1, mode: 1 });
        assert_eq!(stacks.flags(), 1);
    }

    #[test]
    fn alternate_screen_has_its_own_stack_and_forgets_it_on_exit() {
        let mut stacks = KittyKeyboardStacks::new();
        stacks.apply(KittyKeyboardOp::Push(1));
        stacks.enter_alt_screen();
        assert_eq!(stacks.flags(), 0, "the alt stack starts empty");
        stacks.apply(KittyKeyboardOp::Push(1));
        assert_eq!(stacks.flags(), 1);
        stacks.leave_alt_screen();
        assert_eq!(stacks.flags(), 1, "back to the main stack, still pushed");
        stacks.enter_alt_screen();
        assert_eq!(stacks.flags(), 0, "a crashed full-screen app left nothing behind");
        stacks.reset();
        assert_eq!(stacks, KittyKeyboardStacks::new());
    }

    #[test]
    fn a_full_stack_drops_its_oldest_entry() {
        let mut stacks = KittyKeyboardStacks::new();
        for _ in 0..MAX_STACK_DEPTH {
            stacks.apply(KittyKeyboardOp::Push(0));
        }
        stacks.apply(KittyKeyboardOp::Push(1));
        assert_eq!(stacks.flags(), 1);
        for _ in 0..MAX_STACK_DEPTH {
            stacks.apply(KittyKeyboardOp::Pop(1));
        }
        assert_eq!(stacks, KittyKeyboardStacks::new());
    }

    #[test]
    fn nothing_is_encoded_without_the_disambiguate_flag() {
        assert_eq!(encode_key(KittyKey::Escape, mods(false, false, false), 0), None);
        assert_eq!(
            encode_key(KittyKey::Enter, mods(true, false, false), REPORT_EVENT_TYPES),
            None
        );
    }

    #[test]
    fn shift_enter_becomes_csi_u_and_plain_enter_stays_legacy() {
        assert_eq!(
            encode_key(KittyKey::Enter, mods(true, false, false), 1).as_deref(),
            Some(&b"\x1b[13;2u"[..])
        );
        assert_eq!(encode_key(KittyKey::Enter, mods(false, false, false), 1), None);
        assert_eq!(
            encode_key(KittyKey::Enter, mods(false, false, true), 1).as_deref(),
            Some(&b"\x1b[13;5u"[..])
        );
        assert_eq!(
            encode_key(KittyKey::Tab, mods(true, false, false), 1).as_deref(),
            Some(&b"\x1b[9;2u"[..])
        );
        assert_eq!(encode_key(KittyKey::Tab, mods(false, false, false), 1), None);
        assert_eq!(
            encode_key(KittyKey::Backspace, mods(false, true, false), 1).as_deref(),
            Some(&b"\x1b[127;3u"[..])
        );
        assert_eq!(encode_key(KittyKey::Backspace, mods(false, false, false), 1), None);
    }

    #[test]
    fn escape_is_always_disambiguated() {
        assert_eq!(
            encode_key(KittyKey::Escape, mods(false, false, false), 1).as_deref(),
            Some(&b"\x1b[27u"[..])
        );
        assert_eq!(
            encode_key(KittyKey::Escape, mods(true, false, false), 1).as_deref(),
            Some(&b"\x1b[27;2u"[..])
        );
    }

    #[test]
    fn text_keys_stay_text_until_ctrl_alt_or_super_is_held() {
        // Shift alone produces text; the VTE sends it.
        assert_eq!(encode_key(KittyKey::Unicode('a'), mods(true, false, false), 1), None);
        assert_eq!(encode_key(KittyKey::Space, mods(true, false, false), 1), None);
        // Ctrl+c: no longer the raw 0x03 byte.
        assert_eq!(
            encode_key(KittyKey::Unicode('c'), mods(false, false, true), 1).as_deref(),
            Some(&b"\x1b[99;5u"[..])
        );
        // Alt+x: no longer ESC followed by x.
        assert_eq!(
            encode_key(KittyKey::Unicode('x'), mods(false, true, false), 1).as_deref(),
            Some(&b"\x1b[120;3u"[..])
        );
        // Ctrl+Shift+a reports the base key with both modifiers.
        assert_eq!(
            encode_key(KittyKey::Unicode('a'), mods(true, false, true), 1).as_deref(),
            Some(&b"\x1b[97;6u"[..])
        );
        assert_eq!(
            encode_key(KittyKey::Space, mods(false, false, true), 1).as_deref(),
            Some(&b"\x1b[32;5u"[..])
        );
        let sup = Modifiers {
            super_key: true,
            ..Modifiers::default()
        };
        assert_eq!(
            encode_key(KittyKey::Unicode('k'), sup, 1).as_deref(),
            Some(&b"\x1b[107;9u"[..])
        );
    }

    #[test]
    fn functional_keys_keep_their_legacy_encoding() {
        assert_eq!(encode_key(KittyKey::Functional, mods(true, true, true), 1), None);
    }

    #[test]
    fn commits_are_rewritten_only_when_they_are_the_keys_own_legacy_bytes() {
        // Shift+Enter: VTE commits "\r" — replaced.
        assert_eq!(
            rewrite_commit(KittyKey::Enter, mods(true, false, false), b"\r", 1).as_deref(),
            Some(&b"\x1b[13;2u"[..])
        );
        // The IME used Enter to choose a candidate and committed text instead:
        // untouched.
        assert_eq!(
            rewrite_commit(KittyKey::Enter, mods(true, false, false), "中".as_bytes(), 1),
            None
        );
        // Plain Esc.
        assert_eq!(
            rewrite_commit(KittyKey::Escape, mods(false, false, false), b"\x1b", 1).as_deref(),
            Some(&b"\x1b[27u"[..])
        );
        // Ctrl+c is 0x03 in legacy; Alt+x is ESC x; Alt+Shift+a is ESC A.
        assert_eq!(
            rewrite_commit(KittyKey::Unicode('c'), mods(false, false, true), b"\x03", 1)
                .as_deref(),
            Some(&b"\x1b[99;5u"[..])
        );
        assert_eq!(
            rewrite_commit(KittyKey::Unicode('x'), mods(false, true, false), b"\x1bx", 1)
                .as_deref(),
            Some(&b"\x1b[120;3u"[..])
        );
        assert_eq!(
            rewrite_commit(KittyKey::Unicode('a'), mods(true, true, false), b"\x1bA", 1)
                .as_deref(),
            Some(&b"\x1b[97;4u"[..])
        );
        // Shift+Tab arrives as back-tab.
        assert_eq!(
            rewrite_commit(KittyKey::Tab, mods(true, false, false), b"\x1b[Z", 1).as_deref(),
            Some(&b"\x1b[9;2u"[..])
        );
        // Ctrl+Space is NUL.
        assert_eq!(
            rewrite_commit(KittyKey::Space, mods(false, false, true), b"\0", 1).as_deref(),
            Some(&b"\x1b[32;5u"[..])
        );
        // A commit that is not this key's legacy form is left alone.
        assert_eq!(
            rewrite_commit(KittyKey::Unicode('c'), mods(false, false, true), b"hello", 1),
            None
        );
        // And nothing happens without the flag.
        assert_eq!(
            rewrite_commit(KittyKey::Enter, mods(true, false, false), b"\r", 0),
            None
        );
    }
}
