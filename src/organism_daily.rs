//! Small, offline presentation contracts for the organism's everyday UI.
//!
//! Settings can render [`PreviewPose`] with the existing organism renderer and
//! offer a gentle, explicit interaction through [`GentleInteraction`]. Neither
//! operation touches the reducer, life state, repo memory, or terminal input.
//! This module has no clock, I/O, persistence, content collection, or toolkit
//! dependency. Hosts own visibility, focus, and Full/Calm/Static frame cadence;
//! the live terminal organism can remain a non-interactive overlay.

use std::time::Duration;

use crate::organism::{Behavior, BodyLanguage, RenderContext};
use crate::organism_attention::{AttentionArbiter, AttentionCue, AttentionPolicy};

/// Representative poses for a settings gallery, never synthetic work events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewPose {
    Calm,
    Curious,
    Working,
    Waiting,
    Success,
    Concerned,
    Sleeping,
    Greeting,
}

impl PreviewPose {
    pub const ALL: [Self; 8] = [
        Self::Calm,
        Self::Curious,
        Self::Working,
        Self::Waiting,
        Self::Success,
        Self::Concerned,
        Self::Sleeping,
        Self::Greeting,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Calm => "Calm",
            Self::Curious => "Curious",
            Self::Working => "Working",
            Self::Waiting => "Waiting",
            Self::Success => "Success",
            Self::Concerned => "Concerned",
            Self::Sleeping => "Sleeping",
            Self::Greeting => "Greeting",
        }
    }

    /// Explain the example without claiming that the displayed work happened.
    pub const fn explanation(self) -> &'static str {
        match self {
            Self::Calm => "A quiet moment between commands.",
            Self::Curious => "A little exploration during idle time.",
            Self::Working => "Watching while a command runs.",
            Self::Waiting => "Settling in while a longer command continues.",
            Self::Success => "A small celebration after successful work.",
            Self::Concerned => "Looking toward a command that reported failure.",
            Self::Sleeping => "Resting during a quiet or low-energy moment.",
            Self::Greeting => "A brief, friendly response to attention.",
        }
    }

    /// Independent visual input; feed it to `sprite_frame_with_context`.
    /// Hosts may set growth, rhythm, or frame without creating reducer events.
    pub const fn context(self) -> RenderContext {
        let behavior = match self {
            Self::Calm => Behavior::Idle,
            Self::Curious => Behavior::Explore,
            Self::Working => Behavior::WatchCommand,
            Self::Waiting => Behavior::WatchSettled,
            Self::Success => Behavior::Celebrate,
            Self::Concerned => Behavior::InspectError,
            Self::Sleeping => Behavior::Sleep,
            Self::Greeting => Behavior::Approach,
        };
        RenderContext::new(
            behavior,
            BodyLanguage {
                drowsy: false,
                tense: false,
                listless: false,
            },
            false,
        )
    }
}

/// A short explanation of an authoritative live pose. It describes the
/// existing behavior only; it does not infer command text, output, or intent.
pub const fn behavior_explanation(behavior: Behavior) -> &'static str {
    match behavior {
        Behavior::Idle => "Quietly keeping you company between commands.",
        Behavior::WatchCommand => "Watching a running command.",
        Behavior::InspectError => "A command reported failure; taking a closer look.",
        Behavior::SitNearError => "Staying beside a recent failure.",
        Behavior::Celebrate => "A little celebration after successful work.",
        Behavior::CelebrateBig => "Celebrating a successful recovery.",
        Behavior::RestAfterPush => "Resting after a successful push.",
        Behavior::UnknownOutcome => "The command ended without a confirmed outcome.",
        Behavior::GlanceAside => "Another local pane reported a failure.",
        Behavior::Sleep => "Resting during a quiet or low-energy moment.",
        Behavior::Explore => "Exploring during idle time.",
        Behavior::Approach => "Moving a little closer for company.",
        Behavior::WatchAgent => "Watching the local Shell Agent work.",
        Behavior::WatchSettled => "Settled in while a longer command continues.",
        Behavior::GuardFailure => "Keeping unresolved build or test failures in sight.",
        Behavior::GuardStuck => "Quietly keeping several unresolved failures company.",
        Behavior::GuardRecovery => "The build recovered; keeping it company before push.",
        Behavior::GuardCautious => "Recovered after repeated changes between failure and success.",
    }
}

/// Ephemeral acknowledgment of an explicit settings interaction.
///
/// Only idle, exploring, approaching, and sleeping bodies are eligible, with
/// no transition in progress. Commands, reactions, unknown outcomes, and repo
/// vigils always keep their meaning. A greeting lasts two seconds and shares
/// an eight-second cooldown through the existing attention arbiter. Repeated
/// or busy requests are discarded, never queued or used to extend a greeting.
/// No interaction changes life state or earns persistent growth.
#[derive(Debug, Clone)]
pub struct GentleInteraction {
    attention: AttentionArbiter,
    active_at: Option<Duration>,
    observed_at: Duration,
}

