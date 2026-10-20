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

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Vim9RenderCursor {
    pub row: usize,
    pub column: usize,
    /// Vim display cells, including wide-character geometry.
    pub width: usize,
    pub visible: bool,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Vim9RenderRun {
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
    /// Plain ASCII with equal style may be coalesced. Each non-ASCII grapheme
    /// remains separate so font fallback cannot shift later cursor geometry.
    pub runs: Vec<Vim9RenderRun>,
}

impl Vim9RenderFrame {
    pub(super) fn valid(&self) -> bool {
        fn style_valid(style: &Vim9RenderStyle) -> bool {
            [style.foreground, style.background, style.special]
                .into_iter()
                .flatten()
                .all(|color| color <= 0xff_ffff)
        }
        let within =
            |start: usize, count: usize, limit: usize| start <= limit && count <= limit - start;
        self.rows > 0
            && self.rows <= 4096
            && self.columns > 0
            && self.columns <= 4096
            && (!self.supported
                || (self.window > 0
                    && self.buffer > 0
                    && self.view.rows > 0
                    && self.view.columns > 0))
            && self.runs.len() <= 24_000
            && style_valid(&self.default_style)
            && within(self.view.row, self.view.rows, self.rows)
            && within(self.view.column, self.view.columns, self.columns)
            && (!self.cursor.visible
                || (self.cursor.row < self.rows
                    && self.cursor.width > 0
                    && within(self.cursor.column, self.cursor.width, self.columns)))
            && self.runs.iter().all(|run| {
                run.row >= self.view.row
                    && run.row < self.view.row + self.view.rows
                    && run.column >= self.view.column
                    && run.cells > 0
                    && within(run.column, run.cells, self.view.column + self.view.columns)
                    && run.byte_start <= run.byte_end
                    && run.text.len() <= 64 * 1024
                    && !run.text.contains(['\n', '\r', '\t', '\u{1b}'])
                    && style_valid(&run.style)
            })
    }
}
