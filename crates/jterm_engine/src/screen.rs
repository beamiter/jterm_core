//! Live VT screen ownership, row edits, region shifts and physical resizing.
//!
//! The host owns identity allocation, history, resize/scroll policy and
//! graphics. Public fields are an explicit transitional orchestration boundary,
//! not a mirrored screen or a Deref facade. Every row operation returns bounded
//! effects which the host must consume before the next protocol command.
pub use crate::grid::RegionDirection;
use crate::grid::{
    AsciiOverwritePolicy, Color, CursorShape, RawRowId, RegionWrapPolicy, ScrollbackLine,
    StyleFlags, TerminalCell, TerminalGrid, WideCellEditPolicy, WideContinuationProtection,
};
use crate::hyperlink::HyperlinkId;
use crate::protocol_state::ProtocolState;
use smallvec::SmallVec;
use std::ops::{Range, RangeInclusive};

/// Choose how many primary rows to move into history on a height reduction.
/// Use blank space below the cursor first and keep the cursor on screen.
pub fn rows_to_archive_on_height_shrink(
    old_rows: usize,
    new_rows: usize,
    cursor_row: usize,
    mut blank_row: impl FnMut(usize) -> bool,
) -> usize {
    let shrink = old_rows.saturating_sub(new_rows);
    let blank_below = (cursor_row.saturating_add(1)..old_rows)
        .rev()
        .take(shrink)
        .take_while(|&row| blank_row(row))
        .count();
    shrink.saturating_sub(blank_below).min(cursor_row)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Charset {
    #[default]
    Ascii,
    DecSpecialGraphics,
}

impl Charset {
    /// Map a graphic character under the selected DEC character set.
    pub fn translate(self, ch: char) -> char {
        match self {
            Charset::Ascii => ch,
            Charset::DecSpecialGraphics => match ch {
                '`' => '◆',
                'a' => '▒',
                'f' => '°',
                'g' => '±',
                'j' => '┘',
                'k' => '┐',
                'l' => '┌',
                'm' => '└',
                'n' => '┼',
                'o' => '⎺',
                'p' => '⎻',
                'q' => '─',
                'r' => '⎼',
                's' => '⎽',
                't' => '├',
                'u' => '┤',
                'v' => '┴',
                'w' => '┬',
                'x' => '│',
                'y' => '≤',
                'z' => '≥',
                '{' => 'π',
                '|' => '≠',
                '}' => '£',
                '~' => '·',
                _ => ch,
            },
        }
    }
}

/// DECSC/DECRC 保存的完整光标状态(含 SGR 属性、字符集、模式),
/// 与备用屏幕缓冲使用的 saved_cursor_row/col 解耦。
#[derive(Clone, Debug)]
pub struct SavedCursorState {
    pub row: usize,
    pub col: usize,
    pub fg: Color,
    pub bg: Color,
    pub flags: StyleFlags,
    pub g0: Charset,
    pub g1: Charset,
    pub active: Charset,
    pub origin_mode: bool,
    pub autowrap: bool,
    // VT510: DECSC 须保存 Last Column Flag(延迟换行标志),DECRC 须恢复。
    // 否则右 prompt(如 starship cmd_duration)写到末列后 pending_wrap=true,
    // 经 CSI u/ESC 8 恢复光标到 prompt 之后,下一字符(如 zsh-autosuggestions 的
    // ghost 文本)会立刻触发换行,在屏底触发滚动,光标看似下移一行。
    pub pending_wrap: bool,
}

/// A borrow of the active cursor, pen and character set. The DECSC slot and
/// the hidden primary screen both use the same snapshot representation.
pub struct CursorDrawing<'a> {
    pub position: (&'a mut usize, &'a mut usize),
    pub fg: &'a mut Color,
    pub bg: &'a mut Color,
    pub flags: &'a mut StyleFlags,
    pub g0: &'a mut Charset,
    pub g1: &'a mut Charset,
    pub active: &'a mut Charset,
    pub origin_mode: &'a mut bool,
    pub pending_wrap: &'a mut bool,
}

impl CursorDrawing<'_> {
    pub fn snapshot(&self, autowrap: bool) -> SavedCursorState {
        SavedCursorState {
            row: *self.position.0,
            col: *self.position.1,
            fg: *self.fg,
            bg: *self.bg,
            flags: *self.flags,
            g0: *self.g0,
            g1: *self.g1,
            active: *self.active,
            origin_mode: *self.origin_mode,
            autowrap,
            pending_wrap: *self.pending_wrap,
        }
    }

    fn restore_pen(&mut self, saved: &SavedCursorState) {
        *self.fg = saved.fg;
        *self.bg = saved.bg;
        *self.flags = saved.flags;
        *self.g0 = saved.g0;
        *self.g1 = saved.g1;
        *self.active = saved.active;
        *self.origin_mode = saved.origin_mode;
    }

    /// DECRC clamps saved coordinates to the current geometry. The caller owns
    /// protocol mode 7 and chooses whether to restore the snapshot's value.
    pub fn restore(&mut self, saved: Option<&SavedCursorState>, rows: usize, cols: usize) {
        if let Some(saved) = saved {
            *self.position.0 = saved.row.min(rows.saturating_sub(1));
            *self.position.1 = saved.col.min(cols.saturating_sub(1));
            self.restore_pen(saved);
            *self.pending_wrap = saved.pending_wrap;
        } else {
            *self.position.0 = 0;
            *self.position.1 = 0;
            *self.pending_wrap = false;
        }
    }
}

/// A completed, nonstructural edit of one physical row. `changed` includes
/// collateral wide-pair repairs; `printed` includes only cells actually written
/// by printable output. Erases and shifts must never claim output ownership.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowMutation {
    pub row: usize,
    pub row_id: RawRowId,
    pub changed: Range<usize>,
    pub printed: Option<Range<usize>>,
}

/// One printable-ASCII overwrite step, never an accumulated effect batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AsciiWriteStep {
    /// No bytes consumed. Complete host wrap/scroll/rebase before retrying.
    NeedsWrap,
    /// Apply this row mutation immediately, then advance by `consumed` bytes.
    Written {
        consumed: usize,
        mutation: RowMutation,
    },
    /// Empty input or the original row writer's no-progress result.
    Stopped,
}

