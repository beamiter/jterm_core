//! Shared, content-free physiology time accounting for native companions.
use crate::organism::{CircadianPhase, LifeState, NativeOrganism};
use std::time::Duration;

pub const MAX_TICK: Duration = Duration::from_secs(1);
const INPUT_ACTIVITY: Duration = Duration::from_millis(900);

/// Bounded union of recent, content-free input activity windows.
/// Records only monotonic timestamps, never keys or submitted text. Consumers
/// may measure at most the final second of an interval; older time is dropped.
#[derive(Debug, Default)]
pub struct InputActivity {
    windows: std::collections::VecDeque<(Duration, Duration)>,
    latest: Option<Duration>,
}

impl InputActivity {
    pub fn note_input(&mut self, now: Duration) {
        if self.latest.is_some_and(|last| now < last) {
            return;
        }
        self.latest = Some(now);
        self.prune(now.saturating_sub(MAX_TICK));
        let end = now.saturating_add(INPUT_ACTIVITY);
        if let Some((_, previous_end)) = self.windows.back_mut().filter(|(_, end)| *end >= now) {
            *previous_end = end;
        } else {
            self.windows.push_back((now, end));
        }
        // Each disjoint window is 900ms long, so a one-second lookback
        // plus the current window retains at most three windows.
        debug_assert!(self.windows.len() <= 3);
    }

    pub fn clear(&mut self) {
        self.windows.clear();
        self.latest = None;
    }

    fn prune(&mut self, start: Duration) {
        while self.windows.front().is_some_and(|(_, end)| *end <= start) {
            self.windows.pop_front();
        }
    }

    /// Discard history before a resume boundary, preserving any future tail.
    pub fn trim_before(&mut self, start: Duration) {
        self.prune(start);
        for (input, _) in &mut self.windows {
            *input = (*input).max(start);
        }
    }

    /// Measure the union intersecting the final second of [start, end].
    /// A reversed interval is ignored, including its pruning side effects.
    pub fn active_during(&mut self, start: Duration, end: Duration) -> Duration {
        if end < start {
            return Duration::ZERO;
        }
        let start = start.max(end.saturating_sub(MAX_TICK));
        self.prune(start);
        self.windows
            .iter()
            .fold(Duration::ZERO, |total, (input, until)| {
                total.saturating_add((*until).min(end).saturating_sub((*input).max(start)))
            })
            .min(end.saturating_sub(start))
    }
}

/// One volatile physiology clock per window, never one per pane or frame.
/// Timestamps are injected, monotonic durations; no wall-clock or I/O policy.
pub struct WindowLife {
    state: LifeState,
    anchor: Duration,
    input_activity: InputActivity,
    last_activity: Duration,
    eligible: bool,
}

impl WindowLife {
    pub fn new_at(now: Duration) -> Self {
        Self {
            state: LifeState::default(),
            anchor: now,
            input_activity: InputActivity::default(),
            last_activity: now,
            eligible: false,
        }
    }

    pub fn state(&self) -> LifeState {
        self.state
    }

    pub fn replace_state(&mut self, state: LifeState) {
        self.state = NativeOrganism::from_persisted_state(state).state();
    }

    pub fn note_input(&mut self, now: Duration) {
        if now >= self.anchor && now >= self.last_activity {
            self.input_activity.note_input(now);
            self.last_activity = now;
        }
    }

    pub fn note_output(&mut self, now: Duration) {
        if now >= self.anchor {
            self.last_activity = self.last_activity.max(now);
        }
    }

    pub fn idle_for(&self, now: Duration) -> Duration {
        now.saturating_sub(self.last_activity)
    }

