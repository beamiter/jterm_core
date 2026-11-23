use std::{
    ops::Range,
    rc::{Rc, Weak},
};

use jterm_engine::{
    frame::{CellFrame, Screen, Stamp},
    history_reflow::HistoryLayout,
    terminal::{Terminal, TranscriptAnchor},
};

use crate::{view::RowSource, viewport::ScrollTarget};

#[derive(Clone, Copy)]
pub enum FindRegion {
    /// The live primary or alternate grid, without retained history.
    Screen,
    /// The current command from its source boundary through the tail.
    CommandOutput(TranscriptAnchor),
    /// The continuous primary document, starting near the viewport.
    Transcript,
}

/// Identity and visual geometry of one immutable search snapshot.
#[derive(Clone, Debug)]
pub struct FindFrame(pub(crate) Rc<FindSource>);

#[derive(Debug)]
pub(crate) struct FindSource {
    pub(crate) owner: Weak<()>,
    pub(crate) stamp: Stamp,
    pub(crate) history_rows: usize,
    pub(crate) history_start: u64,
    pub(crate) history_end: u64,
}

impl FindFrame {
    pub(crate) fn same_snapshot(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct FindPoint {
    pub(crate) row: usize,
    pub(crate) offset: i32,
}

#[derive(Clone, Debug)]
pub struct FindMatch {
    pub(crate) frame: FindFrame,
    pub(crate) start: FindPoint,
    pub(crate) end: FindPoint,
}

pub struct FindSnapshot {
    pub frame: FindFrame,
    pub windows: Vec<FindWindow>,
    pub incomplete: bool,
}

#[derive(Clone)]
pub(crate) struct FindHighlights {
    pub(crate) frame: FindFrame,
    pub(crate) hits: Rc<[FindMatch]>,
    pub(crate) current: Option<usize>,
}

/// A contiguous logical text range. The index resolves UTF-8 matches to
/// characters of the same rows the GTK presentation paints.
pub struct FindWindow {
    frame: FindFrame,
    text: String,
    rows: Vec<FindRow>,
}

struct FindRow {
    visual_row: usize,
    start: usize,
    end: usize,
    offset: i32,
    gap: Option<i32>,
    wrapped: bool,
}

impl FindWindow {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn match_at(&self, bytes: Range<usize>) -> Option<FindMatch> {
        if self.text.get(bytes.clone())?.is_empty() {
            return None;
        }
        let start = self.point(bytes.start, false)?;
        let end = self.point(bytes.end, true)?;
        (start < end).then(|| FindMatch {
            frame: self.frame.clone(),
            start,
            end,
        })
    }

    fn point(&self, byte: usize, exclusive: bool) -> Option<FindPoint> {
        let index = self
            .rows
            .partition_point(|row| row.start <= byte)
            .checked_sub(1)?;
        let mut row = &self.rows[index];
        if exclusive && row.start == byte && index > 0 && self.rows[index - 1].end == byte {
            row = &self.rows[index - 1];
        }
        let logical = self.text.get(row.start..byte.min(row.end))?.chars().count() as i32;
        let crossed_gap = row
            .gap
            .is_some_and(|gap| logical > gap || logical == gap && !exclusive);
        Some(FindPoint {
            row: row.visual_row,
            offset: row.offset + logical + i32::from(crossed_gap),
        })
    }

    fn push_row(
        &mut self,
        visual_row: usize,
        source: RowSource<'_>,
        first_column: usize,
        byte_limit: usize,
    ) -> bool {
        let Some(row) = source.row() else {
            return false;
        };
        let before = self.text.len();
        if self.rows.last().is_some_and(|row| !row.wrapped) {
            self.text.push('\n');
        }
        let start = self.text.len();
        let mut offset = 0;
        let mut logical_chars = 0;
        let mut gap = None;
        let mut content_end = start;
        let margin_gap = source.margin_gap();
        for (column, cell) in row.cells.iter().enumerate() {
            if cell.flags.wide_continuation() {
                continue;
            }
            if column < first_column {
                offset += source.character_count(column);
            } else if margin_gap == Some(column) {
                gap = Some(logical_chars);
            } else {
                logical_chars += source.push_text(&mut self.text, column, cell.character);
                if !cell.is_reflow_trimmable_blank() {
                    content_end = self.text.len();
                }
            }
        }
        if !row.wrapped {
            let end = start + self.text[start..].trim_end_matches(' ').len();
            self.text.truncate(end.max(content_end));
        }
        if self.text.len() > byte_limit {
            self.text.truncate(before);
            return false;
        }
        self.rows.push(FindRow {
            visual_row,
            start,
            end: self.text.len(),
            offset,
            gap,
            wrapped: row.wrapped,
        });
        true
    }
}

pub(crate) fn capture(
    terminal: &Terminal,
    owner: Weak<()>,
    region: FindRegion,
    top: Option<ScrollTarget>,
    mut byte_limit: usize,
    expired: &mut dyn FnMut() -> bool,
) -> Option<FindSnapshot> {
    if terminal.publication_held() {
        return None;
    }
    let cells = terminal.frame().ok()?;
    let history = terminal.history_extent();
    let layout = if cells.stamp().screen == Screen::Primary {
        Some(terminal.history_layout().ok()?)
    } else {
        None
    };
    let history_rows = layout.as_deref().map_or(0, HistoryLayout::len);
    let frame = FindFrame(Rc::new(FindSource {
        owner,
        stamp: cells.stamp(),
        history_rows,
        history_start: history.start,
        history_end: history.end,
    }));
    let mut snapshot = FindSnapshot {
        frame,
        windows: Vec::new(),
        incomplete: false,
    };
    let bottom = history_rows + cells.rows().len();
    let (first, column) = match region {
        FindRegion::Screen => (history_rows, 0),
        FindRegion::Transcript => (
            top.zip(layout.as_deref())
                .map_or(history_rows, |(top, layout)| top.row(layout)),
            0,
        ),
        FindRegion::CommandOutput(anchor) => {
            let Some(source) = terminal.locate_transcript_source(anchor) else {
                snapshot.incomplete = true;
                return Some(snapshot);
            };
            if source.row < history.end {
                let layout = layout.as_deref()?;
                let row = layout.row_at(source);
                let page = terminal.projected_history_page(layout, row, 1).ok()?;
                let columns = page.row(0)?.cells.len();
                let column = (0..columns)
                    .find(|&column| {
                        terminal
                            .projected_history_source(layout, row, column)
                            .is_some_and(|point| point >= source)
                    })
                    .unwrap_or(columns);
                (row, column)
            } else {
                (
                    history_rows + (source.row - history.end) as usize,
                    usize::from(source.column),
                )
            }
        }
    };
    let sources = FindRows {
        terminal,
        layout: layout.as_deref(),
        cells: &cells,
        history_rows,
        frame: &snapshot.frame,
    };
    let first = if matches!(region, FindRegion::Transcript) {
        let (row, complete) = sources.line_start(first, expired)?;
        snapshot.incomplete = !complete;
        row
    } else {
        first
    };
    let windows = [
        first..bottom,
        0..if matches!(region, FindRegion::Transcript) {
            first
        } else {
            0
        },
    ];
    for (index, rows) in windows
        .into_iter()
        .enumerate()
        .filter(|(_, rows)| !rows.is_empty())
    {
        let (window, complete) = sources.window(
            rows,
            if index == 0 { column } else { 0 },
            byte_limit,
            expired,
        )?;
        byte_limit -= window.text.len();
        snapshot.windows.push(window);
        if !complete {
            snapshot.incomplete = true;
            break;
        }
    }
    Some(snapshot)
}

struct FindRows<'a> {
    terminal: &'a Terminal,
    layout: Option<&'a HistoryLayout>,
    cells: &'a CellFrame,
    history_rows: usize,
    frame: &'a FindFrame,
}

impl FindRows<'_> {
    fn line_start(
        &self,
        mut row: usize,
        expired: &mut dyn FnMut() -> bool,
    ) -> Option<(usize, bool)> {
        while row > self.history_rows {
            if !self.cells.rows()[row - self.history_rows - 1].wrapped {
                return Some((row, true));
            }
            row -= 1;
        }
        while row > 0 {
            if expired() {
                return Some((row, false));
            }
            let first = row.saturating_sub(16);
            let page = self
                .terminal
                .projected_history_page(self.layout?, first, row - first)
                .ok()?;
            for index in (0..row - first).rev() {
                if !page.row(index)?.wrapped {
                    return Some((first + index + 1, true));
                }
            }
            row = first;
        }
        Some((0, true))
    }

