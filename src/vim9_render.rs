//! Buffer-origin GUI paint data. No field is decoded from terminal escape
//! sequences or a terminal palette. Screen coordinates preserve Vim's input
//! geometry while every glyph, RGB color and decoration is painted by the host.

use serde::Deserialize;

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Vim9RenderStyle {
    /// Packed `0xRRGGBB` GUI colors. `None` inherits the frame's normal style.
    pub foreground: Option<u32>,
    pub background: Option<u32>,
    pub special: Option<u32>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub undercurl: bool,
    pub strikethrough: bool,
    pub reverse: bool,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Vim9RenderRect {
    /// Zero-based position in the editor PTY's current screen geometry.
    pub row: usize,
    pub column: usize,
    pub rows: usize,
    pub columns: usize,
}

impl Vim9RenderRect {
    pub fn contains(&self, row: usize, column: usize) -> bool {
        row >= self.row
            && row - self.row < self.rows
            && column >= self.column
            && column - self.column < self.columns
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Vim9RenderCursor {
    pub row: usize,
    pub column: usize,
    /// Vim display cells, including wide-character geometry.
    pub width: usize,
    pub visible: bool,
    /// Source position for native overlays at empty lines and insertion ends.
    /// Zero line means the optional source position was not supplied.
    pub line: u64,
    pub byte_column: usize,
    /// Optional native GUI shape, RGB highlight and millisecond blink policy.
    pub appearance: Option<super::Vim9CursorAppearance>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Vim9RenderRun {
    /// Vim window identity. Zero is accepted only for legacy single-view frames.
    pub window: i64,
    pub row: usize,
    pub column: usize,
    pub text: String,
    pub cells: usize,
    pub style: Vim9RenderStyle,
    /// One-based buffer line, zero for gutters and end-of-buffer decoration.
    pub line: u64,
    /// Zero-based UTF-8 byte range in the original buffer line. Tabs are
    /// expanded only in `text`; these boundaries still refer to Vim's bytes.
    pub byte_start: usize,
    pub byte_end: usize,
}

impl Vim9RenderRun {
    /// A run of one repeated single-cell glyph, such as the closed-fold
    /// filler. Painters place it per cell: a fallback font's advance for the
    /// glyph must not stretch or shrink the filler across Vim's columns.
    pub fn repeated_glyph(&self) -> Option<char> {
        let mut characters = self.text.chars();
        let first = characters.next()?;
        (self.cells > 1
            && !first.is_ascii_control()
            && self.text.chars().count() == self.cells
            && characters.all(|character| character == first))
        .then_some(first)
    }
}

/// A real buffer row outside the visible split. `row` is signed and relative
/// to its viewport's top, never a terminal screen coordinate. Nested runs use
/// row zero; their columns and source bytes retain the same meaning as visible
/// runs. Empty lines are explicit, so missing paint is never guessed as text.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Vim9RenderMarginRow {
    pub row: i32,
    pub line: u64,
    pub byte_length: usize,
    pub runs: Vec<Vim9RenderRun>,
    /// Protocol 8: last line of the closed fold shown by this row, zero for
    /// an ordinary source row. A fold row is display-only paint.
    pub fold_end: u64,
    /// Protocol 9: continuation row of a wrapped line, zero for its first row.
    pub part: u32,
    /// Protocol 9: screen rows the line occupies; zero means one.
    pub parts: u32,
}

/// A visible Vim window, sampled in that window's own buffer/option context.
/// Status lines and split separators are deliberately outside `view`.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Vim9RenderWindow {
    pub window: i64,
    pub buffer: i64,
    pub changedtick: u64,
    pub view: Vim9RenderRect,
    pub cursor: Vim9RenderCursor,
    /// One-based buffer line limits of the sampled viewport.
    pub topline: u64,
    pub botline: u64,
    pub leftcol: usize,
    pub textoff: usize,
    /// Zero for old wire versions. Otherwise the actual buffer line count.
    pub line_count: u64,
    /// Version 3 supplies two source rows per edge; version 4 supplies eight.
    pub overscan: Vec<Vim9RenderMarginRow>,
    /// Optional protocol-3 native input capability, false for legacy producers.
    pub native_pointer: bool,
    /// Source-bound scrolling is independent of mouse mappings, mouse options
    /// and virtual editing. False for producers predating this capability.
    pub native_scroll: bool,
    /// Last successfully applied source-bound scroll request in this split.
    pub scroll_ack: u64,
    /// Explicit visible source-line lengths, including empty and EOL targets.
    /// With a display-row map there is one entry per visible display row.
    pub line_lengths: Vec<usize>,
    /// Protocol 8 display-row map, present only while closed folds are in the
    /// visible or margin rows. Entry `i` is the first source line shown by
    /// visible row `i`; empty means the legacy `topline + i` mapping.
    pub display_lines: Vec<u64>,
    /// Parallel to `display_lines`: the closed fold's last line, or zero.
    pub fold_ends: Vec<u64>,
    /// Protocol 8: display rows actually moved by the request in `scroll_ack`.
    /// Older producers omit it and the topline delta remains authoritative.
    pub scroll_rows: Option<i32>,
    /// Protocol 9, parallel to `display_lines` while wrapped lines are
    /// sampled: the continuation row of each visible row and the number of
    /// screen rows its line occupies. Empty means every row is a whole line.
    pub wrap_parts: Vec<u32>,
    pub wrap_rows: Vec<u32>,
    /// Protocol 9 smoothscroll offset: screen cells of the topline above the
    /// first visible row. With `topline` it identifies the viewport.
    pub skipcol: usize,
}

/// One sampled display row of a split: an ordinary source line, one screen
/// row (`part` of `parts`) of a wrapped line, or a closed fold covering
/// `line..=last`. `row` is viewport-relative, negative above.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Vim9DisplayRow {
    pub row: i64,
    pub line: u64,
    pub last: u64,
    pub folded: bool,
    pub part: u32,
    pub parts: u32,
}

impl Vim9DisplayRow {
    /// The row holding the end of its line, where EOL targets live.
    pub fn ends_line(&self) -> bool {
        !self.folded && self.part + 1 >= self.parts
    }
}

impl Vim9RenderWindow {
    /// True when closed folds or wrapped lines make display rows differ from
    /// source lines.
    pub fn has_display_rows(&self) -> bool {
        !self.display_lines.is_empty()
    }

    /// Visible rows in order. Without a display-row map each row is the
    /// legacy `topline + row` source line.
    pub fn visible_rows(&self) -> impl Iterator<Item = Vim9DisplayRow> + '_ {
        let legacy = (!self.has_display_rows() && self.topline > 0)
            .then_some(self.topline..=self.botline)
            .into_iter()
            .flatten()
            .take(self.view.rows)
            .map(|line| (line, 0, 0, 1));
        let mapped = self.display_lines.iter().enumerate().map(|(index, line)| {
            (
                *line,
                self.fold_ends.get(index).copied().unwrap_or(0),
                self.wrap_parts.get(index).copied().unwrap_or(0),
                self.wrap_rows.get(index).copied().unwrap_or(1),
            )
        });
        legacy
            .chain(mapped)
            .enumerate()
            .map(|(row, (line, end, part, parts))| Vim9DisplayRow {
                row: row as i64,
                line,
                last: end.max(line),
                folded: end != 0,
                part,
                parts: parts.max(1),
            })
    }

    /// Visible and margin rows, ordered from the top margin down.
    pub fn display_rows(&self) -> impl Iterator<Item = Vim9DisplayRow> + '_ {
        self.overscan
            .iter()
            .filter(|edge| edge.row < 0)
            .map(margin_display_row)
            .chain(self.visible_rows())
            .chain(
                self.overscan
                    .iter()
                    .filter(|edge| edge.row >= 0)
                    .map(margin_display_row),
            )
    }

    /// The signed viewport row that displays `line`, including a line hidden
    /// inside a closed fold; a wrapped line resolves to its first sampled
    /// row. Margin rows count; unsampled lines return None.
    pub fn display_row_of(&self, line: u64) -> Option<i64> {
        self.display_rows()
            .find(|row| (row.line..=row.last).contains(&line))
            .map(|row| row.row)
    }

    /// The row showing continuation `part` of `line` (any line of a fold).
    pub fn display_row_of_part(&self, line: u64, part: u32) -> Option<i64> {
        self.display_rows()
            .find(|row| (row.line..=row.last).contains(&line) && (row.folded || row.part == part))
            .map(|row| row.row)
    }

    /// Continuation row of the topline shown first (smoothscroll).
    pub fn top_part(&self) -> u32 {
        self.wrap_parts.first().copied().unwrap_or(0)
    }

    /// Whether both samples start at the same display row: the same topline
    /// and, with 'smoothscroll', the same skipped part of a wrapped topline.
    pub fn same_first_row(&self, other: &Vim9RenderWindow) -> bool {
        self.topline == other.topline && self.skipcol == other.skipcol
    }

    /// Display rows between this viewport's first row and `next`'s. Without
    /// folds or wrapped lines this is the plain line delta. A closed fold and
    /// each wrapped screen row count once, as Vim scrolls; None when neither
    /// sample covers the other's first row, so callers snap instead of
    /// animating a guess.
    pub fn topline_shift(&self, next: &Vim9RenderWindow) -> Option<i64> {
        if !self.has_display_rows() && !next.has_display_rows() {
            return Some(next.topline as i64 - self.topline as i64);
        }
        self.display_row_of_part(next.topline, next.top_part())
            .or_else(|| {
                next.display_row_of_part(self.topline, self.top_part())
                    .map(|row| -row)
            })
    }

    /// Protocol 8/9 producers send the map only together with contiguous row
    /// coverage: each row is the next continuation row of its line or the
    /// first row after the previous row's last line, in both margins too.
    fn display_map_valid(&self) -> bool {
        if self.display_lines.is_empty() {
            return self.fold_ends.is_empty()
                && self.wrap_parts.is_empty()
                && self.wrap_rows.is_empty()
                && self.skipcol == 0
                && self
                    .overscan
                    .iter()
                    .all(|edge| edge.fold_end == 0 && edge.part == 0 && edge.parts <= 1);
        }
        let wrapped = !self.wrap_parts.is_empty();
        if self.fold_ends.len() != self.display_lines.len()
            || (wrapped
                && (self.wrap_parts.len() != self.display_lines.len()
                    || self.wrap_rows.len() != self.display_lines.len()))
            || (!wrapped && !self.wrap_rows.is_empty())
            || self.display_lines.len() > self.view.rows
            || self.line_lengths.len() != self.display_lines.len()
            || self.native_pointer
            || self.line_count == 0
        {
            return false;
        }
        let shape = |row: &Vim9DisplayRow| {
            row.line > 0
                && row.line <= row.last
                && row.last <= self.line_count
                && row.part < row.parts
                && row.parts <= 4096
                && (!row.folded || (row.part == 0 && row.parts == 1))
                && (wrapped || row.parts == 1)
        };
        // Whether `next` is the display row immediately after `row`.
        let follows = |row: &Vim9DisplayRow, next: &Vim9DisplayRow| {
            if row.part + 1 < row.parts {
                next.line == row.line && next.part == row.part + 1 && next.parts == row.parts
            } else {
                row.last.checked_add(1) == Some(next.line) && next.part == 0
            }
        };
        let mut previous: Option<Vim9DisplayRow> = None;
        for row in self.visible_rows() {
            let chained = match &previous {
                None => row.line == self.topline && (row.part > 0) == (self.skipcol > 0),
                Some(previous) => follows(previous, &row),
            };
            if !shape(&row) || !chained {
                return false;
            }
            previous = Some(row);
        }
        let Some(bottom) = previous else {
            return false;
        };
        if bottom.last != self.botline {
            return false;
        }
        // Margins continue the same chain outward from both viewport edges.
        let mut below = bottom;
        let below_rows = self.overscan.iter().filter(|edge| edge.row >= 0);
        for (expected, edge) in (self.view.rows as i64..).zip(below_rows) {
            let row = margin_display_row(edge);
            if row.row != expected || !shape(&row) || !follows(&below, &row) {
                return false;
            }
            below = row;
        }
        let Some(mut above) = self.visible_rows().next() else {
            return false;
        };
        let above_rows = self.overscan.iter().rev().filter(|edge| edge.row < 0);
        for (distance, edge) in (1..).zip(above_rows) {
            let row = margin_display_row(edge);
            if row.row != -distance || !shape(&row) || !follows(&row, &above) {
                return false;
            }
            above = row;
        }
        true
    }
}

