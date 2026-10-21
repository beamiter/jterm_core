//! Toolkit-independent policy and geometry for the native Vim buffer surface.
//!
//! Vim owns source bytes and display cells. Frontends own logical pixels and
//! overlay widgets. These types join the two without creating another editor.

use serde::{Deserialize, Serialize};

use super::{Vim9RenderCursor, Vim9RenderFrame, Vim9RenderRect, Vim9RenderRun};

/// The same `[vim9]` configuration in all four frontends. Optional workbench
/// controls must never decide whether the native buffer itself is enabled.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct Vim9UiConfig {
    pub native_buffer: bool,
    pub toolbar: bool,
    pub buffer_tabs: bool,
    pub status_bar: bool,
    pub project_panel: bool,
    pub tool_panel: bool,
}

impl Default for Vim9UiConfig {
    fn default() -> Self {
        Self {
            native_buffer: true,
            toolbar: false,
            buffer_tabs: false,
            status_bar: false,
            project_panel: false,
            tool_panel: false,
        }
    }
}

/// A rectangle in the host renderer's logical pixel coordinate space.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Vim9PixelRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Vim9PixelRect {
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.width && y < self.y + self.height
    }
}

/// Exact metrics of the visible buffer. Do not round each cell separately:
/// fractional font advances and DPI must agree for paint, input and overlays.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Vim9RenderMetrics {
    pub origin_x: f32,
    pub origin_y: f32,
    pub cell_width: f32,
    pub line_height: f32,
}

impl Vim9RenderMetrics {
    pub fn is_valid(&self) -> bool {
        self.origin_x.is_finite()
            && self.origin_y.is_finite()
            && self.cell_width.is_finite()
            && self.line_height.is_finite()
            && self.cell_width > 0.0
            && self.line_height > 0.0
    }

    pub fn rect(&self, view: &Vim9RenderRect) -> Option<Vim9PixelRect> {
        if !self.is_valid() || view.rows == 0 || view.columns == 0 {
            return None;
        }
        let rect = Vim9PixelRect {
            x: self.origin_x + view.column as f32 * self.cell_width,
            y: self.origin_y + view.row as f32 * self.line_height,
            width: view.columns as f32 * self.cell_width,
            height: view.rows as f32 * self.line_height,
        };
        [
            rect.x,
            rect.y,
            rect.width,
            rect.height,
            rect.x + rect.width,
            rect.y + rect.height,
        ]
        .into_iter()
        .all(f32::is_finite)
        .then_some(rect)
    }

    pub fn cell_rect(&self, row: usize, column: usize, cells: usize) -> Option<Vim9PixelRect> {
        self.rect(&Vim9RenderRect {
            row,
            column,
            rows: 1,
            columns: cells,
        })
    }

    pub fn cursor_rect(&self, cursor: &Vim9RenderCursor) -> Option<Vim9PixelRect> {
        cursor
            .visible
            .then(|| self.cell_rect(cursor.row, cursor.column, cursor.width))
            .flatten()
    }

    /// Convert host pixels to Vim cells. The caller still checks frame/view
    /// ownership; a status line or split separator is not buffer text.
    pub fn cell_at(&self, x: f32, y: f32) -> Option<(usize, usize)> {
        if !self.is_valid() || !x.is_finite() || !y.is_finite() {
            return None;
        }
        fn axis(pixel: f32, origin: f32, advance: f32) -> Option<usize> {
            let value = (f64::from(pixel) - f64::from(origin)) / f64::from(advance);
            if !(value >= 0.0 && value < usize::MAX as f64) {
                return None;
            }
            let mut index = value.floor() as usize;
            let boundary = |index: usize| origin + index as f32 * advance;
            // Invert the f32 boundary that was actually painted, not only the
            // ideal f64 quotient: 36.5f32 / 7.3f32 is slightly below five.
            if pixel < boundary(index) {
                index = index.checked_sub(1)?;
            } else if pixel >= boundary(index.checked_add(1)?) {
                index = index.checked_add(1)?;
            }
            let start = boundary(index);
            let end = boundary(index.checked_add(1)?);
            // Very large origins/indices can lose an entire cell to f32
            // precision. Reject ambiguous geometry instead of guessing.
            (start.is_finite() && end.is_finite() && end > start && pixel >= start && pixel < end)
                .then_some(index)
        }
        Some((
            axis(y, self.origin_y, self.line_height)?,
            axis(x, self.origin_x, self.cell_width)?,
        ))
    }
}

