//! The terminal state machine behind [`super::ScreenReplay`].
//!
//! Every operation is modelled on the VTE 0.76 function that implements it
//! (named in the doc comments), because the point of the replay is to rebuild
//! exactly what the live VTE showed. The deliberate differences are listed in
//! the module docs of [`super`]; anything VTE does that is not listed here
//! and not listed there (DECSLRM, bidi, sixel, colour palette changes) has no
//! effect on the text and attributes a finished card shows.

use super::grid::{
    Cell, Row, SavedCursor, Screen, CLUSTER, FRAGMENT, TAB, TAB_WIDTH_MAX, VALUE_MASK, WIDE,
};
use super::parser::{Params, Perform};
use super::pen::{apply_sgr, Color, Pen};
use super::tables::{CompactRoots, Tables};
use unicode_width::UnicodeWidthChar;

const NORMAL: usize = 0;
const ALTERNATE: usize = 1;

/// VTE's `line_drawing_map`: DEC Special Graphics for `_` (0x5f) to `~`.
const LINE_DRAWING: [char; 32] = [
    ' ', '◆', '▒', '␉', '␌', '␍', '␊', '°', '±', '␤', '␋', '┘', '┐', '┌', '└', '┼', '⎺', '⎻', '─',
    '⎼', '⎽', '├', '┤', '┴', '┬', '│', '≤', '≥', 'π', '≠', '£', '·',
];

/// Longest base-plus-combining-marks cluster kept in one cell, in bytes.
const MAX_CLUSTER_BYTES: usize = 64;

/// Longest OSC 8 `id=` VTE accepts (`VTE_HYPERLINK_ID_LENGTH_MAX`).
const HYPERLINK_ID_MAX: usize = 250;
/// Longest OSC 8 URI VTE accepts (`VTE_HYPERLINK_URI_LENGTH_MAX`).
const HYPERLINK_URI_MAX: usize = 2083;

/// Columns a printed character occupies, as VTE's `_vte_unichar_width` with
/// the default (narrow) ambiguous width. The table is the same `unicode-width`
/// one [`crate::char_width::cached_char_width`] caches for UI code; the
/// replay calls it directly because a per-character thread-local LRU costs
/// more than the table lookup it would save here.
fn char_width(c: char) -> usize {
    if (c as u32) < 0x80 {
        return 1;
    }
    UnicodeWidthChar::width(c).unwrap_or(0).min(2)
}

pub(super) struct Emulator {
    pub(super) cols: usize,
    rows: usize,
    /// Normal and alternate screen; only the normal one is ever output.
    pub(super) screens: [Screen; 2],
    active: usize,
    /// Current attributes (VTE's `m_defaults`, hyperlink included).
    pen: Pen,
    /// Interned id of `pen`, dropped whenever it changes.
    pen_id: Option<u32>,
    /// Interned id of `pen.erase()` (VTE's `m_color_defaults`).
    erase_id: Option<u32>,
    pub(super) tables: Tables,
    /// DECSTBM margins, inclusive visible rows.
    top: usize,
    bottom: usize,
    autowrap: bool,
    origin: bool,
    insert: bool,
    /// DECLRMM: `CSI s` is DECSLRM (not implemented) instead of SCOSC.
    lr_margins: bool,
    /// G0/G1 designated as DEC Special Graphics.
    charsets: [bool; 2],
    /// SO (LS1) active: G1 is in use.
    shift_out: bool,
    tabstops: Vec<bool>,
    /// REP repeats this (VTE's `m_last_graphic_character`, pre-charset).
    last_graphic: Option<char>,
    budget: usize,
    pub(super) head_dropped: bool,
}

impl Emulator {
    pub(super) fn new(cols: usize, rows: usize, budget: usize) -> Emulator {
        Emulator {
            cols,
            rows,
            screens: [Screen::new(rows, true), Screen::new(rows, false)],
            active: NORMAL,
            pen: Pen::default(),
            pen_id: None,
            erase_id: None,
            tables: Tables::default(),
            top: 0,
            bottom: rows - 1,
            autowrap: true,
            origin: false,
            insert: false,
            lr_margins: false,
            charsets: [false; 2],
            shift_out: false,
            tabstops: default_tabstops(cols),
            last_graphic: None,
            budget,
            head_dropped: false,
        }
    }

    fn scr(&mut self) -> &mut Screen {
        &mut self.screens[self.active]
    }

    /// The cursor column with a pending wrap folded back onto the last
    /// column (VTE's `get_xterm_cursor_column`).
    fn xterm_col(&self) -> usize {
        self.screens[self.active].col.min(self.cols - 1)
    }

    /// VTE's `maybe_retreat_cursor`: leave the pending-wrap position.
    fn retreat(&mut self) {
        let last = self.cols - 1;
        let screen = self.scr();
        screen.col = screen.col.min(last);
    }