/// Physical vertical operations. LF also covers VT, FF, IND and NEL's
/// second half. Wrap is admitted by the existing printable-output path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerticalMotion {
    LineFeed,
    ReverseIndex,
    Wrap,
}

/// Original departure coordinates, captured before any physical row shift.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VerticalDeparture {
    pub row_id: RawRowId,
    pub row: usize,
    pub width: usize,
}

/// One synchronous physical operation, not an event queue. Consume `region`
/// completely before hard-LF ownership or initial-wrap rebasing. RI has no
/// departure ownership notification. Host archive admission and all revisions,
/// graphics, history, viewport and output ownership remain outside the engine.
#[derive(Clone, Debug)]
pub struct VerticalMutation {
    pub departure: Option<VerticalDeparture>,
    pub region: Option<RegionMutation>,
}

/// A physical row removed by a completed region operation. The original identity
/// is retained even when untracked. Captured content belongs to this removed row,
/// not to a still-live snapshot. Capturing it does not decide to archive it.
#[derive(Clone, Debug)]
pub struct RemovedRow {
    pub original_row: usize,
    pub row_id: RawRowId,
    pub captured: Option<ScrollbackLine>,
}

/// Bounded structural effects, to be applied before another protocol operation.
/// No host revisions, graphics, history, selection or output ownership are
/// updated here. Retained row identities move with their cells; only vacancies
/// receive new IDs. No part of a region shift is printable output.
#[derive(Clone, Debug)]
pub struct RegionMutation {
    pub direction: RegionDirection,
    pub top: usize,
    pub bottom: usize,
    pub count: usize,
    pub removed: SmallVec<[RemovedRow; 1]>,
    pub dirty_rows: Range<usize>,
}

/// Completed physical resize. Only the host-selected top prefix is captured;
/// bottom rows truncated by either grid are discarded, not copied to history.
/// The transferred rows retain their original widths, wrap flags and identities.
/// Consume these effects before another operation; history admission/pinning,
/// graphics, cache invalidation, revisions and selection remain host concerns.
#[derive(Clone, Debug)]
pub struct ResizeMutation {
    pub old_rows: usize,
    pub old_cols: usize,
    pub rows: usize,
    pub cols: usize,
    pub transferred: SmallVec<[ScrollbackLine; 1]>,
    pub dirty_rows: Range<usize>,
}

/// Completed entry into the alternate screen. A clear is an in-place BCE fill,
/// not a row replacement: identities and wraps are unchanged. The host applies
/// its clear/provenance/dirty/graphics hooks only when this range is present.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AlternateEntry {
    pub cleared_rows: Option<Range<usize>>,
}

/// Completed return to the primary screen. Apply drawing effects after the
/// host's keyboard-stack switch and before dispatching another command.
/// Autowrap remains owned by ProtocolState; None retains its current mode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AlternateExit {
    pub global_background: Color,
    pub restore_autowrap: Option<bool>,
    pub dirty_rows: Range<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AlternateRestorePolicy {
    /// Preserve Ember's prior 47/1047 behavior and the 1049 cursor save.
    Standard { restore_position: bool },
    /// Frost historically restores position and the last-column latch for all
    /// three alternate modes, and keeps the live DECAWM protocol flag.
    Frost,
}

/// Live buffer pair with matching dimensions. The grid type includes any host
/// row metadata that accompanies the grid during the swap. History admission,
/// screen clearing, cursor shape, colors outside the cursor pen, hyperlinks and
/// keyboard state are applied by the caller in its original event order.
pub struct AlternateScreenControls<'a, Grid> {
    pub grid: &'a mut Grid,
    pub alt_grid: &'a mut Grid,
    pub use_alt_buffer: &'a mut bool,
    pub cursor: CursorDrawing<'a>,
    pub primary_position: (&'a mut usize, &'a mut usize),
    pub alternate_position: (&'a mut usize, &'a mut usize),
    pub saved_primary: &'a mut Option<SavedCursorState>,
    pub margins: (&'a mut usize, &'a mut usize),
    pub rows: usize,
}

impl<Grid> AlternateScreenControls<'_, Grid> {
    fn reset_margins(&mut self) {
        *self.margins.0 = 0;
        *self.margins.1 = self.rows.saturating_sub(1);
    }

    /// Record the primary cursor if requested and swap the actual grid pair.
    /// The alternate cursor slot records the pre-clear cursor at entry.
    pub fn enter(&mut self, save_cursor: bool, autowrap: bool) -> bool {
        if *self.use_alt_buffer {
            return false;
        }
        if save_cursor {
            *self.primary_position.0 = *self.cursor.position.0;
            *self.primary_position.1 = *self.cursor.position.1;
            *self.saved_primary = Some(self.cursor.snapshot(autowrap));
        }
        std::mem::swap(self.grid, self.alt_grid);
        *self.alternate_position.0 = *self.cursor.position.0;
        *self.alternate_position.1 = *self.cursor.position.1;
        *self.use_alt_buffer = true;
        self.reset_margins();
        true
    }

    /// A successful swap yields the optional DECAWM setting to apply after the
    /// host switches its protocol screen. No saved cursor is consumed on no-op.
    pub fn leave(&mut self, policy: AlternateRestorePolicy) -> Option<AlternateExit> {
        if !*self.use_alt_buffer {
            return None;
        }
        let frost = policy == AlternateRestorePolicy::Frost;
        *self.alternate_position.0 = *self.cursor.position.0;
        *self.alternate_position.1 = *self.cursor.position.1;
        std::mem::swap(self.grid, self.alt_grid);
        if frost
            || matches!(
                policy,
                AlternateRestorePolicy::Standard {
                    restore_position: true
                }
            )
        {
            *self.cursor.position.0 = *self.primary_position.0;
            *self.cursor.position.1 = *self.primary_position.1;
        }
        *self.use_alt_buffer = false;
        self.reset_margins();
        *self.cursor.pending_wrap = false;
        let restore_autowrap = if let Some(saved) = self.saved_primary.take() {
            self.cursor.restore_pen(&saved);
            *self.cursor.pending_wrap = frost && saved.pending_wrap;
            (!frost).then_some(saved.autowrap)
        } else {
            *self.cursor.fg = Color::Default;
            *self.cursor.bg = Color::Default;
            *self.cursor.flags = StyleFlags::default();
            if frost {
                *self.cursor.origin_mode = false;
            }
            None
        };
        Some(AlternateExit {
            global_background: *self.cursor.bg,
            restore_autowrap,
            dirty_rows: 0..self.rows,
        })
    }
}

