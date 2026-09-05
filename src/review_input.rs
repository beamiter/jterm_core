//! Safety boundary for text inserted into a live shell editor for review.
//!
//! Review-first surfaces promise not to submit a command. A carriage return,
//! line feed, NUL, escape, or other control character would break that promise
//! when written to a PTY, so every such surface shares this validator.

use std::fmt;

/// Review-only insertion is not a bulk-transfer API. Keeping one command
/// bounded also caps the allocation each frontend makes for its PTY message.
pub const MAX_REVIEW_INPUT_BYTES: usize = 256 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewInputError {
    Empty,
    TooLarge,
    ControlCharacter,
    VisualSpoof,
}

impl fmt::Display for ReviewInputError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("the command is empty"),
            Self::TooLarge => write!(
                formatter,
                "the command exceeds the {MAX_REVIEW_INPUT_BYTES}-byte review limit"
            ),
            Self::ControlCharacter => formatter
                .write_str("the command contains a line break, NUL, or terminal control character"),
            Self::VisualSpoof => formatter.write_str(
                "the command contains an invisible or bidirectional formatting character",
            ),
        }
    }
}

/// Validate text before inserting it into an interactive prompt without Enter.
pub fn validate(text: &str) -> Result<&str, ReviewInputError> {
    if text.len() > MAX_REVIEW_INPUT_BYTES {
        return Err(ReviewInputError::TooLarge);
    }
    if text.trim().is_empty() {
        return Err(ReviewInputError::Empty);
    }
    if text.chars().any(char::is_control) {
        return Err(ReviewInputError::ControlCharacter);
    }
    if contains_visual_spoofing(text) {
        return Err(ReviewInputError::VisualSpoof);
    }
    Ok(text)
}

/// Whether text contains Unicode formatting code points that can make a label
/// or reviewed command display differently from the bytes it represents.
pub fn contains_visual_spoofing(text: &str) -> bool {
    text.chars().any(is_visual_spoofing_character)
}

/// Whether text contains visual-spoofing Unicode that is not itself a control.
///
/// Multiline PTY encoders treat `\n` and `\t` as structural controls and
/// account for their execution risk separately. Since every non-ASCII
/// whitespace character is otherwise classified as visually ambiguous, using
/// [`contains_visual_spoofing`] on that normalized body would accidentally
/// reject every legitimate multiline insertion solely because it contains a
/// newline. This narrower predicate preserves that structure while continuing
/// to reject bidi controls, default-ignorables, non-breaking spaces, and line
/// separators.
pub fn contains_noncontrol_visual_spoofing(text: &str) -> bool {
    text.chars()
        .any(|ch| !ch.is_control() && is_visual_spoofing_character(ch))
}

/// Render untrusted text for one display line without letting controls, bidi
/// marks, or default-ignorable characters alter the surrounding UI. The
/// returned value is always valid UTF-8 and never exceeds `max_bytes`.
pub fn safe_inline_display(text: &str, max_bytes: usize) -> String {
    safe_display(text, max_bytes, false)
}

/// Display a command or document fragment while preserving only structural
/// newline/tab controls. All other terminal and visual formatting controls are
/// made explicit as replacement glyphs.
pub fn safe_multiline_display(text: &str, max_bytes: usize) -> String {
    safe_display(text, max_bytes, true)
}

fn safe_display(text: &str, max_bytes: usize, multiline: bool) -> String {
    if max_bytes == 0 {
        return String::new();
    }
    let mut output = String::with_capacity(text.len().min(max_bytes));
    let mut truncated = false;
    for ch in text.chars() {
        let rendered = if multiline && matches!(ch, '\n' | '\t') {
            ch
        } else if ch.is_control() || is_visual_spoofing_character(ch) {
            '\u{fffd}'
        } else {
            ch
        };
        if output.len().saturating_add(rendered.len_utf8()) > max_bytes {
            truncated = true;
            break;
        }
        output.push(rendered);
    }
    if truncated && max_bytes >= '…'.len_utf8() {
        while output.len().saturating_add('…'.len_utf8()) > max_bytes {
            if output.pop().is_none() {
                break;
            }
        }
        output.push('…');
    }
    output
}

