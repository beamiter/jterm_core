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
/// Two ranges are kept wider than Unicode's current default-ignorable
/// assignments: the unassigned specials `FFF0..=FFF8` and the entire
/// supplementary tag plane `E0000..=E0FFF` (rather than the enumerated tag
/// characters). A future format assignment inside a reserved range must fail
/// closed without waiting for a release, while the assigned interlinear
/// annotation anchors (`FFF9..=FFFB`) and Egyptian layout controls
/// (`U+13430` onward) stay allowed because Unicode does not classify them as
/// default-ignorable.
pub fn is_visual_spoofing_character(ch: char) -> bool {
    (ch.is_whitespace() && ch != ' ')
        || matches!(
            ch,
            // Unicode default-ignorable and bidi formatting code points that can
            // make reviewed shell text display differently from the bytes the
            // child receives. This boundary intentionally errs on the strict side:
            // a command can spell these explicitly (for example with printf) when
            // they are genuinely data.
            '\u{00ad}'
                | '\u{034f}'
                | '\u{061c}'
                | '\u{115f}'..='\u{1160}'
                | '\u{17b4}'..='\u{17b5}'
                | '\u{180b}'..='\u{180f}'
                | '\u{200b}'..='\u{200f}'
                | '\u{2028}'..='\u{202e}'
                | '\u{2060}'..='\u{206f}'
                | '\u{3164}'
                | '\u{fe00}'..='\u{fe0f}'
                | '\u{feff}'
                | '\u{ffa0}'
                | '\u{fff0}'..='\u{fff8}'
                | '\u{1bca0}'..='\u{1bca3}'
                | '\u{1d173}'..='\u{1d17a}'
                | '\u{e0000}'..='\u{e0fff}'
        )
}

/// Whether a scalar is ambiguous specifically on a terminal-rendered surface.
///
/// U+FFF9..=U+FFFB are assigned interlinear annotation controls, so the
/// generic review contract above deliberately does not call them Unicode
/// default-ignorables. Terminal renderers used by the family nevertheless
/// display them as nothing. OSC chrome, notifications, shell metadata and cwd
/// correlation therefore use this stricter predicate while review-only APIs
/// retain their established contract.
pub(crate) fn is_terminal_visual_spoofing_character(ch: char) -> bool {
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
