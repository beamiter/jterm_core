//! Turning toolkit scroll deltas into whole wheel notches.
//!
//! An application with mouse reporting on (claude's fullscreen UI, opencode,
//! htop) is told about the wheel one notch at a time: every report is a full
//! button-4/5 press. A scroll event, however, is not a notch. Touchpads and
//! high-resolution wheels deliver fractions of one, and on Wayland GTK 4.14
//! delivers touchpad scrolls in surface pixels (`GDK_SCROLL_UNIT_SURFACE`),
//! which `EventControllerScroll` converts to steps only when its DISCRETE
//! flag is set. A host that sends one report per event, whatever its size,
//! scrolls an application pages at a time for a gentle two-finger swipe.
//!
//! libvte accumulates instead (vte.cc `m_mouse_smooth_scroll_delta += dy`,
//! then reports `trunc(sum)` notches and keeps the remainder), and
//! [`WheelAccumulator`] does the same for hosts that synthesise wheel reports
//! themselves. Unlike VTE 0.76 it also normalises surface-unit deltas, which
//! VTE's own GTK 4 path passes through as if each pixel were a notch.
//!
//! No toolkit types appear here: the caller says whether the event's unit was
//! the surface one (`EventControllerScroll::unit() == ScrollUnit::Surface`).

/// Surface pixels per wheel step. This is GTK's own
/// `SURFACE_UNIT_DISCRETE_MAPPING`, the factor `EventControllerScroll` uses
/// when it is asked for discrete steps, so a converted swipe scrolls the
/// application as far as GTK would scroll a widget.
pub const SURFACE_UNITS_PER_STEP: f64 = 10.0;

/// How close an accumulated sum must come to a whole step to count as one.
/// Summing binary fractions drifts below the integer — ten deltas of 0.3
/// leave 0.9999999999999998 after the third step — and plain truncation would
/// then lose a notch the user physically scrolled. Real deltas are nowhere
/// near this fine.
const WHOLE_STEP_TOLERANCE: f64 = 1e-6;

/// Carries the fractional part of a scroll gesture from one event to the next
/// and hands out whole notches.
///
/// One accumulator belongs to one scroll target. The caller resets it when the
/// gesture ends (`EventControllerScroll::scroll-end`) and whenever the target
/// changes meaning under the pointer, such as an alternate-screen switch, so a
/// remainder never leaks into an unrelated scroll.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct WheelAccumulator {
    /// Steps accumulated but not yet reported; always strictly between -1 and
    /// 1 between calls.
    pending: f64,
}