    fn pen_id(&mut self) -> u32 {
        match self.pen_id {
            Some(id) => id,
            None => {
                let id = self.tables.intern_pen(&self.pen);
                self.pen_id = Some(id);
                id
            }
        }
    }

    /// The cell an erase leaves behind (VTE's `m_color_defaults`).
    fn erase_cell(&mut self) -> Cell {
        let id = match self.erase_id {
            Some(id) => id,
            None => {
                let id = self.tables.intern_pen(&self.pen.erase());
                self.erase_id = Some(id);
                id
            }
        };
        Cell::blank(id)
    }

    /// The erase cell when the background is not the default one. VTE only
    /// pads rows out to the full width in that case (`not_default_bg`).
    fn bg_fill(&mut self) -> Option<Cell> {
        (self.pen.bg != Color::Default).then(|| self.erase_cell())
    }

    fn set_pen(&mut self, pen: Pen) {
        self.pen = pen;
        self.pen_id = None;
        self.erase_id = None;
    }

    /// Rebuilds the interning tables once one has doubled. Called only at
    /// the start of a parser callback, so no interned id is held across it.
    fn maybe_compact(&mut self) {
        if !self.tables.needs_compaction() {
            return;
        }
        let [normal, alternate] = &mut self.screens;
        let roots = CompactRoots {
            pens: vec![
                &mut self.pen,
                &mut normal.saved.pen,
                &mut alternate.saved.pen,
            ],
        };
        self.tables
            .compact(&mut [&mut normal.lines, &mut alternate.lines], roots);
        self.pen_id = None;
        self.erase_id = None;
    }

    // ---- scrolling -----------------------------------------------------

    /// A row that scrolls in. `fill` is VTE's bce: explicit scrolls paint the
    /// new row with a non-default background, autowrap does not.
    fn new_row(&mut self, fill: bool) -> Row {
        match fill.then(|| self.bg_fill()).flatten() {
            Some(cell) => Row::filled(self.cols, cell),
            None => Row::default(),
        }
    }

    /// VTE's `set_hard_wrapped`; row `-1` is the last history row.
    fn set_hard_wrapped(&mut self, row: isize) {
        let screen = self.scr();
        let base = screen.history_len();
        if row < 0 {
            if base > 0 {
                screen.lines[base - 1].wrapped = false;
            }
        } else if (row as usize) < screen.rows {
            screen.lines[base + row as usize].wrapped = false;
        }
    }

    /// VTE's `scroll_text_up` over rows `top..=bottom`. With the region
    /// anchored at the top of the screen the rows scrolled out go to the
    /// history — that is how codex's `insert_history` keeps its transcript
    /// in a plain terminal — otherwise they are dropped.
    fn scroll_up(&mut self, top: usize, bottom: usize, amount: usize, fill: bool) {
        let amount = amount.clamp(1, bottom - top + 1);
        if top == 0 {
            if bottom != self.rows - 1 {
                self.set_hard_wrapped(bottom as isize);
            }
            let first = self.scr().history_len();
            for k in 0..amount {
                let row = self.new_row(fill);
                self.scr().lines.insert(first + bottom + 1 + k, row);
            }
            self.freeze_into_history(first, amount);
        } else {
            self.set_hard_wrapped(top as isize - 1);
            self.set_hard_wrapped(bottom as isize);
            for _ in 0..amount {
                let row = self.new_row(fill);
                let screen = self.scr();
                let base = screen.history_len();
                screen.lines.remove(base + top);
                screen.lines.insert(base + bottom, row);
            }
        }
    }

    /// VTE's `scroll_text_down` over rows `top..=bottom` (never touches the
    /// history).
    fn scroll_down(&mut self, top: usize, bottom: usize, amount: usize, fill: bool) {
        let amount = amount.clamp(1, bottom - top + 1);
        for _ in 0..amount {
            let row = self.new_row(fill);
            let screen = self.scr();
            let base = screen.history_len();
            screen.lines.remove(base + bottom);
            screen.lines.insert(base + top, row);
        }
        self.set_hard_wrapped(top as isize - 1);
        self.set_hard_wrapped(bottom as isize);
    }

    /// Accounts for the `count` rows starting at `first` that just scrolled
    /// off the top of the screen, then enforces the cell budget.
    fn freeze_into_history(&mut self, first: usize, count: usize) {
        let screen = &mut self.screens[self.active];
        if !screen.keeps_history {
            let history = screen.history_len();
            screen.lines.drain(..history);
            return;
        }
        let tables = &self.tables;
        for row in screen.lines.range_mut(first..first + count) {
            row.trim_invisible_tail(|pen| tables.pen(pen).blank_is_visible());
            screen.history_cells += row.cells.len();
        }
        self.enforce_budget();
    }

