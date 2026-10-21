//! Fine wheel input and time-based presentation shared by all native buffers.
//! Vim remains the viewport authority: animation starts only after an actual
//! semantic frame acknowledges scrolling, never by predicting document text.

use std::collections::VecDeque;

/// Retain sub-line trackpad input independently for each split. GUI toolkits
/// convert pixel events using the actual line height; discrete notches normally
/// use three lines. Horizontal events and Ctrl+wheel remain frontend concerns.
#[derive(Clone, Debug, Default)]
pub struct Vim9ScrollAccumulator {
    // Least recently used first. Eviction must not erase other live gestures.
    fractions: VecDeque<(i64, f64)>,
}

impl Vim9ScrollAccumulator {
    pub fn add(&mut self, window: i64, delta_lines: f64) -> i32 {
        if window <= 0 || !delta_lines.is_finite() || delta_lines == 0.0 {
            return 0;
        }
        let mut fraction = if let Some(index) = self.fractions.iter().position(|v| v.0 == window) {
            self.fractions.remove(index).map_or(0.0, |v| v.1)
        } else {
            if self.fractions.len() >= 16 {
                self.fractions.pop_front();
            }
            0.0
        };
        // Reverse immediately instead of first paying down an old fractional
        // gesture. This matters when stopping a fast trackpad with a tiny flick.
        if fraction != 0.0 && fraction.signum() != delta_lines.signum() {
            fraction = 0.0;
        }
        let total = fraction + delta_lines.clamp(-120.0, 120.0);
        // Ten 0.1-line events must emit one line, not stall at 0.9999999999999999.
        // Snap only floating-point noise, never a meaningful sub-line movement.
        let nearest = total.round();
        let total = if nearest.abs() <= 120.0
            && (total - nearest).abs() <= 8.0 * f64::EPSILON * total.abs().max(1.0)
        {
            nearest
        } else {
            total
        };
        let lines = total.trunc().clamp(-120.0, 120.0) as i32;
        self.fractions
            .push_back((window, (total - f64::from(lines)).clamp(-1.0, 1.0)));
        lines
    }

    /// Discard a closed or repurposed split without disturbing other gestures.
    pub fn discard(&mut self, window: i64) {
        self.fractions.retain(|v| v.0 != window);
    }

    pub fn clear(&mut self) {
        self.fractions.clear();
    }
}

#[derive(Clone, Debug)]
struct ScrollImpulse {
    offset: f64,
    started: f64,
    duration: f64,
}

impl ScrollImpulse {
    fn value(&self, now: f64) -> f64 {
        if now < self.started || self.duration <= 0.0 {
            return 0.0;
        }
        let t = ((now - self.started) / self.duration).clamp(0.0, 1.0);
        self.offset * (1.0 - t).powi(3)
    }

    fn deadline(&self) -> f64 {
        self.started + self.duration
    }
}

/// A short easing tail that is independent of refresh rate. Hosts apply the
/// returned logical-pixel offset only inside the owning split, retain exposed
/// edge rows, and clear it on editing, input, zoom or topology changes.
///
/// Each acknowledged movement keeps its original deadline. Repeated updates
/// do not restart old motion, so sustained scrolling cannot build a renewed
/// tail on every frame. This remains acknowledged-row presentation, not a
/// speculative pixel viewport or an additional source of touchpad momentum.
#[derive(Clone, Debug, Default)]
pub struct Vim9ScrollAnimation {
    impulses: VecDeque<ScrollImpulse>,
    last_update: Option<f64>,
}

impl Vim9ScrollAnimation {
    const DURATION: f64 = 0.10;
    const MAX_IMPULSES: usize = 64;
    const MAX_OFFSET: f64 = 8192.0;
    const SETTLED: f64 = 0.05;

