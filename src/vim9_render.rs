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
    /// Source-bound scrolling is independent of mouse mappings, mouse options
    /// and virtual editing. False for producers predating this capability.
    pub native_scroll: bool,
    /// Last successfully applied source-bound scroll request in this split.
    pub scroll_ack: u64,
    /// Explicit visible source-line lengths, including empty and EOL targets.
    pub line_lengths: Vec<usize>,
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
                        || window.line_lengths.len() as u64
                            == window.botline.saturating_sub(window.topline) + 1)
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