/// Tab stops at columns 8, 16, and so on. The engine and Frost retain their
/// respective initial flag at column zero, which is also the backward fallback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabStopOrigin {
    Set,
    Clear,
}

pub fn default_tab_stops(cols: usize, origin: TabStopOrigin) -> Vec<bool> {
    let mut stops = Vec::new();
    resize_tab_stops(&mut stops, cols, origin);
    stops
}

/// Preserve customized stops in retained columns and initialize only new ones.
pub fn resize_tab_stops(stops: &mut Vec<bool>, cols: usize, origin: TabStopOrigin) {
    let old_len = stops.len();
    stops.resize(cols, false);
    for (col, stop) in stops.iter_mut().enumerate().skip(old_len) {
        *stop = col % 8 == 0 && (col != 0 || origin == TabStopOrigin::Set);
    }
}

fn next_tab_stop(stops: &[bool], cols: usize, col: usize) -> usize {
    let mut next = col.saturating_add(1);
    while next < cols {
        if stops.get(next).copied().unwrap_or(false) {
            return next;
        }
        next += 1;
    }
    cols.saturating_sub(1)
}

fn previous_tab_stop(stops: &[bool], cols: usize, col: usize) -> usize {
    let mut prev = col.min(cols);
    while prev > 0 {
        prev -= 1;
        if stops.get(prev).copied().unwrap_or(false) {
            return prev;
        }
    }
    0
}

/// Mutate the actual live cursor and margin state for VT controls. Both screen
/// hosts borrow their existing state here, so no cursor copy needs reconciling.
/// These operations never scroll physical rows and require a nonempty grid.
pub struct CursorControls<'a> {
    pub rows: usize,
    pub cols: usize,
    pub row: &'a mut usize,
    pub col: &'a mut usize,
    pub top: &'a mut usize,
    pub bottom: &'a mut usize,
    pub origin_mode: &'a mut bool,
    pub pending_wrap: &'a mut bool,
    pub tab_stops: &'a mut [bool],
}

impl CursorControls<'_> {
    pub fn move_up(&mut self, count: usize) {
        *self.pending_wrap = false;
        let limit = if *self.row >= *self.top { *self.top } else { 0 };
        *self.row = self.row.saturating_sub(count).max(limit);
    }

    pub fn move_down(&mut self, count: usize) {
        *self.pending_wrap = false;
        let limit = if *self.row <= *self.bottom {
            *self.bottom
        } else {
            self.rows - 1
        };
        *self.row = self.row.saturating_add(count).min(limit);
    }

    pub fn move_forward(&mut self, count: usize) {
        *self.pending_wrap = false;
        *self.col = self.col.saturating_add(count).min(self.cols - 1);
    }

    pub fn move_backward(&mut self, count: usize) {
        *self.pending_wrap = false;
        *self.col = self.col.saturating_sub(count);
    }

    pub fn set_column(&mut self, one_based: usize) {
        *self.pending_wrap = false;
        *self.col = one_based.saturating_sub(1).min(self.cols - 1);
    }

    pub fn set_row(&mut self, one_based: usize) {
        *self.pending_wrap = false;
        let row = one_based.saturating_sub(1);
        *self.row = if *self.origin_mode {
            self.top.saturating_add(row).min(*self.bottom)
        } else {
            row.min(self.rows - 1)
        };
    }

    pub fn set_position(&mut self, row: usize, col: usize) {
        self.set_row(row);
        self.set_column(col);
    }

    pub fn tab_forward(&mut self, count: usize) {
        *self.pending_wrap = false;
        for _ in 0..count.min(self.cols) {
            *self.col = next_tab_stop(self.tab_stops, self.cols, *self.col);
        }
    }

    pub fn tab_backward(&mut self, count: usize) {
        *self.pending_wrap = false;
        for _ in 0..count.min(self.cols) {
            *self.col = previous_tab_stop(self.tab_stops, self.cols, *self.col);
        }
    }

    pub fn set_tab_stop(&mut self, enabled: bool) {
        if let Some(stop) = self.tab_stops.get_mut(*self.col) {
            *stop = enabled;
        }
    }

    pub fn clear_all_tab_stops(&mut self) {
        self.tab_stops.fill(false);
    }

    /// DECSTBM uses the page limits when the requested bounds are reversed.
    /// The caller resolves default parameters before setting the margins.
    pub fn set_scroll_region(&mut self, top: usize, bottom: usize) {
        let last_row = self.rows - 1;
        *self.top = top.saturating_sub(1).min(last_row);
        *self.bottom = bottom.saturating_sub(1).min(last_row);
        if *self.top > *self.bottom {
            *self.top = 0;
            *self.bottom = last_row;
        }
        self.set_position(1, 1);
    }

    pub fn set_origin_mode(&mut self, enabled: bool) {
        *self.origin_mode = enabled;
        self.set_position(1, 1);
    }
}

/// Sole live owner of the active and inactive grids and their drawing state.
/// Autowrap is deliberately absent: ProtocolState mode 7 is authoritative.
///
/// During this transitional extraction the host may mutate fields for structural
/// operations. It must keep both grids nonempty, cursor positions in bounds and
/// margins valid before calling row operations, and immediately consume their
/// effects. Rows keep their identities throughout each nonstructural operation.
pub struct ScreenState<Grid = TerminalGrid> {
    pub grid: Grid,
    pub alt_grid: Grid,
    pub use_alt_buffer: bool,
    pub cursor_row: usize,
    pub cursor_col: usize,
    pub saved_cursor_row: usize,
    pub saved_cursor_col: usize,
    pub alt_cursor_row: usize,
    pub alt_cursor_col: usize,
    pub cursor_shape: CursorShape,
    pub saved_primary_cursor_shape: Option<CursorShape>,
    pub saved_state: Option<SavedCursorState>,
    pub saved_primary_screen_state: Option<SavedCursorState>,
    pub insert_mode: bool,
    pub last_printed_char: Option<char>,
    pub origin_mode: bool,
    pub tab_stops: Vec<bool>,
    pub pending_wrap: bool,
    pub current_fg: Color,
    pub current_bg: Color,
    pub current_flags: StyleFlags,
    pub current_hyperlink: HyperlinkId,
    pub scroll_region_top: usize,
    pub scroll_region_bottom: usize,
    pub g0_charset: Charset,
    pub g1_charset: Charset,
    pub active_charset: Charset,
}

