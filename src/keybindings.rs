//! Toolkit-neutral keyboard-chord core for the jterm family.
//!
//! jterm1..4 each grew their own chord handling (GTK `parse_key_combo` /
//! `key_combo_to_string` in jterm1/4, string-level `normalize_binding` in
//! jterm2, `KeyBinding::canonical` in jterm3). This module is the shared
//! replacement: the *union* of the four grammars in, one canonical form
//! out, with zero toolkit types so GTK, egui and iced frontends can all
//! link it.
//!
//! Family decisions frozen here (do not relitigate per-app):
//!
//! - **Canonical modifier order is `ctrl+shift+alt+super`** — the display
//!   order jterm1/4 already print and the storage order jterm2/3 already
//!   persist. [`Chord::display`] renders `Ctrl+Shift+Alt+Super+<Key>`;
//!   [`Chord::canonical`] renders the all-lowercase storage form.
//! - **Numpad digits fold onto the main row**: chords only ever store
//!   `Char('0'..='9')`. Frontends normalize keypad keysyms (`KP_1`, ...)
//!   to plain digits before lookup so `ctrl+1` covers both keyboards.
//! - **Backslash stores as the word `backslash`** (jterm3's fold): both
//!   `ctrl+\` and `ctrl+backslash` parse to the same chord and
//!   [`Chord::canonical`] always emits `ctrl+backslash`, so TOML/JSON
//!   configs never need escaping. [`Chord::display`] shows the literal
//!   `\`.
//! - **`Shift+Tab` is one chord**: toolkits that report a distinct keysym
//!   for it (GTK's `ISO_Left_Tab`) must fold it to `Named(Tab)` before
//!   lookup.
//! - **[`DEFAULT_CHORDS`]** is the cross-app ergonomic contract — the
//!   chords each repo's `common_default_chord_table`-style test pins.
//!   Deliberately *excluded* rows are documented on the constant.

use std::fmt;

/// Modifier set of a chord. Canonical order everywhere in the family is
/// ctrl, shift, alt, super (`sup` — `super` is a Rust keyword).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Mods {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub sup: bool,
}

/// Non-character keys the family binds. Kept to the set every frontend
/// can actually deliver; anything toolkit-specific stays out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NamedKey {
    Tab,
    Escape,
    Return,
    Space,
    Backspace,
    Delete,
    Home,
    End,
    Insert,
    PageUp,
    PageDown,
    Up,
    Down,
    Left,
    Right,
}

impl NamedKey {
    /// Human-facing spelling used by [`Chord::display`]. `Return` shows
    /// as "Enter" — the family UI convention (jterm2's prettify, jterm1/4
    /// display).
    fn display_name(self) -> &'static str {
        match self {
            NamedKey::Tab => "Tab",
            NamedKey::Escape => "Escape",
            NamedKey::Return => "Enter",
            NamedKey::Space => "Space",
            NamedKey::Backspace => "Backspace",
            NamedKey::Delete => "Delete",
            NamedKey::Home => "Home",
            NamedKey::End => "End",
            NamedKey::Insert => "Insert",
            NamedKey::PageUp => "PageUp",
            NamedKey::PageDown => "PageDown",
            NamedKey::Up => "Up",
            NamedKey::Down => "Down",
            NamedKey::Left => "Left",
            NamedKey::Right => "Right",
        }
    }

    /// Lowercase storage spelling used by [`Chord::canonical`]. Note the
    /// storage form keeps `return` (jterm2/3 store `ctrl+shift+return`)
    /// even though the display form says "Enter"; both parse.
    fn canonical_name(self) -> &'static str {
        match self {
            NamedKey::Tab => "tab",
            NamedKey::Escape => "escape",
            NamedKey::Return => "return",
            NamedKey::Space => "space",
            NamedKey::Backspace => "backspace",
            NamedKey::Delete => "delete",
            NamedKey::Home => "home",
            NamedKey::End => "end",
            NamedKey::Insert => "insert",
            NamedKey::PageUp => "pageup",
            NamedKey::PageDown => "pagedown",
            NamedKey::Up => "up",
            NamedKey::Down => "down",
            NamedKey::Left => "left",
            NamedKey::Right => "right",
        }
    }
}

/// A chord's non-modifier key.
///
/// Invariants maintained by [`parse`] (direct construction must follow
/// them too, or chord equality and map lookup silently break):
///
/// - `Char` holds the Unicode-lowercased character (`'T'` is stored as
///   `'t'`; digits and symbols are themselves). Numpad digits fold to
///   these same main-row chars by family convention.
/// - `Function` is in `1..=24`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KeySym {
    Char(char),
    Named(NamedKey),
    Function(u8),
}

