//! Family-wide classification for completed command blocks.
//!
//! Frontends own command records, persistence schemas, badges, colors, and
//! widgets. This module owns only the semantic boundary they all need before
//! making those choices: whether a completed block was background output, an
//! observed success, an observed failure, or a command whose exit status the
//! shell never reported.
//!
//! The distinction between [`CompletedBlockOutcome::Success`] and
//! [`CompletedBlockOutcome::Unknown`] is intentional. A bare `OSC 133;D` says
//! that a command ended, but it does not say that the command exited zero.

/// The semantic outcome of one completed command block.
///
/// This type deliberately contains no renderer or persistence details. In
/// particular, it does not derive `serde` traits: frontends keep ownership of
/// their existing on-disk schemas and translate this short-lived value into
/// their own UI state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompletedBlockOutcome {
    /// Output that belongs to no command because the frontend-resolved command
    /// text is absent or contains only Unicode whitespace. Any accompanying
    /// exit code is ignored.
    Background,
    /// A non-blank command for which the shell explicitly reported exit code 0.
    Success,
    /// A non-blank command for which the shell explicitly reported a non-zero
    /// exit code.
    Failed(i32),
    /// A non-blank command completed without a shell-reported exit code.
    Unknown,
}

impl CompletedBlockOutcome {
    /// Whether this outcome is an observed command failure.
    ///
    /// Background output and an unknown status are not failures: neither one
    /// carries evidence that a command exited non-zero.
    #[must_use]
    pub const fn is_failed(self) -> bool {
        matches!(self, Self::Failed(_))
    }

    /// The exit code the classified command actually reported, if any.
    ///
    /// Success returns `Some(0)` and failure preserves its exact code.
    /// Background output has no command-level status, while unknown means the
    /// shell supplied none; both return `None` rather than inventing a zero.
    #[must_use]
    pub const fn reported_exit_code(self) -> Option<i32> {
        match self {
            Self::Success => Some(0),
            Self::Failed(code) => Some(code),
            Self::Background | Self::Unknown => None,
        }
    }
}

