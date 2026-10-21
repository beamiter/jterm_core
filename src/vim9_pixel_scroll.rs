//! Bounded, direct precision scrolling over real semantic margin rows.
//! Precision pixels are integrated once; the OS already supplies momentum.
//! Discrete notches follow a short transition driven by the GUI frame clock.
//! One generation-checked request per split may be in flight. A render carries
//! its request id before the response, so unrelated frames are never treated as
//! scroll acknowledgements. Hosts clear this state before keyboard/edit input.

use super::{
    Vim9Command, Vim9RenderFrame, Vim9RenderMetrics, Vim9RenderRect, Vim9RenderWindow,
    Vim9ScrollAnimation, Vim9Viewport,
};
use std::time::Instant;

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
    clock: Instant,
    wheel_tail: Vim9ScrollAnimation,
    wheel_target: f64,
    wheel_applied: f64,
}

impl Vim9PixelScroll {
    pub fn new(window: &Vim9RenderWindow, metrics: Vim9RenderMetrics) -> Option<Self> {
        if !window.native_pointer
            || window.window <= 0
            || window.buffer <= 0
            || window.topline == 0
            || !metrics.is_valid()
        {
            return None;
        }
        Some(Self {
            window: window.window,
            buffer: window.buffer,
            changedtick: window.changedtick,
            view: window.view.clone(),
            metrics,
            leftcol: window.leftcol,
            textoff: window.textoff,
            topline: window.topline,
            desired: 0.0,
            sent: None,
            clock: Instant::now(),
            wheel_tail: Vim9ScrollAnimation::default(),
            wheel_target: 0.0,
            wheel_applied: 0.0,
        })
    }

    fn matches(&self, window: &Vim9RenderWindow, metrics: Vim9RenderMetrics) -> bool {
        window.native_pointer
            && self.window == window.window
            && self.buffer == window.buffer
            && self.changedtick == window.changedtick
            && self.view == window.view
            && self.metrics == metrics
            && self.leftcol == window.leftcol
            && self.textoff == window.textoff
    }

    pub fn push(&mut self, delta_lines: f64) {
        if !delta_lines.is_finite() {
            return;
        }
        // A precision device already includes the OS's motion profile. Retire
        // an unfinished notch transition at its current position, then follow
        // the supplied pixels directly; never add a second momentum curve.
        self.advance_wheel(self.clock.elapsed().as_secs_f64());
        self.clear_wheel();
        self.integrate(delta_lines);
    }

    fn integrate(&mut self, delta_lines: f64) {
        if !delta_lines.is_finite() {
            return;
        }
        self.desired = (self.desired + delta_lines.clamp(-120.0, 120.0)).clamp(-120.0, 120.0);
        let rounded = self.desired.round();
        if (self.desired - rounded).abs() <= 8.0 * f64::EPSILON * self.desired.abs().max(1.0) {
            self.desired = rounded;
        }
    }

    /// Discrete wheel notches become a short continuous input trajectory.
    /// Reconcile on animation frames even when Vim has not published new paint.
    /// This advances the real viewport incrementally instead of jumping three
    /// rows first and animating a disconnected copy afterwards.
    pub fn push_wheel(&mut self, delta_lines: f64) {
        self.push_wheel_at(delta_lines, self.clock.elapsed().as_secs_f64());
    }

    fn push_wheel_at(&mut self, delta_lines: f64, now: f64) {
        if !delta_lines.is_finite() || delta_lines == 0.0 {
            return;
        }
        self.advance_wheel(now);
        let pending = self.wheel_target - self.wheel_applied;
        if pending != 0.0 && pending.signum() != delta_lines.signum() {
            // Reverse from the visible position rather than paying down the
            // previous notch's unplayed tail. In-flight Vim requests retain
            // their original source identity and acknowledgement ordering.
            self.clear_wheel();
        }
        let target = (self.wheel_target + delta_lines.clamp(-120.0, 120.0)).clamp(-120.0, 120.0);
        self.wheel_tail.retarget(
            (target - self.wheel_target) * f64::from(self.metrics.line_height),
            now,
        );
        self.wheel_target = target;
    }

    fn advance_wheel(&mut self, now: f64) {
        let position =
            self.wheel_target - self.wheel_tail.offset(now) / f64::from(self.metrics.line_height);
        self.integrate(position - self.wheel_applied);
        self.wheel_applied = position;
        if !self.wheel_tail.is_active(now) {
            self.clear_wheel();
        }
    }

    fn clear_wheel(&mut self) {
        self.wheel_tail.clear();
        self.wheel_target = 0.0;
        self.wheel_applied = 0.0;
    }

    /// Keep the GUI frame clock running until the final fractional step has
    /// actually been integrated, including when the timer passed its deadline.
    pub fn is_animating(&self) -> bool {
        self.wheel_target != self.wheel_applied
            || self
                .wheel_tail
                .is_active(self.clock.elapsed().as_secs_f64())
    }

    /// A source-bound press freezes the image that supplied its byte target.
    /// Keep its existing fractional transform, but retire future wheel motion.
    pub fn stop_animation(&mut self) {
        self.clear_wheel();
    }