fn margin_display_row(edge: &Vim9RenderMarginRow) -> Vim9DisplayRow {
    Vim9DisplayRow {
        row: i64::from(edge.row),
        line: edge.line,
        last: edge.fold_end.max(edge.line),
        folded: edge.fold_end != 0,
        part: edge.part,
        parts: edge.parts.max(1),
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Vim9RenderCompletion {
    /// Vim's exact popup rectangle, independent of buffer/split ownership.
    pub view: Vim9RenderRect,
    pub default_style: Vim9RenderStyle,
    /// Display-only glyphs and row backgrounds, never buffer source anchors.
    pub runs: Vec<Vim9RenderRun>,
}

/// Protocol 6 display-only Vim popup. Vim owns its contents, geometry, input
/// filters and lifetime; hosts only paint the sampled surface. Popup runs
/// deliberately carry no editable buffer/window/source coordinates.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Vim9RenderPopup {
    /// Protocol 7 optional provider presentation hint; unknown values remain classic.
    pub kind: String,
    pub id: i64,
    /// Vim's stacking order. The ordinary completion menu has z-index 100.
    pub zindex: i32,
    pub view: Vim9RenderRect,
    pub default_style: Vim9RenderStyle,
    /// Includes body, padding, border and syntax glyphs in paint order.
    pub runs: Vec<Vim9RenderRun>,
}

impl Vim9RenderPopup {
    pub fn is_documentation_card(&self) -> bool {
        matches!(
            self.kind.as_str(),
            "hover" | "signature" | "diagnostic" | "completion_documentation"
        )
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Vim9RenderFrame {
    pub revision: u64,
    /// False means the host must immediately show Vim's compatibility view.
    /// Never retain a previous native frame over an unsupported editor state.
    pub supported: bool,
    pub fallback_reason: String,
    pub rows: usize,
    pub columns: usize,
    pub window: i64,
    pub buffer: i64,
    pub changedtick: u64,
    pub mode: String,
    /// Exact opaque paint rectangle; Vim's status/command rows remain outside.
    pub view: Vim9RenderRect,
    pub cursor: Vim9RenderCursor,
    pub default_style: Vim9RenderStyle,
    /// All visible ordinary windows in the active Vim tab. Empty for the
    /// original single-view protocol. `view` remains the active window.
    pub windows: Vec<Vim9RenderWindow>,
    /// Plain ASCII with equal style may be coalesced. Each non-ASCII grapheme
    /// remains separate so font fallback cannot shift later cursor geometry.
    pub runs: Vec<Vim9RenderRun>,
    /// Protocol 5 ordinary Insert completion; None also explicitly clears it.
    pub completion: Option<Vim9RenderCompletion>,
    /// Protocol 6 popups, ordered by ascending zindex then descending id (Vim
    /// puts older windows on top at equal zindex). An empty list clears every
    /// previous surface, including in an otherwise empty row delta.
    pub popups: Vec<Vim9RenderPopup>,
}

impl Vim9RenderFrame {
    /// Overlay input belongs to Vim. Hosts must retire native pointer/scroll
    /// transforms while these surfaces are visible, including outside them.
    pub fn has_overlays(&self) -> bool {
        self.completion.is_some() || !self.popups.is_empty()
    }

    /// Exact native paint ownership, without covering Vim's split separators,
    /// per-window status lines, tab line or command line.
    pub fn paint_views(&self) -> impl Iterator<Item = &Vim9RenderRect> {
        self.windows
            .iter()
            .map(|window| &window.view)
            .chain(std::iter::once(&self.view).filter(|_| self.windows.is_empty()))
    }

    pub fn covers(&self, row: usize, column: usize) -> bool {
        self.paint_views().any(|view| view.contains(row, column))
    }

    pub(super) fn valid(&self) -> bool {
        // Closed fold rows are display-only. A source-addressed run there
        // could anchor a hover or pointer to a hidden line.
        let fold_rows: std::collections::HashSet<(i64, usize)> = self
            .windows
            .iter()
            .flat_map(|window| {
                window
                    .visible_rows()
                    .filter(|row| row.folded)
                    .map(move |row| (window.window, window.view.row + row.row as usize))
            })
            .collect();
        fn style_valid(style: &Vim9RenderStyle) -> bool {
            [style.foreground, style.background, style.special]
                .into_iter()
                .flatten()
                .all(|color| color <= 0xff_ffff)
        }
        let within =
            |start: usize, count: usize, limit: usize| start <= limit && count <= limit - start;
        let overlay_valid =
            |view: &Vim9RenderRect, style: &Vim9RenderStyle, runs: &[Vim9RenderRun]| {
                self.supported
                    && view.rows > 0
                    && view.rows <= 64
                    && view.columns > 0
                    && view.columns <= 512
                    && within(view.row, view.rows, self.rows)
                    && within(view.column, view.columns, self.columns)
                    && style_valid(style)
                    && runs.len() <= 4096
                    && runs.iter().all(|run| {
                        run.cells > 0
                            && view.contains(run.row, run.column)
                            && within(run.column, run.cells, view.column + view.columns)
                            && run.window == 0
                            && run.line == 0
                            && run.byte_start == 0
                            && run.byte_end == 0
                            && run.text.len() <= 4096
                            && !run.text.chars().any(|c| c.is_ascii_control())
                            && style_valid(&run.style)
                    })
            };
        self.rows > 0
            && self.rows <= 4096
            && self.columns > 0
            && self.columns <= 4096
            && (!self.supported
                || (self.window > 0
                    && self.buffer > 0
                    && self.view.rows > 0
                    && self.view.columns > 0))
            && self.runs.len()
                + self
                    .windows
                    .iter()
                    .flat_map(|window| &window.overscan)
                    .map(|row| row.runs.len())
                    .sum::<usize>()
                <= 24_000
            && self.windows.len() <= 16
            && style_valid(&self.default_style)
            && self.completion.as_ref().is_none_or(|menu| {
                self.mode.starts_with('i')
                    && overlay_valid(&menu.view, &menu.default_style, &menu.runs)
            })
            && self.popups.len() <= 8
            && self
                .popups
                .iter()
                .map(|popup| popup.runs.len())
                .sum::<usize>()
                <= 4096
            && self.popups.iter().enumerate().all(|(index, popup)| {
                popup.id > 0
                    && popup.kind.len() <= 32
                    && (1..=32_000).contains(&popup.zindex)
                    && overlay_valid(&popup.view, &popup.default_style, &popup.runs)
                    && self.popups[..index]
                        .iter()
                        .all(|other| other.id != popup.id)
                    && (index == 0
                        || self.popups[index - 1].zindex < popup.zindex
                        || (self.popups[index - 1].zindex == popup.zindex
                            && self.popups[index - 1].id > popup.id))
            })
            && self.cursor.appearance.as_ref().is_none_or(|a| a.valid())
            && within(self.view.row, self.view.rows, self.rows)
            && within(self.view.column, self.view.columns, self.columns)
            && (!self.cursor.visible
                || (self.cursor.row < self.rows
                    && self.cursor.width > 0
                    && self.view.contains(self.cursor.row, self.cursor.column)
                    && within(
                        self.cursor.column,
                        self.cursor.width,
                        self.view.column + self.view.columns,
                    )))
            && self.windows.iter().enumerate().all(|(index, window)| {
                window.window > 0
                    && window.cursor.appearance.as_ref().is_none_or(|a| a.valid())
                    && window.buffer > 0
                    && window.view.rows > 0
                    && window.view.columns > 0
                    && within(window.view.row, window.view.rows, self.rows)
                    && within(window.view.column, window.view.columns, self.columns)
                    && window.textoff <= window.view.columns
                    && window.scroll_ack <= i64::MAX as u64
                    && window.line_lengths.len() <= window.view.rows
                    && window.line_lengths.iter().all(|length| *length <= 16384)
                    && (window.line_lengths.is_empty()
                        || window.has_display_rows()
                        || window.line_lengths.len() as u64
                            == window.botline.saturating_sub(window.topline) + 1)
                    && window.display_map_valid()
                    && (!window.native_pointer || !window.line_lengths.is_empty())
                    && (!window.native_scroll
                        || (window.line_count > 0 && !window.line_lengths.is_empty()))
                    && (window.line_count == 0
                        || (window.topline > 0
                            && window.topline <= window.botline
                            && window.botline <= window.line_count))
                    && window.overscan.len() <= 16
                    && window.overscan.iter().enumerate().all(|(index, row)| {
                        let source_line = if row.row < 0 {
                            window
                                .topline
                                .checked_sub(u64::from(row.row.unsigned_abs()))
                        } else {
                            window.topline.checked_add(row.row as u64)
                        };
                        // Fold-aware margins are checked as one chain by
                        // display_map_valid(); fold rows never carry source text.
                        window.line_count > 0
                            && row.line > 0
                            && row.line <= window.line_count
                            && (window.has_display_rows() || source_line == Some(row.line))
                            && (row.fold_end == 0 || row.runs.iter().all(|run| run.line == 0))
                            && ((-8..0).contains(&row.row)
                                || (window.view.rows as i32..window.view.rows as i32 + 8)
                                    .contains(&row.row))
                            && row.byte_length <= 16_384
                            && row.runs.len() <= 512
                            && window.overscan[..index]
                                .iter()
                                .all(|other| other.row < row.row)
                            && row.runs.iter().all(|run| {
                                run.window == window.window
                                    && run.row == 0
                                    && run.cells > 0
                                    && run.column >= window.view.column
                                    && within(
                                        run.column,
                                        run.cells,
                                        window.view.column + window.view.columns,
                                    )
                                    && (run.line == 0 || run.line == row.line)
                                    && run.byte_start <= run.byte_end
                                    && (run.line == 0 || run.byte_end <= row.byte_length)
                                    && run.text.len() <= 64 * 1024
                                    && !run
                                        .text
                                        .chars()
                                        .any(|character| character.is_ascii_control())
                                    && style_valid(&run.style)
                            })
                    })
                    && (!window.cursor.visible
                        || (window.window == self.window
                            && window
                                .view
                                .contains(window.cursor.row, window.cursor.column)
                            && window.cursor.width > 0
                            && within(
                                window.cursor.column,
                                window.cursor.width,
                                window.view.column + window.view.columns,
                            )))
                    && self.windows[..index].iter().all(|other| {
                        other.window != window.window
                            && (window.view.row >= other.view.row + other.view.rows
                                || other.view.row >= window.view.row + window.view.rows
                                || window.view.column >= other.view.column + other.view.columns
                                || other.view.column >= window.view.column + window.view.columns)
                    })
            })
            && (!self.supported
                || self.windows.is_empty()
                || self.windows.iter().any(|window| {
                    window.window == self.window
                        && window.buffer == self.buffer
                        && window.changedtick == self.changedtick
                        && window.view == self.view
                }))
            && self.runs.iter().all(|run| {
                run.cells > 0
                    && self.paint_views().any(|view| {
                        view.contains(run.row, run.column)
                            && within(run.column, run.cells, view.column + view.columns)
                    })
                    && (self.windows.is_empty()
                        || self.windows.iter().any(|window| {
                            window.window == run.window
                                && window.view.contains(run.row, run.column)
                                && within(
                                    run.column,
                                    run.cells,
                                    window.view.column + window.view.columns,
                                )
                        }))
                    && run.byte_start <= run.byte_end
                    && (run.line == 0 || !fold_rows.contains(&(run.window, run.row)))
                    && run.text.len() <= 64 * 1024
                    && !run
                        .text
                        .chars()
                        .any(|character| character.is_ascii_control())
                    && style_valid(&run.style)
            })
    }
}

/// Version 2 wire update: replacement paint runs for complete screen rows.
/// A row includes all windows that share that screen row; an empty run list
/// explicitly clears old glyphs. The host never patches editable buffer text.
#[derive(Debug, Deserialize)]
pub(super) struct RenderDelta {
    pub base_revision: u64,
    pub row_updates: Vec<RenderRow>,
    #[serde(flatten)]
    pub frame: Vim9RenderFrame,
}

#[derive(Debug, Deserialize)]
pub(super) struct RenderRow {
    pub row: usize,
    pub runs: Vec<Vim9RenderRun>,
}

impl RenderDelta {
    /// Apply atomically to the exact prior paint revision. A missing base,
    /// changed layout or invalid patch requests a fresh keyframe instead.
    pub(super) fn apply(self, previous: &Vim9RenderFrame) -> Option<Vim9RenderFrame> {
        let mut frame = self.frame;
        if self.base_revision != previous.revision
            || frame.revision <= self.base_revision
            || !previous.supported
            || !frame.supported
            || !frame.runs.is_empty()
            || frame.rows != previous.rows
            || frame.columns != previous.columns
            || frame.window != previous.window
            || frame.buffer != previous.buffer
            || frame.view != previous.view
            || frame.default_style != previous.default_style
            || frame.windows.len() != previous.windows.len()
            || frame
                .windows
                .iter()
                .zip(&previous.windows)
                .any(|(next, old)| {
                    next.window != old.window || next.buffer != old.buffer || next.view != old.view
                })
            || self.row_updates.len() > frame.rows
        {
            return None;
        }
        let mut replacements = std::collections::BTreeMap::new();
        for row in self.row_updates {
            if row.row >= frame.rows
                || row.runs.iter().any(|run| run.row != row.row)
                || replacements.insert(row.row, row.runs).is_some()
            {
                return None;
            }
        }
        let mut runs: Vec<_> = previous
            .runs
            .iter()
            .filter(|run| !replacements.contains_key(&run.row))
            .cloned()
            .collect();
        runs.extend(replacements.into_values().flatten());
        // Stable sort preserves background/text/decoration layering within a
        // screen row while making a recovered keyframe and delta identical.
        runs.sort_by_key(|run| run.row);
        frame.runs = runs;
        frame.valid().then_some(frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn popup(id: i64, zindex: i32) -> Value {
        json!({
            "id": id, "zindex": zindex,
            "view": {"row":2,"column":35,"rows":3,"columns":20},
            "default_style":{"foreground":0xabcdef,"background":0x123456},
            "runs":[{"row":3,"column":36,"text":"文档 é","cells":7,
                "style":{"foreground":0xfedcba,"italic":true}}]
        })
    }

    #[test]
    fn popups_cross_splits_without_creating_buffer_source_paint() {
        let mut value = split_frame();
        let original: Vim9RenderFrame = serde_json::from_value(value.clone()).unwrap();
        assert!(!original.has_overlays());
        value["popups"] = json!([popup(300, 50), popup(400, 200)]);
        let frame: Vim9RenderFrame = serde_json::from_value(value.clone()).unwrap();
        assert!(frame.valid());
        assert!(frame.has_overlays());
        assert_eq!(frame.runs, original.runs);
        // Column 39 is the separator, not part of either editable split.
        assert!(!frame.covers(3, 39));
        assert!(frame.popups[0].view.contains(3, 39));
        value["mode"] = json!("ic");
        value["completion"] = json!({
            "view":{"row":2,"column":35,"rows":1,"columns":20},
            "runs":[{"row":2,"column":35,"text":"candidate","cells":9}]
        });
        let both: Vim9RenderFrame = serde_json::from_value(value).unwrap();
        assert!(both.valid());
        assert_eq!(both.popups.len(), 2);
        assert!(both.completion.is_some());
    }

    #[test]
    fn popup_wire_rejects_invalid_geometry_order_and_source_anchors() {
        let mut valid = split_frame();
        valid["popups"] = json!([popup(300, 50)]);
        for (field, bad) in [
            ("id", json!(0)),
            ("zindex", json!(0)),
            ("zindex", json!(32_001)),
            ("view", json!({"row":9,"column":0,"rows":2,"columns":20})),
            (
                "view",
                json!({"row":0,"column":usize::MAX,"rows":1,"columns":20}),
            ),
            ("default_style", json!({"background":0x1000000})),
        ] {
            let mut bad_frame = valid.clone();
            bad_frame["popups"][0][field] = bad;
            assert!(
                !serde_json::from_value::<Vim9RenderFrame>(bad_frame)
                    .unwrap()
                    .valid(),
                "popup field {field}"
            );
        }
        for (field, bad) in [
            ("window", json!(100)),
            ("line", json!(1)),
            ("byte_start", json!(1)),
            ("byte_end", json!(1)),
            ("cells", json!(30)),
            ("row", json!(9)),
            ("text", json!("docs\u{1b}[31m")),
            ("style", json!({"foreground":0x1000000})),
        ] {
            let mut bad_frame = valid.clone();
            bad_frame["popups"][0]["runs"][0][field] = bad;
            assert!(
                !serde_json::from_value::<Vim9RenderFrame>(bad_frame)
                    .unwrap()
                    .valid(),
                "popup run field {field}"
            );
        }
        for surfaces in [
            json!([popup(300, 50), popup(300, 200)]),
            json!([popup(400, 200), popup(300, 50)]),
            json!([popup(300, 50), popup(400, 50)]),
            json!((1..=9).rev().map(|id| popup(id, 50)).collect::<Vec<_>>()),
        ] {
            let mut bad_frame = valid.clone();
            bad_frame["popups"] = surfaces;
            assert!(!serde_json::from_value::<Vim9RenderFrame>(bad_frame)
                .unwrap()
                .valid());
        }
        valid["supported"] = json!(false);
        assert!(!serde_json::from_value::<Vim9RenderFrame>(valid)
            .unwrap()
            .valid());
    }

    #[test]
    fn popup_total_paint_budget_is_bounded_across_all_surfaces() {
        let mut value = split_frame();
        let mut first = popup(400, 50);
        let mut second = popup(300, 50);
        let run = first["runs"][0].clone();
        first["runs"] = json!(vec![run.clone(); 2048]);
        second["runs"] = json!(vec![run.clone(); 2048]);
        value["popups"] = json!([first, second]);
        assert!(serde_json::from_value::<Vim9RenderFrame>(value.clone())
            .unwrap()
            .valid());
        value["popups"][1]["runs"].as_array_mut().unwrap().push(run);
        assert!(!serde_json::from_value::<Vim9RenderFrame>(value)
            .unwrap()
            .valid());
    }

    #[test]
    fn popup_card_kinds_are_bounded_and_unknown_values_remain_classic() {
        for kind in [
            "",
            "hover",
            "signature",
            "diagnostic",
            "completion_documentation",
            "future",
        ] {
            let mut value = split_frame();
            let mut surface = popup(300, 50);
            surface["kind"] = json!(kind);
            value["popups"] = json!([surface]);
            let frame: Vim9RenderFrame = serde_json::from_value(value).unwrap();
            assert!(frame.valid());
            assert_eq!(
                frame.popups[0].is_documentation_card(),
                !matches!(kind, "" | "future")
            );
        }
        let mut value = split_frame();
        let mut surface = popup(300, 50);
        surface["kind"] = json!("x".repeat(33));
        value["popups"] = json!([surface]);
        assert!(!serde_json::from_value::<Vim9RenderFrame>(value)
            .unwrap()
            .valid());
    }

    #[test]
    fn popup_only_deltas_replace_and_clear_without_stale_surfaces() {
        let value = split_frame();
        let first: Vim9RenderFrame = serde_json::from_value(value.clone()).unwrap();
        let mut delta = patch(&value, json!([]));
        delta["popups"] = json!([popup(300, 50)]);
        delta["popups"][0]["kind"] = json!("hover");
        let opened = serde_json::from_value::<RenderDelta>(delta.clone())
            .unwrap()
            .apply(&first)
            .unwrap();
        assert!(opened.has_overlays());
        assert!(opened.popups[0].is_documentation_card());
        assert_eq!(opened.runs, first.runs);
        delta["revision"] = json!(3);
        delta["base_revision"] = json!(2);
        delta["popups"][0]["default_style"]["foreground"] = json!(0x665544);
        let updated = serde_json::from_value::<RenderDelta>(delta.clone())
            .unwrap()
            .apply(&opened)
            .unwrap();
        assert_eq!(updated.popups[0].default_style.foreground, Some(0x665544));
        assert_eq!(updated.popups[0].kind, "hover");
        assert_eq!(opened.popups[0].default_style.foreground, Some(0xabcdef));
        delta["revision"] = json!(4);
        delta["base_revision"] = json!(3);
        delta["popups"][0]["runs"][0]["line"] = json!(12);
        assert!(serde_json::from_value::<RenderDelta>(delta.clone())
            .unwrap()
            .apply(&updated)
            .is_none());
        delta["popups"] = json!([]);
        let closed = serde_json::from_value::<RenderDelta>(delta)
            .unwrap()
            .apply(&updated)
            .unwrap();
        assert!(!closed.has_overlays());
        assert_eq!(closed.runs, first.runs);
    }

    #[test]
    fn completion_overlay_can_cross_splits_but_never_becomes_source_paint() {
        let mut value = split_frame();
        value["mode"] = json!("ic");
        value["completion"] = json!({
            "view":{"row":2,"column":35,"rows":2,"columns":20},
            "default_style":{"foreground":0xabcdef,"background":0x123456},
            "runs":[{"row":2,"column":35,"text":"native completion","cells":17}]
        });
        let frame: Vim9RenderFrame = serde_json::from_value(value.clone()).unwrap();
        assert!(frame.valid());
        assert_eq!(frame.runs.len(), 3);
        for (field, bad) in [
            ("cells", json!(30)),
            ("line", json!(1)),
            ("text", json!("untrusted\ncontrol")),
            ("style", json!({"foreground":0x1000000})),
        ] {
            let mut broken = value.clone();
            broken["completion"]["runs"][0][field] = bad;
            assert!(!serde_json::from_value::<Vim9RenderFrame>(broken)
                .unwrap()
                .valid());
        }
        value["mode"] = json!("n");
        assert!(!serde_json::from_value::<Vim9RenderFrame>(value)
            .unwrap()
            .valid());
    }

    #[test]
    fn completion_only_delta_updates_and_closes_without_reusing_old_menu() {
        let mut value = split_frame();
        value["mode"] = json!("i");
        let first: Vim9RenderFrame = serde_json::from_value(value.clone()).unwrap();
        value["revision"] = json!(2);
        value["base_revision"] = json!(1);
        value["row_updates"] = json!([]);
        value["runs"] = json!([]);
        value["completion"] = json!({"view":{"row":2,"column":35,"rows":1,"columns":20},
            "runs":[{"row":2,"column":35,"text":"candidate","cells":9}]});
        let menu = serde_json::from_value::<RenderDelta>(value.clone())
            .unwrap()
            .apply(&first)
            .unwrap();
        assert!(menu.completion.is_some());
        assert_eq!(menu.runs, first.runs);
        value["revision"] = json!(3);
        value["base_revision"] = json!(2);
        value["completion"] = Value::Null;
        let closed = serde_json::from_value::<RenderDelta>(value)
            .unwrap()
            .apply(&menu)
            .unwrap();
        assert!(closed.completion.is_none());
        assert_eq!(closed.runs, first.runs);
    }

    fn split_frame() -> Value {
        json!({
            "revision":1,"supported":true,"rows":10,"columns":80,
            "window":100,"buffer":1,"changedtick":2,"mode":"n",
            "view":{"row":0,"column":0,"rows":8,"columns":39},
            "cursor":{"row":0,"column":0,"width":2,"visible":true},
            "default_style":{"foreground":0x123456,"background":0x20242c},
            "windows":[
                {"window":100,"buffer":1,"changedtick":2,
                    "view":{"row":0,"column":0,"rows":8,"columns":39}},
                {"window":200,"buffer":2,"changedtick":3,
                    "view":{"row":0,"column":40,"rows":8,"columns":40}}
            ],
            "runs":[
                {"window":100,"row":0,"column":0,"text":"中文","cells":4,
                    "line":1,"byte_start":0,"byte_end":6},
                {"window":200,"row":0,"column":40,"text":"peer","cells":4,
                    "line":1,"byte_start":0,"byte_end":4},
                {"window":100,"row":1,"column":0,"text":"clear me","cells":8,
                    "line":2,"byte_start":0,"byte_end":8}
            ]
        })
    }

    /// One split: 15..=21, a fold 22..=30, 31 and 32; margins are a fold
    /// 10..=13 and 14 above and 33 below. Fold rows only carry display paint.
    fn folded_frame() -> Value {
        let overscan = json!([
            {"row":-2,"line":10,"fold_end":13,"byte_length":2,
                "runs":[{"window":100,"row":0,"column":0,"text":"+-- 3","cells":5}]},
            {"row":-1,"line":14,"byte_length":2,
                "runs":[{"window":100,"row":0,"column":0,"text":"14","cells":2,
                    "line":14,"byte_start":0,"byte_end":2}]},
            {"row":10,"line":33,"byte_length":2,"runs":[]}
        ]);
        let window = json!({"window":100,"buffer":1,"changedtick":2,
            "view":{"row":0,"column":0,"rows":10,"columns":40},
            "topline":15,"botline":32,"line_count":400,"native_scroll":true,
            "native_pointer":false,"scroll_rows":0,
            "line_lengths":[2,2,2,2,2,2,2,2,2,2],
            "display_lines":[15,16,17,18,19,20,21,22,31,32],
            "fold_ends":[0,0,0,0,0,0,0,30,0,0],
            "overscan":overscan});
        let runs = json!([
            {"window":100,"row":0,"column":0,"text":"15","cells":2,
                "line":15,"byte_start":0,"byte_end":2},
            {"window":100,"row":7,"column":0,"text":"+--  9 lines","cells":12},
            {"window":100,"row":7,"column":12,"text":"\u{b7}\u{b7}","cells":2},
            {"window":100,"row":8,"column":0,"text":"31","cells":2,
                "line":31,"byte_start":0,"byte_end":2}
        ]);
        json!({
            "revision":1,"supported":true,"rows":11,"columns":40,
            "window":100,"buffer":1,"changedtick":2,"mode":"n",
            "view":{"row":0,"column":0,"rows":10,"columns":40},
            "windows":[window],
            "runs":runs
        })
    }

    #[test]
    fn repeated_filler_glyphs_are_recognized_per_cell() {
        let run = |text: &str, cells| Vim9RenderRun {
            text: text.into(),
            cells,
            ..Default::default()
        };
        assert_eq!(
            run("\u{b7}\u{b7}\u{b7}", 3).repeated_glyph(),
            Some('\u{b7}')
        );
        assert_eq!(run("----", 4).repeated_glyph(), Some('-'));
        assert_eq!(run("\u{b7}", 1).repeated_glyph(), None);
        assert_eq!(
            run("\u{754c}\u{754c}", 4).repeated_glyph(),
            None,
            "wide glyphs keep their advance"
        );
        assert_eq!(run("ab", 2).repeated_glyph(), None);
        assert_eq!(run("e\u{301}e\u{301}", 2).repeated_glyph(), None);
    }

    #[test]
    fn closed_fold_rows_map_display_rows_to_source_ranges() {
        let frame: Vim9RenderFrame = serde_json::from_value(folded_frame()).unwrap();
        assert!(frame.valid());
        let window = &frame.windows[0];
        assert!(window.has_display_rows());
        let rows: Vec<_> = window.display_rows().collect();
        assert_eq!(
            rows.first().map(|row| (row.row, row.line, row.last)),
            Some((-2, 10, 13))
        );
        assert!(rows[0].folded && !rows[1].folded);
        assert_eq!(
            rows[9],
            Vim9DisplayRow {
                row: 7,
                line: 22,
                last: 30,
                folded: true,
                part: 0,
                parts: 1,
            }
        );
        assert_eq!(
            window.display_row_of(26),
            Some(7),
            "hidden lines belong to their fold row"
        );
        assert_eq!(window.display_row_of(31), Some(8));
        assert_eq!(window.display_row_of(11), Some(-2));
        assert_eq!(window.display_row_of(33), Some(10));
        assert_eq!(window.display_row_of(34), None);
        let mut next = window.clone();
        next.topline = 31;
        assert_eq!(window.topline_shift(&next), Some(8), "the fold counts once");
        next.topline = 10;
        assert_eq!(window.topline_shift(&next), Some(-2));
        let far = Vim9RenderWindow {
            topline: 200,
            botline: 209,
            ..Default::default()
        };
        assert_eq!(
            window.topline_shift(&far),
            None,
            "unsampled distance never animates a guess"
        );
        let mut legacy = Vim9RenderWindow {
            topline: 5,
            botline: 8,
            ..Default::default()
        };
        let mut moved = legacy.clone();
        moved.topline = 9;
        assert_eq!(legacy.topline_shift(&moved), Some(4));
        legacy.view.rows = 4;
        assert_eq!(
            legacy
                .visible_rows()
                .map(|row| row.line)
                .collect::<Vec<_>>(),
            [5, 6, 7, 8]
        );
    }

    #[test]
    fn fold_row_maps_must_be_contiguous_and_display_only() {
        let base = folded_frame();
        let accepted: Vim9RenderFrame = serde_json::from_value(base.clone()).unwrap();
        assert!(accepted.valid());
        let cases: [(&str, Value); 12] = [
            ("/windows/0/display_lines/8", json!(30)),
            ("/windows/0/display_lines/0", json!(14)),
            ("/windows/0/fold_ends/7", json!(21)),
            ("/windows/0/botline", json!(33)),
            ("/windows/0/line_lengths", json!([2, 2])),
            ("/windows/0/native_pointer", json!(true)),
            ("/windows/0/fold_ends", json!([0, 0, 0])),
            ("/windows/0/overscan/1/line", json!(13)),
            ("/windows/0/overscan/0/fold_end", json!(11)),
            ("/windows/0/overscan/2/line", json!(34)),
            ("/windows/0/overscan/0/runs/0/line", json!(10)),
            ("/runs/1/line", json!(22)),
        ];
        for (pointer, replacement) in cases {
            let mut value = base.clone();
            let (parent, key) = pointer.rsplit_once('/').unwrap();
            match value.pointer_mut(parent).unwrap() {
                Value::Array(items) => items[key.parse::<usize>().unwrap()] = replacement,
                object => {
                    object
                        .as_object_mut()
                        .unwrap()
                        .insert(key.into(), replacement);
                }
            }
            if pointer == "/runs/1/line" {
                value["runs"][1]["byte_end"] = json!(1);
            }
            let frame: Vim9RenderFrame = serde_json::from_value(value).unwrap();
            assert!(!frame.valid(), "{pointer} must be rejected");
        }
        // A legacy producer cannot smuggle fold metadata into margin rows.
        let mut legacy = base.clone();
        let window = legacy["windows"][0].as_object_mut().unwrap();
        window.remove("display_lines");
        window.remove("fold_ends");
        let frame: Vim9RenderFrame = serde_json::from_value(legacy).unwrap();
        assert!(!frame.valid());
    }

    /// One split, 20 columns: smoothscroll shows parts 1..=2 of line 5, then
    /// line 6, both parts of 7 and line 8. Margins: 4 and part 0 of 5 above,
    /// both parts of 9 below.
    fn wrapped_frame() -> Value {
        let margin = |row: i32, line: u64, part: u32, parts: u32| {
            json!({"row":row,"line":line,"part":part,"parts":parts,"byte_length":40,"runs":[
                {"window":100,"row":0,"column":0,"text":"w","cells":1,
                    "line":line,"byte_start":part * 20,"byte_end":part * 20 + 1}]})
        };
        let overscan = json!([
            margin(-2, 4, 0, 1),
            margin(-1, 5, 0, 3),
            margin(6, 9, 0, 2),
            margin(7, 9, 1, 2)
        ]);
        let window = json!({"window":100,"buffer":1,"changedtick":2,
            "view":{"row":0,"column":0,"rows":6,"columns":20},
            "topline":5,"botline":8,"skipcol":20,"line_count":40,
            "native_scroll":true,"native_pointer":false,"scroll_rows":0,
            "line_lengths":[50,50,3,30,30,4],
            "display_lines":[5,5,6,7,7,8],
            "fold_ends":[0,0,0,0,0,0],
            "wrap_parts":[1,2,0,0,1,0],
            "wrap_rows":[3,3,1,2,2,1],
            "overscan":overscan});
        json!({
            "revision":1,"supported":true,"rows":7,"columns":20,
            "window":100,"buffer":1,"changedtick":2,"mode":"n",
            "view":{"row":0,"column":0,"rows":6,"columns":20},
            "windows":[window],
            "runs":[
                {"window":100,"row":1,"column":0,"text":"tail","cells":4,
                    "line":5,"byte_start":40,"byte_end":44},
                {"window":100,"row":4,"column":0,"text":"seven","cells":5,
                    "line":7,"byte_start":20,"byte_end":25}
            ]
        })
    }

    #[test]
    fn wrapped_rows_map_each_screen_row_to_a_line_part() {
        let frame: Vim9RenderFrame = serde_json::from_value(wrapped_frame()).unwrap();
        assert!(frame.valid());
        let window = &frame.windows[0];
        let rows: Vec<_> = window
            .display_rows()
            .map(|row| (row.row, row.line, row.part, row.parts))
            .collect();
        assert_eq!(
            rows,
            [
                (-2, 4, 0, 1),
                (-1, 5, 0, 3),
                (0, 5, 1, 3),
                (1, 5, 2, 3),
                (2, 6, 0, 1),
                (3, 7, 0, 2),
                (4, 7, 1, 2),
                (5, 8, 0, 1),
                (6, 9, 0, 2),
                (7, 9, 1, 2),
            ]
        );
        let ends: Vec<_> = window.visible_rows().map(|row| row.ends_line()).collect();
        assert_eq!(ends, [false, true, true, false, true, true]);
        assert_eq!(window.display_row_of(5), Some(-1), "first sampled part");
        assert_eq!(window.display_row_of_part(7, 1), Some(4));
        assert_eq!(window.display_row_of_part(9, 1), Some(7));
        assert_eq!(window.display_row_of_part(5, 3), None);
        assert_eq!(window.top_part(), 1);
        // Each wrapped screen row is one scrolled row, like <C-E> with
        // 'smoothscroll'; a move to line 7's second part is four rows.
        let shifted = |line: u64, part: u32| {
            let mut next = window.clone();
            next.topline = line;
            next.wrap_parts[0] = part;
            window.topline_shift(&next)
        };
        assert!(window.same_first_row(window));
        let mut unskipped = window.clone();
        unskipped.skipcol = 0;
        assert!(!window.same_first_row(&unskipped), "skipcol moves the view");
        assert_eq!(shifted(6, 0), Some(2));
        assert_eq!(shifted(7, 1), Some(4));
        assert_eq!(shifted(5, 0), Some(-1));
        assert_eq!(shifted(9, 1), Some(7));
        assert_eq!(shifted(4, 0), Some(-2));
        assert_eq!(shifted(12, 0), None);
        // The reverse lookup answers when only the next sample covers the
        // previous top row.
        let previous = Vim9RenderWindow {
            topline: 7,
            botline: 7,
            display_lines: vec![7],
            fold_ends: vec![0],
            wrap_parts: vec![1],
            wrap_rows: vec![2],
            ..Default::default()
        };
        assert_eq!(previous.topline_shift(window), Some(-4));
    }

    #[test]
    fn wrapped_row_maps_must_chain_parts_and_skipcol() {
        let base = wrapped_frame();
        let cases: [(&str, Value); 14] = [
            ("/windows/0/wrap_parts/0", json!(0)),
            ("/windows/0/skipcol", json!(0)),
            ("/windows/0/wrap_parts/1", json!(1)),
            ("/windows/0/wrap_rows/1", json!(4)),
            ("/windows/0/wrap_parts/5", json!(1)),
            ("/windows/0/wrap_parts/2", json!(1)),
            ("/windows/0/wrap_rows/5", json!(2)),
            ("/windows/0/wrap_rows", json!([3, 3, 1, 2, 2])),
            ("/windows/0/wrap_parts", json!([1, 2, 0, 0, 1])),
            ("/windows/0/wrap_rows/2", json!(5000)),
            ("/windows/0/overscan/1/part", json!(1)),
            ("/windows/0/overscan/2/part", json!(1)),
            ("/windows/0/overscan/3/part", json!(0)),
            ("/windows/0/overscan/3/parts", json!(3)),
        ];
        for (pointer, replacement) in cases {
            let mut value = base.clone();
            let (parent, key) = pointer.rsplit_once('/').unwrap();
            match value.pointer_mut(parent).unwrap() {
                Value::Array(items) => items[key.parse::<usize>().unwrap()] = replacement,
                object => {
                    object
                        .as_object_mut()
                        .unwrap()
                        .insert(key.into(), replacement);
                }
            }
            let frame: Vim9RenderFrame = serde_json::from_value(value).unwrap();
            assert!(!frame.valid(), "{pointer} must be rejected");
        }
        // A wrapped row is never a closed fold.
        let mut folded = base.clone();
        folded["windows"][0]["fold_ends"][2] = json!(6);
        folded["windows"][0]["wrap_rows"][2] = json!(1);
        let frame: Vim9RenderFrame = serde_json::from_value(folded.clone()).unwrap();
        assert!(frame.valid(), "a one-row fold row is still well formed");
        folded["windows"][0]["wrap_parts"][3] = json!(0);
        folded["windows"][0]["fold_ends"][3] = json!(7);
        folded["windows"][0]["wrap_rows"][3] = json!(2);
        let frame: Vim9RenderFrame = serde_json::from_value(folded).unwrap();
        assert!(!frame.valid(), "fold rows have exactly one part");
        // Fold-only (protocol 8) maps cannot carry wrap metadata, and legacy
        // producers cannot smuggle a smoothscroll offset or margin parts.
        let mut fold_only = base.clone();
        fold_only["windows"][0]
            .as_object_mut()
            .unwrap()
            .remove("wrap_parts");
        let frame: Vim9RenderFrame = serde_json::from_value(fold_only).unwrap();
        assert!(!frame.valid());
        let mut legacy = folded_frame();
        legacy["windows"][0]
            .as_object_mut()
            .unwrap()
            .retain(|key, _| !["display_lines", "fold_ends", "overscan"].contains(&key.as_str()));
        legacy["windows"][0]["botline"] = json!(24);
        assert!(serde_json::from_value::<Vim9RenderFrame>(legacy.clone())
            .unwrap()
            .valid());
        let mut skipped = legacy.clone();
        skipped["windows"][0]["skipcol"] = json!(40);
        assert!(!serde_json::from_value::<Vim9RenderFrame>(skipped)
            .unwrap()
            .valid());
        legacy["windows"][0]["overscan"] =
            json!([{"row":10,"line":25,"part":1,"parts":2,"byte_length":0,"runs":[]}]);
        assert!(!serde_json::from_value::<Vim9RenderFrame>(legacy)
            .unwrap()
            .valid());
    }

    fn patch(frame: &Value, row_updates: Value) -> Value {
        let mut delta = frame.clone();
        delta.as_object_mut().unwrap().remove("runs");
        delta["revision"] = json!(2);
        delta["base_revision"] = json!(1);
        delta["row_updates"] = row_updates;
        delta
    }

    #[test]
    fn split_paint_ownership_excludes_status_and_separators() {
        let frame: Vim9RenderFrame = serde_json::from_value(split_frame()).unwrap();
        assert!(frame.valid());
        assert_eq!(frame.paint_views().count(), 2);
        assert!(frame.covers(7, 79));
        assert!(!frame.covers(0, 39));
        assert!(!frame.covers(8, 0));
        for (property, value) in [("window", json!(100)), ("buffer", json!(0))] {
            let mut bad = split_frame();
            bad["windows"][1][property] = value;
            assert!(!serde_json::from_value::<Vim9RenderFrame>(bad)
                .unwrap()
                .valid());
        }
        let mut overlap = split_frame();
        overlap["windows"][1]["view"]["column"] = json!(38);
        assert!(!serde_json::from_value::<Vim9RenderFrame>(overlap)
            .unwrap()
            .valid());
        let mut leaked = split_frame();
        leaked["runs"][1]["window"] = json!(100);
        assert!(!serde_json::from_value::<Vim9RenderFrame>(leaked)
            .unwrap()
            .valid());
    }

    #[test]
    fn row_delta_preserves_peer_window_and_explicitly_clears_old_text() {
        let original = split_frame();
        let previous: Vim9RenderFrame = serde_json::from_value(original.clone()).unwrap();
        // Both panes share screen row zero and must travel in one replacement.
        let mut row = json!([original["runs"][0].clone(), original["runs"][1].clone()]);
        row[0]["text"] = json!("字e\u{301}");
        row[0]["cells"] = json!(3);
        row[0]["byte_end"] = json!(6);
        let delta: RenderDelta = serde_json::from_value(patch(
            &original,
            json!([{"row":0,"runs":row},{"row":1,"runs":[]}]),
        ))
        .unwrap();
        let next = delta.apply(&previous).unwrap();
        assert_eq!(next.runs.len(), 2);
        assert_eq!(next.runs[0].text, "字e\u{301}");
        assert_eq!(next.runs[1], previous.runs[1]);
        assert_eq!(
            previous.runs.len(),
            3,
            "prior immutable paint frame survives"
        );
    }

    #[test]
    fn cursor_only_delta_does_not_change_buffer_paint() {
        let original = split_frame();
        let previous = serde_json::from_value::<Vim9RenderFrame>(original.clone()).unwrap();
        let mut delta = patch(&original, json!([]));
        delta["cursor"]["column"] = json!(2);
        let next = serde_json::from_value::<RenderDelta>(delta)
            .unwrap()
            .apply(&previous)
            .unwrap();
        assert_eq!(next.runs, previous.runs);
        assert_eq!(next.cursor.column, 2);
    }

    #[test]
    fn deltas_reject_wrong_base_layout_duplicate_rows_and_cross_pane_paint() {
        let original = split_frame();
        let previous = serde_json::from_value::<Vim9RenderFrame>(original.clone()).unwrap();
        let empty = patch(&original, json!([]));
        for (key, value) in [
            ("base_revision", json!(0)),
            ("revision", json!(1)),
            ("columns", json!(79)),
            ("supported", json!(false)),
            ("buffer", json!(2)),
        ] {
            let mut bad = empty.clone();
            bad[key] = value;
            assert!(
                serde_json::from_value::<RenderDelta>(bad)
                    .unwrap()
                    .apply(&previous)
                    .is_none(),
                "{key}"
            );
        }
        for rows in [
            json!([{"row":1,"runs":[]},{"row":1,"runs":[]}]),
            json!([{"row":10,"runs":[]}]),
            json!([{"row":1,"runs":[original["runs"][0].clone()]}]),
            json!([{"row":0,"runs":[{"window":100,"row":0,"column":38,
                "text":"xx","cells":2}]}]),
            json!([{"row":0,"runs":[{"window":100,"row":0,"column":0,
                "text":"\u{0}","cells":1}]}]),
        ] {
            assert!(
                serde_json::from_value::<RenderDelta>(patch(&original, rows))
                    .unwrap()
                    .apply(&previous)
                    .is_none()
            );
        }
    }
}