/// Source location used to re-resolve a hover/preview after scrolling or zoom.
/// The window identity is essential when two splits show the same buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Vim9BufferPosition {
    pub window: i64,
    pub buffer: i64,
    pub changedtick: u64,
    pub line: u64,
    /// Zero-based UTF-8 byte offset, never a character or display-cell index.
    pub byte_column: usize,
}

/// An immutable attachment point for a native overlay. A frontend owns the
/// content and lifetime of that overlay; Vim remains the editing authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Vim9RenderAnchor {
    pub revision: u64,
    pub window: i64,
    pub buffer: i64,
    pub changedtick: u64,
    pub line: u64,
    pub byte_start: usize,
    pub byte_end: usize,
    pub rect: Vim9RenderRect,
}

impl Vim9RenderAnchor {
    pub fn position(&self) -> Vim9BufferPosition {
        Vim9BufferPosition {
            window: self.window,
            buffer: self.buffer,
            changedtick: self.changedtick,
            line: self.line,
            byte_column: self.byte_start,
        }
    }
}

impl Vim9RenderFrame {
    fn source_identity(&self, window: i64) -> Option<(i64, i64, u64)> {
        if !self.supported {
            return None;
        }
        if self.windows.is_empty() {
            return (window == 0 || window == self.window).then_some((
                self.window,
                self.buffer,
                self.changedtick,
            ));
        }
        self.windows
            .iter()
            .find(|view| view.window == window)
            .map(|view| (view.window, view.buffer, view.changedtick))
    }

    fn run_anchor(&self, run: &Vim9RenderRun, offset: usize) -> Option<Vim9RenderAnchor> {
        let (window, buffer, changedtick) = self.source_identity(run.window)?;
        if run.line == 0 || run.byte_start >= run.byte_end || offset >= run.cells {
            return None;
        }
        // The bridge only coalesces ASCII with a one-byte/one-cell mapping.
        // Expanded tabs and Unicode graphemes keep their full source range.
        let plain = run.text.is_ascii()
            && run.text.len() == run.cells
            && run.byte_end - run.byte_start == run.cells;
        let (column, cells, byte_start, byte_end) = if plain {
            (
                run.column.checked_add(offset)?,
                1,
                run.byte_start.checked_add(offset)?,
                run.byte_start.checked_add(offset)?.checked_add(1)?,
            )
        } else {
            (run.column, run.cells, run.byte_start, run.byte_end)
        };
        Some(Vim9RenderAnchor {
            revision: self.revision,
            window,
            buffer,
            changedtick,
            line: run.line,
            byte_start,
            byte_end,
            rect: Vim9RenderRect {
                row: run.row,
                column,
                rows: 1,
                columns: cells,
            },
        })
    }

    /// Hit-test buffer text, ignoring gutters, row backgrounds, separators and
    /// terminal-owned command rows. Wide glyphs and tabs map to source bytes.
    pub fn anchor_at_cell(&self, row: usize, column: usize) -> Option<Vim9RenderAnchor> {
        if !self.supported || !self.covers(row, column) {
            return None;
        }
        self.runs
            .iter()
            .rev()
            .find_map(|run| {
                (run.row == row && column >= run.column && column - run.column < run.cells)
                    .then(|| self.run_anchor(run, column - run.column))
                    .flatten()
            })
            .or_else(|| {
                (self.cursor.visible && self.cursor.row == row && self.cursor.column == column)
                    .then(|| self.empty_cursor_anchor())
                    .flatten()
            })
    }

