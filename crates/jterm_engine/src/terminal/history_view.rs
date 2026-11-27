use super::{
    append_copy_cell, selection_text, underline_color_at, Arc, HistoryCopyRow, HistoryLayout,
    HistoryPage, Row, RowAnnotations, ScrollbackLine, Terminal, UnderlineColorRun, MAX_ROWS,
};
use crate::{
    grid::ScrollbackContent,
    history_reflow::{
        process_history_viewport_budget, source_columns, HistoryAnchor, ProjectedHistoryRow,
    },
    retained_bytes::RetainedBytesBudget,
    TerminalCell,
};
use std::{collections::VecDeque, io, ops::Range};

fn format_cell_text(
    text: &mut String,
    cells: &[TerminalCell],
    annotations: Option<&RowAnnotations>,
    margin_gap: Option<usize>,
) -> usize {
    text.clear();
    let mut written_space_end = 0;
    for (column, cell) in cells.iter().enumerate() {
        if cell.flags.wide_continuation() || margin_gap == Some(column) {
            continue;
        }
        append_copy_cell(text, cell, annotations, column);
        if cell.flags.printed_space() {
            written_space_end = text.len();
        }
    }
    text.trim_end_matches(' ').len().max(written_space_end)
}

// Keep cell streaming out of the plain-text export path.
#[inline(never)]
fn format_streamed_archive_row<'a>(
    line: &ScrollbackLine,
    text: &mut String,
    annotations: impl Iterator<Item = (usize, &'a str)>,
) -> (usize, usize) {
    text.clear();
    let mut physical_end = 0;
    let mut written_space_end = 0;
    let gap = line.synthetic_margin_gap();
    let mut annotations = annotations.peekable();
    line.visit_search_cells(|column, cell| {
        physical_end = column + 1;
        let cluster = annotations.next_if(|&(at, _)| at == column);
        if cell.flags.wide_continuation() || gap == Some(column) {
            return;
        }
        if let Some((_, cluster)) = cluster {
            text.push_str(cluster);
        } else {
            text.push(cell.character);
        }
        if cell.flags.printed_space() {
            written_space_end = text.len();
        }
    });
    let content_columns = line.columns() - usize::from(gap.is_some());
    // Compression may omit a trailing blank cell with a combining mark.
    for (column, cluster) in annotations.take_while(|&(at, _)| at < content_columns) {
        text.extend(std::iter::repeat_n(' ', column - physical_end));
        text.push_str(cluster);
        physical_end = column + 1;
    }
    let end = text.trim_end_matches(' ').len().max(written_space_end);
    let implicit_padding = content_columns.saturating_sub(physical_end);
    (end, text.len() - end + implicit_padding)
}

fn format_archive_row<'a>(line: &'a ScrollbackLine, text: &'a mut String) -> (&'a str, usize) {
    let annotations = line.row_annotations().filter(|row| row.has_text());
    let (end, trailing) = match (line.content(), annotations) {
        (ScrollbackContent::Plain(text, trailing), None)
            if line.synthetic_margin_gap().is_none() =>
        {
            let trimmed = if line.has_printed_spaces() {
                text
            } else {
                text.trim_end_matches(' ')
            };
            return (trimmed, text.len() - trimmed.len() + usize::from(trailing));
        }
        (_, Some(row)) => format_streamed_archive_row(line, text, row.texts()),
        _ => format_streamed_archive_row(line, text, std::iter::empty()),
    };
    (&text[..end], trailing)
}

impl Terminal {
    /// Distinguish spaces written into the live grid from erase padding. Enable
    /// before feeding text when later reads must prove an empty editor suffix.
    pub fn track_printed_spaces(&mut self) {
        self.vt.screen.grid.track_printed_spaces = true;
        self.vt.screen.alt_grid.track_printed_spaces = true;
    }

    fn primary_cursor_column(&self) -> usize {
        if self.vt.screen.pending_wrap {
            self.vt.screen.grid.cols()
        } else {
            self.vt.screen.cursor_col
        }
    }

