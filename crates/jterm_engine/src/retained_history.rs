//! Byte accounting for compressed terminal scrollback.
use std::{collections::VecDeque, fmt, ops::Deref};

use crate::ScrollbackLine;

pub const RETAINED_HISTORY_BYTES: usize = 8 * 1024 * 1024;

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
