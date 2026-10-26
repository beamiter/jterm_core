//! One clipped pixel transform for native paint, source hits and caret/IME.
//! The immutable Vim frame is authoritative. Only supplied semantic margin
//! rows may be exposed; this module never extrapolates document text.

use super::{
    Vim9BufferPosition, Vim9PixelRect, Vim9RenderFrame, Vim9RenderMetrics, Vim9RenderRun,
    Vim9RenderWindow,
};

#[derive(Clone, Copy, Debug)]
pub struct Vim9ProjectedRun<'a> {
    pub run: &'a Vim9RenderRun,
    /// Unclipped glyph geometry. Do not stretch a partly visible row.
    pub rect: Vim9PixelRect,
    /// The owning split's intersection with this run.
    pub clip: Vim9PixelRect,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Vim9ViewportAnchor {
    pub revision: u64,
    pub position: Vim9BufferPosition,
    pub byte_end: usize,
    pub rect: Vim9PixelRect,
    pub clip: Vim9PixelRect,
}

impl Vim9ViewportAnchor {
    /// Asynchronous overlays must re-resolve their source on a newer frame.
    pub fn is_current(&self, frame: &Vim9RenderFrame) -> bool {
        frame.supported
            && frame.revision == self.revision
            && frame.windows.iter().any(|window| {
                (window.window, window.buffer, window.changedtick)
                    == (
                        self.position.window,
                        self.position.buffer,
                        self.position.changedtick,
                    )
            })
    }
}

/// A presentation of one split at a fractional logical-pixel offset. Hosts
/// must use this same instance for paint and input/overlay coordinates.
/// Admission of editing or mouse commands remains the frontend's responsibility.
#[derive(Clone, Copy, Debug)]
pub struct Vim9Viewport<'a> {
    frame: &'a Vim9RenderFrame,
    window: &'a Vim9RenderWindow,
    metrics: Vim9RenderMetrics,
    offset_y: f32,
    clip: Vim9PixelRect,
}

fn intersect(a: Vim9PixelRect, b: Vim9PixelRect) -> Option<Vim9PixelRect> {
    let x = a.x.max(b.x);
    let y = a.y.max(b.y);
    let width = (a.x + a.width).min(b.x + b.width) - x;
    let height = (a.y + a.height).min(b.y + b.height) - y;
    (width > 0.0 && height > 0.0).then_some(Vim9PixelRect {
        x,
        y,
        width,
        height,
    })
}

impl<'a> Vim9Viewport<'a> {
    /// Positive offsets move content down. A missing margin, unsupported
    /// frame or invalid geometry returns None: snap/fallback, never invent rows.
    pub fn new(
        frame: &'a Vim9RenderFrame,
        window: i64,
        metrics: Vim9RenderMetrics,
        offset_y: f32,
    ) -> Option<Self> {
        if !frame.supported || !metrics.is_valid() || !offset_y.is_finite() {
            return None;
        }
        let window = frame.windows.iter().find(|item| item.window == window)?;
        let clip = metrics.rect(&window.view)?;
        let rows = (offset_y.abs() / metrics.line_height).ceil();
        if rows > 8.0 {
            return None;
        }
        for distance in 1..=rows as i32 {
            let row = if offset_y > 0.0 {
                -distance
            } else {
                i32::try_from(window.view.rows)
                    .ok()?
                    .checked_add(distance - 1)?
            };
            if !window.overscan.iter().any(|item| item.row == row) {
                return None;
            }
        }
        Some(Self {
            frame,
            window,
            metrics,
            offset_y,
            clip,
        })
    }

    pub fn clip(&self) -> Vim9PixelRect {
        self.clip
    }

