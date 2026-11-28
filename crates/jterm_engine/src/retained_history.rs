//! Byte accounting for compressed terminal scrollback.
use std::{
    collections::VecDeque,
    fmt,
    ops::Deref,
    sync::{Arc, OnceLock},
};

pub use crate::retained_bytes::{
    RetainedBytesBudget as HistoryBudget, RetainedBytesUsage as HistoryUsage,
};
use crate::{history_reflow::reflow_cell_rows, ScrollbackLine, TerminalCell};

pub const RETAINED_HISTORY_BYTES: usize = 8 * 1024 * 1024;
pub const PROCESS_RETAINED_HISTORY_BYTES: usize = 16 * RETAINED_HISTORY_BYTES;
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

fn process_budget() -> &'static Arc<HistoryBudget> {
    static PROCESS: OnceLock<Arc<HistoryBudget>> = OnceLock::new();
    PROCESS.get_or_init(|| Arc::new(HistoryBudget::new(PROCESS_RETAINED_HISTORY_BYTES)))
}

pub fn process_history_usage() -> HistoryUsage {
    process_budget().usage()
}

/// Tracks stored entries and their backing allocations. Spare collection
/// capacity and separate renderer, image and document resources have their own
/// lifetimes. The owner chooses when to evict so it can rebase row references.
pub struct RetainedHistory<Row> {
    rows: VecDeque<Row>,
    bytes: usize,
    byte_limit: usize,
    process: Arc<HistoryBudget>,
}

impl<Row> RetainedHistory<Row> {
    pub fn with_byte_limit(byte_limit: usize) -> Self {
        Self::with_process_budget(byte_limit, Arc::clone(process_budget()))
    }

    pub fn with_process_budget(byte_limit: usize, process: Arc<HistoryBudget>) -> Self {
        Self {
            rows: VecDeque::new(),
            bytes: 0,
            byte_limit,
            process,
        }
    }

    /// Empty storage for a reversible or rebuilt history in the same budget.
    pub fn empty_sibling(&self) -> Self {
        Self::with_process_budget(self.byte_limit, Arc::clone(&self.process))
    }

    /// Scale the byte allowance for an explicitly configured scrollback size.
    /// The owner still enforces both limits and retires identities for rows it evicts.
    pub fn scale_byte_limit_for_rows(&mut self, row_limit: usize) {
        self.byte_limit = row_limit
            .saturating_mul(ROW_SCALED_HISTORY_BYTES_PER_ROW)
            .clamp(RETAINED_HISTORY_BYTES, MAX_ROW_SCALED_HISTORY_BYTES);
    }
}

