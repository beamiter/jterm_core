//! Shared cross-block palette search cursor and generation-cancel helpers.
//!
//! Frontends own GTK idle scheduling and hit row schemas. This module holds
//! the resume-cursor shape and the generation-current predicate both UIs
//! already agree on, plus the cross-block and live Find-overlay scan budget
//! constants so anvil and forge cannot drift.

use std::time::Duration;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn continue_respects_search_generation() {
        assert!(cross_block_search_continue_is_current(7, 7, true));
        assert!(!cross_block_search_continue_is_current(7, 8, true));
        assert!(!cross_block_search_continue_is_current(7, 7, false));
    }

    #[test]
    fn cross_block_budget_constants_match_the_family_contract() {
        assert_eq!(CROSS_BLOCK_SCAN_BYTE_LIMIT, 8 * 1024 * 1024);
        assert_eq!(CROSS_BLOCK_SCAN_TIME_LIMIT, Duration::from_millis(48));
        assert_eq!(CROSS_BLOCK_REGEX_SIZE_LIMIT, 2 * 1024 * 1024);
        assert_eq!(FIND_OVERLAY_SCAN_BYTE_LIMIT, 4 * 1024 * 1024);
        assert_eq!(FIND_OVERLAY_SCAN_TIME_LIMIT, Duration::from_millis(12));
    }
}