impl<Grid: AsRef<TerminalGrid>> ScreenState<Grid> {
    /// Adopt the actual grid pair, including host row identity and metadata.
    ///
    /// # Panics
    /// Both grids must have matching, nonzero dimensions.
    pub fn with_grid_pair(grid: Grid, alt_grid: Grid, tab_stop_origin: TabStopOrigin) -> Self {
        let (rows, cols) = (grid.as_ref().rows(), grid.as_ref().cols());
        assert!(rows > 0 && cols > 0);
        assert_eq!(
            (rows, cols),
            (alt_grid.as_ref().rows(), alt_grid.as_ref().cols())
        );
        Self {
            grid,
            alt_grid,
            use_alt_buffer: false,
            cursor_row: 0,
            cursor_col: 0,
            saved_cursor_row: 0,
            saved_cursor_col: 0,
            alt_cursor_row: 0,
            alt_cursor_col: 0,
            cursor_shape: CursorShape::default(),
            saved_primary_cursor_shape: None,
            saved_state: None,
            saved_primary_screen_state: None,
            insert_mode: false,
            last_printed_char: None,
            origin_mode: false,
            tab_stops: default_tab_stops(cols, tab_stop_origin),
            pending_wrap: false,
            current_fg: Color::Default,
            current_bg: Color::Default,
            current_flags: StyleFlags::default(),
            current_hyperlink: HyperlinkId::NONE,
            scroll_region_top: 0,
            scroll_region_bottom: rows - 1,
            g0_charset: Charset::Ascii,
            g1_charset: Charset::Ascii,
            active_charset: Charset::Ascii,
        }
    }

    pub fn alternate_screen_controls(&mut self) -> AlternateScreenControls<'_, Grid> {
        let rows = self.grid.as_ref().rows();
        AlternateScreenControls {
            grid: &mut self.grid,
            alt_grid: &mut self.alt_grid,
            use_alt_buffer: &mut self.use_alt_buffer,
            cursor: CursorDrawing {
                position: (&mut self.cursor_row, &mut self.cursor_col),
                fg: &mut self.current_fg,
                bg: &mut self.current_bg,
                flags: &mut self.current_flags,
                g0: &mut self.g0_charset,
                g1: &mut self.g1_charset,
                active: &mut self.active_charset,
                origin_mode: &mut self.origin_mode,
                pending_wrap: &mut self.pending_wrap,
            },
            primary_position: (&mut self.saved_cursor_row, &mut self.saved_cursor_col),
            alternate_position: (&mut self.alt_cursor_row, &mut self.alt_cursor_col),
            saved_primary: &mut self.saved_primary_screen_state,
            margins: (&mut self.scroll_region_top, &mut self.scroll_region_bottom),
            rows,
        }
    }

    pub fn cursor_controls(&mut self) -> CursorControls<'_> {
        CursorControls {
            rows: self.grid.as_ref().rows(),
            cols: self.grid.as_ref().cols(),
            row: &mut self.cursor_row,
            col: &mut self.cursor_col,
            top: &mut self.scroll_region_top,
            bottom: &mut self.scroll_region_bottom,
            origin_mode: &mut self.origin_mode,
            pending_wrap: &mut self.pending_wrap,
            tab_stops: &mut self.tab_stops,
        }
    }

    pub fn save_cursor(&mut self, autowrap: bool) {
        self.saved_state = Some(self.alternate_screen_controls().cursor.snapshot(autowrap));
    }

    /// Restore the drawing state and report the saved autowrap value to the
    /// caller. The terminal profile decides whether DECRC restores that mode.
    pub fn restore_cursor(&mut self) -> Option<bool> {
        let saved = self.saved_state.clone();
        let (rows, cols) = (self.grid.as_ref().rows(), self.grid.as_ref().cols());
        self.alternate_screen_controls()
            .cursor
            .restore(saved.as_ref(), rows, cols);
        saved.map(|state| state.autowrap)
    }
}

impl ScreenState {
    /// NEL's first stage. The host must notify cursor reposition immediately
    /// after this call, BEFORE running LineFeed. Pending wrap is deliberately
    /// retained until that second stage; no action can span protocol events.
    pub fn begin_next_line(&mut self) {
        self.cursor_col = 0;
    }

    /// Apply the actual physical motion and (when needed) one region shift.
    /// `capture_outgoing` is host-selected archive eligibility, not a policy
    /// decision. The allocator contract is the same as `scroll_region_one`:
    /// exactly one call for a valid replacement and none for a no-op.
    ///
    /// Consume returned region effects synchronously in host order: identity,
    /// graphics, dirty, captured history. Only THEN compute LF's next boundary
    /// using the updated history counter, or rebase an initial autowrap. The
    /// host snapshots its history counter before calling for LF's departure.
    pub fn vertical_motion(
        &mut self,
        motion: VerticalMotion,
        capture_outgoing: bool,
        allocate_id: impl FnMut() -> RawRowId,
    ) -> VerticalMutation {
        self.vertical_motion_impl(motion, capture_outgoing, allocate_id, false)
    }

    /// Perform an early wrap and attach archive-only proof when it leaves a
    /// virgin margin in a full-height region. Opt-in callers must pass the
    /// width of the admitted printable after resolving deferred wrap and
    /// checking DECAWM via `prepare_character_width`. Consume returned region
    /// effects in order, then write that printable to the next row before
    /// handling another input event. Host history admission is unchanged.
    pub fn vertical_early_wide_wrap(
        &mut self,
        width: usize,
        capture_outgoing: bool,
        allocate_id: impl FnMut() -> RawRowId,
    ) -> VerticalMutation {
        let margin_candidate = width == 2 && !self.pending_wrap;
        self.vertical_motion_impl(
            VerticalMotion::Wrap,
            capture_outgoing,
            allocate_id,
            margin_candidate,
        )
    }

    /// Destination of a wrap and whether its row will be replaced by a blank.
    /// Used to admit a late graphic before committing its one real wrap.
    pub(crate) fn wrap_target(&self) -> (usize, bool) {
        if self.cursor_row == self.scroll_region_bottom {
            (self.cursor_row, self.wrap_scrolls_region())
        } else {
            ((self.cursor_row + 1).min(self.grid.rows() - 1), false)
        }
    }