/// Classify one completed block without interpreting or retaining its text.
///
/// An absent or Unicode-whitespace-only command is background output, and that
/// classification takes precedence over any supplied exit code. For a real
/// command, only an explicit zero is success, only an explicit non-zero is
/// failure, and an absent status remains unknown.
///
/// `resolved_command` is the frontend's final command text after applying any
/// protocol or screen fallback. A missing field in one metadata source is not,
/// by itself, proof that the completed block was background output.
///
/// This classifier does not validate command text. Controls, bidirectional
/// formatting, and other non-whitespace content still count as a command;
/// review and display sanitization remain separate frontend boundaries.
#[must_use]
pub fn classify_completed(
    resolved_command: Option<&str>,
    reported_exit_code: Option<i32>,
) -> CompletedBlockOutcome {
    if resolved_command.is_none_or(|command| command.trim().is_empty()) {
        return CompletedBlockOutcome::Background;
    }

    match reported_exit_code {
        Some(0) => CompletedBlockOutcome::Success,
        Some(code) => CompletedBlockOutcome::Failed(code),
        None => CompletedBlockOutcome::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::{classify_completed, CompletedBlockOutcome};

    #[derive(Clone, Copy, Debug)]
    struct GoldenCase {
        label: &'static str,
        command: Option<&'static str>,
        exit_code: Option<i32>,
        expected: CompletedBlockOutcome,
    }

    #[test]
    fn hostile_golden_cases_preserve_the_four_way_contract() {
        use CompletedBlockOutcome::{Background, Failed, Success, Unknown};

        let cases = [
            GoldenCase {
                label: "absent command trumps success",
                command: None,
                exit_code: Some(0),
                expected: Background,
            },
            GoldenCase {
                label: "absent command trumps failure",
                command: None,
                exit_code: Some(i32::MIN),
                expected: Background,
            },
            GoldenCase {
                label: "empty command",
                command: Some(""),
                exit_code: None,
                expected: Background,
            },
            GoldenCase {
                label: "mixed ASCII whitespace",
                command: Some(" \t\r\n"),
                exit_code: Some(127),
                expected: Background,
            },
            GoldenCase {
                label: "Unicode whitespace",
                command: Some("\u{00a0}\u{2003}\u{3000}"),
                exit_code: Some(-1),
                expected: Background,
            },
            GoldenCase {
                label: "explicit success",
                command: Some("cargo test"),
                exit_code: Some(0),
                expected: Success,
            },
            GoldenCase {
                label: "ordinary failure",
                command: Some("cargo test"),
                exit_code: Some(101),
                expected: Failed(101),
            },
            GoldenCase {
                label: "negative status remains a failure",
                command: Some("command"),
                exit_code: Some(i32::MIN),
                expected: Failed(i32::MIN),
            },
            GoldenCase {
                label: "largest status is preserved",
                command: Some("command"),
                exit_code: Some(i32::MAX),
                expected: Failed(i32::MAX),
            },
            GoldenCase {
                label: "unreported is not success",
                command: Some("false"),
                exit_code: None,
                expected: Unknown,
            },
            GoldenCase {
                label: "embedded newline is still command text",
                command: Some("printf ok\nfalse"),
                exit_code: None,
                expected: Unknown,
            },
            GoldenCase {
                label: "NUL is not whitespace",
                command: Some("\0"),
                exit_code: Some(9),
                expected: Failed(9),
            },
            GoldenCase {
                label: "bidi formatting is not whitespace",
                command: Some("\u{202e}"),
                exit_code: Some(0),
                expected: Success,
            },
            GoldenCase {
                label: "zero width formatting is not whitespace",
                command: Some("\u{200b}"),
                exit_code: None,
                expected: Unknown,
            },
        ];

        for case in cases {
            let actual = classify_completed(case.command, case.exit_code);
            assert_eq!(actual, case.expected, "{}", case.label);
            assert_eq!(
                actual.is_failed(),
                matches!(case.expected, Failed(_)),
                "{}",
                case.label
            );
            assert_eq!(
                actual.reported_exit_code(),
                match case.expected {
                    Success => Some(0),
                    Failed(code) => Some(code),
                    Background | Unknown => None,
                },
                "{}",
                case.label
            );
        }
    }

    #[test]
    fn property_matrix_only_a_nonblank_nonzero_command_is_failed() {
        let commands = [
            (None, false),
            (Some(""), false),
            (Some(" \t\n"), false),
            (Some("\u{1680}\u{205f}"), false),
            (Some("x"), true),
            (Some(" x "), true),
            (Some("\0"), true),
            (Some("\u{200b}"), true),
        ];
        let exit_codes = [
            None,
            Some(i32::MIN),
            Some(-255),
            Some(-1),
            Some(0),
            Some(1),
            Some(126),
            Some(127),
            Some(128),
            Some(130),
            Some(255),
            Some(i32::MAX),
        ];

        for (command, has_command) in commands {
            for exit_code in exit_codes {
                let outcome = classify_completed(command, exit_code);
                let should_fail = has_command && exit_code.is_some_and(|code| code != 0);
                let reported = has_command.then_some(exit_code).flatten();

                assert_eq!(
                    outcome.is_failed(),
                    should_fail,
                    "command={command:?}, exit_code={exit_code:?}, outcome={outcome:?}"
                );
                assert_eq!(
                    outcome.reported_exit_code(),
                    reported,
                    "command={command:?}, exit_code={exit_code:?}, outcome={outcome:?}"
                );
            }
        }
    }

    #[test]
    fn representative_nonzero_codes_round_trip_without_normalization() {
        let mut raw = 0x6d2b_79f5_u32;
        for _ in 0..4_096 {
            raw = raw.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let code = raw as i32;
            if code == 0 {
                continue;
            }
            let outcome = classify_completed(Some("command"), Some(code));
            assert_eq!(outcome, CompletedBlockOutcome::Failed(code));
            assert!(outcome.is_failed());
            assert_eq!(outcome.reported_exit_code(), Some(code));
        }
    }
}
