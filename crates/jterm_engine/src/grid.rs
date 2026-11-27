use super::hyperlink::HyperlinkId;
use crate::{graphemes::MAX_CLUSTER_BYTES, row_annotations::RowAnnotations};
use smallvec::SmallVec;
use std::ops::{Range, RangeInclusive};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Reusable snapshot of one source row for overlap-safe rectangular copies.
#[derive(Default)]
pub(crate) struct RectangularCopyScratch {
    cells: Vec<TerminalCell>,
    colors: Vec<Color>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegionDirection {
    Up,
    Down,
}

/// How a physical row shift treats the legacy wrap flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegionWrapPolicy {
    MoveWithRows,
    KeepPhysicalIndices,
}

/// Wide-cell compatibility when a character edit changes row coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WideCellEditPolicy {
    /// Repair pairs split by the affected range or the edge of the row.
    RepairSplitPairs,
    /// Clear a complete pair when either edit boundary touches it.
    ClearBoundaryPairs,
}

/// Edge repair and metadata rules for an in-row ASCII overwrite.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AsciiOverwritePolicy {
    /// Inspect adjacent cells and preserve untouched row metadata.
    SharedScreen,
    /// Inspect the overwritten edge cells and invalidate the mutable row view.
    FrostLegacy,
}

/// Source of the selective-erase protection bit on a wide continuation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WideContinuationProtection {
    FromLead,
    FromBlank,
}

/// Explicit underline color for a half-open range of physical columns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnderlineColorRun {
    pub start: u16,
    pub end: u16,
    pub color: Color,
}

#[derive(Clone, Copy)]
enum MarginOrigin {
    /// A newly allocated right cell with exact default attributes.
    VirginDefault,
    Unknown,
    WideGap,
}

/// Reversible footprint of a glyph whose presentation can change as bytes arrive.
pub(crate) struct PrintCheckpoint {
    cols: Range<usize>,
    cells: SmallVec<[TerminalCell; 4]>,
    colors: SmallVec<[Color; 4]>,
    annotations: Option<Arc<RowAnnotations>>,
    margin_origin: MarginOrigin,
}

pub(crate) struct WrapCheckpoint {
    row: usize,
    wrapped: bool,
    margin_origin: MarginOrigin,
}

impl PrintCheckpoint {
    pub(crate) fn row_annotations(&self) -> Option<&RowAnnotations> {
        self.annotations.as_deref()
    }
}

/// 连续内存网格存储 - 优化内存局部性和缓存命中率
/// 内存布局：`cells[row * cols + col]` 对应 `grid[row][col]`。
#[derive(Clone)]
pub struct TerminalGrid {
    cells: Vec<TerminalCell>,
    rows: usize,
    cols: usize,
    /// Legacy wrap bits. Hosts opting into certified archive gaps must use
    /// engine structural edits, not rewrite these bits independently of cells.
    pub row_wrapped: Vec<bool>,
    /// Stable identity travels with retained physical rows across structural
    /// moves. `UNTRACKED` is fail-closed after allocator exhaustion.
    row_ids: Vec<RawRowId>,
    identity_revision: u64,
    margin_origin: Vec<MarginOrigin>,
    // Allocate the row table on first colored output, and allocate cells only
    // for rows with explicit decoration. The base cell stays 16 bytes.
    underline_colors: Vec<Option<Vec<Color>>>,
    annotations: Vec<Option<Arc<RowAnnotations>>>,
    capture_revision: u64,
    last_row_change: u64,
    row_changes: Vec<u64>,
    frame_base_revision: u64,
    scroll_origins_revision: u64,
    scroll_origins: Vec<Option<usize>>,
    pub(crate) track_printed_spaces: bool,
}

impl TerminalGrid {
    pub fn new(rows: usize, cols: usize) -> Self {
        TerminalGrid {
            cells: vec![TerminalCell::default(); rows * cols],
            rows,
            cols,
            row_wrapped: vec![false; rows],
            row_ids: vec![RawRowId::UNTRACKED; rows],
            identity_revision: 1,
            margin_origin: vec![MarginOrigin::VirginDefault; rows],
            underline_colors: Vec::new(),
            annotations: Vec::new(),
            capture_revision: 0,
            last_row_change: 0,
            row_changes: Vec::new(),
            frame_base_revision: 0,
            scroll_origins_revision: 0,
            scroll_origins: Vec::new(),
            track_printed_spaces: false,
        }
    }

    /// Initialize the physical rows in ascending order with the caller's IDs.
    pub fn with_row_ids(
        rows: usize,
        cols: usize,
        mut allocate_id: impl FnMut() -> RawRowId,
    ) -> Self {
        let mut grid = Self::new(rows, cols);
        for id in &mut grid.row_ids {
            *id = allocate_id();
        }
        grid
    }

    pub(crate) fn begin_capture_revision(&mut self, revision: u64) {
        debug_assert!(revision >= self.capture_revision);
        self.capture_revision = revision;
        if self.row_changes.len() != self.rows {
            self.row_changes.resize(self.rows, revision);
            self.last_row_change = revision;
        }
    }

    pub(crate) fn set_frame_base_revision(&mut self, revision: u64) {
        self.frame_base_revision = revision;
    }

    /// Source row positions in a captured frame, before subsequent structural
    /// moves. An inserted row has no source even if its cells match old paint.
    pub(crate) fn scroll_origins(&self, revision: u64) -> Option<&[Option<usize>]> {
        (revision != 0 && self.scroll_origins_revision == revision)
            .then_some(self.scroll_origins.as_slice())
    }

    /// Grid users that do not start capture revisions have no change certificate.
    /// Legacy public wrap bits are compared independently of this certificate.
    pub(crate) fn row_changed_after(&self, row: usize, revision: u64) -> bool {
        self.row_changes
            .get(row)
            .is_none_or(|changed_at| *changed_at > revision)
    }

    // Wrap bits remain independently observable through the public legacy
    // vector; this certificate covers cells and row-owned paint metadata only.
    #[inline]
    pub(crate) fn captured_rows_unchanged_after(&self, revision: u64) -> bool {
        self.row_changes.len() == self.rows && self.last_row_change <= revision
    }

    #[inline]
    fn mark_row_changed(&mut self, row: usize) {
        if self.capture_revision != 0 {
            self.row_changes[row] = self.capture_revision;
            self.last_row_change = self.capture_revision;
        }
    }

    /// Flat cells with a fixed length. Hosts must preserve wide-cell invariants.
    pub fn cells_mut(&mut self) -> &mut [TerminalCell] {
        self.row_changes.fill(self.capture_revision);
        self.last_row_change = self.capture_revision;
        self.margin_origin.fill(MarginOrigin::Unknown);
        self.underline_colors = Vec::new();
        self.annotations = Vec::new();
        &mut self.cells
    }

    /// Stable row identities in physical row order.
    pub fn row_ids(&self) -> &[RawRowId] {
        &self.row_ids
    }

    /// Fixed-length identity view for host-owned allocation and region moves.
    /// The host must keep identities synchronized with physical cell moves.
    pub fn row_ids_mut(&mut self) -> &mut [RawRowId] {
        &mut self.row_ids
    }

    /// Revision of physical row shifts and resizes. Direct repairs to the
    /// tracking IDs and changes to cells have their own invalidation paths.
    pub fn identity_revision(&self) -> u64 {
        self.identity_revision
    }

    fn bump_identity_revision(&mut self) {
        self.identity_revision = self.identity_revision.wrapping_add(1).max(1);
    }

    pub fn row_slice(&self, row: usize) -> Option<&[TerminalCell]> {
        (row < self.rows).then(|| &self[row])
    }

    pub fn raw_row_id(&self, row: usize) -> Option<RawRowId> {
        self.row_ids.get(row).copied()
    }

    #[inline]
    pub fn get(&self, row: usize, col: usize) -> &TerminalCell {
        &self.cells[row * self.cols + col]
    }

    #[inline]
    pub fn get_mut(&mut self, row: usize, col: usize) -> &mut TerminalCell {
        self.mark_row_changed(row);
        if col == self.cols.saturating_sub(1) {
            self.margin_origin[row] = MarginOrigin::Unknown;
        }
        self.set_underline_color(row, col..col + 1, Color::Default);
        self.erase_annotations(row, col..col + 1);
        &mut self.cells[row * self.cols + col]
    }

    pub(crate) fn row_annotations(&self, row: usize) -> Option<&Arc<RowAnnotations>> {
        self.annotations.get(row)?.as_ref()
    }

    pub(crate) fn cluster_text(&self, row: usize, col: usize) -> Option<&str> {
        self.row_annotations(row)?.text(col)
    }

    pub(crate) fn set_cluster_text(
        &mut self,
        row: usize,
        col: usize,
        text: &str,
    ) -> Option<Arc<str>> {
        if text.len() > MAX_CLUSTER_BYTES
            || self
                .row_annotations(row)
                .is_some_and(|line| !line.can_store(col, text.len()))
        {
            return None;
        }
        self.mark_row_changed(row);
        self.annotations.resize_with(self.rows, || None);
        let line = self.annotations[row].get_or_insert_with(Arc::default);
        let text: Arc<str> = Arc::from(text);
        Arc::make_mut(line).set(col, text.clone());
        Some(text)
    }

    pub(crate) fn set_image(
        &mut self,
        row: usize,
        column: usize,
        image: crate::terminal::ImageAnnotation,
    ) {
        self.mark_row_changed(row);
        self.annotations.resize_with(self.rows, || None);
        let line = self.annotations[row].get_or_insert_with(Arc::default);
        Arc::make_mut(line).set_image(column, image);
    }

    fn edit_annotations(&mut self, row: usize, edit: impl FnOnce(&mut RowAnnotations)) {
        self.mark_row_changed(row);
        let Some(slot) = self.annotations.get_mut(row) else {
            return;
        };
        let Some(line) = slot else { return };
        let line = Arc::make_mut(line);
        edit(line);
        if line.is_empty() {
            *slot = None;
        }
    }

    pub(crate) fn erase_annotations(&mut self, row: usize, cols: Range<usize>) {
        if self
            .row_annotations(row)
            .is_some_and(|line| line.end() > cols.start)
        {
            self.edit_annotations(row, |line| line.erase(cols));
        }
    }

    pub(crate) fn checkpoint_print(
        &self,
        row: usize,
        col: usize,
        width: usize,
        insert: bool,
    ) -> PrintCheckpoint {
        // Insert mode may drop old suffix cells or clear a split wide pair at
        // either edge, so its reversible footprint includes the complete row.
        let cols = if insert {
            0..self.cols
        } else {
            self.expanded_wide_span(row, col..col + width)
        };
        let cells = SmallVec::from_slice(&self[row][cols.clone()]);
        let colors = self
            .underline_colors(row)
            .get(cols.clone())
            .map_or_else(SmallVec::new, SmallVec::from_slice);
        PrintCheckpoint {
            cols,
            cells,
            colors,
            annotations: self.row_annotations(row).cloned(),
            margin_origin: self.margin_origin[row],
        }
    }

    pub(crate) fn restore_print(&mut self, row: usize, checkpoint: PrintCheckpoint) {
        self.mark_row_changed(row);
        let start = row * self.cols + checkpoint.cols.start;
        self.cells[start..start + checkpoint.cells.len()].copy_from_slice(&checkpoint.cells);
        if let Some(Some(colors)) = self.underline_colors.get_mut(row) {
            if checkpoint.colors.is_empty() {
                colors[checkpoint.cols].fill(Color::Default);
            } else {
                colors[checkpoint.cols].copy_from_slice(&checkpoint.colors);
            }
        }
        // The live row table persists after an empty row's last cluster dies.
        if let Some(slot) = self.annotations.get_mut(row) {
            *slot = checkpoint.annotations;
        }
        self.margin_origin[row] = checkpoint.margin_origin;
    }

    pub(crate) fn checkpoint_wrap(&self, row: usize) -> WrapCheckpoint {
        WrapCheckpoint {
            row,
            wrapped: self.row_wrapped[row],
            margin_origin: self.margin_origin[row],
        }
    }

    pub(crate) fn restore_wrap(&mut self, checkpoint: WrapCheckpoint) {
        self.row_wrapped[checkpoint.row] = checkpoint.wrapped;
        self.margin_origin[checkpoint.row] = checkpoint.margin_origin;
    }

    pub(crate) fn underline_colors(&self, row: usize) -> &[Color] {
        self.underline_colors
            .get(row)
            .and_then(Option::as_deref)
            .unwrap_or_default()
    }