    fn row_rect(&self, row: i64, column: usize, cells: usize) -> Option<Vim9PixelRect> {
        if cells == 0
            || column < self.window.view.column
            || column - self.window.view.column >= self.window.view.columns
            || cells > self.window.view.columns - (column - self.window.view.column)
        {
            return None;
        }
        let rect = Vim9PixelRect {
            x: self.metrics.origin_x + column as f32 * self.metrics.cell_width,
            y: self.clip.y + row as f32 * self.metrics.line_height + self.offset_y,
            width: cells as f32 * self.metrics.cell_width,
            height: self.metrics.line_height,
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

    /// Visible and overscanned text keep their original layer order. Painters
    /// use `rect` for layout and `clip` for scissoring, including partial rows.
    pub fn runs(&self) -> impl DoubleEndedIterator<Item = Vim9ProjectedRun<'a>> + '_ {
        let window = self.window;
        let visible = self
            .frame
            .runs
            .iter()
            .filter(move |run| {
                run.window == window.window && window.view.contains(run.row, run.column)
            })
            .map(move |run| (run, run.row as i64 - window.view.row as i64));
        let margins = window
            .overscan
            .iter()
            .flat_map(|row| row.runs.iter().map(move |run| (run, i64::from(row.row))));
        visible.chain(margins).filter_map(move |(run, row)| {
            let rect = self.row_rect(row, run.column, run.cells)?;
            Some(Vim9ProjectedRun {
                run,
                rect,
                clip: intersect(rect, self.clip)?,
            })
        })
    }

    /// The caret and OS input-method candidate rectangle share paint's exact
    /// offset and split clipping. A fully hidden caret has no candidate anchor.
    pub fn cursor_rect(&self) -> Option<Vim9PixelRect> {
        let cursor = &self.window.cursor;
        if !cursor.visible || !self.window.view.contains(cursor.row, cursor.column) {
            return None;
        }
        let rect = self.row_rect(
            cursor.row as i64 - self.window.view.row as i64,
            cursor.column,
            cursor.width,
        )?;
        intersect(rect, self.clip)
    }

    fn anchor(&self, projected: Vim9ProjectedRun<'a>, offset: usize) -> Option<Vim9ViewportAnchor> {
        let run = projected.run;
        if run.line == 0 || run.byte_start >= run.byte_end || offset >= run.cells {
            return None;
        }
        let plain = run.text.is_ascii()
            && run.text.len() == run.cells
            && run.byte_end - run.byte_start == run.cells;
        let (start, end, rect) = if plain {
            let start = run.byte_start.checked_add(offset)?;
            (
                start,
                start.checked_add(1)?,
                Vim9PixelRect {
                    x: self.metrics.origin_x
                        + run.column.checked_add(offset)? as f32 * self.metrics.cell_width,
                    width: self.metrics.cell_width,
                    ..projected.rect
                },
            )
        } else {
            (run.byte_start, run.byte_end, projected.rect)
        };
        Some(Vim9ViewportAnchor {
            revision: self.frame.revision,
            position: Vim9BufferPosition {
                window: self.window.window,
                buffer: self.window.buffer,
                changedtick: self.window.changedtick,
                line: run.line,
                byte_column: start,
            },
            byte_end: end,
            rect,
            clip: intersect(rect, self.clip)?,
        })
    }

    /// Source-byte hit testing in precisely the image the user is seeing.
    /// Gutters, separators, background-only runs and off-screen pixels are not text.
    pub fn anchor_at(&self, x: f32, y: f32) -> Option<Vim9ViewportAnchor> {
        if !x.is_finite() || !y.is_finite() || !self.clip.contains(x, y) {
            return None;
        }
        self.runs()
            .rev()
            .find_map(|projected| {
                if !projected.clip.contains(x, y) {
                    return None;
                }
                // Invert the original painted column boundaries, not a separately
                // rounded copy of the width of a shaped/fallback glyph.
                let metrics = Vim9RenderMetrics {
                    origin_y: projected.rect.y,
                    ..self.metrics
                };
                let (_, column) = metrics.cell_at(x, y)?;
                self.anchor(projected, column.checked_sub(projected.run.column)?)
            })
            .or_else(|| self.empty_anchor_at(x, y))
    }

    /// Empty-line and EOL targets. Closed fold rows show display-only fold
    /// text and never expose an editable source endpoint; a wrapped line ends
    /// on its last continuation row only.
    fn source_rows(&self) -> impl Iterator<Item = (i64, u64, usize)> + '_ {
        self.window
            .visible_rows()
            .zip(self.window.line_lengths.iter())
            .filter(|(row, _)| row.ends_line())
            .map(|(row, length)| (row.row, row.line, *length))
            .chain(
                self.window
                    .overscan
                    .iter()
                    .filter(|row| row.fold_end == 0 && row.part + 1 >= row.parts.max(1))
                    .map(|row| (i64::from(row.row), row.line, row.byte_length)),
            )
    }

    fn end_anchor(&self, row: i64, line: u64, length: usize) -> Option<Vim9ViewportAnchor> {
        let mut rect = self.row_rect(
            row,
            self.window.view.column.checked_add(self.window.textoff)?,
            1,
        )?;
        if length > 0 {
            rect.x = self
                .runs()
                .filter(|p| {
                    p.run.line == line
                        && p.run.byte_end == length
                        && p.run.byte_start < p.run.byte_end
                })
                .map(|p| p.rect.x + p.rect.width)
                .reduce(f32::max)?;
        }
        Some(Vim9ViewportAnchor {
            revision: self.frame.revision,
            position: Vim9BufferPosition {
                window: self.window.window,
                buffer: self.window.buffer,
                changedtick: self.window.changedtick,
                line,
                byte_column: length,
            },
            byte_end: length,
            rect,
            clip: intersect(rect, self.clip)?,
        })
    }

    fn empty_anchor_at(&self, x: f32, y: f32) -> Option<Vim9ViewportAnchor> {
        self.source_rows().find_map(|(row, line, length)| {
            let anchor = self.end_anchor(row, line, length)?;
            (x >= anchor.rect.x && y >= anchor.clip.y && y < anchor.clip.y + anchor.clip.height)
                .then_some(anchor)
        })
    }

    /// Re-resolve a delayed card/hover by source bytes and buffer generation.
    pub fn anchor_for_position(&self, position: &Vim9BufferPosition) -> Option<Vim9ViewportAnchor> {
        if (position.window, position.buffer, position.changedtick)
            != (
                self.window.window,
                self.window.buffer,
                self.window.changedtick,
            )
        {
            return None;
        }
        self.runs()
            .rev()
            .find_map(|projected| {
                let run = projected.run;
                if run.line != position.line
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
                self.anchor(projected, offset)
            })
            .or_else(|| {
                self.source_rows().find_map(|(row, line, length)| {
                    if line == position.line && length == position.byte_column {
                        self.end_anchor(row, line, length)
                    } else {
                        None
                    }
                })
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vim9::Vim9RenderMarginRow;
    use serde_json::json;

    fn frame() -> Vim9RenderFrame {
        serde_json::from_value(json!({
            "revision": 7, "supported": true, "rows": 6, "columns": 20,
            "window": 10, "buffer": 1, "changedtick": 3,
            "view": {"row":0,"column":0,"rows":4,"columns":8},
            "windows": [{"window":10,"buffer":1,"changedtick":3,"topline":10,"botline":13,
                "line_count":30,"textoff":0,
                "view":{"row":0,"column":0,"rows":4,"columns":8},
                "cursor":{"row":1,"column":1,"width":2,"visible":true,"line":11,"byte_column":0},
                "overscan":[
                    {"row":-2,"line":8,"byte_length":3,"runs":[{"window":10,"row":0,"column":0,"cells":3,"text":"old","line":8,"byte_start":0,"byte_end":3}]},
                    {"row":-1,"line":9,"byte_length":3,"runs":[{"window":10,"row":0,"column":0,"cells":3,"text":"pre","line":9,"byte_start":0,"byte_end":3}]},
                    {"row":4,"line":14,"byte_length":4,"runs":[{"window":10,"row":0,"column":0,"cells":4,"text":"post","line":14,"byte_start":0,"byte_end":4}]},
                    {"row":5,"line":15,"byte_length":0,"runs":[]}
                ]}],
            "runs":[
                {"window":10,"row":0,"column":0,"cells":2,"text":"AB","line":10,"byte_start":0,"byte_end":2},
                {"window":10,"row":1,"column":1,"cells":2,"text":"界","line":11,"byte_start":0,"byte_end":3},
                {"window":10,"row":2,"column":0,"cells":4,"text":"    ","line":12,"byte_start":0,"byte_end":1}
            ]
        })).unwrap()
    }

    fn metrics() -> Vim9RenderMetrics {
        Vim9RenderMetrics {
            origin_x: 0.0,
            origin_y: 0.0,
            cell_width: 8.0,
            line_height: 20.0,
        }
    }

    #[test]
    fn partial_bottom_row_paint_and_hit_use_one_transform() {
        let frame = frame();
        assert!(frame.valid());
        let view = Vim9Viewport::new(&frame, 10, metrics(), -6.25).unwrap();
        let row = view.runs().find(|run| run.run.line == 14).unwrap();
        assert_eq!(row.rect.y, 73.75);
        assert_eq!(row.clip.height, 6.25);
        let hit = view.anchor_at(9.0, 75.0).unwrap();
        assert_eq!((hit.position.line, hit.position.byte_column), (14, 1));
        assert_eq!(view.anchor_for_position(&hit.position), Some(hit.clone()));
        assert!(hit.is_current(&frame));
        assert!(view.anchor_at(9.0, 80.0).is_none());
        assert!(view.anchor_at(64.0, 75.0).is_none());
        assert_eq!(view.cursor_rect().unwrap().y, 13.75);
    }

    #[test]
    fn closed_fold_rows_paint_but_never_become_source_targets() {
        // Row 2 is a closed fold 12..=20; rows below it continue at line 21.
        let mut frame = frame();
        let window = &mut frame.windows[0];
        window.botline = 21;
        window.display_lines = vec![10, 11, 12, 21];
        window.fold_ends = vec![0, 0, 20, 0];
        window.line_lengths = vec![2, 3, 0, 0];
        window.overscan.retain(|row| row.row < 0);
        window.overscan.push(Vim9RenderMarginRow {
            row: 4,
            line: 22,
            fold_end: 25,
            byte_length: 6,
            ..Default::default()
        });
        frame.runs[2] = Vim9RenderRun {
            window: 10,
            row: 2,
            column: 0,
            cells: 8,
            text: "+-- 9 li".into(),
            ..Default::default()
        };
        assert!(frame.valid());
        let view = Vim9Viewport::new(&frame, 10, metrics(), 0.0).unwrap();
        assert!(view.runs().any(|run| run.run.text.starts_with("+--")));
        // Neither the fold text nor its EOL position addresses a hidden line.
        assert!(view.anchor_at(4.0, 45.0).is_none());
        assert!(view.anchor_at(70.0, 45.0).is_none());
        let empty = view.anchor_at(4.0, 65.0).unwrap();
        assert_eq!((empty.position.line, empty.position.byte_column), (21, 0));
        let hidden = Vim9BufferPosition {
            line: 15,
            ..empty.position
        };
        assert!(view.anchor_for_position(&hidden).is_none());
        assert_eq!(frame.windows[0].display_row_of(15), Some(2));
        assert_eq!(frame.windows[0].display_row_of(24), Some(4));
    }

    #[test]
    fn wrapped_line_ends_only_on_its_last_continuation_row() {
        // Line 10 wraps onto rows 0 and 1; 11 and 12 follow on rows 2 and 3.
        let mut frame = frame();
        let window = &mut frame.windows[0];
        window.botline = 12;
        window.display_lines = vec![10, 10, 11, 12];
        window.fold_ends = vec![0; 4];
        window.wrap_parts = vec![0, 1, 0, 0];
        window.wrap_rows = vec![2, 2, 1, 1];
        window.line_lengths = vec![4, 4, 3, 1];
        window.cursor.row = 2;
        window.overscan.retain(|row| row.row < 0);
        window.overscan.push(Vim9RenderMarginRow {
            row: 4,
            line: 13,
            ..Default::default()
        });
        frame.runs[1].row = 2;
        frame.runs[2].row = 3;
        frame.runs.insert(
            1,
            Vim9RenderRun {
                window: 10,
                row: 1,
                column: 0,
                cells: 2,
                text: "CD".into(),
                line: 10,
                byte_start: 2,
                byte_end: 4,
                ..Default::default()
            },
        );
        assert!(frame.valid());
        let view = Vim9Viewport::new(&frame, 10, metrics(), 0.0).unwrap();
        let hit = view.anchor_at(12.0, 25.0).unwrap();
        assert_eq!((hit.position.line, hit.position.byte_column), (10, 3));
        // Space right of the first part is not the end of line 10.
        assert!(view.anchor_at(40.0, 5.0).is_none());
        let end = view.anchor_at(40.0, 25.0).unwrap();
        assert_eq!((end.position.line, end.position.byte_column), (10, 4));
        assert_eq!(end.rect.y, 20.0);
        assert_eq!(end.rect.x, 16.0);
        assert_eq!(view.anchor_for_position(&end.position), Some(end));
    }

    #[test]
    fn negative_margin_row_is_visible_at_the_screen_origin() {
        let frame = frame();
        let view = Vim9Viewport::new(&frame, 10, metrics(), 6.25).unwrap();
        let row = view.runs().find(|run| run.run.line == 9).unwrap();
        assert_eq!(row.rect.y, -13.75);
        assert_eq!(row.clip.y, 0.0);
        assert_eq!(row.clip.height, 6.25);
        assert_eq!(view.anchor_at(1.0, 1.0).unwrap().position.line, 9);
        assert_eq!(view.anchor_at(1.0, 6.25).unwrap().position.line, 10);
    }

    #[test]
    fn unicode_and_expanded_tabs_preserve_source_bytes() {
        let frame = frame();
        let view = Vim9Viewport::new(&frame, 10, metrics(), -0.5).unwrap();
        for x in [8.5, 16.5, 23.5] {
            let hit = view.anchor_at(x, 25.0).unwrap();
            assert_eq!(
                (hit.position.line, hit.position.byte_column, hit.byte_end),
                (11, 0, 3)
            );
        }
        for x in [0.5, 8.5, 24.5] {
            let hit = view.anchor_at(x, 45.0).unwrap();
            assert_eq!(
                (hit.position.line, hit.position.byte_column, hit.byte_end),
                (12, 0, 1)
            );
        }
    }

    #[test]
    fn visible_whitespace_glyphs_keep_original_bytes_and_eol_target() {
        let mut frame = frame();
        frame.windows[0].line_lengths = vec![2, 3, 1, 0];
        frame.runs.retain(|run| run.line != 10);
        // The source is a space followed by one Tab. Display markers may use
        // multiple UTF-8 bytes, while every expanded Tab cell owns one byte.
        for (column, text, start, end) in [
            (0, "·", 0, 1),
            (1, "»", 1, 2),
            (2, "·", 1, 2),
            (3, "·", 1, 2),
            (4, "↲", 2, 2),
        ] {
            frame.runs.push(super::super::Vim9RenderRun {
                window: 10,
                row: 0,
                column,
                cells: 1,
                text: text.into(),
                line: 10,
                byte_start: start,
                byte_end: end,
                ..Default::default()
            });
        }
        assert!(frame.valid());
        let view = Vim9Viewport::new(&frame, 10, metrics(), -6.25).unwrap();
        for (x, start, end) in [(1.0, 0, 1), (9.0, 1, 2), (17.0, 1, 2), (25.0, 1, 2)] {
            let hit = view.anchor_at(x, 5.0).unwrap();
            assert_eq!(
                (hit.position.line, hit.position.byte_column, hit.byte_end),
                (10, start, end)
            );
            assert_eq!(
                view.anchor_for_position(&hit.position).unwrap().position,
                hit.position,
            );
        }
        for x in [33.0, 49.0] {
            let end = view.anchor_at(x, 5.0).unwrap();
            assert_eq!(
                (end.position.line, end.position.byte_column, end.byte_end),
                (10, 2, 2)
            );
            assert_eq!(
                end.rect.x, 32.0,
                "The EOL marker must not extend source text"
            );
        }
    }

    #[test]
    fn geometry_and_missing_or_insufficient_margins_fail_closed() {
        let mut frame = frame();
        for offset in [f32::NAN, f32::INFINITY, 40.01, -40.01] {
            assert!(Vim9Viewport::new(&frame, 10, metrics(), offset).is_none());
        }
        assert!(Vim9Viewport::new(&frame, 999, metrics(), 0.0).is_none());
        let bad = Vim9RenderMetrics {
            line_height: 0.0,
            ..metrics()
        };
        assert!(Vim9Viewport::new(&frame, 10, bad, 0.0).is_none());
        frame.windows[0].overscan.retain(|row| row.row != -1);
        assert!(Vim9Viewport::new(&frame, 10, metrics(), 1.0).is_none());
        assert!(Vim9Viewport::new(&frame, 10, metrics(), 0.0).is_some());
        frame.supported = false;
        assert!(Vim9Viewport::new(&frame, 10, metrics(), 0.0).is_none());
    }

    #[test]
    fn a_later_generation_or_revision_cannot_reuse_an_old_anchor() {
        let mut frame = frame();
        let anchor = Vim9Viewport::new(&frame, 10, metrics(), -3.0)
            .unwrap()
            .anchor_at(1.0, 78.0)
            .unwrap();
        frame.revision += 1;
        assert!(!anchor.is_current(&frame));
        frame.windows[0].changedtick += 1;
        assert!(Vim9Viewport::new(&frame, 10, metrics(), -3.0)
            .unwrap()
            .anchor_for_position(&anchor.position)
            .is_none());
    }

    #[test]
    fn fractional_metrics_invert_the_painted_ascii_boundary() {
        let frame = frame();
        let geometry = Vim9RenderMetrics {
            origin_x: 1.3,
            origin_y: 2.7,
            cell_width: 7.3,
            line_height: 18.7,
        };
        let view = Vim9Viewport::new(&frame, 10, geometry, -0.25).unwrap();
        let hit = view
            .anchor_at(geometry.origin_x + geometry.cell_width, 3.0)
            .unwrap();
        assert_eq!(hit.position.byte_column, 1);
    }

    #[test]
    fn explicitly_empty_margin_has_a_source_anchor() {
        let frame = frame();
        let view = Vim9Viewport::new(&frame, 10, metrics(), -25.0).unwrap();
        let hit = view.anchor_at(1.0, 77.0).unwrap();
        assert_eq!(
            (hit.position.line, hit.position.byte_column, hit.byte_end),
            (15, 0, 0)
        );
        assert_eq!(view.anchor_for_position(&hit.position), Some(hit));
    }

    #[test]
    fn malformed_margins_are_rejected_by_the_protocol_validator() {
        let original = frame();
        for case in 0..8 {
            let mut frame = original.clone();
            let window = &mut frame.windows[0];
            match case {
                0 => window.overscan[0].row = -9,
                1 => window.overscan[0].line = 7,
                2 => window.overscan[0].runs[0].window = 999,
                3 => window.overscan[0].runs[0].row = 1,
                4 => window.overscan[0].runs[0].cells = 9,
                5 => window.overscan[0].runs[0].byte_end = 4,
                6 => window.overscan[0] = window.overscan[1].clone(),
                _ => window.line_count = 12,
            }
            assert!(!frame.valid(), "case {case}");
        }
    }

    #[test]
    fn explicit_visible_empty_rows_and_eol_are_source_targets() {
        let mut frame = frame();
        frame.windows[0].line_lengths = vec![2, 3, 1, 0];
        let view = Vim9Viewport::new(&frame, 10, metrics(), -0.5).unwrap();
        let empty = view.anchor_at(45.0, 65.0).unwrap();
        assert_eq!((empty.position.line, empty.position.byte_column), (13, 0));
        assert_eq!(view.anchor_for_position(&empty.position), Some(empty));
        let end = view.anchor_at(45.0, 5.0).unwrap();
        assert_eq!((end.position.line, end.position.byte_column), (10, 2));
        assert_eq!(view.anchor_for_position(&end.position), Some(end));
        assert!(view.anchor_at(65.0, 65.0).is_none());
    }

    #[test]
    fn display_only_column_backgrounds_do_not_capture_source_hits() {
        let mut frame = frame();
        frame.windows[0].line_lengths = vec![2, 3, 1, 0];
        // Exercise decoration on either side of the text in paint order.
        // Pure backgrounds own display cells, never a source byte range.
        for row in 0..4 {
            let decoration = super::super::Vim9RenderRun {
                window: 10,
                row,
                column: 0,
                cells: 8,
                style: super::super::Vim9RenderStyle {
                    background: Some(0x123456),
                    ..Default::default()
                },
                ..Default::default()
            };
            frame.runs.insert(0, decoration.clone());
            frame.runs.push(decoration);
        }
        frame.windows[0].overscan[2]
            .runs
            .push(super::super::Vim9RenderRun {
                window: 10,
                column: 1,
                cells: 1,
                style: super::super::Vim9RenderStyle {
                    background: Some(0x654321),
                    ..Default::default()
                },
                ..Default::default()
            });
        assert!(frame.valid());
        let view = Vim9Viewport::new(&frame, 10, metrics(), -6.25).unwrap();
        for (x, y, line, byte, end) in [
            (8.5, 5.0, 10, 1, 2),
            (16.5, 25.0, 11, 0, 3),
            (24.5, 45.0, 12, 0, 1),
            (45.0, 65.0, 13, 0, 0),
            (45.0, 5.0, 10, 2, 2),
            (9.0, 75.0, 14, 1, 2),
        ] {
            let hit = view.anchor_at(x, y).unwrap();
            assert_eq!(
                (hit.position.line, hit.position.byte_column, hit.byte_end),
                (line, byte, end)
            );
            assert_eq!(view.anchor_for_position(&hit.position), Some(hit));
        }
    }

    #[test]
    fn inline_labels_shift_source_anchors_without_becoming_source_text() {
        let mut frame = frame();
        frame.windows[0].line_lengths = vec![2, 3, 1, 0];
        frame.windows[0].native_pointer = false;
        frame.windows[0].native_scroll = true;
        frame.runs[0].text = "A".into();
        frame.runs[0].cells = 1;
        frame.runs[0].byte_end = 1;
        let mut second = frame.runs[0].clone();
        second.column = 4;
        second.text = "B".into();
        second.byte_start = 1;
        second.byte_end = 2;
        frame.runs.push(second);
        frame.runs.push(super::super::Vim9RenderRun {
            window: 10,
            row: 0,
            column: 1,
            cells: 3,
            text: "tip".into(),
            ..Default::default()
        });
        assert!(frame.valid());
        let view = Vim9Viewport::new(&frame, 10, metrics(), -0.5).unwrap();
        // Hints have no source anchor. The host retains Vim's cell-based
        // pointer path, while hovering real text still resolves exact bytes.
        assert!(view.anchor_at(16.0, 5.0).is_none());
        let source = view.anchor_at(33.0, 5.0).unwrap();
        assert_eq!(
            (
                source.position.line,
                source.position.byte_column,
                source.byte_end
            ),
            (10, 1, 2)
        );
        assert_eq!(source.rect.x, 32.0);
        assert_eq!(view.anchor_for_position(&source.position), Some(source));
        let eol = view.anchor_at(48.0, 5.0).unwrap();
        assert_eq!((eol.position.line, eol.position.byte_column), (10, 2));
        assert_eq!(eol.rect.x, 40.0);
    }

    #[test]
    fn absent_visible_source_metadata_does_not_invent_an_empty_line() {
        let frame = frame();
        let view = Vim9Viewport::new(&frame, 10, metrics(), 0.0).unwrap();
        assert!(view.anchor_at(45.0, 65.0).is_none());
    }
}
