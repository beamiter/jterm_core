//! Live VT screen ownership, row edits, region shifts and physical resizing.
//!
//! The host owns identity allocation, history, resize/scroll policy and
//! graphics. Public fields are an explicit transitional orchestration boundary,
//! not a mirrored screen or a Deref facade. Every row operation returns bounded
//! effects which the host must consume before the next protocol command.
use crate::grid::{
    Color, CursorShape, RawRowId, ScrollbackLine, StyleFlags, TerminalCell, TerminalGrid,
};
use crate::hyperlink::HyperlinkId;
use crate::protocol_state::ProtocolState;
use smallvec::SmallVec;
use std::ops::{Range, RangeInclusive};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Charset {
    #[default]
    Ascii,
    DecSpecialGraphics,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegionDirection {
    Up,
    Down,
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

/// Sole live owner of the active and inactive grids and their drawing state.
/// Autowrap is deliberately absent: ProtocolState mode 7 is authoritative.
///
/// During this transitional extraction the host may mutate fields for structural
/// operations. It must keep both grids nonempty, cursor positions in bounds and
/// margins valid before calling row operations, and immediately consume their
/// effects. Rows keep their identities throughout each nonstructural operation.
pub struct ScreenState {
    pub grid: TerminalGrid,
    pub alt_grid: TerminalGrid,
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
        if motion != VerticalMotion::Wrap {
            self.pending_wrap = false;
        }
        let departure = (motion != VerticalMotion::ReverseIndex).then(|| VerticalDeparture {
            row_id: self.grid.row_id(self.cursor_row),
            row: self.cursor_row,
            width: self.grid.row_len(),
        });
        if motion == VerticalMotion::Wrap {
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
        self.grid.cells_mut().fill(blank);
    }

    /// DECALN: fill the physical active grid with default-style E cells, reset
    /// margins and home the cursor. Unlike ED2 this deliberately does not use
    /// the current background. Origin mode and all other drawing/saved state,
    /// row metadata and the inactive grid are retained. The host invalidates
    /// provenance first and owns reposition notification and dirty bookkeeping.
    pub fn screen_alignment_test(&mut self) {
        self.grid.cells_mut().fill(TerminalCell {
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
        if self.use_alt_buffer {
            return None;
        }
        if save_cursor {
            self.saved_cursor_row = self.cursor_row;
            self.saved_cursor_col = self.cursor_col;
            self.saved_primary_screen_state = Some(SavedCursorState {
                row: self.cursor_row,
                col: self.cursor_col,
                fg: self.current_fg,
                bg: self.current_bg,
                flags: self.current_flags,
                g0: self.g0_charset,
                g1: self.g1_charset,
                active: self.active_charset,
                origin_mode: self.origin_mode,
                autowrap: protocol.modes().contains(&7),
                pending_wrap: self.pending_wrap,
            });
        }
        self.saved_primary_cursor_shape = Some(self.cursor_shape);
        std::mem::swap(&mut self.grid, &mut self.alt_grid);
        self.alt_cursor_row = self.cursor_row;
        self.alt_cursor_col = self.cursor_col;
        self.use_alt_buffer = true;
        self.current_hyperlink = HyperlinkId::NONE;
        self.scroll_region_top = 0;
        self.scroll_region_bottom = self.grid.rows() - 1;
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
        if !self.use_alt_buffer {
            return None;
        }
        self.alt_cursor_row = self.cursor_row;
        self.alt_cursor_col = self.cursor_col;
        std::mem::swap(&mut self.grid, &mut self.alt_grid);
        if restore_cursor {
            self.cursor_row = self.saved_cursor_row;
            self.cursor_col = self.saved_cursor_col;
        }
        self.use_alt_buffer = false;
        if let Some(shape) = self.saved_primary_cursor_shape.take() {
            self.cursor_shape = shape;
        }
        self.current_hyperlink = HyperlinkId::NONE;
        self.scroll_region_top = 0;
        self.scroll_region_bottom = self.grid.rows() - 1;
        let restore_autowrap = if let Some(saved) = self.saved_primary_screen_state.take() {
            self.current_fg = saved.fg;
            self.current_bg = saved.bg;
            self.current_flags = saved.flags;
            self.g0_charset = saved.g0;
            self.g1_charset = saved.g1;
            self.active_charset = saved.active;
            self.origin_mode = saved.origin_mode;
            Some(saved.autowrap)
        } else {
            self.current_fg = Color::Default;
            self.current_bg = Color::Default;
            self.current_flags = StyleFlags::default();
            None
        };
        self.pending_wrap = false;
        Some(AlternateExit {
            global_background: self.current_bg,
            restore_autowrap,
            dirty_rows: 0..self.grid.rows(),
        })
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
        let transferred = (0..transfer_top)
            .map(|row| {
                let mut line =
                    ScrollbackLine::compress(&self.grid[row], self.grid.row_wrapped[row]);
                line.set_raw_row_id(self.grid.row_id(row));
                line
            })
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
        if cols != self.tab_stops.len() {
            let old_len = self.tab_stops.len();
            self.tab_stops.resize(cols, false);
            for col in old_len..cols {
                self.tab_stops[col] = col % 8 == 0;
            }
        }
        if let Some(saved) = self.saved_state.as_mut() {
            saved.row = saved.row.min(rows - 1);
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
            true,
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
        self.shift_region(direction, top..=bottom, count, false, false, allocate_id)
    }

    fn shift_region(
        &mut self,
        direction: RegionDirection,
        region: RangeInclusive<usize>,
        count: usize,
        move_wraps: bool,
        capture_outgoing: bool,
        mut allocate_id: impl FnMut() -> RawRowId,
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
        let removed = removed_rows
            .map(|row| {
                let row_id = self.grid.row_id(row);
                let captured = capture_outgoing.then(|| {
                    let mut line =
                        ScrollbackLine::compress(&self.grid[row], self.grid.row_wrapped[row]);
                    line.set_raw_row_id(row_id);
                    line
                });
                RemovedRow {
                    original_row: row,
                    row_id,
                    captured,
                }
            })
            .collect();

        let cols = self.grid.cols();
        let blank = self.create_blank_cell();
        let blank_rows = match direction {
            RegionDirection::Up => {
                if count < height {
                    self.grid
                        .cells_mut()
                        .copy_within((top + count) * cols..end * cols, top * cols);
                    if move_wraps {
                        self.grid.row_wrapped.copy_within(top + count..end, top);
                    }
                    self.grid.row_ids_mut().copy_within(top + count..end, top);
                }
                end - count..end
            }
            RegionDirection::Down => {
                if count < height {
                    self.grid
                        .cells_mut()
                        .copy_within(top * cols..(end - count) * cols, (top + count) * cols);
                    if move_wraps {
                        self.grid
                            .row_wrapped
                            .copy_within(top..end - count, top + count);
                    }
                    self.grid
                        .row_ids_mut()
                        .copy_within(top..end - count, top + count);
                }
                top..top + count
            }
        };
        self.grid.cells_mut()[blank_rows.start * cols..blank_rows.end * cols].fill(blank);
        if move_wraps {
            self.grid.row_wrapped[blank_rows.clone()].fill(false);
        }
        // Allocate in ascending destination-row order, as Ember's separate
        // shift_row_id_region_{up,down} helpers did. Never refill retained IDs,
        // including UNTRACKED, and never allocate an identity for captured rows.
        for row in blank_rows {
            self.grid.row_ids_mut()[row] = allocate_id();
        }
        Some(RegionMutation {
            direction,
            top,
            bottom,
            count,
            removed,
            dirty_rows: top..end,
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
        assert!(grid.rows() > 0 && grid.cols() > 0);
        assert_eq!(
            (grid.rows(), grid.cols()),
            (alt_grid.rows(), alt_grid.cols())
        );
        let rows = grid.rows();
        let cols = grid.cols();
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
            tab_stops: Self::default_tab_stops(cols),
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

    pub fn default_tab_stops(cols: usize) -> Vec<bool> {
        (0..cols).map(|c| c % 8 == 0).collect()
    }

    pub fn next_tab_stop(&self, col: usize) -> usize {
        let cols = self.grid.row_len();
        let mut c = col.saturating_add(1);
        while c < cols {
            if self.tab_stops.get(c).copied().unwrap_or(false) {
                return c;
            }
            c += 1;
        }
        cols.saturating_sub(1)
    }

    pub fn prev_tab_stop(&self, col: usize) -> usize {
        let mut c = col.min(self.grid.cols());
        while c > 0 {
            c -= 1;
            if self.tab_stops.get(c).copied().unwrap_or(false) {
                return c;
            }
        }
        0
    }

    pub fn save_cursor_state(&mut self, protocol: &ProtocolState) {
        self.saved_state = Some(SavedCursorState {
            row: self.cursor_row,
            col: self.cursor_col,
            fg: self.current_fg,
            bg: self.current_bg,
            flags: self.current_flags,
            g0: self.g0_charset,
            g1: self.g1_charset,
            active: self.active_charset,
            origin_mode: self.origin_mode,
            autowrap: protocol.modes().contains(&7),
            pending_wrap: self.pending_wrap,
        });
    }

    pub fn restore_cursor_state(&mut self, protocol: &mut ProtocolState) {
        if let Some(s) = self.saved_state.clone() {
            self.cursor_row = s.row.min(self.grid.rows().saturating_sub(1));
            self.cursor_col = s.col.min(self.grid.row_len().saturating_sub(1));
            self.current_fg = s.fg;
            self.current_bg = s.bg;
            self.current_flags = s.flags;
            self.g0_charset = s.g0;
            self.g1_charset = s.g1;
            self.active_charset = s.active;
            self.origin_mode = s.origin_mode;
            if s.autowrap {
                protocol.modes_mut().insert(7);
            } else {
                protocol.modes_mut().remove(&7);
            }
            self.pending_wrap = s.pending_wrap;
        } else {
            self.cursor_row = 0;
            self.cursor_col = 0;
            self.pending_wrap = false;
        }
    }

    pub fn set_cursor_position(&mut self, row_param: usize, col_param: usize) {
        self.pending_wrap = false;
        let col = col_param
            .saturating_sub(1)
            .min(self.grid.row_len().saturating_sub(1));
        let row0 = row_param.saturating_sub(1);
        self.cursor_row = if self.origin_mode {
            self.scroll_region_top
                .saturating_add(row0)
                .min(self.scroll_region_bottom)
        } else {
            row0.min(self.grid.rows().saturating_sub(1))
        };
        self.cursor_col = col;
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
        match self.active_charset {
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

    fn split_wide_pairs_around(&mut self, row: usize, start: usize, end: usize) {
        let cols = self.grid.row_len();
        if row >= self.grid.rows() || start >= end || start >= cols {
            return;
        }
        let end = end.min(cols);
        let blank_cell = self.create_blank_cell();
        if start > 0 && self.grid.get(row, start - 1).flags.wide() {
            *self.grid.get_mut(row, start - 1) = blank_cell;
        }
        if end < cols && self.grid.get(row, end).flags.wide_continuation() {
            *self.grid.get_mut(row, end) = blank_cell;
        }
    }

    pub fn expanded_cell_mutation_span(
        &self,
        row: usize,
        start: usize,
        end: usize,
    ) -> (usize, usize) {
        let cols = self.grid.row_len();
        let mut start = start.min(cols);
        let mut end = end.min(cols);
        if row >= self.grid.rows() || start >= end {
            return (start, end);
        }
        if start > 0 && self.grid.get(row, start - 1).flags.wide() {
            start -= 1;
        }
        if end < cols && self.grid.get(row, end).flags.wide_continuation() {
            end += 1;
        }
        (start, end)
    }

    fn clear_cell_unchecked(&mut self, row: usize, col: usize) {
        let cols = self.grid.row_len();
        let bg_color = self.current_bg;
        let blank_cell = TerminalCell {
            character: ' ',
            foreground: Color::Default,
            background: bg_color,
            flags: StyleFlags::default(),
            hyperlink_id: HyperlinkId::NONE,
        };
        // If clearing a continuation cell, also clear the wide character body
        if self.grid.get(row, col).flags.wide_continuation() && col > 0 {
            *self.grid.get_mut(row, col - 1) = blank_cell;
        }
        // If clearing a wide character body, also clear the continuation cell
        if self.grid.get(row, col).flags.wide() && col + 1 < cols {
            *self.grid.get_mut(row, col + 1) = blank_cell;
        }
        *self.grid.get_mut(row, col) = blank_cell;
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
        self.split_wide_pairs_around(row, col, end);
        let cell = self.grid.get_mut(row, col);
        *cell = TerminalCell {
            character: ch,
            foreground: self.current_fg,
            background: self.current_bg,
            flags: self.current_flags,
            hyperlink_id: self.current_hyperlink,
        };
        cell.flags.set_wide(width == 2);
        cell.flags.set_wide_continuation(false);
        if width == 2 && col + 1 < cols {
            let cell = self.grid.get_mut(row, col + 1);
            *cell = blank;
            cell.hyperlink_id = self.current_hyperlink;
            cell.flags.set_wide_continuation(true);
            cell.flags.set_protected(self.current_flags.protected());
        }
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
        debug_assert!(bytes[..count].iter().all(|b| (0x20..=0x7e).contains(b)));
        let effect = self.row_mutation(row, col, col + count, Some(col..col + count));
        self.split_wide_pairs_around(row, col, col + count);
        let mut flags = self.current_flags;
        flags.set_wide(false);
        flags.set_wide_continuation(false);
        for (cell, &byte) in self.grid[row][col..col + count]
            .iter_mut()
            .zip(&bytes[..count])
        {
            *cell = TerminalCell {
                character: byte as char,
                foreground: self.current_fg,
                background: self.current_bg,
                flags,
                hyperlink_id: self.current_hyperlink,
            };
        }
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
        self.grid.get_mut(row, col).character = ch;
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
        if self.grid.get(row, col).flags.wide_continuation() {
            self.clear_cell_unchecked(row, col);
        }
        for _ in 0..count {
            self.grid.insert_cell_in_row(row, col, blank);
        }
        self.grid.clear_dangling_wide_at_row_end(row, blank);
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
        self.split_wide_pairs_around(row, col, col + count);
        for _ in 0..count {
            self.grid.remove_cell_from_row(row, col);
            *self.grid.get_mut(row, cols - 1) = blank;
        }
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
            for erased in start..col {
                self.clear_cell_unchecked(row, erased);
            }
        }
        effects
    }

    pub fn move_cursor_up(&mut self, count: usize) {
        self.pending_wrap = false;
        let floor = if self.cursor_row >= self.scroll_region_top {
            self.scroll_region_top
        } else {
            0
        };
        self.cursor_row = self.cursor_row.saturating_sub(count).max(floor);
    }
    pub fn move_cursor_down(&mut self, count: usize) {
        self.pending_wrap = false;
        let ceil = if self.cursor_row <= self.scroll_region_bottom {
            self.scroll_region_bottom
        } else {
            self.grid.rows() - 1
        };
        self.cursor_row = self.cursor_row.saturating_add(count).min(ceil);
    }
    pub fn move_cursor_forward(&mut self, count: usize) {
        self.pending_wrap = false;
        self.cursor_col = self
            .cursor_col
            .saturating_add(count)
            .min(self.grid.cols() - 1);
    }
    pub fn move_cursor_backward(&mut self, count: usize) {
        self.pending_wrap = false;
        self.cursor_col = self.cursor_col.saturating_sub(count);
    }
    pub fn set_cursor_column(&mut self, column: usize) {
        self.pending_wrap = false;
        self.cursor_col = column.saturating_sub(1).min(self.grid.cols() - 1);
    }
    pub fn set_cursor_row(&mut self, row: usize) {
        self.pending_wrap = false;
        let row = row.saturating_sub(1);
        self.cursor_row = if self.origin_mode {
            self.scroll_region_top
                .saturating_add(row)
                .min(self.scroll_region_bottom)
        } else {
            row.min(self.grid.rows() - 1)
        };
    }
    pub fn tab_forward(&mut self, count: usize) {
        self.pending_wrap = false;
        for _ in 0..count.min(self.grid.cols()) {
            self.cursor_col = self.next_tab_stop(self.cursor_col);
        }
    }
    pub fn tab_backward(&mut self, count: usize) {
        self.pending_wrap = false;
        for _ in 0..count.min(self.grid.cols()) {
            self.cursor_col = self.prev_tab_stop(self.cursor_col);
        }
    }
    pub fn set_tab_stop(&mut self) {
        if let Some(stop) = self.tab_stops.get_mut(self.cursor_col) {
            *stop = true;
        }
    }
    pub fn clear_tab_stop(&mut self) {
        if let Some(stop) = self.tab_stops.get_mut(self.cursor_col) {
            *stop = false;
        }
    }
    pub fn clear_all_tab_stops(&mut self) {
        self.tab_stops.fill(false);
    }
    pub fn set_scroll_region(&mut self, top: usize, bottom: usize) {
        self.scroll_region_top = top.saturating_sub(1).min(self.grid.rows() - 1);
        self.scroll_region_bottom = bottom.saturating_sub(1).min(self.grid.rows() - 1);
        if self.scroll_region_top > self.scroll_region_bottom {
            self.scroll_region_top = 0;
            self.scroll_region_bottom = self.grid.rows() - 1;
        }
        self.set_cursor_position(1, 1);
    }
}
