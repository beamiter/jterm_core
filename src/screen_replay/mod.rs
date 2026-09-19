//! Rebuild what a terminal showed from the raw PTY bytes of one command.
//!
//! A finished Block card is rebuilt from the bytes its command wrote, long
//! after the live VTE that displayed them has been reset. Inline TUIs such as
//! codex address that live screen absolutely (`CUP`), confine scrolling to a
//! region (`DECSTBM`), scroll it backwards (`RI`) and insert their transcript
//! above the viewport by line-feeding at the bottom of a top-anchored region
//! — which a real terminal turns into scrollback. A replay without a screen
//! height, margins or reverse index rewrites every batch of that history at
//! the same absolute rows and loses everything after the first screenful.
//! This module is a small terminal emulator that models exactly the parts of
//! VTE 0.76 that decide the text and attributes of the result, so the card
//! matches what the user saw scroll by.
//!
//! # Model
//!
//! * A screen of `rows` × `cols` cells over a bounded history (scrollback),
//!   plus an alternate screen (DECSET 47/1047/1049) that has no history and is
//!   never part of the result.
//! * A cursor with DECAWM's pending-wrap state (printing into the last column
//!   parks the cursor; the next printable character wraps and marks the row
//!   soft-wrapped). `CUP`/`HVP`/`CUU`/`CUD`/`CUF`/`CUB`/`CNL`/`CPL`/`CHA`/
//!   `HPA`/`VPA` are clamped to the screen (DECOM honoured). `HPR`/`VPR`
//!   (`CSI a`/`CSI e`) are accepted and ignored because VTE 0.76 ignores
//!   them too (the differential test checks this).
//! * `LF`/`VT`/`FF`/`IND`/`NEL` at the bottom margin scroll the region: with
//!   the region's top at row 0 the top row moves into history (VTE's
//!   `scroll_text_up`), otherwise it is dropped. `RI` at the top margin
//!   scrolls the region down. `DECSTBM` is clamped like VTE's `collect1`,
//!   ignored unless bottom > top, and homes the cursor.
//! * `IL`/`DL`/`ICH`/`DCH`/`ECH`/`SU`/`SD`/`REP`/`HT`/`CBT`/`BS`/`CR`, tab
//!   stops (`HTS`/`TBC`), `IRM`, `DECSC`/`DECRC` and `CSI s`/`CSI u`,
//!   `DECSTR`, `RIS`, `EL 0/1/2`, `ED 0/1/2/3`.
//! * SGR per cell, including 256-colour and truecolour in both the `;` and the
//!   `:` forms, underline styles and the underline colour; OSC 8 hyperlinks
//!   per cell. Wide characters occupy two cells and combining marks join the
//!   previous cell (`unicode-width`, narrow ambiguous width, as VTE's
//!   default). DEC special graphics (`ESC ( 0`, `SO`/`SI`) are mapped.
//! * Everything else — other OSC strings, DCS/APC/PM/SOS, unknown CSI/ESC
//!   sequences, mode changes that do not affect the text — is consumed
//!   without printing anything. Invalid UTF-8 becomes U+FFFD, and a sequence
//!   or character split across two [`ScreenReplay::feed`] calls is resumed.
//!
//! # Deliberate divergence from VTE: ED 2
//!
//! VTE's `CSI 2J` scrolls the whole screen into the history before blanking
//! it. Here ED 2 blanks the screen in place and leaves the history alone, so a
//! `top`/`watch`-style loop (`CSI H CSI 2J` + a frame, forever) collapses to
//! its final frame instead of stacking every frame into the card. ED 3 clears
//! the history, as in VTE, so codex's resize answer (`CSI 2J CSI 3J` plus a
//! full replay of its transcript) also yields exactly one transcript.
//!
//! # Output
//!
//! [`ScreenReplay::finish`] returns the history followed by the screen of the
//! normal (primary) screen, with the never-touched blank rows above the first
//! used row and the blank rows at the bottom trimmed. [`Replay::to_plain`]
//! joins soft-wrapped rows the way VTE's text export does (`Copy output`,
//! Find, persistence); [`Replay::to_ansi`] serialises one line per row joined
//! by `\r\n`, with minimal SGR transitions, OSC 8 around linked cells and a
//! final reset, ready to be fed to a finished card's VTE of any width ≥ cols.
//!
//! # Budget
//!
//! Memory is bounded by a cell budget ([`DEFAULT_CELL_BUDGET`], or
//! [`ScreenReplay::with_budget`]): history rows (trimmed of invisible trailing
//! cells) plus one full screen. When the history would exceed it, the OLDEST
//! history rows are evicted and [`Replay::head_dropped`] is set — exactly like
//! a terminal whose scrollback limit was reached. Input is never discarded,
//! so a long session keeps its final answer and exit hint. Attribute,
//! hyperlink and cluster tables are compacted as they grow, so they are
//! bounded by the budget too. Throughput is tens of MiB/s (8 MiB of TUI
//! output replays in a few tens of milliseconds in release builds).
//!
//! # How an app uses it
//!
//! 1. At finish, take the command's captured normal-screen PTY bytes (the
//!    ring between prompt end and command end; alternate-screen bytes are not
//!    captured and are not needed).
//! 2. If the ring dropped its front (it is bounded), pass the bytes through
//!    [`resync_ring_head`] first so a CSI or UTF-8 fragment at the cut is not
//!    printed as text, and show an "earlier output not retained" notice.
//! 3. If [`stream_needs_screen_replay`] is false (plain line output with SGR,
//!    CR, EL and the like), the app's cheaper horizontal-only strip is
//!    equivalent and may be kept. Otherwise:
//! 4. `ScreenReplay::new(cols, rows)` with the winsize the CHILD LAST SAW
//!    (the last `TIOCSWINSZ` the app sent to the PTY), not the card's size:
//!    absolute cursor addressing and scroll regions only mean what they meant
//!    to the program at that size. Mid-command resizes need no special
//!    handling for programs that repaint on `SIGWINCH` (codex clears the
//!    screen and scrollback and replays its transcript).
//! 5. [`ScreenReplay::feed`] the bytes (any chunking), [`ScreenReplay::finish`],
//!    then use [`Replay::to_plain`] for the block's text, [`Replay::to_ansi`]
//!    for the card's VTE, and OR [`Replay::head_dropped`] into the
//!    "earlier output not retained" notice.
//!
//! The cursor starts at row 0, column 0. That is enough even though the live
//! command started lower on the screen: absolute addressing lands where it
//! did, and relative output only differs once scrolling begins, where the
//! history keeps it anyway.

