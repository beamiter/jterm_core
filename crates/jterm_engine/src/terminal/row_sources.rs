use super::{RawRowId, Terminal};
use crate::history_reflow::HistoryAnchor;
use std::collections::{HashMap, VecDeque};

/// A column on one physical primary row. The row can move from the live
/// screen into retained history. Its identity is never recycled.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TranscriptAnchor {
    pub row: RawRowId,
    pub column: u16,
}

#[derive(Default)]
pub(super) struct RowSources {
    next: u64,
    archived: HashMap<RawRowId, u64>,
    order: VecDeque<RawRowId>,
}

impl RowSources {
    pub(super) fn allocate(sources: &mut Option<Self>) -> RawRowId {
        let Some(sources) = sources else {
            return RawRowId::UNTRACKED;
        };
        let Some(next) = sources.next.checked_add(1) else {
            return RawRowId::UNTRACKED;
        };
        sources.next = next;
        RawRowId::new(next)
    }

    pub(super) fn appended(&mut self, row: RawRowId, position: u64) {
        if row.is_tracked() {
            self.archived.insert(row, position);
        }
        self.order.push_back(row);
    }

    pub(super) fn trim_to(&mut self, retained: usize) {
        let evicted = self.order.len().saturating_sub(retained);
        for row in self.order.drain(..evicted) {
            self.archived.remove(&row);
        }
    }
}

impl Terminal {
    /// Start indexing live primary rows for a continuous transcript.
    /// History that predates attachment has no transcript source identities.
    pub fn track_transcript_rows(&mut self) {
        if self.row_sources.is_none() {
            self.row_sources = Some(RowSources {
                order: std::iter::repeat_n(RawRowId::UNTRACKED, self.history.len()).collect(),
                ..RowSources::default()
            });
            self.renew_live_source_rows();
        }
    }

    /// The primary cursor, including the exclusive column on pending wrap.
    pub fn transcript_cursor(&self) -> Option<TranscriptAnchor> {
        self.transcript_source_at(self.primary_cursor_anchor()?)
    }

    /// Find the source identity at a retained physical coordinate.
    pub fn transcript_source_at(&self, point: HistoryAnchor) -> Option<TranscriptAnchor> {
        if self.row_sources.is_none() || self.alternate_screen_active() {
            return None;
        }
        let history = self.history_extent();
        if point.row < history.start {
            return None;
        }
        let (row, columns) = if point.row < history.end {
            let line = &self.history[(point.row - history.start) as usize];
            (line.raw_row_id(), line.columns())
        } else {
            let index: usize = (point.row - history.end).try_into().ok()?;
            (self.screen.grid.row_id(index), self.screen.grid.cols())
        };
        (row.is_tracked() && usize::from(point.column) <= columns).then_some(TranscriptAnchor {
            row,
            column: point.column,
        })
    }

    /// Resolve exactly in the current primary screen or archive. Removed rows
    /// and invalid columns fail without moving the reader to a nearby row.
    pub fn locate_transcript_source(&self, anchor: TranscriptAnchor) -> Option<HistoryAnchor> {
        let sources = self.row_sources.as_ref()?;
        if self.alternate_screen_active() || !anchor.row.is_tracked() {
            return None;
        }
        let row = if let Some(&position) = sources.archived.get(&anchor.row) {
            position
        } else {
            let index = self
                .screen
                .grid
                .row_ids()
                .iter()
                .position(|&source| source == anchor.row)?;
            self.history_end.checked_add(index as u64)?
        };
        let point = HistoryAnchor {
            row,
            column: anchor.column,
        };
        (self.transcript_source_at(point) == Some(anchor)).then_some(point)
    }

    pub(super) fn renew_live_source_rows(&mut self) {
        if self.row_sources.is_some() {
            let sources = &mut self.row_sources;
            for id in self
                .screen
                .grid
                .row_ids_mut()
                .iter_mut()
                .chain(self.screen.alt_grid.row_ids_mut())
            {
                *id = RowSources::allocate(sources);
            }
        }
    }

    pub(super) fn invalidate_clipped_sources(&mut self, columns: usize, transfer: usize) {
        let sources = &mut self.row_sources;
        if sources.is_none() || columns >= self.screen.grid.cols() {
            return;
        }
        for (grid, start) in [
            (&mut self.screen.grid, transfer),
            (&mut self.screen.alt_grid, 0),
        ] {
            for row in start..grid.rows() {
                let clipped = grid[row][columns..]
                    .iter()
                    .any(|cell| !cell.is_reflow_trimmable_blank())
                    || grid
                        .row_annotations(row)
                        .is_some_and(|clusters| clusters.end() > columns)
                    || grid
                        .underline_color_runs(row)
                        .any(|run| usize::from(run.end) > columns);
                // An unchanged source row keeps its address when shrinking
                // only erases padding. Full outgoing rows archive at their
                // original width and keep their identities too.
                if clipped {
                    grid.row_ids_mut()[row] = RowSources::allocate(sources);
                }
            }
        }
    }

    pub(super) fn renew_active_source_rows(&mut self) {
        if self.row_sources.is_some() {
            for id in self.screen.grid.row_ids_mut() {
                *id = RowSources::allocate(&mut self.row_sources);
            }
        }
    }

