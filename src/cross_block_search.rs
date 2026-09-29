//! Shared cross-block palette search cursor, scan budget, and report helpers.
//!
//! Frontends own GTK idle scheduling and hit row schemas (`CrossBlockHit`
//! stays app-owned: both UIs may carry optional exit_code/duration_ms/cwd
//! palette chrome, but badge helpers and ActionRow wiring still differ). This
//! module holds the resume-cursor shape, generation-current predicate,
//! scan-budget type, options/scope enums, and the hit-generic report both UIs
//! already agree on, plus the CROSS_BLOCK_* / FIND_OVERLAY_* constants so
//! anvil and forge cannot drift.

use std::time::{Duration, Instant};

/// Wider than the live Find overlay: palette search is user-initiated and may
/// walk retained history, but must still fail visibly when the walk stops early.
pub const CROSS_BLOCK_SCAN_BYTE_LIMIT: usize = 8 * 1024 * 1024;
/// Per-idle-slice wall-clock cap for one cross-block palette walk.
pub const CROSS_BLOCK_SCAN_TIME_LIMIT: Duration = Duration::from_millis(48);
/// Regex compile size cap shared by anvil/forge cross-block pattern scans.
pub const CROSS_BLOCK_REGEX_SIZE_LIMIT: usize = 2 * 1024 * 1024;

/// Live Find overlay per-surface byte cap. Tighter than
/// [`CROSS_BLOCK_SCAN_BYTE_LIMIT`] because overlay scans run on every keystroke
/// against the active block, not a user-initiated palette walk.
pub const FIND_OVERLAY_SCAN_BYTE_LIMIT: usize = 4 * 1024 * 1024;
/// Live Find overlay wall-clock cap for one surface scan slice.
pub const FIND_OVERLAY_SCAN_TIME_LIMIT: Duration = Duration::from_millis(12);

/// Mid-record resume point for a pattern scan that stopped inside one record's
/// command or output lines. Metadata browse never sets this.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CrossBlockSearchMidRecord {
    /// True when the next line to examine is on the output surface.
    pub on_output: bool,
    /// Next line index within that surface (0-based).
    pub next_line: usize,
    /// Match-occurrence counter for the current surface, carried so VTE jump
    /// stepping stays aligned across idle slices.
    pub occurrence: usize,
}

/// Where a budget-stopped cross-block scan should resume on the next idle slice.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CrossBlockSearchCursor {
    /// Next record index in the current records list.
    pub record_index: usize,
    /// Present when the previous slice stopped mid-record on a pattern scan.
    pub mid: Option<CrossBlockSearchMidRecord>,
}

/// Whether a dialog idle continuation still owns the live search generation
/// and should apply another budget slice. Stale generations are dropped so a
/// newer keystroke / filter change cancels in-flight walks.
pub fn cross_block_search_continue_is_current(
    scheduled_generation: u64,
    live_generation: u64,
    has_resume: bool,
) -> bool {
    has_resume && scheduled_generation == live_generation
}

/// Pattern-matching toggles for a cross-block palette scan.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CrossBlockSearchOptions {
    pub case_sensitive: bool,
    pub regex: bool,
    pub whole_word: bool,
}

/// Text surfaces included in a cross-block scan. Scope is applied before the
/// hit cap so a command-heavy history cannot hide output-only results.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CrossBlockSearchScope {
    #[default]
    All,
    Command,
    Output,
}

impl CrossBlockSearchScope {
    pub fn includes_command(self) -> bool {
        matches!(self, Self::All | Self::Command)
    }

    pub fn includes_output(self) -> bool {
        matches!(self, Self::All | Self::Output)
    }

    pub fn from_index(index: u32) -> Self {
        match index {
            1 => Self::Command,
            2 => Self::Output,
            _ => Self::All,
        }
    }

    pub fn index(self) -> u32 {
        match self {
            Self::All => 0,
            Self::Command => 1,
            Self::Output => 2,
        }
    }

    pub fn cycled(self) -> Self {
        match self {
            Self::All => Self::Command,
            Self::Command => Self::Output,
            Self::Output => Self::All,
        }
    }
}

