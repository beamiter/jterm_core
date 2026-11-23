use super::{history_view::HistoryReader, HistoryLayout, RowSources, ScrollbackLine, Terminal};
use crate::{
    graphemes,
    history_reflow::{source_columns, HistoryAnchor},
    TerminalCell, TerminalGrid,
};
use std::collections::VecDeque;
use unicode_width::UnicodeWidthChar;

#[derive(Clone, Copy)]
struct Cursor {
    row: usize,
    column: usize,
    pending_wrap: bool,
    autowrap: bool,
}

impl Cursor {
    fn offset(self, columns: usize) -> usize {
        if self.pending_wrap {
            columns
        } else {
            self.column.min(columns - 1)
        }
    }

    fn project(self, raw: &VecDeque<ScrollbackLine>, layout: &HistoryLayout) -> (usize, usize) {
        let Some(line) = raw.get(self.row) else {
            return (layout.len() + self.row - raw.len(), self.column);
        };
        let mut column = self.offset(line.columns()).min(source_columns(line)) as u16;
        if layout.columns() == 1 && line.reflow_wide_continuations().contains(&column) {
            column -= 1;
        }
        let point = HistoryAnchor {
            row: self.row as u64,
            column,
        };
        let row = layout.row_at(point);
        let start = layout.anchor_at(row).expect("primary projection has a row");
        let mut visible_column = 0;
        for index in start.row..=point.row {
            let line = &raw[index as usize];
            let from = if index == start.row { start.column } else { 0 };
            let to = if index == point.row {
                point.column
            } else {
                source_columns(line) as u16
            };
            visible_column += usize::from(to.saturating_sub(from));
            if layout.columns() == 1 {
                visible_column -= line
                    .reflow_wide_continuations()
                    .iter()
                    .filter(|&&column| (from..to).contains(&column))
                    .count();
            }
        }
        (row, visible_column)
    }

    fn in_viewport(self, grid: &TerminalGrid, place: (usize, usize), archived: usize) -> Self {
        let (absolute_row, column) = place;
        let row = absolute_row.saturating_sub(archived).min(grid.rows() - 1);
        let columns = grid.cols();
        let pending_wrap = absolute_row >= archived
            && absolute_row - archived < grid.rows()
            && column == columns
            && self.autowrap;
        let column = if absolute_row < archived {
            0
        } else if pending_wrap && grid[row][columns - 1].flags.wide_continuation() {
            columns - 2
        } else {
            column.min(columns - 1)
        };
        Self {
            row,
            column,
            pending_wrap,
            ..self
        }
    }
}

impl Terminal {
    /// Project the primary scroll buffer with the same geometry used to read
    /// its archive. The alternate screen and restricted scrolling regions
    /// address physical cells and use their coordinate-preserving resize.
    pub(super) fn reflow_primary(&mut self, rows: usize, columns: usize) -> bool {
        let screen = &self.screen;
        let hidden = screen.use_alt_buffer;
        let primary = if hidden {
            &screen.alt_grid
        } else {
            &screen.grid
        };
        if columns == primary.cols()
            || (!hidden
                && (screen.origin_mode
                    || screen.scroll_region_top != 0
                    || screen.scroll_region_bottom + 1 != primary.rows()))
        {
            return false;
        }

        let current = if hidden {
            screen.saved_primary_screen_state.as_ref().map_or(
                Cursor {
                    row: screen.alt_cursor_row,
                    column: screen.alt_cursor_col,
                    pending_wrap: false,
                    autowrap: false,
                },
                |saved| Cursor {
                    row: saved.row,
                    column: saved.col,
                    pending_wrap: saved.pending_wrap,
                    autowrap: saved.autowrap,
                },
            )
        } else {
            Cursor {
                row: screen.cursor_row,
                column: screen.cursor_col,
                pending_wrap: screen.pending_wrap,
                autowrap: self.protocol.modes().contains(&7),
            }
        };
        let saved = screen
            .saved_state
            .as_ref()
            .filter(|_| !hidden)
            .map(|saved| Cursor {
                row: saved.row,
                column: saved.col,
                pending_wrap: saved.pending_wrap,
                autowrap: saved.autowrap,
            });
        let track_printed_spaces = primary.track_printed_spaces;
        let raw = primary_rows(primary, current, saved, columns);
        let layout = HistoryLayout::new(&raw, 0..raw.len() as u64, columns);
        let projected = layout
            .projected
            .as_ref()
            .expect("bounded primary projection");
        let archived = projected.len().saturating_sub(rows);
        let current_place = current.project(&raw, &layout);
        let saved_place = saved.map(|saved| (saved, saved.project(&raw, &layout)));
        let mut reader = HistoryReader::new(&raw, &layout);
        let mut grid = TerminalGrid::new(rows, columns);
        grid.begin_capture_revision(self.revision);
        grid.track_printed_spaces = track_printed_spaces;

        // Earlier visual rows become history before we install the suffix.
        // Keep the prior history's coordinates even when new live rows leave
        // the viewport. A cursor that scrolled out is clamped to the top.
        let mut padded = vec![TerminalCell::default(); columns];
        for (index, &projected_row) in projected.iter().enumerate() {
            if index < archived && self.history_row_limit == 0 {
                continue;
            }
            let wide_gap = projected_row.wrapped
                && usize::from(projected_row.cells) + 1 == columns
                && index + 1 < projected.len();
            let rendered = reader.read(projected_row);
            let underline = rendered.underline_colors.as_deref().unwrap_or_default();
            if index < archived {
                padded[..rendered.row.cells.len()].copy_from_slice(&rendered.row.cells);
                padded[rendered.row.cells.len()..].fill(TerminalCell::default());
                let mut line =
                    ScrollbackLine::compress_preserving_blink(&padded, rendered.row.wrapped);
                line.set_synthetic_margin_gap(wide_gap);
                line.attach_decorations(underline.to_vec(), rendered.annotations);
                line.set_raw_row_id(RowSources::allocate(&mut self.row_sources));
                self.append_history(line);
            } else {
                grid.install_projected_row(
                    index - archived,
                    &rendered.row.cells,
                    rendered.row.wrapped,
                    wide_gap,
                    underline,
                    rendered.annotations,
                );
            }
        }
        for id in grid.row_ids_mut() {
            *id = RowSources::allocate(&mut self.row_sources);
        }
        let current = current.in_viewport(&grid, current_place, archived);
        let saved = saved_place.map(|(saved, place)| saved.in_viewport(&grid, place, archived));

        // Reuse the established margins, tabs and alternate-grid resize.
        self.screen.resize_geometry(rows, columns, 0, || {
            RowSources::allocate(&mut self.row_sources)
        });
        if hidden {
            self.screen.alt_grid = grid;
            self.screen.alt_cursor_row = current.row;
            self.screen.alt_cursor_col = current.column;
            if let Some(saved) = &mut self.screen.saved_primary_screen_state {
                self.screen.saved_cursor_row = current.row;
                self.screen.saved_cursor_col = current.column;
                saved.row = current.row;
                saved.col = current.column;
                saved.pending_wrap = current.pending_wrap;
            }
        } else {
            self.screen.grid = grid;
            self.screen.cursor_row = current.row;
            self.screen.cursor_col = current.column;
            self.screen.pending_wrap = current.pending_wrap;
            if let (Some(state), Some(saved)) = (&mut self.screen.saved_state, saved) {
                state.row = saved.row;
                state.col = saved.column;
                state.pending_wrap = saved.pending_wrap;
            }
        }
        true
    }
}