/// A complete keyboard chord: modifier set plus one key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Chord {
    pub mods: Mods,
    pub key: KeySym,
}

impl Chord {
    /// Human-facing form: `Ctrl+Shift+Alt+Super+<Key>`.
    ///
    /// Named keys are title-cased (`Tab`, `PageUp`), `Return` shows as
    /// `Enter`, letters are uppercased, symbols are verbatim (`Ctrl+=`,
    /// `Ctrl+\`), function keys are `F5`-style. Always re-parses to the
    /// same chord: [`parse`]`(c.display()) == Ok(c)`.
    pub fn display(&self) -> String {
        let mut out = String::new();
        push_mods(&mut out, self.mods, ["Ctrl+", "Shift+", "Alt+", "Super+"]);
        match self.key {
            KeySym::Named(named) => out.push_str(named.display_name()),
            KeySym::Function(n) => out.push_str(&format!("F{n}")),
            KeySym::Char(c) => push_display_char(&mut out, c),
        }
        out
    }

    /// Lowercase storage form: `ctrl+shift+alt+super+<key>`.
    ///
    /// This is the one spelling configs and maps should persist. Named
    /// keys use their lowercase names (`tab`, `return`, `pageup`), chars
    /// are themselves — except `'\\'`, which folds to `backslash` so the
    /// stored string never needs TOML/JSON escaping. Always re-parses to
    /// the same chord: [`parse`]`(c.canonical()) == Ok(c)`.
    pub fn canonical(&self) -> String {
        let mut out = String::new();
        push_mods(&mut out, self.mods, ["ctrl+", "shift+", "alt+", "super+"]);
        match self.key {
            KeySym::Named(named) => out.push_str(named.canonical_name()),
            KeySym::Function(n) => out.push_str(&format!("f{n}")),
            KeySym::Char('\\') => out.push_str("backslash"),
            KeySym::Char(c) => out.push(c),
        }
        out
    }
}

fn push_mods(out: &mut String, mods: Mods, names: [&str; 4]) {
    let flags = [mods.ctrl, mods.shift, mods.alt, mods.sup];
    for (on, name) in flags.into_iter().zip(names) {
        if on {
            out.push_str(name);
        }
    }
}

/// Letters display uppercase; digits and symbols verbatim. Only
/// uppercase when the result is a single char that lowers back to `c` —
/// e.g. `'ß'` uppercases to `"SS"`, which [`parse`] could not read back,
/// so it displays verbatim.
fn push_display_char(out: &mut String, c: char) {
    let mut upper = c.to_uppercase();
    if let (Some(u), None) = (upper.next(), upper.next()) {
        let mut back = u.to_lowercase();
        if (back.next(), back.next()) == (Some(c), None) {
            out.push(u);
            return;
        }
    }
    out.push(c);
}

/// Why a chord string failed to parse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseError {
    /// Input was empty (or all whitespace).
    Empty,
    /// A modifier appeared twice, possibly via aliases (`ctrl+control+t`).
    DuplicateModifier(String),
    /// A non-final token is not a known modifier name.
    UnknownModifier(String),
    /// The final token is not a known key.
    UnknownKey(String),
    /// Every token was a modifier — there is no key to bind.
    MissingKey,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::Empty => write!(f, "empty key chord"),
            ParseError::DuplicateModifier(m) => write!(f, "duplicate modifier '{m}'"),
            ParseError::UnknownModifier(m) => write!(f, "unknown modifier '{m}'"),
            ParseError::UnknownKey(k) => write!(f, "unknown key '{k}'"),
            ParseError::MissingKey => write!(f, "chord has modifiers but no key"),
        }
    }
}

impl std::error::Error for ParseError {}