    /// Empty primary-grid cells after the cursor, including subsequent rows.
    /// A literal space or tab written by the shell keeps the suffix nonempty.
    pub fn primary_suffix_is_empty(&self) -> Option<bool> {
        let grid = &self.vt.screen.grid;
        if self.alternate_screen_active() || !grid.track_printed_spaces {
            return None;
        }
        let row = self.vt.screen.cursor_row;
        let col = self.primary_cursor_column();
        Some((row..grid.rows()).all(|index| {
            let start = if index == row { col } else { 0 };
            grid[index][start..].iter().all(|cell| {
                cell.character == ' '
                    && !cell.flags.printed_space()
                    && !cell.flags.wide_continuation()
            })
        }))
    }

    /// Absolute primary-screen cursor in the same physical coordinate space
    /// as retained history. A pending wrap puts it after the last column.
    /// Callers should discard live anchors on a reset or geometry change.
    pub fn primary_cursor_anchor(&self) -> Option<HistoryAnchor> {
        if self.alternate_screen_active() {
            return None;
        }
        Some(HistoryAnchor {
            row: self
                .history_end
                .checked_add(self.vt.screen.cursor_row as u64)?,
            column: self.primary_cursor_column() as u16,
        })
    }

    /// Copy an exact half-open range over retained primary rows and the live
    /// grid, using their original cells. Reject tab expansions, whose cells
    /// cannot prove literal command text. At most one grid of rows is copied.
    pub fn primary_text_range(
        &self,
        start: HistoryAnchor,
        end: HistoryAnchor,
    ) -> Result<String, String> {
        let history = self.history_extent();
        let grid = &self.vt.screen.grid;
        let last = history
            .end
            .checked_add(grid.rows() as u64)
            .ok_or("row index exhausted")?;
        if self.alternate_screen_active()
            || start.row < history.start
            || start.row > end.row
            || end.row > last
        {
            return Err("primary text range out of bounds".into());
        }
        let stop = end.row.saturating_add(1).min(last);
        if stop - start.row > MAX_ROWS as u64 {
            return Err("primary text range row limit".into());
        }
        let rows: Vec<HistoryCopyRow> = (start.row..stop)
            .map(|row| {
                if row < history.end {
                    let line = &self.history[(row - history.start) as usize];
                    HistoryCopyRow {
                        row: Row {
                            cells: line.decompress().into(),
                            wrapped: line.is_wrapped,
                        },
                        underline_colors: None,
                        annotations: line.row_annotations().cloned(),
                        hyperlink_ids: None,
                        synthetic_gap: line.synthetic_margin_gap(),
                    }
                } else {
                    let index = (row - history.end) as usize;
                    HistoryCopyRow {
                        row: Row {
                            cells: Arc::from(&grid[index][..]),
                            wrapped: grid.row_wrapped[index],
                        },
                        underline_colors: None,
                        annotations: grid.row_annotations(index).cloned(),
                        hyperlink_ids: None,
                        synthetic_gap: grid.synthetic_margin_gap(index),
                    }
                }
            })
            .collect();
        selection_text(
            &rows,
            (0, usize::from(start.column)),
            ((end.row - start.row) as usize, usize::from(end.column)),
            true,
        )
    }

    /// Export the current screen and its retained text in document order.
    /// The sink can stop the scan on a byte or time limit without buffering
    /// the scrollback. A soft wrap joins rows, including at the archive edge.
    pub fn write_retained_text(&self, output: &mut impl io::Write) -> io::Result<()> {
        const SPACES: &[u8] = &[b' '; 256];
        let grid = &self.vt.screen.grid;
        let mut row_text = String::new();
        let mut padding = 0;
        let mut separate = false;
        let mut finish_row = |text: &str, trailing: usize, wrapped: bool| {
            if separate {
                output.write_all(b"\n")?;
            }
            if !text.is_empty() {
                // Padding at an earlier wrap is internal when this row
                // contains text. Only padding at a hard line end is dropped.
                while padding > 0 {
                    let count = padding.min(SPACES.len());
                    output.write_all(&SPACES[..count])?;
                    padding -= count;
                }
                output.write_all(text.as_bytes())?;
            }
            padding = if wrapped { padding + trailing } else { 0 };
            separate = !wrapped;
            Ok::<(), io::Error>(())
        };
        if !self.alternate_screen_active() {
            for line in self.history.iter() {
                let (text, trailing) = format_archive_row(line, &mut row_text);
                finish_row(text, trailing, line.is_wrapped)?;
            }
        }
        let last_drawn = (self.vt.screen.cursor_row..grid.rows())
            .rfind(|&row| {
                grid[row]
                    .iter()
                    .any(|cell| !cell.is_reflow_trimmable_blank())
                    || grid
                        .row_annotations(row)
                        .is_some_and(|line| line.has_text())
            })
            .unwrap_or(self.vt.screen.cursor_row);
        for row in 0..=last_drawn {
            let end = format_cell_text(
                &mut row_text,
                &grid[row],
                grid.row_annotations(row).map(Arc::as_ref),
                grid.synthetic_margin_gap(row),
            );
            finish_row(
                &row_text[..end],
                row_text.len() - end,
                grid.row_wrapped[row],
            )?;
        }
        Ok(())
    }