fn primary_rows(
    grid: &TerminalGrid,
    current: Cursor,
    saved: Option<Cursor>,
    columns: usize,
) -> VecDeque<ScrollbackLine> {
    let last = (current.row.min(grid.rows() - 1)..grid.rows())
        .rev()
        .find(|&row| {
            row == current.row
                || grid[row]
                    .iter()
                    .any(|cell| !cell.is_reflow_trimmable_blank())
                || grid.row_annotations(row).is_some()
                || grid.underline_color_runs(row).next().is_some()
        })
        .unwrap_or(grid.rows() - 1);
    (0..=last)
        .map(|row| {
            let minimum = [Some(current), saved]
                .into_iter()
                .flatten()
                .filter(|cursor| cursor.row == row)
                .map(|cursor| cursor.offset(grid.cols()))
                .max()
                .unwrap_or(0);
            let restored = restore_wide_glyph(grid, row, columns);
            let cells = restored.as_ref().map_or(&grid[row], |pair| &pair[..]);
            let mut underline: Vec<_> = grid.underline_color_runs(row).collect();
            let minimum = if restored.is_some() && minimum == 1 {
                2
            } else {
                minimum
            };
            if restored.is_some() {
                for run in &mut underline {
                    run.end = 2;
                }
            }
            let mut line = ScrollbackLine::compress_preserving_blink(cells, grid.row_wrapped[row]);
            line.set_synthetic_margin_gap(grid.synthetic_margin_gap(row).is_some());
            line.attach_decorations(underline, grid.row_annotations(row).cloned());
            line.include_reflow_columns(minimum);
            line
        })
        .collect()
}

// At width one the glyph is painted in one slot. Restore its natural pair
// when there is room, using the same Unicode presentation rules as printing.
fn restore_wide_glyph(
    grid: &TerminalGrid,
    row: usize,
    columns: usize,
) -> Option<[TerminalCell; 2]> {
    if grid.cols() != 1 || columns == 1 {
        return None;
    }
    let mut lead = grid[row][0];
    let cluster = grid.cluster_text(row, 0);
    let wide = if lead.character.width() == Some(2) {
        !cluster.is_some_and(graphemes::has_narrow_text_presentation)
    } else {
        cluster.is_some_and(graphemes::has_wide_emoji_presentation)
    };
    if !wide {
        return None;
    }
    lead.flags.set_wide(true);
    let mut continuation = TerminalCell {
        background: lead.background,
        hyperlink_id: lead.hyperlink_id,
        ..TerminalCell::default()
    };
    continuation.flags.set_wide_continuation(true);
    continuation.flags.set_protected(lead.flags.protected());
    Some([lead, continuation])
}