    fn window(
        &self,
        rows: Range<usize>,
        first_column: usize,
        byte_limit: usize,
        expired: &mut dyn FnMut() -> bool,
    ) -> Option<(FindWindow, bool)> {
        let mut window = FindWindow {
            frame: self.frame.clone(),
            text: String::new(),
            rows: Vec::new(),
        };
        let historical_end = rows.end.min(self.history_rows);
        let mut row = rows.start;
        while row < historical_end {
            if expired() || window.text.len() == byte_limit {
                return Some((window, false));
            }
            let count = (historical_end - row).min(16);
            let page = self
                .terminal
                .projected_history_page(self.layout?, row, count)
                .ok()?;
            for index in 0..count {
                let column = if row == rows.start { first_column } else { 0 };
                if expired()
                    || !window.push_row(row, RowSource::History(&page, index), column, byte_limit)
                {
                    return Some((window, false));
                }
                row += 1;
            }
        }
        let gaps = self.terminal.live_margin_gaps();
        for row in row..rows.end {
            let column = if row == rows.start { first_column } else { 0 };
            let source = RowSource::Live(self.cells, row - self.history_rows, gaps);
            if expired() || !window.push_row(row, source, column, byte_limit) {
                return Some((window, false));
            }
        }
        Some((window, true))
    }
}