    fn wrap_scrolls_region(&self) -> bool {
        self.cursor_row == self.scroll_region_bottom
            && self.scroll_region_top <= self.scroll_region_bottom
            && self.scroll_region_bottom < self.grid.rows()
    }

    fn vertical_motion_impl(
        &mut self,
        motion: VerticalMotion,
        capture_outgoing: bool,
        allocate_id: impl FnMut() -> RawRowId,
        margin_candidate: bool,
    ) -> VerticalMutation {
        if motion != VerticalMotion::Wrap {
            self.pending_wrap = false;
        }
        let departure = (motion != VerticalMotion::ReverseIndex).then(|| VerticalDeparture {
            row_id: self.grid.row_id(self.cursor_row),
            row: self.cursor_row,
            width: self.grid.row_len(),
        });
        if motion == VerticalMotion::Wrap {
            let next_row_exists = self.cursor_row + 1 < self.grid.rows();
            let scrolls = self.wrap_scrolls_region();
            let full_region = self.scroll_region_top == 0
                && self.scroll_region_bottom == self.grid.rows().saturating_sub(1);
            self.grid.record_wrap_margin(
                self.cursor_row,
                self.cursor_col,
                margin_candidate && full_region && (next_row_exists || scrolls),
            );
            self.grid.row_wrapped[self.cursor_row] = true;
            self.cursor_col = 0;
        }
        let region = if motion == VerticalMotion::ReverseIndex {
            if self.cursor_row == self.scroll_region_top {
                self.scroll_region_one(
                    RegionDirection::Down,
                    self.scroll_region_top,
                    self.scroll_region_bottom,
                    false,
                    allocate_id,
                )
            } else {
                self.cursor_row = self.cursor_row.saturating_sub(1);
                None
            }
        } else if self.cursor_row == self.scroll_region_bottom {
            self.scroll_region_one(
                RegionDirection::Up,
                self.scroll_region_top,
                self.scroll_region_bottom,
                capture_outgoing,
                allocate_id,
            )
        } else {
            if self.cursor_row + 1 < self.grid.rows() {
                self.cursor_row += 1;
            }
            None
        };
        VerticalMutation { departure, region }
    }

    /// ED2: fill every physical active cell with a BCE blank, including protected
    /// cells and wide pairs. This is not printable output or a row replacement:
    /// identities, row wraps, margins, cursor, drawing state and the inactive
    /// grid are retained. Host provenance must be invalidated before this call;
    /// dirty bookkeeping, history policy and graphics remain with the host.
    pub fn erase_screen(&mut self) {
        self.pending_wrap = false;
        let blank = self.create_blank_cell();
        self.grid.fill_cells(blank);
    }

    /// DECALN: fill the physical active grid with default-style E cells, reset
    /// margins and home the cursor. Unlike ED2 this deliberately does not use
    /// the current background. Origin mode and all other drawing/saved state,
    /// row metadata and the inactive grid are retained. The host invalidates
    /// provenance first and owns reposition notification and dirty bookkeeping.
    pub fn screen_alignment_test(&mut self) {
        self.grid.fill_cells(TerminalCell {
            character: 'E',
            ..TerminalCell::default()
        });
        self.scroll_region_top = 0;
        self.scroll_region_bottom = self.grid.rows().saturating_sub(1);
        self.cursor_row = 0;
        self.cursor_col = 0;
        self.pending_wrap = false;
    }

    /// Swap the actual grids and enter the alternate screen. DECSET 47 uses
    /// (false, false), 1047 uses (false, true), and 1049 uses (true, true).
    /// Repeated entry is a complete no-op, including saved slots and clears.
    ///
    /// DECSC is a single shared slot and is never overwritten here. The old
    /// alternate cursor is recorded but deliberately not loaded on entry.
    /// Clearing homes the live cursor without cancelling pending wrap, moving
    /// row IDs or resetting wrap flags. Host selection, history, graphics,
    /// keyboard stacks, watches, cache/revision and provenance hooks stay out.
    pub fn enter_alternate_screen(
        &mut self,
        save_cursor: bool,
        clear: bool,
        protocol: &ProtocolState,
    ) -> Option<AlternateEntry> {
        if !self
            .alternate_screen_controls()
            .enter(save_cursor, protocol.modes().contains(&7))
        {
            return None;
        }
        self.saved_primary_cursor_shape = Some(self.cursor_shape);
        self.current_hyperlink = HyperlinkId::NONE;
        let cleared_rows = clear.then(|| {
            let blank = self.create_blank_cell();
            self.grid.cells_mut().fill(blank);
            self.cursor_row = 0;
            self.cursor_col = 0;
            0..self.grid.rows()
        });
        Some(AlternateEntry { cleared_rows })
    }

    /// Swap the actual grids back to primary. DECRST 1049 restores separately
    /// saved cursor coordinates; 47/1047 do not. Any saved primary drawing state
    /// is restored independently of that flag, including saved autowrap via an
    /// effect. Without a snapshot only colors and style flags reset: charsets,
    /// origin and the current autowrap mode are retained. Shape is always restored
    /// if saved. Exit cancels pending wrap, even when the saved snapshot had it.
    /// Repeated exit is a complete no-op. No row identity is allocated or lost.
    pub fn leave_alternate_screen(&mut self, restore_cursor: bool) -> Option<AlternateExit> {
        let effect = self
            .alternate_screen_controls()
            .leave(AlternateRestorePolicy::Standard {
                restore_position: restore_cursor,
            })?;
        if let Some(shape) = self.saved_primary_cursor_shape.take() {
            self.cursor_shape = shape;
        }
        self.current_hyperlink = HyperlinkId::NONE;
        Some(effect)
    }