    pub(crate) fn underline_color_runs(
        &self,
        row: usize,
    ) -> impl Iterator<Item = UnderlineColorRun> + '_ {
        self.underline_colors(row)
            .chunk_by(|left, right| left == right)
            .scan(0, |column, cells| {
                let start = *column;
                *column += cells.len();
                Some((start, *column, cells[0]))
            })
            .filter_map(|(start, end, color)| {
                (color != Color::Default).then_some(UnderlineColorRun {
                    start: start as u16,
                    end: end as u16,
                    color,
                })
            })
    }

    pub(crate) fn set_underline_color(&mut self, row: usize, cols: Range<usize>, color: Color) {
        self.mark_row_changed(row);
        if color != Color::Default {
            debug_assert!(u16::try_from(self.cols).is_ok());
            self.underline_colors.resize_with(self.rows, || None);
            self.underline_colors[row].get_or_insert_with(|| vec![Color::Default; self.cols])[cols]
                .fill(color);
        } else if let Some(Some(line)) = self.underline_colors.get_mut(row) {
            line[cols].fill(Color::Default);
        }
    }

    fn shift_underline_colors(&mut self, row: usize, col: usize, count: usize, insert: bool) {
        if let Some(Some(line)) = self.underline_colors.get_mut(row) {
            let tail = &mut line[col..];
            let retained = tail.len() - count;
            if insert {
                tail.copy_within(..retained, count);
                tail[..count].fill(Color::Default);
            } else {
                tail.copy_within(count.., 0);
                tail[retained..].fill(Color::Default);
            }
        }
    }

    #[inline]
    pub fn rows(&self) -> usize {
        self.rows
    }

    #[inline]
    pub fn cols(&self) -> usize {
        self.cols
    }

    /// 返回行数（兼容 `grid[i].len()`）。
    #[inline]
    pub fn row_len(&self) -> usize {
        self.cols
    }

    #[inline]
    pub fn row_id(&self, row: usize) -> RawRowId {
        self.raw_row_id(row).unwrap_or(RawRowId::UNTRACKED)
    }

    /// Record exactly one wrap departure. An ordinary or ambiguous wrap must
    /// not carry an earlier proof through a new logical row relationship.
    pub(crate) fn record_wrap_margin(&mut self, row: usize, col: usize, early_wide: bool) {
        self.margin_origin[row] = if early_wide
            && self.cols >= 2
            && u16::try_from(self.cols).is_ok()
            && col == self.cols - 1
            && matches!(self.margin_origin[row], MarginOrigin::VirginDefault)
            && *self.get(row, col) == TerminalCell::default()
        {
            MarginOrigin::WideGap
        } else {
            MarginOrigin::Unknown
        };
    }

    pub(crate) fn synthetic_margin_gap(&self, row: usize) -> Option<usize> {
        (self.row_wrapped[row] && matches!(self.margin_origin[row], MarginOrigin::WideGap))
            .then(|| self.cols - 1)
    }

    pub(crate) fn row_cells_mut(
        &mut self,
        row: usize,
        cols: std::ops::Range<usize>,
    ) -> &mut [TerminalCell] {
        self.mark_row_changed(row);
        if cols.end == self.cols && !cols.is_empty() {
            self.margin_origin[row] = MarginOrigin::Unknown;
        }
        let base = row * self.cols;
        &mut self.cells[base + cols.start..base + cols.end]
    }

    /// Write printable ASCII to a physical row span. Hosts handle cursor
    /// movement, wraps and their own effects before and after this write.
    ///
    /// The caller provides a valid row and a span that fits within it.
    #[inline]
    pub fn overwrite_ascii(
        &mut self,
        row: usize,
        col: usize,
        bytes: &[u8],
        rendition: TerminalCell,
        blank: TerminalCell,
        policy: AsciiOverwritePolicy,
    ) {
        if bytes.is_empty() {
            return;
        }
        assert!(row < self.rows && bytes.len() <= self.cols.saturating_sub(col));
        let track_spaces = self.track_printed_spaces;
        debug_assert!(bytes.iter().all(|byte| (0x20..=0x7e).contains(byte)));
        let target = self.narrow_overwrite_cells(row, col..col + bytes.len(), blank, policy);
        Self::write_ascii_cells(target, bytes, rendition, track_spaces, char::from);
    }

    /// Write printable bytes mapped to single-column graphics. The supported
    /// character sets preserve spaces as well as the number of columns.
    pub(crate) fn overwrite_mapped_ascii(
        &mut self,
        row: usize,
        col: usize,
        bytes: &[u8],
        rendition: TerminalCell,
        blank: TerminalCell,
        map: impl Fn(u8) -> char,
    ) {
        let track_spaces = self.track_printed_spaces;
        let target = self.narrow_overwrite_cells(
            row,
            col..col + bytes.len(),
            blank,
            AsciiOverwritePolicy::SharedScreen,
        );
        Self::write_ascii_cells(target, bytes, rendition, track_spaces, map);
    }

    #[inline]
    fn narrow_overwrite_cells(
        &mut self,
        row: usize,
        columns: Range<usize>,
        blank: TerminalCell,
        policy: AsciiOverwritePolicy,
    ) -> &mut [TerminalCell] {
        match policy {
            AsciiOverwritePolicy::SharedScreen => {
                self.repair_wide_edges(row, columns.clone(), blank);
                self.erase_annotations(row, columns.clone());
                self.row_cells_mut(row, columns)
            }
            AsciiOverwritePolicy::FrostLegacy => {
                let (col, end) = (columns.start, columns.end);
                if col > 0 && self.get(row, col).flags.wide_continuation() {
                    *self.get_mut(row, col - 1) = blank;
                }
                if end < self.cols && self.get(row, end - 1).flags.wide() {
                    *self.get_mut(row, end) = blank;
                }
                &mut self[row][col..end]
            }
        }
    }

    #[inline]
    fn write_ascii_cells(
        target: &mut [TerminalCell],
        bytes: &[u8],
        mut rendition: TerminalCell,
        track_spaces: bool,
        map: impl Fn(u8) -> char,
    ) {
        rendition.flags.set_wide(false);
        rendition.flags.set_wide_continuation(false);
        if track_spaces || bytes.contains(&b' ') {
            let mut space = rendition;
            space.character = ' ';
            if track_spaces {
                space.flags.set_printed_space(true);
                rendition.flags.set_printed_space(false);
            } else if !space.flags.printed_space() {
                space.flags.mark_untracked_drawn_space();
            }
            if bytes.iter().all(|&byte| byte == b' ') {
                target.fill(space);
                return;
            }
            for (cell, &byte) in target.iter_mut().zip(bytes) {
                if byte == b' ' {
                    *cell = space;
                } else {
                    rendition.character = map(byte);
                    *cell = rendition;
                }
            }
        } else {
            for (cell, &byte) in target.iter_mut().zip(bytes) {
                rendition.character = map(byte);
                *cell = rendition;
            }
        }
    }

    /// Fill a valid span with the same single-column graphic and rendition.
    pub(crate) fn overwrite_repeated_narrow(
        &mut self,
        row: usize,
        columns: Range<usize>,
        mut rendition: TerminalCell,
        blank: TerminalCell,
    ) {
        self.mark_drawn_space(&mut rendition);
        rendition.flags.set_wide(false);
        rendition.flags.set_wide_continuation(false);
        self.narrow_overwrite_cells(row, columns, blank, AsciiOverwritePolicy::SharedScreen)
            .fill(rendition);
    }

    #[inline]
    fn mark_drawn_space(&self, rendition: &mut TerminalCell) {
        if self.track_printed_spaces {
            rendition
                .flags
                .set_printed_space(rendition.character == ' ');
        } else if rendition.character == ' ' && !rendition.flags.printed_space() {
            rendition.flags.mark_untracked_drawn_space();
        }
    }

    /// Replace a glyph at a valid row and column. The caller has already
    /// resolved wrapping and any insert-mode shift.
    ///
    /// The full display width determines which old pairs need edge repair.
    /// A width of two writes a lead and a continuation if it fits. Other
    /// positive widths write only the lead, preserving the host's width policy.
    pub fn overwrite_graphic(
        &mut self,
        row: usize,
        col: usize,
        width: usize,
        mut rendition: TerminalCell,
        blank: TerminalCell,
        protection: WideContinuationProtection,
    ) {
        assert!(row < self.rows && col < self.cols && width > 0);
        self.repair_wide_edges(row, col..col.saturating_add(width), blank);
        self.mark_drawn_space(&mut rendition);
        rendition.flags.set_wide(width == 2);
        rendition.flags.set_wide_continuation(false);
        *self.get_mut(row, col) = rendition;
        if width == 2 && col + 1 < self.cols {
            let mut continuation = blank;
            continuation.hyperlink_id = rendition.hyperlink_id;
            continuation.flags.set_wide_continuation(true);
            if protection == WideContinuationProtection::FromLead {
                continuation
                    .flags
                    .set_protected(rendition.flags.protected());
            }
            *self.get_mut(row, col + 1) = continuation;
        }
    }

    pub(crate) fn archive_row(&self, row: usize, allow_margin: bool) -> ScrollbackLine {
        let wrapped = self.row_wrapped[row];
        let mut line = ScrollbackLine::compress(&self[row], wrapped);
        line.attach_decorations(
            self.underline_color_runs(row).collect(),
            self.row_annotations(row).cloned(),
        );
        line.set_raw_row_id(self.row_id(row));
        if allow_margin && self.synthetic_margin_gap(row).is_some() {
            line.set_synthetic_margin_gap(true);
        }
        line
    }

    /// Transfer a FrostLegacyV1 row with its original identity and blink bit.
    pub fn archive_frost_legacy_row(&self, row: usize) -> ScrollbackLine {
        let mut line = ScrollbackLine::compress_preserving_blink(&self[row], self.row_wrapped[row]);
        line.set_raw_row_id(self.row_id(row));
        line
    }

    pub(crate) fn install_projected_row(
        &mut self,
        row: usize,
        cells: &[TerminalCell],
        wrapped: bool,
        wide_gap: bool,
        underline: &[UnderlineColorRun],
        annotations: Option<Arc<RowAnnotations>>,
    ) {
        self.row_cells_mut(row, 0..cells.len())
            .copy_from_slice(cells);
        self.row_wrapped[row] = wrapped;
        if wide_gap {
            debug_assert!(wrapped && cells.len() + 1 == self.cols);
            let margin = self.cols - 1;
            *self.get_mut(row, margin) = TerminalCell::default();
            self.margin_origin[row] = MarginOrigin::WideGap;
        }
        for run in underline {
            self.set_underline_color(row, usize::from(run.start)..usize::from(run.end), run.color);
        }
        if let Some(annotations) = annotations {
            self.annotations.resize_with(self.rows, || None);
            self.annotations[row] = Some(annotations);
        }
    }

    /// Fill a control-command span without cursor movement or output ownership.
    /// Repair clipped wide partners with default blanks, never the fill glyph.
    pub(crate) fn fill_rectangular_span(
        &mut self,
        row: usize,
        columns: Range<usize>,
        mut cell: TerminalCell,
        underline: Color,
    ) {
        cell.flags.clear_cell_contents();
        if cell.character == ' ' {
            if self.track_printed_spaces {
                cell.flags.set_printed_space(true);
            } else {
                cell.flags.mark_untracked_drawn_space();
            }
        }
        self.repair_wide_edges(row, columns.clone(), TerminalCell::default());
        self.row_cells_mut(row, columns.clone()).fill(cell);
        self.erase_annotations(row, columns.clone());
        self.set_underline_color(row, columns, underline);
    }

    pub(crate) fn apply_rectangular_rendition(
        &mut self,
        row: usize,
        columns: Range<usize>,
        rectangle_extent: bool,
        rendition: RectangularRendition,
    ) -> Option<Range<usize>> {
        let base = row * self.cols;
        let mut changed: Option<Range<usize>> = None;
        for column in columns {
            let drawn = self.cell_drawn(row, column);
            if !rectangle_extent && !drawn {
                continue;
            }
            let cell = &mut self.cells[base + column];
            let mut flags = rendition.apply(cell.flags);
            if !drawn {
                flags.mark_untracked_drawn_space();
            }
            if flags != cell.flags {
                cell.flags = flags;
                if let Some(span) = &mut changed {
                    span.end = column + 1;
                } else {
                    changed = Some(column..column + 1);
                }
            }
        }
        if changed.is_some() {
            self.mark_row_changed(row);
        }
        changed
    }

    pub(crate) fn cell_drawn(&self, row: usize, column: usize) -> bool {
        let cell = self.get(row, column);
        cell.character != ' '
            || cell.flags.wide_continuation()
            || cell.flags.untracked_drawn_space()
            || (cell.flags.printed_space() && !cell.flags.tab_padding())
    }

    fn rectangular_text_columns(&self, row: usize, columns: Range<usize>) -> Range<usize> {
        let mut retained = columns;
        if self.get(row, retained.start).flags.wide_continuation() {
            retained.start += 1;
        }
        if retained.start < retained.end && self.get(row, retained.end - 1).flags.wide() {
            retained.end -= 1;
        }
        retained
    }

    pub(crate) fn rectangular_copy_fits(
        &self,
        source_row: usize,
        source: Range<usize>,
        target_row: usize,
        target_col: usize,
    ) -> bool {
        if source_row >= self.rows
            || target_row >= self.rows
            || source.start >= source.end
            || source.end > self.cols
        {
            return false;
        }
        let Some(end) = target_col
            .checked_add(source.len())
            .filter(|end| *end <= self.cols)
        else {
            return false;
        };
        let columns = self.rectangular_text_columns(source_row, source);
        let bytes = self
            .row_annotations(source_row)
            .map_or(0, |line| line.text_bytes_in(columns));
        self.row_annotations(target_row).is_none_or(|line| {
            line.can_store_after_displacing(
                self.expanded_wide_span(target_row, target_col..end),
                bytes,
            )
        })
    }

    /// Snapshot before repairing the destination, including same-row overlaps.
    pub(crate) fn copy_rectangular_row(
        &mut self,
        source_row: usize,
        source: Range<usize>,
        target_row: usize,
        target_col: usize,
        scratch: &mut RectangularCopyScratch,
    ) -> Range<usize> {
        debug_assert!(self.rectangular_copy_fits(
            source_row,
            source.clone(),
            target_row,
            target_col
        ));
        let target = target_col..target_col + source.len();
        let affected = self.expanded_wide_span(target_row, target.clone());
        let text_columns = self.rectangular_text_columns(source_row, source.clone());
        let source_annotations = self.row_annotations(source_row).cloned();
        scratch.cells.clear();
        let base = source_row * self.cols;
        scratch
            .cells
            .extend_from_slice(&self.cells[base + source.start..base + source.end]);
        for column in [source.start, source.end - 1] {
            if !text_columns.contains(&column) {
                let cell = &mut scratch.cells[column - source.start];
                cell.character = ' ';
                cell.hyperlink_id = HyperlinkId::NONE;
                cell.flags.clear_cell_contents();
            }
        }
        scratch.colors.clear();
        let colors = self.underline_colors(source_row);
        if !colors.is_empty() {
            scratch.colors.extend_from_slice(&colors[source.clone()]);
        }
        self.repair_wide_edges(target_row, target.clone(), TerminalCell::default());
        self.row_cells_mut(target_row, target.clone())
            .copy_from_slice(&scratch.cells);
        self.erase_annotations(target_row, target.clone());
        self.set_underline_color(target_row, target.clone(), Color::Default);
        let mut offset = 0;
        for run in scratch.colors.chunk_by(|a, b| a == b) {
            if run[0] != Color::Default {
                self.set_underline_color(
                    target_row,
                    target_col + offset..target_col + offset + run.len(),
                    run[0],
                );
            }
            offset += run.len();
        }
        if let Some(line) =
            source_annotations.filter(|line| line.text_bytes_in(text_columns.clone()) != 0)
        {
            self.annotations.resize_with(self.rows, || None);
            let destination = self.annotations[target_row]
                .get_or_insert_with(|| Arc::new(RowAnnotations::default()));
            Arc::make_mut(destination).copy_text_from(
                &line,
                text_columns.clone(),
                target_col + text_columns.start - source.start,
            );
        }
        affected
    }

    pub(crate) fn mark_tab_spaces(&mut self, row: usize, columns: std::ops::Range<usize>) {
        if !self.track_printed_spaces || columns.is_empty() {
            return;
        }
        for cell in self.row_cells_mut(row, columns) {
            if cell.character == ' ' && !cell.flags.wide_continuation() {
                cell.flags.set_printed_space(true);
                cell.flags.set_tab_padding(true);
            }
        }
    }

    fn copy_region_rows(&mut self, source: Range<usize>, target: usize, preserve_margin: bool) {
        self.cells.copy_within(
            source.start * self.cols..source.end * self.cols,
            target * self.cols,
        );
        if preserve_margin {
            self.margin_origin.copy_within(source, target);
        }
    }

    fn fill_region_rows(&mut self, rows: Range<usize>, blank: TerminalCell) {
        self.cells[rows.start * self.cols..rows.end * self.cols].fill(blank);
        if !self.underline_colors.is_empty() {
            self.underline_colors[rows.clone()].fill(None);
        }
        if !self.annotations.is_empty() {
            self.annotations[rows.clone()].fill(None);
        }
        self.margin_origin[rows].fill(Self::new_margin_origin(blank));
    }

    fn forget_region_margins(&mut self, rows: Range<usize>) {
        self.margin_origin[rows].fill(MarginOrigin::Unknown);
    }

    pub(crate) fn preserves_shift_margin(
        &self,
        direction: RegionDirection,
        region: &RangeInclusive<usize>,
        wrap_policy: RegionWrapPolicy,
    ) -> bool {
        wrap_policy == RegionWrapPolicy::MoveWithRows
            && direction == RegionDirection::Up
            && *region.start() == 0
            && region.end().checked_add(1) == Some(self.rows())
    }

    /// Shift rows in an inclusive physical range, returning the dirty rows.
    /// Counts are capped to the region height. Retained cells and IDs travel
    /// together. The infallible allocator fills blank rows in ascending
    /// destination order, returning a fresh ID or `RawRowId::UNTRACKED`.
    /// Capture outgoing content before calling if the host archives it.
    pub fn shift_rows(
        &mut self,
        direction: RegionDirection,
        region: RangeInclusive<usize>,
        count: usize,
        blank: TerminalCell,
        wrap_policy: RegionWrapPolicy,
        mut allocate_id: impl FnMut() -> RawRowId,
    ) -> Option<Range<usize>> {
        let (top, bottom) = (*region.start(), *region.end());
        if region.is_empty() || bottom >= self.rows() || count == 0 {
            return None;
        }
        let end = bottom + 1;
        let height = end - top;
        let count = count.min(height);
        let move_wraps = wrap_policy == RegionWrapPolicy::MoveWithRows;
        let preserve_margin = self.preserves_shift_margin(direction, &region, wrap_policy);
        let (source, target, blank_rows) = match direction {
            RegionDirection::Up => (top + count..end, top, end - count..end),
            RegionDirection::Down => (top..end - count, top + count, top..top + count),
        };
        if self.capture_revision != 0 {
            self.row_changes.copy_within(source.clone(), target);
            self.row_changes[blank_rows.clone()].fill(self.capture_revision);
            self.last_row_change = self.capture_revision;
            if self.frame_base_revision != 0 {
                if self.scroll_origins_revision != self.frame_base_revision {
                    self.scroll_origins.clear();
                    self.scroll_origins.extend((0..self.rows).map(Some));
                    self.scroll_origins_revision = self.frame_base_revision;
                }
                self.scroll_origins.copy_within(source.clone(), target);
                self.scroll_origins[blank_rows.clone()].fill(None);
            }
        }
        if count < height && !self.underline_colors.is_empty() {
            let rows = &mut self.underline_colors[top..end];
            match direction {
                RegionDirection::Up => rows.rotate_left(count),
                RegionDirection::Down => rows.rotate_right(count),
            }
        }
        if count < height && !self.annotations.is_empty() {
            let rows = &mut self.annotations[top..end];
            match direction {
                RegionDirection::Up => rows.rotate_left(count),
                RegionDirection::Down => rows.rotate_right(count),
            }
        }
        if count < height {
            self.copy_region_rows(source.clone(), target, preserve_margin);
            if move_wraps {
                self.row_wrapped.copy_within(source.clone(), target);
            }
            self.row_ids.copy_within(source, target);
        }
        self.fill_region_rows(blank_rows.clone(), blank);
        if !preserve_margin {
            self.forget_region_margins(top.saturating_sub(1)..end);
        }
        if move_wraps {
            self.row_wrapped[blank_rows.clone()].fill(false);
        }
        for row in blank_rows {
            self.row_ids[row] = allocate_id();
        }
        self.bump_identity_revision();
        Some(top..end)
    }

    fn new_margin_origin(blank: TerminalCell) -> MarginOrigin {
        if blank == TerminalCell::default() {
            MarginOrigin::VirginDefault
        } else {
            MarginOrigin::Unknown
        }
    }

    /// 获取所有行为 `Vec<Vec<_>>`（用于兼容旧代码）。
    pub fn to_vec(&self) -> Vec<Vec<TerminalCell>> {
        self.cells
            .chunks_exact(self.cols)
            .map(|chunk| chunk.to_vec())
            .collect()
    }

    /// 在行内指定列插入一个cell，右侧cell右移，末尾cell被丢弃
    pub fn insert_cell_in_row(&mut self, row: usize, col: usize, cell: TerminalCell) {
        if row >= self.rows || col >= self.cols {
            return;
        }
        self.mark_row_changed(row);
        self.margin_origin[row] = MarginOrigin::Unknown;
        let start = row * self.cols;
        self.cells
            .copy_within(start + col..start + self.cols - 1, start + col + 1);
        self.cells[start + col] = cell;
        self.shift_underline_colors(row, col, 1, true);
        let cols = self.cols;
        self.edit_annotations(row, |line| line.insert_columns(col, 1, cols));
    }

    /// 删除行内指定列的cell，右侧cell左移，末尾补blank
    pub fn remove_cell_from_row(&mut self, row: usize, col: usize) {
        if row >= self.rows || col >= self.cols {
            return;
        }
        self.mark_row_changed(row);
        self.margin_origin[row] = MarginOrigin::Unknown;
        let start = row * self.cols;
        self.cells
            .copy_within(start + col + 1..start + self.cols, start + col);
        // Fill last cell with default
        self.cells[start + self.cols - 1] = TerminalCell::default();
        self.shift_underline_colors(row, col, 1, false);
        self.edit_annotations(row, |line| line.delete_columns(col, 1));
    }

    /// Clear a double-width character left holding only its lead half at the
    /// right edge. The renderer paints `wide()` cells two columns wide, and a
    /// later shift would carry the half character back into the middle of the
    /// row, where it hides or overpaints a neighbour. Callers use this after an
    /// operation that can push a continuation cell off the row: an ICH/IRM
    /// insert, or a resize that narrows the grid.
    pub fn clear_dangling_wide_at_row_end(&mut self, row: usize, blank: TerminalCell) {
        if row >= self.rows || self.cols == 0 {
            return;
        }
        let last = row * self.cols + self.cols - 1;
        if self.cells[last].flags.wide() {
            self.mark_row_changed(row);
            self.margin_origin[row] = MarginOrigin::Unknown;
            self.cells[last] = blank;
            self.set_underline_color(row, self.cols - 1..self.cols, Color::Default);
            self.erase_annotations(row, self.cols - 1..self.cols);
        }
    }

    /// Expand a valid overwrite span to include any adjacent wide partners.
    pub fn expanded_wide_span(&self, row: usize, cols: Range<usize>) -> Range<usize> {
        let mut start = cols.start.min(self.cols);
        let mut end = cols.end.min(self.cols);
        if row >= self.rows || start >= end {
            return start..end;
        }
        if start > 0 && self.get(row, start - 1).flags.wide() {
            start -= 1;
        }
        if end < self.cols && self.get(row, end).flags.wide_continuation() {
            end += 1;
        }
        start..end
    }

    /// Blank the partners left outside an overwrite span.
    pub fn repair_wide_edges(&mut self, row: usize, cols: Range<usize>, blank: TerminalCell) {
        let start = cols.start;
        let end = cols.end.min(self.cols);
        if row >= self.rows || start >= end {
            return;
        }
        let affected = self.expanded_wide_span(row, start..end);
        if affected.start < start {
            *self.get_mut(row, affected.start) = blank;
        }
        if affected.end > end {
            *self.get_mut(row, end) = blank;
        }
    }

    fn erase_wide_pair_touching(&mut self, row: usize, col: usize, blank: TerminalCell) {
        if col < self.cols && self.get(row, col).flags.wide_continuation() && col > 0 {
            *self.get_mut(row, col - 1) = blank;
            *self.get_mut(row, col) = blank;
        } else if col + 1 < self.cols && self.get(row, col).flags.wide() {
            *self.get_mut(row, col) = blank;
            *self.get_mut(row, col + 1) = blank;
        } else if col > 0 && self.get(row, col - 1).flags.wide() {
            *self.get_mut(row, col - 1) = blank;
            if col < self.cols {
                *self.get_mut(row, col) = blank;
            }
        }
    }

    /// Insert up to the remaining row width. Returns whether cells were edited.
    pub fn insert_blank_cells(
        &mut self,
        row: usize,
        col: usize,
        count: usize,
        blank: TerminalCell,
        policy: WideCellEditPolicy,
    ) -> bool {
        let count = count.min(self.cols.saturating_sub(col));
        if row >= self.rows || count == 0 {
            return false;
        }
        match policy {
            WideCellEditPolicy::RepairSplitPairs => {
                if self.get(row, col).flags.wide_continuation() {
                    self.erase_cells(row, col..col + 1, blank);
                }
            }
            WideCellEditPolicy::ClearBoundaryPairs => {
                self.erase_wide_pair_touching(row, col, blank);
            }
        }
        let line = self.row_cells_mut(row, col..self.cols);
        line.copy_within(..line.len() - count, count);
        line[..count].fill(blank);
        self.shift_underline_colors(row, col, count, true);
        let cols = self.cols;
        self.edit_annotations(row, |line| line.insert_columns(col, count, cols));
        self.clear_dangling_wide_at_row_end(row, blank);
        true
    }

    /// Delete up to the remaining row width and fill the vacated cells.
    pub fn delete_cells(
        &mut self,
        row: usize,
        col: usize,
        count: usize,
        blank: TerminalCell,
        policy: WideCellEditPolicy,
    ) -> bool {
        let count = count.min(self.cols.saturating_sub(col));
        if row >= self.rows || count == 0 {
            return false;
        }
        match policy {
            WideCellEditPolicy::RepairSplitPairs => {
                self.repair_wide_edges(row, col..col + count, blank);
            }
            WideCellEditPolicy::ClearBoundaryPairs => {
                self.erase_wide_pair_touching(row, col, blank);
                self.erase_wide_pair_touching(row, col + count, blank);
            }
        }
        let line = self.row_cells_mut(row, col..self.cols);
        line.copy_within(count.., 0);
        let retained = line.len() - count;
        line[retained..].fill(blank);
        self.shift_underline_colors(row, col, count, false);
        self.edit_annotations(row, |line| line.delete_columns(col, count));
        true
    }

    /// Erase a row span and its wide partners. Callers select any protected runs.
    pub fn erase_cells(&mut self, row: usize, cols: Range<usize>, blank: TerminalCell) {
        let end = cols.end.min(self.cols);
        if row >= self.rows || cols.start >= end {
            return;
        }
        let right_partner = end < self.cols && self.get(row, end - 1).flags.wide();
        // Adjacent malformed leads and wide blanks can change the flags that
        // scalar erasure reads at the last selected cell.
        let scalar_edges = end < self.cols
            && (blank.flags.wide()
                || (right_partner && end - cols.start > 1 && self.get(row, end - 2).flags.wide()));
        if !scalar_edges {
            let mut start = cols.start;
            if start > 0 && self.get(row, start).flags.wide_continuation() {
                start -= 1;
            }
            let affected_end = end + usize::from(right_partner);
            self.row_cells_mut(row, start..affected_end).fill(blank);
            self.set_underline_color(row, start..affected_end, Color::Default);
            self.erase_annotations(row, start..affected_end);
            return;
        }
        for col in cols.start..end {
            let flags = self.get(row, col).flags;
            if col > 0 && flags.wide_continuation() {
                *self.get_mut(row, col - 1) = blank;
            }
            if col + 1 < self.cols && flags.wide() {
                *self.get_mut(row, col + 1) = blank;
            }
            *self.get_mut(row, col) = blank;
        }
    }

    /// Fill the physical grid while preserving row IDs and wrap flags.
    pub fn fill_cells(&mut self, cell: TerminalCell) {
        self.cells_mut().fill(cell);
    }

    /// 是否为空
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.rows == 0
    }

    /// 调整网格大小
    pub fn resize(&mut self, new_rows: usize, new_cols: usize, default_cell: TerminalCell) {
        if self.capture_revision != 0 {
            self.row_changes.resize(new_rows, self.capture_revision);
            self.row_changes.fill(self.capture_revision);
            self.last_row_change = self.capture_revision;
            self.scroll_origins_revision = 0;
        }
        let mut new_cells = vec![default_cell; new_rows * new_cols];
        let copy_rows = self.rows.min(new_rows);
        let copy_cols = self.cols.min(new_cols);
        for row in 0..copy_rows {
            let src_start = row * self.cols;
            let dst_start = row * new_cols;
            new_cells[dst_start..dst_start + copy_cols]
                .copy_from_slice(&self.cells[src_start..src_start + copy_cols]);
        }
        self.cells = new_cells;
        let mut new_wrapped = vec![false; new_rows];
        new_wrapped[..copy_rows].copy_from_slice(&self.row_wrapped[..copy_rows]);
        self.row_wrapped = new_wrapped;
        let mut new_row_ids = vec![RawRowId::UNTRACKED; new_rows];
        new_row_ids[..copy_rows].copy_from_slice(&self.row_ids[..copy_rows]);
        self.row_ids = new_row_ids;
        if !self.underline_colors.is_empty() {
            self.underline_colors.resize_with(new_rows, || None);
            for colors in self.underline_colors.iter_mut().flatten() {
                colors.resize(new_cols, Color::Default);
            }
        }
        if !self.annotations.is_empty() {
            self.annotations.resize_with(new_rows, || None);
            if new_cols < self.cols {
                for row in 0..copy_rows {
                    self.erase_annotations(row, new_cols..self.cols);
                }
            }
        }
        if self.cols != new_cols {
            // Width changes invalidate both the old margin coordinate and proof
            // for the new last cell; do not infer provenance from its value.
            self.margin_origin = vec![MarginOrigin::Unknown; new_rows];
        } else {
            self.margin_origin
                .resize(new_rows, Self::new_margin_origin(default_cell));
            if new_rows < self.rows && new_rows > 0 {
                self.margin_origin[new_rows - 1] = MarginOrigin::Unknown;
            }
        }
        self.rows = new_rows;
        self.cols = new_cols;
        self.bump_identity_revision();
    }

    /// Resize using FrostLegacyV1's wide-pair repair and identity cadence.
    /// The original grid reserves an ID for every destination row, including
    /// the retained rows whose existing identities are copied forward.
    pub fn resize_frost_legacy(&mut self, rows: usize, cols: usize, blank: TerminalCell) {
        let retained = self.rows.min(rows);
        self.resize(rows, cols, blank);
        for row in 0..retained {
            let line = &mut self[row];
            for col in 0..cols {
                if line[col].flags.wide()
                    && (col + 1 >= cols || !line[col + 1].flags.wide_continuation())
                {
                    line[col] = blank;
                }
            }
            for col in 0..cols {
                if line[col].flags.wide_continuation() && (col == 0 || !line[col - 1].flags.wide())
                {
                    line[col] = blank;
                }
            }
        }
        for (row, id) in self.row_ids.iter_mut().enumerate() {
            let fresh = RawRowId::allocate_process_unique();
            if row >= retained {
                *id = fresh;
            }
        }
    }

    /// 向上滚动 n 行:丢弃顶部 n 行,其余行上移,底部补 blank。缩小高度前调用,
    /// 把底部内容(光标行/近期输出)上移保留;调用方负责把被丢弃的顶部行存入 scrollback。
    pub fn scroll_up_by(&mut self, n: usize, blank: TerminalCell) {
        self.shift_rows(
            RegionDirection::Up,
            0..=self.rows.saturating_sub(1),
            n,
            blank,
            RegionWrapPolicy::MoveWithRows,
            || RawRowId::UNTRACKED,
        );
    }

    /// 获取mut访问所有行
    pub fn iter_mut(&mut self) -> std::slice::ChunksMut<'_, TerminalCell> {
        let columns = self.cols;
        self.cells_mut().chunks_mut(columns)
    }

    /// 获取只读访问所有行
    pub fn iter(&self) -> std::slice::Chunks<'_, TerminalCell> {
        self.cells.chunks(self.cols)
    }
}

