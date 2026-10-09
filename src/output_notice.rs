//! Shared finished-block output-loss notices for the jterm family.
//!
//! Frontends persist their own schema (`bool` or `Option<String>`), but the
//! display strings and the known-set restore gate must mean the same thing in
//! every terminal. Classification of *why* output was lost stays app-owned.

/// Head of the capture was dropped; oldest lines are gone.
pub const EARLIER_OUTPUT_NOT_RETAINED: &str = "Earlier output not retained";
/// Capture was truncated from the end.
pub const OUTPUT_TEXT_TRUNCATED: &str = "Output text truncated";
/// Both ends of the capture were lost.
pub const OUTPUT_PARTLY_RETAINED: &str = "Output only partly retained";

/// Typed finished-card output-loss notice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FinishedOutputNotice {
    EarlierNotRetained,
    TextTruncated,
    PartlyRetained,
}

impl FinishedOutputNotice {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::EarlierNotRetained => EARLIER_OUTPUT_NOT_RETAINED,
            Self::TextTruncated => OUTPUT_TEXT_TRUNCATED,
            Self::PartlyRetained => OUTPUT_PARTLY_RETAINED,
        }
    }

    pub const fn tooltip(self) -> &'static str {
        match self {
            Self::EarlierNotRetained => {
                "The command wrote more than a finished block keeps; its oldest output was dropped"
            }
            Self::TextTruncated => {
                "The command wrote more than a finished block keeps; the text stops before the end of its output"
            }
            Self::PartlyRetained => {
                "The command wrote more than a finished block keeps; both its oldest and its latest output were dropped"
            }
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            EARLIER_OUTPUT_NOT_RETAINED => Some(Self::EarlierNotRetained),
            OUTPUT_TEXT_TRUNCATED => Some(Self::TextTruncated),
            OUTPUT_PARTLY_RETAINED => Some(Self::PartlyRetained),
            _ => None,
        }
    }
}

/// The notice `text` names, when it is one of the finished-card notices.
/// History / undo restore paths show a notice only through this gate.
pub fn known_output_notice(text: &str) -> Option<&'static str> {
    FinishedOutputNotice::parse(text).map(FinishedOutputNotice::as_str)
}

/// Tooltip for a known notice string, if any.
pub fn output_notice_tooltip(notice: &str) -> Option<&'static str> {
    FinishedOutputNotice::parse(notice).map(FinishedOutputNotice::tooltip)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_set_accepts_the_three_family_notices_and_rejects_forged_text() {
        assert_eq!(
            known_output_notice(EARLIER_OUTPUT_NOT_RETAINED),
            Some(EARLIER_OUTPUT_NOT_RETAINED)
        );
        assert_eq!(
            known_output_notice(OUTPUT_TEXT_TRUNCATED),
            Some(OUTPUT_TEXT_TRUNCATED)
        );
        assert_eq!(
            known_output_notice(OUTPUT_PARTLY_RETAINED),
            Some(OUTPUT_PARTLY_RETAINED)
        );
        assert_eq!(known_output_notice("<b>forged</b>"), None);
        assert_eq!(
            output_notice_tooltip(EARLIER_OUTPUT_NOT_RETAINED),
            Some(FinishedOutputNotice::EarlierNotRetained.tooltip())
        );
        assert_eq!(output_notice_tooltip("<b>forged</b>"), None);
    }

    #[test]
    fn parse_as_str_and_tooltip_round_trip_every_variant_and_reject_blank() {
        for notice in [
            FinishedOutputNotice::EarlierNotRetained,
            FinishedOutputNotice::TextTruncated,
            FinishedOutputNotice::PartlyRetained,
        ] {
            let text = notice.as_str();
            assert_eq!(FinishedOutputNotice::parse(text), Some(notice));
            assert_eq!(known_output_notice(text), Some(text));
            assert_eq!(output_notice_tooltip(text), Some(notice.tooltip()));
            assert!(
                !notice.tooltip().is_empty(),
                "{notice:?} tooltip must stay non-empty"
            );
        }
        assert_eq!(FinishedOutputNotice::parse(""), None);
        assert_eq!(FinishedOutputNotice::parse(" "), None);
        assert_eq!(known_output_notice(""), None);
        assert_eq!(output_notice_tooltip(""), None);
    }

    /// Known-set restore must stay exact: padded / cased / truncated near-miss
    /// strings cannot resurrect a finished-card notice from hostile or drifted
    /// history bytes.
    #[test]
    fn known_set_rejects_near_miss_and_whitespace_padded_notice_strings() {
        for base in [
            EARLIER_OUTPUT_NOT_RETAINED,
            OUTPUT_TEXT_TRUNCATED,
            OUTPUT_PARTLY_RETAINED,
        ] {
            for candidate in [
                format!(" {base}"),
                format!("{base} "),
                format!("\t{base}"),
                format!("{base}\n"),
                base.to_ascii_uppercase(),
                base[..base.len().saturating_sub(1)].to_string(),
                format!("{base}!"),
            ] {
                assert_eq!(
                    FinishedOutputNotice::parse(&candidate),
                    None,
                    "near-miss {candidate:?} must stay outside the known set"
                );
                assert_eq!(known_output_notice(&candidate), None);
                assert_eq!(output_notice_tooltip(&candidate), None);
            }
        }
    }

    /// The three family notices must keep distinct display strings and
    /// tooltips so Truncated cannot silently render as Earlier (or share a
    /// tooltip) after a restore-gate round-trip.
    #[test]
    fn family_notice_strings_and_tooltips_stay_pairwise_distinct() {
        let variants = [
            FinishedOutputNotice::EarlierNotRetained,
            FinishedOutputNotice::TextTruncated,
            FinishedOutputNotice::PartlyRetained,
        ];
        assert_eq!(
            variants.len(),
            3,
            "finished-card known set is exactly three notices"
        );
        let mut texts: Vec<&str> = variants.iter().map(|n| n.as_str()).collect();
        let mut tips: Vec<&str> = variants.iter().map(|n| n.tooltip()).collect();
        texts.sort_unstable();
        tips.sort_unstable();
        texts.dedup();
        tips.dedup();
        assert_eq!(texts.len(), 3);
        assert_eq!(tips.len(), 3);
    }
}