    /// Evicts the oldest history rows while history plus a full screen would
    /// exceed the budget. The input is never cut short: a long session keeps
    /// its newest output (the final answer, the resume hint) and loses its
    /// head, like a terminal whose scrollback limit was reached.
    fn enforce_budget(&mut self) {
        let screen_cells = self.rows * self.cols;
        let screen = &mut self.screens[NORMAL];
        while screen.history_len() > 0 && screen.history_cells + screen_cells > self.budget {
            if let Some(row) = screen.lines.pop_front() {
                screen.history_cells -= row.cells.len();
                self.head_dropped = true;
            }
        }
    }

    /// VTE's `cursor_down_with_scrolling`.
    fn cursor_down_with_scrolling(&mut self, fill: bool) {
        let row = self.scr().row;
        if row == self.bottom {
            self.scroll_up(self.top, self.bottom, 1, fill);
        } else if row + 1 < self.rows {
            self.scr().row += 1;
        }
    }

    /// VTE's `cursor_up_with_scrolling`.
    fn cursor_up_with_scrolling(&mut self, fill: bool) {
        let row = self.scr().row;
        if row == self.top {
            self.scroll_down(self.top, self.bottom, 1, fill);
        } else if row > 0 {
            self.scr().row -= 1;
        }
    }

    /// LF, VT, FF and IND (VTE's `line_feed`).
    fn line_feed(&mut self) {
        self.retreat();
        self.scr().cursor_row_mut().touched = true;
        self.cursor_down_with_scrolling(true);
    }

    fn next_line(&mut self) {
        self.scr().cursor_row_mut().touched = true;
        self.cursor_down_with_scrolling(true);
        self.scr().col = 0;
    }

    fn reverse_index(&mut self) {
        self.retreat();
        self.cursor_up_with_scrolling(true);
    }

    // ---- printing ------------------------------------------------------

    /// VTE's autowrap branch of `insert_char`.
    fn autowrap_newline(&mut self) {
        let screen = self.scr();
        screen.col = 0;
        let row = screen.cursor_row_mut();
        row.wrapped = true;
        row.touched = true;
        self.cursor_down_with_scrolling(false);
    }

    /// Printable ASCII in bulk: the same result as one `insert_char` per
    /// byte, without the per-character bookkeeping.
    fn print_ascii_run(&mut self, run: &[u8]) {
        if self.insert || self.charsets[usize::from(self.shift_out)] {
            for &byte in run {
                self.print_char(char::from(byte));
            }
            return;
        }
        let Some(&last) = run.last() else {
            return;
        };
        self.last_graphic = Some(char::from(last));
        let pen = self.pen_id();
        let cols = self.cols;
        let mut rest = run;
        while !rest.is_empty() {
            if self.scr().col >= cols {
                if self.autowrap {
                    self.autowrap_newline();
                } else {
                    // Without DECAWM every further character overwrites
                    // the last column, so only the final one survives.
                    self.scr().col = cols - 1;
                    rest = &rest[rest.len() - 1..];
                }
            }
            let screen = &mut self.screens[self.active];
            let col = screen.col;
            let n = rest.len().min(cols - col);
            let row = screen.cursor_row_mut();
            row.cleanup_fragments(col, col + n);
            row.fill_to(col);
            let overwrite = row.cells.len().saturating_sub(col).min(n);
            for (cell, &byte) in row.cells[col..col + overwrite].iter_mut().zip(rest) {
                *cell = Cell {
                    code: u32::from(byte),
                    pen,
                };
            }
            row.cells
                .extend(rest[overwrite..n].iter().map(|&byte| Cell {
                    code: u32::from(byte),
                    pen,
                }));
            row.touched = true;
            screen.col = col + n;
            rest = &rest[n..];
        }
    }

    /// VTE's `insert_char`.
    fn print_char(&mut self, c: char) {
        let unmapped = c;
        let c = if self.charsets[usize::from(self.shift_out)] && ('_'..='~').contains(&c) {
            LINE_DRAWING[c as usize - 0x5f]
        } else {
            c
        };
        let width = char_width(c);
        if width == 0 {
            self.combine(c);
            return;
        }
        self.last_graphic = Some(unmapped);
        let cols = self.cols;
        if self.scr().col + width > cols {
            if self.autowrap {
                self.autowrap_newline();
            } else {
                self.scr().col = cols - width;
            }
        }
        let pen = self.pen_id();
        let code = if width == 2 {
            WIDE | c as u32
        } else {
            c as u32
        };
        let insert = self.insert;
        let screen = &mut self.screens[self.active];
        let col = screen.col;
        let row = screen.cursor_row_mut();
        if insert {
            shift_right(row, col, cols - 1, width, Cell::BLANK);
        } else {
            row.cleanup_fragments(col, col + width);
            row.fill_to(col + width);
        }
        row.cells[col] = Cell { code, pen };
        if width == 2 {
            row.cells[col + 1] = Cell {
                code: FRAGMENT,
                pen,
            };
        }
        if row.cells.len() > cols {
            let len = row.cells.len();
            row.cleanup_fragments(cols, len);
            row.cells.truncate(cols);
        }
        row.touched = true;
        screen.col = col + width;
    }