    /// Read an archive window without decoding its paint resources. Preserve
    /// the boundary to a following row when search will append a newer prefix.
    pub(crate) fn history_search_text(
        &self,
        rows: Range<usize>,
        following_row: bool,
        max_bytes: usize,
    ) -> Option<(u64, String)> {
        debug_assert!(rows.end <= self.history_rows());
        let row_count = rows.len();
        let mut text = String::new();
        let mut row_text = String::new();
        let mut padding = 0;
        for (index, line) in self.history.range(rows).enumerate() {
            let (row, trailing) = format_archive_row(line, &mut row_text);
            if !row.is_empty() {
                if text.len() + padding + row.len() > max_bytes {
                    return None;
                }
                if padding != 0 {
                    text.extend(std::iter::repeat_n(' ', padding));
                    padding = 0;
                }
                text.push_str(row);
            }
            if line.is_wrapped {
                padding += trailing;
            } else {
                padding = 0;
                if following_row || index + 1 < row_count {
                    if text.len() == max_bytes {
                        return None;
                    }
                    text.push('\n');
                }
            }
        }
        if text.len() + padding > max_bytes {
            return None;
        }
        text.extend(std::iter::repeat_n(' ', padding));
        Some((self.revision, text))
    }

    /// Index the retained archive at the current width. The caller owns the
    /// index while reading history, and repeated captures can reuse it.
    pub fn history_layout(&self) -> Result<Arc<HistoryLayout>, String> {
        self.read_history_layout(None)
    }

    /// Use the process viewport budget to keep the index and its decoded
    /// window available between independent reads for this terminal.
    pub fn history_layout_for_viewport(&self) -> Result<Arc<HistoryLayout>, String> {
        self.read_history_layout(Some(process_history_viewport_budget()))
    }

    fn read_history_layout(
        &self,
        retention: Option<&Arc<RetainedBytesBudget>>,
    ) -> Result<Arc<HistoryLayout>, String> {
        let mut cache = self
            .retained_layout
            .lock()
            .map_err(|_| "history layout cache poisoned")?;
        Ok(cache.read(
            &self.history,
            self.history_extent(),
            self.vt.screen.grid.cols(),
            retention,
        ))
    }

    /// Release the terminal's retained viewport when the reader leaves.
    pub fn release_history_viewport(&self) {
        if let Ok(mut cache) = self.retained_layout.lock() {
            cache.release_retained();
        }
    }

    /// Validate a stable physical cell before resolving it in any history layout.
    pub fn retains_history_anchor(&self, anchor: HistoryAnchor) -> bool {
        let extent = self.history_extent();
        extent.contains(&anchor.row)
            && usize::from(anchor.column)
                < self.history[(anchor.row - extent.start) as usize].columns()
    }

    /// Return the original cell behind one rendered history coordinate. The
    /// layout and source geometry are the same ones used to paint the page.
    pub fn projected_history_source(
        &self,
        layout: &HistoryLayout,
        row: usize,
        column: usize,
    ) -> Option<HistoryAnchor> {
        if !std::ptr::eq(self.history_layout().ok()?.as_ref(), layout) {
            return None;
        }
        let start = layout.anchor_at(row)?;
        let Some(rows) = &layout.projected else {
            let line = &self.history[(start.row - layout.extent.start) as usize];
            return (column < line.columns()).then_some(HistoryAnchor {
                row: start.row,
                column: column as u16,
            });
        };
        if column >= usize::from(rows[row].cells) {
            return None;
        }
        let mut remaining = column;
        let mut raw_column = usize::from(start.column);
        for raw_row in start.row..layout.extent.end {
            let line = &self.history[(raw_row - layout.extent.start) as usize];
            let end = source_columns(line);
            if layout.columns != 1 {
                let span = end.saturating_sub(raw_column);
                if remaining < span {
                    return Some(HistoryAnchor {
                        row: raw_row,
                        column: (raw_column + remaining) as u16,
                    });
                }
                remaining -= span;
            } else {
                for cell in raw_column..end {
                    if line
                        .reflow_wide_continuations()
                        .binary_search(&(cell as u16))
                        .is_ok()
                    {
                        continue;
                    }
                    if remaining == 0 {
                        return Some(HistoryAnchor {
                            row: raw_row,
                            column: cell as u16,
                        });
                    }
                    remaining -= 1;
                }
            }
            raw_column = 0;
            if !line.is_wrapped {
                break;
            }
        }
        None
    }