impl Default for GentleInteraction {
    fn default() -> Self {
        Self {
            attention: AttentionArbiter::new(AttentionPolicy::uniform(Self::HOLD, Self::COOLDOWN)),
            active_at: None,
            observed_at: Duration::ZERO,
        }
    }
}

impl GentleInteraction {
    pub const HOLD: Duration = Duration::from_secs(2);
    pub const COOLDOWN: Duration = Duration::from_secs(8);

    /// Request one acknowledgment using monotonic, session-relative time.
    /// Pass the underlying pose, not the last interaction's displayed pose.
    /// `false` means no new greeting was admitted. Busy poses cancel any old
    /// greeting immediately. A rejected request consumes no future attention.
    #[must_use]
    pub fn request(&mut self, now: Duration, context: RenderContext) -> bool {
        let now = self.observe(now);
        if !Self::eligible(context) {
            self.cancel();
            return false;
        }
        if !self.attention.offer(AttentionCue::Greeting, now) {
            return false;
        }
        self.active_at = Some(now);
        true
    }

    /// Apply the temporary pose without modifying the host's underlying state.
    /// Call on each render with the current authoritative context. Work or a
    /// transition cancels the greeting, so it cannot resume after work ends.
    /// Growth remains visible; movement, body-language and rhythm overrides
    /// are cleared only for the short acknowledgment.
    pub fn apply(&mut self, now: Duration, context: RenderContext) -> RenderContext {
        let now = self.observe(now);
        if !Self::eligible(context) {
            self.cancel();
            return context;
        }
        if self
            .active_at
            .is_some_and(|start| now.saturating_sub(start) < Self::HOLD)
        {
            PreviewPose::Greeting
                .context()
                .with_growth_stage(context.growth_stage)
        } else {
            self.cancel();
            context
        }
    }

    /// Clear visible feedback on close, disable, or preview selection change.
    /// Keep the cooldown, so reopening settings cannot amplify attention.
    pub fn cancel(&mut self) {
        self.active_at = None;
    }

    fn eligible(context: RenderContext) -> bool {
        context.transition.is_none()
            && matches!(
                context.behavior,
                Behavior::Idle | Behavior::Explore | Behavior::Approach | Behavior::Sleep
            )
    }

