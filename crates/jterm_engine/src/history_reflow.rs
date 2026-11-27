use crate::{
    frame::Row,
    retained_bytes::{RetainedBytes, RetainedBytesBudget},
    row_annotations::RowAnnotations,
    ScrollbackLine, TerminalCell, UnderlineColorRun,
};
use std::{
    collections::VecDeque,
    ops::Range,
    sync::{Arc, Mutex, OnceLock},
};

const MAX_PROJECTED_ROWS: usize = 65_536;
const PROCESS_HISTORY_VIEWPORT_BYTES: usize = 8 * 1024 * 1024;

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
    pub(crate) viewport: Mutex<HistoryViewportCache>,
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
            viewport: Mutex::default(),
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
            if columns > 1 && start.is_none() && !line.is_wrapped && length <= columns {
                // A complete logical line needs no wrapping or continuation checks.
                push_projected(&mut rows, HistoryAnchor { row, column: 0 }, length, false)?;
                wrapped_tail = false;
                continue;
            }
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

#[derive(Clone)]
pub(crate) struct HistoryCopyRow {
    pub(crate) row: Row,
    pub(crate) underline_colors: Option<Arc<[UnderlineColorRun]>>,
    pub(crate) annotations: Option<Arc<RowAnnotations>>,
    // Physical addressing retains this slot. Only text extraction omits it.
    pub(crate) synthetic_gap: Option<usize>,
}

impl HistoryCopyRow {
    pub(crate) fn from_line(line: &ScrollbackLine) -> Self {
        Self {
            row: Row {
                cells: Arc::from(line.decompress()),
                wrapped: line.is_wrapped,
            },
            underline_colors: (!line.underline_color_runs().is_empty())
                .then(|| Arc::from(line.underline_color_runs())),
            annotations: line.row_annotations().cloned(),
            synthetic_gap: line.synthetic_margin_gap(),
        }
    }
}

pub(crate) fn history_rows_retained_bytes(rows: &[HistoryCopyRow]) -> usize {
    let header = 2 * size_of::<usize>();
    header
        + std::mem::size_of_val(rows)
        + rows
            .iter()
            .map(|row| {
                header
                    + std::mem::size_of_val(row.row.cells.as_ref())
                    + row
                        .underline_colors
                        .as_ref()
                        .map_or(0, |runs| header + std::mem::size_of_val(runs.as_ref()))
                    + row
                        .annotations
                        .as_ref()
                        .map_or(0, |data| data.retained_bytes())
            })
            .sum::<usize>()
}

pub(crate) fn process_history_viewport_budget() -> &'static Arc<RetainedBytesBudget> {
    static BUDGET: OnceLock<Arc<RetainedBytesBudget>> = OnceLock::new();
    BUDGET.get_or_init(|| Arc::new(RetainedBytesBudget::new(PROCESS_HISTORY_VIEWPORT_BYTES)))
}

#[derive(Default)]
pub(crate) struct HistoryViewportCache(Option<CachedViewport>);

struct CachedViewport {
    first: usize,
    rows: Arc<[HistoryCopyRow]>,
    lease: RetainedBytes,
}

impl HistoryViewportCache {
    pub(crate) fn read(
        &mut self,
        range: Range<usize>,
        mut materialize: impl FnMut(usize) -> HistoryCopyRow,
        budget: &Arc<RetainedBytesBudget>,
    ) -> Arc<[HistoryCopyRow]> {
        let first = range.start;
        let cached = self.0.as_ref();
        if let Some(window) =
            cached.filter(|window| window.first == first && window.rows.len() == range.len())
        {
            return Arc::clone(&window.rows);
        }
        let rows: Arc<[HistoryCopyRow]> = range
            .map(|index| {
                let retained = cached.and_then(|window| {
                    index
                        .checked_sub(window.first)
                        .and_then(|row| window.rows.get(row))
                });
                retained.cloned().unwrap_or_else(|| materialize(index))
            })
            .collect();
        if rows.is_empty() {
            return rows;
        }
        let bytes = history_rows_retained_bytes(&rows);
        // A cache miss or a full budget leaves delivery available. The layout
        // owns at most one decoded window and releases it with its lease.
        if let Some(window) = &mut self.0 {
            if window.lease.try_resize(bytes) {
                window.first = first;
                window.rows = Arc::clone(&rows);
            }
        } else if let Some(lease) = budget.reserve(bytes) {
            self.0 = Some(CachedViewport {
                first,
                rows: Arc::clone(&rows),
                lease,
            });
        }
        rows
    }
}

#[cfg(test)]
#[test]
fn viewport_cache_serves_larger_windows_when_the_retention_budget_is_full() {
    fn row(index: usize) -> HistoryCopyRow {
        HistoryCopyRow {
            row: Row {
                cells: Arc::from([TerminalCell {
                    character: char::from(b'a' + index as u8),
                    ..TerminalCell::default()
                }]),
                wrapped: false,
            },
            underline_colors: None,
            annotations: None,
            synthetic_gap: None,
        }
    }

    let capacity = history_rows_retained_bytes(&[row(0)]);
    let budget = Arc::new(RetainedBytesBudget::new(capacity));
    let mut cache = HistoryViewportCache::default();
    let first = cache.read(0..1, row, &budget);
    assert_eq!(budget.usage().retained_bytes, capacity);

    let larger = cache.read(0..2, row, &budget);
    assert!(Arc::ptr_eq(&first[0].row.cells, &larger[0].row.cells));
    assert_eq!(larger[1].row.cells[0].character, 'b');
    assert_eq!(budget.usage().retained_bytes, capacity);
    let first_again = cache.read(0..1, |_| panic!("original cached row"), &budget);
    assert!(Arc::ptr_eq(&first, &first_again));

    let replacement = cache.read(1..2, row, &budget);
    assert!(!Arc::ptr_eq(
        &larger[1].row.cells,
        &replacement[0].row.cells
    ));
    assert_eq!(replacement[0].row.cells[0].character, 'b');
    drop(cache);
    assert_eq!(budget.usage().retained_bytes, 0);
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
