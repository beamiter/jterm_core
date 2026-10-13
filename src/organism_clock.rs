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

    /// Advance physiology chronologically through input and rest boundaries.
    /// Only the final second is consumed; an empty or reversed interval is a
    /// no-op. `rest_from` selects the quiet/forced-rest boundary, or no rest.
    pub fn tick_life(
        &mut self,
        state: &mut LifeState,
        start: Duration,
        end: Duration,
        rest_from: Option<Duration>,
        phase: CircadianPhase,
    ) -> f32 {
        if end <= start {
            return 0.0;
        }
        let start = start.max(end.saturating_sub(MAX_TICK));
        self.prune(start);
        // At most three retained windows, two endpoints, and one rest
        // boundary. Stack storage avoids allocating on each animation tick.
        let mut cuts = [Duration::ZERO; 9];
        cuts[0] = start;
        cuts[1] = end;
        let mut count = 2;
        for boundary in self
            .windows
            .iter()
            .flat_map(|(from, until)| [*from, *until])
            .chain(rest_from)
        {
            if boundary > start && boundary < end {
                cuts[count] = boundary;
                count += 1;
            }
        }
        cuts[..count].sort_unstable();
        for pair in cuts[..count].windows(2) {
            let (from, until) = (pair[0], pair[1]);
            if from == until {
                continue;
            }
            let active = self
                .windows
                .iter()
                .any(|(input, end)| *input <= from && *end >= until);
            let resting = rest_from.is_some_and(|threshold| from >= threshold);
            state.tick((until - from).as_secs_f32(), active, resting, phase);
        }
        (end - start).as_secs_f32()
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
        let start = now.saturating_sub(elapsed);
        // A tick crossing the quiet threshold must not credit its awake
        // prefix as rest. Overflow means the threshold is unreachable.
        let rest_from = (!any_running)
            .then(|| self.last_activity.checked_add(Duration::from_secs(60)))
            .flatten();
        self.input_activity
            .tick_life(&mut self.state, start, now, rest_from, phase)
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

    fn assert_same_state(actual: LifeState, expected: LifeState) {
        let values = |state: LifeState| {
            [
                state.energy,
                state.mood,
                state.curiosity,
                state.boredom,
                state.stress,
                state.social_need,
                state.attachment,
                state.confidence,
            ]
        };
        for (actual, expected) in values(actual).into_iter().zip(values(expected)) {
            assert!(
                (actual - expected).abs() < 0.000001,
                "{actual} != {expected}"
            );
        }
    }

    #[test]
    fn life_tick_orders_late_input_and_rest_boundaries_chronologically() {
        let mut input = InputActivity::default();
        input.note_input(Duration::from_millis(700));
        let phase = CircadianPhase::Unlearned;
        let mut state = LifeState::default();
        let mut expected = state;
        expected.tick(0.4, false, false, phase);
        expected.tick(0.3, false, true, phase);
        expected.tick(0.3, true, true, phase);
        assert_eq!(
            input.tick_life(
                &mut state,
                Duration::ZERO,
                Duration::from_secs(1),
                Some(Duration::from_millis(400)),
                phase
            ),
            1.0
        );
        assert_same_state(state, expected);
    }

    #[test]
    fn life_tick_preserves_forced_micro_rest_after_the_awake_prefix() {
        let mut input = InputActivity::default();
        input.note_input(Duration::from_millis(700));
        let phase = CircadianPhase::Unlearned;
        let mut state = LifeState {
            energy: 0.1505,
            ..LifeState::default()
        };
        let mut expected = state;
        expected.tick(0.7, false, false, phase);
        expected.tick(0.3, true, false, phase);
        input.tick_life(
            &mut state,
            Duration::ZERO,
            Duration::from_secs(1),
            None,
            phase,
        );
        assert_same_state(state, expected);
        assert!((state.energy - 0.1581).abs() < 0.000001);
    }

    #[test]
    fn life_tick_caps_time_and_empty_or_reversed_intervals_do_not_mutate() {
        let mut input = InputActivity::default();
        input.note_input(Duration::from_millis(1500));
        let phase = CircadianPhase::Unlearned;
        let mut state = LifeState::default();
        let before = state;
        let windows = input.windows.clone();
        assert_eq!(
            input.tick_life(
                &mut state,
                Duration::from_secs(3),
                Duration::from_secs(2),
                Some(Duration::ZERO),
                phase
            ),
            0.0
        );
        assert_eq!(
            input.tick_life(
                &mut state,
                Duration::from_secs(3),
                Duration::from_secs(3),
                None,
                phase
            ),
            0.0
        );
        assert_eq!(input.windows, windows);
        assert_same_state(state, before);
        let mut expected = before;
        expected.tick(0.5, false, true, phase);
        expected.tick(0.5, true, true, phase);
        assert_eq!(
            input.tick_life(
                &mut state,
                Duration::ZERO,
                Duration::from_secs(2),
                Some(Duration::ZERO),
                phase
            ),
            1.0
        );
        assert_same_state(state, expected);
    }

    fn assert_energy_delta(start_ms: u64, end_ms: u64, running: bool, expected: f32) {
        let mut life = clock();
        life.advance(
            Duration::from_millis(start_ms),
            true,
            false,
            CircadianPhase::Unlearned,
        );
        let before = life.state().energy;
        life.advance(
            Duration::from_millis(end_ms),
            true,
            running,
            CircadianPhase::Unlearned,
        );
        assert!((life.state().energy - before - expected).abs() < 0.000001);
    }

    #[test]
    fn quiet_threshold_credits_only_the_resting_tail() {
        assert_energy_delta(59_500, 60_400, false, -0.002 * 0.5 + 0.030 * 0.4);
        // Ending exactly at the threshold has no resting tail.
        assert_energy_delta(59_500, 60_000, false, -0.002 * 0.5);
    }

    #[test]
    fn quiet_threshold_preserves_fully_awake_resting_and_running_ticks() {
        assert_energy_delta(58_500, 59_400, false, -0.002 * 0.9);
        assert_energy_delta(60_000, 60_900, false, 0.030 * 0.9);
        assert_energy_delta(59_500, 60_400, true, -0.002 * 0.9);
        // A long gap still consumes only its final second.
        assert_energy_delta(59_500, 600_000, false, 0.030);
    }

    #[test]
    fn unrepresentable_quiet_threshold_never_manufactures_rest() {
        let start = Duration::MAX - Duration::from_secs(30);
        let mut life = WindowLife::new_at(start);
        life.advance(start, true, false, CircadianPhase::Unlearned);
        let before = life.state().energy;
        life.advance(Duration::MAX, true, false, CircadianPhase::Unlearned);
        assert!((life.state().energy - before + 0.002).abs() < 0.000001);
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
