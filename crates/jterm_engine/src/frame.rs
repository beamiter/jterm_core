//! Immutable raw-cell paint values and their referenced style metadata.
use crate::{
    colors::ColorState,
    grid::{Color, CursorShape, TerminalCell, UnderlineColorRun},
    hyperlink::{resolve_resource, retain_resources, HyperlinkResources},
    row_annotations::RowAnnotations,
    HyperlinkId,
};
use smallvec::SmallVec;
use std::{fmt, num::NonZeroU64, sync::Arc};

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
#[derive(Clone, PartialEq, Eq)]
pub struct CellFrame {
    stamp: AdmittedStamp,
    cols: usize,
    rows: Arc<[Row]>,
    cursor: Cursor,
    underline_colors: Option<UnderlineRows>,
    hyperlinks: Option<HyperlinkResources>,
    annotations: Option<AnnotationRows>,
    paint_colors: Option<Arc<ColorState>>,
}

impl fmt::Debug for CellFrame {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = formatter.debug_struct("CellFrame");
        debug
            .field("stamp", &self.stamp)
            .field("cols", &self.cols)
            .field("rows", &self.rows)
            .field("cursor", &self.cursor);
        if let Some(underline_colors) = &self.underline_colors {
            debug.field("underline_colors", underline_colors);
        }
        if let Some(hyperlinks) = &self.hyperlinks {
            debug.field("hyperlink_count", &hyperlinks.len());
        }
        if let Some(annotations) = &self.annotations {
            debug.field("annotations", annotations);
        }
        debug.finish()
    }
}

pub(crate) type UnderlineRows = Arc<[Option<Arc<[UnderlineColorRun]>>]>;
pub(crate) type AnnotationRows = Arc<[Option<Arc<RowAnnotations>>]>;

fn same_shared_table<T: ?Sized>(before: &Option<Arc<T>>, after: &Option<Arc<T>>) -> bool {
    match (before, after) {
        (Some(before), Some(after)) => Arc::ptr_eq(before, after),
        (None, None) => true,
        _ => false,
    }
}

#[inline(never)]
fn retain_delta_hyperlinks(
    resources: &HyperlinkResources,
    rows: &[Row],
) -> Option<HyperlinkResources> {
    retain_resources(
        resources,
        rows.iter()
            .flat_map(|row| row.cells.iter().map(|cell| cell.hyperlink_id)),
    )
}

fn same_underline_runs(
    before: Option<&Arc<[UnderlineColorRun]>>,
    after: Option<&Arc<[UnderlineColorRun]>>,
) -> bool {
    match (before, after) {
        (Some(before), Some(after)) => before == after,
        (Some(runs), None) | (None, Some(runs)) => runs.is_empty(),
        (None, None) => true,
    }
}

pub(crate) fn underline_color_at(runs: &[UnderlineColorRun], col: usize) -> Option<Color> {
    let run = runs.get(runs.partition_point(|run| usize::from(run.end) <= col))?;
    (usize::from(run.start) <= col).then_some(run.color)
}

fn valid_underline_colors(runs: &[UnderlineColorRun], cols: usize) -> bool {
    let mut previous_end = 0;
    runs.iter().all(|run| {
        let valid = run.start >= previous_end
            && run.start < run.end
            && usize::from(run.end) <= cols
            && run.color != Color::Default;
        previous_end = run.end;
        valid
    })
}

