//! Bounded, direct precision scrolling over real semantic margin rows.
//! Input is integrated once; the operating system already supplies momentum.
//! One generation-checked request per split may be in flight. A render carries
//! its request id before the response, so unrelated frames are never treated as
//! scroll acknowledgements. Hosts clear this state before keyboard/edit input.

use super::{Vim9Command, Vim9RenderFrame, Vim9RenderMetrics, Vim9RenderRect, Vim9RenderWindow, Vim9Viewport};

#[derive(Clone, Debug)]
pub struct Vim9PixelScroll {
    window: i64,
    buffer: i64,
    changedtick: u64,
    view: Vim9RenderRect,
    metrics: Vim9RenderMetrics,
    leftcol: usize,
    textoff: usize,
    topline: u64,
    /// Desired motion relative to the last acknowledged viewport, including
    /// the request in flight. The fraction persists between render frames.
    desired: f64,
    sent: Option<(u64, i32)>,
}

impl Vim9PixelScroll {
    pub fn new(window: &Vim9RenderWindow, metrics: Vim9RenderMetrics) -> Option<Self> {
        if !window.native_pointer || window.window <= 0 || window.buffer <= 0
            || window.topline == 0 || !metrics.is_valid() {
            return None;
        }
        Some(Self {
            window: window.window, buffer: window.buffer, changedtick: window.changedtick,
            view: window.view.clone(), metrics, leftcol: window.leftcol,
            textoff: window.textoff, topline: window.topline, desired: 0.0, sent: None,
        })
    }

    fn matches(&self, window: &Vim9RenderWindow, metrics: Vim9RenderMetrics) -> bool {
        window.native_pointer && self.window == window.window && self.buffer == window.buffer
            && self.changedtick == window.changedtick && self.view == window.view
            && self.metrics == metrics && self.leftcol == window.leftcol
            && self.textoff == window.textoff
    }

    pub fn push(&mut self, delta_lines: f64) {
        if !delta_lines.is_finite() { return; }
        self.desired = (self.desired + delta_lines.clamp(-120.0, 120.0)).clamp(-120.0, 120.0);
        let rounded = self.desired.round();
        if (self.desired - rounded).abs() <= 8.0 * f64::EPSILON * self.desired.abs().max(1.0) {
            self.desired = rounded;
        }
    }

    pub fn request_id(&self) -> Option<u64> { self.sent.map(|(id, _)| id) }

    /// False means retire the gesture, not retry an obsolete request. Failed
    /// commands cannot leave a phantom fractional viewport behind. Geometry,
    /// source edits and unrequested viewport motion invalidate the binding.
    pub fn reconcile(&mut self, window: &Vim9RenderWindow, metrics: Vim9RenderMetrics,
        request_pending: bool) -> bool {
        if !self.matches(window, metrics) { return false; }
        if let Some((id, lines)) = self.sent {
            if window.scroll_ack == id {
                let moved = window.topline as i128 - self.topline as i128;
                if moved == i128::from(lines) {
                    self.desired -= f64::from(lines);
                } else {
                    // File edges and Vim constraints are authoritative. Never
                    // accumulate a debt against a viewport that cannot move.
                    self.desired = 0.0;
                }
                self.sent = None;
                self.topline = window.topline;
            } else if !request_pending || self.topline != window.topline {
                return false;
            }
        } else if self.topline != window.topline {
            return false;
        }
        true
    }

    pub fn command(&self) -> Option<Vim9Command> {
        let lines = self.desired.trunc() as i32;
        if self.sent.is_some() || lines == 0 { return None; }
        Some(Vim9Command::ScrollView {
            window: self.window, buffer: self.buffer, changedtick: self.changedtick,
            topline: self.topline, leftcol: self.leftcol, lines,
        })
    }

    /// Call only after the immediately preceding command() was accepted by
    /// Vim9Session::send(). Rejection leaves the local input unsent and bounded.
    pub fn submitted(&mut self, id: u64) {
        let lines = self.desired.trunc() as i32;
        if self.sent.is_none() && id > 0 && lines != 0 {
            self.sent = Some((id, lines));
        }
    }

