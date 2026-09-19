//! Cells, rows and one screen (history + visible window) of the replay.
//!
//! The layout mirrors VTE's ring: a row is a vector of cells that is only as
//! long as something wrote into it (VTE's `row->len`), cells that were never
//! written read as blank, and a wide character is a head cell followed by a
//! fragment cell. Keeping the same shape is what lets the emulator copy VTE's
//! edge cases (fragment cleanup, "smart" tabs, the row length a tab checks)
//! instead of approximating them.

use std::collections::VecDeque;

/// The cell continues the character to its left (the right half of a wide
/// character, or a column covered by a tab).
pub(super) const FRAGMENT: u32 = 1 << 31;
/// The cell is the head of a two-column character.
pub(super) const WIDE: u32 = 1 << 30;
/// The low bits index the cluster table (a base character with combining
/// marks) instead of holding a scalar value.
pub(super) const CLUSTER: u32 = 1 << 29;
/// Scalar value or cluster index.
pub(super) const VALUE_MASK: u32 = CLUSTER - 1;
/// A "smart tab" head (and, with [`FRAGMENT`], the columns it covers).
pub(super) const TAB: u32 = '\t' as u32;
const SPACE: u32 = ' ' as u32;
/// Longest run VTE stores as one copyable tab (`VTE_TAB_WIDTH_MAX`, the
/// 4-bit column count of a cell).
pub(super) const TAB_WIDTH_MAX: usize = 15;

/// One grid cell: 8 bytes, so a full 2 Mi-cell budget stays around 16 MiB.
/// `pen` indexes the interned attribute table (0 is the default pen).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Cell {
    pub(super) code: u32,
    pub(super) pen: u32,
}

impl Cell {
    /// Never written, or erased (VTE's `basic_cell` / `m_color_defaults`
    /// both carry character 0). Erased cells keep the erase pen's colours.
    pub(super) const BLANK: Cell = Cell { code: 0, pen: 0 };

    pub(super) const fn blank(pen: u32) -> Cell {
        Cell { code: 0, pen }
    }

    pub(super) fn is_fragment(self) -> bool {
        self.code & FRAGMENT != 0
    }

    pub(super) fn is_tab_head(self) -> bool {
        self.code == TAB
    }

    /// A column covered by a tab (not its head).
    pub(super) fn is_tab_head_fragment(self) -> bool {
        self.code == FRAGMENT | TAB
    }

    /// No glyph of its own: never written, erased, a space, or a tab column.
    pub(super) fn is_blank(self) -> bool {
        matches!(self.code, 0 | SPACE | TAB) || self.code == FRAGMENT | TAB
    }
}

/// One terminal row. `wrapped` is VTE's `soft_wrapped` (the row continues on
/// the next one because the text autowrapped). `touched` records that a
/// character was printed into the row or the cursor left it with a line
/// feed/autowrap; together with blankness it decides which never-used rows at
/// the top of the capture are trimmed from the output.
#[derive(Clone, Default, Debug)]
pub(super) struct Row {
    pub(super) cells: Vec<Cell>,
    pub(super) wrapped: bool,
    pub(super) touched: bool,
}

impl Row {
    pub(super) fn filled(cols: usize, cell: Cell) -> Row {
        Row {
            cells: vec![cell; cols],
            ..Row::default()
        }
    }

    /// Pads the row with never-written cells up to `len` (VTE's
    /// `_vte_row_data_fill(row, &basic_cell, len)`).
    pub(super) fn fill_to(&mut self, len: usize) {
        if self.cells.len() < len {
            self.cells.resize(len, Cell::BLANK);
        }
    }

    /// Pads the row up to `len` with `cell` (the erase fill of EL/ED/ECH).
    pub(super) fn fill_with(&mut self, len: usize, cell: Cell) {
        if self.cells.len() < len {
            self.cells.resize(len, cell);
        }
    }

    /// Repairs wide characters and tabs that a write to columns
    /// `start..end` is about to cut in half, exactly like VTE's
    /// `cleanup_fragments`: a tab cut on its right keeps its remainder as a
    /// shorter tab, a wide character cut on either side becomes a space in
    /// the surviving half, and a tab cut on its left turns into spaces.
    pub(super) fn cleanup_fragments(&mut self, start: usize, end: usize) {
        let cells = &mut self.cells;
        let start_is_fragment = cells.get(start).is_some_and(|c| c.is_fragment());
        if let Some(cell_end) = cells.get(end).copied() {
            if cell_end.is_fragment() {
                let mut head = end;
                while head > 0 {
                    head -= 1;
                    if !cells[head].is_fragment() {
                        break;
                    }
                }
                cells[end].code = if cells[head].is_tab_head() {
                    TAB
                } else {
                    SPACE
                };
            }
        }
        if start_is_fragment {
            let mut col = start;
            while col > 0 {
                col -= 1;
                let is_head = !cells[col].is_fragment();
                cells[col].code = SPACE;
                if is_head {
                    break;
                }
            }
        }
    }