    /// Resolve a source location in this exact window and buffer generation.
    /// Returns None after edits, window closure, fallback or an off-screen move.
    pub fn anchor_for_position(&self, position: &Vim9BufferPosition) -> Option<Vim9RenderAnchor> {
        let identity = self.source_identity(position.window)?;
        if identity != (position.window, position.buffer, position.changedtick)
            || position.line == 0
        {
            return None;
        }
        self.runs
            .iter()
            .rev()
            .find_map(|run| {
                if self.source_identity(run.window) != Some(identity)
                    || run.line != position.line
                    || position.byte_column < run.byte_start
                    || position.byte_column >= run.byte_end
                {
                    return None;
                }
                let offset = if run.text.is_ascii()
                    && run.text.len() == run.cells
                    && run.byte_end - run.byte_start == run.cells
                {
                    position.byte_column - run.byte_start
                } else {
                    0
                };
                self.run_anchor(run, offset)
            })
            .or_else(|| {
                self.empty_cursor_anchor().filter(|anchor| {
                    anchor.window == position.window
                        && anchor.buffer == position.buffer
                        && anchor.changedtick == position.changedtick
                        && anchor.line == position.line
                        && anchor.byte_start == position.byte_column
                })
            })
    }

    fn empty_cursor_anchor(&self) -> Option<Vim9RenderAnchor> {
        let (window, buffer, changedtick) = self.source_identity(self.window)?;
        let cursor = &self.cursor;
        if !cursor.visible
            || cursor.line == 0
            || cursor.width == 0
            || !self.view.contains(cursor.row, cursor.column)
        {
            return None;
        }
        Some(Vim9RenderAnchor {
            revision: self.revision,
            window,
            buffer,
            changedtick,
            line: cursor.line,
            byte_start: cursor.byte_column,
            byte_end: cursor.byte_column,
            rect: Vim9RenderRect {
                row: cursor.row,
                column: cursor.column,
                rows: 1,
                columns: cursor.width,
            },
        })
    }

    /// A native card can attach even on an empty line or at end of insert text.
    pub fn cursor_anchor(&self) -> Option<Vim9RenderAnchor> {
        if !self.cursor.visible {
            return None;
        }
        self.anchor_at_cell(self.cursor.row, self.cursor.column)
    }

