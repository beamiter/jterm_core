use super::hyperlink::HyperlinkId;
use crate::{graphemes::MAX_CLUSTER_BYTES, row_annotations::RowAnnotations};
use smallvec::SmallVec;
use std::ops::{Range, RangeInclusive};
use std::sync::Arc;

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
    margin_origin: Vec<MarginOrigin>,
    // Allocate the row table on first colored output, and allocate cells only
    // for rows with explicit decoration. The base cell stays 16 bytes.
    underline_colors: Vec<Option<Vec<Color>>>,
    annotations: Vec<Option<Arc<RowAnnotations>>>,
    capture_revision: u64,
    row_changes: Vec<u64>,
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
            margin_origin: vec![MarginOrigin::VirginDefault; rows],
            underline_colors: Vec::new(),
            annotations: Vec::new(),
            capture_revision: 0,
            row_changes: Vec::new(),
            track_printed_spaces: false,
        }
    }

    pub(crate) fn begin_capture_revision(&mut self, revision: u64) {
        self.capture_revision = revision;
        self.row_changes.resize(self.rows, revision);
    }

    /// Grid users that do not start capture revisions have no change certificate.
    /// Legacy public wrap bits are compared independently of this certificate.
    pub(crate) fn row_changed_after(&self, row: usize, revision: u64) -> bool {
        self.row_changes
            .get(row)
            .is_none_or(|changed_at| *changed_at > revision)
    }

    #[inline]
    fn mark_row_changed(&mut self, row: usize) {
        if self.capture_revision != 0 {
            self.row_changes[row] = self.capture_revision;
        }
    }

    /// Flat cells with a fixed length. Hosts must preserve wide-cell invariants.
    pub fn cells_mut(&mut self) -> &mut [TerminalCell] {
        self.row_changes.fill(self.capture_revision);
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
        self.row_ids
            .get(row)
            .copied()
            .unwrap_or(RawRowId::UNTRACKED)
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
        mut rendition: TerminalCell,
        blank: TerminalCell,
        policy: AsciiOverwritePolicy,
    ) {
        if bytes.is_empty() {
            return;
        }
        assert!(row < self.rows && bytes.len() <= self.cols.saturating_sub(col));
        let end = col + bytes.len();
        let track_spaces = self.track_printed_spaces;
        debug_assert!(bytes.iter().all(|byte| (0x20..=0x7e).contains(byte)));
        let target = match policy {
            AsciiOverwritePolicy::SharedScreen => {
                self.repair_wide_edges(row, col..end, blank);
                self.erase_annotations(row, col..end);
                self.row_cells_mut(row, col..end)
            }
            AsciiOverwritePolicy::FrostLegacy => {
                if col > 0 && self.get(row, col).flags.wide_continuation() {
                    *self.get_mut(row, col - 1) = blank;
                }
                if end < self.cols && self.get(row, end - 1).flags.wide() {
                    *self.get_mut(row, end) = blank;
                }
                &mut self[row][col..end]
            }
        };
        rendition.flags.set_wide(false);
        rendition.flags.set_wide_continuation(false);
        for (cell, &byte) in target.iter_mut().zip(bytes) {
            rendition.character = char::from(byte);
            if track_spaces {
                rendition.flags.set_printed_space(byte == b' ');
            }
            *cell = rendition;
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
        if self.track_printed_spaces {
            rendition
                .flags
                .set_printed_space(rendition.character == ' ');
        }
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
            line.synthetic_margin_gap = true;
        }
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
        if self.capture_revision != 0 {
            self.row_changes[top..end].fill(self.capture_revision);
        }
        let height = end - top;
        let count = count.min(height);
        let move_wraps = wrap_policy == RegionWrapPolicy::MoveWithRows;
        let preserve_margin = self.preserves_shift_margin(direction, &region, wrap_policy);
        let (source, target, blank_rows) = match direction {
            RegionDirection::Up => (top + count..end, top, end - count..end),
            RegionDirection::Down => (top..end - count, top + count, top..top + count),
        };
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
        if row >= self.rows {
            return;
        }
        for col in cols.start..cols.end.min(self.cols) {
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
///   bit 13: padding produced by a tab
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

impl StyleFlags {
    #[inline(always)]
    pub fn new() -> Self {
        Self(0)
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
        self.0 & TAB_PADDING_BIT != 0
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

#[derive(Clone, Debug)]
pub struct ScrollbackLine {
    data: CompressedLineData,
    pub is_wrapped: bool,
    synthetic_margin_gap: bool,
    printed_spaces: bool,
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
    raw_row_id: RawRowId,
}

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

    fn compress_with_style(cells: &[TerminalCell], is_wrapped: bool, preserve_blink: bool) -> Self {
        let cols = cells.len() as u16;
        let reflow_content_len = cells
            .iter()
            .rposition(|cell| !cell.is_reflow_trimmable_blank())
            .map_or(0, |index| index + 1);
        let reflow_wide_continuations = cells[..reflow_content_len]
            .iter()
            .enumerate()
            .filter_map(|(column, cell)| {
                if cell.flags.wide_continuation() {
                    u16::try_from(column).ok()
                } else {
                    None
                }
            })
            .collect();
        let reflow_content_len = u16::try_from(reflow_content_len).unwrap_or(u16::MAX);
        let trailing_blanks = cells
            .iter()
            .rev()
            .take_while(|c| {
                c.character == ' '
                    && c.foreground == Color::Default
                    && c.background == Color::Default
                    && c.flags.is_default_style()
                    && !c.flags.wide()
                    && !c.flags.wide_continuation()
                    && !c.flags.printed_space()
                    && c.hyperlink_id.is_none()
            })
            .count();

        let active_len = cells.len() - trailing_blanks;
        let printed_spaces = cells[..active_len]
            .iter()
            .any(|cell| cell.flags.printed_space());
        let all_default_attrs = !printed_spaces
            && cells[..active_len].iter().all(|c| {
                c.foreground == Color::Default
                    && c.background == Color::Default
                    && c.flags.is_default_style()
                    && !c.flags.wide()
                    && !c.flags.wide_continuation()
                    && c.hyperlink_id.is_none()
            });

        if all_default_attrs {
            let text: String = cells[..active_len].iter().map(|c| c.character).collect();
            ScrollbackLine {
                data: CompressedLineData::Plain(text, trailing_blanks as u16),
                is_wrapped,
                synthetic_margin_gap: false,
                printed_spaces: false,
                cols,
                reflow_content_len,
                reflow_wide_continuations,
                raw_row_id: RawRowId::UNTRACKED,
            }
        } else {
            let encoded = Self::encode_cells(&cells[..active_len], preserve_blink);
            ScrollbackLine {
                data: CompressedLineData::Encoded(encoded),
                is_wrapped,
                synthetic_margin_gap: false,
                printed_spaces,
                cols,
                reflow_content_len,
                reflow_wide_continuations,
                raw_row_id: RawRowId::UNTRACKED,
            }
        }
    }

    /// Physical column certified as a synthetic early-wide-wrap gap.
    /// Unlike the projection reflow estimate, no visible-cell heuristic is used.
    pub fn synthetic_margin_gap(&self) -> Option<usize> {
        if self.synthetic_margin_gap && self.is_wrapped {
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
        &self.reflow_wide_continuations
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
        self.synthetic_margin_gap = present;
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
        match self.content() {
            ScrollbackContent::Plain(text, trailing) => {
                let mut cells: Vec<TerminalCell> = text
                    .chars()
                    .map(|ch| TerminalCell {
                        character: ch,
                        ..Default::default()
                    })
                    .collect();
                cells.resize(cells.len() + usize::from(trailing), TerminalCell::default());
                cells
            }
            ScrollbackContent::Encoded(data) => {
                Self::decode_cells(data, self.cols as usize, self.printed_spaces)
            }
        }
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
                Self::visit_encoded_cells(data, self.columns(), self.printed_spaces, visit);
            }
        }
    }

    fn visit_encoded_cells(
        data: &[u8],
        cols: usize,
        printed_spaces: bool,
        mut visit: impl FnMut(usize, TerminalCell),
    ) {
        let mut column = 0;
        let mut pos = 0;
        while column < cols {
            let Some((cell, run)) = Self::decode_run(data, &mut pos, printed_spaces) else {
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
                    let mut flags = Self::decode_flags(*data.get(pos)?);
                    pos += 1;
                    let wide = *data.get(pos)?;
                    pos += 1;
                    flags.set_wide(wide & 1 != 0);
                    flags.set_wide_continuation(wide & 2 != 0);
                    flags.set_blink(wide & 4 != 0);
                    flags.set_printed_space(self.printed_spaces && wide & 8 != 0);
                    flags.set_tab_padding(self.printed_spaces && wide & 16 != 0);
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
            ScrollbackContent::Plain(text, _) => {
                (std::borrow::Cow::Borrowed(text), None, text.chars().count())
            }
            ScrollbackContent::Encoded(_) => {
                let cells = self.decompress();
                let (text, col_map, total_cols) = searchable_line_text(&cells);
                (std::borrow::Cow::Owned(text), Some(col_map), total_cols)
            }
        }
    }

    fn encode_color(color: &Color, buf: &mut Vec<u8>) {
        match color {
            Color::Default => buf.push(0),
            Color::Black => buf.push(1),
            Color::Red => buf.push(2),
            Color::Green => buf.push(3),
            Color::Yellow => buf.push(4),
            Color::Blue => buf.push(5),
            Color::Magenta => buf.push(6),
            Color::Cyan => buf.push(7),
            Color::White => buf.push(8),
            Color::BrightBlack => buf.push(9),
            Color::BrightRed => buf.push(10),
            Color::BrightGreen => buf.push(11),
            Color::BrightYellow => buf.push(12),
            Color::BrightBlue => buf.push(13),
            Color::BrightMagenta => buf.push(14),
            Color::BrightCyan => buf.push(15),
            Color::BrightWhite => buf.push(16),
            Color::Indexed(i) => {
                buf.push(17);
                buf.push(*i);
            }
            Color::Rgb(r, g, b) => {
                buf.push(18);
                buf.push(*r);
                buf.push(*g);
                buf.push(*b);
            }
        }
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
        let underline = match (f >> 2) & 0x7 {
            0 => UnderlineStyle::None,
            1 => UnderlineStyle::Single,
            2 => UnderlineStyle::Double,
            3 => UnderlineStyle::Curly,
            4 => UnderlineStyle::Dotted,
            5 => UnderlineStyle::Dashed,
            _ => UnderlineStyle::None,
        };
        let mut flags = StyleFlags::new();
        flags.set_bold(f & 1 != 0);
        flags.set_italic(f & 2 != 0);
        flags.set_underline(underline);
        flags.set_inverse(f & 32 != 0);
        flags.set_dim(f & 64 != 0);
        flags.set_strikethrough(f & 128 != 0);
        flags
    }

    fn encode_cells(cells: &[TerminalCell], preserve_blink: bool) -> Vec<u8> {
        let mut buf = Vec::with_capacity(cells.len() * 3);
        let mut i = 0;
        while i < cells.len() {
            let cell = &cells[i];
            let ch_str = cell.character.to_string();
            let ch_bytes = ch_str.as_bytes();

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
            buf.push(ch_bytes.len() as u8);
            buf.extend_from_slice(ch_bytes);
            Self::encode_color(&cell.foreground, &mut buf);
            Self::encode_color(&cell.background, &mut buf);
            let f = Self::encode_flags(&cell.flags);
            buf.push(f);
            let extra_flags = if cell.flags.wide() { 1u8 } else { 0 }
                | if cell.flags.wide_continuation() { 2 } else { 0 }
                | if preserve_blink && cell.flags.blink() {
                    4
                } else {
                    0
                }
                | if cell.flags.printed_space() { 8 } else { 0 }
                | if cell.flags.tab_padding() { 16 } else { 0 };
            buf.push(extra_flags);
            buf.extend_from_slice(&cell.hyperlink_id.as_raw().to_le_bytes());
            buf.push(run);

            i += run as usize;
        }
        buf
    }

    fn decode_cells(data: &[u8], cols: usize, printed_spaces: bool) -> Vec<TerminalCell> {
        let mut cells = Vec::with_capacity(cols);
        let mut pos = 0;
        while let Some((cell, run)) = Self::decode_run(data, &mut pos, printed_spaces) {
            for _ in 0..run {
                cells.push(cell);
            }
        }
        // Pad to cols
        cells.resize(cols, TerminalCell::default());
        cells
    }

    fn decode_run(
        data: &[u8],
        pos: &mut usize,
        printed_spaces: bool,
    ) -> Option<(TerminalCell, usize)> {
        let ch_len = usize::from(*data.get(*pos)?);
        *pos += 1;
        let ch = std::str::from_utf8(data.get(*pos..*pos + ch_len)?)
            .ok()
            .and_then(|s| s.chars().next())
            .unwrap_or(' ');
        *pos += ch_len;

        let foreground = Self::decode_color(data, pos);
        let background = Self::decode_color(data, pos);
        let mut flags = Self::decode_flags(data.get(*pos).copied().unwrap_or(0));
        *pos += 1;
        let extra_flags = data.get(*pos).copied().unwrap_or(0);
        *pos += 1;
        flags.set_wide(extra_flags & 1 != 0);
        flags.set_wide_continuation(extra_flags & 2 != 0);
        flags.set_blink(extra_flags & 4 != 0);
        flags.set_printed_space(printed_spaces && extra_flags & 8 != 0);
        flags.set_tab_padding(printed_spaces && extra_flags & 16 != 0);
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
    /// Match the historical reflow rule exactly.  Foreground-only styling on
    /// a space has never kept that space in a joined logical line; changing
    /// that here would move existing search/selection coordinates.
    #[inline]
    pub fn is_reflow_trimmable_blank(&self) -> bool {
        self.character == ' '
            && self.background == Color::Default
            && !self.flags.wide()
            && !self.flags.wide_continuation()
            && !self.flags.printed_space()
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
                    ScrollbackLine::visit_encoded_cells(
                        &bytes[..length],
                        cols,
                        false,
                        |_, cell| streamed.push(cell),
                    );
                    streamed.resize(cols, TerminalCell::default());
                    assert_eq!(
                        streamed,
                        ScrollbackLine::pinned_decode_cells(&bytes[..length], cols)
                    );
                }
            }
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
                    ScrollbackLine::visit_encoded_cells(&bytes, cols, false, |_, cell| {
                        streamed.push(cell)
                    });
                    streamed.resize(cols, TerminalCell::default());
                    assert_eq!(streamed, ScrollbackLine::pinned_decode_cells(&bytes, cols));
                }
            }
        }
    }
}