impl AsRef<TerminalGrid> for TerminalGrid {
    fn as_ref(&self) -> &TerminalGrid {
        self
    }
}

impl std::ops::Index<usize> for TerminalGrid {
    type Output = [TerminalCell];
    fn index(&self, row: usize) -> &[TerminalCell] {
        let start = row * self.cols;
        &self.cells[start..start + self.cols]
    }
}

impl std::ops::IndexMut<usize> for TerminalGrid {
    fn index_mut(&mut self, row: usize) -> &mut [TerminalCell] {
        self.mark_row_changed(row);
        self.margin_origin[row] = MarginOrigin::Unknown;
        if let Some(colors) = self.underline_colors.get_mut(row) {
            *colors = None;
        }
        if let Some(line) = self.annotations.get_mut(row) {
            *line = None;
        }
        let start = row * self.cols;
        &mut self.cells[start..start + self.cols]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Color {
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    White,
    BrightBlack,
    BrightRed,
    BrightGreen,
    BrightYellow,
    BrightBlue,
    BrightMagenta,
    BrightCyan,
    BrightWhite,
    Indexed(u8),
    Rgb(u8, u8, u8),
    Default,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CursorShape {
    #[default]
    Block, // 0 or 1 - block cursor (default)
    Underline, // 2 - underline cursor
    Beam,      // 3 - beam/vertical line cursor
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum UnderlineStyle {
    #[default]
    None,
    Single,
    Double,
    Curly,  // SGR 4:3
    Dotted, // SGR 4:4
    Dashed, // SGR 4:5
}

/// Packed style flags in a u16 bitfield (includes wide character bits).
/// Layout:
///   bit 0: bold
///   bit 1: italic
///   bit 2-4: underline style (3 bits, 0-5)
///   bit 5: inverse
///   bit 6: dim
///   bit 7: blink
///   bit 8: strikethrough
///   bit 9: wide
///   bit 10: wide_continuation
///   bit 11: protected (DECSCA)
///   bit 12: a printed space, distinct from erase padding
///   bits 12–13 together distinguish printed spaces, tab padding, and drawn
///   spaces that do not change history trimming
///   bits 14-15: overline and conceal
#[derive(Clone, Copy, PartialEq, Eq, Default)]
#[repr(transparent)]
pub struct StyleFlags(u16);

impl std::fmt::Debug for StyleFlags {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StyleFlags")
            .field("bold", &self.bold())
            .field("italic", &self.italic())
            .field("underline", &self.underline())
            .field("inverse", &self.inverse())
            .field("dim", &self.dim())
            .field("blink", &self.blink())
            .field("strikethrough", &self.strikethrough())
            .field("protected", &self.protected())
            .finish()
    }
}

const BOLD_BIT: u16 = 1 << 0;
const ITALIC_BIT: u16 = 1 << 1;
const UNDERLINE_SHIFT: u32 = 2;
const UNDERLINE_MASK: u16 = 0b111 << 2;
const INVERSE_BIT: u16 = 1 << 5;
const DIM_BIT: u16 = 1 << 6;
const BLINK_BIT: u16 = 1 << 7;
const STRIKETHROUGH_BIT: u16 = 1 << 8;
const WIDE_BIT: u16 = 1 << 9;
const WIDE_CONT_BIT: u16 = 1 << 10;
const PROTECTED_BIT: u16 = 1 << 11;
const PRINTED_SPACE_BIT: u16 = 1 << 12;
const TAB_PADDING_BIT: u16 = 1 << 13;
const OVERLINE_BIT: u16 = 1 << 14;
const CONCEALED_BIT: u16 = 1 << 15;

/// A command's DEC rendition edits, compiled once for all affected cells.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RectangularRendition {
    keep: u16,
    xor: u16,
    underline: RectangularUnderline,
}

#[derive(Clone, Copy, Debug)]
enum RectangularUnderline {
    Preserve { toggle: bool },
    Constant { enabled: bool },
}

impl RectangularRendition {
    pub(crate) fn compile(params: &[u16], reverse: bool) -> Self {
        let mut result = Self {
            keep: u16::MAX,
            xor: 0,
            underline: RectangularUnderline::Preserve { toggle: false },
        };
        for &parameter in if params.is_empty() { &[0][..] } else { params } {
            let (bits, underline) = match parameter {
                0 => (BOLD_BIT | BLINK_BIT | INVERSE_BIT, true),
                1 | 22 if !reverse || parameter == 1 => (BOLD_BIT, false),
                4 | 24 if !reverse || parameter == 4 => (0, true),
                5 | 25 if !reverse || parameter == 5 => (BLINK_BIT, false),
                7 | 27 if !reverse || parameter == 7 => (INVERSE_BIT, false),
                _ => continue,
            };
            if reverse {
                result.xor ^= bits;
                if underline {
                    result.underline = match result.underline {
                        RectangularUnderline::Preserve { toggle } => {
                            RectangularUnderline::Preserve { toggle: !toggle }
                        }
                        RectangularUnderline::Constant { enabled } => {
                            RectangularUnderline::Constant { enabled: !enabled }
                        }
                    };
                }
            } else {
                let enabled = matches!(parameter, 1 | 4 | 5 | 7);
                result.keep &= !bits;
                if enabled {
                    result.xor |= bits;
                } else {
                    result.xor &= !bits;
                }
                if underline {
                    result.underline = RectangularUnderline::Constant { enabled };
                }
            }
        }
        result
    }

    pub(crate) fn apply(self, flags: StyleFlags) -> StyleFlags {
        let mut result = StyleFlags((flags.0 & self.keep) ^ self.xor);
        let enabled = match self.underline {
            RectangularUnderline::Preserve { toggle: false } => return result,
            RectangularUnderline::Preserve { toggle: true } => {
                flags.underline() == UnderlineStyle::None
            }
            RectangularUnderline::Constant { enabled } => enabled,
        };
        result.set_underline(if enabled {
            UnderlineStyle::Single
        } else {
            UnderlineStyle::None
        });
        result
    }
}

impl StyleFlags {
    pub(crate) fn common_sgr(self, other: Self) -> Self {
        let mut mask = BOLD_BIT
            | ITALIC_BIT
            | INVERSE_BIT
            | DIM_BIT
            | BLINK_BIT
            | STRIKETHROUGH_BIT
            | OVERLINE_BIT
            | CONCEALED_BIT;
        if (self.0 ^ other.0) & UNDERLINE_MASK == 0 {
            mask |= UNDERLINE_MASK;
        }
        Self(self.0 & other.0 & mask)
    }

    /// Remove attributes that describe the contents of one cell.
    pub(crate) fn clear_cell_contents(&mut self) {
        self.0 &= !(WIDE_BIT | WIDE_CONT_BIT | PRINTED_SPACE_BIT | TAB_PADDING_BIT);
    }

    /// Renderable attributes without live cell geometry or protection.
    pub(crate) fn archive_rendition(self) -> Self {
        Self(
            self.0
                & (BOLD_BIT
                    | ITALIC_BIT
                    | UNDERLINE_MASK
                    | INVERSE_BIT
                    | DIM_BIT
                    | BLINK_BIT
                    | STRIKETHROUGH_BIT
                    | OVERLINE_BIT
                    | CONCEALED_BIT),
        )
    }

    #[inline(always)]
    pub fn new() -> Self {
        Self(0)
    }

    /// Whether this cell has only ordinary text occupancy flags. Rendition,
    /// protection, and wide-cell geometry all disqualify plain text.
    #[inline]
    pub fn is_plain_text(&self) -> bool {
        self.0 & !(PRINTED_SPACE_BIT | TAB_PADDING_BIT) == 0
    }

    #[inline(always)]
    pub fn bold(&self) -> bool {
        self.0 & BOLD_BIT != 0
    }
    #[inline(always)]
    pub fn italic(&self) -> bool {
        self.0 & ITALIC_BIT != 0
    }
    #[inline(always)]
    pub fn underline(&self) -> UnderlineStyle {
        match (self.0 & UNDERLINE_MASK) >> UNDERLINE_SHIFT {
            1 => UnderlineStyle::Single,
            2 => UnderlineStyle::Double,
            3 => UnderlineStyle::Curly,
            4 => UnderlineStyle::Dotted,
            5 => UnderlineStyle::Dashed,
            _ => UnderlineStyle::None,
        }
    }
    #[inline(always)]
    pub fn inverse(&self) -> bool {
        self.0 & INVERSE_BIT != 0
    }
    #[inline(always)]
    pub fn dim(&self) -> bool {
        self.0 & DIM_BIT != 0
    }
    #[inline(always)]
    pub fn blink(&self) -> bool {
        self.0 & BLINK_BIT != 0
    }
    #[inline(always)]
    pub fn strikethrough(&self) -> bool {
        self.0 & STRIKETHROUGH_BIT != 0
    }
    pub fn overline(&self) -> bool {
        self.0 & OVERLINE_BIT != 0
    }
    pub fn concealed(&self) -> bool {
        self.0 & CONCEALED_BIT != 0
    }
    pub fn set_overline(&mut self, enabled: bool) {
        self.0 = (self.0 & !OVERLINE_BIT) | if enabled { OVERLINE_BIT } else { 0 };
    }
    pub fn set_concealed(&mut self, enabled: bool) {
        self.0 = (self.0 & !CONCEALED_BIT) | if enabled { CONCEALED_BIT } else { 0 };
    }
    #[inline(always)]
    pub fn wide(&self) -> bool {
        self.0 & WIDE_BIT != 0
    }
    #[inline(always)]
    pub fn wide_continuation(&self) -> bool {
        self.0 & WIDE_CONT_BIT != 0
    }
    #[inline(always)]
    pub fn protected(&self) -> bool {
        self.0 & PROTECTED_BIT != 0
    }

    pub(crate) fn printed_space(&self) -> bool {
        self.0 & PRINTED_SPACE_BIT != 0
    }

    pub(crate) fn tab_padding(&self) -> bool {
        self.0 & (PRINTED_SPACE_BIT | TAB_PADDING_BIT) == (PRINTED_SPACE_BIT | TAB_PADDING_BIT)
    }

    pub(crate) fn untracked_drawn_space(&self) -> bool {
        self.0 & (PRINTED_SPACE_BIT | TAB_PADDING_BIT) == TAB_PADDING_BIT
    }

    pub(crate) fn mark_untracked_drawn_space(&mut self) {
        self.0 = (self.0 & !PRINTED_SPACE_BIT) | TAB_PADDING_BIT;
    }

    fn set_printed_space(&mut self, printed: bool) {
        if printed {
            self.0 |= PRINTED_SPACE_BIT;
        } else {
            self.0 &= !PRINTED_SPACE_BIT;
        }
    }

    fn set_tab_padding(&mut self, tab: bool) {
        if tab {
            self.0 |= TAB_PADDING_BIT;
        } else {
            self.0 &= !TAB_PADDING_BIT;
        }
    }

    #[inline(always)]
    pub fn set_bold(&mut self, v: bool) {
        if v {
            self.0 |= BOLD_BIT;
        } else {
            self.0 &= !BOLD_BIT;
        }
    }
    #[inline(always)]
    pub fn set_italic(&mut self, v: bool) {
        if v {
            self.0 |= ITALIC_BIT;
        } else {
            self.0 &= !ITALIC_BIT;
        }
    }
    #[inline(always)]
    pub fn set_underline(&mut self, v: UnderlineStyle) {
        self.0 = (self.0 & !UNDERLINE_MASK) | ((v as u16) << UNDERLINE_SHIFT);
    }
    #[inline(always)]
    pub fn set_inverse(&mut self, v: bool) {
        if v {
            self.0 |= INVERSE_BIT;
        } else {
            self.0 &= !INVERSE_BIT;
        }
    }
    #[inline(always)]
    pub fn set_dim(&mut self, v: bool) {
        if v {
            self.0 |= DIM_BIT;
        } else {
            self.0 &= !DIM_BIT;
        }
    }
    #[inline(always)]
    pub fn set_blink(&mut self, v: bool) {
        if v {
            self.0 |= BLINK_BIT;
        } else {
            self.0 &= !BLINK_BIT;
        }
    }
    #[inline(always)]
    pub fn set_strikethrough(&mut self, v: bool) {
        if v {
            self.0 |= STRIKETHROUGH_BIT;
        } else {
            self.0 &= !STRIKETHROUGH_BIT;
        }
    }
    #[inline(always)]
    pub fn set_wide(&mut self, v: bool) {
        if v {
            self.0 |= WIDE_BIT;
        } else {
            self.0 &= !WIDE_BIT;
        }
    }
    #[inline(always)]
    pub fn set_wide_continuation(&mut self, v: bool) {
        if v {
            self.0 |= WIDE_CONT_BIT;
        } else {
            self.0 &= !WIDE_CONT_BIT;
        }
    }
    #[inline(always)]
    pub fn set_protected(&mut self, v: bool) {
        if v {
            self.0 |= PROTECTED_BIT;
        } else {
            self.0 &= !PROTECTED_BIT;
        }
    }

    #[inline(always)]
    pub fn is_default_style(&self) -> bool {
        self.0 & 0x1FF == 0
    }
}

/// Stable identity of one retained physical raw row. Zero means the row is
/// deliberately untracked (allocator exhausted); it never aliases a real id.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RawRowId(u64);

impl RawRowId {
    pub const UNTRACKED: Self = Self(0);

    /// Allocate within the process-wide row identity domain used by Frost.
    /// Session-local domains can use their own allocator and `RawRowId::new`.
    #[allow(deprecated)] // fetch_update supports the engine's Rust 1.86 MSRV.
    pub fn allocate_process_unique() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        NEXT.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
            next.checked_add(1)
        })
        .map(Self::new)
        .unwrap_or(Self::UNTRACKED)
    }

    #[allow(dead_code)] // Public library accessor; binary keeps the id opaque.
    pub const fn get(self) -> Option<u64> {
        if self.0 == 0 {
            None
        } else {
            Some(self.0)
        }
    }

    pub const fn is_tracked(self) -> bool {
        self.0 != 0
    }

    pub const fn new(value: u64) -> Self {
        Self(value)
    }
}