#[cfg(test)]
#[test]
fn find_keeps_a_command_boundary_when_its_row_archives() {
    let mut terminal = Terminal::new(3, 12).unwrap();
    terminal.track_transcript_rows();
    terminal.feed(b"older word\r\npre: ").unwrap();
    let start = terminal.transcript_cursor().unwrap();
    terminal.feed(b"word-new\r\n1\r\n2\r\n3\r\n4").unwrap();
    let region = FindRegion::CommandOutput(start);
    let scan = capture(&terminal, Weak::new(), region, None, 4096, &mut || false).unwrap();
    assert!(!scan.incomplete);
    assert_eq!(scan.windows[0].text(), "word-new\n1\n2\n3\n4");
    let focus = scan.windows[0].match_at(0..8).unwrap();
    assert_eq!(focus.start, FindPoint { row: 1, offset: 5 });
    assert_eq!(focus.end, FindPoint { row: 2, offset: 1 });

    let limited = capture(&terminal, Weak::new(), region, None, 8, &mut || false).unwrap();
    assert!(limited.incomplete);
    terminal.set_history_row_limit(0);
    let evicted = capture(&terminal, Weak::new(), region, None, 4096, &mut || false).unwrap();
    assert!(evicted.incomplete);
    assert!(evicted.windows.is_empty());
}

#[cfg(test)]
#[test]
fn transcript_find_keeps_a_wrapped_match_across_the_viewport_boundary() {
    let mut terminal = Terminal::new(2, 5).unwrap();
    terminal
        .feed(b"head\r\nabcdefgh\r\nthird\r\nfourth\r\nfifth")
        .unwrap();
    let top =
        ScrollTarget::Source(jterm_engine::history_reflow::HistoryAnchor { row: 2, column: 0 });
    let scan = capture(
        &terminal,
        Weak::new(),
        FindRegion::Transcript,
        Some(top),
        4096,
        &mut || false,
    )
    .unwrap();
    assert!(!scan.incomplete);
    assert!(
        scan.windows[0].text().contains("abcdefgh"),
        "{:?}",
        scan.windows[0].text()
    );
    assert!(scan.windows[1].text().contains("head"));
    assert_eq!(
        scan.windows
            .iter()
            .map(|window| window.text().matches("abcdefgh").count())
            .sum::<usize>(),
        1
    );
}