impl WheelAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add one scroll event's vertical delta and return the signed number of
    /// whole notches it completes: negative scrolls up (towards history),
    /// positive down, as in GTK.
    ///
    /// `surface_unit` deltas are pixels and are divided by
    /// [`SURFACE_UNITS_PER_STEP`] first. A reversal drops whatever was
    /// pending in the old direction, so turning the wheel back never has to
    /// unwind a remainder before it moves. A plain wheel notch (±1.0 in
    /// wheel units) always yields exactly ±1.
    pub fn push(&mut self, dy: f64, surface_unit: bool) -> i32 {
        let steps = if surface_unit {
            dy / SURFACE_UNITS_PER_STEP
        } else {
            dy
        };
        // A non-finite delta would poison every later event; drop it.
        if !steps.is_finite() {
            return 0;
        }
        if steps != 0.0 && self.pending != 0.0 && steps.signum() != self.pending.signum() {
            self.pending = 0.0;
        }
        self.pending += steps;
        let whole = (self.pending + WHOLE_STEP_TOLERANCE.copysign(self.pending)).trunc();
        self.pending -= whole;
        // The tolerance can overshoot the integer by a rounding error; that
        // residue is not a real remainder in the other direction.
        if self.pending.abs() < WHOLE_STEP_TOLERANCE {
            self.pending = 0.0;
        }
        // `as` saturates, so even an absurd delta cannot wrap the count.
        whole as i32
    }

    /// Forget any pending fraction: the gesture ended or its target changed.
    pub fn reset(&mut self) {
        self.pending = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn total(acc: &mut WheelAccumulator, deltas: &[f64], surface_unit: bool) -> i32 {
        deltas.iter().map(|&dy| acc.push(dy, surface_unit)).sum()
    }

    #[test]
    fn plain_wheel_notches_pass_through_one_for_one() {
        let mut acc = WheelAccumulator::new();
        assert_eq!(acc.push(1.0, false), 1);
        assert_eq!(acc.push(1.0, false), 1);
        assert_eq!(acc.push(-1.0, false), -1);
        assert_eq!(acc.pending, 0.0);
    }

    #[test]
    fn fractional_wheel_deltas_accumulate_into_whole_notches() {
        let mut acc = WheelAccumulator::new();
        // A touchpad on X11 or a hi-res wheel: ten 0.3 deltas are three
        // notches, not ten, and none is lost to float drift.
        assert_eq!(total(&mut acc, &[0.3; 10], false), 3);
        assert_eq!(acc.pending, 0.0);

        let mut acc = WheelAccumulator::new();
        assert_eq!(acc.push(0.3, false), 0);
        assert_eq!(acc.push(0.3, false), 0);
        assert_eq!(acc.push(0.4, false), 1);
    }

    #[test]
    fn a_large_delta_reports_several_notches_and_carries_the_rest() {
        let mut acc = WheelAccumulator::new();
        assert_eq!(acc.push(-2.5, false), -2);
        assert_eq!(acc.pending, -0.5);
        assert_eq!(acc.push(-0.5, false), -1);
        assert_eq!(acc.pending, 0.0);
    }

    #[test]
    fn surface_pixels_are_converted_to_steps() {
        let mut acc = WheelAccumulator::new();
        // 25 px of a Wayland touchpad swipe: two steps, five pixels kept.
        assert_eq!(acc.push(25.0, true), 2);
        assert_eq!(acc.pending, 0.5);
        assert_eq!(acc.push(5.0, true), 1);
        // One pixel at a time never becomes one notch per pixel.
        let mut acc = WheelAccumulator::new();
        assert_eq!(total(&mut acc, &[1.0; 9], true), 0);
        assert_eq!(acc.push(1.0, true), 1);
    }

    #[test]
    fn reversing_direction_drops_the_old_remainder() {
        let mut acc = WheelAccumulator::new();
        assert_eq!(acc.push(0.9, false), 0);
        // Without the reset this would net to 0.4 and report nothing up.
        assert_eq!(acc.push(-1.0, false), -1);
        assert_eq!(acc.pending, 0.0);
        assert_eq!(acc.push(-0.6, false), 0);
        assert_eq!(acc.push(0.6, false), 0);
        assert_eq!(acc.pending, 0.6);
    }

    #[test]
    fn a_zero_delta_keeps_the_remainder() {
        // A horizontal-only event carries dy == 0.0; it is not a reversal.
        let mut acc = WheelAccumulator::new();
        assert_eq!(acc.push(0.5, false), 0);
        assert_eq!(acc.push(0.0, false), 0);
        assert_eq!(acc.push(-0.0, false), 0);
        assert_eq!(acc.push(0.5, false), 1);
    }

    #[test]
    fn reset_forgets_the_pending_fraction() {
        let mut acc = WheelAccumulator::new();
        assert_eq!(acc.push(0.7, false), 0);
        acc.reset();
        assert_eq!(acc, WheelAccumulator::new());
        assert_eq!(acc.push(0.7, false), 0);
    }

    #[test]
    fn non_finite_deltas_are_ignored() {
        let mut acc = WheelAccumulator::new();
        assert_eq!(acc.push(0.5, false), 0);
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(acc.push(bad, false), 0);
            assert_eq!(acc.push(bad, true), 0);
        }
        assert_eq!(acc.push(0.5, false), 1, "the remainder survived");
    }
}