    /// Number of columns the tab whose head is at `col` covers.
    #[cfg(test)]
    pub(super) fn tab_width(&self, col: usize) -> usize {
        1 + self.cells[col + 1..]
            .iter()
            .take_while(|c| c.code == FRAGMENT | TAB)
            .count()
    }

    /// Drops trailing cells that show nothing, so a row that is frozen into
    /// history only charges the budget for what it displays. Soft-wrapped
    /// rows keep their width: the blanks there sit *inside* a logical line.
    pub(super) fn trim_invisible_tail(&mut self, blank_is_visible: impl Fn(u32) -> bool) {
        if self.wrapped {
            return;
        }
        let keep = self
            .cells
            .iter()
            .rposition(|c| c.code != 0 || blank_is_visible(c.pen))
            .map_or(0, |i| i + 1);
        self.cells.truncate(keep);
    }
}

/// DECSC state (VTE's `VteScreen::saved`).
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct SavedCursor {
    pub(super) row: usize,
    pub(super) col: usize,
    pub(super) pen: super::pen::Pen,
    pub(super) origin: bool,
    pub(super) charsets: [bool; 2],
    pub(super) shift_out: bool,
}

/// One screen: its scrollback plus the visible window, the cursor and the
/// saved cursor. The normal screen keeps history; the alternate screen (like
/// VTE's) has none.
pub(super) struct Screen {
    /// History rows first, then exactly `rows` visible rows.
    pub(super) lines: VecDeque<Row>,
    pub(super) rows: usize,
    /// Sum of the cell counts of the history rows (the budget's variable part).
    pub(super) history_cells: usize,
    pub(super) keeps_history: bool,
    /// Visible row of the cursor, `0..rows`.
    pub(super) row: usize,
    /// Cursor column, `0..=cols`. `cols` is VTE's "pending wrap" position:
    /// the last column was just printed and the next character wraps.
    pub(super) col: usize,
    pub(super) saved: SavedCursor,
}

impl Screen {
    pub(super) fn new(rows: usize, keeps_history: bool) -> Screen {
        Screen {
            lines: (0..rows).map(|_| Row::default()).collect(),
            rows,
            history_cells: 0,
            keeps_history,
            row: 0,
            col: 0,
            saved: SavedCursor::default(),
        }
    }

    pub(super) fn history_len(&self) -> usize {
        self.lines.len() - self.rows
    }

    /// Visible row `r`.
    pub(super) fn row_mut(&mut self, r: usize) -> &mut Row {
        let base = self.history_len();
        &mut self.lines[base + r]
    }

    pub(super) fn cursor_row_mut(&mut self) -> &mut Row {
        let r = self.row;
        self.row_mut(r)
    }

    /// Drops the whole scrollback (ED 3).
    pub(super) fn clear_history(&mut self) {
        let history = self.history_len();
        self.lines.drain(..history);
        self.history_cells = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row_of(codes: &[u32]) -> Row {
        Row {
            cells: codes.iter().map(|&code| Cell { code, pen: 0 }).collect(),
            ..Row::default()
        }
    }

    #[test]
    fn cutting_a_wide_character_leaves_a_space_in_the_surviving_half() {
        let wide = WIDE | '中' as u32;
        let mut row = row_of(&[wide, FRAGMENT, 'x' as u32]);
        // Overwrite the right half only.
        row.cleanup_fragments(1, 2);
        assert_eq!(row.cells[0].code, SPACE);
        let mut row = row_of(&[wide, FRAGMENT, 'x' as u32]);
        // Overwrite the left half only: the orphaned right half becomes a space.
        row.cleanup_fragments(0, 1);
        assert_eq!(row.cells[1].code, SPACE);
    }

    #[test]
    fn cutting_a_tab_keeps_the_right_remainder_as_a_shorter_tab() {
        let mut row = row_of(&[TAB, FRAGMENT | TAB, FRAGMENT | TAB, FRAGMENT | TAB]);
        row.cleanup_fragments(1, 2);
        assert_eq!(row.cells[0].code, SPACE, "left part turns into spaces");
        assert_eq!(row.cells[2].code, TAB, "right part becomes a new tab head");
        assert_eq!(row.tab_width(2), 2);
    }

    #[test]
    fn history_rows_drop_their_invisible_tail_unless_soft_wrapped() {
        let mut row = row_of(&['a' as u32, 0, 0]);
        row.trim_invisible_tail(|_| false);
        assert_eq!(row.cells.len(), 1);
        let mut row = row_of(&['a' as u32, 0, 0]);
        row.wrapped = true;
        row.trim_invisible_tail(|_| false);
        assert_eq!(row.cells.len(), 3);
        let mut row = row_of(&['a' as u32, 0, 0]);
        row.trim_invisible_tail(|_| true);
        assert_eq!(row.cells.len(), 3, "a visible erase colour is content");
    }
}
