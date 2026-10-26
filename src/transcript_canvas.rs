//! Continuous logical-pixel motion for native Block and Chat transcripts.
//!
//! One instance belongs to one tab/pane, not to a window-wide input loop.
//! Frontends own the document, reading anchor, follow-live policy and frame
//! clock. This module owns only the presented position and its destination.
//! Use [`CanvasMotion::jump_to`] for precision touchpad/scrollbar movement;
//! the operating system has already supplied that movement and its momentum.
//! Use [`CanvasMotion::scroll_by`] for discrete wheels and
//! [`CanvasMotion::set_target`] for a growing live edge or explicit navigation.
//!
//! Keep the fractional position after motion settles. Paint, clipping and hit
//! testing must all use that same position. A terminal-backed projection may
//! split it into whole rows plus a fractional translation and one overscan row;
//! rounding the final position back to a row recreates the visible jump.

/// Toolkit-independent, critically damped motion in logical pixels.
///
/// The closed-form update uses elapsed seconds, so a 60 Hz display and a
/// 144 Hz display follow the same trajectory. Repeated output retargets the
/// existing motion without restarting an easing timer or losing velocity.
/// No animation, timer or allocation runs when the position has settled.
#[derive(Clone, Debug)]
pub struct CanvasMotion {
    position: f64,
    target: f64,
    velocity: f64,
    lower: f64,
    upper: f64,
    reduced_motion: bool,
}

impl Default for CanvasMotion {
    fn default() -> Self {
        Self::new(0.0)
    }
}

impl CanvasMotion {
    // About 95% of an ordinary wheel step arrives in 150 ms. Unlike a fixed
    // per-frame multiplier this remains the same on high-refresh displays.
    const RESPONSE: f64 = 32.0;
    const SETTLED_DISTANCE: f64 = 0.05;
    const SETTLED_SPEED: f64 = 0.5;
    const MAX_FRAME_GAP: f64 = 0.25;
    // Far above a bounded transcript's extent, while preventing overflow in
    // the spring even if a native allocation briefly reports bad geometry.
    const MAX_COORDINATE: f64 = 1.0e12;

    pub fn new(position: f64) -> Self {
        let position = if position.is_finite() {
            position.clamp(-Self::MAX_COORDINATE, Self::MAX_COORDINATE)
        } else {
            0.0
        };
        Self {
            position,
            target: position,
            velocity: 0.0,
            lower: -Self::MAX_COORDINATE,
            upper: Self::MAX_COORDINATE,
            reduced_motion: false,
        }
    }

    pub fn position(&self) -> f64 {
        self.position
    }

    pub fn target(&self) -> f64 {
        self.target
    }

    /// Current logical pixels per second; useful for diagnostics, not for
    /// applying an additional frontend inertia multiplier.
    pub fn velocity(&self) -> f64 {
        self.velocity
    }

    pub fn is_animating(&self) -> bool {
        self.position != self.target || self.velocity != 0.0
    }

    /// Update the valid scroll range after layout. Invalid geometry is ignored.
    /// Expanding the range never moves a reader; the host explicitly retargets
    /// the live edge only while its own follow-live state is enabled.
    pub fn set_bounds(&mut self, lower: f64, upper: f64) {
        if !lower.is_finite() || !upper.is_finite() || lower > upper {
            return;
        }
        self.lower = lower.clamp(-Self::MAX_COORDINATE, Self::MAX_COORDINATE);
        self.upper = upper.clamp(-Self::MAX_COORDINATE, Self::MAX_COORDINATE);
        self.position = self.clamp(self.position);
        self.target = self.clamp(self.target);
        self.reconcile_direction();
    }

    /// Move the destination without moving the presented pixels. Updating an
    /// unchanged destination does not restart or prolong the motion.
    pub fn set_target(&mut self, position: f64) {
        if !position.is_finite() {
            return;
        }
        self.target = self.clamp(position);
        if self.reduced_motion {
            self.jump_to(self.target);
        } else {
            self.reconcile_direction();
        }
    }