    /// Materialize only the requested rows. The raw archive and its row
    /// identities are retained for subsequent widths and existing readers.
    pub fn projected_history_page(
        &self,
        layout: &HistoryLayout,
        first: usize,
        limit: usize,
    ) -> Result<HistoryPage, String> {
        self.read_projected_history_page(layout, first, limit, false, None)
            .map(|page| page.expect("unbounded capture returns a page"))
    }

    /// Capture a scrolling viewport, sharing decoded rows with the previous
    /// viewport from the same layout when the cache has room.
    pub fn projected_history_viewport_page(
        &self,
        layout: &HistoryLayout,
        first: usize,
        limit: usize,
    ) -> Result<HistoryPage, String> {
        self.read_projected_history_page(layout, first, limit, true, None)
            .map(|page| page.expect("unbounded capture returns a page"))
    }

    /// Reuse viewport rows while honoring a caller's copy-text byte budget.
    /// An oversized page returns `None` without assembling its remaining text.
    pub fn projected_history_viewport_page_with_text_limit(
        &self,
        layout: &HistoryLayout,
        first: usize,
        limit: usize,
        text_limit: usize,
    ) -> Result<Option<HistoryPage>, String> {
        self.read_projected_history_page(layout, first, limit, true, Some(text_limit))
    }

    fn read_projected_history_page(
        &self,
        layout: &HistoryLayout,
        first: usize,
        limit: usize,
        use_cache: bool,
        text_limit: Option<usize>,
    ) -> Result<Option<HistoryPage>, String> {
        if limit == 0 || limit > MAX_ROWS || first > layout.len() {
            return Err("history page out of bounds".into());
        }
        if layout.extent != self.history_extent()
            || layout.columns != self.vt.screen.grid.cols()
            || !std::ptr::eq(self.history_layout()?.as_ref(), layout)
        {
            return Err("history layout is stale".into());
        }
        let end = (first + limit).min(layout.len());
        let mut reader = HistoryReader::new(&self.history, layout);
        let materialize = |index: usize| match &layout.projected {
            Some(projected) => reader.read(projected[index]),
            None => HistoryCopyRow::from_line(&self.history[index]),
        };
        let cache = use_cache.then(|| layout.viewport.lock().ok()).flatten();
        let (rows, cached_links) = if let Some(mut cache) = cache {
            let collect_links = self
                .hyperlinks
                .as_ref()
                .is_some_and(|(_, table)| !table.is_empty());
            cache.read(
                first..end,
                materialize,
                collect_links,
                process_history_viewport_budget(),
            )
        } else {
            ((first..end).map(materialize).collect(), false)
        };
        self.make_history_page(
            layout.len() - end,
            layout.len(),
            rows,
            cached_links,
            text_limit,
        )
    }
}

fn empty_projected_cells() -> Arc<[TerminalCell]> {
    static EMPTY: std::sync::LazyLock<Arc<[TerminalCell]>> =
        std::sync::LazyLock::new(|| Arc::from([]));
    Arc::clone(&EMPTY)
}

pub(super) struct HistoryReader<'a> {
    raw: &'a VecDeque<ScrollbackLine>,
    cached: Option<usize>,
    decoded: Vec<TerminalCell>,
    projected: Vec<TerminalCell>,
    first: u64,
    single_column: bool,
}

impl<'a> HistoryReader<'a> {
    pub(super) fn new(raw: &'a VecDeque<ScrollbackLine>, layout: &HistoryLayout) -> Self {
        Self {
            raw,
            cached: None,
            decoded: Vec::new(),
            projected: Vec::new(),
            first: layout.extent.start,
            single_column: layout.columns == 1,
        }
    }