#[cfg(test)]
#[test]
fn recapture_validates_changed_metadata_geometry_and_current_limits() {
    let limits = Limits {
        max_rows: 2,
        max_cols: 4,
        max_cells: 8,
    };
    let stamp = Stamp {
        epoch: 1,
        revision: 1,
        layout: 1,
        screen: Screen::Primary,
    };
    let cursor = Cursor {
        row: 0,
        col: 0,
        pending_wrap: false,
        visible: true,
        shape: CursorShape::Block,
    };
    let rows: Arc<[Row]> = vec![
        Row {
            cells: vec![TerminalCell::default(); 4].into(),
            wrapped: false
        };
        2
    ]
    .into();
    let colors: UnderlineRows = vec![
        Some(
            vec![UnderlineColorRun {
                start: 0,
                end: 4,
                color: Color::Red,
            }]
            .into(),
        ),
        None,
    ]
    .into();
    let base = CellFrame::new_with_underline_colors(
        stamp,
        4,
        rows.clone(),
        cursor,
        limits,
        Some(colors.clone()),
        None,
    )
    .unwrap();
    let capture = |cols, rows, cursor, limits, colors| {
        CellFrame::new_with_underline_colors(stamp, cols, rows, cursor, limits, colors, Some(&base))
    };
    assert_eq!(
        capture(4, rows.clone(), cursor, limits, Some(colors.clone())),
        Ok(base.clone())
    );
    let invalid: UnderlineRows = vec![
        colors[0].clone(),
        Some(
            vec![UnderlineColorRun {
                start: 0,
                end: 5,
                color: Color::Blue,
            }]
            .into(),
        ),
    ]
    .into();
    assert_eq!(
        capture(4, rows.clone(), cursor, limits, Some(invalid)),
        Err(Error::Bounds)
    );
    assert_eq!(
        capture(3, rows.clone(), cursor, limits, Some(colors.clone())),
        Err(Error::Bounds)
    );
    assert_eq!(
        capture(
            4,
            rows.clone(),
            cursor,
            Limits {
                max_cells: 7,
                ..limits
            },
            Some(colors.clone())
        ),
        Err(Error::Bounds)
    );
    assert_eq!(
        capture(4, rows, Cursor { col: 4, ..cursor }, limits, Some(colors)),
        Err(Error::Cursor)
    );
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
    underline_colors: Vec<(usize, Arc<[UnderlineColorRun]>)>,
    hyperlinks: HyperlinkDelta,
    paint_colors: Option<Option<Arc<ColorState>>>,
    annotations: Vec<(usize, Arc<RowAnnotations>)>,
}

#[derive(Clone, Debug)]
enum HyperlinkDelta {
    Inherit,
    Snapshot(Option<HyperlinkResources>),
}

impl Delta {
    /// Construct raw cell changes without explicit text or underline decoration.
    pub fn new(base: Stamp, next: Stamp, cursor: Cursor, rows: Vec<(usize, Row)>) -> Self {
        Self {
            base,
            next,
            cursor,
            rows,
            underline_colors: Vec::new(),
            hyperlinks: HyperlinkDelta::Inherit,
            paint_colors: None,
            annotations: Vec::new(),
        }
    }
}
impl CellFrame {
    pub fn new(
        stamp: Stamp,
        cols: usize,
        rows: Arc<[Row]>,
        cursor: Cursor,
        limits: Limits,
    ) -> Result<Self, Error> {
        Self::new_with_underline_colors(stamp, cols, rows, cursor, limits, None, None)
    }