    fn observe(&mut self, now: Duration) -> Duration {
        self.observed_at = self.observed_at.max(now);
        self.observed_at
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::organism::{
        sprite_frame_with_context, VisualGrowthStage, VisualTransition, WatchRhythm,
    };

    fn seconds(value: u64) -> Duration {
        Duration::from_secs(value)
    }

    #[test]
    fn previews_are_distinct_canonical_poses_with_explanations() {
        let mut behaviors = Vec::new();
        for pose in PreviewPose::ALL {
            let context = pose.context();
            assert!(!behaviors.contains(&context.behavior));
            behaviors.push(context.behavior);
            assert!(!pose.label().is_empty());
            assert!(!pose.explanation().is_empty());
            assert!(!behavior_explanation(context.behavior).is_empty());
            assert_eq!(context.transition, None);
            assert_eq!(context.body_language, BodyLanguage::default());
            assert!(!context.walking);
            assert_eq!(
                sprite_frame_with_context(context, 0),
                context.behavior.sprite()
            );
        }
    }

    #[test]
    fn previews_reuse_renderer_for_any_host_cadence_and_growth() {
        for pose in PreviewPose::ALL {
            for growth in [
                VisualGrowthStage::Juvenile,
                VisualGrowthStage::Adult,
                VisualGrowthStage::Seasoned,
            ] {
                let context = pose.context().with_growth_stage(growth);
                for frame in [0, 1, 5, 8, 100, u64::MAX] {
                    let sprite = sprite_frame_with_context(context, frame);
                    assert!(sprite.is_ascii());
                    assert_eq!(sprite.lines().count(), 3);
                    assert!(sprite.lines().all(|line| line.len() <= 16));
                    assert_eq!(sprite, sprite_frame_with_context(context, frame));
                }
            }
        }
    }

    #[test]
    fn each_ambient_pose_accepts_a_short_greeting() {
        for pose in [
            PreviewPose::Calm,
            PreviewPose::Curious,
            PreviewPose::Sleeping,
            PreviewPose::Greeting,
        ] {
            let mut interaction = GentleInteraction::default();
            let context = pose.context();
            assert_eq!(interaction.apply(Duration::ZERO, context), context);
            assert!(interaction.request(Duration::ZERO, context));
            assert_eq!(
                interaction.apply(seconds(1), context).behavior,
                Behavior::Approach
            );
            assert_eq!(interaction.apply(GentleInteraction::HOLD, context), context);
        }
    }

    #[test]
    fn repeated_requests_do_not_extend_or_queue_feedback() {
        let context = PreviewPose::Calm.context();
        let mut interaction = GentleInteraction::default();
        assert!(interaction.request(Duration::ZERO, context));
        assert!(!interaction.request(seconds(1), context));
        assert_eq!(interaction.apply(seconds(2), context), context);
        assert!(!interaction.request(seconds(7), context));
        assert_eq!(interaction.apply(seconds(8), context), context);
        assert!(interaction.request(seconds(8), context));
        assert_eq!(
            interaction.apply(seconds(9), context).behavior,
            Behavior::Approach
        );
    }

    #[test]
    fn work_poses_reject_and_cancel_without_replaying_later() {
        let idle = PreviewPose::Calm.context();
        for behavior in [
            Behavior::WatchCommand,
            Behavior::WatchAgent,
            Behavior::WatchSettled,
            Behavior::InspectError,
            Behavior::SitNearError,
            Behavior::Celebrate,
            Behavior::CelebrateBig,
            Behavior::RestAfterPush,
            Behavior::UnknownOutcome,
            Behavior::GlanceAside,
            Behavior::GuardFailure,
            Behavior::GuardStuck,
            Behavior::GuardRecovery,
            Behavior::GuardCautious,
        ] {
            let context = RenderContext::new(behavior, BodyLanguage::default(), false);
            assert!(!behavior_explanation(behavior).is_empty());
            let mut interaction = GentleInteraction::default();
            assert!(!interaction.request(Duration::ZERO, context));
            // Rejected busy input does not consume the idle request's budget.
            assert!(interaction.request(Duration::ZERO, idle));
            assert_eq!(interaction.apply(seconds(1), context), context);
            assert_eq!(interaction.apply(seconds(1), idle), idle);
        }
    }

    #[test]
    fn busy_request_also_cancels_an_active_greeting() {
        let mut interaction = GentleInteraction::default();
        let idle = PreviewPose::Calm.context();
        assert!(interaction.request(Duration::ZERO, idle));
        assert!(!interaction.request(seconds(1), PreviewPose::Working.context()));
        assert_eq!(interaction.apply(seconds(1), idle), idle);
    }

    #[test]
    fn semantic_transition_cannot_be_hidden_or_resumed() {
        let idle = PreviewPose::Calm.context();
        let transition = idle.with_transition(Some(VisualTransition::CelebrateToIdle));
        let mut interaction = GentleInteraction::default();
        assert!(!interaction.request(Duration::ZERO, transition));
        assert!(interaction.request(Duration::ZERO, idle));
        assert_eq!(interaction.apply(seconds(1), transition), transition);
        assert_eq!(interaction.apply(seconds(1), idle), idle);
    }

    #[test]
    fn cancel_keeps_cooldown_but_drops_visible_feedback() {
        let idle = PreviewPose::Calm.context();
        let mut interaction = GentleInteraction::default();
        assert!(interaction.request(Duration::ZERO, idle));
        interaction.cancel();
        assert_eq!(interaction.apply(Duration::ZERO, idle), idle);
        assert!(!interaction.request(seconds(1), idle));
        assert!(interaction.request(seconds(8), idle));
    }

    #[test]
    fn backward_time_cannot_resurrect_or_retrigger_a_greeting() {
        let idle = PreviewPose::Calm.context();
        let mut interaction = GentleInteraction::default();
        assert!(interaction.request(seconds(10), idle));
        assert_eq!(interaction.apply(seconds(12), idle), idle);
        assert_eq!(interaction.apply(seconds(10), idle), idle);
        assert!(!interaction.request(seconds(1), idle));
        assert!(interaction.request(seconds(18), idle));
    }

    #[test]
    fn time_at_duration_max_does_not_overflow() {
        let idle = PreviewPose::Calm.context();
        let mut interaction = GentleInteraction::default();
        assert!(interaction.request(Duration::MAX, idle));
        assert_eq!(
            interaction.apply(Duration::MAX, idle).behavior,
            Behavior::Approach
        );
        assert!(!interaction.request(Duration::MAX, idle));
    }

    #[test]
    fn acknowledgment_preserves_growth_and_restores_the_exact_context() {
        let original = RenderContext::new(
            Behavior::Explore,
            BodyLanguage {
                drowsy: true,
                tense: true,
                listless: true,
            },
            true,
        )
        .with_growth_stage(VisualGrowthStage::Seasoned)
        .with_watch_rhythm(WatchRhythm::Busy);
        let mut interaction = GentleInteraction::default();
        assert!(interaction.request(Duration::ZERO, original));
        let display = interaction.apply(seconds(1), original);
        assert_eq!(display.growth_stage, original.growth_stage);
        assert_eq!(display.behavior, Behavior::Approach);
        assert!(!display.walking);
        assert_eq!(display.body_language, BodyLanguage::default());
        assert_eq!(display.watch_rhythm, WatchRhythm::Steady);
        assert_eq!(interaction.apply(seconds(2), original), original);
    }
}
