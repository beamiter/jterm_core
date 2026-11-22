//! Immutable raw-cell paint values. No projection, transport or resource ownership.
use crate::grid::{CursorShape, TerminalCell};
use std::{num::NonZeroU64, sync::Arc};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    Primary,
    Alternate,
}
/// Epoch is supplied by the owning publisher, never a pointer or grid dirty
/// version. This is a frame stream stamp, not a runtime or attachment lease.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    pub epoch: u64,
    pub revision: u64,
    pub layout: u64,
    pub screen: Screen,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PublicationEpoch(NonZeroU64);
impl PublicationEpoch {
    pub(crate) fn from_raw(epoch: u64) -> Option<Self> {
        NonZeroU64::new(epoch).map(Self)
    }

    pub(crate) fn into_raw(self) -> u64 {
        self.0.get()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct FrameRevision(NonZeroU64);
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct LayoutRevision(NonZeroU64);

/// Stored in place of the raw public stamp. Only admitted values participate in
/// publication ordering; epoch, frame revision and layout cannot be interchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AdmittedStamp {
    pub(crate) epoch: PublicationEpoch,
    pub(crate) revision: FrameRevision,
    pub(crate) layout: LayoutRevision,
    screen: Screen,
}

impl TryFrom<Stamp> for AdmittedStamp {
    type Error = Error;

    fn try_from(stamp: Stamp) -> Result<Self, Self::Error> {
        Ok(Self {
            epoch: PublicationEpoch::from_raw(stamp.epoch).ok_or(Error::Revision)?,
            revision: FrameRevision(NonZeroU64::new(stamp.revision).ok_or(Error::Revision)?),
            layout: LayoutRevision(NonZeroU64::new(stamp.layout).ok_or(Error::Revision)?),
            screen: stamp.screen,
        })
    }
}

impl From<AdmittedStamp> for Stamp {
    fn from(stamp: AdmittedStamp) -> Self {
        Self {
            epoch: stamp.epoch.into_raw(),
            revision: stamp.revision.0.get(),
            layout: stamp.layout.0.get(),
            screen: stamp.screen,
        }
    }
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
    stamp: AdmittedStamp,
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
        let stamp = stamp.try_into()?;
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
        self.stamp.into()
    }
    pub(crate) fn admitted_stamp(&self) -> AdmittedStamp {
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
        let next_stamp = next.stamp();
        self.admit_delta_stamp(next_stamp)?;
        if self.cols != next.cols || self.rows.len() != next.rows.len() {
            return Err(Error::Layout);
        }
        Ok(Delta {
            base: self.stamp(),
            next: next_stamp,
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
    fn admit_delta_stamp(&self, next: Stamp) -> Result<AdmittedStamp, Error> {
        // Keep malformed raw-delta error precedence: a mismatched stream or
        // layout (including zero) outranks an invalid next revision.
        let raw = self.stamp();
        if next.epoch != raw.epoch || next.layout != raw.layout || next.screen != raw.screen {
            return Err(Error::Layout);
        }
        let revision = FrameRevision(NonZeroU64::new(next.revision).ok_or(Error::Revision)?);
        if revision <= self.stamp.revision {
            return Err(Error::Revision);
        }
        Ok(AdmittedStamp {
            revision,
            ..self.stamp
        })
    }
    /// Budget checks precede scratch/copy allocation; rejection leaves the original intact.
    pub fn apply(&self, delta: &Delta, limits: Limits) -> Result<Self, Error> {
        if delta.base != self.stamp() {
            return Err(Error::Base);
        }
        let next = self.admit_delta_stamp(delta.next)?;
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
        // The admitted base and validated edit set already establish all
        // invariants, including row widths. Do not rescan cloned rows.
        Ok(Self {
            stamp: next,
            cols: self.cols,
            rows: rows.into(),
            cursor: delta.cursor,
        })
    }
}
/// Receiver does not invent epochs: reconnect must explicitly bind an expected epoch.
pub struct Receiver {
    epoch: Option<PublicationEpoch>,
    frame: Option<CellFrame>,
}
impl Receiver {
    pub fn new(epoch: u64) -> Self {
        Self {
            epoch: PublicationEpoch::from_raw(epoch),
            frame: None,
        }
    }
    pub fn frame(&self) -> Option<&CellFrame> {
        self.frame.as_ref()
    }
    pub fn full(&mut self, frame: CellFrame) -> Result<(), Error> {
        if Some(frame.stamp.epoch) != self.epoch
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