/// UTF-8-safe slice taken from a scan surface under a [`FindScanBudget`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanPrefix<'a> {
    pub text: &'a str,
    pub incomplete: bool,
}

/// Byte + wall-clock budget for one Find overlay or cross-block palette slice.
///
/// Live overlay uses [`Self::new`] ([`FIND_OVERLAY_SCAN_*`]); palette walks use
/// [`Self::for_cross_block`] ([`CROSS_BLOCK_SCAN_*`]). Fields are public so UI
/// tests can install tight synthetic caps without a separate test constructor.
#[derive(Clone, Debug)]
pub struct FindScanBudget {
    pub remaining_bytes: usize,
    pub started: Instant,
    pub time_limit: Duration,
}

impl FindScanBudget {
    /// Live Find overlay: tighter shared [`FIND_OVERLAY_SCAN_*`] caps.
    pub fn new() -> Self {
        Self {
            remaining_bytes: FIND_OVERLAY_SCAN_BYTE_LIMIT,
            started: Instant::now(),
            time_limit: FIND_OVERLAY_SCAN_TIME_LIMIT,
        }
    }

    /// Palette cross-block walks: wider shared [`CROSS_BLOCK_SCAN_*`] caps.
    pub fn for_cross_block() -> Self {
        Self {
            remaining_bytes: CROSS_BLOCK_SCAN_BYTE_LIMIT,
            started: Instant::now(),
            time_limit: CROSS_BLOCK_SCAN_TIME_LIMIT,
        }
    }

    pub fn exhausted(&self) -> bool {
        self.time_exhausted() || self.remaining_bytes == 0
    }

    pub fn take_prefix<'a>(&mut self, text: &'a str) -> ScanPrefix<'a> {
        if self.time_exhausted() || self.remaining_bytes == 0 {
            return ScanPrefix {
                text: "",
                incomplete: !text.is_empty(),
            };
        }
        let prefix = utf8_prefix(text, self.remaining_bytes);
        self.remaining_bytes = self.remaining_bytes.saturating_sub(prefix.len());
        ScanPrefix {
            text: prefix,
            incomplete: prefix.len() < text.len(),
        }
    }

    pub fn time_exhausted(&self) -> bool {
        self.started.elapsed() >= self.time_limit
    }

    pub fn remaining_bytes(&self) -> usize {
        self.remaining_bytes
    }

    pub fn consume_bytes(&mut self, bytes: usize) {
        self.remaining_bytes = self.remaining_bytes.saturating_sub(bytes);
    }
}

impl Default for FindScanBudget {
    fn default() -> Self {
        Self::new()
    }
}

/// Longest UTF-8 prefix of `text` that fits in `max_bytes` without splitting a
/// code point. Shared by Find overlay and cross-block palette scans.
pub fn utf8_prefix(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Outcome of a cross-block palette scan. `H` is the frontend hit row type
/// ([`CrossBlockHit`] differs across anvil/forge — see module docs).
///
/// `scan_incomplete` is true when the byte/time budget stopped the walk before
/// every eligible record was examined — distinct from hitting `max_hits`, which
/// is a result cap the status line already discloses as "(capped)". When
/// incomplete, `resume` names the next idle continuation point; it is always
/// `None` when the walk finished or stopped at the hit cap.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CrossBlockSearchReport<H> {
    pub hits: Vec<H>,
    pub scan_incomplete: bool,
    pub resume: Option<CrossBlockSearchCursor>,
}

impl<H> CrossBlockSearchReport<H> {
    pub fn finished(hits: Vec<H>) -> Self {
        Self {
            hits,
            scan_incomplete: false,
            resume: None,
        }
    }