    pub fn retarget(&mut self, delta_pixels: f64, now_seconds: f64) {
        if !delta_pixels.is_finite() || !now_seconds.is_finite() {
            self.clear();
            return;
        }
        if self.last_update.is_some_and(|last| now_seconds < last) {
            self.clear();
        }
        self.last_update = Some(now_seconds);
        self.impulses.retain(|v| now_seconds < v.deadline());
        // A frame with no viewport movement must not prolong an old gesture.
        if delta_pixels == 0.0 {
            return;
        }
        let current = self.raw_offset(now_seconds);
        let target = (current + delta_pixels).clamp(-Self::MAX_OFFSET, Self::MAX_OFFSET);
        let delta = target - current;
        if delta == 0.0 {
            return;
        }
        // Preserve the sampled position and the original completion deadline
        // even under pathological event rates. Normal frame-coalesced input
        // stays well below this bound, including 240 Hz presentation.
        if self.impulses.len() >= Self::MAX_IMPULSES {
            let first = self.impulses.pop_front().expect("bounded impulse queue");
            let second = self.impulses.pop_front().expect("bounded impulse queue");
            self.impulses.push_front(ScrollImpulse {
                offset: first.value(now_seconds) + second.value(now_seconds),
                started: now_seconds,
                duration: (first.deadline().max(second.deadline()) - now_seconds).max(0.0),
            });
        }
        self.impulses.push_back(ScrollImpulse {
            offset: delta,
            started: now_seconds,
            duration: Self::DURATION,
        });
    }

    fn raw_offset(&self, now_seconds: f64) -> f64 {
        self.impulses.iter().map(|v| v.value(now_seconds)).sum()
    }

    pub fn offset(&self, now_seconds: f64) -> f64 {
        if !now_seconds.is_finite() || self.last_update.is_some_and(|last| now_seconds < last) {
            return 0.0;
        }
        let value = self
            .raw_offset(now_seconds)
            .clamp(-Self::MAX_OFFSET, Self::MAX_OFFSET);
        if value.abs() < Self::SETTLED {
            0.0
        } else {
            value
        }
    }

    pub fn is_active(&self, now_seconds: f64) -> bool {
        if !now_seconds.is_finite() || self.last_update.is_some_and(|last| now_seconds < last) {
            return false;
        }
        // Opposing impulses may cross zero before both finish. Stopping the
        // frame clock at that instant would freeze a later nonzero offset.
        self.impulses
            .iter()
            .map(|v| v.value(now_seconds).abs())
            .sum::<f64>()
            >= Self::SETTLED
    }