    /// Resize both actual grids without reflow. The host chooses how many top
    /// rows to transfer; this method does not decide primary/alternate/history
    /// policy. The count must fit the height reduction and must not transfer
    /// the live cursor row itself. Zero dimensions, unchanged geometry and invalid
    /// transfer counts are no-ops, returning None without allocating identities.
    ///
    /// Admission limits belong to the host, as for `new`. All final UNTRACKED
    /// rows receive IDs in active-then-hidden order. Compaction creates no
    /// temporary identities. The infallible allocator must return a fresh ID or
    /// UNTRACKED after exhaustion and must not publish host effects.
    ///
    /// Active growth/edge repair uses the current BCE background; hidden growth
    /// and repair use default blanks. DECSC coordinates are clipped but its
    /// saved wrap flag and pen are retained. The separate saved-primary state
    /// is deliberately untouched, matching existing alternate-screen semantics.
    pub fn resize_geometry(
        &mut self,
        rows: usize,
        cols: usize,
        transfer_top: usize,
        mut allocate_id: impl FnMut() -> RawRowId,
    ) -> Option<ResizeMutation> {
        let old_rows = self.grid.rows();
        let old_cols = self.grid.cols();
        if rows == 0
            || cols == 0
            || (rows == old_rows && cols == old_cols)
            || transfer_top > old_rows.saturating_sub(rows).min(self.cursor_row)
        {
            return None;
        }
        let had_full_screen_region =
            self.scroll_region_top == 0 && self.scroll_region_bottom >= old_rows.saturating_sub(1);
        let blank = self.create_blank_cell();
        let hidden_blank = TerminalCell::default();
        let preserve_margin =
            self.scroll_region_top == 0 && self.scroll_region_bottom == old_rows - 1;
        let transferred = (0..transfer_top)
            .map(|row| self.grid.archive_row(row, preserve_margin))
            .collect();
        self.grid.scroll_up_by(transfer_top, blank);
        self.cursor_row -= transfer_top;
        self.grid.resize(rows, cols, blank);
        self.alt_grid.resize(rows, cols, hidden_blank);
        for id in self
            .grid
            .row_ids_mut()
            .iter_mut()
            .chain(self.alt_grid.row_ids_mut())
        {
            if !id.is_tracked() {
                *id = allocate_id();
            }
        }
        // The legacy path repairs every real resize, including height-only
        // changes, not only column shrink. Keep that behavior for both grids.
        for row in 0..rows {
            self.grid.clear_dangling_wide_at_row_end(row, blank);
            self.alt_grid
                .clear_dangling_wide_at_row_end(row, hidden_blank);
        }
        self.pending_wrap = false;
        self.cursor_row = self.cursor_row.min(rows - 1);
        self.cursor_col = self.cursor_col.min(cols - 1);
        self.saved_cursor_row = self.saved_cursor_row.min(rows - 1);
        self.saved_cursor_col = self.saved_cursor_col.min(cols - 1);
        self.alt_cursor_row = self.alt_cursor_row.min(rows - 1);
        self.alt_cursor_col = self.alt_cursor_col.min(cols - 1);
        resize_tab_stops(&mut self.tab_stops, cols, TabStopOrigin::Set);
        if let Some(saved) = self.saved_state.as_mut() {
            saved.row = saved.row.saturating_sub(transfer_top).min(rows - 1);
            saved.col = saved.col.min(cols - 1);
        }
        if had_full_screen_region {
            self.scroll_region_top = 0;
            self.scroll_region_bottom = rows - 1;
        } else {
            self.scroll_region_top = self.scroll_region_top.min(rows - 1);
            self.scroll_region_bottom = self.scroll_region_bottom.min(rows - 1);
            if self.scroll_region_top > self.scroll_region_bottom {
                self.scroll_region_top = 0;
                self.scroll_region_bottom = rows - 1;
            }
        }
        Some(ResizeMutation {
            old_rows,
            old_cols,
            rows,
            cols,
            transferred,
            dirty_rows: 0..rows,
        })
    }

    /// Move one physical row through an inclusive region. Repeated SU/SD calls
    /// deliberately stay separate transactions so the host can apply history,
    /// graphics and revision effects after each row. Both cells and wrap flags
    /// move; the newly blank row has no wrap flag.
    ///
    /// `capture_outgoing` requests compression before the source is removed. It
    /// is not archive permission: primary/alternate/sync/CLI policy stays with
    /// the host. No full row allocation is required when content is discarded.
    /// The infallible allocator must return a fresh tracked ID or UNTRACKED.
    /// It is called exactly once after validating the
    /// region; it may return UNTRACKED after exhaustion and must not publish
    /// host effects. Invalid regions are no-ops and consume no identity.
    pub fn scroll_region_one(
        &mut self,
        direction: RegionDirection,
        top: usize,
        bottom: usize,
        capture_outgoing: bool,
        allocate_id: impl FnMut() -> RawRowId,
    ) -> Option<RegionMutation> {
        self.shift_region(
            direction,
            top..=bottom,
            1,
            RegionWrapPolicy::MoveWithRows,
            capture_outgoing,
            allocate_id,
        )
    }

    /// Batched IL/DL physical edits: Down inserts, Up deletes. The host passes
    /// cursor..=bottom only when the cursor is within its current margins.
    /// Counts are capped to the region height; zero/invalid requests are no-ops
    /// and do not invoke the allocator. No outgoing content is captured.
    ///
    /// Compatibility detail: Ember's existing IL/DL leave `row_wrapped` at its
    /// physical indices, unlike SU/SD. Preserve that behavior during extraction;
    /// changing it is a separate terminal-semantics change. Cursor, pending wrap
    /// and the inactive grid are untouched by both kinds of region operation.
    pub fn edit_lines(
        &mut self,
        direction: RegionDirection,
        top: usize,
        bottom: usize,
        count: usize,
        allocate_id: impl FnMut() -> RawRowId,
    ) -> Option<RegionMutation> {
        self.shift_region(
            direction,
            top..=bottom,
            count,
            RegionWrapPolicy::KeepPhysicalIndices,
            false,
            allocate_id,
        )
    }

    fn shift_region(
        &mut self,
        direction: RegionDirection,
        region: RangeInclusive<usize>,
        count: usize,
        wrap_policy: RegionWrapPolicy,
        capture_outgoing: bool,
        allocate_id: impl FnMut() -> RawRowId,
    ) -> Option<RegionMutation> {
        let (top, bottom) = (*region.start(), *region.end());
        if top > bottom || bottom >= self.grid.rows() || count == 0 {
            return None;
        }
        let height = bottom - top + 1;
        let count = count.min(height);
        let end = bottom + 1;
        let removed_rows = match direction {
            RegionDirection::Up => top..top + count,
            RegionDirection::Down => end - count..end,
        };
        let preserve_margin = self
            .grid
            .preserves_shift_margin(direction, &region, wrap_policy);
        let removed = removed_rows
            .map(|row| {
                let row_id = self.grid.row_id(row);
                let captured =
                    capture_outgoing.then(|| self.grid.archive_row(row, preserve_margin));
                RemovedRow {
                    original_row: row,
                    row_id,
                    captured,
                }
            })
            .collect();

        let blank = self.create_blank_cell();
        let dirty_rows =
            self.grid
                .shift_rows(direction, region, count, blank, wrap_policy, allocate_id)?;
        Some(RegionMutation {
            direction,
            top,
            bottom,
            count,
            removed,
            dirty_rows,
        })
    }