mod emulator;
mod grid;
mod parser;
mod pen;
mod tables;
#[cfg(test)]
mod tests;

use emulator::{cell_text, Emulator};
use grid::{Row, FRAGMENT};
use parser::Parser;
use pen::{write_sgr_transition, Pen, SgrScratch};
use tables::Tables;

/// Default cell budget: history plus one screen. Cells are 8 bytes, so this
/// is about 16 MiB, which holds roughly 17 thousand full 120-column rows —
/// and far more of the short rows real transcripts consist of, because
/// history rows are stored without their blank tails.
pub const DEFAULT_CELL_BUDGET: usize = 2 * 1024 * 1024;

/// Geometry bounds. Anything outside is clamped; a zero-sized winsize (a PTY
/// that was never sized) becomes 1 so the emulator stays well-defined.
const MAX_COLS: usize = 4096;
const MAX_ROWS: usize = 1024;

/// An incremental replay of one command's output. See the module docs.
pub struct ScreenReplay {
    parser: Parser,
    emulator: Emulator,
}

impl ScreenReplay {
    /// A replay of a `cols` × `rows` screen with [`DEFAULT_CELL_BUDGET`].
    pub fn new(cols: usize, rows: usize) -> Self {
        Self::with_budget(cols, rows, DEFAULT_CELL_BUDGET)
    }

    /// A replay whose history plus one screen stays within `budget_cells`
    /// cells. A budget smaller than one screen keeps no history at all.
    pub fn with_budget(cols: usize, rows: usize, budget_cells: usize) -> Self {
        let cols = cols.clamp(1, MAX_COLS);
        let rows = rows.clamp(1, MAX_ROWS);
        ScreenReplay {
            parser: Parser::new(),
            emulator: Emulator::new(cols, rows, budget_cells),
        }
    }