impl<Row> Drop for RetainedHistory<Row> {
    fn drop(&mut self) {
        self.process.release(self.bytes);
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

    /// Append an already prepared row after process admission. Callers with
    /// normal live scrollback should use `retain_latest` for local limits and
    /// reporting evictions. This can also build a reversible stash.
    pub fn push_back(&mut self, row: Row) -> bool {
        self.try_push_back(row).is_ok()
    }

    /// Keep the unadmitted row with the caller so it can expire old anchors
    /// and try again without rebuilding compressed data.
    pub fn try_push_back(&mut self, row: Row) -> Result<(), Row> {
        let size = row.retained_bytes();
        if !self.process.try_charge(size) {
            return Err(row);
        }
        self.bytes += size;
        self.rows.push_back(row);
        Ok(())
    }

    pub fn push_front(&mut self, row: Row) -> bool {
        let size = row.retained_bytes();
        if !self.process.try_charge(size) {
            return false;
        }
        self.bytes += size;
        self.rows.push_front(row);
        true
    }

    /// Keep a contiguous newest tail. Evict this owner's oldest cached rows
    /// before trying again and report each eviction so anchors can expire.
    /// Another live session's history is never mutated from this caller.
    pub fn retain_latest(
        &mut self,
        row: Row,
        row_limit: usize,
        mut on_evict: impl FnMut(Row),
    ) -> bool {
        let size = row.retained_bytes();
        while self.rows.len() >= row_limit
            || size > self.byte_limit.saturating_sub(self.bytes)
            || !self.process.try_charge(size)
        {
            let Some(evicted) = self.pop_front() else {
                return false;
            };
            on_evict(evicted);
        }
        self.bytes += size;
        self.rows.push_back(row);
        true
    }

    pub fn pop_front(&mut self) -> Option<Row> {
        let row = self.rows.pop_front()?;
        let size = row.retained_bytes();
        self.bytes -= size;
        self.process.release(size);
        Some(row)
    }

    pub fn replace(&mut self, index: usize, row: Row) -> Option<Row> {
        let old = self.rows.get_mut(index)?;
        let (before, after) = (old.retained_bytes(), row.retained_bytes());
        if !self.process.try_charge(after.saturating_sub(before)) {
            return None;
        }
        self.process.release(before.saturating_sub(after));
        self.bytes = self.bytes - before + after;
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
        let size = self
            .rows
            .range(..count)
            .map(RetainedRow::retained_bytes)
            .sum::<usize>();
        self.bytes -= size;
        self.process.release(size);
        self.rows.drain(..count)
    }

    pub fn discard_prefix(&mut self, count: usize) {
        drop(self.drain_prefix(count));
    }

    pub fn take_prefix(&mut self, count: usize) -> Vec<Row> {
        self.drain_prefix(count).collect()
    }

    /// Move a prefix to another owner without releasing its charge. Useful
    /// when a clear operation retains rows for a later undo.
    pub fn take_charged_prefix(&mut self, count: usize) -> Self {
        let mut prefix = self.empty_sibling();
        prefix.rows.extend(self.rows.drain(..count));
        prefix.bytes = prefix.rows.iter().map(RetainedRow::retained_bytes).sum();
        self.bytes -= prefix.bytes;
        prefix
    }

    /// Restore already charged rows without allocating another reservation.
    /// The caller can trim against its local row and byte limits afterwards.
    pub fn prepend_charged(&mut self, mut prefix: Self) {
        assert!(Arc::ptr_eq(&self.process, &prefix.process));
        prefix.rows.append(&mut self.rows);
        std::mem::swap(&mut self.rows, &mut prefix.rows);
        self.bytes += std::mem::take(&mut prefix.bytes);
    }

    pub fn truncate(&mut self, length: usize) {
        let end = length.min(self.rows.len());
        let size = self
            .rows
            .range(end..)
            .map(RetainedRow::retained_bytes)
            .sum::<usize>();
        self.rows.truncate(end);
        self.bytes -= size;
        self.process.release(size);
    }

    pub fn clear(&mut self) {
        *self = self.empty_sibling();
    }

    pub(crate) fn shrink_to_fit(&mut self) {
        self.rows.shrink_to_fit();
    }

    pub fn into_rows(mut self) -> VecDeque<Row> {
        std::mem::take(&mut self.rows)
    }
}

impl RetainedHistory<ScrollbackLine> {
    pub(crate) fn retain_latest_line(&mut self, row: ScrollbackLine, row_limit: usize) -> bool {
        if self.rows.len() == row_limit
            && self.bytes <= self.byte_limit
            && self
                .rows
                .front()
                .is_some_and(|oldest| oldest.retained_bytes() == row.retained_bytes())
        {
            // Transfer an equal charge to the new tail without touching the
            // process counter. Both the owner and process totals stay bounded.
            let oldest = self.rows.pop_front();
            self.rows.push_back(row);
            drop(oldest);
            true
        } else {
            self.retain_latest(row, row_limit, drop)
        }
    }

