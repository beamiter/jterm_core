//! Byte accounting for compressed terminal scrollback.
use std::{collections::VecDeque, fmt, ops::Deref};

use crate::{history_reflow::reflow_cell_rows, ScrollbackLine, TerminalCell};

pub const RETAINED_HISTORY_BYTES: usize = 8 * 1024 * 1024;
const MAX_ROW_SCALED_HISTORY_BYTES: usize = 32 * 1024 * 1024;
// Allow for a 120-column ASCII row plus its retained entry.
const ROW_SCALED_HISTORY_BYTES_PER_ROW: usize = 192;

pub trait RetainedRow {
    /// Charge the row and the backing storage it retains. Keep this cost
    /// stable while the row belongs to history.
    fn retained_bytes(&self) -> usize;
}

impl RetainedRow for ScrollbackLine {
    fn retained_bytes(&self) -> usize {
        self.retained_bytes()
    }
}

/// Tracks stored entries and their backing allocations. Spare collection
/// capacity and separate renderer, image and document resources have their own
/// lifetimes. The owner chooses when to evict so it can rebase row references.
pub struct RetainedHistory<Row> {
    rows: VecDeque<Row>,
    bytes: usize,
    byte_limit: usize,
}

impl<Row> RetainedHistory<Row> {
    pub fn with_byte_limit(byte_limit: usize) -> Self {
        Self {
            rows: VecDeque::new(),
            bytes: 0,
            byte_limit,
        }
    }

    /// Scale the byte allowance for an explicitly configured scrollback size.
    /// The owner still enforces both limits and retires identities for rows it evicts.
    pub fn scale_byte_limit_for_rows(&mut self, row_limit: usize) {
        self.byte_limit = row_limit
            .saturating_mul(ROW_SCALED_HISTORY_BYTES_PER_ROW)
            .clamp(RETAINED_HISTORY_BYTES, MAX_ROW_SCALED_HISTORY_BYTES);
    }
}

impl<Row> Default for RetainedHistory<Row> {
    fn default() -> Self {
        Self::with_byte_limit(RETAINED_HISTORY_BYTES)
    }
}

impl<Row: fmt::Debug> fmt::Debug for RetainedHistory<Row> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.rows.fmt(formatter)
    }
}

impl<Row> Deref for RetainedHistory<Row> {
    type Target = VecDeque<Row>;

    fn deref(&self) -> &Self::Target {
        &self.rows
    }
}

impl<'a, Row> IntoIterator for &'a RetainedHistory<Row> {
    type Item = &'a Row;
    type IntoIter = std::collections::vec_deque::Iter<'a, Row>;

    fn into_iter(self) -> Self::IntoIter {
        self.rows.iter()
    }
}

impl<Row: RetainedRow> RetainedHistory<Row> {
    pub fn retained_bytes(&self) -> usize {
        self.bytes
    }

    pub fn over_budget(&self, row_limit: usize) -> bool {
        self.rows.len() > row_limit || self.bytes > self.byte_limit
    }

    pub fn needs_room_for(&self, row: &Row, row_limit: usize) -> bool {
        self.rows.len() >= row_limit
            || self.bytes.saturating_add(row.retained_bytes()) > self.byte_limit
    }

    pub fn push_back(&mut self, row: Row) {
        self.bytes += row.retained_bytes();
        self.rows.push_back(row);
    }

    pub fn push_front(&mut self, row: Row) {
        self.bytes += row.retained_bytes();
        self.rows.push_front(row);
    }

    pub fn pop_front(&mut self) -> Option<Row> {
        let row = self.rows.pop_front()?;
        self.bytes -= row.retained_bytes();
        Some(row)
    }

    pub fn replace(&mut self, index: usize, row: Row) -> Option<Row> {
        let old = self.rows.get_mut(index)?;
        self.bytes = self.bytes - old.retained_bytes() + row.retained_bytes();
        Some(std::mem::replace(old, row))
    }

    pub fn trim_oldest(&mut self, row_limit: usize) -> usize {
        let previous = self.rows.len();
        while self.over_budget(row_limit) {
            self.pop_front();
        }
        previous - self.rows.len()
    }

    fn drain_prefix(&mut self, count: usize) -> impl Iterator<Item = Row> + '_ {
        self.bytes -= self
            .rows
            .range(..count)
            .map(RetainedRow::retained_bytes)
            .sum::<usize>();
        self.rows.drain(..count)
    }

    pub fn discard_prefix(&mut self, count: usize) {
        drop(self.drain_prefix(count));
    }

    pub fn take_prefix(&mut self, count: usize) -> Vec<Row> {
        self.drain_prefix(count).collect()
    }

    pub fn truncate(&mut self, length: usize) {
        let end = length.min(self.rows.len());
        self.bytes -= self
            .rows
            .range(end..)
            .map(RetainedRow::retained_bytes)
            .sum::<usize>();
        self.rows.truncate(end);
    }

    pub fn clear(&mut self) {
        *self = Self::with_byte_limit(self.byte_limit);
    }

    pub(crate) fn shrink_to_fit(&mut self) {
        self.rows.shrink_to_fit();
    }

    pub fn into_rows(self) -> VecDeque<Row> {
        self.rows
    }
}

