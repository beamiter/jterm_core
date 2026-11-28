use crate::{
    frame::Row,
    hyperlink::{capture_row_hyperlinks, HyperlinkId},
    retained_bytes::{RetainedBytes, RetainedBytesBudget},
    row_annotations::RowAnnotations,
    ScrollbackLine, TerminalCell, UnderlineColorRun,
};
use std::{
    collections::VecDeque,
    ops::Range,
    sync::{Arc, Mutex, OnceLock, Weak},
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

#[derive(Default)]
pub(crate) struct HistoryLayoutCache {
    current: Weak<HistoryLayout>,
    retained: Option<(Arc<HistoryLayout>, RetainedBytes)>,
}

impl HistoryLayoutCache {
    pub(crate) fn read(
        &mut self,
        history: &VecDeque<ScrollbackLine>,
        extent: Range<u64>,
        columns: usize,
        retention: Option<&Arc<RetainedBytesBudget>>,
    ) -> Arc<HistoryLayout> {
        let layout = self
            .current
            .upgrade()
            .filter(|layout| layout.extent == extent && layout.columns == columns)
            .unwrap_or_else(|| {
                self.release_retained();
                let layout = Arc::new(HistoryLayout::new(history, extent, columns));
                self.current = Arc::downgrade(&layout);
                layout
            });
        if self.retained.is_none() && !layout.is_empty() {
            self.retained = retention
                .and_then(|budget| budget.reserve(layout.index_bytes()))
                .map(|lease| (Arc::clone(&layout), lease));
        }
        layout
    }

    pub(crate) fn release_retained(&mut self) {
        self.retained = None;
    }
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

    fn index_bytes(&self) -> usize {
        size_of::<Self>()
            + 2 * size_of::<usize>()
            + self
                .projected
                .as_ref()
                .map_or(0, |rows| rows.capacity() * size_of::<ProjectedHistoryRow>())
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
    pub(crate) hyperlink_ids: Option<Arc<[HyperlinkId]>>,
    // Physical addressing retains this slot. Only text extraction omits it.
    pub(crate) synthetic_gap: Option<usize>,
}

impl HistoryCopyRow {
    pub(crate) fn from_line(line: &ScrollbackLine, decoded: &mut Vec<TerminalCell>) -> Self {
        line.decompress_into(decoded);
        Self {
            row: Row {
                cells: Arc::from(decoded.as_slice()),
                wrapped: line.is_wrapped,
            },
            underline_colors: (!line.underline_color_runs().is_empty())
                .then(|| Arc::from(line.underline_color_runs())),
            annotations: line.row_annotations().cloned(),
            hyperlink_ids: None,
            synthetic_gap: line.synthetic_margin_gap(),
        }
    }

    fn cache_hyperlinks(&mut self, ids: &mut Vec<HyperlinkId>) {
        capture_row_hyperlinks(&self.row.cells, ids);
        self.hyperlink_ids = (!ids.is_empty()).then(|| Arc::from(ids.as_slice()));
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
                    + row
                        .hyperlink_ids
                        .as_deref()
                        .map_or(0, |ids| header + std::mem::size_of_val(ids))
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
    links_cached: bool,
    lease: RetainedBytes,
}

impl HistoryViewportCache {
    pub(crate) fn read(
        &mut self,
        range: Range<usize>,
        mut materialize: impl FnMut(usize) -> HistoryCopyRow,
        collect_links: bool,
        budget: &Arc<RetainedBytesBudget>,
    ) -> (Arc<[HistoryCopyRow]>, bool) {
        let first = range.start;
        let cached = self
            .0
            .as_ref()
            .filter(|window| window.first < range.end && first < window.first + window.rows.len());
        // A page jump needs its link table once. Build row summaries when a
        // later overlapping request can reuse them during scrolling.
        let cache_links = collect_links && cached.is_some();
        let links_ready = cached.is_some_and(|window| window.links_cached);
        if let Some(window) = cached.filter(|window| {
            window.first == first
                && window.rows.len() == range.len()
                && (!cache_links || links_ready)
        }) {
            return (Arc::clone(&window.rows), links_ready);
        }
        let rows: Arc<[HistoryCopyRow]> = if let Some(window) = cached {
            let mut ids = Vec::new();
            range
                .map(|index| {
                    let retained = index
                        .checked_sub(window.first)
                        .and_then(|row| window.rows.get(row));
                    let mut row = retained.cloned().unwrap_or_else(|| materialize(index));
                    if cache_links && (!links_ready || retained.is_none()) {
                        row.cache_hyperlinks(&mut ids);
                    }
                    row
                })
                .collect()
        } else {
            range.map(materialize).collect()
        };
        if rows.is_empty() {
            return (rows, cache_links);
        }
        let bytes = history_rows_retained_bytes(&rows);
        // A cache miss or a full budget leaves delivery available. The layout
        // owns at most one decoded window and releases it with its lease.
        if let Some(window) = &mut self.0 {
            if window.lease.try_resize(bytes) {
                window.first = first;
                window.rows = Arc::clone(&rows);
                window.links_cached = cache_links;
            }
        } else if let Some(lease) = budget.reserve(bytes) {
            self.0 = Some(CachedViewport {
                first,
                rows: Arc::clone(&rows),
                links_cached: cache_links,
                lease,
            });
        }
        (rows, cache_links)
    }
}

#[cfg(test)]
#[test]
fn index_retention_is_bounded_and_a_new_width_releases_its_predecessor() {
    let cells = [TerminalCell {
        character: 'A',
        ..TerminalCell::default()
    }; 8];
    let history = VecDeque::from([ScrollbackLine::compress(&cells, false)]);
    let extent = 0..1;
    let mut cache = HistoryLayoutCache::default();
    let initial = cache.read(&history, extent.clone(), 8, None);
    let bytes = initial.index_bytes();
    let budget = Arc::new(RetainedBytesBudget::new(bytes));
    let retained = cache.read(&history, extent.clone(), 8, Some(&budget));
    assert!(Arc::ptr_eq(&initial, &retained));
    let weak = Arc::downgrade(&retained);
    drop(initial);
    drop(retained);
    assert!(weak.upgrade().is_some());
    assert_eq!(budget.usage().retained_bytes, bytes);

    let narrow = cache.read(&history, extent, 1, Some(&budget));
    assert!(weak.upgrade().is_none());
    assert_eq!(narrow.len(), 8);
    assert!(narrow.index_bytes() > bytes);
    let narrow_weak = Arc::downgrade(&narrow);
    drop(narrow);
    assert!(narrow_weak.upgrade().is_none());
    assert_eq!(budget.usage().retained_bytes, 0);
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
            hyperlink_ids: None,
            synthetic_gap: None,
        }
    }

    let capacity = history_rows_retained_bytes(&[row(0)]);
    let budget = Arc::new(RetainedBytesBudget::new(capacity));
    let mut cache = HistoryViewportCache::default();
    let (first, _) = cache.read(0..1, row, false, &budget);
    assert_eq!(budget.usage().retained_bytes, capacity);

    let (larger, _) = cache.read(0..2, row, false, &budget);
    assert!(Arc::ptr_eq(&first[0].row.cells, &larger[0].row.cells));
    assert_eq!(larger[1].row.cells[0].character, 'b');
    assert_eq!(budget.usage().retained_bytes, capacity);
    let (first_again, _) = cache.read(0..1, |_| panic!("original cached row"), false, &budget);
    assert!(Arc::ptr_eq(&first, &first_again));

    let (replacement, _) = cache.read(1..2, row, false, &budget);
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