    /// A zero-width character joins the cell before the cursor (or the end
    /// of the previous row when that row soft-wrapped), as in VTE.
    fn combine(&mut self, mark: char) {
        let screen = &self.screens[self.active];
        let mut line = screen.history_len() + screen.row;
        let mut col = screen.col;
        if col == 0 {
            if line == 0 || !screen.lines[line - 1].wrapped {
                return;
            }
            line -= 1;
            col = screen.lines[line].cells.len();
        }
        if col == 0 {
            return;
        }
        let cells = &screen.lines[line].cells;
        col -= 1;
        let Some(mut cell) = cells.get(col).copied() else {
            return;
        };
        while cell.is_fragment() && col > 0 {
            col -= 1;
            cell = cells[col];
        }
        if cell.is_fragment() || cell.is_tab_head() {
            return;
        }
        let mut text = cell_text(&self.tables, cell);
        if text.len() >= MAX_CLUSTER_BYTES {
            // A stream of marks on one cell would otherwise intern ever
            // longer strings (quadratic); nothing legible needs more.
            return;
        }
        text.push(mark);
        let id = self.tables.intern_cluster(text);
        self.screens[self.active].lines[line].cells[col].code = (cell.code & WIDE) | CLUSTER | id;
    }

    // ---- cursor movement ------------------------------------------------

    /// VTE's `set_cursor_column` (0-based; DECOM has no effect on columns
    /// without DECSLRM).
    fn set_cursor_col(&mut self, col: i32) {
        let last = self.cols - 1;
        self.scr().col = (col.max(0) as usize).min(last);
    }

    /// VTE's `set_cursor_row` (0-based, relative to the margins under DECOM).
    fn set_cursor_row(&mut self, row: i32) {
        let (top, bottom) = if self.origin {
            (self.top, self.bottom)
        } else {
            (0, self.rows - 1)
        };
        self.scr().row = (row.max(0) as usize + top).min(bottom);
    }

    fn home(&mut self) {
        self.set_cursor_col(0);
        self.set_cursor_row(0);
    }

    fn move_up(&mut self, count: i32) {
        let count = count.clamp(1, self.rows as i32) as usize;
        self.retreat();
        let top = if self.scr().row >= self.top {
            self.top
        } else {
            0
        };
        let screen = self.scr();
        screen.row = screen.row.saturating_sub(count).max(top);
    }

    fn move_down(&mut self, count: i32) {
        let count = count.clamp(1, self.rows as i32) as usize;
        self.retreat();
        let bottom = if self.scr().row <= self.bottom {
            self.bottom
        } else {
            self.rows - 1
        };
        let screen = self.scr();
        screen.row = (screen.row + count).min(bottom);
    }

    fn move_forward(&mut self, count: i32) {
        let count = count.clamp(1, self.cols as i32) as usize;
        self.retreat();
        let last = self.cols - 1;
        let screen = self.scr();
        screen.col = (screen.col + count).min(last);
    }

    fn move_backward(&mut self, count: i32) {
        let count = count.clamp(1, self.cols as i32) as usize;
        self.retreat();
        let screen = self.scr();
        screen.col = screen.col.saturating_sub(count);
    }

    /// VTE's `move_cursor_tab_forward`, including its "smart tab": when
    /// nothing follows the cursor on the row, the skipped columns become one
    /// copyable `\t` cell instead of blanks.
    fn tab_forward(&mut self, count: i32) {
        if count <= 0 {
            return;
        }
        let col = self.xterm_col();
        if col < self.scr().col {
            // A pending wrap: a tab neither wraps nor snaps the cursor back.
            return;
        }
        let stop = self.cols - 1;
        let mut next = col;
        for _ in 0..count {
            match (next + 1..=stop).find(|&c| self.tabstops[c]) {
                Some(c) => next = c,
                None => {
                    next = stop;
                    break;
                }
            }
        }
        if next == col {
            return;
        }
        let row = self.scr().cursor_row_mut();
        let old_len = row.cells.len();
        row.fill_to(next);
        if col >= old_len && next - col <= TAB_WIDTH_MAX {
            row.cells[col] = Cell { code: TAB, pen: 0 };
            for cell in &mut row.cells[col + 1..next] {
                *cell = Cell {
                    code: FRAGMENT | TAB,
                    pen: 0,
                };
            }
        }
        self.scr().col = next;
    }

