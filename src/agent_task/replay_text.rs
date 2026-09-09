//! Sanitizing a replayed command before it authorizes work.
//!
//! Lifted here with the rest of the subsystem: each app had its own copy in a
//! `review_text` module, and the budget those copies applied had already
//! drifted — the same call site meant 64 KiB in three apps and 256 KiB in the
//! fourth, at a boundary that decides what an agent may re-run.
//!
//! The budget now comes from the writer that owns it. `execution_journal` is
//! what produces an OSC 133 replay record, so its cap is the one a replayed
//! command is measured against; the wider `review_input` limit governs a
//! different channel and is deliberately not used here.

use std::fmt;

/// The replay channel's own budget, read from the journal that writes it.
pub const MAX_REPLAY_COMMAND_BYTES: usize = crate::execution_journal::MAX_COMMAND_BYTES;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplayTextError {
    Empty,
    TooLarge { limit: usize },
    ControlCharacter,
    VisualSpoof,
}

impl fmt::Display for ReplayTextError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("the command is empty"),
            Self::TooLarge { limit } => {
                write!(
                    formatter,
                    "the command exceeds the {limit}-byte replay limit"
                )
            }
            Self::ControlCharacter => {
                formatter.write_str("the command contains a NUL or terminal control character")
            }
            Self::VisualSpoof => formatter.write_str(
                "the command contains an invisible or bidirectional formatting character",
            ),
        }
    }
}

fn is_c0_or_c1(character: char) -> bool {
    matches!(character as u32, 0x00..=0x1f | 0x7f..=0x9f)
}

/// Normalize a command reconstructed from a replay record.
///
/// LF and tab survive because a replayed command may legitimately be
/// multi-line; CR and CRLF fold to LF; every other C0/C1 control is dropped.
/// A character that could visually misrepresent what will run is refused
/// outright rather than stripped — silently changing what a human approved is
/// worse than declining to offer it.
pub fn sanitize_history_replay(text: &str, max_bytes: usize) -> Result<String, ReplayTextError> {
    if text.len() > max_bytes {
        return Err(ReplayTextError::TooLarge { limit: max_bytes });
    }
    let mut sanitized = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\r' => {
                if characters.peek() == Some(&'\n') {
                    characters.next();
                }
                sanitized.push('\n');
            }
            '\n' | '\t' => sanitized.push(character),
            control if is_c0_or_c1(control) => {}
            visual if crate::review_input::is_visual_spoofing_character(visual) => {
                return Err(ReplayTextError::VisualSpoof)
            }
            visible => sanitized.push(visible),
        }
    }
    if sanitized
        .trim_matches(|character| matches!(character, ' ' | '\n' | '\t'))
        .is_empty()
    {
        return Err(ReplayTextError::Empty);
    }
    Ok(sanitized)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_replay_budget_is_the_journals_not_the_review_boundarys() {
        assert_eq!(
            MAX_REPLAY_COMMAND_BYTES,
            crate::execution_journal::MAX_COMMAND_BYTES
        );
        // Deliberately const-evaluable: the point is that the two budgets are
        // different numbers, and a future edit that collapses them should stop
        // compiling here rather than quietly widen what a replay may authorize.
        const _: () = assert!(
            MAX_REPLAY_COMMAND_BYTES < crate::review_input::MAX_REVIEW_INPUT_BYTES,
            "the replay budget must stay narrower than the review boundary's"
        );
    }

    #[test]
    fn line_structure_survives_and_controls_do_not() {
        assert_eq!(
            sanitize_history_replay("a\r\nb\tc\u{1}d", MAX_REPLAY_COMMAND_BYTES).unwrap(),
            "a\nb\tcd"
        );
    }

    #[test]
    fn a_visual_spoof_is_refused_rather_than_silently_rewritten() {
        assert_eq!(
            sanitize_history_replay("echo \u{202e}hi", MAX_REPLAY_COMMAND_BYTES),
            Err(ReplayTextError::VisualSpoof)
        );
    }

    #[test]
    fn blank_and_oversized_commands_are_refused() {
        assert_eq!(
            sanitize_history_replay("   \t\n ", MAX_REPLAY_COMMAND_BYTES),
            Err(ReplayTextError::Empty)
        );
        assert_eq!(
            sanitize_history_replay(&"x".repeat(9), 8),
            Err(ReplayTextError::TooLarge { limit: 8 })
        );
    }
}
