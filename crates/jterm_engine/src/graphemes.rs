use crate::TerminalCell;
use std::{ops::Range, sync::Arc};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub(crate) const MAX_CLUSTER_BYTES: usize = 64;
// Together with up to 256 four-byte base scalars, this stays at or below
// the wire's 4 KiB text budget per maximum-width row.
pub(crate) const MAX_ROW_CLUSTER_BYTES: usize = 3 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Cluster {
    column: u16,
    text: Arc<str>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RowClusters {
    cells: Vec<Cluster>,
    text_bytes: usize,
}

impl RowClusters {
    fn position(&self, column: usize) -> usize {
        self.cells
            .partition_point(|cell| usize::from(cell.column) < column)
    }

    pub(crate) fn text(&self, column: usize) -> Option<&str> {
        self.shared_text(column).map(Arc::as_ref)
    }

    pub(crate) fn shared_text(&self, column: usize) -> Option<&Arc<str>> {
        let cell = self.cells.get(self.position(column))?;
        (usize::from(cell.column) == column).then_some(&cell.text)
    }

    pub(crate) fn end(&self) -> usize {
        self.cells
            .last()
            .map_or(0, |cell| usize::from(cell.column) + 1)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    pub(crate) fn retained_bytes(&self) -> usize {
        // Include the Arc headers for the row and each retained string.
        let arc_header = 2 * size_of::<usize>();
        size_of::<Self>()
            + self.cells.capacity() * size_of::<Cluster>()
            + self.text_bytes
            + (self.cells.len() + 1) * arc_header
    }

    pub(crate) fn can_store(&self, column: usize, bytes: usize) -> bool {
        let previous = self.text(column).map_or(0, str::len);
        self.text_bytes - previous + bytes <= MAX_ROW_CLUSTER_BYTES
    }

    pub(crate) fn can_store_after_displacing(&self, columns: Range<usize>, bytes: usize) -> bool {
        let range = self.position(columns.start)..self.position(columns.end);
        let removed = self.cells[range]
            .iter()
            .map(|cell| cell.text.len())
            .sum::<usize>();
        self.text_bytes - removed + bytes <= MAX_ROW_CLUSTER_BYTES
    }

    pub(crate) fn set(&mut self, column: usize, text: Arc<str>) {
        let index = self.position(column);
        let cell = Cluster {
            column: column as u16,
            text,
        };
        self.text_bytes += cell.text.len();
        if self
            .cells
            .get(index)
            .is_some_and(|previous| previous.column == cell.column)
        {
            self.text_bytes -= self.cells[index].text.len();
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
            .map(|cell| cell.text.len())
            .sum::<usize>();
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
        self.cells.iter().all(|cluster| {
            cells.get(usize::from(cluster.column)).is_some_and(|cell| {
                !cell.flags.wide_continuation() && cluster.text.starts_with(cell.character)
            })
        })
    }
}

fn regional_indicator(ch: char) -> bool {
    ('\u{1f1e6}'..='\u{1f1ff}').contains(&ch)
}

/// Only keep a reversible narrow write for bases that can widen in this profile.
pub(crate) fn may_grow(first: char) -> bool {
    if first.is_ascii() {
        return matches!(first, '#' | '*' | '0'..='9');
    }
    if regional_indicator(first) {
        return true;
    }
    selector_width(first, '\u{fe0f}') == 2
}

pub(crate) fn may_contract(first: char) -> bool {
    first.width() == Some(2) && selector_width(first, '\u{fe0e}') == 1
}

fn selector_width(first: char, selector: char) -> usize {
    let mut bytes = [0; 7];
    let len = first.encode_utf8(&mut bytes).len();
    let end = len + selector.encode_utf8(&mut bytes[len..]).len();
    std::str::from_utf8(&bytes[..end])
        .expect("UTF-8 scalar and selector")
        .width()
}

pub(crate) fn has_narrow_text_presentation(cluster: &str) -> bool {
    cluster.chars().nth(1) == Some('\u{fe0e}') && cluster.width() == 1
}

pub(crate) fn has_wide_emoji_presentation(cluster: &str) -> bool {
    let mut chars = cluster.chars();
    let (Some(first), Some(next)) = (chars.next(), chars.next()) else {
        return false;
    };
    (next == '\u{fe0f}' || (regional_indicator(first) && regional_indicator(next)))
        && cluster.width() == 2
}

/// Candidate includes one scalar beyond the accepted size so overflow
/// rejects only a true continuation, leaving ordinary adjacent glyphs intact.
pub(crate) fn extension<'a>(
    base: &str,
    next: char,
    buf: &'a mut [u8; MAX_CLUSTER_BYTES + 4],
) -> Option<&'a str> {
    buf[..base.len()].copy_from_slice(base.as_bytes());
    let len = base.len() + next.encode_utf8(&mut buf[base.len()..]).len();
    let candidate = std::str::from_utf8(&buf[..len]).expect("UTF-8 cluster and scalar");
    (candidate.graphemes(true).nth(1).is_none()).then_some(candidate)
}