/// Parse a chord string using the union of the four jterm grammars.
///
/// - Tokens are split on `+` and individually trimmed; matching is
///   case-insensitive throughout.
/// - Modifier aliases: `ctrl`/`control`, `shift`, `alt`/`option`,
///   `super`/`cmd`/`command`/`win`/`meta`. A duplicated modifier (even
///   via an alias) is an error — jterm2's strictness, kept because it
///   catches typo'd chords in config files.
/// - `+` is also the separator, so a literal plus key uses the jterm1/4
///   special cases: an input ending in `++` with at least three parts
///   (`ctrl+shift++`), or a trailing empty part with at least two parts
///   (`ctrl++`), means "key is `+`". `plus` also always works.
/// - Digits parse to `Char('0'..='9')`; frontends fold numpad keysyms to
///   the main row before lookup (family convention, see module docs).
/// - Single characters store Unicode-lowercased; a character whose
///   lowercase expands to multiple chars (e.g. `'İ'`) is rejected rather
///   than stored unmatchable.
pub fn parse(s: &str) -> Result<Chord, ParseError> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err(ParseError::Empty);
    }
    let parts: Vec<&str> = trimmed.split('+').map(str::trim).collect();

    // The last part is the key, but a literal '+' key needs special cases:
    // a bare "+" splits into two empty parts, and "ctrl++"/"ctrl+shift++"
    // carry two trailing empties. A single trailing '+' with no key (e.g.
    // "ctrl+") is a malformed chord, not a plus key — the historical jterm1/4
    // branch silently dropped the modifier there.
    let (mod_parts, key_token) = if trimmed == "+" {
        (&parts[..0], "+")
    } else if trimmed.ends_with("++") && parts.len() >= 3 {
        (&parts[..parts.len() - 2], "+")
    } else {
        let (last, rest) = parts.split_last().expect("split always yields one part");
        if last.is_empty() {
            return Err(ParseError::MissingKey);
        }
        (rest, *last)
    };

    let mut mods = Mods::default();
    for part in mod_parts {
        apply_modifier(&mut mods, part)?;
    }
    let key = parse_key_token(key_token)?;
    Ok(Chord { mods, key })
}

fn is_modifier_name(lower: &str) -> bool {
    matches!(
        lower,
        "ctrl"
            | "control"
            | "shift"
            | "alt"
            | "option"
            | "super"
            | "cmd"
            | "command"
            | "win"
            | "meta"
    )
}

fn apply_modifier(mods: &mut Mods, token: &str) -> Result<(), ParseError> {
    let lower = token.to_lowercase();
    let slot = match lower.as_str() {
        "ctrl" | "control" => &mut mods.ctrl,
        "shift" => &mut mods.shift,
        "alt" | "option" => &mut mods.alt,
        "super" | "cmd" | "command" | "win" | "meta" => &mut mods.sup,
        _ => return Err(ParseError::UnknownModifier(token.to_string())),
    };
    if *slot {
        return Err(ParseError::DuplicateModifier(token.to_string()));
    }
    *slot = true;
    Ok(())
}

fn parse_key_token(token: &str) -> Result<KeySym, ParseError> {
    let lower = token.to_lowercase();
    if is_modifier_name(&lower) {
        // "ctrl+shift" ends in a modifier: a modifiers-only chord.
        return Err(ParseError::MissingKey);
    }
    use NamedKey::*;
    let sym = match lower.as_str() {
        "tab" => KeySym::Named(Tab),
        "esc" | "escape" => KeySym::Named(Escape),
        "return" | "enter" => KeySym::Named(Return),
        "space" => KeySym::Named(Space),
        "backspace" => KeySym::Named(Backspace),
        "delete" | "del" => KeySym::Named(Delete),
        "home" => KeySym::Named(Home),
        "end" => KeySym::Named(End),
        "insert" | "ins" => KeySym::Named(Insert),
        "pageup" | "page_up" | "prior" => KeySym::Named(PageUp),
        "pagedown" | "page_down" | "next" => KeySym::Named(PageDown),
        "up" | "arrowup" => KeySym::Named(Up),
        "down" | "arrowdown" => KeySym::Named(Down),
        "left" | "arrowleft" => KeySym::Named(Left),
        "right" | "arrowright" => KeySym::Named(Right),
        // X11-style symbol names (jterm1/4 accept these via GTK's
        // Key::from_name; kept so existing configs keep parsing) plus
        // the literal characters themselves.
        "plus" | "+" => KeySym::Char('+'),
        "equal" | "equals" | "=" => KeySym::Char('='),
        "minus" | "-" => KeySym::Char('-'),
        "backslash" | "\\" => KeySym::Char('\\'),
        "exclam" | "!" => KeySym::Char('!'),
        "comma" | "," => KeySym::Char(','),
        "period" | "." => KeySym::Char('.'),
        "slash" | "/" => KeySym::Char('/'),
        "semicolon" | ";" => KeySym::Char(';'),
        "apostrophe" | "'" => KeySym::Char('\''),
        "bracketleft" | "[" => KeySym::Char('['),
        "bracketright" | "]" => KeySym::Char(']'),
        "grave" | "`" => KeySym::Char('`'),
        _ => return parse_fallback_key(token, &lower),
    };
    Ok(sym)
}