#[cfg(test)]
#[test]
fn structural_grid_revision_stays_nonzero_after_wrap() {
    let mut grid = TerminalGrid::new(2, 2);
    grid.identity_revision = u64::MAX;
    grid.scroll_up_by(1, TerminalCell::default());
    assert_eq!(grid.identity_revision(), 1);
    grid.resize(2, 3, TerminalCell::default());
    assert_eq!(grid.identity_revision(), 2);
}

#[derive(Clone, Debug)]
pub struct ScrollbackLine {
    data: CompressedLineData,
    pub is_wrapped: bool,
    archive_flags: u8,
    cols: u16,
    /// Number of cells retained when historical soft-wrapped rows are joined
    /// before being split at the current terminal width.  Computing this when
    /// the line is compressed lets viewport reflow count rows without first
    /// allocating and decoding every line in the scrollback tail.
    reflow_content_len: u16,
    /// Physical columns containing wide-character continuation cells inside
    /// the exact prefix retained by projection reflow. Keeping this compact
    /// geometry beside the compressed row lets the projection planner avoid
    /// decoding historical cells merely to preserve wide glyph boundaries.
    reflow_wide_continuations: SmallVec<[u16; 2]>,
    // Geometry occupies the prefix; packed drawn-space occupancy follows it.
    reflow_wide_count: u16,
    raw_row_id: RawRowId,
}

const ARCHIVE_PRINTED_SPACES: u8 = 1 << 1;
const ARCHIVE_EXTENDED_RENDITION: u8 = 1 << 3;
const ARCHIVE_CELL_OVERLINE: u8 = 1 << 5;
const ARCHIVE_CELL_CONCEALED: u8 = 1 << 6;

#[derive(Clone, Debug)]
enum CompressedLineData {
    Plain(String, u16),
    Encoded(Vec<u8>),
    Decorated(Box<DecoratedLineData>),
}