    /// Dimensions must be nonzero; admission limits belong to the host.
    /// # Panics
    /// Panics for zero dimensions, as does `with_grids`.
    pub fn new(rows: usize, cols: usize) -> Self {
        Self::with_grids(TerminalGrid::new(rows, cols), TerminalGrid::new(rows, cols))
    }

    /// Adopt real grids, including any identities already allocated by the host.
    /// # Panics
    /// Both grids must have matching, nonzero dimensions.
    pub fn with_grids(grid: TerminalGrid, alt_grid: TerminalGrid) -> Self {
        Self::with_grid_pair(grid, alt_grid, TabStopOrigin::Set)
    }

    pub fn default_tab_stops(cols: usize) -> Vec<bool> {
        default_tab_stops(cols, TabStopOrigin::Set)
    }

    pub fn next_tab_stop(&self, col: usize) -> usize {
        next_tab_stop(&self.tab_stops, self.grid.cols(), col)
    }

    pub fn prev_tab_stop(&self, col: usize) -> usize {
        previous_tab_stop(&self.tab_stops, self.grid.cols(), col)
    }

    pub fn save_cursor_state(&mut self, protocol: &ProtocolState) {
        self.save_cursor(protocol.modes().contains(&7));
    }

    pub fn restore_cursor_state(&mut self, protocol: &mut ProtocolState) {
        if let Some(autowrap) = self.restore_cursor() {
            if autowrap {
                protocol.modes_mut().insert(7);
            } else {
                protocol.modes_mut().remove(&7);
            }
        }
    }

    pub fn set_cursor_position(&mut self, row_param: usize, col_param: usize) {
        self.cursor_controls().set_position(row_param, col_param);
    }

    pub fn create_blank_cell(&self) -> TerminalCell {
        TerminalCell {
            character: ' ',
            foreground: Color::Default,
            background: self.current_bg, // Preserve current background color
            flags: StyleFlags::default(),
            hyperlink_id: HyperlinkId::NONE,
        }
    }

    pub fn translate_char(&self, ch: char) -> char {
        self.active_charset.translate(ch)
    }

    pub fn expanded_cell_mutation_span(
        &self,
        row: usize,
        start: usize,
        end: usize,
    ) -> (usize, usize) {
        let affected = self.grid.expanded_wide_span(row, start..end);
        (affected.start, affected.end)
    }

    /// Consume the deferred-wrap latch. Structural wrap/scroll is host-owned.
    pub fn take_pending_wrap(&mut self, autowrap: bool) -> bool {
        let pending = self.pending_wrap;
        self.pending_wrap = false;
        pending && autowrap
    }

    /// Request a host wrap when a wide glyph does not fit, or clamp with DECAWM off.
    pub fn prepare_character_width(&mut self, width: usize, autowrap: bool) -> bool {
        if self.cursor_col.saturating_add(width) > self.grid.cols() {
            if autowrap {
                return true;
            }
            self.cursor_col = self.grid.cols().saturating_sub(width);
        }
        false
    }

    fn row_mutation(
        &self,
        row: usize,
        start: usize,
        end: usize,
        printed: Option<Range<usize>>,
    ) -> RowMutation {
        let (start, end) = self.expanded_cell_mutation_span(row, start, end);
        RowMutation {
            row,
            row_id: self.grid.row_id(row),
            changed: start..end,
            printed,
        }
    }

    /// Write one already-translated graphic character after host wrap resolution.
    /// Width comes from the host's Unicode width policy and must be one or two.
    /// The one-column viewport retains Ember's existing truncated-wide behavior:
    /// its lead occupies the only cell, with no out-of-bounds continuation.
    /// # Panics
    /// A zero/invalid width or an out-of-bounds host cursor violates this contract.
    pub fn write_char_in_row(&mut self, ch: char, width: usize, autowrap: bool) -> RowMutation {
        assert!((1..=2).contains(&width));
        let cols = self.grid.cols();
        let row = self.cursor_row;
        let col = self.cursor_col;
        let blank = self.create_blank_cell();
        let end = col.saturating_add(width).min(cols);
        let effect = self.row_mutation(
            row,
            col,
            if self.insert_mode { cols } else { end },
            Some(col..end),
        );
        if self.insert_mode {
            // Preserve the host's shift-then-repair ordering. The displaced
            // continuation is at the right boundary repaired below.
            for _ in 0..width {
                self.grid.insert_cell_in_row(row, col, blank);
            }
            self.grid.clear_dangling_wide_at_row_end(row, blank);
        }
        self.grid.overwrite_graphic(
            row,
            col,
            width,
            TerminalCell {
                character: ch,
                foreground: self.current_fg,
                background: self.current_bg,
                flags: self.current_flags,
                hyperlink_id: self.current_hyperlink,
            },
            blank,
            WideContinuationProtection::FromLead,
        );
        self.cursor_col += width;
        if self.cursor_col >= cols {
            self.cursor_col = cols.saturating_sub(width);
            if autowrap {
                self.pending_wrap = true;
            }
        }
        effect
    }

    /// One step of Ember's printable ASCII, ASCII-charset, non-insert lane.
    /// The parser/host guarantees printable bytes (0x20..=0x7e), ASCII charset
    /// and no insert mode. Unicode, charset translation and REP stay elsewhere.
    /// Read autowrap once per run. On NeedsWrap, finish host vertical/history/
    /// identity/provenance effects before calling again with the same input.
    /// On Written, consume the mutation before advancing input or taking another
    /// step. Never aggregate row effects or add per-row takeover observations.
    /// Empty input leaves the latch alone; nonempty input clears it even when
    /// autowrap is disabled. Cursor bounds/no-progress retain the row contract.
    pub fn step_ascii_write(&mut self, bytes: &[u8], autowrap: bool) -> AsciiWriteStep {
        if bytes.is_empty() {
            return AsciiWriteStep::Stopped;
        }
        if self.take_pending_wrap(autowrap) {
            return AsciiWriteStep::NeedsWrap;
        }
        match self.write_ascii_in_row(bytes, autowrap) {
            Some((consumed, mutation)) => AsciiWriteStep::Written { consumed, mutation },
            None => AsciiWriteStep::Stopped,
        }
    }

