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
    /// Motion actually exposed using available source rows. Input can run
    /// ahead of these rows, but an acknowledgement must not reveal that debt
    /// as an instantaneous jump in the next viewport.
    presented: f64,
    before: f64,
    after: f64,
    last_step: f64,
    sent: Option<(u64, i32, bool)>,
    clock: Instant,
    wheel_tail: Vim9ScrollAnimation,
    wheel_target: f64,
    wheel_applied: f64,
}

impl Vim9PixelScroll {
    pub fn new(window: &Vim9RenderWindow, metrics: Vim9RenderMetrics) -> Option<Self> {
        if !window.native_scroll
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
            presented: 0.0,
            before: Self::coverage(window, false),
            after: Self::coverage(window, true),
            last_step: 0.0,
            sent: None,
            clock: Instant::now(),
            wheel_tail: Vim9ScrollAnimation::default(),
            wheel_target: 0.0,
            wheel_applied: 0.0,
        })
    }

    fn matches(&self, window: &Vim9RenderWindow, metrics: Vim9RenderMetrics) -> bool {
        // A source-bound drag freezes its displayed fraction before entering
        // characterwise Visual mode. That image remains valid for pointer
        // hit testing even though Visual mode does not admit new scrolls.
        (window.native_scroll || window.native_pointer)
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
        self.advance(self.clock.elapsed().as_secs_f64());
        self.clear_wheel();
        self.integrate(delta_lines);
    }

    fn integrate(&mut self, delta_lines: f64) {
        if !delta_lines.is_finite() || delta_lines == 0.0 {
            return;
        }
        let debt = self.desired - self.presented;
        if debt.abs() > self.settle_distance() && debt * delta_lines < 0.0 {
            // A reversal should move the visible image immediately, not first
            // pay off motion that never reached the screen during a slow ack.
            self.desired = self.presented;
        }
        let previous = self.desired;
        self.desired = (self.desired + delta_lines.clamp(-120.0, 120.0)).clamp(-120.0, 120.0);
        let rounded = self.desired.round();
        if (self.desired - rounded).abs() <= 8.0 * f64::EPSILON * self.desired.abs().max(1.0) {
            self.desired = rounded;
        }
        self.presented = self.clamp_presentation(self.desired - (previous - self.presented));
    }

    fn coverage(window: &Vim9RenderWindow, down: bool) -> f64 {
        (1..=8)
            .take_while(|distance| {
                let row = if down {
                    window.view.rows as i32 + distance - 1
                } else {
                    -distance
                };
                window.overscan.iter().any(|edge| edge.row == row)
            })
            .count() as f64
    }

    fn settle_distance(&self) -> f64 {
        0.05 / f64::from(self.metrics.line_height)
    }

    fn movable_debt(&self) -> f64 {
        self.clamp_presentation(self.desired) - self.presented
    }

    fn presentation_bounds(&self) -> Option<(f64, f64)> {
        let mut low = -self.before;
        let mut high = self.after;
        if let Some((_, lines, true)) = self.sent {
            // A large reversal while a commit is in flight must also fit the
            // replacement viewport's margin. Protocol 3 only supplies two
            // rows; use observed capacity rather than assuming protocol 4's
            // eight rows. Hold only the unseen remainder until the ack,
            // rather than showing pixels that would
            // disappear on receipt of the next source frame.
            let capacity = self.before.max(self.after);
            low = low.max(f64::from(lines) - capacity);
            high = high.min(f64::from(lines) + capacity);
        }
        (low <= high).then_some((low, high))
    }

    fn clamp_presentation(&self, value: f64) -> f64 {
        self.presentation_bounds()
            .map_or(0.0, |(low, high)| value.clamp(low, high))
    }

    fn advance(&mut self, now: f64) {
        // Only input stalled by missing source rows gets this catch-up. Raw
        // touchpad input inside the sampled area is still applied exactly once
        // and directly, including momentum already supplied by the OS.
        let elapsed = (now - self.last_step).max(0.0);
        self.last_step = now;
        let debt = self.movable_debt();
        if elapsed > 0.0 && debt != 0.0 {
            // Integrate the rate cap and exponential exactly. Clamping an
            // Euler step would give 60/120/240 Hz different trajectories.
            const RATE: f64 = 120.0;
            const TAU: f64 = 0.040;
            let linear_time = ((debt.abs() - RATE * TAU) / RATE).max(0.0);
            let remaining = if elapsed <= linear_time {
                debt.abs() - RATE * elapsed
            } else {
                debt.abs().min(RATE * TAU) * (-(elapsed - linear_time) / TAU).exp()
            };
            self.presented += debt.signum() * (debt.abs() - remaining);
            if self.movable_debt().abs() < self.settle_distance() {
                self.presented = self.clamp_presentation(self.desired);
            }
        }
        self.advance_wheel(now);
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
        self.advance(now);
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
        self.movable_debt() != 0.0
            || self.wheel_target != self.wheel_applied
            || self
                .wheel_tail
                .is_active(self.clock.elapsed().as_secs_f64())
    }

    /// A source-bound press freezes the image that supplied its byte target.
    /// Keep its existing fractional transform, but retire future wheel motion.
    pub fn stop_animation(&mut self) {
        self.clear_wheel();
        self.desired = self.presented;
    }

    pub fn request_id(&self) -> Option<u64> {
        self.sent.map(|(id, _, _)| id)
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
        self.advance(now);
        if let Some((id, lines, interpolated)) = self.sent {
            if window.scroll_ack == id {
                let moved = window.topline as i128 - self.topline as i128;
                if moved == i128::from(lines) {
                    self.desired -= f64::from(lines);
                    if interpolated {
                        self.presented -= f64::from(lines);
                    } else {
                        // There were no source rows to display this move.
                        // Do not invent a reverse animation on its receipt.
                        self.presented = 0.0;
                    }
                } else {
                    // File edges and Vim constraints are authoritative. Never
                    // accumulate a debt against a viewport that cannot move,
                    // but retain a reverse gesture queued after this request.
                    let queued = self.desired - f64::from(lines);
                    self.desired = if queued * f64::from(lines) < 0.0 {
                        queued
                    } else {
                        0.0
                    };
                    self.presented = self.desired;
                    if (self.wheel_target - self.wheel_applied) * f64::from(lines) >= 0.0 {
                        self.clear_wheel();
                    }
                }
                self.sent = None;
                self.topline = window.topline;
            } else if !request_pending || self.topline != window.topline {
                return false;
            }
        } else if self.topline != window.topline {
            return false;
        }
        self.before = Self::coverage(window, false);
        self.after = Self::coverage(window, true);
        if self.presentation_bounds().is_none() {
            // A plugin can retire margins without editing the buffer while a
            // request is pending. No common source image remains to translate;
            // retire the gesture rather than clamp with an inverted interval.
            return false;
        }
        self.presented = self.clamp_presentation(self.presented);
        true
    }

    fn command_lines(&self) -> i32 {
        let presented = self.presented.trunc() as i32;
        if presented != 0 {
            return presented;
        }
        // Missing/unsupported margin rows cannot be invented. Keep the
        // existing whole-line route when pixel interpolation is unavailable,
        // including a request that lets Vim retire input at a document edge.
        if (self.desired > 0.0 && self.after == 0.0) || (self.desired < 0.0 && self.before == 0.0) {
            self.desired.trunc() as i32
        } else {
            0
        }
    }

    pub fn command(&self) -> Option<Vim9Command> {
        let lines = self.command_lines();
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
        let lines = self.command_lines();
        if self.sent.is_none() && id > 0 && lines != 0 {
            self.sent = Some((id, lines, self.presented.trunc() as i32 != 0));
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
            let row = if self.presented < 0.0 {
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
            .presented
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
            native_scroll: true,
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
    fn scroll_capability_is_independent_of_source_pointer_admission() {
        let mut frame = frame(10, 0);
        assert!(!frame.windows[0].native_pointer);
        let mut scroll = Vim9PixelScroll::new(&frame.windows[0], metrics()).unwrap();
        scroll.push(0.0125);
        assert_eq!(scroll.offset(&frame, metrics()), -0.25);
        // A click mapping changing does not retire a valid scroll gesture.
        frame.windows[0].native_pointer = true;
        assert!(scroll.reconcile(&frame.windows[0], metrics(), false));
        assert_eq!(scroll.offset(&frame, metrics()), -0.25);
        frame.windows[0].native_scroll = false;
        frame.windows[0].native_pointer = false;
        assert!(!scroll.reconcile(&frame.windows[0], metrics(), false));
        assert!(Vim9PixelScroll::new(&frame.windows[0], metrics()).is_none());
    }
    #[test]
    fn frozen_pointer_fraction_survives_characterwise_visual_mode() {
        let mut frame = frame(10, 0);
        let mut scroll = Vim9PixelScroll::new(&frame.windows[0], metrics()).unwrap();
        scroll.push(0.3125);
        scroll.stop_animation();
        frame.mode = "v".into();
        frame.windows[0].native_scroll = false;
        frame.windows[0].native_pointer = true;
        assert!(scroll.reconcile(&frame.windows[0], metrics(), false));
        assert_eq!(scroll.offset(&frame, metrics()), -6.25);
        assert!(Vim9PixelScroll::new(&frame.windows[0], metrics()).is_none());
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
                _ => w.native_scroll = false,
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
        assert!(matches!(
            scroll.command(),
            Some(Vim9Command::ScrollView { lines: 2, .. })
        ));
        frame.windows[0].overscan.retain(|r| r.row != 5);
        assert_eq!(scroll.offset(&frame, metrics()), -20.0);
        frame.windows[0].overscan.clear();
        assert_eq!(scroll.offset(&frame, metrics()), 0.0);
        assert!(scroll.reconcile(&frame.windows[0], metrics(), false));
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

    fn visual_origin(scroll: &Vim9PixelScroll, frame: &Vim9RenderFrame) -> f64 {
        frame.windows[0].topline as f64
            - f64::from(scroll.offset(frame, metrics())) / f64::from(metrics().line_height)
    }

    #[test]
    fn saturated_precision_input_keeps_the_displayed_origin_across_each_ack() {
        for direction in [-1.0, 1.0] {
            let mut frame = wheel_frame(40, 0);
            let mut scroll = Vim9PixelScroll::new(&frame.windows[0], metrics()).unwrap();
            scroll.integrate(20.25 * direction);
            assert_eq!(visual_origin(&scroll, &frame), 40.0 + 8.0 * direction);
            let mut request = 1;
            for tick in 0..200 {
                let now = tick as f64 / 120.0;
                assert!(scroll.reconcile_at(&frame.windows[0], metrics(), false, now));
                if let Some(Vim9Command::ScrollView { lines, .. }) = scroll.command() {
                    assert!(lines.abs() <= 8, "never commit invisible backlog");
                    scroll.submitted(request);
                    let before = visual_origin(&scroll, &frame);
                    frame = wheel_frame(
                        (frame.windows[0].topline as i64 + i64::from(lines)) as u64,
                        request,
                    );
                    assert!(scroll.reconcile_at(&frame.windows[0], metrics(), false, now));
                    assert!(
                        (visual_origin(&scroll, &frame) - before).abs() < 1e-6,
                        "ack must not snap to a row or expose blocked motion"
                    );
                    request += 1;
                }
            }
            assert!((visual_origin(&scroll, &frame) - (40.0 + 20.25 * direction)).abs() < 1e-6);
            assert!(!scroll.is_animating());
            assert!(scroll.command().is_none());
        }
    }

    #[test]
    fn backlog_catchup_is_pixel_continuous_and_refresh_rate_independent() {
        let mut samples = Vec::new();
        for hz in [60.0, 120.0, 144.0, 240.0] {
            let old = wheel_frame(20, 0);
            let mut scroll = Vim9PixelScroll::new(&old.windows[0], metrics()).unwrap();
            scroll.integrate(20.25);
            scroll.submitted(1);
            let next = wheel_frame(28, 1);
            assert!(scroll.reconcile_at(&next.windows[0], metrics(), false, 0.0));
            assert_eq!(scroll.offset(&next, metrics()), 0.0);
            assert!(scroll.is_animating());
            let mut previous = visual_origin(&scroll, &next);
            let mut fractional = false;
            // Same target and no further commits isolate the integration from
            // bridge scheduling; 1/6 second is shared by all four rates.
            for tick in 1..=(hz / 6.0) as usize {
                scroll.reconcile_at(&next.windows[0], metrics(), false, tick as f64 / hz);
                let current = visual_origin(&scroll, &next);
                assert!(current > previous);
                assert!(current - previous <= 120.0 / hz + 1e-5);
                fractional |= (current - current.round()).abs() > 0.01;
                previous = current;
            }
            assert!(fractional, "the renderer must move between logical rows");
            samples.push(previous);
        }
        assert!(samples
            .iter()
            .all(|sample| (sample - samples[0]).abs() < 1e-5));
    }

    #[test]
    fn ordinary_quarter_pixel_events_have_no_added_tail_or_row_snap() {
        let mut frame = wheel_frame(20, 0);
        let mut scroll = Vim9PixelScroll::new(&frame.windows[0], metrics()).unwrap();
        for tick in 1..=400 {
            let now = tick as f64 / 240.0;
            scroll.reconcile_at(&frame.windows[0], metrics(), false, now);
            scroll.integrate(0.25 / 20.0);
            let origin = 20.0 + tick as f64 * 0.25 / 20.0;
            assert!((visual_origin(&scroll, &frame) - origin).abs() < 1e-6);
            assert!(!scroll.is_animating(), "ordinary OS pixels are direct");
            if let Some(Vim9Command::ScrollView { lines, .. }) = scroll.command() {
                scroll.submitted(tick);
                frame = wheel_frame(
                    (frame.windows[0].topline as i64 + i64::from(lines)) as u64,
                    tick,
                );
                scroll.reconcile_at(&frame.windows[0], metrics(), false, now);
                assert!((visual_origin(&scroll, &frame) - origin).abs() < 1e-6);
            }
        }
    }

    #[test]
    fn reversal_and_pointer_capture_retire_only_unseen_backlog() {
        let frame = wheel_frame(20, 0);
        let mut scroll = Vim9PixelScroll::new(&frame.windows[0], metrics()).unwrap();
        scroll.integrate(20.25);
        scroll.integrate(-0.25);
        assert_eq!(visual_origin(&scroll, &frame), 27.75);
        assert_eq!(scroll.desired, 7.75);
        assert!(!scroll.is_animating());
        scroll.integrate(20.0);
        let before = visual_origin(&scroll, &frame);
        scroll.stop_animation();
        scroll.reconcile_at(&frame.windows[0], metrics(), false, 1.0);
        assert_eq!(visual_origin(&scroll, &frame), before);
        assert_eq!(scroll.desired, scroll.presented);
    }

    #[test]
    fn in_flight_reversal_stays_inside_both_source_viewports() {
        let old = wheel_frame(40, 0);
        let mut scroll = Vim9PixelScroll::new(&old.windows[0], metrics()).unwrap();
        scroll.integrate(8.0);
        scroll.submitted(1);
        scroll.integrate(-10.0);
        let origin = visual_origin(&scroll, &old);
        assert_eq!(origin, 40.0, "hold the part outside the replacement margin");
        let next = wheel_frame(48, 1);
        scroll.reconcile_at(&next.windows[0], metrics(), false, 0.0);
        assert_eq!(visual_origin(&scroll, &next), origin);
        assert!(matches!(
            scroll.command(),
            Some(Vim9Command::ScrollView { lines: -8, .. })
        ));
        scroll.submitted(2);
        let returned = wheel_frame(40, 2);
        scroll.reconcile_at(&returned.windows[0], metrics(), false, 0.0);
        assert_eq!(visual_origin(&scroll, &returned), origin);
        assert!(scroll.is_animating());
    }

    #[test]
    fn legacy_two_row_margins_preserve_reversal_continuity() {
        for direction in [-1.0, 1.0] {
            let old = frame(30, 0);
            let mut scroll = Vim9PixelScroll::new(&old.windows[0], metrics()).unwrap();
            scroll.integrate(2.0 * direction);
            scroll.submitted(1);
            scroll.integrate(-3.0 * direction);
            let origin = visual_origin(&scroll, &old);
            let next = frame((30.0 + 2.0 * direction) as u64, 1);
            scroll.reconcile_at(&next.windows[0], metrics(), false, 0.0);
            assert_eq!(visual_origin(&scroll, &next), origin);
        }
    }

    #[test]
    fn retired_margins_during_a_pending_commit_retire_the_gesture() {
        let old = wheel_frame(40, 0);
        for retained in [0, 1, 2] {
            let mut scroll = Vim9PixelScroll::new(&old.windows[0], metrics()).unwrap();
            scroll.integrate(8.0);
            scroll.submitted(1);
            let mut next = old.clone();
            next.windows[0]
                .overscan
                .retain(|edge| edge.row >= -retained && edge.row < 4 + retained);
            assert!(!scroll.reconcile_at(&next.windows[0], metrics(), true, 0.01));
        }
    }

    #[test]
    fn a_blocked_old_request_does_not_eat_a_new_reverse_pixel_gesture() {
        let old = wheel_frame(40, 0);
        let mut scroll = Vim9PixelScroll::new(&old.windows[0], metrics()).unwrap();
        scroll.integrate(2.0);
        scroll.submitted(1);
        scroll.integrate(-0.25);
        let edge = wheel_frame(40, 1);
        scroll.reconcile_at(&edge.windows[0], metrics(), false, 0.0);
        assert_eq!(scroll.desired, -0.25);
        assert_eq!(scroll.offset(&edge, metrics()), 5.0);
        assert!(!scroll.is_animating());
    }

    #[test]
    fn no_margin_compatibility_does_not_invent_a_reverse_animation() {
        let mut old = wheel_frame(20, 0);
        old.windows[0].overscan.clear();
        let mut scroll = Vim9PixelScroll::new(&old.windows[0], metrics()).unwrap();
        scroll.integrate(3.0);
        assert_eq!(scroll.offset(&old, metrics()), 0.0);
        assert!(!scroll.is_animating());
        assert!(matches!(
            scroll.command(),
            Some(Vim9Command::ScrollView { lines: 3, .. })
        ));
        scroll.submitted(1);
        let next = wheel_frame(23, 1);
        scroll.reconcile_at(&next.windows[0], metrics(), false, 0.0);
        assert_eq!(scroll.offset(&next, metrics()), 0.0);
        assert!(!scroll.is_animating());
    }
}