    /// Normalize Frost's physical history after a resize. Source rows are
    /// consumed one at a time and the output retains only the newest tail.
    /// Every emitted row receives a fresh identity, including discarded rows.
    pub fn reflow_frost_legacy(
        mut self,
        columns: usize,
        blank: TerminalCell,
        row_limit: usize,
    ) -> Self {
        let mut result = self.empty_sibling();
        let source = std::iter::from_fn(|| self.pop_front()).map(|line| {
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
            result.retain_latest_line(line, row_limit);
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
    fn shared_limit_expires_the_writing_sessions_prefix_and_preserves_the_peer() {
        let budget = Arc::new(HistoryBudget::new(10));
        let mut active = RetainedHistory::with_process_budget(10, Arc::clone(&budget));
        let mut peer = active.empty_sibling();
        assert!(active.push_back(ChargedRow(4)));
        assert!(active.push_back(ChargedRow(2)));
        assert!(peer.push_back(ChargedRow(4)));
        assert!(!peer.push_back(ChargedRow(1)));

        let mut expired = Vec::new();
        assert!(active.retain_latest(ChargedRow(3), 10, |row| expired.push(row.0)));
        assert_eq!(expired, [4]);
        assert_eq!(active.iter().map(|row| row.0).collect::<Vec<_>>(), [2, 3]);
        assert_eq!(peer[0].0, 4);
        assert_eq!(budget.usage().retained_bytes, 9);
        assert_eq!(budget.usage().peak_retained_bytes, 10);
        drop(peer);
        drop(active);
        assert_eq!(budget.usage().retained_bytes, 0);
    }

    #[test]
    fn replacing_an_equal_charge_keeps_the_newest_tail_at_the_budget_limit() {
        let line = |character| {
            ScrollbackLine::compress(
                &[TerminalCell {
                    character,
                    ..TerminalCell::default()
                }; 80],
                false,
            )
        };
        let size = line('a').retained_bytes();
        let budget = Arc::new(HistoryBudget::new(size * 3));
        let mut history = RetainedHistory::with_process_budget(size * 2, Arc::clone(&budget));
        let mut peer = history.empty_sibling();
        assert!(peer.push_back(line('p')));
        assert!(history.retain_latest_line(line('a'), 2));
        assert!(history.retain_latest_line(line('b'), 2));
        let before = budget.usage();
        assert!(history.retain_latest_line(line('c'), 2));
        assert_eq!(budget.usage(), before);
        assert_eq!(history.retained_bytes(), size * 2);
        assert_eq!(history[0].decompress()[0].character, 'b');
        assert_eq!(history[1].decompress()[0].character, 'c');
        assert_eq!(peer[0].decompress()[0].character, 'p');
        drop(history);
        assert_eq!(budget.usage().retained_bytes, size);
    }

    #[test]
    fn reversible_prefixes_keep_one_charge_and_restore_at_full_capacity() {
        let budget = Arc::new(HistoryBudget::new(12));
        let mut live = RetainedHistory::with_process_budget(12, Arc::clone(&budget));
        for size in [2, 3, 7] {
            assert!(live.push_back(ChargedRow(size)));
        }
        let stash = live.take_charged_prefix(2);
        assert_eq!((live.retained_bytes(), stash.retained_bytes()), (7, 5));
        assert_eq!(budget.usage().retained_bytes, 12);
        assert!(!live.push_back(ChargedRow(1)));
        assert!(live.replace(0, ChargedRow(8)).is_none());
        live.prepend_charged(stash);
        assert_eq!(live.iter().map(|row| row.0).collect::<Vec<_>>(), [2, 3, 7]);
        assert_eq!(budget.usage().retained_bytes, 12);
        live.truncate(2);
        assert_eq!(budget.usage().retained_bytes, 5);
        live.clear();
        assert_eq!(budget.usage().retained_bytes, 0);
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
        let oldest = TerminalCell {
            character: 'b',
            ..blank
        };
        // The middle row needs an encoded payload larger than the byte
        // limit. Alternating backgrounds keep its cell runs distinct.
        let large = std::array::from_fn(|column| TerminalCell {
            character: '界',
            background: if column % 2 == 0 {
                crate::Color::Red
            } else {
                crate::Color::Green
            },
            ..blank
        });
        let row_bytes = ScrollbackLine::compress_frost_legacy(&[small; 80], false).retained_bytes();
        let mut history = RetainedHistory::with_byte_limit(row_bytes * 2);
        for row in [[oldest; 80], large, [small; 80]] {
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