    pub fn budget_stopped(hits: Vec<H>, resume: CrossBlockSearchCursor) -> Self {
        Self {
            hits,
            scan_incomplete: true,
            resume: Some(resume),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn continue_respects_search_generation() {
        assert!(cross_block_search_continue_is_current(7, 7, true));
        assert!(!cross_block_search_continue_is_current(7, 8, true));
        assert!(!cross_block_search_continue_is_current(7, 7, false));
    }

    /// Idle `pending_scan_continue` stores a SourceId only while a resume
    /// cursor remains. A finished walk (no resume) must Break even when the
    /// generation is still current, and a bumped generation must Break even
    /// with a resume still in hand — both edges cancel the pending source.
    #[test]
    fn continue_idle_resume_edges_drop_stale_or_finished_walks() {
        // Matching generation + resume: keep walking.
        assert!(cross_block_search_continue_is_current(0, 0, true));
        assert!(cross_block_search_continue_is_current(u64::MAX, u64::MAX, true));
        // wrapping_add bump (dialog schedule_rebuild) invalidates the slice.
        let scheduled = 0_u64;
        let live = scheduled.wrapping_add(1);
        assert!(!cross_block_search_continue_is_current(scheduled, live, true));
        // Same wrapping schedule bump at the u64 boundary (MAX→0).
        assert!(!cross_block_search_continue_is_current(u64::MAX, 0, true));
        // Finished last slice cleared the cursor before Break.
        assert!(!cross_block_search_continue_is_current(3, 3, false));
        // Stale generation without a resume is still not current.
        assert!(!cross_block_search_continue_is_current(1, 2, false));
        // Scheduled ahead of live (speculative gen / rewound live) must cancel
        // even with a resume still in hand — close bumps generation first, and
        // a slice captured after the bump must not walk against an older live.
        assert!(!cross_block_search_continue_is_current(5, 4, true));
        // Gen-0 finished walk (no resume) cancels the same way as any other
        // matching-generation empty cursor.
        assert!(!cross_block_search_continue_is_current(0, 0, false));
        // Wrapping MAX→0 bump with a finished cursor cancels like any empty resume.
        assert!(!cross_block_search_continue_is_current(u64::MAX, 0, false));
        // Finished walk at the wrap generation itself (MAX,MAX,no resume).
        assert!(!cross_block_search_continue_is_current(u64::MAX, u64::MAX, false));
        // Live wrapping ahead of scheduled (0 vs MAX) cancels even with a
        // resume still in hand — the reverse of the MAX→0 schedule bump.
        assert!(!cross_block_search_continue_is_current(0, u64::MAX, true));
        // Same reverse-wrap cancel without a resume still drops the idle slice.
        assert!(!cross_block_search_continue_is_current(0, u64::MAX, false));
        // Near-wrap bump (MAX-1→MAX) cancels with a resume still in hand — the
        // non-wrapping sibling of the MAX→0 schedule bump at the u64 boundary.
        let near_wrap = u64::MAX.wrapping_sub(1);
        assert!(cross_block_search_continue_is_current(
            near_wrap, near_wrap, true
        ));
        assert!(!cross_block_search_continue_is_current(
            near_wrap, u64::MAX, true
        ));
        // Same near-wrap bump with a finished cursor still drops the idle slice.
        assert!(!cross_block_search_continue_is_current(
            near_wrap, u64::MAX, false
        ));
        // Matching generation at MAX itself still keeps a live resume.
        assert!(cross_block_search_continue_is_current(
            u64::MAX, u64::MAX, true
        ));
        // Scheduled ahead at the near-wrap boundary (MAX vs MAX-1) cancels with
        // a resume — speculative gen / rewound live beside the MAX-1→MAX bump.
        assert!(!cross_block_search_continue_is_current(
            u64::MAX, near_wrap, true
        ));
        assert!(!cross_block_search_continue_is_current(
            u64::MAX, near_wrap, false
        ));
        // Finished walk at the near-wrap generation itself (MAX-1,MAX-1,no
        // resume) cancels like MAX,MAX finished — beside the MAX-1→MAX bump.
        assert!(!cross_block_search_continue_is_current(
            near_wrap, near_wrap, false
        ));
        // Near-near-wrap bump (MAX-2→MAX-1) cancels with a resume — one step
        // earlier than the MAX-1→MAX sibling, still away from the wrap.
        let near_near_wrap = near_wrap.wrapping_sub(1);
        assert!(cross_block_search_continue_is_current(
            near_near_wrap, near_near_wrap, true
        ));
        assert!(!cross_block_search_continue_is_current(
            near_near_wrap, near_wrap, true
        ));
        assert!(!cross_block_search_continue_is_current(
            near_near_wrap, near_wrap, false
        ));
        // Scheduled ahead at the near-near-wrap boundary (MAX-1 vs MAX-2).
        assert!(!cross_block_search_continue_is_current(
            near_wrap, near_near_wrap, true
        ));
        assert!(!cross_block_search_continue_is_current(
            near_wrap, near_near_wrap, false
        ));
        // Finished walk at the near-near-wrap generation itself.
        assert!(!cross_block_search_continue_is_current(
            near_near_wrap, near_near_wrap, false
        ));

        // Near-near-near-wrap bump (MAX-3→MAX-2) cancels with a resume — one
        // step earlier than the MAX-2→MAX-1 sibling.
        let near_near_near_wrap = near_near_wrap.wrapping_sub(1);
        assert!(cross_block_search_continue_is_current(
            near_near_near_wrap, near_near_near_wrap, true
        ));
        assert!(!cross_block_search_continue_is_current(
            near_near_near_wrap, near_near_wrap, true
        ));
        assert!(!cross_block_search_continue_is_current(
            near_near_near_wrap, near_near_wrap, false
        ));
        // Scheduled ahead at the near-near-near-wrap boundary (MAX-2 vs MAX-3).
        assert!(!cross_block_search_continue_is_current(
            near_near_wrap, near_near_near_wrap, true
        ));
        assert!(!cross_block_search_continue_is_current(
            near_near_wrap, near_near_near_wrap, false
        ));
        // Finished walk at the near-near-near-wrap generation itself.
        assert!(!cross_block_search_continue_is_current(
            near_near_near_wrap, near_near_near_wrap, false
        ));
        // Near-near-near-near-wrap bump (MAX-4→MAX-3) cancels with a resume —
        // one step earlier than the MAX-3→MAX-2 sibling.
        let near_near_near_near_wrap = near_near_near_wrap.wrapping_sub(1);
        assert!(cross_block_search_continue_is_current(
            near_near_near_near_wrap, near_near_near_near_wrap, true
        ));
        assert!(!cross_block_search_continue_is_current(
            near_near_near_near_wrap, near_near_near_wrap, true
        ));
        assert!(!cross_block_search_continue_is_current(
            near_near_near_near_wrap, near_near_near_wrap, false
        ));
        // Scheduled ahead at the near-near-near-near-wrap boundary (MAX-3 vs MAX-4).
        assert!(!cross_block_search_continue_is_current(
            near_near_near_wrap, near_near_near_near_wrap, true
        ));
        assert!(!cross_block_search_continue_is_current(
            near_near_near_wrap, near_near_near_near_wrap, false
        ));
        // Finished walk at the near-near-near-near-wrap generation itself.
        assert!(!cross_block_search_continue_is_current(
            near_near_near_near_wrap, near_near_near_near_wrap, false
        ));
        // Near-near-near-near-near-wrap bump (MAX-5→MAX-4) cancels with a
        // resume — one step earlier than the MAX-4→MAX-3 sibling.
        let near_near_near_near_near_wrap = near_near_near_near_wrap.wrapping_sub(1);
        assert!(cross_block_search_continue_is_current(
            near_near_near_near_near_wrap, near_near_near_near_near_wrap, true
        ));
        assert!(!cross_block_search_continue_is_current(
            near_near_near_near_near_wrap, near_near_near_near_wrap, true
        ));
        assert!(!cross_block_search_continue_is_current(
            near_near_near_near_near_wrap, near_near_near_near_wrap, false
        ));
        // Scheduled ahead at the near-near-near-near-near-wrap boundary
        // (MAX-4 vs MAX-5).
        assert!(!cross_block_search_continue_is_current(
            near_near_near_near_wrap, near_near_near_near_near_wrap, true
        ));
        assert!(!cross_block_search_continue_is_current(
            near_near_near_near_wrap, near_near_near_near_near_wrap, false
        ));
        // Finished walk at the near-near-near-near-near-wrap generation itself.
        assert!(!cross_block_search_continue_is_current(
            near_near_near_near_near_wrap, near_near_near_near_near_wrap, false
        ));
        // Near-near-near-near-near-near-wrap bump (MAX-6→MAX-5) cancels with a
        // resume — one step earlier than the MAX-5→MAX-4 sibling.
        let near_near_near_near_near_near_wrap = near_near_near_near_near_wrap.wrapping_sub(1);
        assert!(cross_block_search_continue_is_current(
            near_near_near_near_near_near_wrap, near_near_near_near_near_near_wrap, true
        ));
        assert!(!cross_block_search_continue_is_current(
            near_near_near_near_near_near_wrap, near_near_near_near_near_wrap, true
        ));
        assert!(!cross_block_search_continue_is_current(
            near_near_near_near_near_near_wrap, near_near_near_near_near_wrap, false
        ));
        // Scheduled ahead at the near-near-near-near-near-near-wrap boundary
        // (MAX-5 vs MAX-6).
        assert!(!cross_block_search_continue_is_current(
            near_near_near_near_near_wrap, near_near_near_near_near_near_wrap, true
        ));
        assert!(!cross_block_search_continue_is_current(
            near_near_near_near_near_wrap, near_near_near_near_near_near_wrap, false
        ));
        // Finished walk at the near-near-near-near-near-near-wrap generation itself.
        assert!(!cross_block_search_continue_is_current(
            near_near_near_near_near_near_wrap, near_near_near_near_near_near_wrap, false
        ));

        // Near-near-near-near-near-near-near-wrap bump (MAX-7→MAX-6) cancels with a
        // resume — one step earlier than the MAX-6→MAX-5 sibling.
        let near_near_near_near_near_near_near_wrap = near_near_near_near_near_near_wrap.wrapping_sub(1);
        assert!(cross_block_search_continue_is_current(
            near_near_near_near_near_near_near_wrap, near_near_near_near_near_near_near_wrap, true
        ));
        assert!(!cross_block_search_continue_is_current(
            near_near_near_near_near_near_near_wrap, near_near_near_near_near_near_wrap, true
        ));
        assert!(!cross_block_search_continue_is_current(
            near_near_near_near_near_near_near_wrap, near_near_near_near_near_near_wrap, false
        ));
        // Scheduled ahead at the near-near-near-near-near-near-near-wrap boundary
        // (MAX-6 vs MAX-7).
        assert!(!cross_block_search_continue_is_current(
            near_near_near_near_near_near_wrap, near_near_near_near_near_near_near_wrap, true
        ));
        assert!(!cross_block_search_continue_is_current(
            near_near_near_near_near_near_wrap, near_near_near_near_near_near_near_wrap, false
        ));
        // Finished walk at the near-near-near-near-near-near-near-wrap generation itself.
        assert!(!cross_block_search_continue_is_current(
            near_near_near_near_near_near_near_wrap, near_near_near_near_near_near_near_wrap, false
        ));

        // Near-near-near-near-near-near-near-near-wrap bump (MAX-8→MAX-7) cancels with a
        // resume — one step earlier than the MAX-7→MAX-6 sibling.
        let near_near_near_near_near_near_near_near_wrap = near_near_near_near_near_near_near_wrap.wrapping_sub(1);
        assert!(cross_block_search_continue_is_current(
            near_near_near_near_near_near_near_near_wrap, near_near_near_near_near_near_near_near_wrap, true
        ));
        assert!(!cross_block_search_continue_is_current(
            near_near_near_near_near_near_near_near_wrap, near_near_near_near_near_near_near_wrap, true
        ));
        assert!(!cross_block_search_continue_is_current(
            near_near_near_near_near_near_near_near_wrap, near_near_near_near_near_near_near_wrap, false
        ));
        // Scheduled ahead at the near-near-near-near-near-near-near-near-wrap boundary
        // (MAX-7 vs MAX-8).
        assert!(!cross_block_search_continue_is_current(
            near_near_near_near_near_near_near_wrap, near_near_near_near_near_near_near_near_wrap, true
        ));
        assert!(!cross_block_search_continue_is_current(
            near_near_near_near_near_near_near_wrap, near_near_near_near_near_near_near_near_wrap, false
        ));
        // Finished walk at the near-near-near-near-near-near-near-near-wrap generation itself.
        assert!(!cross_block_search_continue_is_current(
            near_near_near_near_near_near_near_near_wrap, near_near_near_near_near_near_near_near_wrap, false
        ));

        // Near-near-near-near-near-near-near-near-near-wrap bump (MAX-9→MAX-8) cancels with a
        // resume — one step earlier than the MAX-8→MAX-7 sibling.
        let near_near_near_near_near_near_near_near_near_wrap = near_near_near_near_near_near_near_near_wrap.wrapping_sub(1);
        assert!(cross_block_search_continue_is_current(
            near_near_near_near_near_near_near_near_near_wrap, near_near_near_near_near_near_near_near_near_wrap, true
        ));
        assert!(!cross_block_search_continue_is_current(
            near_near_near_near_near_near_near_near_near_wrap, near_near_near_near_near_near_near_near_wrap, true
        ));
        assert!(!cross_block_search_continue_is_current(
            near_near_near_near_near_near_near_near_near_wrap, near_near_near_near_near_near_near_near_wrap, false
        ));
        // Scheduled ahead at the near-near-near-near-near-near-near-near-near-wrap boundary
        // (MAX-8 vs MAX-9).
        assert!(!cross_block_search_continue_is_current(
            near_near_near_near_near_near_near_near_wrap, near_near_near_near_near_near_near_near_near_wrap, true
        ));
        assert!(!cross_block_search_continue_is_current(
            near_near_near_near_near_near_near_near_wrap, near_near_near_near_near_near_near_near_near_wrap, false
        ));
        // Finished walk at the near-near-near-near-near-near-near-near-near-wrap generation itself.
        assert!(!cross_block_search_continue_is_current(
            near_near_near_near_near_near_near_near_near_wrap, near_near_near_near_near_near_near_near_near_wrap, false
        ));
    }

    #[test]
    fn mid_record_cursor_roundtrips_through_budget_stopped_report() {
        let mid = CrossBlockSearchMidRecord {
            on_output: false,
            next_line: 4,
            occurrence: 11,
        };
        let resume = CrossBlockSearchCursor {
            record_index: 2,
            mid: Some(mid.clone()),
        };
        let report = CrossBlockSearchReport::budget_stopped(vec!["hit"], resume.clone());
        assert!(report.scan_incomplete);
        let restored = report.resume.expect("budget-stopped walks keep a cursor");
        assert_eq!(restored.record_index, 2);
        assert_eq!(restored.mid.as_ref(), Some(&mid));
        // Metadata browse never sets mid; pattern mid must stay distinct.
        let browse = CrossBlockSearchCursor {
            record_index: 5,
            mid: None,
        };
        assert_ne!(browse, restored);
    }

    #[test]
    fn cross_block_budget_constants_match_the_family_contract() {
        assert_eq!(CROSS_BLOCK_SCAN_BYTE_LIMIT, 8 * 1024 * 1024);
        assert_eq!(CROSS_BLOCK_SCAN_TIME_LIMIT, Duration::from_millis(48));
        assert_eq!(CROSS_BLOCK_REGEX_SIZE_LIMIT, 2 * 1024 * 1024);
        assert_eq!(FIND_OVERLAY_SCAN_BYTE_LIMIT, 4 * 1024 * 1024);
        assert_eq!(FIND_OVERLAY_SCAN_TIME_LIMIT, Duration::from_millis(12));
        // Palette walks (`FindScanBudget::for_cross_block`) must keep the wider
        // CROSS_BLOCK_* caps; live overlay (`FindScanBudget::new`) stays on the
        // tighter FIND_OVERLAY_* pair — never interchange the two.
        assert!(CROSS_BLOCK_SCAN_BYTE_LIMIT > FIND_OVERLAY_SCAN_BYTE_LIMIT);
        assert!(CROSS_BLOCK_SCAN_TIME_LIMIT > FIND_OVERLAY_SCAN_TIME_LIMIT);
    }

    #[test]
    fn find_scan_budget_constructors_split_overlay_and_cross_block_caps() {
        let overlay = FindScanBudget::new();
        assert_eq!(overlay.remaining_bytes(), FIND_OVERLAY_SCAN_BYTE_LIMIT);
        assert_eq!(overlay.time_limit, FIND_OVERLAY_SCAN_TIME_LIMIT);

        let cross_block = FindScanBudget::for_cross_block();
        assert_eq!(cross_block.remaining_bytes(), CROSS_BLOCK_SCAN_BYTE_LIMIT);
        assert_eq!(cross_block.time_limit, CROSS_BLOCK_SCAN_TIME_LIMIT);
    }

    #[test]
    fn utf8_scan_prefix_never_splits_a_code_point() {
        assert_eq!(utf8_prefix("ab界cd", 4), "ab");
        assert_eq!(utf8_prefix("ab界cd", 5), "ab界");
        assert_eq!(utf8_prefix("ab界cd", usize::MAX), "ab界cd");
    }

    /// Zero-byte caps and a 3-byte code point under a 1–2 byte budget must
    /// yield empty prefixes, never a split `char`. Empty input stays empty.
    #[test]
    fn utf8_scan_prefix_zero_budget_and_partial_code_point_stay_empty() {
        assert_eq!(utf8_prefix("", 0), "");
        assert_eq!(utf8_prefix("abc", 0), "");
        assert_eq!(utf8_prefix("界", 0), "");
        assert_eq!(utf8_prefix("界", 1), "");
        assert_eq!(utf8_prefix("界", 2), "");
        assert_eq!(utf8_prefix("界", 3), "界");
    }

    #[test]
    fn aggregate_scan_budget_reports_an_incomplete_utf8_safe_prefix() {
        let mut budget = FindScanBudget {
            remaining_bytes: 5,
            started: Instant::now(),
            time_limit: Duration::from_secs(5),
        };
        let first = budget.take_prefix("abc");
        assert_eq!(first.text, "abc");
        assert!(!first.incomplete);

        let second = budget.take_prefix("界z");
        assert_eq!(second.text, "");
        assert!(second.incomplete);
        assert_eq!(budget.remaining_bytes(), 2);
    }

    #[test]
    fn cross_block_search_scope_cycles_all_command_output() {
        assert_eq!(CrossBlockSearchScope::All.cycled(), CrossBlockSearchScope::Command);
        assert_eq!(
            CrossBlockSearchScope::Command.cycled(),
            CrossBlockSearchScope::Output
        );
        assert_eq!(
            CrossBlockSearchScope::Output.cycled(),
            CrossBlockSearchScope::All
        );
        assert_eq!(CrossBlockSearchScope::from_index(0), CrossBlockSearchScope::All);
        assert_eq!(
            CrossBlockSearchScope::from_index(1),
            CrossBlockSearchScope::Command
        );
        assert_eq!(
            CrossBlockSearchScope::from_index(2),
            CrossBlockSearchScope::Output
        );
        assert_eq!(CrossBlockSearchScope::from_index(3), CrossBlockSearchScope::All);
        assert_eq!(
            CrossBlockSearchScope::from_index(u32::MAX),
            CrossBlockSearchScope::All
        );
        assert_eq!(CrossBlockSearchScope::All.index(), 0);
    }

    /// `Default` is the live overlay constructor, never the wider palette walk.
    #[test]
    fn find_scan_budget_default_is_the_overlay_constructor() {
        let defaulted = FindScanBudget::default();
        let overlay = FindScanBudget::new();
        assert_eq!(defaulted.remaining_bytes(), overlay.remaining_bytes());
        assert_eq!(defaulted.time_limit, overlay.time_limit);
        assert_eq!(defaulted.remaining_bytes(), FIND_OVERLAY_SCAN_BYTE_LIMIT);
        assert_ne!(defaulted.remaining_bytes(), CROSS_BLOCK_SCAN_BYTE_LIMIT);
    }

    #[test]
    fn cross_block_search_report_is_hit_generic() {
        // Hit rows stay app-owned (optional palette chrome may converge; GTK
        // badge wiring still differs). The shared report is therefore generic
        // over H so both UIs can reuse the scan_incomplete + resume contract
        // without lifting a shared CrossBlockHit into core.
        let finished = CrossBlockSearchReport::finished(vec!["a", "b"]);
        assert!(!finished.scan_incomplete);
        assert!(finished.resume.is_none());
        assert_eq!(finished.hits, ["a", "b"]);

        let resume = CrossBlockSearchCursor {
            record_index: 3,
            mid: Some(CrossBlockSearchMidRecord {
                on_output: true,
                next_line: 9,
                occurrence: 2,
            }),
        };
        let stopped = CrossBlockSearchReport::budget_stopped(vec![1u8], resume.clone());
        assert!(stopped.scan_incomplete);
        assert_eq!(stopped.resume, Some(resume));
        assert_eq!(stopped.hits, [1u8]);
    }

    /// finished() and budget_stopped() are the only constructors after the
    /// lift: a finished walk never carries a resume, and a budget stop always
    /// does. Pin both shapes across two distinct H types so a regression that
    /// re-specializes the report on one frontend's hit row fails closed.
    #[test]
    fn cross_block_search_report_constructors_preserve_incomplete_contract() {
        let done_str: CrossBlockSearchReport<&str> = CrossBlockSearchReport::finished(vec![]);
        assert!(!done_str.scan_incomplete);
        assert!(done_str.resume.is_none());
        assert!(done_str.hits.is_empty());

        let cursor = CrossBlockSearchCursor {
            record_index: 0,
            mid: None,
        };
        let paused_u32 = CrossBlockSearchReport::budget_stopped(vec![7u32, 8], cursor.clone());
        assert!(paused_u32.scan_incomplete);
        assert_eq!(paused_u32.resume, Some(cursor));
        assert_eq!(paused_u32.hits, [7, 8]);

        // A finished report with hits still discloses completeness honestly.
        let capped = CrossBlockSearchReport::finished(vec![(1u8, "x"), (2, "y")]);
        assert!(!capped.scan_incomplete);
        assert!(capped.resume.is_none());
        assert_eq!(capped.hits.len(), 2);
    }

    /// Hit rows stay app-owned even when both UIs carry the same optional
    /// palette-chrome columns (`exit_code` / `duration_ms` / `cwd`). Report is
    /// generic over `H` because GTK wiring, badge helpers, and ActionRow
    /// suffixes still differ — lifting a shared `CrossBlockHit` would drag
    /// those UI contracts into core. Navigation (6) + optional chrome (3) = 9.
    #[test]
    fn cross_block_hit_schema_keeps_rows_app_owned_with_optional_palette_chrome() {
        const SHARED_NAVIGATION_FIELDS: &[&str] = &[
            "block_id",
            "is_output",
            "line_no",
            "line_text",
            "cmd_preview",
            "occurrence",
        ];
        const OPTIONAL_PALETTE_CHROME_FIELDS: &[&str] = &["exit_code", "duration_ms", "cwd"];
        assert_eq!(SHARED_NAVIGATION_FIELDS.len(), 6);
        assert_eq!(OPTIONAL_PALETTE_CHROME_FIELDS.len(), 3);
        assert_eq!(
            SHARED_NAVIGATION_FIELDS.len() + OPTIONAL_PALETTE_CHROME_FIELDS.len(),
            9
        );
        // Optional columns must stay Option-shaped at the contract level so a
        // metadata-only or background hit can omit them without inventing
        // sentinel exit codes / empty cwd strings.
        for name in OPTIONAL_PALETTE_CHROME_FIELDS {
            assert!(
                matches!(*name, "exit_code" | "duration_ms" | "cwd"),
                "{name} is not an agreed optional palette-chrome column"
            );
        }
    }
}