/// Function keys (`f1`..`f24`) and bare single characters.
fn parse_fallback_key(token: &str, lower: &str) -> Result<KeySym, ParseError> {
    if let Some(digits) = lower.strip_prefix('f') {
        if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
            return match digits.parse::<u8>() {
                Ok(n @ 1..=24) => Ok(KeySym::Function(n)),
                _ => Err(ParseError::UnknownKey(token.to_string())),
            };
        }
    }
    let mut chars = token.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        // Store Unicode-lowercased, but only when the lowering is a
        // single char; 'İ' lowers to "i\u{307}" and is rejected.
        let mut low = c.to_lowercase();
        if let (Some(lc), None) = (low.next(), low.next()) {
            return Ok(KeySym::Char(lc));
        }
    }
    Err(ParseError::UnknownKey(token.to_string()))
}

/// True for the config values every jterm treats as "remove this
/// binding" rather than as a chord: empty, `false`, `none`, `disabled`,
/// `unbind` (case-insensitive, surrounding whitespace ignored).
pub fn is_unbind_token(s: &str) -> bool {
    let t = s.trim();
    t.is_empty()
        || t.eq_ignore_ascii_case("false")
        || t.eq_ignore_ascii_case("none")
        || t.eq_ignore_ascii_case("disabled")
        || t.eq_ignore_ascii_case("unbind")
}

/// Actions every jterm binds by default. Names are app-neutral: the
/// split variants are *geometric* (side-by-side vs stacked) because the
/// repos disagree on the words — `ctrl+shift+e` is "SplitHorizontal" in
/// jterm4 but "TerminalSplitVertical" in jterm2/3, yet it is the same
/// gesture (new pane beside the current one). `ctrl+shift+d` stacks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CommonAction {
    NewTab,
    ClosePaneOrTab,
    Copy,
    Paste,
    NextTab,
    PrevTab,
    /// `ctrl+pagedown` — terminal-conventional tab cycling, kept distinct
    /// from [`CommonAction::NextTab`] so apps may map them independently.
    NextTabPage,
    /// `ctrl+pageup` — see [`CommonAction::NextTabPage`].
    PrevTabPage,
    /// Jump straight to tab N (1-based, `1..=8`) — browser muscle
    /// memory. `ctrl+9` is [`CommonAction::LastTab`], never "tab 9", and
    /// `ctrl+0` belongs to [`CommonAction::FontReset`].
    QuickSwitch(u8),
    LastTab,
    FontIncrease,
    FontDecrease,
    FontReset,
    Search,
    CommandPalette,
    Settings,
    Sidebar,
    DebugPanel,
    ScrollUp,
    ScrollDown,
    PaneFocusLeft,
    PaneFocusRight,
    PaneFocusUp,
    PaneFocusDown,
    PaneResizeLeft,
    PaneResizeRight,
    PaneResizeUp,
    PaneResizeDown,
    /// New pane beside the current one (`ctrl+shift+e`).
    SplitSideBySide,
    /// New pane below the current one (`ctrl+shift+d`).
    SplitStacked,
    PaneZoom,
}

