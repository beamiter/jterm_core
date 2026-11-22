use super::{
    underline_color_at, Arc, HistoryCopyRow, HistoryLayout, HistoryPage, Row, RowClusters,
    ScrollbackLine, Terminal, UnderlineColorRun, MAX_ROWS,
};
use crate::{
    history_reflow::{source_columns, ProjectedHistoryRow},
    TerminalCell,
};
use std::collections::VecDeque;

impl Terminal {
    /// Index the retained archive at the current width. The caller owns the
    /// index while reading history, and repeated captures can reuse it.
    pub fn history_layout(&self) -> Result<Arc<HistoryLayout>, String> {
        let extent = self.history_extent();
        let columns = self.screen.grid.cols();
        let mut cache = self
            .retained_layout
            .lock()
            .map_err(|_| "history layout cache poisoned")?;
        if let Some(layout) = cache
            .upgrade()
            .filter(|layout| layout.extent == extent && layout.columns == columns)
        {
            return Ok(layout);
        }
        let layout = Arc::new(HistoryLayout::new(&self.history, extent, columns));
        *cache = Arc::downgrade(&layout);
        Ok(layout)
    }

    /// Materialize only the requested rows. The raw archive and its row
    /// identities are retained for subsequent widths and existing readers.
    pub fn projected_history_page(
        &self,
        layout: &HistoryLayout,
        first: usize,
        limit: usize,
    ) -> Result<HistoryPage, String> {
        if limit == 0 || limit > MAX_ROWS || first > layout.len() {
            return Err("history page out of bounds".into());
        }
        if layout.extent != self.history_extent()
            || layout.columns != self.screen.grid.cols()
            || !std::ptr::eq(self.history_layout()?.as_ref(), layout)
        {
            return Err("history layout is stale".into());
        }
        let end = (first + limit).min(layout.len());
        let offset = layout.len() - end;
        let Some(projected) = &layout.projected else {
            return self.history_page(offset, limit);
        };
        let mut reader = HistoryReader {
            raw: &self.history,
            cached: None,
            first: layout.extent.start,
            single_column: layout.columns == 1,
        };
        let rows = projected[first..end]
            .iter()
            .map(|row| reader.read(*row))
            .collect();
        self.make_history_page(offset, layout.len(), rows)
    }
}

struct HistoryReader<'a> {
    raw: &'a VecDeque<ScrollbackLine>,
    cached: Option<(usize, Vec<TerminalCell>)>,
    first: u64,
    single_column: bool,
}

impl HistoryReader<'_> {
    fn read(&mut self, row: ProjectedHistoryRow) -> HistoryCopyRow {
        let width = usize::from(row.cells);
        let mut cells = Vec::with_capacity(width);
        let mut underline: Vec<UnderlineColorRun> = Vec::new();
        let mut clusters = RowClusters::default();
        let mut index = (row.anchor.row - self.first) as usize;
        let mut column = usize::from(row.anchor.column);
        while cells.len() < width {
            let line = &self.raw[index];
            if self.cached.as_ref().is_none_or(|(row, _)| *row != index) {
                self.cached = Some((index, line.decompress()));
            }
            let source = &self.cached.as_ref().expect("decoded history row").1;
            for (raw_column, &cell) in source
                .iter()
                .enumerate()
                .take(source_columns(line))
                .skip(column)
            {
                let mut cell = cell;
                if self.single_column && cell.flags.wide_continuation() {
                    continue;
                }
                if self.single_column {
                    cell.flags.set_wide(false);
                }
                let column = cells.len();
                if let Some(text) = line
                    .cluster_row()
                    .and_then(|row| row.shared_text(raw_column))
                {
                    clusters.set(column, Arc::clone(text));
                }
                if let Some(color) = underline_color_at(line.underline_color_runs(), raw_column) {
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
                cells: cells.into(),
                wrapped: row.wrapped,
            },
            underline_colors: (!underline.is_empty()).then(|| underline.into()),
            clusters: (!clusters.is_empty()).then(|| Arc::new(clusters)),
            synthetic_gap: None,
        }
    }
}