    fn tab_backward(&mut self, count: i32) {
        if count <= 0 {
            return;
        }
        let mut col = self.xterm_col();
        for _ in 0..count {
            match (0..col).rev().find(|&c| self.tabstops[c]) {
                Some(c) => col = c,
                None => {
                    col = 0;
                    break;
                }
            }
        }
        self.scr().col = col;
    }

    // ---- erasing and editing --------------------------------------------

    /// EL 0 (VTE's `clear_to_eol`, which deliberately keeps a pending wrap).
    fn clear_to_eol(&mut self) {
        let cols = self.cols;
        let fill = self.bg_fill();
        let screen = self.scr();
        let col = screen.col;
        let row = screen.cursor_row_mut();
        row.fill_to(col);
        if row.cells.len() > col {
            let len = row.cells.len();
            row.cleanup_fragments(col, len);
            row.cells.truncate(col);
        }
        if let Some(cell) = fill {
            row.fill_with(cols, cell);
        }
        row.wrapped = false;
    }

    /// EL 1 (VTE's `clear_to_bol`), inclusive of the cursor cell.
    fn clear_to_bol(&mut self) {
        self.retreat();
        let erase = self.erase_cell();
        let screen = self.scr();
        let col = screen.col;
        let row = screen.cursor_row_mut();
        row.cleanup_fragments(0, col + 1);
        row.fill_with(col + 1, erase);
        row.cells[..=col].fill(erase);
    }

    /// EL 2 (VTE's `clear_current_line`).
    fn clear_line(&mut self) {
        self.retreat();
        let cols = self.cols;
        let erase = self.erase_cell();
        let row = self.scr().cursor_row_mut();
        row.cells.clear();
        row.fill_with(cols, erase);
        row.wrapped = false;
    }

    /// ED 0 (VTE's `clear_below_current`).
    fn clear_below(&mut self) {
        self.retreat();
        let (cols, rows) = (self.cols, self.rows);
        let fill = self.bg_fill();
        let screen = self.scr();
        let (cur, col) = (screen.row, screen.col);
        let row = screen.row_mut(cur);
        if row.cells.len() > col {
            let len = row.cells.len();
            row.cleanup_fragments(col, len);
            row.cells.truncate(col);
        }
        for r in cur..rows {
            let row = screen.row_mut(r);
            if r > cur {
                row.cells.clear();
            }
            if let Some(cell) = fill {
                row.fill_with(cols, cell);
            }
            row.wrapped = false;
        }
    }

    /// ED 1 (VTE's `clear_above_current` followed by `clear_to_bol`).
    fn clear_above(&mut self) {
        let cols = self.cols;
        let erase = self.erase_cell();
        self.set_hard_wrapped(-1);
        let screen = self.scr();
        for r in 0..screen.row {
            let row = screen.row_mut(r);
            row.cells.clear();
            row.fill_with(cols, erase);
            row.wrapped = false;
        }
        self.clear_to_bol();
    }

    /// ED 2. DELIBERATE DIVERGENCE from VTE, whose `clear_screen` scrolls
    /// the whole screen into the history: here the visible rows are blanked
    /// in place. A `top`/`watch`-style loop (`CSI H CSI 2J` + frame, forever)
    /// would otherwise stack every frame into the finished block. The cursor
    /// stays where it was, as in VTE.
    fn clear_screen(&mut self) {
        self.retreat();
        let cols = self.cols;
        let fill = self.bg_fill();
        self.set_hard_wrapped(-1);
        let screen = self.scr();
        for r in 0..screen.rows {
            let row = screen.row_mut(r);
            row.cells.clear();
            row.wrapped = false;
            row.touched = false;
            if let Some(cell) = fill {
                row.fill_with(cols, cell);
            }
        }
    }

    /// ED 3: VTE drops the normal screen's scrollback whichever screen is
    /// active.
    fn clear_history(&mut self) {
        self.screens[NORMAL].clear_history();
    }

    /// ECH (VTE's `erase_characters`).
    fn erase_chars(&mut self, count: i32) {
        self.retreat();
        let cols = self.cols;
        let erase = self.erase_cell();
        let screen = self.scr();
        let col = screen.col;
        let count = (count.max(1) as usize).min(cols - col);
        let row = screen.cursor_row_mut();
        row.cleanup_fragments(col, col + count);
        row.fill_to(col);
        row.fill_with(col + count, erase);
        row.cells[col..col + count].fill(erase);
    }

    /// ICH.
    fn insert_chars(&mut self, count: i32) {
        self.retreat();
        let last = self.cols - 1;
        let erase = self.erase_cell();
        let screen = self.scr();
        let col = screen.col;
        shift_right(
            screen.cursor_row_mut(),
            col,
            last,
            count.max(1) as usize,
            erase,
        );
    }

