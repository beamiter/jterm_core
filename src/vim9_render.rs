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
    /// Last successfully applied source-bound scroll request in this split.
    pub scroll_ack: u64,
    /// Explicit visible source-line lengths, including empty and EOL targets.
    pub line_lengths: Vec<usize>,
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
}

impl Vim9RenderFrame {
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
                        || window.line_lengths.len() as u64
                            == window.botline.saturating_sub(window.topline) + 1)
                    && (!window.native_pointer || !window.line_lengths.is_empty())
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
                        window.line_count > 0
                            && row.line > 0
                            && row.line <= window.line_count
                            && source_line == Some(row.line)
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