    pub fn advance(
        &mut self,
        now: Duration,
        eligible_live: bool,
        any_running: bool,
        phase: CircadianPhase,
    ) -> f32 {
        // Revocation is authoritative even with a stale timestamp: pause
        // immediately, retaining the monotonic anchor and dropping input.
        if !eligible_live {
            self.anchor = self.anchor.max(now);
            self.eligible = false;
            self.input_activity.clear();
            return 0.0;
        }
        if now < self.anchor {
            return 0.0;
        }
        if any_running {
            self.last_activity = self.last_activity.max(now);
        }
        if !self.eligible {
            self.anchor = now;
            self.eligible = eligible_live;
            self.input_activity.trim_before(now);
            return 0.0;
        }
        let elapsed = now.saturating_sub(self.anchor).min(MAX_TICK);
        self.anchor = now;
        if elapsed.is_zero() {
            return 0.0;
        }
        let resting = !any_running && self.idle_for(now) >= Duration::from_secs(60);
        // Account for input over the consumed interval, not only its endpoint.
        // A 900ms dormant tick therefore cannot miss the 900ms input window.
        let start = now.saturating_sub(elapsed);
        let active = self.input_activity.active_during(start, now);
        let inactive = elapsed.saturating_sub(active);
        if !active.is_zero() {
            self.state.tick(active.as_secs_f32(), true, resting, phase);
        }
        if !inactive.is_zero() {
            self.state
                .tick(inactive.as_secs_f32(), false, resting, phase);
        }
        elapsed.as_secs_f32()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_union_merges_overlaps_without_counting_gaps() {
        let mut activity = InputActivity::default();
        activity.note_input(Duration::ZERO);
        activity.note_input(Duration::from_millis(800));
        activity.note_input(Duration::from_millis(1750));
        assert_eq!(
            activity.active_during(Duration::from_millis(1000), Duration::from_millis(2000)),
            Duration::from_millis(950)
        );
        assert_eq!(activity.windows.len(), 2);
    }

    #[test]
    fn input_union_trims_resume_history_and_ignores_backward_observations() {
        let mut activity = InputActivity::default();
        activity.note_input(Duration::from_millis(500));
        activity.note_input(Duration::ZERO);
        assert_eq!(
            activity.active_during(Duration::from_secs(2), Duration::ZERO),
            Duration::ZERO
        );
        activity.trim_before(Duration::from_secs(1));
        assert_eq!(
            activity.active_during(Duration::ZERO, Duration::from_millis(1400)),
            Duration::from_millis(400)
        );
        activity.clear();
        assert_eq!(
            activity.active_during(Duration::ZERO, Duration::from_secs(2)),
            Duration::ZERO
        );
    }

    #[test]
    fn input_union_bounds_long_intervals_and_timestamp_extremes() {
        let mut activity = InputActivity::default();
        for millis in 0..10_000 {
            activity.note_input(Duration::from_millis(millis));
            assert!(activity.windows.len() <= 3);
        }
        assert_eq!(
            activity.active_during(Duration::ZERO, Duration::from_secs(10)),
            MAX_TICK
        );
        activity.note_input(Duration::MAX);
        assert_eq!(
            activity.active_during(Duration::MAX - MAX_TICK, Duration::MAX),
            Duration::ZERO
        );
    }

    fn clock() -> WindowLife {
        let mut life = WindowLife::new_at(Duration::ZERO);
        life.advance(Duration::ZERO, true, false, CircadianPhase::Unlearned);
        life
    }

    fn assert_activity(life: &WindowLife, active: f32, inactive: f32) {
        let mut expected = LifeState::default();
        expected.tick(active, true, false, CircadianPhase::Unlearned);
        expected.tick(inactive, false, false, CircadianPhase::Unlearned);
        let actual = life.state();
        for (a, b) in [
            (actual.curiosity, expected.curiosity),
            (actual.boredom, expected.boredom),
            (actual.social_need, expected.social_need),
        ] {
            assert!((a - b).abs() < 0.000001, "{a} != {b}");
        }
    }

    #[test]
    fn overlapping_inputs_preserve_the_entire_active_interval() {
        let mut life = clock();
        life.note_input(Duration::ZERO);
        life.note_input(Duration::from_millis(800));
        life.advance(
            Duration::from_millis(900),
            true,
            false,
            CircadianPhase::Unlearned,
        );
        assert_activity(&life, 0.9, 0.0);
    }

    #[test]
    fn separated_inputs_preserve_the_inactive_gap_in_bounded_tick() {
        let mut life = clock();
        life.note_input(Duration::ZERO);
        life.note_input(Duration::from_millis(950));
        life.advance(
            Duration::from_secs(1),
            true,
            false,
            CircadianPhase::Unlearned,
        );
        assert_activity(&life, 0.95, 0.05);
    }

    #[test]
    fn delayed_ticks_count_only_the_last_second_and_storage_is_bounded() {
        let mut life = clock();
        for millis in (0..100_000).step_by(950) {
            life.note_input(Duration::from_millis(millis));
            assert!(life.input_activity.windows.len() <= 3);
        }
        life.advance(
            Duration::from_secs(200),
            true,
            false,
            CircadianPhase::Unlearned,
        );
        assert_activity(&life, 0.0, 1.0);
        assert!(life.input_activity.windows.is_empty());
    }

    #[test]
    fn pause_and_resume_discard_unconsumed_input_without_catchup() {
        let mut life = clock();
        life.note_input(Duration::ZERO);
        life.advance(
            Duration::from_millis(100),
            false,
            false,
            CircadianPhase::Unlearned,
        );
        life.note_input(Duration::from_millis(200));
        assert_eq!(
            life.advance(
                Duration::from_millis(300),
                true,
                false,
                CircadianPhase::Unlearned
            ),
            0.0
        );
        life.advance(
            Duration::from_millis(400),
            true,
            false,
            CircadianPhase::Unlearned,
        );
        assert_activity(&life, 0.1, 0.0);
    }

    #[test]
    fn repeated_timestamp_and_backward_input_do_not_duplicate_activity() {
        let mut life = clock();
        life.note_input(Duration::from_millis(100));
        life.note_input(Duration::from_millis(100));
        life.note_input(Duration::ZERO);
        life.advance(
            Duration::from_secs(1),
            true,
            false,
            CircadianPhase::Unlearned,
        );
        assert_activity(&life, 0.9, 0.1);
        let before = life.state().curiosity;
        assert_eq!(
            life.advance(
                Duration::from_secs(1),
                true,
                false,
                CircadianPhase::Unlearned
            ),
            0.0
        );
        assert_eq!(
            life.advance(Duration::ZERO, true, false, CircadianPhase::Unlearned),
            0.0
        );
        assert_eq!(life.state().curiosity, before);
    }
}