    pub fn request_id(&self) -> Option<u64> {
        self.sent.map(|(id, _)| id)
    }

    /// False means retire the gesture, not retry an obsolete request. Failed
    /// commands cannot leave a phantom fractional viewport behind. Geometry,
    /// source edits and unrequested viewport motion invalidate the binding.
    pub fn reconcile(
        &mut self,
        window: &Vim9RenderWindow,
        metrics: Vim9RenderMetrics,
        request_pending: bool,
    ) -> bool {
        self.reconcile_at(
            window,
            metrics,
            request_pending,
            self.clock.elapsed().as_secs_f64(),
        )
    }

    fn reconcile_at(
        &mut self,
        window: &Vim9RenderWindow,
        metrics: Vim9RenderMetrics,
        request_pending: bool,
        now: f64,
    ) -> bool {
        if !self.matches(window, metrics) {
            return false;
        }
        self.advance_wheel(now);
        if let Some((id, lines)) = self.sent {
            if window.scroll_ack == id {
                let moved = window.topline as i128 - self.topline as i128;
                if moved == i128::from(lines) {
                    self.desired -= f64::from(lines);
                } else {
                    // File edges and Vim constraints are authoritative. Never
                    // accumulate a debt against a viewport that cannot move.
                    self.desired = 0.0;
                    self.clear_wheel();
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
        if self.sent.is_some() || lines == 0 {
            return None;
        }
        Some(Vim9Command::ScrollView {
            window: self.window,
            buffer: self.buffer,
            changedtick: self.changedtick,
            topline: self.topline,
            leftcol: self.leftcol,
            lines,
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
        let Some(window) = frame.windows.iter().find(|w| w.window == self.window) else {
            return 0.0;
        };
        if !self.matches(window, metrics) || window.topline != self.topline {
            return 0.0;
        }
        let mut coverage = 0;
        for distance in 1..=8 {
            let row = if self.desired < 0.0 {
                -distance
            } else {
                window.view.rows as i32 + distance - 1
            };
            if window.overscan.iter().any(|edge| edge.row == row) {
                coverage += 1;
            } else {
                break;
            }
        }
        let offset = -(self
            .desired
            .clamp(-f64::from(coverage), f64::from(coverage)) as f32)
            * metrics.line_height;
        if Vim9Viewport::new(frame, self.window, metrics, offset).is_some() {
            offset
        } else {
            0.0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vim9::Vim9RenderMarginRow;

    fn metrics() -> Vim9RenderMetrics {
        Vim9RenderMetrics {
            origin_x: 0.0,
            origin_y: 0.0,
            cell_width: 8.0,
            line_height: 20.0,
        }
    }
    fn frame(top: u64, ack: u64) -> Vim9RenderFrame {
        let window = Vim9RenderWindow {
            window: 10,
            buffer: 1,
            changedtick: 3,
            topline: top,
            botline: top + 3,
            view: Vim9RenderRect {
                row: 0,
                column: 0,
                rows: 4,
                columns: 10,
            },
            line_count: 100,
            native_pointer: true,
            scroll_ack: ack,
            line_lengths: vec![0; 4],
            overscan: [-2, -1, 4, 5]
                .into_iter()
                .map(|row| Vim9RenderMarginRow {
                    row,
                    line: (top as i64 + i64::from(row)) as u64,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        Vim9RenderFrame {
            supported: true,
            windows: vec![window],
            ..Default::default()
        }
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
        assert!(matches!(
            scroll.command(),
            Some(Vim9Command::ScrollView {
                lines: 1,
                topline: 10,
                ..
            })
        ));
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
                0 => w.buffer += 1,
                1 => w.changedtick += 1,
                2 => w.view.column += 1,
                3 => w.leftcol += 1,
                4 => w.textoff += 1,
                5 => w.topline += 1,
                _ => w.native_pointer = false,
            }
            assert!(!original.clone().reconcile(w, metrics(), false));
        }
        let scaled = Vim9RenderMetrics {
            line_height: 21.0,
            ..metrics()
        };
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
        assert!(matches!(
            scroll.command(),
            Some(Vim9Command::ScrollView { lines: 120, .. })
        ));
    }
    #[test]
    fn reversal_integrates_signed_pixels_without_discarding_the_fraction() {
        let frame = frame(10, 0);
        let mut scroll = Vim9PixelScroll::new(&frame.windows[0], metrics()).unwrap();
        scroll.push(0.5);
        scroll.push(-0.25);
        assert_eq!(scroll.offset(&frame, metrics()), -5.0);
        scroll.push(-0.5);
        assert_eq!(scroll.offset(&frame, metrics()), 5.0);
    }
    #[test]
    fn decimal_noise_and_invalid_input_do_not_lose_whole_lines() {
        let frame = frame(10, 0);
        let mut scroll = Vim9PixelScroll::new(&frame.windows[0], metrics()).unwrap();
        for _ in 0..10 {
            scroll.push(0.1);
        }
        scroll.push(f64::NAN);
        scroll.push(f64::INFINITY);
        assert!(matches!(
            scroll.command(),
            Some(Vim9Command::ScrollView { lines: 1, .. })
        ));
    }
    #[test]
    fn a_clamped_vim_viewport_does_not_leave_scroll_debt() {
        let old = frame(10, 0);
        let mut scroll = Vim9PixelScroll::new(&old.windows[0], metrics()).unwrap();
        scroll.push(1.5);
        scroll.submitted(4);
        let new = frame(10, 4);
        assert!(scroll.reconcile(&new.windows[0], metrics(), false));
        assert_eq!(scroll.offset(&new, metrics()), 0.0);
        assert!(scroll.command().is_none());
    }

    fn wheel_frame(top: u64, ack: u64) -> Vim9RenderFrame {
        let mut frame = frame(top, ack);
        frame.windows[0].overscan = (-8..0)
            .chain(4..12)
            .map(|row| Vim9RenderMarginRow {
                row,
                line: (top as i64 + i64::from(row)) as u64,
                ..Default::default()
            })
            .collect();
        frame
    }

    #[test]
    fn wheel_notches_follow_time_at_60_120_and_240_hz_and_keep_exact_distance() {
        for hz in [60.0, 120.0, 240.0] {
            let frame = wheel_frame(20, 0);
            let mut scroll = Vim9PixelScroll::new(&frame.windows[0], metrics()).unwrap();
            scroll.push_wheel_at(3.0, 0.0);
            assert!(
                scroll.command().is_none(),
                "a notch must not jump three rows immediately"
            );
            let mut previous = 0.0;
            for tick in 1..=(hz * 0.15) as usize {
                let now = tick as f64 / hz;
                assert!(scroll.reconcile_at(&frame.windows[0], metrics(), false, now));
                let moved = -f64::from(scroll.offset(&frame, metrics())) / 20.0;
                assert!(moved >= previous && moved <= 3.0 + 1e-6);
                assert!(moved - previous < 1.3, "{hz} Hz: {previous} -> {moved}");
                previous = moved;
            }
            assert_eq!(previous, 3.0);
            assert!(!scroll.is_animating());
            assert!(matches!(
                scroll.command(),
                Some(Vim9Command::ScrollView { lines: 3, .. })
            ));
        }
    }

    #[test]
    fn wheel_ack_preserves_the_same_source_pixel_position_during_animation() {
        let old = wheel_frame(20, 0);
        let mut scroll = Vim9PixelScroll::new(&old.windows[0], metrics()).unwrap();
        scroll.push_wheel_at(3.0, 0.0);
        assert!(scroll.reconcile_at(&old.windows[0], metrics(), false, 0.04));
        scroll.submitted(8);
        let before = 20.0 - scroll.offset(&old, metrics()) / 20.0;
        let new = wheel_frame(22, 8);
        assert!(scroll.reconcile_at(&new.windows[0], metrics(), false, 0.04));
        let after = 22.0 - scroll.offset(&new, metrics()) / 20.0;
        assert!((after - before).abs() < 0.00001, "{before} -> {after}");
        assert!(scroll.reconcile_at(&new.windows[0], metrics(), false, 0.15));
        assert_eq!(scroll.offset(&new, metrics()), -20.0);
    }

    #[test]
    fn reversing_a_wheel_cancels_unplayed_motion_and_source_press_freezes_it() {
        let frame = wheel_frame(20, 0);
        let mut scroll = Vim9PixelScroll::new(&frame.windows[0], metrics()).unwrap();
        scroll.push_wheel_at(3.0, 0.0);
        scroll.reconcile_at(&frame.windows[0], metrics(), false, 0.03);
        let before = scroll.offset(&frame, metrics());
        scroll.push_wheel_at(-3.0, 0.03);
        assert_eq!(scroll.offset(&frame, metrics()), before);
        scroll.reconcile_at(&frame.windows[0], metrics(), false, 0.04);
        assert!(
            scroll.offset(&frame, metrics()) > before,
            "reverse on the very next frame"
        );
        let frozen = scroll.offset(&frame, metrics());
        scroll.stop_animation();
        scroll.reconcile_at(&frame.windows[0], metrics(), false, 0.3);
        assert_eq!(scroll.offset(&frame, metrics()), frozen);
        assert!(!scroll.is_animating());
    }

    #[test]
    fn reaching_a_file_edge_retires_remaining_wheel_animation() {
        let old = wheel_frame(20, 0);
        let mut scroll = Vim9PixelScroll::new(&old.windows[0], metrics()).unwrap();
        scroll.push_wheel_at(3.0, 0.0);
        scroll.reconcile_at(&old.windows[0], metrics(), false, 0.02);
        scroll.submitted(8);
        let edge = wheel_frame(20, 8);
        assert!(scroll.reconcile_at(&edge.windows[0], metrics(), false, 0.03));
        assert!(!scroll.is_animating());
        scroll.reconcile_at(&edge.windows[0], metrics(), false, 1.0);
        assert_eq!(scroll.offset(&edge, metrics()), 0.0);
        assert!(scroll.command().is_none());
    }
}