    /// The bounded ASCII overwrite fast path. The host resolves wrap between chunks.
    /// Returns the number consumed, never crossing a physical row boundary.
    pub fn write_ascii_in_row(
        &mut self,
        bytes: &[u8],
        autowrap: bool,
    ) -> Option<(usize, RowMutation)> {
        assert!(!self.insert_mode);
        let cols = self.grid.cols();
        let col = self.cursor_col;
        let row = self.cursor_row;
        let count = bytes.len().min(cols.saturating_sub(col));
        if count == 0 {
            return None;
        }
        let effect = self.row_mutation(row, col, col + count, Some(col..col + count));
        let blank = self.create_blank_cell();
        self.grid.overwrite_ascii(
            row,
            col,
            &bytes[..count],
            TerminalCell {
                character: ' ',
                foreground: self.current_fg,
                background: self.current_bg,
                flags: self.current_flags,
                hyperlink_id: self.current_hyperlink,
            },
            blank,
            AsciiOverwritePolicy::SharedScreen,
        );
        self.cursor_col += count;
        if self.cursor_col >= cols {
            self.cursor_col = cols - 1;
            if autowrap {
                self.pending_wrap = true;
            }
        }
        self.last_printed_char = Some(bytes[count - 1] as char);
        Some((count, effect))
    }

    pub fn combining_target(&self) -> Option<(usize, char)> {
        let mut col = if self.pending_wrap {
            self.cursor_col
        } else {
            self.cursor_col.checked_sub(1)?
        };
        if self
            .grid
            .get(self.cursor_row, col)
            .flags
            .wide_continuation()
        {
            col = col.checked_sub(1)?;
        }
        let ch = self.grid.get(self.cursor_row, col).character;
        (!matches!(ch, ' ' | '\0')).then_some((col, ch))
    }

    /// Commit a normalized single-codepoint combination without moving the cursor.
    pub fn replace_combining_target(&mut self, col: usize, ch: char) -> RowMutation {
        let row = self.cursor_row;
        let effect = RowMutation {
            row,
            row_id: self.grid.row_id(row),
            changed: col..col + 1,
            printed: Some(col..col + 1),
        };
        let color = self.grid.underline_colors(row).get(col).copied();
        self.grid.get_mut(row, col).character = ch;
        if let Some(color) = color {
            self.grid.set_underline_color(row, col..col + 1, color);
        }
        effect
    }

    pub fn insert_characters(&mut self, count: usize) -> Option<RowMutation> {
        let row = self.cursor_row;
        let col = self.cursor_col;
        let cols = self.grid.cols();
        let count = count.min(cols.saturating_sub(col));
        if count == 0 {
            return None;
        }
        let effect = self.row_mutation(row, col, cols, None);
        self.pending_wrap = false;
        let blank = self.create_blank_cell();
        self.grid
            .insert_blank_cells(row, col, count, blank, WideCellEditPolicy::RepairSplitPairs);
        Some(effect)
    }

    pub fn delete_characters(&mut self, count: usize) -> Option<RowMutation> {
        let row = self.cursor_row;
        let col = self.cursor_col;
        let cols = self.grid.cols();
        let count = count.min(cols.saturating_sub(col));
        if count == 0 {
            return None;
        }
        let effect = self.row_mutation(row, col, cols, None);
        self.pending_wrap = false;
        let blank = self.create_blank_cell();
        self.grid
            .delete_cells(row, col, count, blank, WideCellEditPolicy::RepairSplitPairs);
        Some(effect)
    }

    /// At most one effect per eligible run, bounded by the width of this row.
    /// Protected-only selective erasure leaves the Last Column Flag untouched.
    pub fn erase_span(
        &mut self,
        row: usize,
        start: usize,
        end: usize,
        selective: bool,
    ) -> SmallVec<[RowMutation; 4]> {
        let mut effects = SmallVec::new();
        if row >= self.grid.rows() {
            return effects;
        }
        let end = end.min(self.grid.cols());
        let mut col = start.min(end);
        while col < end {
            if selective && self.grid.get(row, col).flags.protected() {
                col += 1;
                continue;
            }
            let start = col;
            col += 1;
            while col < end && !(selective && self.grid.get(row, col).flags.protected()) {
                col += 1;
            }
            effects.push(self.row_mutation(row, start, col, None));
            self.pending_wrap = false;
            let blank = self.create_blank_cell();
            self.grid.erase_cells(row, start..col, blank);
        }
        effects
    }

    pub fn move_cursor_up(&mut self, count: usize) {
        self.cursor_controls().move_up(count);
    }
    pub fn move_cursor_down(&mut self, count: usize) {
        self.cursor_controls().move_down(count);
    }
    pub fn move_cursor_forward(&mut self, count: usize) {
        self.cursor_controls().move_forward(count);
    }
    pub fn move_cursor_backward(&mut self, count: usize) {
        self.cursor_controls().move_backward(count);
    }
    pub fn set_cursor_column(&mut self, column: usize) {
        self.cursor_controls().set_column(column);
    }
    pub fn set_cursor_row(&mut self, row: usize) {
        self.cursor_controls().set_row(row);
    }
    pub fn tab_forward(&mut self, count: usize) {
        self.cursor_controls().tab_forward(count);
    }
    pub fn tab_backward(&mut self, count: usize) {
        self.cursor_controls().tab_backward(count);
    }
    pub fn set_tab_stop(&mut self) {
        self.cursor_controls().set_tab_stop(true);
    }
    pub fn clear_tab_stop(&mut self) {
        self.cursor_controls().set_tab_stop(false);
    }
    pub fn clear_all_tab_stops(&mut self) {
        self.cursor_controls().clear_all_tab_stops();
    }
    pub fn set_scroll_region(&mut self, top: usize, bottom: usize) {
        self.cursor_controls().set_scroll_region(top, bottom);
    }
}
