use crate::{ScrollbackLine, TerminalCell};
use std::{collections::VecDeque, ops::Range};

const MAX_PROJECTED_ROWS: usize = 65_536;

/// An original, immutable history row and physical column within it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct HistoryAnchor {
    pub row: u64,
    pub column: u16,
}

#[derive(Clone, Copy)]
pub(crate) struct ProjectedHistoryRow {
    pub anchor: HistoryAnchor,
    pub cells: u16,
    pub wrapped: bool,
}

/// Maps visual history rows to stable source coordinates at one viewport width.
/// Indexing uses compressed row geometry. Cell decoding is deferred to paging.
/// Histories that exceed the projection budget use their original row geometry.
pub struct HistoryLayout {
    pub(crate) extent: Range<u64>,
    pub(crate) columns: usize,
    pub(crate) projected: Option<Vec<ProjectedHistoryRow>>,
}

impl HistoryLayout {
    pub(crate) fn new(
        history: &VecDeque<ScrollbackLine>,
        extent: Range<u64>,
        columns: usize,
    ) -> Self {
        let projected = if extent.is_empty() {
            Some(Vec::new())
        } else {
            debug_assert_eq!(extent.end - extent.start, history.len() as u64);
            Self::project(history, extent.start, columns)
        };
        Self {
            extent,
            columns,
            projected,
        }
    }

    pub fn len(&self) -> usize {
        self.projected
            .as_ref()
            .map_or_else(|| (self.extent.end - self.extent.start) as usize, Vec::len)
    }

    pub fn columns(&self) -> usize {
        self.columns
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn anchor_at(&self, visual_row: usize) -> Option<HistoryAnchor> {
        if visual_row >= self.len() {
            return None;
        }
        Some(self.projected.as_ref().map_or(
            HistoryAnchor {
                row: self.extent.start + visual_row as u64,
                column: 0,
            },
            |rows| rows[visual_row].anchor,
        ))
    }

    /// Find the visual row containing the anchor, clamping an evicted source
    /// coordinate to the oldest retained row. The live boundary is `len()`.
    pub fn row_at(&self, anchor: HistoryAnchor) -> usize {
        if anchor.row >= self.extent.end {
            return self.len();
        }
        self.projected.as_ref().map_or_else(
            || anchor.row.saturating_sub(self.extent.start) as usize,
            |rows| {
                rows.partition_point(|row| row.anchor <= anchor)
                    .saturating_sub(1)
            },
        )
    }

    fn project(
        history: &VecDeque<ScrollbackLine>,
        first: u64,
        columns: usize,
    ) -> Option<Vec<ProjectedHistoryRow>> {
        if history.len() > MAX_PROJECTED_ROWS {
            return None;
        }
        let mut rows = Vec::with_capacity(history.len());
        let mut start = None;
        let mut used = 0;
        let mut wrapped_tail = false;
        for (index, line) in history.iter().enumerate() {
            let row = first + index as u64;
            let length = source_columns(line);
            let continuations = line.reflow_wide_continuations();
            let mut column = 0;
            start.get_or_insert(HistoryAnchor { row, column: 0 });
            while column < length {
                let continuation_at =
                    |column| continuations.binary_search(&(column as u16)).is_ok();
                if columns == 1 && continuation_at(column) {
                    column += 1;
                    continue;
                }
                let mut take = (columns - used).min(length - column);
                if columns > 1 && take > 0 && continuation_at(column + take) {
                    take -= 1;
                }
                if take == 0 {
                    push_projected(&mut rows, start.take()?, used, true)?;
                    used = 0;
                    continue;
                }
                start.get_or_insert(HistoryAnchor {
                    row,
                    column: column as u16,
                });
                used += take;
                column += take;
            }
            if !line.is_wrapped {
                push_projected(&mut rows, start.take()?, used, false)?;
                used = 0;
            }
            wrapped_tail = line.is_wrapped;
        }
        if let Some(anchor) = start {
            push_projected(&mut rows, anchor, used, wrapped_tail)?;
        }
        Some(rows)
    }
}

fn push_projected(
    rows: &mut Vec<ProjectedHistoryRow>,
    anchor: HistoryAnchor,
    cells: usize,
    wrapped: bool,
) -> Option<()> {
    if rows.len() == MAX_PROJECTED_ROWS {
        return None;
    }
    rows.push(ProjectedHistoryRow {
        anchor,
        cells: cells as u16,
        wrapped,
    });
    Some(())
}

pub(crate) fn source_columns(line: &ScrollbackLine) -> usize {
    if line.is_wrapped {
        return line
            .synthetic_margin_gap()
            .unwrap_or_else(|| line.columns());
    }
    let underline_end = line
        .underline_color_runs()
        .last()
        .map_or(0, |run| usize::from(run.end));
    line.reflow_content_len().max(underline_end)
}

/// Emit reflowed physical rows from decompressed history. Soft wraps join
/// adjacent source rows. The caller retains output, assigns row identities,
/// and chooses the compression policy. Scratch holds one source row and one
/// destination row regardless of the length of a wrapped logical line.
pub fn reflow_cell_rows(
    source: impl IntoIterator<Item = (Vec<TerminalCell>, bool)>,
    columns: usize,
    blank: TerminalCell,
    mut emit: impl FnMut(&[TerminalCell], bool),
) {
    assert!(columns > 0);
    let mut row = Vec::with_capacity(columns);
    let mut group_open = false;
    let mut group_has_cells = false;
    for source in source.into_iter().map(Some).chain(std::iter::once(None)) {
        let mut wrapped = false;
        if let Some((cells, continues)) = source {
            group_open = true;
            wrapped = continues;
            let active_len = cells
                .iter()
                .rposition(|cell| !cell.is_reflow_trimmable_blank())
                .map_or(0, |last| last + 1);
            group_has_cells |= active_len != 0;
            for mut cell in cells.into_iter().take(active_len) {
                if columns == 1 && cell.flags.wide_continuation() {
                    continue;
                }
                if row.len() == columns {
                    if columns > 1 && cell.flags.wide_continuation() {
                        // Put the body in the next row with its continuation.
                        let body = row.pop().expect("full reflow row");
                        row.resize(columns, blank);
                        emit(&row, true);
                        row.clear();
                        row.push(body);
                    } else {
                        emit(&row, true);
                        row.clear();
                    }
                }
                if columns == 1 {
                    cell.flags.set_wide(false);
                }
                row.push(cell);
            }
        }
        if group_open && !wrapped {
            if !row.is_empty() || !group_has_cells {
                row.resize(columns, blank);
                emit(&row, false);
                row.clear();
            }
            group_open = false;
            group_has_cells = false;
        }
    }
}
