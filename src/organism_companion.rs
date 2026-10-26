//! One companion, several possible native surfaces.
//!
//! A host owns one life, ambient mind and voice per GUI, outside its panes.
//! Panes supply command facts and geometry, never a second companion. This
//! module decides which *one* surface can embody it and offers small offline
//! conversations and optional care prompts. It performs no I/O, reads no
//! terminal contents and never invokes a command or an agent.

use std::time::Duration;

use crate::organism::{Behavior, RenderContext, WatchRhythm};

/// Mutually exclusive representations of the same companion. Inspectors and
/// tooltips show text about this body; they must not paint another live body.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CompanionSurface {
    #[default]
    Hidden,
    Body,
    Compact,
}

impl CompanionSurface {
    /// Choose once from the current frame's facts, then use the result for
    /// every body, card and glyph in that host. `host_available` includes the
    /// native adapter's focus/locality/interaction/alternate-screen gates.
    /// Motion affects the chosen body's pose, never its right to exist.
    pub const fn choose(
        enabled: bool,
        owns_window: bool,
        host_available: bool,
        input_retreat: bool,
        body_fits: bool,
        compact_fits: bool,
    ) -> Self {
        if !enabled || !owns_window || !host_available || input_retreat {
            Self::Hidden
        } else if body_fits {
            Self::Body
        } else if compact_fits {
            Self::Compact
        } else {
            Self::Hidden
        }
    }
}

/// An authored conversational style of the companion, not a diagnosis of
/// its human. Keep the voice at window scope so tab changes cannot reroll it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Temperament {
    Gentle,
    Curious,
    Playful,
    Steadfast,
}

impl Temperament {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Gentle => "Gentle",
            Self::Curious => "Curious",
            Self::Playful => "Playful",
            Self::Steadfast => "Steadfast",
        }
    }

    pub const fn description(self) -> &'static str {
        match self {
            Self::Gentle => "Soft-spoken, fond of quiet company and small pauses.",
            Self::Curious => "Attentive, inquisitive and fond of little discoveries.",
            Self::Playful => "A small mischief-maker who knows when to be quiet.",
            Self::Steadfast => "Patient company through long commands and difficult work.",
        }
    }
}

/// Explicit user interactions. These request a reply, not work or authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompanionIntent {
    Hello,
    Company,
    Story,
    Rest,
}

/// Bounded, offline dialogue. Responses are authored local text, not an LLM
/// conversation. A stable host seed keeps the style across reinstantiation;
/// the story cursor and latest reply belong to this GUI's one companion.
#[derive(Debug, Clone)]
pub struct CompanionVoice {
    temperament: Temperament,
    story: usize,
    last_reply: Option<&'static str>,
}

impl Default for CompanionVoice {
    fn default() -> Self {
        Self::new(0)
    }
}

impl CompanionVoice {
    pub const fn new(seed: u64) -> Self {
        let temperament = match seed % 4 {
            0 => Temperament::Gentle,
            1 => Temperament::Curious,
            2 => Temperament::Playful,
            _ => Temperament::Steadfast,
        };
        Self {
            temperament,
            story: ((seed / 4) % 3) as usize,
            last_reply: None,
        }
    }

    /// Stable across platforms and process restarts. The host can use its
    /// configured memory location or an explicit companion key; no source
    /// string is retained, published, or used as a security identifier.
    pub fn from_key(key: &str) -> Self {
        Self::new(Self::seed_for_key(key))
    }