    /// DCH.
    fn delete_chars(&mut self, count: i32) {
        self.retreat();
        let last = self.cols - 1;
        let erase = self.erase_cell();
        let screen = self.scr();
        let col = screen.col;
        shift_left(
            screen.cursor_row_mut(),
            col,
            last,
            count.max(1) as usize,
            erase,
        );
    }

    /// IL: only inside the margins; homes the column.
    fn insert_lines(&mut self, count: i32) {
        let row = self.scr().row;
        if row < self.top || row > self.bottom {
            return;
        }
        self.scr().col = 0;
        self.scroll_down(row, self.bottom, count.max(1) as usize, true);
    }

    /// DL. Like VTE, deleting from row 0 of an unrestricted region sends the
    /// deleted rows to the history.
    fn delete_lines(&mut self, count: i32) {
        let row = self.scr().row;
        if row < self.top || row > self.bottom {
            return;
        }
        self.scr().col = 0;
        self.scroll_up(row, self.bottom, count.max(1) as usize, true);
    }

    fn repeat(&mut self, params: &Params) {
        let Some(c) = self.last_graphic else {
            return;
        };
        let room = (self.cols - self.scr().col) as i32;
        for _ in 0..params.collect1(0, 1, 1, room) {
            self.print_char(c);
        }
    }

    // ---- state -------------------------------------------------------

    fn save_cursor(&mut self) {
        let (pen, origin, charsets, shift_out) =
            (self.pen, self.origin, self.charsets, self.shift_out);
        let screen = self.scr();
        screen.saved = SavedCursor {
            row: screen.row,
            col: screen.col,
            pen,
            origin,
            charsets,
            shift_out,
        };
    }

    fn restore_cursor(&mut self) {
        let last_row = self.rows - 1;
        let screen = self.scr();
        let saved = screen.saved;
        screen.col = saved.col;
        screen.row = saved.row.min(last_row);
        self.origin = saved.origin;
        self.charsets = saved.charsets;
        self.shift_out = saved.shift_out;
        self.set_pen(saved.pen);
    }

    /// VTE's `switch_screen`: the cursor position carries over, the current
    /// hyperlink does not.
    fn switch_screen(&mut self, to: usize) {
        let (row, col) = (self.scr().row, self.scr().col);
        self.active = to;
        let screen = self.scr();
        screen.row = row;
        screen.col = col;
        if self.pen.link != 0 {
            let mut pen = self.pen;
            pen.link = 0;
            self.set_pen(pen);
        }
    }

    /// DECSET/DECRST 47, 1047 and 1049 (VTE's `set_mode_private`).
    fn alternate_screen(&mut self, mode: i32, set: bool) {
        if set {
            if mode == 1049 {
                self.save_cursor();
            }
            self.switch_screen(ALTERNATE);
            if mode == 1049 {
                self.clear_screen();
            }
        } else {
            if mode == 1047 && self.active == ALTERNATE {
                self.clear_screen();
            }
            self.switch_screen(NORMAL);
            if mode == 1049 {
                self.restore_cursor();
            }
        }
    }

    fn set_private_modes(&mut self, params: &Params, set: bool) {
        let mut i = 0;
        while i < params.len() {
            match params.raw(i) {
                6 => {
                    self.origin = set;
                    self.home();
                }
                7 => self.autowrap = set,
                69 => self.lr_margins = set,
                mode @ (47 | 1047 | 1049) => self.alternate_screen(mode, set),
                1048 => {
                    if set {
                        self.save_cursor();
                    } else {
                        self.restore_cursor();
                    }
                }
                _ => {}
            }
            i = params.next(i);
        }
    }

    fn set_ansi_modes(&mut self, params: &Params, set: bool) {
        let mut i = 0;
        while i < params.len() {
            if params.raw(i) == 4 {
                self.insert = set;
            }
            i = params.next(i);
        }
    }

    /// DECSTBM: clamped like VTE, ignored unless `bottom > top`, homes the
    /// cursor.
    fn set_margins(&mut self, params: &Params) {
        let rows = self.rows as i32;
        let top = params.collect1(0, 1, 1, rows);
        let bottom = params.collect1(params.next(0), rows, 1, rows);
        if bottom <= top {
            return;
        }
        self.top = (top - 1) as usize;
        self.bottom = (bottom - 1) as usize;
        self.home();
    }

    /// The mode/attribute part of VTE's `reset` shared by RIS and DECSTR.
    fn reset_modes(&mut self) {
        self.autowrap = true;
        self.origin = false;
        self.insert = false;
        self.lr_margins = false;
        self.charsets = [false; 2];
        self.shift_out = false;
        self.top = 0;
        self.bottom = self.rows - 1;
        self.set_pen(Pen::default());
    }