    pub(crate) fn new_with_underline_colors(
        stamp: Stamp,
        cols: usize,
        rows: Arc<[Row]>,
        cursor: Cursor,
        limits: Limits,
        underline_colors: Option<UnderlineRows>,
        previous: Option<&Self>,
    ) -> Result<Self, Error> {
        let stamp = stamp.try_into()?;
        let previous =
            previous.filter(|frame| frame.cols == cols && frame.rows.len() == rows.len());
        if cols == 0
            || rows.is_empty()
            || cols > limits.max_cols
            || rows.len() > limits.max_rows
            || rows
                .len()
                .checked_mul(cols)
                .is_none_or(|n| n > limits.max_cells)
            || (!previous.is_some_and(|frame| Arc::ptr_eq(&frame.rows, &rows))
                && rows.iter().any(|r| r.cells.len() != cols))
        {
            return Err(Error::Bounds);
        }
        if cursor.row >= rows.len() || cursor.col >= cols {
            return Err(Error::Cursor);
        }
        if let Some(colors) = &underline_colors {
            let previous_colors = previous.and_then(|frame| frame.underline_colors.as_ref());
            let shared_table = previous_colors.is_some_and(|old| Arc::ptr_eq(old, colors));
            if colors.len() != rows.len()
                || (!shared_table
                    && colors.iter().enumerate().any(|(row, runs)| {
                        let Some(runs) = runs else { return false };
                        let old = previous_colors.and_then(|table| table[row].as_ref());
                        !old.is_some_and(|old| Arc::ptr_eq(old, runs))
                            && !valid_underline_colors(runs, cols)
                    }))
            {
                return Err(Error::Bounds);
            }
        }
        Ok(Self {
            stamp,
            cols,
            rows,
            cursor,
            underline_colors,
            hyperlinks: None,
            paint_colors: None,
            annotations: None,
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
    pub(crate) fn update_rows(
        &self,
        mut updates: impl Iterator<Item = (usize, Row)>,
    ) -> Arc<[Row]> {
        let Some((index, row)) = updates.next() else {
            return self.shared_rows();
        };
        let mut rows: Arc<[Row]> = Arc::from(self.rows.as_ref());
        let target = Arc::get_mut(&mut rows).expect("new row table is unique");
        target[index] = row;
        for (index, row) in updates {
            target[index] = row;
        }
        rows
    }
    pub fn cols(&self) -> usize {
        self.cols
    }
    pub fn cursor(&self) -> Cursor {
        self.cursor
    }

    pub(crate) fn recapture_cursor(
        &self,
        revision: u64,
        cursor: Cursor,
        paint_colors: Option<Arc<ColorState>>,
    ) -> Result<Self, Error> {
        let stamp = self.admit_delta_stamp(Stamp {
            revision,
            ..self.stamp()
        })?;
        if cursor.row >= self.rows.len() || cursor.col >= self.cols {
            return Err(Error::Cursor);
        }
        Ok(Self {
            stamp,
            cols: self.cols,
            rows: self.shared_rows(),
            cursor,
            underline_colors: self.shared_underline_rows(),
            hyperlinks: self.shared_hyperlinks(),
            annotations: self.shared_annotations(),
            paint_colors,
        })
    }

    /// Colors captured with this immutable publication.
    pub fn paint_colors(&self) -> Option<&ColorState> {
        self.paint_colors.as_deref()
    }

    pub(crate) fn with_paint_colors(mut self, colors: Option<Arc<ColorState>>) -> Self {
        self.paint_colors = colors;
        self
    }

    /// Conservative application-storage charge for decoded rows and metadata.
    /// Image raster backing is charged by its owning image store.
    pub fn retained_delivery_bytes(&self) -> usize {
        let header = 2 * size_of::<usize>();
        let rows = self.rows.len().saturating_mul(
            size_of::<Row>() + header + self.cols.saturating_mul(size_of::<TerminalCell>()),
        );
        let underlines = self.underline_colors.as_ref().map_or(0, |table| {
            header
                + table.len() * size_of::<Option<Arc<[UnderlineColorRun]>>>()
                + table
                    .iter()
                    .flatten()
                    .map(|runs| header + std::mem::size_of_val(runs.as_ref()))
                    .sum::<usize>()
        });
        let links = self.hyperlinks.as_ref().map_or(0, |table| {
            header
                + std::mem::size_of_val(table.as_ref())
                + table
                    .iter()
                    .map(|(_, uri)| header + uri.len())
                    .sum::<usize>()
        });
        let annotations = self.annotations.as_ref().map_or(0, |table| {
            header
                + table.len() * size_of::<Option<Arc<RowAnnotations>>>()
                + table
                    .iter()
                    .flatten()
                    .map(|row| row.retained_bytes())
                    .sum::<usize>()
        });
        rows + underlines
            + links
            + annotations
            + header
            + size_of::<Self>()
            + self
                .paint_colors
                .as_ref()
                .map_or(0, |_| header + size_of::<ColorState>())
    }

    pub(crate) fn with_hyperlinks(mut self, hyperlinks: Option<HyperlinkResources>) -> Self {
        self.hyperlinks = hyperlinks;
        self
    }

    pub(crate) fn shared_hyperlinks(&self) -> Option<HyperlinkResources> {
        self.hyperlinks.clone()
    }

    /// Resolve a validated target referenced by this immutable frame.
    pub fn hyperlink_target(&self, id: HyperlinkId) -> Option<&str> {
        resolve_resource(&self.hyperlinks, id)
    }

    /// Complete text at a multi-scalar grapheme head. Single-scalar and
    /// continuation cells remain represented by `rows()`.
    pub fn cluster_text(&self, row: usize, column: usize) -> Option<&str> {
        self.row_annotations(row)?.text(column)
    }

    /// Inline image references published with exactly this cell row.
    pub fn inline_image_rows(
        &self,
        row: usize,
    ) -> impl Iterator<Item = (usize, crate::terminal::InlineImageRow)> + '_ {
        self.row_annotations(row)
            .into_iter()
            .flat_map(|row| row.image_rows())
    }

    /// Image resource and vertical slice at each annotated physical column.
    pub fn inline_images(
        &self,
        row: usize,
    ) -> impl Iterator<Item = (usize, &crate::terminal::InlineImage, u16)> + '_ {
        self.row_annotations(row)
            .into_iter()
            .flat_map(|row| row.images())
    }

    pub(crate) fn row_annotations(&self, row: usize) -> Option<&Arc<RowAnnotations>> {
        self.annotations.as_ref()?.get(row)?.as_ref()
    }

    pub(crate) fn shared_annotations(&self) -> Option<AnnotationRows> {
        self.annotations.clone()
    }

    pub(crate) fn with_annotations(mut self, annotations: Option<AnnotationRows>) -> Self {
        debug_assert!(annotations.as_ref().is_none_or(|annotations| {
            annotations.len() == self.rows.len()
                && annotations
                    .iter()
                    .zip(self.rows.iter())
                    .all(|(line, row)| line.as_ref().is_none_or(|line| line.valid_for(&row.cells)))
        }));
        self.annotations = annotations;
        self
    }

    /// Explicit underline color at an original physical viewport coordinate.
    pub fn underline_color(&self, row: usize, col: usize) -> Option<Color> {
        underline_color_at(self.underline_color_runs(row), col)
    }

    pub fn underline_color_runs(&self, row: usize) -> &[UnderlineColorRun] {
        self.underline_row(row).map_or(&[], Arc::as_ref)
    }

    pub(crate) fn underline_row(&self, row: usize) -> Option<&Arc<[UnderlineColorRun]>> {
        self.underline_colors.as_ref()?.get(row)?.as_ref()
    }

    pub(crate) fn shared_underline_rows(&self) -> Option<UnderlineRows> {
        self.underline_colors.clone()
    }
    /// Fresh publication in the same stream. Screen/layout changes require a full frame.
    pub fn delta_to(&self, next: &Self) -> Result<Delta, Error> {
        let next_stamp = next.stamp();
        self.validate_delta_frame(next)?;
        let mut delta = Delta::new(self.stamp(), next_stamp, next.cursor, Vec::new());
        delta.hyperlinks = HyperlinkDelta::Snapshot(next.shared_hyperlinks());
        delta.paint_colors = Some(next.paint_colors.clone());
        if self.shares_paint_rows(next) {
            return Ok(delta);
        }
        for (index, after) in self.changed_rows(next) {
            delta.rows.push((index, after.clone()));
            if let Some(runs) = next.underline_row(index) {
                delta.underline_colors.push((index, runs.clone()));
            }
            if let Some(line) = next.row_annotations(index) {
                delta.annotations.push((index, line.clone()));
            }
        }
        Ok(delta)
    }

    /// Indices of rows whose cells, wrapping, underline colors, or annotations
    /// changed. Uses the same stream/revision/layout checks as `delta_to`,
    /// without constructing a delta or cloning row/resource metadata.
    #[inline]
    pub fn changed_row_indices(&self, next: &Self) -> Result<Vec<usize>, Error> {
        self.validate_delta_frame(next)?;
        if self.shares_paint_rows(next) {
            return Ok(Vec::new());
        }
        Ok(self.changed_rows(next).map(|(index, _)| index).collect())
    }

    fn validate_delta_frame(&self, next: &Self) -> Result<(), Error> {
        self.admit_delta_stamp(next.stamp())?;
        if self.cols != next.cols || self.rows.len() != next.rows.len() {
            return Err(Error::Layout);
        }
        Ok(())
    }

    fn shares_paint_rows(&self, next: &Self) -> bool {
        Arc::ptr_eq(&self.rows, &next.rows)
            && same_shared_table(&self.underline_colors, &next.underline_colors)
            && same_shared_table(&self.annotations, &next.annotations)
    }

    fn changed_rows<'a>(&'a self, next: &'a Self) -> impl Iterator<Item = (usize, &'a Row)> + 'a {
        let same_underlines = same_shared_table(&self.underline_colors, &next.underline_colors);
        let same_annotations = same_shared_table(&self.annotations, &next.annotations);
        self.rows
            .iter()
            .zip(next.rows.iter())
            .enumerate()
            .filter(move |(index, (before, after))| {
                before != after
                    || (!same_underlines
                        && !same_underline_runs(
                            self.underline_row(*index),
                            next.underline_row(*index),
                        ))
                    || (!same_annotations
                        && self.row_annotations(*index) != next.row_annotations(*index))
            })
            .map(|(index, (_, after))| (index, after))
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
        // Index replacements once for validation and direct shared-row construction.
        let mut edited = SmallVec::<[Option<&Row>; 128]>::new();
        if !delta.rows.is_empty() {
            edited.resize(self.rows.len(), None);
        }
        for (index, row) in &delta.rows {
            if *index >= self.rows.len() || row.cells.len() != self.cols {
                return Err(Error::Bounds);
            }
            if edited[*index].replace(row).is_some() {
                return Err(Error::Duplicate);
            }
        }
        if delta.cursor.row >= self.rows.len() || delta.cursor.col >= self.cols {
            return Err(Error::Cursor);
        }
        if delta.underline_colors.iter().any(|(index, runs)| {
            edited.get(*index).is_none_or(Option::is_none)
                || !valid_underline_colors(runs, self.cols)
        }) {
            return Err(Error::Bounds);
        }
        if delta.annotations.iter().any(|(index, line)| {
            edited
                .get(*index)
                .and_then(|row| *row)
                .is_none_or(|row| !line.valid_for(&row.cells))
        }) {
            return Err(Error::Bounds);
        }
        let rows: Arc<[Row]> = if edited.is_empty() {
            self.shared_rows()
        } else {
            self.rows
                .iter()
                .zip(edited.iter())
                .map(|(before, patch)| patch.unwrap_or(before).clone())
                .collect()
        };
        let underline_colors = if delta.rows.is_empty() {
            self.shared_underline_rows()
        } else if self.underline_colors.is_some() || !delta.underline_colors.is_empty() {
            let mut colors = self
                .underline_colors
                .as_ref()
                .map_or_else(|| vec![None; rows.len()], |current| current.to_vec());
            for (index, _) in &delta.rows {
                colors[*index] = None;
            }
            for (index, runs) in &delta.underline_colors {
                colors[*index] = Some(runs.clone());
            }
            colors.iter().any(Option::is_some).then(|| colors.into())
        } else {
            None
        };
        let annotations = if delta.rows.is_empty() {
            self.shared_annotations()
        } else if self.annotations.is_some() || !delta.annotations.is_empty() {
            let mut annotations = self
                .annotations
                .as_ref()
                .map_or_else(|| vec![None; rows.len()], |current| current.to_vec());
            for (index, _) in &delta.rows {
                annotations[*index] = None;
            }
            for (index, line) in &delta.annotations {
                annotations[*index] = Some(line.clone());
            }
            annotations
                .iter()
                .any(Option::is_some)
                .then(|| annotations.into())
        } else {
            None
        };
        let resources = match &delta.hyperlinks {
            HyperlinkDelta::Inherit => &self.hyperlinks,
            HyperlinkDelta::Snapshot(resources) => resources,
        };
        let hyperlinks = resources.as_ref().and_then(|resources| {
            if self
                .hyperlinks
                .as_ref()
                .is_some_and(|old| Arc::ptr_eq(old, resources))
                && delta.rows.iter().all(|(index, _)| {
                    self.rows[*index]
                        .cells
                        .iter()
                        .all(|cell| cell.hyperlink_id.is_none())
                })
            {
                // Every target in the base is still referenced by an unchanged row.
                Some(resources.clone())
            } else {
                retain_delta_hyperlinks(resources, &rows)
            }
        });
        // The admitted base and validated edit set already establish all
        // invariants, including row widths. Do not rescan cloned rows.
        Ok(Self {
            stamp: next,
            cols: self.cols,
            rows,
            cursor: delta.cursor,
            paint_colors: delta
                .paint_colors
                .as_ref()
                .unwrap_or(&self.paint_colors)
                .clone(),
            underline_colors,
            hyperlinks,
            annotations,
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

#[cfg(test)]
#[test]
fn applying_cursor_only_delta_reuses_rows_and_preserves_paint_metadata() {
    let mut terminal = crate::terminal::Terminal::new(3, 8).unwrap();
    terminal.enable_underline_colors();
    terminal.enable_hyperlinks(|_, _| true);
    terminal
        .feed(b"\x1b]8;;https://example.test/old\x07\x1b[4;58;2;12;34;56mX\x1b[0m\x1b]8;;\x07")
        .unwrap();
    let base = terminal.frame().unwrap();
    terminal.feed(b"\x1b[2;3H").unwrap();
    let next = terminal.frame().unwrap();
    let delta = base.delta_to(&next).unwrap();
    assert!(delta.rows.is_empty());
    let applied = base
        .apply(
            &delta,
            Limits {
                max_rows: 3,
                max_cols: 8,
                max_cells: 24,
            },
        )
        .unwrap();
    assert!(Arc::ptr_eq(&base.shared_rows(), &applied.shared_rows()));
    assert!(Arc::ptr_eq(
        base.hyperlinks.as_ref().unwrap(),
        applied.hyperlinks.as_ref().unwrap()
    ));
    assert_eq!(applied, next);
    assert_eq!(
        applied.underline_color_runs(0),
        base.underline_color_runs(0)
    );
}

#[cfg(test)]
#[test]
fn changed_row_indices_match_delta_rows_and_admission_errors() {
    let mut terminal = crate::terminal::Terminal::new(3, 8).unwrap();
    terminal.enable_underline_colors();
    terminal.feed(b"\x1b[4;58;2;12;34;56mX\x1b[1;1H").unwrap();
    let base = terminal.frame().unwrap();
    let check = |next: &CellFrame| {
        assert_eq!(
            base.changed_row_indices(next),
            base.delta_to(next).map(|delta| delta
                .rows
                .into_iter()
                .map(|(index, _)| index)
                .collect())
        );
    };
    check(&base);
    terminal.feed(b"\x1b[2;3H").unwrap();
    check(&terminal.frame().unwrap());
    terminal
        .feed(b"\x1b[1;1H\x1b[58;2;99;88;77mX\x1b[1;1H")
        .unwrap();
    let decorated = terminal.frame().unwrap();
    assert_eq!(base.rows(), decorated.rows());
    assert_eq!(base.changed_row_indices(&decorated), Ok(vec![0]));
    check(&decorated);
    terminal.feed(b"\x1b[3;1HY").unwrap();
    check(&terminal.frame().unwrap());
    terminal.resize(3, 7).unwrap();
    check(&terminal.frame().unwrap());
}

#[cfg(test)]
#[test]
fn cursor_only_delta_admits_resource_replacement_and_inherited_targets() {
    let mut terminal = crate::terminal::Terminal::new(3, 8).unwrap();
    terminal.enable_hyperlinks(|_, _| true);
    terminal
        .feed(b"\x1b]8;;https://example.test/old\x07X\x1b]8;;\x07")
        .unwrap();
    let base = terminal.frame().unwrap();
    terminal.feed(b"\x1b[2;3H").unwrap();
    let next = terminal.frame().unwrap();
    let limits = Limits {
        max_rows: 3,
        max_cols: 8,
        max_cells: 24,
    };
    let id = base.rows()[0].cells[0].hyperlink_id;
    let inherited = Delta::new(base.stamp(), next.stamp(), next.cursor(), Vec::new());
    let inherited_frame = base.apply(&inherited, limits).unwrap();
    assert_eq!(inherited_frame, next);
    assert!(Arc::ptr_eq(
        base.hyperlinks.as_ref().unwrap(),
        inherited_frame.hyperlinks.as_ref().unwrap()
    ));

    let mut changed = base.delta_to(&next).unwrap();
    changed.hyperlinks = HyperlinkDelta::Snapshot(Some(Arc::from([
        (id, Arc::from("https://example.test/new")),
        (
            HyperlinkId::from_raw(4096),
            Arc::from("https://example.test/unused"),
        ),
    ])));
    let replaced = base.apply(&changed, limits).unwrap();
    assert_eq!(
        replaced.hyperlink_target(id),
        Some("https://example.test/new")
    );
    assert_eq!(replaced.hyperlink_target(HyperlinkId::from_raw(4096)), None);
    assert_eq!(replaced.rows(), base.rows());
    assert!(!Arc::ptr_eq(
        base.hyperlinks.as_ref().unwrap(),
        replaced.hyperlinks.as_ref().unwrap()
    ));
}

#[cfg(test)]
#[test]
fn delta_reuses_links_outside_edited_rows_and_releases_the_last_reference() {
    let mut terminal = crate::terminal::Terminal::new(3, 8).unwrap();
    terminal.enable_hyperlinks(|_, _| true);
    terminal
        .feed(b"\x1b]8;;https://example.test/link\x07X\x1b]8;;\x07")
        .unwrap();
    let before = terminal.frame().unwrap();
    terminal.feed(b"\x1b[2;1HY").unwrap();
    let after = terminal.frame().unwrap();
    let limits = Limits {
        max_rows: 3,
        max_cols: 8,
        max_cells: 24,
    };
    let delta = before.delta_to(&after).unwrap();
    let applied = before.apply(&delta, limits).unwrap();
    assert_eq!(applied, after);
    assert!(Arc::ptr_eq(
        before.hyperlinks.as_ref().unwrap(),
        applied.hyperlinks.as_ref().unwrap()
    ));

    terminal.feed(b"\x1b[1;1H\x1b[2K").unwrap();
    let erased = terminal.frame().unwrap();
    let mut delta = after.delta_to(&erased).unwrap();
    delta.hyperlinks = HyperlinkDelta::Inherit;
    let applied = after.apply(&delta, limits).unwrap();
    let id = before.rows()[0].cells[0].hyperlink_id;
    assert_eq!(applied, erased);
    assert_eq!(applied.hyperlink_target(id), None);
}