/// Whether one Unicode scalar is unsafe in text whose displayed spelling is a
/// security boundary. Prefer [`contains_visual_spoofing`] for whole strings.
///
/// This is `jagent::is_unsafe_invisible_char` under the family's historical
/// name, not a second opinion about it. The two tables used to be identical
/// copies with nothing binding them, which is the failure jagent's own doc
/// names: "a copy stops widening the day this one does, and nothing fails
/// until a reply aims at the difference". jagent is the crate every model
/// reply crosses before becoming a proposal, and it promises this set may be
/// widened and never narrowed, so core delegating to it means a code point
/// jagent starts refusing is refused here in the same release — across all
/// four apps and every core surface that renders untrusted text, not just the
/// agent path.
///
/// Two ranges are wider than Unicode's current default-ignorable assignments:
/// the unassigned specials `FFF0..=FFF8` and the entire supplementary tag
/// plane `E0000..=E0FFF` (rather than the enumerated tag characters). A future
/// format assignment inside a reserved range must fail closed without waiting
/// for a release, while the assigned interlinear annotation anchors
/// (`FFF9..=FFFB`) and Egyptian layout controls (`U+13430` onward) stay
/// allowed because Unicode does not classify them as default-ignorable.
/// Terminal-rendered surfaces need the annotation anchors refused as well;
/// that is [`is_terminal_visual_spoofing_character`], deliberately kept as a
/// separate strict superset rather than pushed into this shared contract.
pub fn is_visual_spoofing_character(ch: char) -> bool {
    jagent::is_unsafe_invisible_char(ch)
}

