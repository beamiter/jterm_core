//! Fine wheel input and time-based presentation shared by all native buffers.
//! Vim remains the viewport authority: animation starts only after an actual
//! semantic frame acknowledges scrolling, never by predicting document text.

use std::collections::HashMap;

/// Retain sub-line trackpad input independently for each split. GUI toolkits
/// convert pixel events using the actual line height; discrete notches normally
/// use three lines. Horizontal events and Ctrl+wheel remain frontend concerns.
#[derive(Clone, Debug, Default)]
pub struct Vim9ScrollAccumulator {
    fractions: HashMap<i64, f64>,
}

impl Vim9ScrollAccumulator {
    pub fn add(&mut self, window: i64, delta_lines: f64) -> i32 {
        if window <= 0 || !delta_lines.is_finite() || delta_lines == 0.0 {
            return 0;
        }
        if self.fractions.len() >= 16 && !self.fractions.contains_key(&window) {
            self.fractions.clear();
        }
        let fraction = self.fractions.entry(window).or_default();
        // Reverse immediately instead of first paying down an old fractional
        // gesture. This matters when stopping a fast trackpad with a tiny flick.
        if *fraction != 0.0 && fraction.signum() != delta_lines.signum() {
            *fraction = 0.0;
        }
        let total = *fraction + delta_lines.clamp(-120.0, 120.0);
        let lines = total.trunc().clamp(-120.0, 120.0) as i32;
        *fraction = (total - f64::from(lines)).clamp(-0.999_999, 0.999_999);
        lines
    }

    pub fn clear(&mut self) {
        self.fractions.clear();
    }
}

/// A short easing tail that is independent of refresh rate. Hosts apply the
/// returned logical-pixel offset only inside the owning split, retain exposed
/// edge rows, and clear it on editing, input, zoom or topology changes.
#[derive(Clone, Debug, Default)]
pub struct Vim9ScrollAnimation {
    initial_offset: f64,
    started: f64,
}

impl Vim9ScrollAnimation {
    const DURATION: f64 = 0.10;

    pub fn retarget(&mut self, delta_pixels: f64, now_seconds: f64) {
        if !delta_pixels.is_finite() || !now_seconds.is_finite() {
            self.clear();
            return;
        }
        self.initial_offset = (self.offset(now_seconds) + delta_pixels).clamp(-8192.0, 8192.0);
        self.started = now_seconds;
    }

    pub fn offset(&self, now_seconds: f64) -> f64 {
        if !now_seconds.is_finite() || now_seconds < self.started {
            return 0.0;
        }
        let t = ((now_seconds - self.started) / Self::DURATION).clamp(0.0, 1.0);
        let value = self.initial_offset * (1.0 - t).powi(3);
        if value.abs() < 0.05 {
            0.0
        } else {
            value
        }
    }

    pub fn is_active(&self, now_seconds: f64) -> bool {
        self.offset(now_seconds) != 0.0
    }

    pub fn clear(&mut self) {
        self.initial_offset = 0.0;
        self.started = 0.0;
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
}
