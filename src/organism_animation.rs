//! Small, time-weighted body performances for the existing ASCII creature.
//!
//! These are visual clips, not new behaviors or simulated events. A native
//! host supplies its existing frame clock; sampling is deterministic, bounded,
//! allocation-free and safe after a suspended window skips any number of
//! frames. Semantic transition arcs and protected work poses retain priority.

use super::{Behavior, RenderContext, WatchRhythm};

type Pose = (u16, &'static str);

// Durations are nominal 100 ms frames. Long holds make the small gestures
// noticeable without turning quiet terminal work into a perpetual dance.
const IDLE: &[Pose] = &[
    (32, super::IDLE_FRAMES[0]),
    (4, " /\\_/\\\n( o.o )\n > ^ <"),
    (2, " /\\_/\\\n( -.o )\n > ^ <"),
    (32, super::IDLE_FRAMES[0]),
    (4, super::IDLE_FRAMES[1]),
    (36, super::IDLE_FRAMES[0]),
    (3, " /\\_/\\\n( -.- )\n > / <"),
    (3, " /\\_/\\\n( -.o )\n > / <"),
    (3, " /\\_/\\\n( -.- )\n > _ <"),
    (41, super::IDLE_FRAMES[0]),
];

const GAIT: &[Pose] = &[
    (4, super::GAIT_FRAMES[0]),
    (2, " /\\_/\\\n( o.o )\n > ^ <"),
    (4, super::GAIT_FRAMES[1]),
    (2, " /\\_/\\\n( o.o )\n >~^ <"),
];

const EXPLORE: &[Pose] = &[
    (14, super::EXPLORE_FRAMES[0]),
    (5, " /\\_/\\\n( >.> )\n > ^ <"),
    (2, " /\\_/\\\n( -.- )\n > ^ <"),
    (14, super::EXPLORE_FRAMES[1]),
    (5, " /\\_/\\\n( <.< )\n > ^ <"),
    (8, " /\\_/\\\n( o.o )\n >~^ <"),
];

const GREETING: &[Pose] = &[
    (5, super::APPROACH_FRAMES[0]),
    (3, " /\\_/\\\n( ^.^ )\n > / <"),
    (2, " /\\_/\\\n( ^.- )\n > / <"),
    (3, " /\\_/\\\n( ^.^ )\n > _ <"),
    (7, super::APPROACH_FRAMES[1]),
    (16, super::APPROACH_FRAMES[0]),
];

const CELEBRATION: &[Pose] = &[
    (4, super::CELE_FRAMES[0]),
    (2, " /\\_/\\\n ( ^.^ ) \n  > ^ <"),
    (3, super::CELE_FRAMES[1]),
    (3, " /\\_/\\\n<( ^.^ )>\n  >~^ <"),
    (8, super::CELE_FRAMES[0]),
];

const RECOVERY: &[Pose] = &[
    (4, super::BIG_FRAMES[0]),
    (2, "  /\\_/\\  \n ( ^.^ )\n  > ^ <"),
    (3, "* /\\_/\\  \n<( ^o^ )>\n  > ^ < *"),
    (3, "  /\\_/\\ *\n<( ^.^ )>\n* >~^ <"),
    (4, super::BIG_FRAMES[1]),
    (8, super::BIG_FRAMES[0]),
];

const REST: &[Pose] = &[
    (20, super::REST_FRAMES[0]),
    (3, " /\\_/\\\n( ^.- )  ok\n > ^ <"),
    (12, super::REST_FRAMES[0]),
    (5, super::REST_FRAMES[1]),
];

const SLEEP: &[Pose] = &[
    (14, super::SLEEP_FRAMES[0]),
    (6, " /\\_/\\\n( -_- )zZ\n (_~_) "),
    (14, super::SLEEP_FRAMES[1]),
    (6, " /\\_/\\\n( -_- )Z \n (_~_) "),
];

/// The host has already resolved Full/Calm/Static. Passing frame zero always
/// produces the original canonical pose. Ineligible/protected contexts fall
/// back to their authoritative renderer, including drowsiness and tense ears.
pub(super) fn sprite(context: RenderContext, frame: u64, cadence: u64) -> Option<&'static str> {
    let poses = match context.behavior {
        Behavior::Idle
            if context.body_language.drowsy
                || context.body_language.tense
                || context.body_language.listless =>
        {
            return None;
        }
        Behavior::Idle | Behavior::Explore if context.walking => GAIT,
        Behavior::Idle => IDLE,
        Behavior::Explore => EXPLORE,
        Behavior::Approach => GREETING,
        Behavior::Celebrate => CELEBRATION,
        Behavior::CelebrateBig => RECOVERY,
        Behavior::RestAfterPush => REST,
        Behavior::Sleep => SLEEP,
        // A rare blink preserves the running-command posture, cadence and
        // actor. Busy/waiting/resumed signals must stay distinct and readable.
        Behavior::WatchCommand
            if !context.body_language.tense && context.watch_rhythm == WatchRhythm::Steady =>
        {
            let phase = phase(frame, cadence, 71);
            return match phase {
                52..=53 => Some(" /\\_/\\\n( -.- )\n > ^ <"),
                54 => Some(" /\\_/\\\n( -.o )\n > ^ <"),
                _ => None,
            };
        }
        _ => return None,
    };
    let period = poses.iter().map(|(duration, _)| u64::from(*duration)).sum();
    let mut at = phase(frame, cadence, period);
    for &(duration, sprite) in poses {
        if at < u64::from(duration) {
            return Some(sprite);
        }
        at -= u64::from(duration);
    }
    unreachable!("the phase is bounded by this nonempty clip's duration")
}