    pub fn seed_for_key(key: &str) -> u64 {
        key.as_bytes()
            .iter()
            .fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
                (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
            })
    }

    pub const fn temperament(&self) -> Temperament {
        self.temperament
    }

    pub const fn last_reply(&self) -> Option<&'static str> {
        self.last_reply
    }

    /// Context is read-only: a greeting cannot clear a failure, turn quiet
    /// output into completion, or claim the human is tired or stressed.
    pub fn respond(&mut self, intent: CompanionIntent, context: RenderContext) -> &'static str {
        let reply = match intent {
            CompanionIntent::Hello => self.hello(context),
            CompanionIntent::Company => self.company(context),
            CompanionIntent::Story => {
                let story = STORIES[self.temperament as usize][self.story];
                self.story = (self.story + 1) % 3;
                story
            }
            CompanionIntent::Rest => match context.behavior {
                Behavior::WatchCommand | Behavior::WatchAgent | Behavior::WatchSettled => {
                    "A pause is welcome. Your command can keep running; I haven't paused or changed it."
                }
                _ => match self.temperament {
                    Temperament::Gentle => "I'll curl up here. Take a little pause whenever it suits you.",
                    Temperament::Curious => "Shall we look away from the screen for a moment? The world has details too.",
                    Temperament::Playful => "Stretch break? I'll do the tiny cat version.",
                    Temperament::Steadfast => "Your place is here when you return. A quiet pause sounds good.",
                },
            },
        };
        self.last_reply = Some(reply);
        reply
    }

    fn hello(&self, context: RenderContext) -> &'static str {
        match context.behavior {
            Behavior::WatchCommand | Behavior::WatchAgent | Behavior::WatchSettled => {
                "Hello. I'm here beside the running command."
            }
            Behavior::InspectError
            | Behavior::SitNearError
            | Behavior::GuardFailure
            | Behavior::GuardStuck => {
                "I'm here. That failure is still open; we can take it one step at a time."
            }
            Behavior::UnknownOutcome => {
                "Hello. I'm here; that command's outcome is still unconfirmed."
            }
            _ => match self.temperament {
                Temperament::Gentle => "Hello, you. I'll keep you a little company.",
                Temperament::Curious => "Oh, hello! What shall we discover today?",
                Temperament::Playful => {
                    "You found me. I was absolutely not plotting a tiny adventure."
                }
                Temperament::Steadfast => "Good to see you. I'm here for the next step.",
            },
        }
    }

    fn company(&self, context: RenderContext) -> &'static str {
        match context.behavior {
            Behavior::WatchCommand | Behavior::WatchAgent | Behavior::WatchSettled => match context.watch_rhythm {
                WatchRhythm::Waiting => "It's quiet, but the command is still running. I'll sit with it.",
                WatchRhythm::Resumed => "There's output again. I'm following along.",
                _ => "I'll keep you company while this command runs. We don't have its result yet.",
            },
            Behavior::InspectError | Behavior::SitNearError | Behavior::GuardFailure | Behavior::GuardStuck => {
                "The failure is still open. I'm staying nearby while you decide the next step."
            }
            Behavior::GuardRecovery | Behavior::GuardCautious => {
                "The build recovered. I'll stay beside it while you decide what comes next."
            }
            Behavior::UnknownOutcome => "I'm here. We still don't have a confirmed outcome for that command.",
            _ => match self.temperament {
                Temperament::Gentle => "We can just sit here for a moment. Nothing to earn, nothing to hurry.",
                Temperament::Curious => "I'm watching the little spaces between things. That's where discoveries like to hide.",
                Temperament::Playful => "This corner is now a very small clubhouse. You have a lifetime membership.",
                Temperament::Steadfast => "One step is enough for now. I'll be beside the next one too.",
            },
        }
    }
}

const STORIES: [[&str; 3]; 4] = [
    [
        "A little cat kept a lantern beside an unfinished bridge. Each evening one more plank appeared; the lantern never asked how many remained.",
        "A cloud stopped over a quiet garden. It had nowhere urgent to be, so it practiced being a very good patch of shade.",
        "A snail found a warm stone and called it a destination. The moon agreed that arriving could be a small thing.",
    ],
    [
        "A little cat followed a blinking cursor to the edge of a map. Beyond it was a blank page, which seemed like an excellent discovery.",
        "A sparrow collected questions instead of shiny things. One day it built a nest with a window in every wall.",
        "A pebble wondered where the river went. It stayed still and listened; every day the river brought a different answer.",
    ],
    [
        "A little cat hid behind a semicolon. It was not a convincing hiding place, but the comma politely pretended not to notice.",
        "A moonbeam applied for the job of desk lamp. Its references were excellent, but it insisted on taking the occasional cloudy night off.",
        "A tiny dragon guarded a single biscuit. After careful deliberation, it promoted the biscuit to lunch and retired for the afternoon.",
    ],
    [
        "A little cat sat beside a stubborn door. When the key finally turned, it stood up quietly, as though it had always saved a place on the other side.",
        "A lighthouse never chased the boats. It kept its light steady and trusted each sailor to choose the way home.",
        "A gardener planted one seed beside an old wall. Through the rain and the waiting, a small green answer was taking shape.",
    ],
];