    pub fn clear(&mut self) {
        self.impulses.clear();
        self.last_update = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trackpad_preserves_fractions_and_split_identity() {
        let mut input = Vim9ScrollAccumulator::default();
        assert_eq!(input.add(1001, 0.4), 0);
        assert_eq!(input.add(1002, 0.8), 0);
        assert_eq!(input.add(1001, 0.7), 1);
        assert_eq!(input.add(1002, 0.3), 1);
        assert_eq!(input.add(1001, -0.6), 0);
        assert_eq!(input.add(1001, -0.5), -1);
        input.clear();
        assert_eq!(input.add(1001, 0.9), 0);
    }

    #[test]
    fn wheel_is_bounded_and_bad_geometry_cannot_poison_input() {
        let mut input = Vim9ScrollAccumulator::default();
        assert_eq!(input.add(1001, f64::NAN), 0);
        assert_eq!(input.add(1001, f64::INFINITY), 0);
        assert_eq!(input.add(0, 10.0), 0);
        assert_eq!(input.add(1001, 9000.0), 120);
        assert_eq!(input.add(1001, -9000.0), -120);
        assert_eq!(input.add(1001, 3.0), 3);
        for id in 1..500 {
            input.add(id, 0.2);
        }
        assert!(input.fractions.len() <= 16);
    }

    #[test]
    fn decimal_deltas_do_not_stall_at_an_integer_boundary() {
        for sign in [-1.0, 1.0] {
            let mut input = Vim9ScrollAccumulator::default();
            let lines: i32 = (0..1000).map(|_| input.add(1, sign * 0.1)).sum();
            assert_eq!(lines, (sign * 100.0) as i32);
            assert_eq!(input.add(1, sign * 0.01), 0);
        }
    }

    #[test]
    fn evicting_an_idle_split_preserves_recent_gestures() {
        let mut input = Vim9ScrollAccumulator::default();
        for window in 1..=16 {
            input.add(window, 0.4);
        }
        input.add(1, 0.1);
        input.add(17, 0.2);
        assert_eq!(input.add(1, 0.5), 1);
        assert_eq!(input.add(16, 0.6), 1);
        input.discard(17);
        assert_eq!(input.add(17, 0.8), 0);
        assert_eq!(input.add(2, 0.6), 0);
    }

    #[test]
    fn presentation_eases_continuously_without_frame_rate_dependence() {
        let mut motion = Vim9ScrollAnimation::default();
        motion.retarget(48.0, 1.0);
        assert_eq!(motion.offset(1.0), 48.0);
        assert!((motion.offset(1.05) - 6.0).abs() < 1e-8);
        let before = motion.offset(1.05);
        motion.retarget(24.0, 1.05);
        assert!((motion.offset(1.05) - before - 24.0).abs() < 1e-8);
        assert!(motion.is_active(1.06));
        assert!(!motion.is_active(1.15));
        motion.retarget(10.0, 2.0);
        motion.clear();
        assert_eq!(motion.offset(2.0), 0.0);
    }

    #[test]
    fn new_frames_do_not_restart_old_impulse_deadlines() {
        let mut motion = Vim9ScrollAnimation::default();
        motion.retarget(48.0, 1.0);
        motion.retarget(24.0, 1.05);
        assert!((motion.offset(1.10) - 3.0).abs() < 1e-8);
        motion.retarget(0.0, 1.14);
        assert!(!motion.is_active(1.151));
        assert_eq!(motion.offset(1.151), 0.0);
    }

    #[test]
    fn reversing_through_zero_keeps_the_frame_clock_running() {
        let mut motion = Vim9ScrollAnimation::default();
        motion.retarget(48.0, 1.0);
        motion.retarget(-motion.offset(1.05), 1.05);
        assert_eq!(motion.offset(1.05), 0.0);
        assert!(motion.is_active(1.05));
        assert!(motion.offset(1.06) < 0.0);
        assert!(!motion.is_active(1.151));
    }

    #[test]
    fn continuous_scroll_has_less_lag_than_a_restarted_tail() {
        for hz in [60.0, 120.0, 144.0, 240.0] {
            let mut motion = Vim9ScrollAnimation::default();
            let step = 1.0 / hz;
            let delta = 600.0 / hz;
            let mut previous = 0.0;
            for frame in 0..1000 {
                let now = f64::from(frame) * step;
                previous = previous * (1.0 - step / 0.10).powi(3) + delta;
                motion.retarget(delta, now);
                if frame > 100 {
                    assert!(motion.offset(now) < previous * 0.90, "{hz} Hz");
                }
            }
        }
    }

    #[test]
    fn querying_at_extra_refresh_ticks_does_not_change_the_trajectory() {
        let mut motion = Vim9ScrollAnimation::default();
        motion.retarget(48.0, 1.0);
        motion.retarget(12.0, 1.02);
        let untouched = motion.clone();
        for tick in 0..25 {
            let _ = motion.offset(1.02 + f64::from(tick) / 240.0);
        }
        assert_eq!(motion.offset(1.07), untouched.offset(1.07));
    }

    #[test]
    fn pathological_input_is_bounded_and_still_settles() {
        let mut motion = Vim9ScrollAnimation::default();
        for event in 0..5000 {
            let now = 1.0 + f64::from(event) / 10_000.0;
            let before = motion.offset(now);
            motion.retarget(4.0, now);
            assert!((motion.offset(now) - before - 4.0).abs() < 0.051);
            assert!(motion.impulses.len() <= Vim9ScrollAnimation::MAX_IMPULSES);
        }
        assert!(!motion.is_active(1.601));
        motion.retarget(f64::MAX, 2.0);
        assert_eq!(motion.offset(2.0), Vim9ScrollAnimation::MAX_OFFSET);
        motion.retarget(f64::NAN, 2.01);
        assert!(!motion.is_active(2.01));
        motion.retarget(10.0, 3.0);
        motion.retarget(5.0, 1.0);
        assert_eq!(motion.offset(1.0), 5.0);
        assert_eq!(motion.offset(f64::INFINITY), 0.0);
        assert!(!motion.is_active(f64::NAN));
    }
}