    /// DECSTR: modes and attributes only; the screens stay, and both saved
    /// cursors are re-seeded from the current state.
    fn soft_reset(&mut self) {
        self.reset_modes();
        let active = self.active;
        for index in [NORMAL, ALTERNATE] {
            self.active = index;
            self.save_cursor();
        }
        self.active = active;
    }

    /// RIS: everything, including both screens and the history.
    fn hard_reset(&mut self) {
        self.reset_modes();
        self.screens = [Screen::new(self.rows, true), Screen::new(self.rows, false)];
        self.active = NORMAL;
        self.tabstops = default_tabstops(self.cols);
        self.last_graphic = None;
    }

    /// DECALN (VTE's `DECALN`): resets the margins, homes the cursor and
    /// fills the whole screen with `E` in the default attributes.
    fn screen_alignment(&mut self) {
        self.top = 0;
        self.bottom = self.rows - 1;
        let cols = self.cols;
        let screen = self.scr();
        for r in 0..screen.rows {
            let row = screen.row_mut(r);
            *row = Row::filled(
                cols,
                Cell {
                    code: 'E' as u32,
                    pen: 0,
                },
            );
            row.touched = true;
        }
        self.home();
    }

    fn set_hyperlink(&mut self, rest: &[u8]) {
        // `8;params;uri` arrives here as `params;uri`. Like VTE, a string
        // without the URI field changes nothing, an empty or overlong URI
        // ends the current link, and an overlong `id=` falls back to an
        // anonymous link.
        let Some(split) = rest.iter().position(|&b| b == b';') else {
            return;
        };
        let (params, uri) = (&rest[..split], &rest[split + 1..]);
        let link = if uri.is_empty() || uri.len() > HYPERLINK_URI_MAX {
            0
        } else {
            let id = params
                .split(|&b| b == b':')
                .find_map(|p| p.strip_prefix(b"id="))
                .filter(|id| !id.is_empty() && id.len() <= HYPERLINK_ID_MAX);
            let uri = String::from_utf8_lossy(uri);
            let text = match id {
                Some(id) => format!("id={};{uri}", String::from_utf8_lossy(id)),
                None => format!(";{uri}"),
            };
            self.tables.intern_link(text)
        };
        let mut pen = self.pen;
        pen.link = link;
        self.set_pen(pen);
    }
}

fn default_tabstops(cols: usize) -> Vec<bool> {
    (0..cols).map(|c| c % 8 == 0).collect()
}

/// The text a cell holds (a space for a never-written cell).
pub(super) fn cell_text(tables: &Tables, cell: Cell) -> String {
    if cell.code & CLUSTER != 0 && !cell.is_fragment() {
        tables.cluster(cell.code & VALUE_MASK).to_string()
    } else {
        match cell.code & VALUE_MASK {
            0 => " ".to_string(),
            code => char::from_u32(code).unwrap_or('\u{fffd}').to_string(),
        }
    }
}

/// VTE's `scroll_text_right` on one row (ICH, and IRM printing).
fn shift_right(row: &mut Row, left: usize, right: usize, amount: usize, fill: Cell) {
    let amount = amount.clamp(1, right - left + 1);
    row.fill_to(right + 1);
    row.cleanup_fragments(left, left);
    row.cleanup_fragments(right + 1 - amount, right + 1);
    row.cells
        .copy_within(left..right + 1 - amount, left + amount);
    row.cells[left..left + amount].fill(fill);
}

/// VTE's `scroll_text_left` on one row (DCH), which also ends a soft wrap.
fn shift_left(row: &mut Row, left: usize, right: usize, amount: usize, fill: Cell) {
    let amount = amount.clamp(1, right - left + 1);
    row.fill_to(right + 1);
    row.cleanup_fragments(left, left + amount);
    row.cleanup_fragments(right + 1, right + 1);
    row.cells.copy_within(left + amount..right + 1, left);
    row.cells[right + 1 - amount..=right].fill(fill);
    row.wrapped = false;
}

impl Perform for Emulator {
    fn print_ascii(&mut self, run: &[u8]) {
        self.maybe_compact();
        self.print_ascii_run(run);
    }

    fn print(&mut self, c: char) {
        self.maybe_compact();
        self.print_char(c);
    }

    fn execute(&mut self, control: u32) {
        self.maybe_compact();
        match control {
            0x08 => self.move_backward(1),
            0x09 => self.tab_forward(1),
            0x0a..=0x0c | 0x84 => self.line_feed(),
            0x0d => self.scr().col = 0,
            0x0e => self.shift_out = true,
            0x0f => self.shift_out = false,
            0x85 => self.next_line(),
            0x88 => {
                let col = self.xterm_col();
                self.tabstops[col] = true;
            }
            0x8d => self.reverse_index(),
            _ => {}
        }
    }