    pub(super) fn clear_transcript_archive(&mut self) {
        if let Some(sources) = &mut self.row_sources {
            sources.trim_to(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_source_follows_scroll_and_never_rebinds_after_eviction() {
        let mut terminal = Terminal::new(3, 8).unwrap();
        terminal.track_transcript_rows();
        terminal.feed(b"first").unwrap();
        let first = terminal.transcript_cursor().unwrap();
        assert_eq!(first.column, 5);
        terminal.feed(b"\r\nsecond\r\nthird\r\nlast").unwrap();
        let point = terminal.locate_transcript_source(first).unwrap();
        assert!(point.row < terminal.history_extent().end);
        assert_eq!(terminal.transcript_source_at(point), Some(first));

        let live = terminal.transcript_cursor().unwrap();
        terminal.feed(b"\x1b[?1049happ").unwrap();
        assert!(terminal.transcript_cursor().is_none());
        assert!(terminal.locate_transcript_source(first).is_none());
        terminal.feed(b"\x1b[?1049l").unwrap();
        assert_eq!(terminal.transcript_cursor(), Some(live));
        terminal.set_history_row_limit(0);
        assert!(terminal.locate_transcript_source(first).is_none());
        assert_eq!(terminal.transcript_cursor(), Some(live));
        terminal.feed(b"\x1b[2J").unwrap();
        assert!(terminal.locate_transcript_source(live).is_none());
    }

    #[test]
    fn shell_prompt_redraw_preserves_rows_that_were_erased_in_place() {
        let mut terminal = Terminal::new(4, 24).unwrap();
        terminal.track_transcript_rows();
        let prompt = terminal.transcript_cursor().unwrap();
        terminal.feed(b"context\r\nprompt> ").unwrap();
        let input = terminal.transcript_cursor().unwrap();

        terminal
            .feed(b"\x1b[1G\x1b[1A\x1b[Jcontext\r\nprompt> command")
            .unwrap();

        let start = terminal.locate_transcript_source(input).unwrap();
        let end = terminal.primary_cursor_anchor().unwrap();
        assert_eq!(terminal.primary_text_range(start, end).unwrap(), "command");
        assert_eq!(terminal.locate_transcript_source(prompt).unwrap().row, 0);
        terminal.feed(b"\x1b[2J").unwrap();
        assert!(terminal.locate_transcript_source(prompt).is_none());
    }

    #[test]
    fn height_resize_archives_departing_sources_and_preserves_survivors() {
        let mut terminal = Terminal::new(4, 8).unwrap();
        terminal.enable_history_preserving_resize();
        terminal.track_transcript_rows();
        terminal.feed(b"first").unwrap();
        let first = terminal.transcript_cursor().unwrap();
        terminal.feed(b"\r\nsecond\r\nthird\r\nfourth").unwrap();
        let live = terminal.transcript_cursor().unwrap();
        terminal.resize(2, 8).unwrap();

        let moved = terminal.locate_transcript_source(first).unwrap();
        assert!(terminal.history_extent().contains(&moved.row));
        assert_eq!(terminal.transcript_cursor(), Some(live));
        terminal.resize(5, 8).unwrap();
        assert_eq!(terminal.locate_transcript_source(first), Some(moved));
        assert_eq!(terminal.transcript_cursor(), Some(live));
        terminal.feed(b"\x1b[3J").unwrap();
        assert!(terminal.locate_transcript_source(first).is_none());
        assert_eq!(terminal.transcript_cursor(), Some(live));
    }

    #[test]
    fn width_resize_keeps_unclipped_rows_and_retires_clipped_ones() {
        let mut terminal = Terminal::new(3, 16).unwrap();
        terminal.track_transcript_rows();
        terminal.feed(b"short").unwrap();
        let safe = terminal.transcript_cursor().unwrap();
        terminal.feed(b"\r\nprinted trail").unwrap();
        let lost = terminal.transcript_cursor().unwrap();
        terminal.resize(3, 8).unwrap();
        assert_eq!(terminal.locate_transcript_source(safe).unwrap().column, 5);
        assert!(terminal.locate_transcript_source(lost).is_none());
        terminal.resize(3, 16).unwrap();
        assert!(terminal.locate_transcript_source(lost).is_none());
        assert!(terminal.locate_transcript_source(safe).is_some());
    }

    #[test]
    fn archive_reflow_hits_original_cells_after_live_width_changes() {
        let mut terminal = Terminal::new(2, 4).unwrap();
        terminal.track_transcript_rows();
        let first = terminal.transcript_cursor().unwrap();
        terminal
            .feed("a界bcdef\r\nmore\r\nend!".as_bytes())
            .unwrap();
        let live = terminal.transcript_cursor().unwrap();
        terminal.resize(2, 3).unwrap();

        assert!(terminal.locate_transcript_source(live).is_none());
        let point = terminal.locate_transcript_source(first).unwrap();
        let layout = terminal.history_layout().unwrap();
        assert_eq!(
            terminal.projected_history_source(&layout, 0, 0),
            Some(point)
        );
        assert_eq!(
            terminal.projected_history_source(&layout, 1, 0),
            Some(HistoryAnchor {
                row: point.row,
                column: 3
            })
        );
        assert_eq!(
            terminal.projected_history_source(&layout, 1, 1),
            Some(HistoryAnchor {
                row: point.row + 1,
                column: 0
            })
        );
        terminal.resize(2, 1).unwrap();
        let layout = terminal.history_layout().unwrap();
        assert_eq!(
            terminal.projected_history_source(&layout, 2, 0),
            Some(HistoryAnchor {
                row: point.row,
                column: 3
            })
        );

        terminal.feed(b"\x1bc").unwrap();
        assert!(terminal.locate_transcript_source(first).is_none());
        assert_ne!(terminal.transcript_cursor().unwrap().row, first.row);
    }
}