    /// Accumulate a discrete wheel step. Reversing direction starts at the
    /// presented position, so a fresh upward wheel does not first have to
    /// cancel a queued downward destination that has not been shown yet.
    pub fn scroll_by(&mut self, delta: f64) {
        if !delta.is_finite() || delta == 0.0 {
            return;
        }
        let pending = self.target - self.position;
        let reversing = (pending != 0.0 && pending.signum() != delta.signum())
            || (self.velocity != 0.0 && self.velocity.signum() != delta.signum());
        if reversing {
            self.cancel();
        }
        // Clamp the input before addition, so even a finite f64::MAX event
        // cannot overflow and poison all subsequent ordinary input.
        self.set_target(self.target + delta.clamp(-Self::MAX_COORDINATE, Self::MAX_COORDINATE));
    }

    /// Present an exact position immediately, cancelling any pending motion.
    /// Precision input should use `jump_to(position() + raw_pixel_delta)`.
    pub fn jump_to(&mut self, position: f64) {
        if !position.is_finite() {
            return;
        }
        self.position = self.clamp(position);
        self.target = self.position;
        self.velocity = 0.0;
    }

    /// Stop exactly where the reader is looking, retaining fractional pixels.
    pub fn cancel(&mut self) {
        self.target = self.position;
        self.velocity = 0.0;
    }

    /// Rebase coordinates after content above the reading anchor is inserted,
    /// removed or remeasured. Update bounds first. Moving both coordinates by
    /// the same amount preserves the on-screen anchor and ongoing trajectory.
    pub fn shift(&mut self, delta: f64) {
        if !delta.is_finite() || delta == 0.0 {
            return;
        }
        let delta = delta.clamp(-Self::MAX_COORDINATE, Self::MAX_COORDINATE);
        self.position = self.clamp(self.position + delta);
        self.target = self.clamp(self.target + delta);
        self.reconcile_direction();
    }

    /// Honour the desktop's reduced-motion preference without losing an
    /// explicitly requested destination. Direct input stays exact in either mode.
    pub fn set_reduced_motion(&mut self, reduced: bool) {
        self.reduced_motion = reduced;
        if reduced {
            self.jump_to(self.target);
        }
    }

    /// Advance from the last displayed frame by elapsed seconds and return
    /// the new position. Hosts schedule another frame only while
    /// [`is_animating`](Self::is_animating) is true, and reset their frame clock
    /// on remap. A suspended/hidden tab settles instead of replaying old motion.
    pub fn advance(&mut self, dt_seconds: f64) -> f64 {
        if !dt_seconds.is_finite() || dt_seconds <= 0.0 || !self.is_animating() {
            return self.position;
        }
        if self.reduced_motion || dt_seconds > Self::MAX_FRAME_GAP {
            self.jump_to(self.target);
            return self.position;
        }

        // Analytic critically damped spring for a constant destination during
        // this frame: x(t) = target + (d + (v + w*d)*t) * exp(-w*t).
        let distance = self.position - self.target;
        let coefficient = self.velocity + Self::RESPONSE * distance;
        let decay = (-Self::RESPONSE * dt_seconds).exp();
        let next = self.target + (distance + coefficient * dt_seconds) * decay;
        let velocity = (self.velocity - Self::RESPONSE * coefficient * dt_seconds) * decay;

        // A live edge can move backwards when a progress line is rewritten or
        // a card collapses. A previously fast spring must not overshoot it.
        if (next - self.target) * distance <= 0.0
            || ((next - self.target).abs() <= Self::SETTLED_DISTANCE
                && velocity.abs() <= Self::SETTLED_SPEED)
        {
            self.jump_to(self.target);
        } else {
            self.position = self.clamp(next);
            self.velocity = velocity;
            self.reconcile_direction();
        }
        self.position
    }

    fn clamp(&self, position: f64) -> f64 {
        position.clamp(self.lower, self.upper)
    }

