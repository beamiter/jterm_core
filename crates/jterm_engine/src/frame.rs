//! Immutable raw-cell paint values. No projection, transport or resource ownership.
use crate::grid::{CursorShape, TerminalCell};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    Primary,
    Alternate,
}
/// Epoch is supplied by the owning publisher, never a pointer or grid dirty version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    pub epoch: u64,
    pub revision: u64,
    pub layout: u64,
    pub screen: Screen,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cursor {
    pub row: usize,
    pub col: usize,
    pub pending_wrap: bool,
    pub visible: bool,
    pub shape: CursorShape,
}
impl Cursor {
    /// Paint stays in the physical cell even when a protocol report adds a column.
    pub fn paint_position(self) -> (usize, usize) {
        (self.row, self.col)
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub cells: Arc<[TerminalCell]>,
    pub wrapped: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CellFrame {
    stamp: Stamp,
    cols: usize,
    rows: Arc<[Row]>,
    cursor: Cursor,
}
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_rows: usize,
    pub max_cols: usize,
    pub max_cells: usize,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Bounds,
    Cursor,
    Revision,
    Base,
    Layout,
    Duplicate,
}
#[derive(Clone, Debug)]
pub struct Delta {
    pub base: Stamp,
    pub next: Stamp,
    pub cursor: Cursor,
    pub rows: Vec<(usize, Row)>,
}
impl CellFrame {
    pub fn new(
        stamp: Stamp,
        cols: usize,
        rows: Arc<[Row]>,
        cursor: Cursor,
        limits: Limits,
    ) -> Result<Self, Error> {
        if stamp.epoch == 0 || stamp.revision == 0 || stamp.layout == 0 {
            return Err(Error::Revision);
        }
        if cols == 0
            || rows.is_empty()
            || cols > limits.max_cols
            || rows.len() > limits.max_rows
            || rows
                .len()
                .checked_mul(cols)
                .is_none_or(|n| n > limits.max_cells)
            || rows.iter().any(|r| r.cells.len() != cols)
        {
            return Err(Error::Bounds);
        }
        if cursor.row >= rows.len() || cursor.col >= cols {
            return Err(Error::Cursor);
        }
        Ok(Self {
            stamp,
            cols,
            rows,
            cursor,
        })
    }
    pub fn stamp(&self) -> Stamp {
        self.stamp
    }
    pub fn rows(&self) -> &[Row] {
        &self.rows
    }
    pub fn shared_rows(&self) -> Arc<[Row]> {
        Arc::clone(&self.rows)
    }
    pub fn cols(&self) -> usize {
        self.cols
    }
    pub fn cursor(&self) -> Cursor {
        self.cursor
    }
    /// Fresh publication in the same stream. Screen/layout changes require a full frame.
    pub fn delta_to(&self, next: &Self) -> Result<Delta, Error> {
        self.check_next(next.stamp)?;
        if self.cols != next.cols || self.rows.len() != next.rows.len() {
            return Err(Error::Layout);
        }
        Ok(Delta {
            base: self.stamp,
            next: next.stamp,
            cursor: next.cursor,
            rows: self
                .rows
                .iter()
                .zip(next.rows.iter())
                .enumerate()
                .filter(|(_, (a, b))| a != b)
                .map(|(i, (_, r))| (i, r.clone()))
                .collect(),
        })
    }
    fn check_next(&self, next: Stamp) -> Result<(), Error> {
        if next.epoch != self.stamp.epoch
            || next.layout != self.stamp.layout
            || next.screen != self.stamp.screen
        {
            return Err(Error::Layout);
        }
        if next.revision <= self.stamp.revision {
            return Err(Error::Revision);
        }
        Ok(())
    }
    /// Budget checks precede scratch/copy allocation; rejection leaves the original intact.
    pub fn apply(&self, delta: &Delta, limits: Limits) -> Result<Self, Error> {
        if delta.base != self.stamp {
            return Err(Error::Base);
        }
        self.check_next(delta.next)?;
        // The base may have been admitted under a larger budget. Check the
        // current budget before allocating either the seen table or copied rows.
        if self.cols > limits.max_cols
            || self.rows.len() > limits.max_rows
            || self
                .rows
                .len()
                .checked_mul(self.cols)
                .is_none_or(|n| n > limits.max_cells)
            || delta.rows.len() > self.rows.len()
        {
            return Err(Error::Bounds);
        }
        let mut seen = vec![false; self.rows.len()];
        for (index, row) in &delta.rows {
            if *index >= self.rows.len() || row.cells.len() != self.cols {
                return Err(Error::Bounds);
            }
            if seen[*index] {
                return Err(Error::Duplicate);
            }
            seen[*index] = true;
        }
        if delta.cursor.row >= self.rows.len() || delta.cursor.col >= self.cols {
            return Err(Error::Cursor);
        }
        let mut rows = self.rows.to_vec();
        for (index, row) in &delta.rows {
            rows[*index] = row.clone();
        }
        Self::new(delta.next, self.cols, rows.into(), delta.cursor, limits)
    }
}
/// Receiver does not invent epochs: reconnect must explicitly bind an expected epoch.
pub struct Receiver {
    epoch: u64,
    frame: Option<CellFrame>,
}
impl Receiver {
    pub fn new(epoch: u64) -> Self {
        Self { epoch, frame: None }
    }
    pub fn frame(&self) -> Option<&CellFrame> {
        self.frame.as_ref()
    }
    pub fn full(&mut self, frame: CellFrame) -> Result<(), Error> {
        if frame.stamp.epoch != self.epoch
            || self
                .frame
                .as_ref()
                .is_some_and(|old| frame.stamp.revision <= old.stamp.revision)
        {
            return Err(Error::Revision);
        }
        if self.frame.as_ref().is_some_and(|old| {
            frame.stamp.layout < old.stamp.layout
                || (frame.stamp.layout == old.stamp.layout
                    && (frame.cols != old.cols || frame.rows.len() != old.rows.len()))
        }) {
            return Err(Error::Layout);
        }
        self.frame = Some(frame);
        Ok(())
    }
    pub fn delta(&mut self, delta: &Delta, limits: Limits) -> Result<(), Error> {
        let next = self
            .frame
            .as_ref()
            .ok_or(Error::Base)?
            .apply(delta, limits)?;
        self.frame = Some(next);
        Ok(())
    }
}