/// The family-wide default chord contract, chord strings in canonical
/// form ([`Chord::canonical`] is a fixed point over every row). This is
/// the table each app's `common_default_chord_table`-style test pins so
/// the four terminals never drift apart again.
///
/// Deliberately EXCLUDED (do not add without a cross-repo decision):
///
/// - `ctrl+shift+a` — hard family conflict: SelectAllBlocks in jterm1/4
///   vs AgentToggle in jterm2/3. No neutral meaning exists.
/// - Vim-letter pane fallbacks (`ctrl+alt+h/j/k/l` and the shift+resize
///   forms) — jterm4-local workaround for desktops that grab
///   Ctrl+Alt+arrows (GNOME workspace switching); not a family contract.
/// - `ctrl++` / `ctrl+shift++` font-size aliases — jterm2-local extras
///   on top of the canonical `ctrl+=`.
/// - History palette, tab switcher and help chords — not yet bound
///   universally; they join the table once all four repos agree.
pub const DEFAULT_CHORDS: &[(CommonAction, &str)] = &[
    // Tabs.
    (CommonAction::NewTab, "ctrl+shift+t"),
    (CommonAction::ClosePaneOrTab, "ctrl+shift+w"),
    // Clipboard.
    (CommonAction::Copy, "ctrl+shift+c"),
    (CommonAction::Paste, "ctrl+shift+v"),
    // Tab cycling: both the ctrl+tab and the ctrl+page pair are contract.
    (CommonAction::NextTab, "ctrl+tab"),
    (CommonAction::PrevTab, "ctrl+shift+tab"),
    (CommonAction::NextTabPage, "ctrl+pagedown"),
    (CommonAction::PrevTabPage, "ctrl+pageup"),
    // Browser-style direct switching: 1..8 direct, 9 = last, 0 = font.
    (CommonAction::QuickSwitch(1), "ctrl+1"),
    (CommonAction::QuickSwitch(2), "ctrl+2"),
    (CommonAction::QuickSwitch(3), "ctrl+3"),
    (CommonAction::QuickSwitch(4), "ctrl+4"),
    (CommonAction::QuickSwitch(5), "ctrl+5"),
    (CommonAction::QuickSwitch(6), "ctrl+6"),
    (CommonAction::QuickSwitch(7), "ctrl+7"),
    (CommonAction::QuickSwitch(8), "ctrl+8"),
    (CommonAction::LastTab, "ctrl+9"),
    // Font size.
    (CommonAction::FontIncrease, "ctrl+="),
    (CommonAction::FontDecrease, "ctrl+-"),
    (CommonAction::FontReset, "ctrl+0"),
    // Discovery surfaces.
    (CommonAction::Search, "ctrl+shift+f"),
    (CommonAction::CommandPalette, "ctrl+shift+p"),
    (CommonAction::Settings, "ctrl+shift+o"),
    (CommonAction::Sidebar, "ctrl+backslash"),
    (CommonAction::DebugPanel, "f12"),
    // Scrollback.
    (CommonAction::ScrollUp, "ctrl+up"),
    (CommonAction::ScrollDown, "ctrl+down"),
    // Pane navigation and layout.
    (CommonAction::PaneFocusLeft, "ctrl+alt+left"),
    (CommonAction::PaneFocusRight, "ctrl+alt+right"),
    (CommonAction::PaneFocusUp, "ctrl+alt+up"),
    (CommonAction::PaneFocusDown, "ctrl+alt+down"),
    (CommonAction::PaneResizeLeft, "ctrl+shift+alt+left"),
    (CommonAction::PaneResizeRight, "ctrl+shift+alt+right"),
    (CommonAction::PaneResizeUp, "ctrl+shift+alt+up"),
    (CommonAction::PaneResizeDown, "ctrl+shift+alt+down"),
    (CommonAction::SplitSideBySide, "ctrl+shift+e"),
    (CommonAction::SplitStacked, "ctrl+shift+d"),
    (CommonAction::PaneZoom, "ctrl+shift+z"),
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    const NO_MODS: Mods = Mods {
        ctrl: false,
        shift: false,
        alt: false,
        sup: false,
    };
    const CTRL: Mods = Mods {
        ctrl: true,
        ..NO_MODS
    };
    const CTRL_SHIFT: Mods = Mods {
        ctrl: true,
        shift: true,
        ..NO_MODS
    };
    const CTRL_ALT: Mods = Mods {
        ctrl: true,
        alt: true,
        ..NO_MODS
    };
    const ALL_MODS: Mods = Mods {
        ctrl: true,
        shift: true,
        alt: true,
        sup: true,
    };

    const ALL_NAMED: [NamedKey; 15] = [
        NamedKey::Tab,
        NamedKey::Escape,
        NamedKey::Return,
        NamedKey::Space,
        NamedKey::Backspace,
        NamedKey::Delete,
        NamedKey::Home,
        NamedKey::End,
        NamedKey::Insert,
        NamedKey::PageUp,
        NamedKey::PageDown,
        NamedKey::Up,
        NamedKey::Down,
        NamedKey::Left,
        NamedKey::Right,
    ];

    fn chord(mods: Mods, key: KeySym) -> Chord {
        Chord { mods, key }
    }

    /// Both string forms must re-parse to the identical chord, or config
    /// save/reload silently drops bindings.
    fn assert_round_trips(c: Chord) {
        let display = c.display();
        let canonical = c.canonical();
        assert_eq!(
            parse(&display),
            Ok(c),
            "display form {display:?} must re-parse to {c:?}"
        );
        assert_eq!(
            parse(&canonical),
            Ok(c),
            "canonical form {canonical:?} must re-parse to {c:?}"
        );
    }

    #[test]
    fn trailing_separator_without_a_key_is_rejected_not_modifier_dropping() {
        // "ctrl+" historically parsed as a bare '+' with the modifier lost.
        assert_eq!(parse("ctrl+"), Err(ParseError::MissingKey));
        assert_eq!(parse("ctrl+shift+"), Err(ParseError::MissingKey));
        // The legitimate plus-key spellings are unaffected.
        assert_eq!(
            parse("+"),
            Ok(Chord {
                mods: Mods::default(),
                key: KeySym::Char('+'),
            })
        );
        assert!(parse("ctrl++").is_ok());
        assert!(parse("ctrl+shift++").is_ok());
    }

    // ---- DEFAULT_CHORDS contract -------------------------------------

    #[test]
    fn every_default_chord_parses_and_is_a_canonical_fixed_point() {
        for (action, s) in DEFAULT_CHORDS {
            let c = parse(s).unwrap_or_else(|e| panic!("{action:?} chord {s:?} must parse: {e}"));
            assert_eq!(
                c.canonical(),
                *s,
                "{action:?}: table row must be written in canonical form"
            );
            assert_round_trips(c);
        }
    }

    #[test]
    fn default_chords_have_no_duplicate_chords_or_actions() {
        let mut chords: HashSet<Chord> = HashSet::new();
        let mut actions: HashSet<CommonAction> = HashSet::new();
        for (action, s) in DEFAULT_CHORDS {
            let c = parse(s).expect("table entries parse");
            assert!(chords.insert(c), "chord {s:?} bound twice (at {action:?})");
            assert!(actions.insert(*action), "action {action:?} listed twice");
        }
    }

    #[test]
    fn default_chord_table_has_the_agreed_row_count() {
        // 8 tab rows + 8 quick-switch + last-tab + 3 font + 5 discovery
        // + 2 scroll + 8 pane focus/resize + 2 splits + zoom = 38.
        assert_eq!(DEFAULT_CHORDS.len(), 38);
    }

    #[test]
    fn digit_rows_follow_browser_muscle_memory() {
        let chord_for = |action: CommonAction| -> &str {
            DEFAULT_CHORDS
                .iter()
                .find(|(a, _)| *a == action)
                .map(|(_, s)| *s)
                .unwrap_or_else(|| panic!("{action:?} missing from DEFAULT_CHORDS"))
        };
        for n in 1..=8u8 {
            assert_eq!(chord_for(CommonAction::QuickSwitch(n)), format!("ctrl+{n}"));
        }
        assert_eq!(chord_for(CommonAction::LastTab), "ctrl+9");
        assert_eq!(chord_for(CommonAction::FontReset), "ctrl+0");
    }

    // ---- round-trip property over the corners ------------------------

    #[test]
    fn named_function_symbol_letter_and_digit_chords_all_round_trip() {
        let mod_sets = [NO_MODS, CTRL, CTRL_SHIFT, CTRL_ALT, ALL_MODS];
        for mods in mod_sets {
            for named in ALL_NAMED {
                assert_round_trips(chord(mods, KeySym::Named(named)));
            }
            for n in 1..=24u8 {
                assert_round_trips(chord(mods, KeySym::Function(n)));
            }
            for c in [
                '+', '=', '-', '\\', '!', ',', '.', '/', ';', '\'', '[', ']', '`',
            ] {
                assert_round_trips(chord(mods, KeySym::Char(c)));
            }
            for c in ['a', 'z', '0', '9', 'ä', 'ß'] {
                assert_round_trips(chord(mods, KeySym::Char(c)));
            }
        }
    }

    // ---- union grammar corners ---------------------------------------

    #[test]
    fn modifier_and_key_matching_is_case_insensitive() {
        let reference = parse("ctrl+shift+t").unwrap();
        for s in ["control+shift+t", "Ctrl+Shift+T", "CTRL+SHIFT+T"] {
            assert_eq!(parse(s), Ok(reference), "{s}");
        }
        assert_eq!(parse("ctrl+PAGEUP"), parse("Ctrl+PageUp"));
        assert_eq!(parse("ctrl+BACKSPACE"), parse("Ctrl+Backspace"));
    }

    #[test]
    fn whitespace_around_parts_is_trimmed() {
        assert_eq!(parse("  ctrl + shift + t  "), parse("ctrl+shift+t"));
        assert_eq!(
            parse(" control + shift + pageup "),
            parse("Ctrl+Shift+PageUp")
        );
    }

    #[test]
    fn plus_key_special_cases_agree_with_the_named_form() {
        let want = chord(CTRL_SHIFT, KeySym::Char('+'));
        assert_eq!(parse("ctrl+shift++"), Ok(want));
        assert_eq!(parse("Ctrl+Shift++"), Ok(want));
        assert_eq!(parse("ctrl+shift+plus"), Ok(want));
        assert_eq!(parse("Ctrl++"), Ok(chord(CTRL, KeySym::Char('+'))));
        assert_eq!(parse("ctrl+plus"), Ok(chord(CTRL, KeySym::Char('+'))));
        assert_eq!(parse("+"), Ok(chord(NO_MODS, KeySym::Char('+'))));
        assert_eq!(parse("plus"), Ok(chord(NO_MODS, KeySym::Char('+'))));
    }

    #[test]
    fn super_and_alt_aliases_collapse_to_one_modifier() {
        let sup = parse("super+t").unwrap();
        for s in ["cmd+t", "command+t", "win+t", "meta+t", "Cmd+T"] {
            assert_eq!(parse(s), Ok(sup), "{s}");
        }
        assert_eq!(parse("option+left"), parse("alt+left"));
        assert_eq!(parse("control+c"), parse("ctrl+c"));
    }

    #[test]
    fn named_key_aliases_agree() {
        let groups: &[&[&str]] = &[
            &["escape", "esc"],
            &["return", "enter"],
            &["delete", "del"],
            &["insert", "ins"],
            &["pageup", "page_up", "prior"],
            &["pagedown", "page_down", "next"],
            &["up", "arrowup"],
            &["down", "arrowdown"],
            &["left", "arrowleft"],
            &["right", "arrowright"],
        ];
        for group in groups {
            let reference = parse(&format!("ctrl+{}", group[0])).unwrap();
            for alias in &group[1..] {
                assert_eq!(
                    parse(&format!("ctrl+{alias}")),
                    Ok(reference),
                    "'{alias}' must alias '{}'",
                    group[0]
                );
            }
        }
    }

    #[test]
    fn symbol_names_and_literal_chars_parse_identically() {
        let pairs = [
            ("plus", '+'),
            ("equal", '='),
            ("equals", '='),
            ("minus", '-'),
            ("backslash", '\\'),
            ("exclam", '!'),
            ("comma", ','),
            ("period", '.'),
            ("slash", '/'),
            ("semicolon", ';'),
            ("apostrophe", '\''),
            ("bracketleft", '['),
            ("bracketright", ']'),
            ("grave", '`'),
        ];
        for (name, ch) in pairs {
            let want = Ok(chord(CTRL, KeySym::Char(ch)));
            assert_eq!(parse(&format!("ctrl+{name}")), want, "named form '{name}'");
            assert_eq!(parse(&format!("ctrl+{ch}")), want, "literal form '{ch}'");
        }
    }

    #[test]
    fn backslash_spellings_collapse_to_one_stored_form() {
        let named = parse("ctrl+backslash").unwrap();
        let literal = parse("ctrl+\\").unwrap();
        assert_eq!(named, literal);
        assert_eq!(named.canonical(), "ctrl+backslash");
        assert_eq!(named.display(), "Ctrl+\\");
    }

    #[test]
    fn shift_tab_and_space_chords_parse() {
        assert_eq!(
            parse("shift+tab"),
            Ok(chord(
                Mods {
                    shift: true,
                    ..NO_MODS
                },
                KeySym::Named(NamedKey::Tab)
            ))
        );
        let space = parse("ctrl+space").unwrap();
        assert_eq!(space, chord(CTRL, KeySym::Named(NamedKey::Space)));
        assert_eq!(space.display(), "Ctrl+Space");
        assert_eq!(space.canonical(), "ctrl+space");
    }

    #[test]
    fn function_keys_span_f1_to_f24() {
        assert_eq!(parse("f1"), Ok(chord(NO_MODS, KeySym::Function(1))));
        assert_eq!(parse("F12"), Ok(chord(NO_MODS, KeySym::Function(12))));
        assert_eq!(parse("ctrl+f24"), Ok(chord(CTRL, KeySym::Function(24))));
        assert_eq!(parse("f25"), Err(ParseError::UnknownKey("f25".to_string())));
        assert_eq!(parse("f0"), Err(ParseError::UnknownKey("f0".to_string())));
        // Bare 'f' is the letter, not a truncated function key.
        assert_eq!(parse("ctrl+f"), Ok(chord(CTRL, KeySym::Char('f'))));
    }

    #[test]
    fn digits_parse_to_main_row_chars() {
        // Family convention: numpad keysyms fold onto the main row in the
        // frontend, so the chord core only ever sees plain digit chars.
        for d in '0'..='9' {
            assert_eq!(
                parse(&format!("ctrl+{d}")),
                Ok(chord(CTRL, KeySym::Char(d)))
            );
        }
    }

    #[test]
    fn single_chars_store_unicode_lowercase() {
        assert_eq!(parse("Ctrl+T"), Ok(chord(CTRL, KeySym::Char('t'))));
        assert_eq!(parse("Ctrl+Ä"), Ok(chord(CTRL, KeySym::Char('ä'))));
        assert_eq!(parse("ctrl+ä"), Ok(chord(CTRL, KeySym::Char('ä'))));
        // 'İ' lowercases to two chars ("i\u{307}") — rejected, not stored
        // as something no key event could ever match.
        assert_eq!(
            parse("ctrl+İ"),
            Err(ParseError::UnknownKey("İ".to_string()))
        );
    }

    #[test]
    fn duplicate_modifiers_are_rejected_even_via_aliases() {
        assert_eq!(
            parse("ctrl+ctrl+t"),
            Err(ParseError::DuplicateModifier("ctrl".to_string()))
        );
        assert_eq!(
            parse("ctrl+control+t"),
            Err(ParseError::DuplicateModifier("control".to_string()))
        );
        assert_eq!(
            parse("cmd+super+t"),
            Err(ParseError::DuplicateModifier("super".to_string()))
        );
    }

    #[test]
    fn modifiers_only_chords_are_rejected() {
        for s in ["ctrl", "shift", "super", "ctrl+shift", "ctrl+shift+alt"] {
            assert_eq!(parse(s), Err(ParseError::MissingKey), "{s}");
        }
    }

    #[test]
    fn empty_unknown_modifier_and_unknown_key_inputs_error() {
        assert_eq!(parse(""), Err(ParseError::Empty));
        assert_eq!(parse("   "), Err(ParseError::Empty));
        assert_eq!(
            parse("hyper+t"),
            Err(ParseError::UnknownModifier("hyper".to_string()))
        );
        assert_eq!(
            parse("ctrl+bogus"),
            Err(ParseError::UnknownKey("bogus".to_string()))
        );
        // Doubled separator mid-chord is an empty modifier, not a key.
        assert_eq!(
            parse("ctrl++t"),
            Err(ParseError::UnknownModifier(String::new()))
        );
    }

    // ---- display form -------------------------------------------------

    #[test]
    fn display_uses_the_family_spelling() {
        assert_eq!(parse("ctrl+return").unwrap().display(), "Ctrl+Enter");
        assert_eq!(parse("ctrl+enter").unwrap().display(), "Ctrl+Enter");
        assert_eq!(parse("ctrl+pageup").unwrap().display(), "Ctrl+PageUp");
        assert_eq!(parse("ctrl+pagedown").unwrap().display(), "Ctrl+PageDown");
        assert_eq!(parse("ctrl+shift+t").unwrap().display(), "Ctrl+Shift+T");
        assert_eq!(parse("f12").unwrap().display(), "F12");
        assert_eq!(parse("ctrl+f5").unwrap().display(), "Ctrl+F5");
    }

    #[test]
    fn display_renders_symbols_verbatim() {
        assert_eq!(parse("ctrl+=").unwrap().display(), "Ctrl+=");
        assert_eq!(parse("ctrl+-").unwrap().display(), "Ctrl+-");
        assert_eq!(parse("ctrl+shift+1").unwrap().display(), "Ctrl+Shift+1");
        assert_eq!(
            parse("ctrl+shift+exclam").unwrap().display(),
            "Ctrl+Shift+!"
        );
        assert_eq!(parse("ctrl+shift+plus").unwrap().display(), "Ctrl+Shift++");
    }

    #[test]
    fn display_and_canonical_order_modifiers_ctrl_shift_alt_super() {
        let c = parse("super+alt+shift+ctrl+a").unwrap();
        assert_eq!(c.display(), "Ctrl+Shift+Alt+Super+A");
        assert_eq!(c.canonical(), "ctrl+shift+alt+super+a");
    }

    // ---- canonical form -----------------------------------------------

    #[test]
    fn canonical_is_the_lowercase_storage_form() {
        assert_eq!(parse("Ctrl+Shift+T").unwrap().canonical(), "ctrl+shift+t");
        assert_eq!(parse("shift+ctrl+f").unwrap().canonical(), "ctrl+shift+f");
        assert_eq!(parse("Ctrl+Enter").unwrap().canonical(), "ctrl+return");
        assert_eq!(parse("Ctrl+PageUp").unwrap().canonical(), "ctrl+pageup");
        assert_eq!(parse("F12").unwrap().canonical(), "f12");
        assert_eq!(parse("Ctrl+=").unwrap().canonical(), "ctrl+=");
        assert_eq!(
            parse("ctrl+shift+plus").unwrap().canonical(),
            "ctrl+shift++"
        );
        assert_eq!(parse("Ctrl+\\").unwrap().canonical(), "ctrl+backslash");
    }

    // ---- unbind tokens ------------------------------------------------

    #[test]
    fn unbind_tokens_are_recognized_case_insensitively() {
        for s in [
            "", "  ", "false", "none", "disabled", "unbind", "False", "NONE", "Disabled", "UNBIND",
            " none ",
        ] {
            assert!(is_unbind_token(s), "{s:?} should unbind");
        }
        for s in ["ctrl+c", "no", "off", "0", "nonee", "disable"] {
            assert!(!is_unbind_token(s), "{s:?} should NOT unbind");
        }
    }
}