    pub(super) fn read(&mut self, row: ProjectedHistoryRow) -> HistoryCopyRow {
        let width = usize::from(row.cells);
        if width == 0 {
            return HistoryCopyRow {
                row: Row {
                    cells: empty_projected_cells(),
                    wrapped: row.wrapped,
                },
                underline_colors: None,
                annotations: None,
                hyperlink_ids: None,
                synthetic_gap: None,
            };
        }
        let cells = &mut self.projected;
        cells.clear();
        cells.reserve(width);
        let mut underline: Vec<UnderlineColorRun> = Vec::new();
        let mut annotations = RowAnnotations::default();
        let mut index = (row.anchor.row - self.first) as usize;
        let mut column = usize::from(row.anchor.column);
        while cells.len() < width {
            let line = &self.raw[index];
            if self.cached != Some(index) {
                line.decompress_into(&mut self.decoded);
                self.cached = Some(index);
            }
            let source = &self.decoded;
            let source_end = source_columns(line).min(source.len());
            let source_start = column.min(source_end);
            let source_annotations = line.row_annotations();
            let source_underline = line.underline_color_runs();
            if !self.single_column {
                let count = (width - cells.len()).min(source_end - source_start);
                let target_start = cells.len();
                let end = source_start + count;
                cells.extend_from_slice(&source[source_start..end]);
                if let Some(source) = source_annotations {
                    annotations.append_columns_from(source, source_start..end, target_start);
                }
                let first =
                    source_underline.partition_point(|run| usize::from(run.end) <= source_start);
                for run in source_underline[first..]
                    .iter()
                    .take_while(|run| usize::from(run.start) < end)
                {
                    let start =
                        target_start + usize::from(run.start).max(source_start) - source_start;
                    let end = target_start + usize::from(run.end).min(end) - source_start;
                    if let Some(previous) = underline.last_mut().filter(|previous| {
                        usize::from(previous.end) == start && previous.color == run.color
                    }) {
                        previous.end = end as u16;
                    } else {
                        underline.push(UnderlineColorRun {
                            start: start as u16,
                            end: end as u16,
                            color: run.color,
                        });
                    }
                }
                column = 0;
                index += 1;
                continue;
            }
            for (raw_column, &cell) in source
                .iter()
                .enumerate()
                .take(source_end)
                .skip(source_start)
            {
                let mut cell = cell;
                if self.single_column && cell.flags.wide_continuation() {
                    continue;
                }
                if self.single_column {
                    cell.flags.set_wide(false);
                }
                let column = cells.len();
                if let Some(source) = source_annotations {
                    if let Some(text) = source.shared_text(raw_column) {
                        annotations.set(column, Arc::clone(text));
                    }
                    if let Some(slice) = source.image_at(raw_column) {
                        annotations.set_image(column, slice.clone());
                    }
                }
                if let Some(color) = underline_color_at(source_underline, raw_column) {
                    let column = column as u16;
                    if let Some(run) = underline
                        .last_mut()
                        .filter(|run| run.end == column && run.color == color)
                    {
                        run.end += 1;
                    } else {
                        underline.push(UnderlineColorRun {
                            start: column,
                            end: column + 1,
                            color,
                        });
                    }
                }
                cells.push(cell);
                if cells.len() == width {
                    break;
                }
            }
            column = 0;
            index += 1;
        }
        HistoryCopyRow {
            row: Row {
                cells: Arc::from(cells.as_slice()),
                wrapped: row.wrapped,
            },
            underline_colors: (!underline.is_empty()).then(|| underline.into()),
            annotations: (!annotations.is_empty()).then(|| Arc::new(annotations)),
            hyperlink_ids: None,
            synthetic_gap: None,
        }
    }
}

#[cfg(test)]
mod underline_projection_tests {
    use super::*;
    use crate::{Color, TerminalCell};