    fn csi(&mut self, params: &Params, prefix: u8, intermediate: u8, fin: u8) {
        self.maybe_compact();
        let (rows, cols) = (self.rows as i32, self.cols as i32);
        let count = || params.collect1(0, 1, i32::MIN, i32::MAX);
        match (prefix, intermediate, fin) {
            (0, 0, b'@') => self.insert_chars(count()),
            (0, 0, b'A') => self.move_up(count()),
            (0, 0, b'B') => self.move_down(count()),
            (0, 0, b'C') => self.move_forward(count()),
            (0, 0, b'D') => self.move_backward(count()),
            (0, 0, b'E') => {
                self.scr().col = 0;
                self.move_down(count());
            }
            (0, 0, b'F') => {
                self.scr().col = 0;
                self.move_up(count());
            }
            (0, 0, b'G' | b'`') => self.set_cursor_col(params.collect1(0, 1, 1, cols) - 1),
            (0, 0, b'H' | b'f') => {
                let row = params.collect1(0, 1, 1, rows);
                let col = params.collect1(params.next(0), 1, 1, cols);
                self.set_cursor_col(col - 1);
                self.set_cursor_row(row - 1);
            }
            (0, 0, b'I') => self.tab_forward(count()),
            (0 | b'?', 0, b'J') => match params.raw(0) {
                -1 | 0 => self.clear_below(),
                1 => self.clear_above(),
                2 => self.clear_screen(),
                3 => self.clear_history(),
                _ => {}
            },
            (0 | b'?', 0, b'K') => match params.raw(0) {
                -1 | 0 => self.clear_to_eol(),
                1 => self.clear_to_bol(),
                2 => self.clear_line(),
                _ => {}
            },
            (0, 0, b'L') => self.insert_lines(count()),
            (0, 0, b'M') => self.delete_lines(count()),
            (0, 0, b'P') => self.delete_chars(count()),
            (0, 0, b'S') => self.scroll_up(self.top, self.bottom, count().max(1) as usize, true),
            // With five parameters `CSI T` is xterm's mouse highlight, not SD.
            (0, 0, b'T') if params.len() < 5 => {
                self.scroll_down(self.top, self.bottom, count().max(1) as usize, true)
            }
            (0, 0, b'X') => self.erase_chars(count()),
            (0, 0, b'Z') => self.tab_backward(count()),
            // HPR and VPR are accepted but do nothing in VTE 0.76 (their
            // handlers are compiled out), so the replay ignores them too.
            (0, 0, b'a' | b'e') => {}
            (0, 0, b'b') => self.repeat(params),
            (0, 0, b'd') => {
                self.retreat();
                self.set_cursor_row(params.collect1(0, 1, 1, rows) - 1);
            }
            (0, 0, b'g') => match params.raw(0) {
                -1 | 0 => {
                    let col = self.xterm_col();
                    self.tabstops[col] = false;
                }
                2 | 3 | 5 => self.tabstops.fill(false),
                _ => {}
            },
            (0, 0, b'h') => self.set_ansi_modes(params, true),
            (0, 0, b'l') => self.set_ansi_modes(params, false),
            (b'?', 0, b'h') => self.set_private_modes(params, true),
            (b'?', 0, b'l') => self.set_private_modes(params, false),
            (0, 0, b'm') => {
                let mut pen = self.pen;
                apply_sgr(&mut pen, params);
                self.set_pen(pen);
            }
            (0, 0, b'r') => self.set_margins(params),
            (0, 0, b's') if !self.lr_margins => self.save_cursor(),
            (0, 0, b'u') => self.restore_cursor(),
            (0, b'!', b'p') => self.soft_reset(),
            _ => {}
        }
    }

    fn esc(&mut self, intermediate: u8, fin: u8) {
        self.maybe_compact();
        match (intermediate, fin) {
            (0, b'7') => self.save_cursor(),
            (0, b'8') => self.restore_cursor(),
            (0, b'D') => self.line_feed(),
            (0, b'E') => self.next_line(),
            (0, b'H') => {
                let col = self.xterm_col();
                self.tabstops[col] = true;
            }
            (0, b'M') => self.reverse_index(),
            (0, b'c') => self.hard_reset(),
            (b'#', b'8') => self.screen_alignment(),
            (b'(', charset) => self.charsets[0] = charset == b'0',
            (b')', charset) => self.charsets[1] = charset == b'0',
            _ => {}
        }
    }

    fn osc(&mut self, payload: &[u8]) {
        self.maybe_compact();
        if let Some(rest) = payload.strip_prefix(b"8;") {
            self.set_hyperlink(rest);
        }
    }
}