    /// Feeds the next chunk of the stream. Chunks may split escape sequences
    /// and UTF-8 characters anywhere.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.emulator, bytes);
    }

    /// Ends the stream (an unfinished trailing sequence is dropped) and
    /// returns the normal screen's history and screen.
    pub fn finish(self) -> Replay {
        let Emulator {
            mut screens,
            tables,
            head_dropped,
            ..
        } = self.emulator;
        let normal = std::mem::replace(&mut screens[0], grid::Screen::new(0, false));
        let lines: Vec<Row> = normal.lines.into();
        let visible = |row: &Row| row_is_visible(&tables, row);
        let first = lines
            .iter()
            .position(|row| row.touched || visible(row))
            .unwrap_or(lines.len());
        let end = lines
            .iter()
            .rposition(visible)
            .map_or(first, |last| (last + 1).max(first));
        let mut rows: Vec<Row> = lines.into_iter().skip(first).take(end - first).collect();
        if let Some(last) = rows.last_mut() {
            // Nothing follows, so a soft wrap there would join with nothing.
            last.wrapped = false;
        }
        Replay {
            rows,
            tables,
            head_dropped,
        }
    }
}

/// The result of a replay: trimmed rows plus the attribute tables they use.
pub struct Replay {
    rows: Vec<Row>,
    tables: Tables,
    /// The cell budget evicted the oldest history rows.
    pub head_dropped: bool,
}

impl Replay {
    /// Number of rows in the result (history and screen, after trimming).
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// Plain text like VTE's text export: one line per logical line
    /// (soft-wrapped rows joined), trailing blanks trimmed per line, lines
    /// joined by `\n`, no trailing newline. Tabs VTE stored as tabs come back
    /// as `\t`.
    pub fn to_plain(&self) -> String {
        let mut out = String::new();
        let mut line_start = 0;
        for (index, row) in self.rows.iter().enumerate() {
            for &cell in &row.cells {
                if cell.is_fragment() {
                    continue;
                }
                if cell.is_tab_head() {
                    out.push('\t');
                } else {
                    push_cell_text(&mut out, &self.tables, cell);
                }
            }
            if row.wrapped && index + 1 < self.rows.len() {
                // A soft-wrapped row is full width: pad cells it never wrote
                // (a wide character that did not fit leaves one) are part of
                // the logical line, exactly as VTE exports them.
                continue;
            }
            trim_trailing_blanks(&mut out, line_start);
            if index + 1 < self.rows.len() {
                out.push('\n');
            }
            line_start = out.len();
        }
        out
    }

    /// The rows as terminal input: each row's visible cells, rows joined by
    /// `\r\n`, SGR emitted only where the style changes, OSC 8 opened and
    /// closed around linked cells, and a final `CSI 0 m` when a style is
    /// still active at the end. Rows never exceed the replay's width, so feeding this to a
    /// terminal at least that wide reproduces the rows one-to-one.
    pub fn to_ansi(&self) -> String {
        let mut out = String::new();
        let mut scratch = SgrScratch::default();
        let mut pen = Pen::default();
        let mut link = 0u32;
        for (index, row) in self.rows.iter().enumerate() {
            if index > 0 {
                out.push_str("\r\n");
            }
            let end = visible_len(&self.tables, row);
            for &cell in &row.cells[..end] {
                if cell.is_fragment() && !cell.is_tab_head_fragment() {
                    continue;
                }
                let cell_pen = *self.tables.pen(cell.pen);
                if cell_pen.link != link {
                    out.push_str("\x1b]8;");
                    out.push_str(if cell_pen.link == 0 {
                        ";"
                    } else {
                        self.tables.link(cell_pen.link)
                    });
                    out.push_str("\x1b\\");
                    link = cell_pen.link;
                }
                if !pen.same_style(&cell_pen) {
                    write_sgr_transition(&mut out, &mut scratch, &pen, &cell_pen);
                    pen = cell_pen;
                }
                if cell.is_tab_head() || cell.is_tab_head_fragment() {
                    out.push(' ');
                } else {
                    push_cell_text(&mut out, &self.tables, cell);
                }
            }
            if link != 0 {
                out.push_str("\x1b]8;;\x1b\\");
                link = 0;
            }
            if pen.bg != pen::Color::Default || pen.flags & pen::REVERSE != 0 {
                // A line feed that scrolls the target terminal paints the new
                // row with the current background (bce); do not let a
                // coloured tail bleed into the next row.
                let plain = Pen::default();
                write_sgr_transition(&mut out, &mut scratch, &pen, &plain);
                pen = plain;
            }
        }
        if !pen.is_plain() {
            out.push_str("\x1b[0m");
        }
        out
    }
}

