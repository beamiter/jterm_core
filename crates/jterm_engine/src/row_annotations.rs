use crate::{
    terminal::{ImageAnnotation, InlineImage, InlineImageRow},
    TerminalCell,
};
use std::{ops::Range, sync::Arc};

// With 256 four-byte base scalars this stays within the 4 KiB row text budget.
const MAX_ROW_CLUSTER_BYTES: usize = 3 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Value {
    Text(Arc<str>),
    Image(ImageAnnotation),
}

impl Value {
    fn text_bytes(&self) -> usize {
        match self {
            Self::Text(text) => text.len(),
            Self::Image(_) => 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Annotation {
    column: u16,
    value: Value,
}

/// Metadata that moves with a physical cell through edits and history reflow.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RowAnnotations {
    cells: Vec<Annotation>,
    text_bytes: usize,
}

impl RowAnnotations {
    fn position(&self, column: usize) -> usize {
        self.cells
            .partition_point(|cell| usize::from(cell.column) < column)
    }

    fn get(&self, column: usize) -> Option<&Value> {
        let cell = self.cells.get(self.position(column))?;
        (usize::from(cell.column) == column).then_some(&cell.value)
    }

    pub(crate) fn text(&self, column: usize) -> Option<&str> {
        self.shared_text(column).map(Arc::as_ref)
    }

    pub(crate) fn shared_text(&self, column: usize) -> Option<&Arc<str>> {
        match self.get(column)? {
            Value::Text(text) => Some(text),
            Value::Image(_) => None,
        }
    }

    pub(crate) fn texts(&self) -> impl Iterator<Item = (usize, &str)> + '_ {
        self.cells.iter().filter_map(|cell| match &cell.value {
            Value::Text(text) => Some((usize::from(cell.column), text.as_ref())),
            Value::Image(_) => None,
        })
    }

    pub(crate) fn image_at(&self, column: usize) -> Option<&ImageAnnotation> {
        match self.get(column)? {
            Value::Image(row) => Some(row),
            Value::Text(_) => None,
        }
    }

    pub(crate) fn image_rows(&self) -> impl Iterator<Item = (usize, InlineImageRow)> + '_ {
        self.cells.iter().filter_map(|cell| match &cell.value {
            Value::Image(image) => Some((usize::from(cell.column), image.row())),
            Value::Text(_) => None,
        })
    }

    pub(crate) fn images(&self) -> impl Iterator<Item = (usize, &InlineImage, u16)> + '_ {
        self.cells.iter().filter_map(|cell| match &cell.value {
            Value::Image(annotation) => {
                let (image, slice) = annotation.resource();
                Some((usize::from(cell.column), image, slice))
            }
            Value::Text(_) => None,
        })
    }

    pub(crate) fn end(&self) -> usize {
        self.cells
            .last()
            .map_or(0, |cell| usize::from(cell.column) + 1)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    pub(crate) fn has_text(&self) -> bool {
        self.text_bytes != 0
    }

    pub(crate) fn retained_bytes(&self) -> usize {
        let arc_header = 2 * size_of::<usize>();
        size_of::<Self>()
            + self.cells.capacity() * size_of::<Annotation>()
            + self.text_bytes
            // Charge each reference conservatively, including shared image leases.
            + (self.cells.len() + 1) * arc_header
    }

    pub(crate) fn can_store(&self, column: usize, bytes: usize) -> bool {
        let previous = self.text(column).map_or(0, str::len);
        self.text_bytes - previous + bytes <= MAX_ROW_CLUSTER_BYTES
    }

    pub(crate) fn can_store_after_displacing(&self, columns: Range<usize>, bytes: usize) -> bool {
        self.text_bytes - self.text_bytes_in(columns) + bytes <= MAX_ROW_CLUSTER_BYTES
    }

    pub(crate) fn set(&mut self, column: usize, text: Arc<str>) {
        self.set_value(column, Value::Text(text));
    }

    pub(crate) fn set_image(&mut self, column: usize, row: ImageAnnotation) {
        self.set_value(column, Value::Image(row));
    }

    fn set_value(&mut self, column: usize, value: Value) {
        let index = self.position(column);
        let cell = Annotation {
            column: column as u16,
            value,
        };
        self.text_bytes += cell.value.text_bytes();
        if self
            .cells
            .get(index)
            .is_some_and(|previous| previous.column == cell.column)
        {
            self.text_bytes -= self.cells[index].value.text_bytes();
            self.cells[index] = cell;
        } else {
            self.cells.insert(index, cell);
        }
    }

    pub(crate) fn erase(&mut self, cols: Range<usize>) {
        let range = self.position(cols.start)..self.position(cols.end);
        self.text_bytes -= self
            .cells
            .drain(range)
            .map(|cell| cell.value.text_bytes())
            .sum::<usize>();
    }

    pub(crate) fn text_bytes_in(&self, columns: Range<usize>) -> usize {
        self.in_columns(columns)
            .iter()
            .map(|cell| cell.value.text_bytes())
            .sum()
    }

    fn in_columns(&self, columns: Range<usize>) -> &[Annotation] {
        &self.cells[self.position(columns.start)..self.position(columns.end)]
    }

    /// Append metadata for a consecutive run of physical cells in a projected row.
    pub(crate) fn append_columns_from(
        &mut self,
        source: &Self,
        columns: Range<usize>,
        target_start: usize,
    ) {
        debug_assert!(self.end() <= target_start);
        let start = columns.start;
        let text_bytes = &mut self.text_bytes;
        self.cells
            .extend(source.in_columns(columns).iter().map(|cell| {
                let offset = usize::from(cell.column) - start;
                *text_bytes += cell.value.text_bytes();
                Annotation {
                    column: (target_start + offset) as u16,
                    value: cell.value.clone(),
                }
            }));
    }

    /// Copy text into an already-cleared, preflighted target span. Raster
    /// placements remain owned by their original screen coordinates.
    pub(crate) fn copy_text_from(
        &mut self,
        source: &Self,
        source_columns: Range<usize>,
        target_start: usize,
    ) {
        let target = self.position(target_start);
        let text_bytes = &mut self.text_bytes;
        let source_start = source_columns.start;
        let copied = source.in_columns(source_columns).iter().filter_map(|cell| {
            let Value::Text(text) = &cell.value else {
                return None;
            };
            let offset = usize::from(cell.column) - source_start;
            *text_bytes += text.len();
            Some(Annotation {
                column: (target_start + offset) as u16,
                value: Value::Text(Arc::clone(text)),
            })
        });
        drop(self.cells.splice(target..target, copied));
        debug_assert!(self.text_bytes <= MAX_ROW_CLUSTER_BYTES);
    }

    pub(crate) fn insert_columns(&mut self, column: usize, count: usize, cols: usize) {
        self.erase(cols - count..cols);
        let start = self.position(column);
        for cell in &mut self.cells[start..] {
            cell.column += count as u16;
        }
    }

    pub(crate) fn delete_columns(&mut self, column: usize, count: usize) {
        self.erase(column..column + count);
        let start = self.position(column);
        for cell in &mut self.cells[start..] {
            cell.column -= count as u16;
        }
    }

    pub(crate) fn valid_for(&self, cells: &[TerminalCell]) -> bool {
        self.cells.iter().all(|entry| {
            cells.get(usize::from(entry.column)).is_some_and(|cell| {
                !cell.flags.wide_continuation()
                    && match &entry.value {
                        Value::Text(text) => text.starts_with(cell.character),
                        Value::Image(_) => cell.character == ' ' && !cell.flags.wide(),
                    }
            })
        })
    }
}