/// A gentle invitation, never a health assessment or an urgency signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CarePrompt {
    Water,
    Stretch,
    LookAway,
}

impl CarePrompt {
    pub const fn text(self) -> &'static str {
        match self {
            Self::Water => "We've had a stretch of time together. Would you like a sip of water?",
            Self::Stretch => "A small pause? Maybe stand up or stretch if that feels comfortable.",
            Self::LookAway => {
                "Shall we rest our eyes on something away from the screen for a moment?"
            }
        }
    }
}

/// Optional window-wide care clock. It counts only sampled, foreground,
/// recently-active time, not time in background panes or suspended machines.
/// Hosts call `observe` from their existing heartbeat, and only set
/// `can_offer` when a quiet text surface can actually display a prompt.
/// Prompts are not queued; changing panes cannot multiply or reset the clock.
#[derive(Debug, Default)]
pub struct CompanionCare {
    enabled: bool,
    observed_at: Option<Duration>,
    was_engaged: bool,
    inactive_since: Option<Duration>,
    work: Duration,
    snoozed_until: Duration,
    next: u8,
}

impl CompanionCare {
    pub const WORK_INTERVAL: Duration = Duration::from_secs(45 * 60);
    pub const REST_RESET: Duration = Duration::from_secs(5 * 60);
    pub const SNOOZE: Duration = Duration::from_secs(20 * 60);
    const MAX_SAMPLE: Duration = Duration::from_secs(5);

    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        if self.enabled != enabled {
            self.enabled = enabled;
            self.observed_at = None;
            self.was_engaged = false;
            self.inactive_since = None;
            self.work = Duration::ZERO;
            self.snoozed_until = Duration::ZERO;
        }
    }

    pub fn snooze(&mut self, now: Duration) {
        let now = now.max(self.observed_at.unwrap_or_default());
        self.snoozed_until = now.saturating_add(Self::SNOOZE);
    }

    /// Acknowledging a real pause resets accumulated time without pretending
    /// that the user drank water, moved, or changed any health measurement.
    pub fn rested(&mut self, now: Duration) {
        let now = now.max(self.observed_at.unwrap_or_default());
        self.work = Duration::ZERO;
        self.observed_at = Some(now);
        self.snoozed_until = now.saturating_add(Self::SNOOZE);
    }

    /// Return at most one offer, at most once per foreground work interval.
    /// `engaged` requires a focused, local interactive host and recent human
    /// input. `can_offer` additionally excludes typing, protected work,
    /// selection, quiet mode, unavailable geometry and modal UI.
    pub fn observe(&mut self, now: Duration, engaged: bool, can_offer: bool) -> Option<CarePrompt> {
        if !self.enabled {
            return None;
        }
        let now = now.max(self.observed_at.unwrap_or_default());
        let elapsed = self
            .observed_at
            .map(|previous| now.saturating_sub(previous));
        // A long scheduling gap is a break, even if the window reports the
        // same focus state after resume. Never replay 45 minutes of debt.
        if elapsed.is_some_and(|gap| gap >= Self::REST_RESET) {
            self.work = Duration::ZERO;
            self.was_engaged = false;
        }
        if engaged {
            if self
                .inactive_since
                .take()
                .is_some_and(|since| now.saturating_sub(since) >= Self::REST_RESET)
            {
                self.work = Duration::ZERO;
            } else if self.was_engaged {
                self.work = self
                    .work
                    .saturating_add(elapsed.unwrap_or_default().min(Self::MAX_SAMPLE));
            }
        } else {
            let since = *self.inactive_since.get_or_insert(now);
            if now.saturating_sub(since) >= Self::REST_RESET {
                self.work = Duration::ZERO;
            }
        }
        self.observed_at = Some(now);
        self.was_engaged = engaged;
        if !engaged || !can_offer || now < self.snoozed_until || self.work < Self::WORK_INTERVAL {
            return None;
        }
        self.work = Duration::ZERO;
        let prompt = match self.next {
            0 => CarePrompt::Water,
            1 => CarePrompt::Stretch,
            _ => CarePrompt::LookAway,
        };
        self.next = (self.next + 1) % 3;
        Some(prompt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::organism::BodyLanguage;

    fn context(behavior: Behavior) -> RenderContext {
        RenderContext::new(behavior, BodyLanguage::default(), false)
    }

    #[test]
    fn every_surface_gate_has_one_representation_or_none() {
        for bits in 0u8..64 {
            let flag = |n: u8| bits & (1u8 << n) != 0u8;
            let surface =
                CompanionSurface::choose(flag(0), flag(1), flag(2), flag(3), flag(4), flag(5));
            if !flag(0) || !flag(1) || !flag(2) || flag(3) {
                assert_eq!(surface, CompanionSurface::Hidden);
            } else if flag(4) {
                assert_eq!(surface, CompanionSurface::Body);
            } else if flag(5) {
                assert_eq!(surface, CompanionSurface::Compact);
            } else {
                assert_eq!(surface, CompanionSurface::Hidden);
            }
        }
    }

    #[test]
    fn moving_between_splits_never_admits_two_bodies() {
        for owner in [Some(0), Some(3), None, Some(1), Some(0)] {
            let visible = (0..8)
                .filter(|pane| {
                    CompanionSurface::choose(
                        true,
                        owner == Some(*pane),
                        true,
                        false,
                        pane % 2 == 0,
                        true,
                    ) != CompanionSurface::Hidden
                })
                .count();
            assert_eq!(visible, usize::from(owner.is_some()));
        }
    }

    #[test]
    fn stable_key_keeps_voice_but_does_not_retain_the_key() {
        let key = "/private/example/organism.json";
        let a = CompanionVoice::from_key(key);
        let b = CompanionVoice::from_key(key);
        assert_eq!(a.temperament(), b.temperament());
        assert!(!format!("{a:?}").contains(key));
        assert_eq!(CompanionVoice::seed_for_key("hello"), 0xa430_d846_80aa_bd0b);
    }

    #[test]
    fn all_temperaments_have_distinct_greetings_and_rotating_stories() {
        let mut greetings = std::collections::BTreeSet::new();
        for seed in 0..4 {
            let mut voice = CompanionVoice::new(seed);
            greetings.insert(voice.respond(CompanionIntent::Hello, context(Behavior::Idle)));
            let first = voice.respond(CompanionIntent::Story, context(Behavior::Idle));
            let second = voice.respond(CompanionIntent::Story, context(Behavior::Idle));
            let third = voice.respond(CompanionIntent::Story, context(Behavior::Idle));
            assert_ne!(first, second);
            assert_ne!(first, third);
            assert_ne!(second, third);
            assert_eq!(
                voice.respond(CompanionIntent::Story, context(Behavior::Idle)),
                first
            );
            assert_eq!(voice.last_reply(), Some(first));
        }
        assert_eq!(greetings.len(), 4);
    }

    #[test]
    fn conversation_cannot_reinterpret_protected_work() {
        let mut voice = CompanionVoice::new(2);
        for behavior in [
            Behavior::WatchCommand,
            Behavior::WatchAgent,
            Behavior::WatchSettled,
        ] {
            let mut waiting = context(behavior);
            waiting.watch_rhythm = WatchRhythm::Waiting;
            assert!(voice
                .respond(CompanionIntent::Company, waiting)
                .contains("still running"));
            assert!(voice
                .respond(CompanionIntent::Rest, waiting)
                .contains("haven't paused"));
        }
        assert!(voice
            .respond(CompanionIntent::Hello, context(Behavior::GuardFailure))
            .contains("still open"));
        assert!(voice
            .respond(CompanionIntent::Company, context(Behavior::UnknownOutcome))
            .contains("don't have a confirmed outcome"));
    }

    fn accrue(
        care: &mut CompanionCare,
        from: u64,
        seconds: u64,
        available: bool,
    ) -> Vec<CarePrompt> {
        (from..=from + seconds)
            .filter_map(|second| care.observe(Duration::from_secs(second), true, available))
            .collect()
    }

    #[test]
    fn care_is_opt_in_and_off_switch_forgets_debt() {
        let mut care = CompanionCare::default();
        assert!(accrue(&mut care, 0, 3000, true).is_empty());
        care.set_enabled(true);
        assert_eq!(accrue(&mut care, 3001, 2700, true), [CarePrompt::Water]);
        care.set_enabled(false);
        care.set_enabled(true);
        assert!(care
            .observe(Duration::from_secs(6000), true, true)
            .is_none());
    }

    #[test]
    fn protected_work_defers_one_prompt_without_a_queue() {
        let mut care = CompanionCare::default();
        care.set_enabled(true);
        assert!(accrue(&mut care, 0, 9000, false).is_empty());
        assert_eq!(
            care.observe(Duration::from_secs(9001), true, true),
            Some(CarePrompt::Water)
        );
        assert!(care
            .observe(Duration::from_secs(9001), true, true)
            .is_none());
        assert!(accrue(&mut care, 9002, 100, true).is_empty());
    }

    #[test]
    fn background_and_suspend_do_not_turn_into_work() {
        let mut care = CompanionCare::default();
        care.set_enabled(true);
        assert!(accrue(&mut care, 0, 2600, true).is_empty());
        assert!(care
            .observe(Duration::from_secs(10_000), true, true)
            .is_none());
        assert!(accrue(&mut care, 10_001, 2698, true).is_empty());
        assert_eq!(
            care.observe(Duration::from_secs(12_700), true, true),
            Some(CarePrompt::Water)
        );
        for second in 12_701..20_000 {
            assert!(care
                .observe(Duration::from_secs(second), false, true)
                .is_none());
        }
        assert!(care
            .observe(Duration::from_secs(20_000), true, true)
            .is_none());
    }

    #[test]
    fn real_break_resets_work_even_with_frequent_background_ticks() {
        let mut care = CompanionCare::default();
        care.set_enabled(true);
        assert!(accrue(&mut care, 0, 2600, true).is_empty());
        for second in 2601..=2901 {
            assert!(care
                .observe(Duration::from_secs(second), false, true)
                .is_none());
        }
        assert!(accrue(&mut care, 2902, 2699, true).is_empty());
        assert_eq!(
            care.observe(Duration::from_secs(5602), true, true),
            Some(CarePrompt::Water)
        );
    }

    #[test]
    fn snooze_and_clock_regressions_do_not_duplicate_offers() {
        let mut care = CompanionCare::default();
        care.set_enabled(true);
        assert!(accrue(&mut care, 0, 2700, false).is_empty());
        care.snooze(Duration::from_secs(2700));
        assert!(accrue(&mut care, 2701, 1198, true).is_empty());
        assert_eq!(
            care.observe(Duration::from_secs(3900), true, true),
            Some(CarePrompt::Water)
        );
        assert!(care
            .observe(Duration::from_secs(2000), true, true)
            .is_none());
        assert!(care
            .observe(Duration::from_secs(3900), true, true)
            .is_none());
        care.rested(Duration::from_secs(3900));
        assert!(accrue(&mut care, 3901, 2698, true).is_empty());
        assert_eq!(
            care.observe(Duration::from_secs(6600), true, true),
            Some(CarePrompt::Stretch)
        );
    }

    #[test]
    fn delayed_frames_are_capped_and_extreme_clocks_do_not_panic() {
        let mut care = CompanionCare::default();
        care.set_enabled(true);
        assert!(care.observe(Duration::ZERO, true, true).is_none());
        for minute in 1..=45 {
            assert!(care
                .observe(Duration::from_secs(minute * 60), true, true)
                .is_none());
        }
        assert_eq!(care.work, Duration::from_secs(225));
        care.snooze(Duration::MAX);
        care.rested(Duration::MAX);
        assert!(care.observe(Duration::MAX, true, true).is_none());
    }
}