/// Whether a scalar is ambiguous specifically on a terminal-rendered surface.
///
/// The contract: everything [`is_visual_spoofing_character`] refuses, plus the
/// assigned interlinear annotation controls `U+FFF9..=U+FFFB`. That is the one
/// and only difference, and it is a strict superset — never a different
/// opinion about a code point the shared rule allows.
///
/// U+FFF9..=U+FFFB carry a Unicode general category that is not
/// Default_Ignorable_Code_Point, so the shared review contract, which core now
/// takes from `jagent`, deliberately allows them. Every terminal this family
/// renders in nevertheless draws them as nothing, so on a terminal surface
/// they hide text exactly the way a zero-width space does. Anything whose
/// displayed spelling is correlated with a shell's own state — an OSC 133
/// cwd or command, notification chrome, journal metadata — must use this
/// predicate; ember and frost could not, because it was `pub(crate)` while
/// core's parser had already begun enforcing it on the OSC 133 text those two
/// apps read back.
pub fn is_terminal_visual_spoofing_character(ch: char) -> bool {
    is_visual_spoofing_character(ch) || matches!(ch, '\u{fff9}'..='\u{fffb}')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_single_line_shell_text_and_unicode() {
        assert_eq!(
            validate("printf '%s' '你好 🙂'").unwrap(),
            "printf '%s' '你好 🙂'"
        );
    }

    #[test]
    fn rejects_empty_and_every_pty_control_vector() {
        assert_eq!(validate("  "), Err(ReviewInputError::Empty));
        for unsafe_text in [
            "echo one\necho two",
            "echo one\recho two",
            "printf '\0'",
            "echo\tsecret",
            "echo \u{1b}[31mred",
        ] {
            assert_eq!(
                validate(unsafe_text),
                Err(ReviewInputError::ControlCharacter),
                "{unsafe_text:?} must never be written through a review-only path"
            );
        }
    }

    #[test]
    fn rejects_oversize_and_visual_spoofing() {
        assert_eq!(
            validate(&"x".repeat(MAX_REVIEW_INPUT_BYTES + 1)),
            Err(ReviewInputError::TooLarge)
        );
        for unsafe_text in [
            "echo soft\u{00ad}hyphen",
            "echo\u{00a0}not-a-shell-separator",
            "echo\u{2003}not-a-shell-separator",
            "echo safe\u{061c}hidden",
            "echo safe\u{200b}hidden",
            "echo safe\u{202e}txt",
            "echo safe\u{2066}hidden",
            "echo emoji\u{fe0f}",
            "echo tag\u{e0020}hidden",
            "echo reserved\u{fff0}mark",
            "echo tag\u{e0080}mark",
            "echo safe\u{feff}hidden",
        ] {
            assert_eq!(validate(unsafe_text), Err(ReviewInputError::VisualSpoof));
        }
    }

    /// Reserved ranges fail closed so a future format assignment needs no
    /// release lag, while assigned layout and annotation controls Unicode
    /// keeps outside Default_Ignorable_Code_Point stay allowed.
    #[test]
    fn reserved_ranges_fail_closed_while_assigned_layout_controls_stay_allowed() {
        for annotation in ['\u{fff9}', '\u{fffa}', '\u{fffb}'] {
            assert!(!is_visual_spoofing_character(annotation));
            assert!(is_terminal_visual_spoofing_character(annotation));
        }
        assert!(!is_visual_spoofing_character('\u{13430}'));
        assert!(!is_terminal_visual_spoofing_character('\u{13430}'));
    }

    /// One character from every class the two predicates disagree about or
    /// agree about, asserted from BOTH sides.
    ///
    /// The shared rule must be jagent's answer, not a copy of it that happens
    /// to match today, and the terminal rule must be a strict superset of the
    /// shared one — the exact contract ember and frost now build on. A jagent
    /// widening therefore lands here for free, and a jagent narrowing (which
    /// its own doc forbids) fails this suite instead of quietly reopening
    /// every core surface that renders untrusted text.
    #[test]
    fn the_shared_rule_is_jagents_and_the_terminal_rule_is_its_strict_superset() {
        // Exhaustive over every Unicode scalar, not a sample. A sampled table
        // cannot detect the thing this test exists to detect: a future edit
        // that re-inlines a copy of jagent's ranges here, which would agree on
        // whatever characters the table happened to list and diverge on the
        // one it did not.
        //
        // The first assertion is trivially true while the body of
        // `is_visual_spoofing_character` is a delegate — that is the point, and
        // it is what makes a re-inlined copy fail here instead of in
        // production. The second is the real content: the terminal rule has its
        // own implementation, and the contract says it is the shared rule plus
        // exactly `U+FFF9..=U+FFFB` — no more, and no different opinion about
        // anything the shared rule allows.
        const ANNOTATION_ANCHORS: std::ops::RangeInclusive<char> = '\u{fff9}'..='\u{fffb}';
        let mut extra = 0_u32;
        for scalar in 0..=0x10_FFFF_u32 {
            let Some(ch) = char::from_u32(scalar) else {
                continue;
            };
            let shared = is_visual_spoofing_character(ch);
            assert_eq!(
                shared,
                jagent::is_unsafe_invisible_char(ch),
                "U+{scalar:04X} forks the shared contract"
            );
            assert_eq!(
                is_terminal_visual_spoofing_character(ch),
                shared || ANNOTATION_ANCHORS.contains(&ch),
                "U+{scalar:04X} is a difference the documented contract does not allow"
            );
            if is_terminal_visual_spoofing_character(ch) && !shared {
                extra += 1;
            }
        }
        assert_eq!(
            extra, 3,
            "the terminal rule must refuse exactly the three interlinear \
             annotation anchors beyond the shared rule"
        );
    }

    #[test]
    fn display_text_is_bounded_and_makes_terminal_metadata_unambiguous() {
        assert_eq!(
            safe_inline_display("safe\n\u{202e}\u{fe0f}name", 64),
            "safe\u{fffd}\u{fffd}\u{fffd}name"
        );
        assert_eq!(
            safe_multiline_display("safe\n\t\u{202e}\u{fff0}\u{e01f0}name", 64),
            "safe\n\t\u{fffd}\u{fffd}\u{fffd}name"
        );
        assert_eq!(safe_inline_display("abcdefgh", 6), "abc…");
        assert!(safe_inline_display(&"界".repeat(100), 32).len() <= 32);
        assert!(safe_inline_display("abc", 0).is_empty());
    }

    #[test]
    fn noncontrol_spoof_check_preserves_structural_multiline_controls() {
        assert!(!contains_noncontrol_visual_spoofing("one\ntwo\tthree"));
        for unsafe_text in [
            "one\u{00a0}two",
            "one\u{2028}two",
            "safe\u{202e}txt",
            "tag\u{e0020}hidden",
        ] {
            assert!(
                contains_noncontrol_visual_spoofing(unsafe_text),
                "{unsafe_text:?} must remain visually unsafe"
            );
        }
    }
}