    /// Check before painting an asynchronous result. On a newer frame, resolve
    /// `anchor.position()` again; never paint the old pixel rectangle blindly.
    pub fn anchor_is_current(&self, anchor: &Vim9RenderAnchor) -> bool {
        self.revision == anchor.revision
            && self.source_identity(anchor.window)
                == Some((anchor.window, anchor.buffer, anchor.changedtick))
            && self.anchor_for_position(&anchor.position()).as_ref() == Some(anchor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn frame() -> Vim9RenderFrame {
        serde_json::from_value(json!({
            "revision": 4, "supported": true, "rows": 7, "columns": 41,
            "window": 11, "buffer": 7, "changedtick": 9, "mode": "i",
            "view": {"row":0,"column":0,"rows":5,"columns":20},
            "cursor": {"row":1,"column":2,"width":1,"visible":true,"line":2,"byte_column":0},
            "windows": [
                {"window":11,"buffer":7,"changedtick":9,"view":{"row":0,"column":0,"rows":5,"columns":20}},
                {"window":12,"buffer":7,"changedtick":9,"view":{"row":0,"column":21,"rows":5,"columns":20}}
            ],
            "runs": [
                {"window":11,"row":0,"column":0,"text":"1 ","cells":2,"line":0},
                {"window":11,"row":0,"column":2,"text":"abc","cells":3,"line":1,"byte_start":0,"byte_end":3},
                {"window":11,"row":0,"column":5,"text":"   ","cells":3,"line":1,"byte_start":3,"byte_end":4},
                {"window":11,"row":0,"column":8,"text":"界","cells":2,"line":1,"byte_start":4,"byte_end":7},
                {"window":11,"row":0,"column":10,"text":"é","cells":1,"line":1,"byte_start":7,"byte_end":10},
                {"window":11,"row":1,"column":0,"text":"                    ","cells":20,"line":2},
                {"window":12,"row":0,"column":23,"text":"abc","cells":3,"line":1,"byte_start":0,"byte_end":3}
            ]
        })).unwrap()
    }

    #[test]
    fn partial_config_keeps_native_buffer_and_opt_in_chrome_independent() {
        let defaults: Vim9UiConfig = toml::from_str("").unwrap();
        assert!(defaults.native_buffer);
        assert!(
            !defaults.toolbar
                && !defaults.buffer_tabs
                && !defaults.status_bar
                && !defaults.project_panel
                && !defaults.tool_panel
        );
        let optional: Vim9UiConfig = toml::from_str("project_panel = true\n").unwrap();
        assert!(optional.native_buffer && optional.project_panel);
        assert!(!optional.toolbar && !optional.tool_panel);
        let fallback: Vim9UiConfig =
            toml::from_str("native_buffer = false\ntoolbar = true\n").unwrap();
        assert!(!fallback.native_buffer && fallback.toolbar);
        assert_eq!(
            toml::from_str::<Vim9UiConfig>(&toml::to_string(&optional).unwrap()).unwrap(),
            optional
        );
    }

    #[test]
    fn fractional_geometry_roundtrips_paint_hit_and_zoom_without_rounding_drift() {
        for zoom in [0.75, 1.0, 1.25, 1.5, 2.0] {
            let metrics = Vim9RenderMetrics {
                origin_x: 13.25,
                origin_y: -4.5,
                cell_width: 8.25 * zoom,
                line_height: 17.5 * zoom,
            };
            let rect = metrics.cell_rect(4, 27, 2).unwrap();
            assert_eq!(rect.x, 13.25 + 27.0 * 8.25 * zoom);
            assert_eq!(rect.y, -4.5 + 4.0 * 17.5 * zoom);
            assert_eq!(
                metrics.cell_at(rect.x + rect.width * 0.25, rect.y + rect.height * 0.5),
                Some((4, 27))
            );
            assert_eq!(
                metrics.cell_at(rect.x + rect.width * 0.75, rect.y + rect.height * 0.5),
                Some((4, 28))
            );
            assert!(rect.contains(rect.x, rect.y));
            assert!(!rect.contains(rect.x + rect.width, rect.y));
            assert_eq!(
                metrics.cell_at(metrics.origin_x - 0.25, metrics.origin_y),
                None
            );
        }
    }

    #[test]
    fn invalid_metrics_never_produce_paint_or_mouse_geometry() {
        let good = Vim9RenderMetrics {
            origin_x: 0.0,
            origin_y: 0.0,
            cell_width: 8.0,
            line_height: 16.0,
        };
        for width in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            let bad = Vim9RenderMetrics {
                cell_width: width,
                ..good
            };
            assert!(!bad.is_valid());
            assert!(bad.cell_rect(0, 0, 1).is_none());
            assert!(bad.cell_at(1.0, 1.0).is_none());
        }
        assert!(good.cell_rect(0, 0, 0).is_none());
        assert!(good.cell_at(f32::INFINITY, 0.0).is_none());
        assert!(good.cell_at(f32::MAX, f32::MAX).is_none());
        assert!(Vim9RenderMetrics {
            origin_x: f32::NAN,
            ..good
        }
        .cell_at(0.0, 0.0)
        .is_none());
        let overflow = Vim9RenderMetrics {
            cell_width: f32::MAX,
            ..good
        };
        assert!(overflow.cell_rect(0, 2, 2).is_none());
    }

    #[test]
    fn exact_painted_edges_map_to_the_correct_fractional_cell() {
        for origin in [0.0, 13.25, -4.5] {
            for advance in [7.3, 8.1, 8.25, 11.7] {
                let metrics = Vim9RenderMetrics {
                    origin_x: origin,
                    origin_y: origin,
                    cell_width: advance,
                    line_height: advance * 1.7,
                };
                for index in [0, 1, 5, 27, 79, 399, 4095] {
                    let rect = metrics.cell_rect(index, index, 1).unwrap();
                    assert_eq!(
                        metrics.cell_at(rect.x, rect.y),
                        Some((index, index)),
                        "origin={origin}, advance={advance}, index={index}"
                    );
                    if index > 0 {
                        assert_eq!(
                            metrics.cell_at(rect.x - 0.01, rect.y - 0.01),
                            Some((index - 1, index - 1))
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn anchors_preserve_ascii_tabs_wide_and_combining_source_bytes() {
        let frame = frame();
        for (column, start, end, width) in [
            (2, 0, 1, 1),
            (3, 1, 2, 1),
            (4, 2, 3, 1),
            (5, 3, 4, 3),
            (6, 3, 4, 3),
            (7, 3, 4, 3),
            (8, 4, 7, 2),
            (9, 4, 7, 2),
            (10, 7, 10, 1),
        ] {
            let anchor = frame.anchor_at_cell(0, column).unwrap();
            assert_eq!(
                (anchor.byte_start, anchor.byte_end, anchor.rect.columns),
                (start, end, width)
            );
            assert_eq!(
                (
                    anchor.window,
                    anchor.buffer,
                    anchor.changedtick,
                    anchor.line
                ),
                (11, 7, 9, 1)
            );
            assert!(frame.anchor_is_current(&anchor));
            assert_eq!(frame.anchor_for_position(&anchor.position()), Some(anchor));
        }
        for byte in 4..7 {
            let position = Vim9BufferPosition {
                window: 11,
                buffer: 7,
                changedtick: 9,
                line: 1,
                byte_column: byte,
            };
            let anchor = frame.anchor_for_position(&position).unwrap();
            assert_eq!(
                (anchor.byte_start, anchor.byte_end, anchor.rect.columns),
                (4, 7, 2)
            );
        }
    }

    #[test]
    fn anchors_ignore_gutters_backgrounds_status_and_split_separators() {
        let frame = frame();
        for (row, column) in [
            (0, 0),
            (0, 1),
            (0, 11),
            (0, 20),
            (1, 3),
            (5, 2),
            (6, 2),
            (7, 2),
        ] {
            assert_eq!(frame.anchor_at_cell(row, column), None, "{row}:{column}");
        }
        let left = frame.anchor_at_cell(0, 3).unwrap();
        let right = frame.anchor_at_cell(0, 24).unwrap();
        assert_eq!(
            (left.buffer, left.byte_start),
            (right.buffer, right.byte_start)
        );
        assert_ne!(left.window, right.window);
        assert_eq!(frame.anchor_for_position(&right.position()), Some(right));
    }

    #[test]
    fn cursor_anchors_cover_blank_lines_and_insert_end_without_fabricating_text() {
        let mut frame = frame();
        let blank = frame.cursor_anchor().unwrap();
        assert_eq!((blank.line, blank.byte_start, blank.byte_end), (2, 0, 0));
        assert!(frame.anchor_is_current(&blank));
        frame.cursor.row = 0;
        frame.cursor.column = 11;
        frame.cursor.line = 1;
        frame.cursor.byte_column = 10;
        let end = frame.cursor_anchor().unwrap();
        assert_eq!((end.line, end.byte_start, end.byte_end), (1, 10, 10));
        assert_eq!(frame.anchor_for_position(&end.position()), Some(end));
        frame.cursor.visible = false;
        assert!(frame.cursor_anchor().is_none());
    }

    #[test]
    fn overlay_anchors_rebind_after_scroll_and_expire_on_edit_close_or_fallback() {
        let mut frame = frame();
        let anchor = frame.anchor_at_cell(0, 3).unwrap();
        frame.revision += 1;
        frame.cursor.visible = false;
        for run in &mut frame.runs {
            if run.line == 1 {
                run.row = 2;
            }
        }
        assert!(!frame.anchor_is_current(&anchor));
        let moved = frame.anchor_for_position(&anchor.position()).unwrap();
        assert_eq!(moved.rect.row, 2);
        assert!(frame.anchor_is_current(&moved));
        frame.windows[0].changedtick += 1;
        assert!(frame.anchor_for_position(&anchor.position()).is_none());
        frame.windows.remove(0);
        assert!(frame.anchor_for_position(&anchor.position()).is_none());
        frame.supported = false;
        assert!(frame.anchor_at_cell(2, 24).is_none());
        assert!(frame.cursor_anchor().is_none());
    }

    #[test]
    fn legacy_single_view_frames_still_provide_text_anchors() {
        let mut frame = frame();
        frame.windows.clear();
        frame.runs.retain(|run| run.window == 11);
        for run in &mut frame.runs {
            run.window = 0;
        }
        let anchor = frame.anchor_at_cell(0, 3).unwrap();
        assert_eq!(anchor.window, 11);
        assert!(frame.anchor_is_current(&anchor));
    }
}
