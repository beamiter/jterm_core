//! Live VT screen ownership, row edits, region shifts and physical resizing.
//!
//! The host selects a compatibility profile and consumes history and graphics
//! effects. Public fields are an explicit transitional orchestration boundary,
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
use unicode_normalization::UnicodeNormalization;

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

/// NFC composition accepted by scalar-cell legacy profiles. Multi-scalar
/// results stay unrepresented, as in the original Ember/Frost cell models.
pub fn compose_scalar_pair(base: char, mark: char) -> Option<char> {
    let mut normalized = [base, mark].into_iter().nfc();
    match (normalized.next(), normalized.next()) {
        (Some(ch), None) => Some(ch),
        _ => None,
    }
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
                'b' => '␉',
                'c' => '␌',
                'd' => '␍',
                'e' => '␊',
                'f' => '°',
                'g' => '±',
                'h' => '␤',
                'i' => '␋',
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

#[derive(Clone, Copy)]
struct CharsetBanks {
    upper: [Charset; 2],
    invoked: u8,
    invoked_gr: u8,
    latin1: u8,
    british: u8,
    single_shift: Option<u8>,
}
impl Default for CharsetBanks {
    fn default() -> Self {
        Self {
            upper: [Charset::Ascii; 2],
            invoked: 0,
            invoked_gr: 2,
            latin1: 0,
            british: 0,
            single_shift: None,
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
            // DEC DECRC defines a normal home drawing state when DECSC has
            // never populated this slot. This is not a screen or mode reset.
            *self.position.0 = 0;
            *self.position.1 = 0;
            *self.fg = Color::Default;
            *self.bg = Color::Default;
            *self.flags = StyleFlags::default();
            *self.g0 = Charset::Ascii;
            *self.g1 = Charset::Ascii;
            *self.active = Charset::Ascii;
            *self.origin_mode = false;
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum WriteProfile {
    Ember,
    FrostLegacy,
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
    charset_banks: &'a mut CharsetBanks,
    saved_primary_banks: &'a mut CharsetBanks,
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
            *self.saved_primary_banks = *self.charset_banks;
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
            *self.charset_banks = *self.saved_primary_banks;
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
        if count == 0 || self.cols == 0 {
            return;
        }
        let end = self.cols.min(self.tab_stops.len());
        let start = self.col.saturating_add(1).min(end);
        *self.col = self.tab_stops[start..end]
            .iter()
            .enumerate()
            .filter(|(_, stop)| **stop)
            .nth(count - 1)
            .map_or(self.cols - 1, |(index, _)| start + index);
    }

    pub fn tab_backward(&mut self, count: usize) {
        *self.pending_wrap = false;
        if count == 0 || self.cols == 0 {
            return;
        }
        let end = (*self.col).min(self.cols).min(self.tab_stops.len());
        *self.col = self.tab_stops[..end]
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, stop)| **stop)
            .nth(count - 1)
            .map_or(0, |(index, _)| index);
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
    pub(crate) extended_rendition: bool,
    pub current_hyperlink: HyperlinkId,
    pub scroll_region_top: usize,
    pub scroll_region_bottom: usize,
    pub g0_charset: Charset,
    pub g1_charset: Charset,
    pub active_charset: Charset,
    // UK NRCS glyph selection over fixed UTF-8. DECNRCM changes this mapping
    // without XTerm's separate UTF-8/ISO-2022 decoder switch and bank reset.
    pub(crate) national_replacement_mode: bool,
    charset_banks: CharsetBanks,
    saved_cursor_banks: CharsetBanks,
    saved_primary_banks: CharsetBanks,
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
            extended_rendition: false,
            current_hyperlink: HyperlinkId::NONE,
            scroll_region_top: 0,
            scroll_region_bottom: rows - 1,
            g0_charset: Charset::Ascii,
            g1_charset: Charset::Ascii,
            active_charset: Charset::Ascii,
            national_replacement_mode: false,
            charset_banks: CharsetBanks::default(),
            saved_cursor_banks: CharsetBanks::default(),
            saved_primary_banks: CharsetBanks::default(),
        }
    }

    /// Admit conceal and overline when the native consumer can draw them.
    pub fn enable_extended_rendition(&mut self) {
        self.extended_rendition = true;
    }

    pub fn alternate_screen_controls(&mut self) -> AlternateScreenControls<'_, Grid> {
        self.charset_banks.invoked = self.invoked_charset_bank();
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
            charset_banks: &mut self.charset_banks,
            saved_primary_banks: &mut self.saved_primary_banks,
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
        self.charset_banks.invoked = self.invoked_charset_bank();
        self.saved_cursor_banks = self.charset_banks;
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
        if saved.is_some() {
            self.charset_banks = self.saved_cursor_banks;
        } else {
            // Reset supported bank designations/invocation, including pending
            // single shifts. Default GR remains the engine's Latin-1 fallback.
            self.reset_charset_banks();
        }
        saved.map(|state| state.autowrap)
    }

    pub(crate) fn charset_bank(&self, index: u8) -> Charset {
        match index {
            0 => self.g0_charset,
            1 => self.g1_charset,
            2 | 3 => self.charset_banks.upper[usize::from(index - 2)],
            _ => Charset::Ascii,
        }
    }
    pub(crate) fn invoked_charset_bank(&self) -> u8 {
        let current = self.charset_banks.invoked;
        if self.charset_bank(current) == self.active_charset {
            return current;
        }
        // Public legacy drawing state may be set directly by a native host.
        (0..4)
            .find(|&index| self.charset_bank(index) == self.active_charset)
            .unwrap_or(0)
    }
    pub(crate) fn invoked_g1(&self) -> bool {
        self.invoked_charset_bank() == 1
    }
    pub(crate) fn invoke_charset(&mut self, g1: bool) {
        self.invoke_charset_bank(u8::from(g1));
    }
    pub(crate) fn invoke_charset_bank(&mut self, index: u8) {
        if index < 4 {
            self.charset_banks.invoked = index;
            self.active_charset = self.charset_bank(index);
        }
    }
    pub(crate) fn invoked_gr_charset_bank(&self) -> u8 {
        self.charset_banks.invoked_gr
    }
    pub(crate) fn invoke_gr_charset_bank(&mut self, index: u8) {
        if index < 4 {
            self.charset_banks.invoked_gr = index;
        }
    }
    /// Numeric DECFRA uses the selected GL/GR table without consuming SS2/SS3.
    /// Ordinary Unicode text uses take_graphic_charset and never this mapping.
    pub(crate) fn translate_fill_character(&self, value: u8) -> char {
        if value < 128 {
            let bank = self
                .single_shift_bank()
                .unwrap_or(self.invoked_charset_bank());
            if self.national_replacement_mode && self.charset_is_british(bank) && value == b'#' {
                return '£';
            }
            let charset = self
                .single_shift_bank()
                .map_or(self.active_charset, |index| self.charset_bank(index));
            return charset.translate(char::from(value));
        }
        let bank = self.invoked_gr_charset_bank();
        if self.national_replacement_mode && self.charset_is_british(bank) {
            return if matches!(value, 0xa0 | 0xff) {
                ' '
            } else {
                self.translate_bank(bank, char::from(value & 0x7f))
            };
        }
        if self.charset_bank(bank) == Charset::Ascii {
            return char::from(value);
        }
        if matches!(value, 0xa0 | 0xff) {
            return ' ';
        }
        let base = char::from(value & 0x7f);
        let mapped = Charset::DecSpecialGraphics.translate(base);
        if mapped == base {
            char::from(value)
        } else {
            mapped
        }
    }
    /// ISO Latin-1 is identity on this UTF-8 terminal, as in XTerm's ISO
    /// Latin-1 output path. Private designation bits preserve the 96-set wire
    /// identity without adding a variant to the public legacy Charset enum.
    pub(crate) fn designate_latin1(&mut self, index: u8) {
        if index < 4 {
            self.designate_charset_bank(index, Charset::Ascii);
            self.charset_banks.latin1 |= 1 << index;
        }
    }
    pub(crate) fn charset_is_latin1(&self, index: u8) -> bool {
        index < 4
            && self.charset_bank(index) == Charset::Ascii
            && self.charset_banks.latin1 & (1 << index) != 0
    }
    pub(crate) fn designate_charset_bank(&mut self, index: u8, charset: Charset) {
        if index >= 4 {
            return;
        }
        self.charset_banks.latin1 &= !(1 << index);
        self.charset_banks.british &= !(1 << index);
        let invoked = self.invoked_charset_bank();
        match index {
            0 => self.g0_charset = charset,
            1 => self.g1_charset = charset,
            _ => self.charset_banks.upper[usize::from(index - 2)] = charset,
        }
        if invoked == index {
            self.invoke_charset_bank(index);
        }
    }
    pub(crate) fn designate_british(&mut self, index: u8) {
        self.designate_charset_bank(index, Charset::Ascii);
        if index < 4 {
            self.charset_banks.british |= 1 << index;
        }
    }
    pub(crate) fn charset_is_british(&self, index: u8) -> bool {
        index < 4
            && self.charset_bank(index) == Charset::Ascii
            && self.charset_banks.british & (1 << index) != 0
    }
    pub(crate) fn ascii_graphics(&self) -> bool {
        self.active_charset == Charset::Ascii && !self.uses_british_charset()
    }
    fn uses_british_charset(&self) -> bool {
        self.national_replacement_mode && self.charset_is_british(self.invoked_charset_bank())
    }
    fn translate_bank(&self, bank: u8, ch: char) -> char {
        if self.national_replacement_mode && self.charset_is_british(bank) && ch == '#' {
            '£'
        } else {
            self.charset_bank(bank).translate(ch)
        }
    }
    pub(crate) fn translate_graphic(&mut self, ch: char) -> char {
        if !self.national_replacement_mode {
            return self.take_graphic_charset(ch).translate(ch);
        }
        let bank = if (' '..='~').contains(&ch) {
            self.charset_banks
                .single_shift
                .unwrap_or(self.invoked_charset_bank())
        } else {
            self.invoked_charset_bank()
        };
        let charset = self.take_graphic_charset(ch);
        if self.national_replacement_mode && self.charset_is_british(bank) && ch == '#' {
            '£'
        } else {
            charset.translate(ch)
        }
    }
    pub(crate) fn single_shift(&mut self, index: u8) {
        if matches!(index, 2 | 3) {
            self.charset_banks.single_shift = Some(index);
        }
    }
    pub(crate) fn single_shift_bank(&self) -> Option<u8> {
        self.charset_banks.single_shift
    }
    pub(crate) fn single_shift_pending(&self) -> bool {
        self.charset_banks.single_shift.is_some()
    }
    pub(crate) fn take_graphic_charset(&mut self, ch: char) -> Charset {
        if (' '..='~').contains(&ch) {
            if let Some(index) = self.charset_banks.single_shift.take() {
                return self.charset_bank(index);
            }
        }
        self.active_charset
    }
    pub(crate) fn reset_charset_banks(&mut self) {
        self.g0_charset = Charset::Ascii;
        self.g1_charset = Charset::Ascii;
        self.active_charset = Charset::Ascii;
        self.charset_banks = CharsetBanks::default();
    }

    pub(crate) fn designate_charset(&mut self, g1: bool, charset: Charset) {
        self.charset_banks.latin1 &= !(1 << u8::from(g1));
        self.charset_banks.british &= !(1 << u8::from(g1));
        if g1 {
            let active = self.invoked_g1();
            self.g1_charset = charset;
            if active {
                self.invoke_charset(true);
            }
        } else {
            self.g0_charset = charset;
            self.invoke_charset(false);
        }
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

    /// Apply Frost's physical cursor move and identify the region shift, if
    /// needed. Frost preserves the row's legacy wrap metadata. The caller
    /// decides whether to archive the outgoing row before shifting the region.
    pub fn frost_legacy_vertical_cursor_motion(
        &mut self,
        motion: VerticalMotion,
    ) -> Option<RegionDirection> {
        if motion == VerticalMotion::Wrap {
            self.grid.row_wrapped[self.cursor_row] = true;
            self.cursor_col = 0;
        } else {
            self.pending_wrap = false;
        }
        self.advance_vertical_cursor(motion)
    }

    fn advance_vertical_cursor(&mut self, motion: VerticalMotion) -> Option<RegionDirection> {
        if motion == VerticalMotion::ReverseIndex {
            if self.cursor_row == self.scroll_region_top {
                return Some(RegionDirection::Down);
            }
            self.cursor_row = self.cursor_row.saturating_sub(1);
        } else if self.cursor_row == self.scroll_region_bottom {
            return Some(RegionDirection::Up);
        } else if self.cursor_row + 1 < self.grid.rows() {
            self.cursor_row += 1;
        }
        None
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
        let region = self.advance_vertical_cursor(motion).and_then(|direction| {
            self.scroll_region_one(
                direction,
                self.scroll_region_top,
                self.scroll_region_bottom,
                capture_outgoing && direction == RegionDirection::Up,
                allocate_id,
            )
        });
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

    /// FrostLegacyV1 ED0/ED1 erase physical cells using the current BCE pen.
    /// Row identities, wrap metadata, pending wrap and cursor state survive.
    /// The host handles ED2 history, ED3 retention, provenance and damage.
    pub fn erase_frost_legacy_display(&mut self, mode: u16) -> Option<RangeInclusive<usize>> {
        let cols = self.grid.row_len();
        let blank = self.create_blank_cell();
        match mode {
            0 => {
                self.grid
                    .erase_cells(self.cursor_row, self.cursor_col..cols, blank);
                for row in self.cursor_row + 1..self.grid.rows() {
                    self.grid.erase_cells(row, 0..cols, blank);
                }
                Some(self.cursor_row..=self.grid.rows().saturating_sub(1))
            }
            1 => {
                for row in 0..self.cursor_row {
                    self.grid.erase_cells(row, 0..cols, blank);
                }
                self.grid
                    .erase_cells(self.cursor_row, 0..self.cursor_col + 1, blank);
                Some(0..=self.cursor_row)
            }
            _ => None,
        }
    }

    /// FrostLegacyV1 DECALN keeps its existing scrolling margins.
    pub fn frost_legacy_alignment_test(&mut self) {
        self.grid.fill_cells(TerminalCell {
            character: 'E',
            ..TerminalCell::default()
        });
        self.cursor_row = 0;
        self.cursor_col = 0;
        self.pending_wrap = false;
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
        if let Some(saved) = self.saved_state.as_mut() {
            saved.row = saved.row.saturating_sub(transfer_top).min(rows - 1);
            saved.col = saved.col.min(cols - 1);
        }
        self.finish_physical_resize(rows, cols, had_full_screen_region, TabStopOrigin::Set);
        Some(ResizeMutation {
            old_rows,
            old_cols,
            rows,
            cols,
            transferred,
            dirty_rows: 0..rows,
        })
    }

    /// Resize both actual FrostLegacyV1 grids and transfer any displaced
    /// primary rows. A resize to the current dimensions still repairs wide
    /// pairs, clears pending wrap, and advances the physical grid revision.
    ///
    /// # Panics
    /// Both dimensions must be nonzero. The host applies its admission limits
    /// before calling and settles history and view effects before the next feed.
    pub fn resize_frost_legacy(&mut self, rows: usize, cols: usize) -> ResizeMutation {
        assert!(rows > 0 && cols > 0);
        let old_rows = self.grid.rows();
        let old_cols = self.grid.cols();
        let had_full_screen_region =
            self.scroll_region_top == 0 && self.scroll_region_bottom >= old_rows - 1;
        let blank = self.create_blank_cell();
        let mut transferred = SmallVec::new();
        if !self.use_alt_buffer && rows < old_rows {
            let transfer_top =
                rows_to_archive_on_height_shrink(old_rows, rows, self.cursor_row, |row| {
                    self.grid[row]
                        .iter()
                        .all(TerminalCell::is_reflow_trimmable_blank)
                });
            if transfer_top > 0 {
                transferred
                    .extend((0..transfer_top).map(|row| self.grid.archive_frost_legacy_row(row)));
                self.grid.scroll_up_by(transfer_top, blank);
                self.cursor_row -= transfer_top;
                self.saved_cursor_row = self.saved_cursor_row.saturating_sub(transfer_top);
                if let Some(saved) = self.saved_state.as_mut() {
                    saved.row = saved.row.saturating_sub(transfer_top);
                }
            }
        }

        let mut hidden_blank = blank;
        if self.use_alt_buffer {
            hidden_blank.background = self
                .saved_primary_screen_state
                .as_ref()
                .map_or(Color::Default, |state| state.bg);
        }
        self.grid.resize_frost_legacy(rows, cols, blank);
        self.alt_grid.resize_frost_legacy(rows, cols, hidden_blank);
        self.finish_physical_resize(rows, cols, had_full_screen_region, TabStopOrigin::Clear);
        ResizeMutation {
            old_rows,
            old_cols,
            rows,
            cols,
            transferred,
            dirty_rows: 0..rows,
        }
    }

    fn finish_physical_resize(
        &mut self,
        rows: usize,
        cols: usize,
        had_full_screen_region: bool,
        tab_stop_origin: TabStopOrigin,
    ) {
        self.pending_wrap = false;
        self.cursor_row = self.cursor_row.min(rows - 1);
        self.cursor_col = self.cursor_col.min(cols - 1);
        self.saved_cursor_row = self.saved_cursor_row.min(rows - 1);
        self.saved_cursor_col = self.saved_cursor_col.min(cols - 1);
        self.alt_cursor_row = self.alt_cursor_row.min(rows - 1);
        self.alt_cursor_col = self.alt_cursor_col.min(cols - 1);
        resize_tab_stops(&mut self.tab_stops, cols, tab_stop_origin);
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
        self.scroll_rows(direction, top..=bottom, 1, capture_outgoing, allocate_id)
    }

    pub(crate) fn scroll_rows(
        &mut self,
        direction: RegionDirection,
        region: RangeInclusive<usize>,
        count: usize,
        capture_outgoing: bool,
        allocate_id: impl FnMut() -> RawRowId,
    ) -> Option<RegionMutation> {
        self.shift_region(
            direction,
            region,
            count,
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

    /// Adopt FrostLegacyV1's process-wide row identities and tab stops.
    /// Panics for zero dimensions, as does `new`.
    pub fn new_frost_legacy(rows: usize, cols: usize) -> Self {
        Self::with_grid_pair(
            TerminalGrid::with_row_ids(rows, cols, RawRowId::allocate_process_unique),
            TerminalGrid::with_row_ids(rows, cols, RawRowId::allocate_process_unique),
            TabStopOrigin::Clear,
        )
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
        if self.uses_british_charset() && ch == '#' {
            '£'
        } else {
            self.active_charset.translate(ch)
        }
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
        self.write_char_profile(ch, width, autowrap, WriteProfile::Ember)
    }

    /// Frost's translated REP source, IRM edge repair and continuation style.
    /// The host finishes wrapping and records output ownership before this call.
    pub fn write_frost_legacy_char_in_row(
        &mut self,
        ch: char,
        width: usize,
        autowrap: bool,
    ) -> RowMutation {
        self.write_char_profile(ch, width, autowrap, WriteProfile::FrostLegacy)
    }

    fn write_char_profile(
        &mut self,
        ch: char,
        width: usize,
        autowrap: bool,
        profile: WriteProfile,
    ) -> RowMutation {
        assert!(width > 0 && (profile == WriteProfile::FrostLegacy || width <= 2));
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
            match profile {
                WriteProfile::Ember => {
                    for _ in 0..width {
                        self.grid.insert_cell_in_row(row, col, blank);
                    }
                    self.grid.clear_dangling_wide_at_row_end(row, blank);
                }
                WriteProfile::FrostLegacy => {
                    self.insert_frost_legacy_blanks(width);
                }
            }
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
            match profile {
                WriteProfile::Ember => WideContinuationProtection::FromLead,
                WriteProfile::FrostLegacy => WideContinuationProtection::FromBlank,
            },
        );
        self.cursor_col += width;
        if self.cursor_col >= cols {
            self.cursor_col = cols.saturating_sub(width);
            if autowrap {
                self.pending_wrap = true;
            }
        }
        if profile == WriteProfile::FrostLegacy {
            self.last_printed_char = Some(ch);
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
        self.step_narrow_write(bytes, autowrap, Self::write_ascii_in_row)
    }

    /// The caller has consumed any one-shot character-set shift on the first
    /// scalar. Map the remaining printable bytes with the active character set.
    pub(crate) fn step_mapped_ascii_write(
        &mut self,
        bytes: &[u8],
        autowrap: bool,
    ) -> AsciiWriteStep {
        debug_assert!(!self.single_shift_pending());
        self.step_narrow_write(bytes, autowrap, Self::write_mapped_ascii_in_row)
    }

    #[inline]
    fn step_narrow_write(
        &mut self,
        bytes: &[u8],
        autowrap: bool,
        write: impl FnOnce(&mut Self, &[u8], bool) -> Option<(usize, RowMutation)>,
    ) -> AsciiWriteStep {
        if bytes.is_empty() {
            return AsciiWriteStep::Stopped;
        }
        if self.take_pending_wrap(autowrap) {
            return AsciiWriteStep::NeedsWrap;
        }
        match write(self, bytes, autowrap) {
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
        self.write_ascii_profile(bytes, autowrap, WriteProfile::Ember)
    }

    /// Frost records REP once for the whole parser run before the first wrap.
    /// Keep that source while sharing bounded writes and cursor advancement.
    pub fn write_frost_legacy_ascii_in_row(
        &mut self,
        bytes: &[u8],
        autowrap: bool,
    ) -> Option<(usize, RowMutation)> {
        self.write_ascii_profile(bytes, autowrap, WriteProfile::FrostLegacy)
    }

    fn write_ascii_profile(
        &mut self,
        bytes: &[u8],
        autowrap: bool,
        profile: WriteProfile,
    ) -> Option<(usize, RowMutation)> {
        let written = self.write_narrow_in_row(
            bytes.len(),
            autowrap,
            |grid, row, columns, rendition, blank| {
                grid.overwrite_ascii(
                    row,
                    columns.start,
                    &bytes[..columns.len()],
                    rendition,
                    blank,
                    match profile {
                        WriteProfile::Ember => AsciiOverwritePolicy::SharedScreen,
                        WriteProfile::FrostLegacy => AsciiOverwritePolicy::FrostLegacy,
                    },
                );
            },
        )?;
        if profile == WriteProfile::Ember {
            self.last_printed_char = Some(char::from(bytes[written.0 - 1]));
        }
        Some(written)
    }

    fn write_mapped_ascii_in_row(
        &mut self,
        bytes: &[u8],
        autowrap: bool,
    ) -> Option<(usize, RowMutation)> {
        let charset = self.active_charset;
        let british = self.uses_british_charset();
        let map = |byte| {
            if british && byte == b'#' {
                '£'
            } else {
                charset.translate(char::from(byte))
            }
        };
        let written = self.write_narrow_in_row(
            bytes.len(),
            autowrap,
            |grid, row, columns, rendition, blank| {
                grid.overwrite_mapped_ascii(
                    row,
                    columns.start,
                    &bytes[..columns.len()],
                    rendition,
                    blank,
                    map,
                );
            },
        )?;
        self.last_printed_char = Some(map(bytes[written.0 - 1]));
        Some(written)
    }

    pub(crate) fn write_repeated_narrow_in_row(
        &mut self,
        character: char,
        requested: usize,
        autowrap: bool,
    ) -> Option<(usize, RowMutation)> {
        let written = self.write_narrow_in_row(
            requested,
            autowrap,
            |grid, row, columns, mut rendition, blank| {
                rendition.character = character;
                grid.overwrite_repeated_narrow(row, columns, rendition, blank);
            },
        )?;
        self.last_printed_char = Some(character);
        Some(written)
    }

    fn write_narrow_in_row(
        &mut self,
        requested: usize,
        autowrap: bool,
        overwrite: impl FnOnce(&mut TerminalGrid, usize, Range<usize>, TerminalCell, TerminalCell),
    ) -> Option<(usize, RowMutation)> {
        assert!(!self.insert_mode);
        let cols = self.grid.cols();
        let col = self.cursor_col;
        let row = self.cursor_row;
        let count = requested.min(cols.saturating_sub(col));
        if count == 0 {
            return None;
        }
        let effect = self.row_mutation(row, col, col + count, Some(col..col + count));
        let blank = self.create_blank_cell();
        overwrite(
            &mut self.grid,
            row,
            col..col + count,
            TerminalCell {
                character: ' ',
                foreground: self.current_fg,
                background: self.current_bg,
                flags: self.current_flags,
                hyperlink_id: self.current_hyperlink,
            },
            blank,
        );
        self.cursor_col += count;
        if self.cursor_col >= cols {
            self.cursor_col = cols - 1;
            if autowrap {
                self.pending_wrap = true;
            }
        }
        Some((count, effect))
    }

    pub fn combining_target(&self) -> Option<(usize, char)> {
        self.combining_target_profile(WriteProfile::Ember)
    }

    fn combining_target_profile(&self, profile: WriteProfile) -> Option<(usize, char)> {
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
            && (col > 0 || profile == WriteProfile::Ember)
        {
            col = col.checked_sub(1)?;
        }
        let ch = self.grid.get(self.cursor_row, col).character;
        (profile == WriteProfile::FrostLegacy || !matches!(ch, ' ' | '\0')).then_some((col, ch))
    }

    /// Ember commits only changed single-scalar compositions and retains
    /// explicit underline color through the ordinary replacement primitive.
    pub fn compose_combining_mark(&mut self, mark: char) -> Option<RowMutation> {
        self.compose_mark_profile(mark, WriteProfile::Ember)
    }

    /// Frost invalidates the mutable row before normalizing, including when
    /// the pair remains unrepresentable. A successful composition dirties its
    /// cell even when the resulting scalar matches the previous value.
    pub fn compose_frost_legacy_mark(&mut self, mark: char) -> Option<RowMutation> {
        self.compose_mark_profile(mark, WriteProfile::FrostLegacy)
    }

    fn compose_mark_profile(&mut self, mark: char, profile: WriteProfile) -> Option<RowMutation> {
        let (col, base) = self.combining_target_profile(profile)?;
        if profile == WriteProfile::Ember {
            let composed = compose_scalar_pair(base, mark)?;
            return (composed != base).then(|| self.replace_combining_target(col, composed));
        }
        let row = self.cursor_row;
        let mutation = RowMutation {
            row,
            row_id: self.grid.row_id(row),
            changed: col..col + 1,
            printed: Some(col..col + 1),
        };
        let cell = self.grid.get_mut(row, col);
        cell.character = compose_scalar_pair(cell.character, mark)?;
        Some(mutation)
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

    /// Frost uses the same wide-cell boundary edit for ICH and printable IRM.
    /// It preserves the deferred-wrap latch.
    pub fn insert_frost_legacy_blanks(&mut self, count: usize) -> bool {
        let blank = self.create_blank_cell();
        self.grid.insert_blank_cells(
            self.cursor_row,
            self.cursor_col,
            count,
            blank,
            WideCellEditPolicy::ClearBoundaryPairs,
        )
    }

    fn selective_erase_protected(&self, row: usize, col: usize) -> bool {
        let flags = self.grid.get(row, col).flags;
        if flags.protected() {
            return true;
        }
        if flags.wide() && !flags.wide_continuation() && col + 1 < self.grid.cols() {
            let next = self.grid.get(row, col + 1).flags;
            if next.wide_continuation() && !next.wide() {
                return next.protected();
            }
        }
        if flags.wide_continuation() && !flags.wide() && col > 0 {
            let previous = self.grid.get(row, col - 1).flags;
            if previous.wide() && !previous.wide_continuation() {
                return previous.protected();
            }
        }
        false
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
            if selective && self.selective_erase_protected(row, col) {
                col += 1;
                continue;
            }
            let start = col;
            col += 1;
            while col < end && !(selective && self.selective_erase_protected(row, col)) {
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
    /// XTerm mode45 reverses only across physical rows joined by autowrap.
    /// The pending last-column position consumes one backward step before
    /// physical motion. Never enter retained history or alter row metadata.
    pub(crate) fn move_cursor_backward_with_wrap(
        &mut self,
        count: usize,
        protocol: &ProtocolState,
    ) {
        if !protocol.modes().contains(&45) || !protocol.modes().contains(&7) {
            self.move_cursor_backward(count);
            return;
        }
        let mut remaining = count.saturating_sub(usize::from(self.pending_wrap));
        self.pending_wrap = false;
        let (mut row, mut col) = (self.cursor_row, self.cursor_col);
        while remaining != 0 {
            let within_row = remaining.min(col);
            col -= within_row;
            remaining -= within_row;
            if remaining == 0 || row == 0 || !self.grid.row_wrapped[row - 1] {
                break;
            }
            row -= 1;
            col = self.grid.cols() - 1;
            remaining -= 1;
        }
        self.cursor_row = row;
        self.cursor_col = col;
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
    pub fn reset_eight_column_tab_stops(&mut self) {
        for (column, stop) in self.tab_stops.iter_mut().enumerate() {
            *stop = column != 0 && column % 8 == 0;
        }
    }
    pub fn set_scroll_region(&mut self, top: usize, bottom: usize) {
        self.cursor_controls().set_scroll_region(top, bottom);
    }
}

#[cfg(test)]
mod composition_tests {
    use super::*;

    #[test]
    fn scalar_pair_keeps_the_single_scalar_nfc_contract() {
        assert_eq!(compose_scalar_pair('e', '\u{0301}'), Some('é'));
        assert_eq!(compose_scalar_pair('\u{1100}', '\u{1161}'), Some('가'));
        assert_eq!(compose_scalar_pair('가', '\u{11a8}'), Some('각'));
        assert_eq!(compose_scalar_pair('e', '\u{200d}'), None);
        assert_eq!(compose_scalar_pair(' ', '\u{0301}'), None);
        assert_eq!(compose_scalar_pair('e', '\u{05c8}'), None);
    }

    #[test]
    fn composition_profiles_preserve_their_original_row_invalidation_and_attributes() {
        for frost in [false, true] {
            for (mark, accepted) in [('\u{0301}', true), ('\u{200d}', false)] {
                let mut screen = ScreenState::new(1, 3);
                screen.grid.get_mut(0, 1).character = 'e';
                screen.grid.set_cluster_text(0, 1, "e\u{fe0e}").unwrap();
                screen.grid.set_underline_color(0, 1..2, Color::Indexed(3));
                screen.grid.begin_capture_revision(1);
                screen.grid.begin_capture_revision(2);
                screen.cursor_col = 2;
                screen.last_printed_char = Some('e');
                let effect = if frost {
                    screen.compose_frost_legacy_mark(mark)
                } else {
                    screen.compose_combining_mark(mark)
                };
                assert_eq!(effect.is_some(), accepted);
                assert_eq!(
                    screen.grid.get(0, 1).character,
                    if accepted { 'é' } else { 'e' }
                );
                assert_eq!(screen.grid.row_changed_after(0, 1), frost || accepted);
                assert_eq!(
                    screen.grid.cluster_text(0, 1),
                    if frost || accepted {
                        None
                    } else {
                        Some("e\u{fe0e}")
                    }
                );
                assert_eq!(
                    screen.grid.underline_colors(0)[1],
                    if frost {
                        Color::Default
                    } else {
                        Color::Indexed(3)
                    }
                );
                assert_eq!(screen.cursor_col, 2);
                assert!(!screen.pending_wrap);
                assert_eq!(screen.last_printed_char, Some('e'));
            }
        }
    }
}

#[cfg(test)]
mod charset_bank_tests {
    use super::*;
    #[test]
    fn single_shift_consumes_only_gl_graphics_and_locking_preserves_it() {
        let mut screen = ScreenState::new(2, 8);
        screen.designate_charset_bank(2, Charset::DecSpecialGraphics);
        screen.designate_charset_bank(3, Charset::Ascii);
        screen.single_shift(2);
        screen.invoke_charset_bank(3);
        for ch in ['\n', '\u{7f}', 'é', '界', '\u{301}'] {
            assert_eq!(screen.take_graphic_charset(ch), Charset::Ascii);
            assert!(screen.single_shift_pending());
        }
        assert_eq!(screen.take_graphic_charset('q').translate('q'), '─');
        assert!(!screen.single_shift_pending());
        assert_eq!(screen.invoked_charset_bank(), 3);
        assert_eq!(screen.take_graphic_charset('q').translate('q'), 'q');
        screen.single_shift(2);
        assert_eq!(
            screen.take_graphic_charset(' '),
            Charset::DecSpecialGraphics
        );
        assert!(!screen.single_shift_pending());
        screen.invoke_charset_bank(2);
        screen.designate_charset_bank(3, Charset::DecSpecialGraphics);
        assert_eq!(screen.invoked_charset_bank(), 2);
        screen.designate_charset_bank(2, Charset::Ascii);
        assert_eq!(screen.active_charset, Charset::Ascii);
        assert_eq!(screen.invoked_charset_bank(), 2);
    }
    #[test]
    fn cursor_and_alternate_snapshots_restore_all_banks_and_pending_shift() {
        let mut screen = ScreenState::new(2, 8);
        screen.designate_charset_bank(2, Charset::DecSpecialGraphics);
        screen.designate_charset_bank(3, Charset::DecSpecialGraphics);
        screen.invoke_charset_bank(3);
        screen.single_shift(2);
        screen.save_cursor(true);
        screen.reset_charset_banks();
        assert_eq!(screen.restore_cursor(), Some(true));
        assert_eq!(screen.invoked_charset_bank(), 3);
        assert_eq!(screen.single_shift_bank(), Some(2));
        assert_eq!(screen.charset_bank(2), Charset::DecSpecialGraphics);
        assert_eq!(screen.charset_bank(3), Charset::DecSpecialGraphics);
        let protocol = ProtocolState::default();
        assert!(screen
            .enter_alternate_screen(true, false, &protocol)
            .is_some());
        screen.reset_charset_banks();
        assert!(screen.leave_alternate_screen(true).is_some());
        assert_eq!(screen.invoked_charset_bank(), 3);
        assert_eq!(screen.single_shift_bank(), Some(2));
        assert_eq!(screen.charset_bank(2), Charset::DecSpecialGraphics);
        screen.reset_charset_banks();
        assert_eq!(screen.invoked_charset_bank(), 0);
        assert!(!screen.single_shift_pending());
        assert_eq!(screen.charset_bank(2), Charset::Ascii);
        assert_eq!(screen.charset_bank(3), Charset::Ascii);
    }
    #[test]
    fn gr_fill_mapping_is_separate_from_unicode_gl_and_single_shift() {
        let mut screen = ScreenState::new(2, 8);
        assert_eq!(screen.invoked_gr_charset_bank(), 2);
        for value in 160..=255 {
            assert_eq!(screen.translate_fill_character(value), char::from(value));
        }
        screen.designate_charset_bank(1, Charset::DecSpecialGraphics);
        screen.invoke_gr_charset_bank(1);
        assert_eq!(screen.invoked_charset_bank(), 0);
        assert_eq!(screen.translate_fill_character(0xf1), '─');
        assert_eq!(screen.translate_fill_character(0xa0), ' ');
        assert_eq!(screen.translate_fill_character(0xff), ' ');
        assert_eq!(screen.translate_fill_character(0xdf), 'ß');
        assert_eq!(screen.translate_fill_character(0xa3), '£');
        assert_eq!(screen.take_graphic_charset('ñ').translate('ñ'), 'ñ');
        assert_eq!(screen.translate_fill_character(b'q'), 'q');
        screen.single_shift(2); // ASCII single shift does not suppress GR mapping.
        assert_eq!(screen.translate_fill_character(0xf1), '─');
        assert_eq!(screen.single_shift_bank(), Some(2));
        screen.designate_charset_bank(2, Charset::DecSpecialGraphics);
        assert_eq!(screen.translate_fill_character(b'q'), '─');
        assert_eq!(screen.single_shift_bank(), Some(2));
        assert_eq!(screen.take_graphic_charset('q').translate('q'), '─');
        assert!(!screen.single_shift_pending());
        assert_eq!(screen.take_graphic_charset('q').translate('q'), 'q');
        screen.invoke_gr_charset_bank(4);
        assert_eq!(screen.invoked_gr_charset_bank(), 1);
    }
    #[test]
    fn gr_invocation_survives_cursor_and_alternate_snapshots_and_resets() {
        let mut screen = ScreenState::new(2, 8);
        for gr in 0..4 {
            screen.invoke_gr_charset_bank(gr);
            screen.save_cursor(true);
            screen.invoke_gr_charset_bank((gr + 1) % 4);
            screen.restore_cursor();
            assert_eq!(screen.invoked_gr_charset_bank(), gr);
            assert!(screen
                .enter_alternate_screen(true, false, &ProtocolState::default())
                .is_some());
            screen.invoke_gr_charset_bank((gr + 1) % 4);
            assert!(screen.leave_alternate_screen(true).is_some());
            assert_eq!(screen.invoked_gr_charset_bank(), gr);
        }
        screen.reset_charset_banks();
        assert_eq!(screen.invoked_gr_charset_bank(), 2);
    }
    #[test]
    fn latin1_designation_preserves_identity_and_retires_on_94_redesignation() {
        let mut screen = ScreenState::new(2, 8);
        for index in 0..4 {
            screen.designate_latin1(index);
            assert!(screen.charset_is_latin1(index));
            assert_eq!(screen.charset_bank(index), Charset::Ascii);
            screen.invoke_charset_bank(index);
            assert_eq!(screen.take_graphic_charset('q').translate('q'), 'q');
            screen.invoke_gr_charset_bank(index);
            for value in [0xa0, 0xa3, 0xf1, 0xff] {
                assert_eq!(screen.translate_fill_character(value), char::from(value));
            }
            screen.designate_charset_bank(index, Charset::DecSpecialGraphics);
            assert!(!screen.charset_is_latin1(index));
            screen.designate_latin1(index);
        }
        screen.designate_charset(false, Charset::Ascii);
        screen.designate_charset(true, Charset::Ascii);
        assert!(!screen.charset_is_latin1(0));
        assert!(!screen.charset_is_latin1(1));
        screen.designate_latin1(0);
        screen.g0_charset = Charset::DecSpecialGraphics;
        assert!(!screen.charset_is_latin1(0));
        screen.designate_latin1(9);
        assert!(!screen.charset_is_latin1(9));
    }
    #[test]
    fn latin1_designation_snapshots_and_reset_preserve_private_wire_state() {
        let mut screen = ScreenState::new(2, 8);
        for index in 0..4 {
            screen.designate_latin1(index);
        }
        screen.invoke_charset_bank(1);
        screen.invoke_gr_charset_bank(3);
        screen.single_shift(2);
        screen.save_cursor(true);
        screen.reset_charset_banks();
        screen.restore_cursor();
        for index in 0..4 {
            assert!(screen.charset_is_latin1(index));
        }
        assert_eq!(screen.invoked_charset_bank(), 1);
        assert_eq!(screen.invoked_gr_charset_bank(), 3);
        assert_eq!(screen.single_shift_bank(), Some(2));
        screen.enter_alternate_screen(true, false, &ProtocolState::default());
        screen.reset_charset_banks();
        screen.leave_alternate_screen(true);
        for index in 0..4 {
            assert!(screen.charset_is_latin1(index));
        }
        screen.reset_charset_banks();
        for index in 0..4 {
            assert!(!screen.charset_is_latin1(index));
        }
    }
    #[test]
    fn unsaved_cursor_restore_resets_only_drawing_state_and_keeps_decawm() {
        for autowrap in [false, true] {
            let mut screen = ScreenState::new(3, 8);
            let mut protocol = ProtocolState::default();
            if autowrap {
                protocol.modes_mut().insert(7);
            }
            protocol.modes_mut().insert(25);
            screen.cursor_row = 2;
            screen.cursor_col = 7;
            screen.pending_wrap = true;
            screen.current_fg = Color::Red;
            screen.current_bg = Color::Blue;
            screen.current_flags.set_bold(true);
            screen.current_flags.set_protected(true);
            screen.current_hyperlink = HyperlinkId::from_raw(4);
            screen.origin_mode = true;
            screen.insert_mode = true;
            screen.scroll_region_top = 1;
            screen.scroll_region_bottom = 2;
            screen.tab_stops[3] = true;
            screen.designate_charset_bank(0, Charset::DecSpecialGraphics);
            screen.designate_latin1(1);
            screen.designate_latin1(2);
            screen.designate_charset_bank(3, Charset::DecSpecialGraphics);
            screen.invoke_charset_bank(3);
            screen.invoke_gr_charset_bank(1);
            screen.single_shift(2);
            screen.grid.get_mut(1, 2).character = 'X';
            screen.grid.row_wrapped[1] = true;
            let cells = screen.grid.to_vec();
            let tabs = screen.tab_stops.clone();
            let ids: Vec<_> = (0..3).map(|r| screen.grid.row_id(r)).collect();
            screen.restore_cursor_state(&mut protocol);
            assert_eq!(
                (
                    screen.cursor_row,
                    screen.cursor_col,
                    screen.origin_mode,
                    screen.pending_wrap
                ),
                (0, 0, false, false)
            );
            assert_eq!(
                (screen.current_fg, screen.current_bg, screen.current_flags),
                (Color::Default, Color::Default, StyleFlags::default())
            );
            for index in 0..4 {
                assert_eq!(screen.charset_bank(index), Charset::Ascii);
                assert!(!screen.charset_is_latin1(index));
            }
            assert_eq!(screen.invoked_charset_bank(), 0);
            assert_eq!(screen.invoked_gr_charset_bank(), 2);
            assert!(!screen.single_shift_pending());
            assert_eq!(screen.grid.to_vec(), cells);
            assert_eq!(screen.tab_stops, tabs);
            assert_eq!(
                (0..3).map(|r| screen.grid.row_id(r)).collect::<Vec<_>>(),
                ids
            );
            assert!(
                screen.grid.row_wrapped[1] && screen.insert_mode && protocol.modes().contains(&25)
            );
            assert_eq!(protocol.modes().contains(&7), autowrap);
            assert_eq!(
                (screen.scroll_region_top, screen.scroll_region_bottom),
                (1, 2)
            );
            assert_eq!(screen.current_hyperlink, HyperlinkId::from_raw(4));
            assert!(screen.saved_state.is_none());
        }
    }
    #[test]
    fn reverse_wrap_matches_independent_physical_step_reference() {
        let mut protocol = ProtocolState::default();
        protocol.modes_mut().insert(7);
        protocol.modes_mut().insert(45);
        for wrapped in 0..8 {
            for start_row in 0..3 {
                for start_col in 0..4 {
                    for pending in [false, true] {
                        for count in 1..=20 {
                            let mut screen = ScreenState::new(3, 4);
                            for row in 0..3 {
                                screen.grid.row_wrapped[row] = wrapped & (1 << row) != 0;
                            }
                            screen.cursor_row = start_row;
                            screen.cursor_col = start_col;
                            screen.pending_wrap = pending;
                            screen.scroll_region_top = 1;
                            screen.scroll_region_bottom = 2;
                            let (mut row, mut col) = (start_row, start_col);
                            let mut pending = pending;
                            for _ in 0..count {
                                if pending {
                                    pending = false;
                                } else if col > 0 {
                                    col -= 1;
                                } else if row > 0 && wrapped & (1 << (row - 1)) != 0 {
                                    row -= 1;
                                    col = 3;
                                }
                            }
                            screen.move_cursor_backward_with_wrap(count, &protocol);
                            assert_eq!(
                                (screen.cursor_row, screen.cursor_col, screen.pending_wrap),
                                (row, col, false)
                            );
                        }
                    }
                }
            }
        }
    }
    #[test]
    fn reverse_wrap_preserves_legacy_off_policy_and_never_changes_row_contents() {
        let mut protocol = ProtocolState::default();
        let mut screen = ScreenState::new(3, 4);
        screen.grid.row_wrapped[0] = true;
        screen.grid.get_mut(0, 1).flags.set_wide(true);
        screen.grid.get_mut(0, 2).flags.set_wide_continuation(true);
        screen.cursor_row = 1;
        screen.cursor_col = 0;
        screen.move_cursor_backward_with_wrap(1, &protocol);
        assert_eq!((screen.cursor_row, screen.cursor_col), (1, 0));
        protocol.modes_mut().insert(45);
        screen.move_cursor_backward_with_wrap(1, &protocol);
        assert_eq!((screen.cursor_row, screen.cursor_col), (1, 0));
        protocol.modes_mut().insert(7);
        screen.scroll_region_top = 1;
        screen.scroll_region_bottom = 2;
        let cells = screen.grid.to_vec();
        let ids: Vec<_> = (0..3).map(|r| screen.grid.row_id(r)).collect();
        screen.move_cursor_backward_with_wrap(1, &protocol);
        assert_eq!((screen.cursor_row, screen.cursor_col), (0, 3)); // Soft wrap crosses vertical margin.
        screen.move_cursor_backward_with_wrap(1, &protocol);
        assert_eq!(screen.cursor_col, 2); // Physical continuation column is not normalized.
        screen.move_cursor_backward_with_wrap(usize::MAX, &protocol);
        assert_eq!((screen.cursor_row, screen.cursor_col), (0, 0));
        assert_eq!(screen.grid.to_vec(), cells);
        assert!(screen.grid.row_wrapped[0]);
        assert_eq!(
            (0..3).map(|r| screen.grid.row_id(r)).collect::<Vec<_>>(),
            ids
        );
        protocol.modes_mut().remove(&45);
        screen.cursor_col = 3;
        screen.pending_wrap = true;
        screen.move_cursor_backward_with_wrap(1, &protocol);
        assert_eq!((screen.cursor_col, screen.pending_wrap), (2, false));
    }
    #[test]
    fn selective_erasure_protects_valid_wide_pairs_from_either_half() {
        for protected_continuation in [false, true] {
            for requested in [0..1, 1..2, 0..2] {
                let mut screen = ScreenState::new(1, 5);
                screen.current_flags.set_protected(true);
                screen.write_frost_legacy_char_in_row('界', 2, true);
                assert!(screen.grid.get(0, 0).flags.protected());
                assert!(!screen.grid.get(0, 1).flags.protected());
                if protected_continuation {
                    screen.grid.get_mut(0, 0).flags.set_protected(false);
                    screen.grid.get_mut(0, 1).flags.set_protected(true);
                }
                screen.current_flags.set_protected(false);
                screen.pending_wrap = true;
                let before = screen.grid.to_vec();
                assert!(screen
                    .erase_span(0, requested.start, requested.end, true)
                    .is_empty());
                assert_eq!(screen.grid.to_vec(), before);
                assert!(screen.pending_wrap);
                let mutations = screen.erase_span(0, requested.start, requested.end, false);
                assert_eq!(mutations.len(), 1);
                assert_eq!(screen.grid.get(0, 0).character, ' ');
                assert_eq!(screen.grid.get(0, 1).character, ' ');
            }
        }
    }
}