#[derive(Clone, Debug)]
struct DecoratedLineData {
    base: CompressedLineData,
    underline_colors: Box<[UnderlineColorRun]>,
    annotations: Option<Arc<RowAnnotations>>,
}

impl CompressedLineData {
    fn content(&self) -> ScrollbackContent<'_> {
        match self {
            Self::Plain(text, trailing) => ScrollbackContent::Plain(text, *trailing),
            Self::Encoded(bytes) => ScrollbackContent::Encoded(bytes),
            Self::Decorated(data) => data.base.content(),
        }
    }

    fn heap_bytes(&self) -> usize {
        match self {
            Self::Plain(text, _) => text.capacity(),
            Self::Encoded(bytes) => bytes.capacity(),
            Self::Decorated(data) => {
                size_of::<DecoratedLineData>()
                    + data.base.heap_bytes()
                    + size_of_val(data.underline_colors.as_ref())
                    + data
                        .annotations
                        .as_ref()
                        .map_or(0, |row| row.retained_bytes())
            }
        }
    }
}

/// Borrowed compressed content for history search, accounting and replay.
/// A plain row holds narrow text and the count of implicit trailing blanks.
#[derive(Clone, Copy, Debug)]
pub enum ScrollbackContent<'a> {
    Plain(&'a str, u16),
    Encoded(&'a [u8]),
}

/// A row's text or cells before matching. Plain history has identity columns.
/// Encoded history can stream through the codec without building a cell vector.
pub enum SearchLine<'a> {
    Text(&'a str),
    Cells(std::borrow::Cow<'a, [TerminalCell]>),
    Scrollback(&'a ScrollbackLine),
}

impl<'a> SearchLine<'a> {
    /// Use bulk decode for encoded rows past the caller's streaming threshold.
    pub fn from_retained(line: &'a ScrollbackLine, stream_columns: usize) -> Self {
        match line.content() {
            ScrollbackContent::Plain(text, _) => Self::Text(text),
            ScrollbackContent::Encoded(_) if line.columns() > stream_columns => {
                Self::Cells(std::borrow::Cow::Owned(line.decompress()))
            }
            ScrollbackContent::Encoded(_) => Self::Scrollback(line),
        }
    }
}

impl ScrollbackLine {
    #[inline]
    pub fn columns(&self) -> usize {
        self.cols as usize
    }

    pub fn compress(cells: &[TerminalCell], is_wrapped: bool) -> Self {
        Self::compress_with_style(cells, is_wrapped, false)
    }

    /// Preserve blink for history profiles that already retain it. The legacy
    /// default codec drops blink while preserving all its other encoded flags.
    pub fn compress_preserving_blink(cells: &[TerminalCell], is_wrapped: bool) -> Self {
        Self::compress_with_style(cells, is_wrapped, true)
    }

    /// Archive an independent Frost row, preserving blink and assigning a fresh
    /// identity even when the archived cells came from another tracked row.
    pub fn compress_frost_legacy(cells: &[TerminalCell], is_wrapped: bool) -> Self {
        let id = RawRowId::allocate_process_unique();
        let mut line = Self::compress_preserving_blink(cells, is_wrapped);
        line.set_raw_row_id(id);
        line
    }

    fn compress_with_style(cells: &[TerminalCell], is_wrapped: bool, preserve_blink: bool) -> Self {
        let needs_encoded_flags = |flags: StyleFlags| {
            !flags.is_default_style()
                || flags.0
                    & (WIDE_BIT | WIDE_CONT_BIT | PRINTED_SPACE_BIT | OVERLINE_BIT | CONCEALED_BIT)
                    != 0
        };
        let default_colors_and_no_link = |cell: &TerminalCell| {
            cell.foreground == Color::Default
                && cell.background == Color::Default
                && cell.hyperlink_id.is_none()
        };
        let active_len = cells
            .iter()
            .rposition(|cell| {
                cell.character != ' '
                    || needs_encoded_flags(cell.flags)
                    || !default_colors_and_no_link(cell)
            })
            .map_or(0, |column| column + 1);
        let active = &cells[..active_len];
        // Storage can keep styled trailing cells that reflow omits.
        let reflow_content_len = active
            .iter()
            .rposition(|cell| !cell.is_reflow_trimmable_blank())
            .map_or(0, |index| index + 1);
        // Include trimmed cells so their drawn spaces can still be restored.
        let row_flags = StyleFlags(cells.iter().fold(0, |bits, cell| bits | cell.flags.0));
        let mut reflow_wide_continuations: SmallVec<[u16; 2]> = if row_flags.wide_continuation() {
            cells[..reflow_content_len]
                .iter()
                .enumerate()
                .filter_map(|(column, cell)| {
                    if cell.flags.wide_continuation() {
                        u16::try_from(column).ok()
                    } else {
                        None
                    }
                })
                .collect()
        } else {
            SmallVec::new()
        };
        let reflow_wide_count = reflow_wide_continuations.len() as u16;
        let last_drawn = if row_flags.0 & TAB_PADDING_BIT != 0 {
            cells
                .iter()
                .rposition(|cell| cell.flags.untracked_drawn_space())
        } else {
            None
        };
        if let Some(last) = last_drawn {
            let start = reflow_wide_continuations.len();
            reflow_wide_continuations.resize(start + last / 16 + 1, 0);
            for (word, block) in reflow_wide_continuations[start..]
                .iter_mut()
                .zip(cells[..=last].chunks(16))
            {
                *word = block.iter().rev().fold(0, |bits, cell| {
                    (bits << 1) | u16::from(cell.flags.untracked_drawn_space())
                });
            }
        }
        let (data, archive_flags) =
            if !needs_encoded_flags(row_flags) && active.iter().all(default_colors_and_no_link) {
                let text: String = active.iter().map(|cell| cell.character).collect();
                (
                    CompressedLineData::Plain(text, (cells.len() - active_len) as u16),
                    u8::from(cells.len() > usize::from(u16::MAX)) << 2,
                )
            } else {
                (
                    CompressedLineData::Encoded(Self::encode_cells(active, preserve_blink)),
                    (u8::from(row_flags.printed_space()) * ARCHIVE_PRINTED_SPACES)
                        | (u8::from(row_flags.0 & (OVERLINE_BIT | CONCEALED_BIT) != 0)
                            * ARCHIVE_EXTENDED_RENDITION),
                )
            };
        ScrollbackLine {
            data,
            is_wrapped,
            archive_flags,
            cols: cells.len() as u16,
            reflow_content_len: u16::try_from(reflow_content_len).unwrap_or(u16::MAX),
            reflow_wide_continuations,
            reflow_wide_count,
            raw_row_id: RawRowId::UNTRACKED,
        }
    }

    /// Physical column certified as a synthetic early-wide-wrap gap.
    /// Unlike the projection reflow estimate, no visible-cell heuristic is used.
    pub fn synthetic_margin_gap(&self) -> Option<usize> {
        if self.archive_flags & 1 != 0 && self.is_wrapped {
            self.columns().checked_sub(1)
        } else {
            None
        }
    }

    #[inline]
    pub fn reflow_content_len(&self) -> usize {
        self.reflow_content_len as usize
    }

    #[inline]
    pub fn reflow_wide_continuations(&self) -> &[u16] {
        &self.reflow_wide_continuations[..usize::from(self.reflow_wide_count)]
    }

    #[inline]
    pub fn raw_row_id(&self) -> RawRowId {
        self.raw_row_id
    }

    #[inline]
    pub fn set_raw_row_id(&mut self, id: RawRowId) {
        self.raw_row_id = id;
    }

    pub(crate) fn attach_decorations(
        &mut self,
        colors: Vec<UnderlineColorRun>,
        annotations: Option<Arc<RowAnnotations>>,
    ) {
        if !colors.is_empty() || annotations.is_some() {
            if let Some(annotations) = &annotations {
                self.reflow_content_len = self.reflow_content_len.max(annotations.end() as u16);
            }
            let base = std::mem::replace(&mut self.data, CompressedLineData::Encoded(Vec::new()));
            self.data = CompressedLineData::Decorated(Box::new(DecoratedLineData {
                base,
                underline_colors: colors.into_boxed_slice(),
                annotations,
            }));
        }
    }

    /// Used by live reflow to preserve an insertion point in unwritten space.
    pub(crate) fn include_reflow_columns(&mut self, columns: usize) {
        self.reflow_content_len = self
            .reflow_content_len
            .max(columns.min(self.columns()) as u16);
    }

    pub(crate) fn set_synthetic_margin_gap(&mut self, present: bool) {
        self.archive_flags = (self.archive_flags & !1) | u8::from(present);
    }

    pub(crate) fn underline_color_runs(&self) -> &[UnderlineColorRun] {
        match &self.data {
            CompressedLineData::Decorated(data) => &data.underline_colors,
            _ => &[],
        }
    }

    pub(crate) fn row_annotations(&self) -> Option<&Arc<RowAnnotations>> {
        match &self.data {
            CompressedLineData::Decorated(data) => data.annotations.as_ref(),
            _ => None,
        }
    }

    pub fn content(&self) -> ScrollbackContent<'_> {
        self.data.content()
    }

    pub fn decompress(&self) -> Vec<TerminalCell> {
        let mut cells = match self.content() {
            ScrollbackContent::Plain(text, trailing) => {
                let cell = |character| TerminalCell {
                    character,
                    ..TerminalCell::default()
                };
                let mut cells = if text.is_ascii() {
                    let mut cells = Vec::with_capacity(self.columns());
                    cells.extend(text.bytes().map(char::from).map(cell));
                    cells
                } else {
                    let mut cells = Vec::with_capacity(self.columns());
                    cells.extend(text.chars().map(cell));
                    cells
                };
                cells.resize(cells.len() + usize::from(trailing), TerminalCell::default());
                cells
            }
            ScrollbackContent::Encoded(data) => {
                Self::decode_cells(data, self.cols as usize, self.archive_flags)
            }
        };
        self.restore_drawn_spaces(&mut cells);
        cells
    }

    fn restore_drawn_spaces(&self, cells: &mut [TerminalCell]) {
        for (word_index, &word) in self.reflow_wide_continuations
            [usize::from(self.reflow_wide_count)..]
            .iter()
            .enumerate()
        {
            let mut occupied = word;
            while occupied != 0 {
                let bit = occupied.trailing_zeros() as usize;
                if let Some(cell) = cells.get_mut(word_index * 16 + bit) {
                    cell.flags.mark_untracked_drawn_space();
                }
                occupied &= occupied - 1;
            }
        }
    }

    /// Replace scratch cells with this row while retaining reusable capacity.
    pub(crate) fn decompress_into(&self, cells: &mut Vec<TerminalCell>) {
        cells.clear();
        cells.reserve(self.columns());
        match self.content() {
            ScrollbackContent::Plain(text, trailing) => {
                let cell = |character| TerminalCell {
                    character,
                    ..TerminalCell::default()
                };
                if text.is_ascii() {
                    cells.extend(text.bytes().map(char::from).map(cell));
                } else {
                    cells.extend(text.chars().map(cell));
                }
                cells.resize(cells.len() + usize::from(trailing), TerminalCell::default());
            }
            ScrollbackContent::Encoded(data) => {
                Self::decode_cells_into(data, self.cols as usize, self.archive_flags, cells);
            }
        }
        self.restore_drawn_spaces(cells);
    }

    /// Visit retained cells in column order without materializing the row.
    /// Implicit trailing blanks are omitted and runs are capped at its width.
    pub fn visit_search_cells(&self, mut visit: impl FnMut(usize, TerminalCell)) {
        match self.content() {
            ScrollbackContent::Plain(text, _) => {
                for (column, character) in text.chars().enumerate() {
                    visit(
                        column,
                        TerminalCell {
                            character,
                            ..Default::default()
                        },
                    );
                }
            }
            ScrollbackContent::Encoded(data) => {
                Self::visit_encoded_cells(data, self.columns(), self.archive_flags, visit);
            }
        }
    }

    fn visit_encoded_cells(
        data: &[u8],
        cols: usize,
        archive_flags: u8,
        mut visit: impl FnMut(usize, TerminalCell),
    ) {
        let mut column = 0;
        let mut pos = 0;
        while column < cols {
            let Some((cell, run)) = Self::decode_run(data, &mut pos, archive_flags) else {
                break;
            };
            for _ in 0..run.min(cols - column) {
                visit(column, cell);
                column += 1;
            }
        }
    }

    /// Search-only admission cost. Reading it does not decode or scan text.
    pub fn search_encoded_bytes(&self) -> usize {
        match self.content() {
            ScrollbackContent::Plain(text, _) => text.len(),
            ScrollbackContent::Encoded(data) => data.len(),
        }
    }

    /// Size of a retained entry and its allocations, excluding allocator overhead.
    pub(crate) fn retained_bytes(&self) -> usize {
        let reflow_bytes = if self.reflow_wide_continuations.spilled() {
            self.reflow_wide_continuations.capacity() * size_of::<u16>()
        } else {
            0
        };
        size_of::<Self>() + self.data.heap_bytes() + reflow_bytes
    }

    /// Whole-row decode with checked allocation/run limits. Existing render
    /// and copy decoders remain untouched. The caller reserves the entire
    /// row's cells and encoded bytes BEFORE invoking this method.
    pub fn decode_search_row(
        &self,
        max_cells: usize,
        max_encoded: usize,
    ) -> Option<Vec<TerminalCell>> {
        let cols = self.columns();
        if cols > super::MAX_TERMINAL_COLS
            || cols > max_cells
            || self.search_encoded_bytes() > max_encoded
        {
            return None;
        }
        let mut cells = Vec::with_capacity(cols);
        match self.content() {
            ScrollbackContent::Plain(text, trailing) => {
                for character in text.chars() {
                    if cells.len() == cols {
                        return None;
                    }
                    cells.push(TerminalCell {
                        character,
                        ..Default::default()
                    });
                }
                if cells.len().checked_add(usize::from(trailing))? != cols {
                    return None;
                }
            }
            ScrollbackContent::Encoded(data) => {
                let mut pos = 0;
                while pos < data.len() {
                    let ch_len = *data.get(pos)? as usize;
                    pos += 1;
                    if !(1..=4).contains(&ch_len) {
                        return None;
                    }
                    let text =
                        std::str::from_utf8(data.get(pos..pos.checked_add(ch_len)?)?).ok()?;
                    let mut chars = text.chars();
                    let character = chars.next()?;
                    if chars.next().is_some() {
                        return None;
                    }
                    pos += ch_len;
                    let foreground = Self::decode_color(data, &mut pos);
                    let background = Self::decode_color(data, &mut pos);
                    let style = *data.get(pos)?;
                    pos += 1;
                    let extra = *data.get(pos)?;
                    pos += 1;
                    let flags = Self::decode_cell_flags(style, extra, self.archive_flags);
                    let hyperlink_id = HyperlinkId::from_raw(u16::from_le_bytes([
                        *data.get(pos)?,
                        *data.get(pos + 1)?,
                    ]));
                    pos += 2;
                    let run = (*data.get(pos)? as usize).max(1);
                    pos += 1;
                    if cells.len().checked_add(run)? > cols {
                        return None;
                    }
                    cells.resize(
                        cells.len() + run,
                        TerminalCell {
                            character,
                            foreground,
                            background,
                            flags,
                            hyperlink_id,
                        },
                    );
                }
            }
        }
        cells.resize(cols, TerminalCell::default());
        Some(cells)
    }

    /// 供搜索使用的行文本视图,避免对 Plain 行做 decompress → 重建字符串的
    /// 往返。返回(文本, 列映射, 可搜索列数):
    /// - Plain 行只含窄字符(压缩时已排除 wide/wide_continuation),直接借用
    ///   内部字符串,列映射为恒等(None)。
    /// - Encoded 行解码后与活动网格行走同一条 `searchable_line_text` 路径,
    ///   列映射跳过宽字符续接单元。
    pub fn search_text(&self) -> (std::borrow::Cow<'_, str>, Option<Vec<usize>>, usize) {
        match self.content() {
            ScrollbackContent::Plain(text, trailing) => {
                // Ordinary archive widths and trailing blanks already count scalars.
                // Public callers may compress wider rows than the u16 archive geometry.
                let columns = if self.archive_flags & 4 == 0 {
                    self.columns() - usize::from(trailing)
                } else {
                    text.chars().count()
                };
                (std::borrow::Cow::Borrowed(text), None, columns)
            }
            ScrollbackContent::Encoded(_) => {
                let cells = self.decompress();
                let (text, col_map, total_cols) = searchable_line_text(&cells);
                (std::borrow::Cow::Owned(text), Some(col_map), total_cols)
            }
        }
    }

    fn encode_color(color: Color, bytes: &mut [u8]) -> usize {
        bytes[0] = match color {
            Color::Default => 0,
            Color::Black => 1,
            Color::Red => 2,
            Color::Green => 3,
            Color::Yellow => 4,
            Color::Blue => 5,
            Color::Magenta => 6,
            Color::Cyan => 7,
            Color::White => 8,
            Color::BrightBlack => 9,
            Color::BrightRed => 10,
            Color::BrightGreen => 11,
            Color::BrightYellow => 12,
            Color::BrightBlue => 13,
            Color::BrightMagenta => 14,
            Color::BrightCyan => 15,
            Color::BrightWhite => 16,
            Color::Indexed(i) => {
                bytes[..2].copy_from_slice(&[17, i]);
                return 2;
            }
            Color::Rgb(r, g, b) => {
                bytes[..4].copy_from_slice(&[18, r, g, b]);
                return 4;
            }
        };
        1
    }

    fn decode_color(data: &[u8], pos: &mut usize) -> Color {
        if *pos >= data.len() {
            return Color::Default;
        }
        let tag = data[*pos];
        *pos += 1;
        match tag {
            0 => Color::Default,
            1 => Color::Black,
            2 => Color::Red,
            3 => Color::Green,
            4 => Color::Yellow,
            5 => Color::Blue,
            6 => Color::Magenta,
            7 => Color::Cyan,
            8 => Color::White,
            9 => Color::BrightBlack,
            10 => Color::BrightRed,
            11 => Color::BrightGreen,
            12 => Color::BrightYellow,
            13 => Color::BrightBlue,
            14 => Color::BrightMagenta,
            15 => Color::BrightCyan,
            16 => Color::BrightWhite,
            17 => {
                let i = data.get(*pos).copied().unwrap_or(0);
                *pos += 1;
                Color::Indexed(i)
            }
            18 => {
                let r = data.get(*pos).copied().unwrap_or(0);
                let g = data.get(*pos + 1).copied().unwrap_or(0);
                let b = data.get(*pos + 2).copied().unwrap_or(0);
                *pos += 3;
                Color::Rgb(r, g, b)
            }
            _ => Color::Default,
        }
    }

    fn encode_flags(flags: &StyleFlags) -> u8 {
        let mut f = 0u8;
        if flags.bold() {
            f |= 1;
        }
        if flags.italic() {
            f |= 2;
        }
        match flags.underline() {
            UnderlineStyle::None => {}
            UnderlineStyle::Single => f |= 4,
            UnderlineStyle::Double => f |= 8,
            UnderlineStyle::Curly => f |= 12,
            UnderlineStyle::Dotted => f |= 16,
            UnderlineStyle::Dashed => f |= 20,
        }
        if flags.inverse() {
            f |= 32;
        }
        if flags.dim() {
            f |= 64;
        }
        if flags.strikethrough() {
            f |= 128;
        }
        f
    }

    fn decode_flags(f: u8) -> StyleFlags {
        let bits = u16::from(f);
        let underline = bits & UNDERLINE_MASK;
        let underline = if underline <= 5 << UNDERLINE_SHIFT {
            underline
        } else {
            0
        };
        StyleFlags(
            (bits & (BOLD_BIT | ITALIC_BIT | INVERSE_BIT | DIM_BIT))
                | underline
                | ((bits & 128) << 1),
        )
    }

    fn encode_cells(cells: &[TerminalCell], preserve_blink: bool) -> Vec<u8> {
        let mut buf = Vec::with_capacity(cells.len() * 3);
        let mut i = 0;
        while i < cells.len() {
            let cell = &cells[i];

            // RLE: count consecutive identical cells (packed flags comparison is a single u16 ==)
            let mut run = 1u8;
            while (run as u16) < 255 && (i + run as usize) < cells.len() {
                let next = &cells[i + run as usize];
                if next.character == cell.character
                    && next.foreground == cell.foreground
                    && next.background == cell.background
                    && next.flags == cell.flags
                    && next.hyperlink_id == cell.hyperlink_id
                {
                    run += 1;
                } else {
                    break;
                }
            }

            // Format: [char_len:1][char_bytes][fg][bg][flags:1]
            //         [extra_flags:1][hyperlink_id:2][run:1]
            // Assemble one record before appending it to the archive.
            let mut record = [0; 1 + 4 + 4 + 4 + 5];
            let glyph_len = cell.character.encode_utf8(&mut record[1..5]).len();
            record[0] = glyph_len as u8;
            let mut end = 1 + glyph_len;
            end += Self::encode_color(cell.foreground, &mut record[end..]);
            end += Self::encode_color(cell.background, &mut record[end..]);
            let f = Self::encode_flags(&cell.flags);
            let extra_flags = if cell.flags.wide() { 1u8 } else { 0 }
                | if cell.flags.wide_continuation() { 2 } else { 0 }
                | if preserve_blink && cell.flags.blink() {
                    4
                } else {
                    0
                }
                | if cell.flags.printed_space() { 8 } else { 0 }
                | if cell.flags.tab_padding() { 16 } else { 0 }
                | if cell.flags.overline() {
                    ARCHIVE_CELL_OVERLINE
                } else {
                    0
                }
                | if cell.flags.concealed() {
                    ARCHIVE_CELL_CONCEALED
                } else {
                    0
                };
            let [link_low, link_high] = cell.hyperlink_id.as_raw().to_le_bytes();
            record[end..end + 5].copy_from_slice(&[f, extra_flags, link_low, link_high, run]);
            buf.extend_from_slice(&record[..end + 5]);

            i += run as usize;
        }
        buf
    }

    fn decode_cells(data: &[u8], cols: usize, archive_flags: u8) -> Vec<TerminalCell> {
        let mut cells = Vec::with_capacity(cols);
        Self::decode_cells_into(data, cols, archive_flags, &mut cells);
        cells
    }

    #[inline]
    fn decode_cells_into(
        data: &[u8],
        cols: usize,
        archive_flags: u8,
        cells: &mut Vec<TerminalCell>,
    ) {
        let mut pos = 0;
        while cells.len() < cols {
            let Some((cell, run)) = Self::decode_run(data, &mut pos, archive_flags) else {
                break;
            };
            if run == 1 {
                cells.push(cell);
            } else {
                let end = cells.len() + run.min(cols - cells.len());
                cells.resize(end, cell);
            }
        }
        // Pad to cols
        cells.resize(cols, TerminalCell::default());
    }

    // Keep uncommon lengths out of the hot one-to-three-byte decoder.
    #[inline(never)]
    fn decode_uncommon_run_char(bytes: &[u8]) -> char {
        if let [lead @ 0xf0..=0xf4, second @ 0x80..=0xbf, third @ 0x80..=0xbf, last @ 0x80..=0xbf] =
            bytes
        {
            let scalar = (u32::from(lead & 7) << 18)
                | (u32::from(second & 0x3f) << 12)
                | (u32::from(third & 0x3f) << 6)
                | u32::from(last & 0x3f);
            // The lower bound excludes overlong F0; from_u32 enforces the upper bound.
            if scalar >= 0x10000 {
                return char::from_u32(scalar).unwrap_or(' ');
            }
            return ' ';
        }
        std::str::from_utf8(bytes)
            .ok()
            .and_then(|s| s.chars().next())
            .unwrap_or(' ')
    }

    fn decode_cell_flags(style: u8, extra: u8, archive_flags: u8) -> StyleFlags {
        let mut flags = Self::decode_flags(style);
        flags.set_wide(extra & 1 != 0);
        flags.set_wide_continuation(extra & 2 != 0);
        flags.set_blink(extra & 4 != 0);
        if archive_flags & ARCHIVE_PRINTED_SPACES != 0 {
            flags.set_printed_space(extra & 8 != 0);
            flags.set_tab_padding(extra & 16 != 0);
        }
        if archive_flags & ARCHIVE_EXTENDED_RENDITION != 0 {
            flags.set_overline(extra & ARCHIVE_CELL_OVERLINE != 0);
            flags.set_concealed(extra & ARCHIVE_CELL_CONCEALED != 0);
        }
        flags
    }

    fn decode_run(
        data: &[u8],
        pos: &mut usize,
        archive_flags: u8,
    ) -> Option<(TerminalCell, usize)> {
        let ch_len = usize::from(*data.get(*pos)?);
        *pos += 1;
        let bytes = data.get(*pos..*pos + ch_len)?;
        let ch = match bytes {
            [byte] if byte.is_ascii() => char::from(*byte),
            [lead @ 0xc2..=0xdf, last @ 0x80..=0xbf] => {
                let scalar = (u32::from(lead & 0x1f) << 6) | u32::from(last & 0x3f);
                char::from_u32(scalar).unwrap_or(' ')
            }
            // E0 needs an overlong check. from_u32 rejects surrogate scalars.
            [lead @ 0xe1..=0xef, middle @ 0x80..=0xbf, last @ 0x80..=0xbf] => {
                let scalar = (u32::from(lead & 0x0f) << 12)
                    | (u32::from(middle & 0x3f) << 6)
                    | u32::from(last & 0x3f);
                char::from_u32(scalar).unwrap_or(' ')
            }
            _ => Self::decode_uncommon_run_char(bytes),
        };
        *pos += ch_len;

        let foreground = Self::decode_color(data, pos);
        let background = Self::decode_color(data, pos);
        let style = data.get(*pos).copied().unwrap_or(0);
        *pos += 1;
        let extra_flags = data.get(*pos).copied().unwrap_or(0);
        *pos += 1;
        let flags = Self::decode_cell_flags(style, extra_flags, archive_flags);
        let hyperlink_id = HyperlinkId::from_raw(u16::from_le_bytes([
            data.get(*pos).copied().unwrap_or(0),
            data.get(*pos + 1).copied().unwrap_or(0),
        ]));
        *pos += 2;
        let run = usize::from(data.get(*pos).copied().unwrap_or(1).max(1));
        *pos += 1;

        Some((
            TerminalCell {
                character: ch,
                foreground,
                background,
                flags,
                hyperlink_id,
            },
            run,
        ))
    }
}

/// 将网格行转换为字符串,并返回每个字符对应的网格列号。
/// 跳过宽字符的续接单元(否则相邻宽字符间会被插入空格导致匹配失败),
/// 因此字符索引与字节偏移都不再等于列号,需经 col_map 映射。
pub fn searchable_line_text(line: &[TerminalCell]) -> (String, Vec<usize>, usize) {
    let mut searchable_len = line.len();
    while searchable_len > 0 {
        let cell = &line[searchable_len - 1];
        if cell.character == ' '
            && cell.background == Color::Default
            && !cell.flags.wide()
            && !cell.flags.wide_continuation()
        {
            searchable_len -= 1;
        } else {
            break;
        }
    }
    let line = &line[..searchable_len];
    let mut s = String::with_capacity(searchable_len);
    let mut col_map = Vec::with_capacity(searchable_len);
    for (col, cell) in line.iter().enumerate() {
        if cell.flags.wide_continuation() {
            continue;
        }
        s.push(cell.character);
        col_map.push(col);
    }
    (s, col_map, searchable_len)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalCell {
    pub character: char,
    pub foreground: Color,
    pub background: Color,
    pub flags: StyleFlags,
    pub hyperlink_id: HyperlinkId,
}

impl Default for TerminalCell {
    fn default() -> Self {
        TerminalCell {
            character: ' ',
            foreground: Color::Default,
            background: Color::Default,
            flags: StyleFlags::new(),
            hyperlink_id: HyperlinkId::NONE,
        }
    }
}

impl TerminalCell {
    /// Omit trailing padding from a joined logical line. An overline marks
    /// even a space visibly, giving the projected row a cell to paint.
    #[inline]
    pub fn is_reflow_trimmable_blank(&self) -> bool {
        self.character == ' '
            && self.background == Color::Default
            && !self.flags.wide()
            && !self.flags.wide_continuation()
            && !self.flags.printed_space()
            && !self.flags.overline()
            && self.hyperlink_id.is_none()
    }
}

const _: () = assert!(std::mem::size_of::<TerminalCell>() == 16);

/// 追踪改变的行和列区间（脏矩形）
#[derive(Clone, Debug)]
pub struct DirtyRegion {
    pub rows: Vec<(usize, usize)>, // (row_start, row_end)，包含端点
}

impl Default for DirtyRegion {
    fn default() -> Self {
        Self::new()
    }
}

impl DirtyRegion {
    pub fn new() -> Self {
        DirtyRegion { rows: Vec::new() }
    }

    /// 标记某一行为脏
    pub fn mark_row(&mut self, row: usize) {
        if let Some(last) = self.rows.last_mut() {
            if row > 0 && last.1 == row - 1 {
                // 合并相邻的行
                last.1 = row;
                return;
            }
        }
        self.rows.push((row, row));
    }

    /// 标记行范围为脏
    pub fn mark_rows(&mut self, start: usize, end: usize) {
        for row in start..=end {
            self.mark_row(row);
        }
    }

    /// 标记整个网格为脏
    pub fn mark_all(&mut self, rows: usize) {
        self.rows.clear();
        self.rows.push((0, rows.saturating_sub(1)));
    }

    /// 清除脏标记
    pub fn clear(&mut self) {
        self.rows.clear();
    }
}

#[cfg(test)]
mod search_chunk_decode_tests {
    use super::*;
    #[test]
    fn search_chunk_decode_admission_and_rle_expansion_are_bounded() {
        let cells = vec![
            TerminalCell {
                character: 'x',
                foreground: Color::Red,
                ..Default::default()
            };
            1024
        ];
        let mut line = ScrollbackLine::compress(&cells, false);
        let bytes = line.search_encoded_bytes();
        assert!(line.decode_search_row(1023, bytes).is_none());
        assert!(line.decode_search_row(1024, bytes - 1).is_none());
        assert_eq!(line.decode_search_row(1024, bytes).unwrap().len(), 1024);
        // Corrupted compressed input cannot allocate a run beyond cols before
        // its final resize, unlike the general-purpose historical decoder.
        if let CompressedLineData::Encoded(data) = &mut line.data {
            *data.last_mut().unwrap() = 255;
        } else {
            panic!("encoded fixture");
        }
        assert!(line.decode_search_row(1024, bytes).is_none());
    }
    #[test]
    fn search_chunk_plain_padding_has_whole_row_cell_cost() {
        let cells = vec![TerminalCell::default(); 1024];
        let line = ScrollbackLine::compress(&cells, false);
        assert_eq!(line.columns(), 1024);
        assert!(line.decode_search_row(1000, 4096).is_none());
        assert_eq!(line.decode_search_row(1024, 4096).unwrap().len(), 1024);
    }
}

#[cfg(test)]
#[test]
fn latest_capture_change_tracks_cell_region_and_geometry_edits() {
    let mut grid = TerminalGrid::new(4, 8);
    assert!(!grid.captured_rows_unchanged_after(0));
    grid.begin_capture_revision(1);
    grid.begin_capture_revision(2);
    assert!(grid.captured_rows_unchanged_after(1));
    grid.get_mut(3, 0).character = 'X';
    assert!(!grid.captured_rows_unchanged_after(1));
    assert!(grid.captured_rows_unchanged_after(2));

    grid.begin_capture_revision(3);
    grid.scroll_up_by(1, TerminalCell::default());
    assert!(!grid.captured_rows_unchanged_after(2));
    assert!(grid.captured_rows_unchanged_after(3));

    grid.begin_capture_revision(4);
    grid.cells_mut()[0].character = 'Y';
    assert!(!grid.captured_rows_unchanged_after(3));
    grid.begin_capture_revision(5);
    grid.resize(3, 8, TerminalCell::default());
    assert!(!grid.captured_rows_unchanged_after(4));
    assert!(grid.captured_rows_unchanged_after(5));
}

#[cfg(test)]
#[test]
fn ascii_overwrite_changes_only_the_expected_space_and_width_flags() {
    for policy in [
        AsciiOverwritePolicy::SharedScreen,
        AsciiOverwritePolicy::FrostLegacy,
    ] {
        for tracked in [false, true] {
            let mut grid = TerminalGrid::new(1, 4);
            grid.track_printed_spaces = tracked;
            let mut style = TerminalCell::default();
            style.flags.set_bold(true);
            style.flags.set_protected(true);
            style.flags.set_wide(true);
            style.flags.set_wide_continuation(true);
            style.flags.set_printed_space(true);
            let input = b" a b";
            grid.overwrite_ascii(0, 0, input, style, TerminalCell::default(), policy);
            for (cell, byte) in grid[0].iter().zip(input.iter().copied()) {
                assert_eq!(cell.character, char::from(byte));
                assert_eq!(cell.flags.printed_space(), !tracked || byte == b' ');
                assert!(cell.flags.bold() && cell.flags.protected());
                assert!(!cell.flags.wide() && !cell.flags.wide_continuation());
            }
        }
    }
}

#[cfg(test)]
#[test]
fn ascii_overwrite_preserves_each_profiles_margin_and_decoration_invalidation() {
    for (policy, retained) in [
        (AsciiOverwritePolicy::SharedScreen, true),
        (AsciiOverwritePolicy::FrostLegacy, false),
    ] {
        let mut grid = TerminalGrid::new(1, 6);
        grid.set_cluster_text(0, 0, " \u{301}").unwrap();
        grid.set_underline_color(0, 0..1, Color::Red);
        let blank = TerminalCell::default();
        grid.overwrite_ascii(0, 2, b"AB", blank, blank, policy);
        grid.row_wrapped[0] = true;
        grid.record_wrap_margin(0, 5, true);
        let archived = grid.archive_row(0, true);
        assert_eq!(archived.synthetic_margin_gap(), retained.then_some(5));
        assert_eq!(archived.row_annotations().is_some(), retained);
        assert_eq!(!archived.underline_color_runs().is_empty(), retained);
    }
}

#[cfg(test)]
#[test]
fn graphic_overwrite_repairs_both_partners_with_bce_and_preserves_archive_provenance() {
    let background = Color::Blue;
    let blank = TerminalCell {
        background,
        ..TerminalCell::default()
    };
    let mut protected = StyleFlags::default();
    protected.set_protected(true);
    let rendition = TerminalCell {
        character: '界',
        foreground: Color::Red,
        background,
        flags: protected,
        hyperlink_id: HyperlinkId::from_raw(7),
    };

    for (policy, protects_continuation) in [
        (WideContinuationProtection::FromLead, true),
        (WideContinuationProtection::FromBlank, false),
    ] {
        let mut grid = TerminalGrid::new(1, 7);
        let row_id = RawRowId::new(11);
        grid.row_ids_mut()[0] = row_id;
        grid.overwrite_graphic(0, 1, 2, rendition, blank, policy);
        grid.overwrite_graphic(0, 3, 2, rendition, blank, policy);
        grid.set_cluster_text(0, 1, "界\u{301}").unwrap();
        grid.set_cluster_text(0, 3, "界\u{301}").unwrap();
        grid.set_cluster_text(0, 5, " \u{301}").unwrap();
        grid.set_underline_color(0, 1..6, Color::Red);
        grid.row_wrapped[0] = true;
        grid.record_wrap_margin(0, 6, true);

        grid.overwrite_graphic(0, 2, 2, rendition, blank, policy);

        assert_eq!(*grid.get(0, 1), blank);
        let mut expected_lead = rendition;
        expected_lead.flags.set_wide(true);
        assert_eq!(*grid.get(0, 2), expected_lead);
        let mut expected_continuation = blank;
        expected_continuation.hyperlink_id = rendition.hyperlink_id;
        expected_continuation.flags.set_wide_continuation(true);
        expected_continuation
            .flags
            .set_protected(protects_continuation);
        assert_eq!(*grid.get(0, 3), expected_continuation);
        assert_eq!(*grid.get(0, 4), blank);
        let archived = grid.archive_row(0, true);
        assert_eq!(archived.raw_row_id(), row_id);
        assert_eq!(archived.synthetic_margin_gap(), Some(6));
        let archived_clusters = archived.row_annotations().unwrap();
        assert_eq!(archived_clusters.text(1), None);
        assert_eq!(archived_clusters.text(3), None);
        assert_eq!(archived_clusters.text(5), Some(" \u{301}"));
        assert_eq!(
            archived.underline_color_runs(),
            [UnderlineColorRun {
                start: 5,
                end: 6,
                color: Color::Red,
            }]
        );
    }
}

#[cfg(test)]
#[test]
fn clipped_right_edge_erase_repairs_wide_lead_and_retains_outside_metadata() {
    let mut grid = TerminalGrid::new(1, 7);
    let blank = TerminalCell {
        background: Color::Blue,
        ..TerminalCell::default()
    };
    let outside = TerminalCell {
        character: 'a',
        foreground: Color::Red,
        ..TerminalCell::default()
    };
    grid.overwrite_graphic(
        0,
        0,
        1,
        outside,
        blank,
        WideContinuationProtection::FromLead,
    );
    grid.overwrite_graphic(
        0,
        2,
        2,
        TerminalCell {
            character: '界',
            ..outside
        },
        blank,
        WideContinuationProtection::FromLead,
    );
    grid.set_cluster_text(0, 0, "a\u{301}").unwrap();
    grid.set_cluster_text(0, 2, "界\u{301}").unwrap();
    grid.set_cluster_text(0, 5, " \u{301}").unwrap();
    grid.set_underline_color(0, 0..7, Color::Red);
    let captured = grid.row_annotations(0).unwrap().clone();

    grid.erase_cells(0, 3..usize::MAX, blank);

    assert_eq!(*grid.get(0, 0), outside);
    assert!(grid[0][2..].iter().all(|cell| *cell == blank));
    assert_eq!(grid.row_annotations(0).unwrap().text(0), Some("a\u{301}"));
    assert_eq!(grid.row_annotations(0).unwrap().text(2), None);
    assert_eq!(grid.row_annotations(0).unwrap().text(5), None);
    assert_eq!(captured.text(2), Some("界\u{301}"));
    assert_eq!(captured.text(5), Some(" \u{301}"));
    assert_eq!(
        grid.underline_color_runs(0).collect::<Vec<_>>(),
        [UnderlineColorRun {
            start: 0,
            end: 2,
            color: Color::Red
        }]
    );
}

#[cfg(test)]
#[test]
fn middle_erase_matches_scalar_order_for_all_small_wide_flag_patterns() {
    fn scalar(grid: &mut TerminalGrid, span: Range<usize>, blank: TerminalCell) {
        for col in span.start..span.end.min(grid.cols()) {
            let flags = grid.get(0, col).flags;
            if col > 0 && flags.wide_continuation() {
                *grid.get_mut(0, col - 1) = blank;
            }
            if col + 1 < grid.cols() && flags.wide() {
                *grid.get_mut(0, col + 1) = blank;
            }
            *grid.get_mut(0, col) = blank;
        }
    }
    for bits in 0..1024usize {
        let mut seed = TerminalGrid::new(1, 5);
        for col in 0..5 {
            let cell = seed.get_mut(0, col);
            cell.character = char::from(b'a' + col as u8);
            cell.flags.set_wide(bits & (1 << (2 * col)) != 0);
            cell.flags
                .set_wide_continuation(bits & (2 << (2 * col)) != 0);
            seed.set_cluster_text(0, col, "e\u{301}").unwrap();
        }
        seed.set_underline_color(0, 0..5, Color::Red);
        for start in 0..5 {
            for end in start + 1..=5 {
                for blank_bits in 0..4 {
                    let mut blank = TerminalCell {
                        background: Color::Blue,
                        ..TerminalCell::default()
                    };
                    blank.flags.set_wide(blank_bits & 1 != 0);
                    blank.flags.set_wide_continuation(blank_bits & 2 != 0);
                    let mut actual = seed.clone();
                    let mut expected = seed.clone();
                    actual.erase_cells(0, start..end, blank);
                    scalar(&mut expected, start..end, blank);
                    assert_eq!(
                        actual.cells, expected.cells,
                        "bits={bits} span={start}..{end} blank={blank_bits}"
                    );
                    assert_eq!(actual.underline_colors, expected.underline_colors);
                    assert_eq!(actual.annotations, expected.annotations);
                }
            }
        }
    }
}

#[cfg(test)]
#[test]
fn archived_underline_colors_keep_the_original_row_payload_and_size() {
    assert_eq!(std::mem::size_of::<TerminalCell>(), 16);
    #[cfg(target_pointer_width = "64")]
    assert_eq!(std::mem::size_of::<ScrollbackLine>(), 72);
    for style in [UnderlineStyle::None, UnderlineStyle::Curly] {
        let mut grid = TerminalGrid::new(2, 6);
        grid[0][1].character = 'é';
        grid[0][1].flags.set_underline(style);
        let original = grid.archive_row(0, false);
        grid.set_underline_color(0, 1..3, Color::Rgb(1, 2, 3));
        let decorated = grid.archive_row(0, false);
        assert_eq!(
            format!("{:?}", original.content()),
            format!("{:?}", decorated.content())
        );
        assert_eq!(
            original.search_encoded_bytes(),
            decorated.search_encoded_bytes()
        );
        assert_eq!(original.decompress(), decorated.decompress());
        assert_eq!(
            decorated.underline_color_runs(),
            [UnderlineColorRun {
                start: 1,
                end: 3,
                color: Color::Rgb(1, 2, 3)
            }]
        );
    }
}

#[cfg(test)]
#[test]
fn archived_cluster_preserves_the_codec_and_counts_decorated_space_as_content() {
    let mut grid = TerminalGrid::new(1, 6);
    grid[0][1].character = 'a';
    let original = grid.archive_row(0, false);
    grid.set_cluster_text(0, 1, "a\u{301}").unwrap();
    grid.set_cluster_text(0, 5, " \u{300}").unwrap();
    let decorated = grid.archive_row(0, false);
    assert!(matches!(
        original.content(),
        ScrollbackContent::Plain(" a", 4)
    ));
    assert!(matches!(
        decorated.content(),
        ScrollbackContent::Plain(" a", 4)
    ));
    assert_eq!(original.decompress(), decorated.decompress());
    assert_eq!(original.reflow_content_len(), 2);
    assert_eq!(decorated.reflow_content_len(), 6);
    assert_eq!(
        decorated.row_annotations().unwrap().text(5),
        Some(" \u{300}")
    );
    grid.erase_cells(0, 0..6, TerminalCell::default());
    assert!(grid.row_annotations(0).is_none());
    assert_eq!(
        decorated.row_annotations().unwrap().text(1),
        Some("a\u{301}")
    );
}

// Regression oracle pinned to fb477d1c; deliberately independent of the streaming visitor.
#[cfg(test)]
impl ScrollbackLine {
    fn pinned_decode_cells(data: &[u8], cols: usize) -> Vec<TerminalCell> {
        let mut cells = Vec::with_capacity(cols);
        let mut pos = 0;
        while pos < data.len() {
            let ch_len = data[pos] as usize;
            pos += 1;
            if pos + ch_len > data.len() {
                break;
            }
            let ch = std::str::from_utf8(&data[pos..pos + ch_len])
                .ok()
                .and_then(|s| s.chars().next())
                .unwrap_or(' ');
            pos += ch_len;

            let fg = Self::decode_color(data, &mut pos);
            let bg = Self::decode_color(data, &mut pos);
            let f = data.get(pos).copied().unwrap_or(0);
            pos += 1;
            let extra_flags = data.get(pos).copied().unwrap_or(0);
            pos += 1;
            let hyperlink = u16::from_le_bytes([
                data.get(pos).copied().unwrap_or(0),
                data.get(pos + 1).copied().unwrap_or(0),
            ]);
            pos += 2;
            let run = data.get(pos).copied().unwrap_or(1).max(1);
            pos += 1;

            let mut flags = Self::decode_flags(f);
            flags.set_wide(extra_flags & 1 != 0);
            flags.set_wide_continuation(extra_flags & 2 != 0);
            flags.set_blink(extra_flags & 4 != 0);

            let cell = TerminalCell {
                character: ch,
                foreground: fg,
                background: bg,
                flags,
                hyperlink_id: HyperlinkId::from_raw(hyperlink),
            };
            for _ in 0..run {
                cells.push(cell);
            }
        }
        // Pad to cols
        cells.resize(cols, TerminalCell::default());
        cells
    }
}

#[cfg(test)]
mod decoder_parity {
    use super::*;

    #[test]
    fn packed_archive_flags_match_scalar_reference_for_all_bytes() {
        for byte in 0..=u8::MAX {
            let mut expected = StyleFlags::default();
            expected.set_bold(byte & 1 != 0);
            expected.set_italic(byte & 2 != 0);
            expected.set_underline(match (byte >> 2) & 7 {
                1 => UnderlineStyle::Single,
                2 => UnderlineStyle::Double,
                3 => UnderlineStyle::Curly,
                4 => UnderlineStyle::Dotted,
                5 => UnderlineStyle::Dashed,
                _ => UnderlineStyle::None,
            });
            expected.set_inverse(byte & 32 != 0);
            expected.set_dim(byte & 64 != 0);
            expected.set_strikethrough(byte & 128 != 0);
            assert_eq!(ScrollbackLine::decode_flags(byte), expected, "{byte}");
        }
    }

    #[test]
    fn plain_search_columns_match_scalars_through_archive_geometry_overflow() {
        for width in [0, 1, 96, 200, 65_535, 65_536, 65_537] {
            for (character, trailing) in [('a', 0), ('é', 1), ('Ж', width / 2), (' ', width)] {
                let mut row = vec![TerminalCell::default(); width];
                for cell in row.iter_mut().take(width.saturating_sub(trailing)) {
                    cell.character = character;
                }
                let line = ScrollbackLine::compress(&row, false);
                let (text, map, columns) = line.search_text();
                assert_eq!(columns, text.chars().count(), "width {width}");
                assert!(map.is_none());
            }
        }
    }

    #[test]
    fn reused_decode_scratch_replaces_prior_rows_and_keeps_capacity() {
        let mut scratch = Vec::with_capacity(600);
        let capacity = scratch.capacity();
        for (width, character, colored) in [
            (400, 'x', false),
            (3, 'é', false),
            (256, '界', true),
            (0, ' ', false),
            (120, 'z', true),
        ] {
            let row = vec![
                TerminalCell {
                    character,
                    foreground: if colored { Color::Red } else { Color::Default },
                    ..TerminalCell::default()
                };
                width
            ];
            let retained = ScrollbackLine::compress(&row, false);
            retained.decompress_into(&mut scratch);
            assert_eq!(scratch, row);
            assert_eq!(scratch.capacity(), capacity);
        }
    }

    #[test]
    fn stored_decoder_rle_boundary_clipping_parity() {
        let cell = TerminalCell {
            character: 'İ',
            foreground: Color::Rgb(1, 2, 3),
            hyperlink_id: HyperlinkId::from_raw(65535),
            ..Default::default()
        };
        for n in [0, 1, 254, 255, 256, 510, 511, 600] {
            let bytes = ScrollbackLine::encode_cells(&vec![cell; n], true);
            for cols in [0, 1, 17, 254, 255, 256, 511, 601] {
                for length in 0..=bytes.len() {
                    let mut streamed = Vec::new();
                    ScrollbackLine::visit_encoded_cells(&bytes[..length], cols, 0, |_, cell| {
                        streamed.push(cell)
                    });
                    streamed.resize(cols, TerminalCell::default());
                    assert_eq!(
                        streamed,
                        ScrollbackLine::pinned_decode_cells(&bytes[..length], cols)
                    );
                    assert_eq!(
                        ScrollbackLine::decode_cells(&bytes[..length], cols, 0),
                        streamed
                    );
                }
            }
        }
    }

    #[test]
    fn longest_archive_record_keeps_both_rgb_colors_and_the_run_boundary() {
        let mut cell = TerminalCell {
            character: '𐐀',
            foreground: Color::Rgb(1, 2, 3),
            background: Color::Rgb(4, 5, 6),
            hyperlink_id: HyperlinkId::from_raw(0x1234),
            ..TerminalCell::default()
        };
        cell.flags.set_bold(true);
        cell.flags.set_blink(true);
        cell.flags.set_underline(UnderlineStyle::Dashed);
        let cells = vec![cell; 256];
        let run = [
            4, 0xf0, 0x90, 0x90, 0x80, 18, 1, 2, 3, 18, 4, 5, 6, 21, 4, 0x34, 0x12, 255,
        ];
        let mut expected = run.repeat(2);
        *expected.last_mut().unwrap() = 1;
        let line = ScrollbackLine::compress_preserving_blink(&cells, true);
        assert!(
            matches!(line.content(), ScrollbackContent::Encoded(bytes) if bytes == expected.as_slice())
        );
        assert_eq!(line.decompress(), cells);
    }

    #[test]
    fn encoded_runs_preserve_valid_scalars_and_replace_invalid_utf8() {
        let cell = TerminalCell {
            character: '界',
            foreground: Color::Red,
            ..Default::default()
        };
        let encoded = ScrollbackLine::encode_cells(&[cell; 2], true);
        for (bytes, character) in [
            ([0xc2, 0x80].as_slice(), '\u{80}'),
            ([0xc3, 0xa9].as_slice(), 'é'),
            ([0xd0, 0x96].as_slice(), 'Ж'),
            ([0xdf, 0xbf].as_slice(), '\u{7ff}'),
            ([0xc0, 0x80].as_slice(), ' '),
            ([0xc3, b'A'].as_slice(), ' '),
            ([0xe0, 0xa0, 0x80].as_slice(), '\u{800}'),
            ([0xe0, 0x9f, 0xbf].as_slice(), ' '),
            ([0xe7, 0x95, 0x8c].as_slice(), '界'),
            ([0xed, 0x9f, 0xbf].as_slice(), '\u{d7ff}'),
            ([0xed, 0xa0, 0x80].as_slice(), ' '),
            ([0xef, 0xbf, 0xbd].as_slice(), '\u{fffd}'),
            ([0xf0, 0x90, 0x80, 0x80].as_slice(), '\u{10000}'),
            ([0xf0, 0x9f, 0xa7, 0xaa].as_slice(), '🧪'),
            ([0xf4, 0x8f, 0xbf, 0xbf].as_slice(), '\u{10ffff}'),
            ([0xf0, 0x8f, 0xbf, 0xbf].as_slice(), ' '),
            ([0xf4, 0x90, 0x80, 0x80].as_slice(), ' '),
            ([0xf0, 0x90, 0x80, b'Y'].as_slice(), ' '),
        ] {
            let mut modified = vec![bytes.len() as u8];
            modified.extend_from_slice(bytes);
            modified.extend_from_slice(&encoded[4..]);
            let expected = [TerminalCell { character, ..cell }; 2];
            assert_eq!(
                ScrollbackLine::decode_cells(&modified, 2, 0),
                expected,
                "{bytes:02x?}"
            );
        }
    }

    #[test]
    fn stored_decoder_matches_pinned_oracle_on_truncation_and_runs() {
        let mut seed = 0xdefaced_u64;
        for length in 0..128 {
            for _ in 0..64 {
                let mut bytes = Vec::new();
                for _ in 0..length {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    bytes.push(seed as u8);
                }
                for cols in [0, 1, 2, 17, 255, 512] {
                    let mut streamed = Vec::new();
                    ScrollbackLine::visit_encoded_cells(&bytes, cols, 0, |_, cell| {
                        streamed.push(cell)
                    });
                    streamed.resize(cols, TerminalCell::default());
                    assert_eq!(streamed, ScrollbackLine::pinned_decode_cells(&bytes, cols));
                    assert_eq!(ScrollbackLine::decode_cells(&bytes, cols, 0), streamed);
                }
            }
        }
    }
}

#[cfg(test)]
mod rectangular_copy_tests {
    use super::*;

    #[test]
    fn overlapping_row_copy_snapshots_text_and_decoration() {
        let mut grid = TerminalGrid::new(1, 8);
        for (column, character) in "abcdefgh".chars().enumerate() {
            grid.get_mut(0, column).character = character;
        }
        grid.set_cluster_text(0, 1, "b\u{301}");
        grid.set_cluster_text(0, 7, "h\u{301}");
        grid.set_underline_color(0, 1..3, Color::Rgb(1, 2, 3));
        let original = grid
            .row_annotations(0)
            .unwrap()
            .shared_text(1)
            .unwrap()
            .clone();
        let mut scratch = RectangularCopyScratch::default();
        assert!(grid.rectangular_copy_fits(0, 0..5, 0, 2));
        assert_eq!(grid.copy_rectangular_row(0, 0..5, 0, 2, &mut scratch), 2..7);
        assert_eq!(
            grid.cells
                .iter()
                .map(|cell| cell.character)
                .collect::<String>(),
            "ababcdeh"
        );
        assert!(Arc::ptr_eq(
            grid.row_annotations(0).unwrap().shared_text(3).unwrap(),
            &original
        ));
        assert_eq!(grid.cluster_text(0, 7), Some("h\u{301}"));
        assert_eq!(grid.underline_colors(0)[3..5], [Color::Rgb(1, 2, 3); 2]);
        assert_eq!(grid.underline_colors(0)[2], Color::Default);
    }

    #[test]
    fn clipped_halves_preserve_style_and_clear_target_partners() {
        let mut grid = TerminalGrid::new(2, 8);
        grid.get_mut(0, 1).flags.set_wide(true);
        grid.get_mut(0, 2).flags.set_wide_continuation(true);
        grid.get_mut(0, 4).flags.set_wide(true);
        grid.get_mut(0, 5).flags.set_wide_continuation(true);
        grid.get_mut(0, 2).foreground = Color::Rgb(1, 2, 3);
        grid.set_cluster_text(0, 2, "excluded");
        grid.set_cluster_text(0, 4, "excluded");
        grid.set_underline_color(0, 2..5, Color::Rgb(4, 5, 6));
        grid.get_mut(1, 0).flags.set_wide(true);
        grid.get_mut(1, 1).flags.set_wide_continuation(true);
        grid.get_mut(1, 3).flags.set_wide(true);
        grid.get_mut(1, 4).flags.set_wide_continuation(true);
        let mut scratch = RectangularCopyScratch::default();
        assert_eq!(grid.copy_rectangular_row(0, 2..5, 1, 1, &mut scratch), 0..5);
        for column in [1, 3] {
            assert_eq!(grid.get(1, column).character, ' ');
            assert!(!grid.get(1, column).flags.wide());
            assert!(!grid.get(1, column).flags.wide_continuation());
            assert_eq!(grid.cluster_text(1, column), None);
        }
        assert_eq!(*grid.get(1, 0), TerminalCell::default());
        assert_eq!(*grid.get(1, 4), TerminalCell::default());
        assert_eq!(grid.get(1, 1).foreground, Color::Rgb(1, 2, 3));
        assert_eq!(grid.underline_colors(1)[1..4], [Color::Rgb(4, 5, 6); 3]);
        assert!(!grid.rectangular_copy_fits(0, 2..5, 1, usize::MAX));
        assert!(!grid.rectangular_copy_fits(0, 5..5, 1, 0));
    }
}

#[cfg(test)]
mod rectangular_rendition_tests {
    use super::*;

    #[test]
    fn compiled_dec_edits_preserve_unrelated_bits_and_compose_in_order() {
        let affected = BOLD_BIT | BLINK_BIT | INVERSE_BIT | UNDERLINE_MASK;
        for bits in 0..=u16::MAX {
            let original = StyleFlags(bits);
            for parameter in [0, 1, 4, 5, 7, 22, 24, 25, 27, 8, 65535] {
                let changed = RectangularRendition::compile(&[parameter], false).apply(original);
                assert_eq!(changed.0 & !affected, bits & !affected);
                let mut expected = original;
                match parameter {
                    0 => {
                        expected.set_bold(false);
                        expected.set_underline(UnderlineStyle::None);
                        expected.set_blink(false);
                        expected.set_inverse(false);
                    }
                    1 => expected.set_bold(true),
                    4 => expected.set_underline(UnderlineStyle::Single),
                    5 => expected.set_blink(true),
                    7 => expected.set_inverse(true),
                    22 => expected.set_bold(false),
                    24 => expected.set_underline(UnderlineStyle::None),
                    25 => expected.set_blink(false),
                    27 => expected.set_inverse(false),
                    _ => {}
                }
                assert_eq!(changed, expected);
                let doubled =
                    RectangularRendition::compile(&[parameter, parameter], true).apply(original);
                assert_eq!(doubled, original);
            }
            assert_eq!(
                RectangularRendition::compile(&[1, 22, 4, 24, 5, 25, 7, 27], false).apply(original),
                RectangularRendition::compile(&[0], false).apply(original)
            );
            assert_eq!(
                RectangularRendition::compile(&[], false).apply(original),
                RectangularRendition::compile(&[0], false).apply(original)
            );
        }
    }

    #[test]
    fn reverse_underline_keeps_all_authored_styles_under_even_parity() {
        for style in [
            UnderlineStyle::None,
            UnderlineStyle::Single,
            UnderlineStyle::Double,
            UnderlineStyle::Curly,
            UnderlineStyle::Dotted,
            UnderlineStyle::Dashed,
        ] {
            let mut original = StyleFlags::default();
            original.set_underline(style);
            assert_eq!(
                RectangularRendition::compile(&[4, 0, 4, 0], true).apply(original),
                original
            );
            assert_eq!(
                RectangularRendition::compile(&[4], true)
                    .apply(original)
                    .underline(),
                if style == UnderlineStyle::None {
                    UnderlineStyle::Single
                } else {
                    UnderlineStyle::None
                }
            );
        }
    }
}

#[cfg(test)]
mod retained_drawn_space_tests {
    use super::*;

    #[test]
    fn plain_and_encoded_rows_restore_untracked_drawn_spaces_including_trimmed_tail() {
        for encoded in [false, true] {
            let mut cells = vec![TerminalCell::default(); 49];
            cells[0].character = 'X';
            for column in [1, 15, 16, 32, 48] {
                cells[column].flags.mark_untracked_drawn_space();
            }
            if encoded {
                cells[0].foreground = Color::Red;
                cells[2].character = '界';
                cells[2].flags.set_wide(true);
                cells[3].flags.set_wide_continuation(true);
            }
            let line = ScrollbackLine::compress(&cells, false);
            assert_eq!(
                matches!(line.content(), ScrollbackContent::Encoded(_)),
                encoded
            );
            assert_eq!(
                line.reflow_wide_continuations(),
                if encoded { &[3][..] } else { &[][..] }
            );
            assert_eq!(line.decompress(), cells);
            let mut scratch = Vec::new();
            line.decompress_into(&mut scratch);
            assert_eq!(scratch, cells);
            let cloned = line.clone();
            assert_eq!(cloned.decompress(), cells);
            assert!(
                line.retained_bytes()
                    >= size_of::<ScrollbackLine>()
                        + line.reflow_wide_continuations.capacity() * size_of::<u16>()
            );
        }
    }

    #[test]
    fn stored_and_reflowed_tails_keep_their_distinct_boundaries() {
        let mut cells = vec![TerminalCell::default(); 33];
        cells[0].character = 'x';
        cells[4].character = '界';
        cells[4].flags.set_wide(true);
        cells[5].flags.set_wide_continuation(true);
        cells[8].flags.set_printed_space(true);
        cells[8].flags.set_tab_padding(true);
        cells[11].foreground = Color::Yellow;
        cells[16].flags.mark_untracked_drawn_space();
        cells[32].flags.mark_untracked_drawn_space();

        let line = ScrollbackLine::compress(&cells, false);
        assert_eq!(line.reflow_content_len(), 9);
        assert_eq!(line.reflow_wide_continuations(), &[5]);
        let mut stored = Vec::new();
        line.visit_search_cells(|column, cell| {
            assert_eq!(column, stored.len());
            stored.push(cell);
        });
        assert_eq!(stored, cells[..12]);
        assert_eq!(line.decompress(), cells);
    }
}

#[cfg(test)]
#[test]
fn styled_history_admission_still_preserves_later_printed_tab_spaces() {
    let mut cells = vec![TerminalCell::default(); 8];
    cells[0].character = 'X';
    cells[0].foreground = Color::Red;
    cells[5].flags.set_printed_space(true);
    cells[5].flags.set_tab_padding(true);
    for line in [
        ScrollbackLine::compress(&cells, false),
        ScrollbackLine::compress_preserving_blink(&cells, false),
    ] {
        assert!(matches!(line.content(), ScrollbackContent::Encoded(_)));
        assert_eq!(line.decompress(), cells);
        let mut scratch = Vec::new();
        line.decompress_into(&mut scratch);
        assert_eq!(scratch, cells);
    }
}