    pub fn offset(&self, frame: &Vim9RenderFrame, metrics: Vim9RenderMetrics) -> f32 {
        let Some(window) = frame.windows.iter().find(|w| w.window == self.window) else { return 0.0; };
        if !self.matches(window, metrics) || window.topline != self.topline { return 0.0; }
        let mut coverage = 0;
        for distance in 1..=2 {
            let row = if self.desired < 0.0 { -distance } else { window.view.rows as i32 + distance - 1 };
            if window.overscan.iter().any(|edge| edge.row == row) { coverage += 1; } else { break; }
        }
        let offset = -(self.desired.clamp(-f64::from(coverage), f64::from(coverage)) as f32) * metrics.line_height;
        if Vim9Viewport::new(frame, self.window, metrics, offset).is_some() { offset } else { 0.0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vim9::Vim9RenderMarginRow;

    fn metrics() -> Vim9RenderMetrics {
        Vim9RenderMetrics { origin_x: 0.0, origin_y: 0.0, cell_width: 8.0, line_height: 20.0 }
    }
    fn frame(top: u64, ack: u64) -> Vim9RenderFrame {
        let window = Vim9RenderWindow {
            window: 10, buffer: 1, changedtick: 3, topline: top, botline: top + 3,
            view: Vim9RenderRect { row: 0, column: 0, rows: 4, columns: 10 },
            line_count: 100, native_pointer: true, scroll_ack: ack, line_lengths: vec![0; 4],
            overscan: [-2, -1, 4, 5].into_iter().map(|row| Vim9RenderMarginRow {
                row, line: (top as i64 + i64::from(row)) as u64, ..Default::default()
            }).collect(), ..Default::default()
        };
        Vim9RenderFrame { supported: true, windows: vec![window], ..Default::default() }
    }
    #[test]
    fn subline_input_moves_immediately_and_persists_without_a_command() {
        let frame = frame(10, 0);
        let mut scroll = Vim9PixelScroll::new(&frame.windows[0], metrics()).unwrap();
        scroll.push(0.3125);
        assert_eq!(scroll.offset(&frame, metrics()), -6.25);
        assert!(scroll.command().is_none());
        assert!(scroll.reconcile(&frame.windows[0], metrics(), false));
        assert_eq!(scroll.offset(&frame, metrics()), -6.25);
    }
    #[test]
    fn acknowledgement_rebases_without_double_movement_and_keeps_new_input() {
        let old = frame(10, 0);
        let mut scroll = Vim9PixelScroll::new(&old.windows[0], metrics()).unwrap();
        scroll.push(1.25);
        assert!(matches!(scroll.command(), Some(Vim9Command::ScrollView { lines: 1, topline: 10, .. })));
        scroll.submitted(41);
        scroll.push(0.25);
        assert!(scroll.command().is_none());
        let before = scroll.offset(&old, metrics());
        let new = frame(11, 41);
        assert!(scroll.reconcile(&new.windows[0], metrics(), false));
        assert_eq!(scroll.offset(&new, metrics()), before + 20.0);
        assert_eq!(scroll.request_id(), None);
    }
    #[test]
    fn failed_or_unrelated_acknowledgements_retire_the_gesture() {
        let old = frame(10, 0);
        let mut scroll = Vim9PixelScroll::new(&old.windows[0], metrics()).unwrap();
        scroll.push(1.0);
        scroll.submitted(41);
        assert!(scroll.reconcile(&old.windows[0], metrics(), true));
        assert!(!scroll.reconcile(&old.windows[0], metrics(), false));
        assert!(!scroll.reconcile(&frame(11, 40).windows[0], metrics(), true));
    }
    #[test]
    fn source_geometry_and_external_navigation_cancel_without_replay() {
        let old = frame(10, 0);
        let original = Vim9PixelScroll::new(&old.windows[0], metrics()).unwrap();
        for case in 0..7 {
            let mut new = old.clone();
            let w = &mut new.windows[0];
            match case {
                0 => w.buffer += 1, 1 => w.changedtick += 1, 2 => w.view.column += 1,
                3 => w.leftcol += 1, 4 => w.textoff += 1, 5 => w.topline += 1,
                _ => w.native_pointer = false,
            }
            assert!(!original.clone().reconcile(w, metrics(), false));
        }
        let scaled = Vim9RenderMetrics { line_height: 21.0, ..metrics() };
        assert!(!original.clone().reconcile(&old.windows[0], scaled, false));
    }
    #[test]
    fn presentation_never_exposes_unsampled_rows() {
        let mut frame = frame(10, 0);
        let mut scroll = Vim9PixelScroll::new(&frame.windows[0], metrics()).unwrap();
        scroll.push(1000.0);
        assert_eq!(scroll.offset(&frame, metrics()), -40.0);
        frame.windows[0].overscan.retain(|r| r.row != 5);
        assert_eq!(scroll.offset(&frame, metrics()), -20.0);
        frame.windows[0].overscan.clear();
        assert_eq!(scroll.offset(&frame, metrics()), 0.0);
        assert!(matches!(scroll.command(), Some(Vim9Command::ScrollView { lines: 120, .. })));
    }
    #[test]
    fn reversal_integrates_signed_pixels_without_discarding_the_fraction() {
        let frame = frame(10, 0);
        let mut scroll = Vim9PixelScroll::new(&frame.windows[0], metrics()).unwrap();
        scroll.push(0.5); scroll.push(-0.25);
        assert_eq!(scroll.offset(&frame, metrics()), -5.0);
        scroll.push(-0.5);
        assert_eq!(scroll.offset(&frame, metrics()), 5.0);
    }
    #[test]
    fn decimal_noise_and_invalid_input_do_not_lose_whole_lines() {
        let frame = frame(10, 0);
        let mut scroll = Vim9PixelScroll::new(&frame.windows[0], metrics()).unwrap();
        for _ in 0..10 { scroll.push(0.1); }
        scroll.push(f64::NAN); scroll.push(f64::INFINITY);
        assert!(matches!(scroll.command(), Some(Vim9Command::ScrollView { lines: 1, .. })));
    }
    #[test]
    fn_a_clamped_vim_viewport_does_not_leave_scroll_debt() {
        let old = frame(10, 0);
        let mut scroll = Vim9PixelScroll::new(&old.windows[0], metrics()).unwrap();
        scroll.push(1.5); scroll.submitted(4);
        let new = frame(10, 4);
        assert!(scroll.reconcile(&new.windows[0], metrics(), false));
        assert_eq!(scroll.offset(&new, metrics()), 0.0);
        assert!(scroll.command().is_none());
    }
}