impl RetainedHistory<ScrollbackLine> {
    /// Normalize Frost's physical history after a resize. Source rows are
    /// consumed one at a time and the output retains only the newest tail.
    /// Every emitted row receives a fresh identity, including discarded rows.
    pub fn reflow_frost_legacy(
        self,
        columns: usize,
        blank: TerminalCell,
        row_limit: usize,
    ) -> Self {
        let mut result = Self::with_byte_limit(self.byte_limit);
        let source = self.rows.into_iter().map(|line| {
            let cells = line.decompress();
            #[cfg(test)]
            FROST_REFLOW_PEAK.with(|peak| {
                let (source, row, retained) = peak.get();
                peak.set((source.max(cells.len()), row, retained));
            });
            (cells, line.is_wrapped)
        });
        reflow_cell_rows(source, columns, blank, |cells, wrapped| {
            let line = ScrollbackLine::compress_frost_legacy(cells, wrapped);
            if row_limit == 0 || line.retained_bytes() > result.byte_limit {
                // An unretainable row ends the previous tail. Later rows
                // can start a new contiguous tail within the same budget.
                if !result.is_empty() {
                    result.clear();
                }
            } else {
                while result.needs_room_for(&line, row_limit) {
                    result.pop_front();
                }
                result.push_back(line);
            }
            #[cfg(test)]
            FROST_REFLOW_PEAK.with(|peak| {
                let (source, row, retained) = peak.get();
                peak.set((source, row.max(cells.len()), retained.max(result.len())));
            });
        });
        result
    }
}

#[cfg(test)]
thread_local! {
    static FROST_REFLOW_PEAK: std::cell::Cell<(usize, usize, usize)> =
        const { std::cell::Cell::new((0, 0, 0)) };
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ChargedRow(usize);

    impl RetainedRow for ChargedRow {
        fn retained_bytes(&self) -> usize {
            self.0
        }
    }

    #[test]
    fn frost_reflow_scratch_and_retained_tail_are_bounded() {
        let blank = TerminalCell::default();
        let printed = TerminalCell {
            character: 'x',
            ..blank
        };
        let make_history = |rows, columns, wrapped| {
            let mut history = RetainedHistory::default();
            for _ in 0..rows {
                history.push_back(ScrollbackLine::compress_frost_legacy(
                    &vec![printed; columns],
                    wrapped,
                ));
            }
            history
        };
        FROST_REFLOW_PEAK.with(|peak| peak.set((0, 0, 0)));
        let history = make_history(64, 64, false).reflow_frost_legacy(1, blank, 64);
        assert_eq!(history.len(), 64);
        assert_eq!(FROST_REFLOW_PEAK.with(|peak| peak.get()), (64, 1, 64));

        FROST_REFLOW_PEAK.with(|peak| peak.set((0, 0, 0)));
        let history = make_history(256, 128, true).reflow_frost_legacy(3, blank, 16);
        assert_eq!(history.len(), 16);
        assert_eq!(FROST_REFLOW_PEAK.with(|peak| peak.get()), (128, 3, 16));
    }

    #[test]
    fn frost_reflow_preserves_the_byte_limit_and_retains_a_contiguous_tail() {
        let blank = TerminalCell::default();
        let small = TerminalCell {
            character: 'a',
            ..blank
        };
        let large = TerminalCell {
            character: '界',
            background: crate::Color::Red,
            ..blank
        };
        let row_bytes = ScrollbackLine::compress_frost_legacy(&[small; 80], false).retained_bytes();
        let mut history = RetainedHistory::with_byte_limit(row_bytes * 2);
        for row in [[small; 80], [large; 80], [small; 80]] {
            history.push_back(ScrollbackLine::compress_frost_legacy(&row, false));
        }
        assert!(history[1].retained_bytes() > row_bytes * 2);
        let history = history.reflow_frost_legacy(80, blank, 8);
        assert_eq!(history.len(), 1);
        assert_eq!(history.retained_bytes(), row_bytes);
        assert_eq!(history[0].decompress(), [small; 80]);
        assert!(!history.over_budget(8));
    }

    #[test]
    fn configured_row_limit_scales_bytes_with_a_floor_and_hard_ceiling() {
        let mut history = RetainedHistory::default();
        let row = ChargedRow(RETAINED_HISTORY_BYTES + 1);
        assert!(history.needs_room_for(&row, 100_000));

        history.scale_byte_limit_for_rows(100_000);
        assert!(!history.needs_room_for(&row, 100_000));
        history.push_back(row);
        assert!(!history.over_budget(100_000));

        history.scale_byte_limit_for_rows(1);
        assert!(history.over_budget(1));
        assert_eq!(history.trim_oldest(1), 1);
        assert!(!history.needs_room_for(&ChargedRow(RETAINED_HISTORY_BYTES), 1));

        history.scale_byte_limit_for_rows(usize::MAX);
        assert!(!history.needs_room_for(&ChargedRow(MAX_ROW_SCALED_HISTORY_BYTES), usize::MAX));
        assert!(history.needs_room_for(&ChargedRow(MAX_ROW_SCALED_HISTORY_BYTES + 1), usize::MAX));
    }
}