    fn history() -> VecDeque<ScrollbackLine> {
        ["ABCD", "EFGH"]
            .into_iter()
            .enumerate()
            .map(|(index, text)| {
                let cells: Vec<_> = text
                    .chars()
                    .map(|character| TerminalCell {
                        character,
                        ..TerminalCell::default()
                    })
                    .collect();
                let mut line = ScrollbackLine::compress(&cells, index == 0);
                let colors = if index == 0 {
                    vec![UnderlineColorRun {
                        start: 1,
                        end: 4,
                        color: Color::Red,
                    }]
                } else {
                    vec![
                        UnderlineColorRun {
                            start: 0,
                            end: 2,
                            color: Color::Red,
                        },
                        UnderlineColorRun {
                            start: 2,
                            end: 4,
                            color: Color::Blue,
                        },
                    ]
                };
                let mut annotations = RowAnnotations::default();
                if index == 0 {
                    annotations.set(2, Arc::from("C\u{301}"));
                } else {
                    annotations.set(1, Arc::from("F\u{301}"));
                }
                line.attach_decorations(colors, Some(Arc::new(annotations)));
                line
            })
            .collect()
    }

    #[test]
    fn projected_rows_remain_immutable_when_the_reader_reuses_its_cell_buffer() {
        let raw = history();
        let layout = HistoryLayout::new(&raw, 0..2, 3);
        let rows = layout.projected.as_ref().unwrap();
        let mut reader = HistoryReader::new(&raw, &layout);
        let first = reader.read(rows[0]);
        let original = first.row.cells.to_vec();
        let original_underline = first.underline_colors.clone();
        let second = reader.read(rows[1]);
        let third = reader.read(rows[2]);
        let text = |row: &HistoryCopyRow| {
            row.row
                .cells
                .iter()
                .map(|cell| cell.character)
                .collect::<String>()
        };
        assert_eq!(text(&first), "ABC");
        assert_eq!(text(&second), "DEF");
        assert_eq!(text(&third), "GH");
        assert_eq!(first.row.cells.as_ref(), original.as_slice());
        assert_eq!(first.underline_colors, original_underline);
    }

    #[test]
    fn bulk_projection_clips_and_joins_like_scalar_projection() {
        let raw = history();
        let layout = HistoryLayout::new(&raw, 0..2, 5);
        let mut bulk_reader = HistoryReader::new(&raw, &layout);
        let mut scalar_reader = HistoryReader::new(&raw, &layout);
        // With no wide cells, both paths preserve the same physical columns.
        scalar_reader.single_column = true;
        for (anchor, cells) in [
            (HistoryAnchor { row: 0, column: 2 }, 5),
            (HistoryAnchor { row: 1, column: 1 }, 3),
        ] {
            let row = ProjectedHistoryRow {
                anchor,
                cells,
                wrapped: false,
            };
            let actual = bulk_reader.read(row);
            let expected = scalar_reader.read(row);
            assert_eq!(actual.row, expected.row);
            assert_eq!(actual.underline_colors, expected.underline_colors);
            assert_eq!(actual.annotations, expected.annotations);
        }
        let combined = bulk_reader.read(ProjectedHistoryRow {
            anchor: HistoryAnchor { row: 0, column: 2 },
            cells: 5,
            wrapped: false,
        });
        assert_eq!(
            combined.underline_colors.as_deref(),
            Some(
                &[
                    UnderlineColorRun {
                        start: 0,
                        end: 4,
                        color: Color::Red
                    },
                    UnderlineColorRun {
                        start: 4,
                        end: 5,
                        color: Color::Blue
                    },
                ][..]
            )
        );
    }
}

#[cfg(test)]
#[test]
fn empty_projected_history_rows_share_cells_without_losing_copy_boundaries() {
    let mut terminal = Terminal::new(3, 8).unwrap();
    terminal.feed(b"\n\n\n\n\n\n").unwrap();
    let layout = terminal.history_layout().unwrap();
    assert_eq!(layout.len(), 4);
    let first = terminal.projected_history_page(&layout, 0, 4).unwrap();
    let second = terminal.projected_history_page(&layout, 1, 3).unwrap();
    assert_eq!(first.text, "\n\n\n");
    assert_eq!(second.text, "\n\n");
    assert_eq!(
        first.copy_offsets_utf16().unwrap(),
        [vec![0], vec![1], vec![2], vec![3]]
    );
    let first_cells = &first.row(0).unwrap().cells;
    for row in 0..first.row_count {
        assert!(Arc::ptr_eq(first_cells, &first.row(row).unwrap().cells));
        assert!(!first.row(row).unwrap().wrapped);
    }
    for row in 0..second.row_count {
        assert!(Arc::ptr_eq(first_cells, &second.row(row).unwrap().cells));
    }
}