fn phase(frame: u64, cadence: u64, period: u64) -> u64 {
    // floor(frame * 5 / cadence) modulo period, without multiplying an
    // unbounded frame. This keeps short juvenile and slower seasoned clips
    // consistent even at u64::MAX and after very long suspended sessions.
    ((frame / cadence % period) * 5 + (frame % cadence) * 5 / cadence) % period
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::organism::{sprite_frame_with_context, BodyLanguage, VisualGrowthStage};

    #[test]
    fn weighted_clips_keep_canonical_static_pose_and_fixed_ascii_envelope() {
        for (behavior, poses) in [
            (Behavior::Idle, IDLE),
            (Behavior::Explore, EXPLORE),
            (Behavior::Approach, GREETING),
            (Behavior::Celebrate, CELEBRATION),
            (Behavior::CelebrateBig, RECOVERY),
            (Behavior::RestAfterPush, REST),
            (Behavior::Sleep, SLEEP),
        ] {
            let shape = |sprite: &str| (sprite.lines().count(), sprite.lines().map(str::len).max());
            let expected = shape(behavior.sprite());
            assert_eq!(poses[0].1, behavior.sprite());
            for &(hold, pose) in poses {
                assert!(hold > 0 && pose.is_ascii());
                assert_eq!(shape(pose), expected, "{behavior:?}: {pose}");
            }
            let context = RenderContext::new(behavior, BodyLanguage::default(), false);
            for stage in [
                VisualGrowthStage::Juvenile,
                VisualGrowthStage::Adult,
                VisualGrowthStage::Seasoned,
            ] {
                let context = context.with_growth_stage(stage);
                for frame in (0..800).chain([u64::MAX - 1, u64::MAX]) {
                    let pose = sprite_frame_with_context(context, frame);
                    assert!(pose.is_ascii());
                    assert_eq!(shape(&pose), expected);
                }
            }
        }
    }

    #[test]
    fn idle_micro_actions_are_varied_but_leave_most_time_quiet() {
        let context = RenderContext::new(Behavior::Idle, BodyLanguage::default(), false);
        let total: u64 = IDLE.iter().map(|(hold, _)| u64::from(*hold)).sum();
        let mut distinct = std::collections::HashSet::new();
        let mut quiet = 0;
        for frame in 0..total {
            let pose = sprite(context, frame, 5).unwrap();
            distinct.insert(pose);
            if pose == super::super::IDLE_FRAMES[0] {
                quiet += 1;
            }
        }
        assert!(distinct.len() >= 5);
        assert!(quiet * 100 / total >= 85);
        assert_eq!(sprite(context, total, 5), sprite(context, 0, 5));
    }

    #[test]
    fn choreography_never_masks_vigils_unknown_outcomes_or_body_language() {
        for behavior in [
            Behavior::InspectError,
            Behavior::SitNearError,
            Behavior::UnknownOutcome,
            Behavior::GuardFailure,
            Behavior::GuardStuck,
            Behavior::GuardRecovery,
            Behavior::GuardCautious,
        ] {
            let context = RenderContext::new(behavior, BodyLanguage::default(), false);
            for frame in 0..400 {
                assert_eq!(sprite(context, frame, 5), None);
            }
        }
        for language in [
            BodyLanguage {
                drowsy: true,
                ..Default::default()
            },
            BodyLanguage {
                tense: true,
                ..Default::default()
            },
            BodyLanguage {
                listless: true,
                ..Default::default()
            },
        ] {
            let context = RenderContext::new(Behavior::Idle, language, true);
            assert_eq!(sprite(context, 54, 5), None);
        }
    }
}