    fn reconcile_direction(&mut self) {
        let direction = self.target - self.position;
        if direction == 0.0
            || (self.velocity != 0.0 && self.velocity.signum() != direction.signum())
        {
            self.velocity = 0.0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::CanvasMotion;

    fn bounded(position: f64) -> CanvasMotion {
        let mut motion = CanvasMotion::new(position);
        motion.set_bounds(0.0, 10_000.0);
        motion
    }

    #[test]
    fn wheel_is_continuous_and_lands_on_exact_fractional_destination() {
        let mut motion = bounded(120.25);
        motion.scroll_by(54.125);
        assert_eq!(motion.position(), 120.25, "input must not jump the canvas");
        assert_eq!(motion.target(), 174.375);
        let mut previous = motion.position();
        let mut moving_frames = 0;
        for _ in 0..120 {
            let position = motion.advance(1.0 / 120.0);
            assert!(position >= previous && position <= 174.375);
            moving_frames += usize::from(motion.is_animating());
            previous = position;
        }
        assert!(
            moving_frames > 10,
            "a wheel step needs real intermediate frames"
        );
        assert!(!motion.is_animating());
        assert_eq!(motion.position(), 174.375, "never round to a terminal row");
    }

    #[test]
    fn trajectory_is_independent_of_display_refresh_rate() {
        let mut reference = bounded(100.0);
        reference.set_target(500.0);
        reference.advance(0.2);
        for hz in [30, 60, 90, 120, 144, 240] {
            let mut motion = bounded(100.0);
            motion.set_target(500.0);
            let whole = (0.2 * f64::from(hz)).floor() as u32;
            for _ in 0..whole {
                motion.advance(1.0 / f64::from(hz));
            }
            motion.advance(0.2 - f64::from(whole) / f64::from(hz));
            assert!((motion.position() - reference.position()).abs() < 1e-9);
            assert!((motion.velocity() - reference.velocity()).abs() < 1e-8);
        }
    }

    #[test]
    fn precision_input_is_exact_and_cancels_queued_wheel_motion() {
        let mut motion = bounded(500.0);
        motion.scroll_by(108.0);
        motion.advance(1.0 / 60.0);
        let before = motion.position();
        motion.jump_to(before - 0.25);
        assert_eq!(motion.position(), before - 0.25);
        assert!(!motion.is_animating());
        assert_eq!(motion.advance(0.1), before - 0.25);
    }

    #[test]
    fn reversing_wheel_responds_from_the_visible_position() {
        let mut motion = bounded(500.0);
        motion.scroll_by(500.0);
        motion.advance(1.0 / 60.0);
        let before = motion.position();
        motion.scroll_by(-20.0);
        assert_eq!(motion.target(), before - 20.0);
        assert!(motion.advance(1.0 / 120.0) < before);
    }

    #[test]
    fn repeated_live_growth_keeps_position_and_velocity_continuous() {
        let mut motion = bounded(500.0);
        motion.set_target(550.0);
        motion.advance(0.03);
        let position = motion.position();
        let velocity = motion.velocity();
        for target in [560.0, 580.0, 600.0] {
            motion.set_target(target);
            assert_eq!(motion.position(), position);
            assert_eq!(motion.velocity(), velocity);
        }
        assert!(motion.advance(1.0 / 60.0) > position);
    }

    #[test]
    fn unchanged_live_target_does_not_keep_animation_alive() {
        let mut motion = bounded(500.0);
        for _ in 0..240 {
            motion.set_target(550.25);
            motion.advance(1.0 / 120.0);
        }
        assert_eq!(motion.position(), 550.25);
        assert!(!motion.is_animating());
    }

    #[test]
    fn output_below_a_reader_does_not_move_their_position() {
        let mut motion = bounded(500.25);
        motion.set_bounds(0.0, 20_000.0);
        for _ in 0..30 {
            assert_eq!(motion.advance(1.0 / 60.0), 500.25);
        }
        assert!(!motion.is_animating());
    }

    #[test]
    fn user_interruption_stays_put_when_more_output_arrives() {
        let mut motion = bounded(500.0);
        motion.set_target(900.0);
        motion.advance(0.04);
        motion.cancel();
        let reading = motion.position();
        motion.set_bounds(0.0, 30_000.0);
        assert_eq!(motion.advance(0.2), reading);
        assert!(!motion.is_animating());
    }

    #[test]
    fn anchor_rebase_keeps_screen_coordinate_and_pending_distance() {
        let mut motion = bounded(500.25);
        motion.set_target(800.5);
        motion.advance(0.03);
        let anchor_document_y = 600.0;
        let screen_y = anchor_document_y - motion.position();
        let pending = motion.target() - motion.position();
        let velocity = motion.velocity();
        motion.shift(200.0);
        assert!((anchor_document_y + 200.0 - motion.position() - screen_y).abs() < 1e-10);
        assert!((motion.target() - motion.position() - pending).abs() < 1e-10);
        assert_eq!(motion.velocity(), velocity);
    }

    #[test]
    fn collapsing_content_clamps_both_position_and_destination() {
        let mut motion = bounded(500.0);
        motion.set_target(900.0);
        motion.advance(0.1);
        motion.set_bounds(0.0, 300.0);
        assert_eq!(motion.position(), 300.0);
        assert_eq!(motion.target(), 300.0);
        assert!(!motion.is_animating());
    }

    #[test]
    fn shortened_destination_never_overshoots_with_old_velocity() {
        let mut motion = bounded(100.0);
        motion.set_target(1_000.0);
        motion.advance(0.04);
        let destination = motion.position() + 0.125;
        motion.set_target(destination);
        for _ in 0..120 {
            assert!(motion.advance(1.0 / 120.0) <= destination);
        }
        assert_eq!(motion.position(), destination);
        assert!(!motion.is_animating());
    }

    #[test]
    fn no_overscroll_accumulates_at_document_boundaries() {
        let mut motion = bounded(0.0);
        for _ in 0..100 {
            motion.scroll_by(-54.0);
            motion.advance(1.0 / 60.0);
        }
        assert!(!motion.is_animating());
        motion.scroll_by(54.0);
        assert_eq!(motion.target(), 54.0);
        assert!(motion.advance(1.0 / 60.0) > 0.0);
    }

    #[test]
    fn reduced_motion_and_resumed_tabs_do_not_replay_old_animations() {
        let mut motion = bounded(100.0);
        motion.set_target(500.25);
        motion.set_reduced_motion(true);
        assert_eq!(motion.position(), 500.25);
        motion.scroll_by(54.0);
        assert_eq!(motion.position(), 554.25);
        assert!(!motion.is_animating());
        motion.set_reduced_motion(false);
        motion.set_target(1_000.25);
        assert_eq!(motion.advance(10.0), 1_000.25);
        assert!(!motion.is_animating());
    }

    #[test]
    fn independent_panes_never_exchange_momentum() {
        let mut first = bounded(100.0);
        let mut second = bounded(500.25);
        first.scroll_by(108.0);
        first.advance(0.04);
        second.advance(0.04);
        assert!(first.is_animating());
        assert_eq!(second.position(), 500.25);
        assert!(!second.is_animating());
    }

    #[test]
    fn nonfinite_input_and_invalid_layout_do_not_poison_the_canvas() {
        let mut motion = bounded(100.25);
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            motion.set_target(bad);
            motion.jump_to(bad);
            motion.scroll_by(bad);
            motion.shift(bad);
            motion.set_bounds(0.0, bad);
            assert_eq!(motion.advance(bad), 100.25);
        }
        motion.set_bounds(500.0, 100.0);
        motion.scroll_by(10.0);
        assert_eq!(motion.target(), 110.25);
        assert_eq!(motion.advance(-1.0), 100.25);
        assert!(motion.advance(0.01).is_finite());
    }

    #[test]
    fn enormous_finite_deltas_saturate_without_overflow() {
        let mut motion = bounded(100.0);
        motion.scroll_by(f64::MAX);
        assert_eq!(motion.target(), 10_000.0);
        assert!(motion.advance(0.01).is_finite());
        motion.scroll_by(-f64::MAX);
        assert_eq!(motion.target(), 0.0);
        assert!(motion.advance(0.01).is_finite());
    }
}