/// Appends what a (non-fragment) cell displays; never-written cells read as
/// spaces.
fn push_cell_text(out: &mut String, tables: &Tables, cell: grid::Cell) {
    let code = cell.code & grid::VALUE_MASK;
    if cell.code & grid::CLUSTER != 0 {
        out.push_str(&cell_text(tables, cell));
    } else if code == 0 {
        out.push(' ');
    } else {
        out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
    }
}

/// Removes trailing spaces and tabs of the line that starts at `from`.
fn trim_trailing_blanks(out: &mut String, from: usize) {
    let keep = out[from..].trim_end_matches([' ', '\t']).len();
    out.truncate(from + keep);
}

/// Whether a cell shows anything: a glyph, or a blank whose background or
/// line decorations are visible.
fn cell_is_visible(tables: &Tables, cell: grid::Cell) -> bool {
    !cell.is_blank() || tables.pen(cell.pen).blank_is_visible()
}

/// Cells up to and including the last visible one (a wide character's
/// fragment is included with its head).
fn visible_len(tables: &Tables, row: &Row) -> usize {
    let Some(last) = row.cells.iter().rposition(|&c| {
        (c.code & FRAGMENT == 0 || c.is_tab_head_fragment()) && cell_is_visible(tables, c)
    }) else {
        return 0;
    };
    let mut end = last + 1;
    while end < row.cells.len() && row.cells[end].code == FRAGMENT {
        end += 1;
    }
    end
}

fn row_is_visible(tables: &Tables, row: &Row) -> bool {
    visible_len(tables, row) > 0
}

/// Whether a captured stream moves the cursor vertically or edits the screen
/// in ways a line-oriented strip cannot reproduce: absolute or upward cursor
/// motion (`CUP`, `HVP`, `VPA`, `CUU`, `CPL`, `CNL`), scroll regions
/// (`DECSTBM`), reverse index, line insertion/deletion, scrolling (`SU`/`SD`),
/// cursor restore (`DECRC`, `CSI u`) or an erase beyond the current line
/// (`ED`). False positives only cost time — the screen replay of plain line
/// output equals the plain strip — so the scan errs on the side of `true`.
pub fn stream_needs_screen_replay(bytes: &[u8]) -> bool {
    let mut i = 0;
    while let Some(offset) = memchr::memchr(0x1b, &bytes[i..]) {
        let esc = i + offset;
        match bytes.get(esc + 1) {
            Some(b'M' | b'8') => return true,
            Some(b'[') => {
                let mut j = esc + 2;
                let private = matches!(bytes.get(j), Some(0x3c..=0x3f));
                while j < bytes.len() && !(0x40..=0x7e).contains(&bytes[j]) {
                    if bytes[j] == 0x1b {
                        break;
                    }
                    j += 1;
                }
                let Some(&fin) = bytes.get(j) else {
                    return false;
                };
                let intermediate = bytes[esc + 2..j].iter().any(|b| (0x20..=0x2f).contains(b));
                if !private
                    && !intermediate
                    && matches!(
                        fin,
                        b'A' | b'E'
                            | b'F'
                            | b'H'
                            | b'f'
                            | b'd'
                            | b'J'
                            | b'L'
                            | b'M'
                            | b'S'
                            | b'T'
                            | b'r'
                            | b'u'
                    )
                {
                    return true;
                }
                i = j;
                continue;
            }
            _ => {}
        }
        i = esc + 1;
    }
    false
}

/// Re-synchronises a byte ring whose front was dropped at an arbitrary byte:
/// returns the stream from the first escape sequence or the first full line,
/// whichever comes first, so a CSI tail (`8;29H`) or half a UTF-8 character at
/// the cut is not printed as text. Only call it when the ring actually dropped
/// its front; an intact stream starts at a real boundary already. When
/// neither an `ESC` nor a line feed exists, only a leading run of UTF-8
/// continuation bytes is skipped.
pub fn resync_ring_head(bytes: &[u8]) -> &[u8] {
    let after_lf = memchr::memchr(b'\n', bytes).map(|i| i + 1);
    let esc = memchr::memchr(0x1b, bytes);
    let start = match (esc, after_lf) {
        (Some(e), Some(l)) => e.min(l),
        (Some(e), None) => e,
        (None, Some(l)) => l,
        (None, None) => bytes
            .iter()
            .position(|&b| b & 0xc0 != 0x80)
            .unwrap_or(bytes.len()),
    };
    &bytes[start..]
}
