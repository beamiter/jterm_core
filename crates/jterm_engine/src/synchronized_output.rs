//! Profiled synchronized-output state. Native archive and dirty effects are
//! synchronous steps; hosted publication keeps its independent 250ms policy.
use crate::protocol_state::ProtocolState;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncProfile {
    Ember,
    FrostLegacy,
    Hosted,
}
#[derive(Debug)]
pub struct SynchronizedOutput {
    profile: SyncProfile,
    // Transitional frozen-oracle/view inspection; production mutations use
    // the owner methods and ordered end command below.
    pub active: bool,
    pub started: Option<Instant>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncEndEffect {
    Archive,
    Dirty,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EndCause {
    Explicit,
    Timeout,
}
pub struct NativeSyncEnd {
    profile: SyncProfile,
    cause: EndCause,
    phase: u8,
}
impl SynchronizedOutput {
    pub fn new(profile: SyncProfile) -> Self {
        Self {
            profile,
            active: false,
            started: None,
        }
    }
    pub fn begin_native(&mut self, protocol: &mut ProtocolState, now: impl FnOnce() -> Instant) {
        assert_ne!(self.profile, SyncProfile::Hosted);
        protocol.modes_mut().insert(2026);
        self.active = true;
        match self.profile {
            SyncProfile::Ember => self.started = Some(now()),
            SyncProfile::FrostLegacy => {
                self.started.get_or_insert_with(now);
            }
            SyncProfile::Hosted => unreachable!(),
        }
    }
    pub fn explicit_end(&self) -> NativeSyncEnd {
        assert_ne!(self.profile, SyncProfile::Hosted);
        NativeSyncEnd {
            profile: self.profile,
            cause: EndCause::Explicit,
            phase: 0,
        }
    }
    pub fn timeout_with(&self, now: impl FnOnce() -> Instant) -> Option<NativeSyncEnd> {
        if self.started.is_none() || (self.profile == SyncProfile::Ember && !self.active) {
            return None;
        }
        self.timeout(now())
    }
    pub fn timeout(&self, now: Instant) -> Option<NativeSyncEnd> {
        let started = self.started?;
        let age = now.saturating_duration_since(started);
        let due = match self.profile {
            SyncProfile::Ember => self.active && age > Duration::from_secs(1),
            SyncProfile::FrostLegacy => age >= Duration::from_secs(1),
            SyncProfile::Hosted => false,
        };
        due.then_some(NativeSyncEnd {
            profile: self.profile,
            cause: EndCause::Timeout,
            phase: 0,
        })
    }
    pub fn begin_hosted(&mut self, now: Instant) {
        assert_eq!(self.profile, SyncProfile::Hosted);
        self.active = true;
        self.started.get_or_insert(now);
    }
    pub fn end_hosted(&mut self) {
        assert_eq!(self.profile, SyncProfile::Hosted);
        self.active = false;
        self.started = None;
    }
    pub fn publication_held_at(&self, now: Instant) -> bool {
        assert_eq!(self.profile, SyncProfile::Hosted);
        self.started
            .is_some_and(|at| now.saturating_duration_since(at) < Duration::from_millis(250))
    }
    pub fn reflect_query(&self, protocol: &mut ProtocolState, now: Instant) {
        if self.publication_held_at(now) {
            protocol.modes_mut().insert(2026);
        }
    }
    pub fn finish_query(&self, protocol: &mut ProtocolState) {
        protocol.modes_mut().remove(&2026);
    }
}
impl NativeSyncEnd {
    pub fn step(
        &mut self,
        state: &mut SynchronizedOutput,
        protocol: &mut ProtocolState,
    ) -> Option<SyncEndEffect> {
        assert_eq!(self.profile, state.profile);
        match self.phase {
            0 => {
                self.phase = 1;
                Some(SyncEndEffect::Archive)
            }
            1 => {
                self.phase = 2;
                if self.profile == SyncProfile::Ember && self.cause == EndCause::Timeout {
                    state.active = false;
                    state.started = None;
                    protocol.modes_mut().remove(&2026);
                } else {
                    protocol.modes_mut().remove(&2026);
                    state.active = false;
                    state.started = None;
                }
                Some(SyncEndEffect::Dirty)
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_begin_profiles_keep_restart_and_lazy_first_begin() {
        let at = Instant::now();
        let mut p = ProtocolState::default();
        let mut e = SynchronizedOutput::new(SyncProfile::Ember);
        e.begin_native(&mut p, || at);
        e.begin_native(&mut p, || at + Duration::from_millis(900));
        assert_eq!(e.started, Some(at + Duration::from_millis(900)));
        let mut f = SynchronizedOutput::new(SyncProfile::FrostLegacy);
        f.begin_native(&mut p, || at);
        f.begin_native(&mut p, || {
            panic!("Frost repeated begin must not sample clock")
        });
        assert_eq!(f.started, Some(at));
        assert!(p.modes().contains(&2026));
    }
    #[test]
    fn native_timeout_boundaries_and_archive_before_clear_are_frozen() {
        let at = Instant::now();
        for profile in [SyncProfile::Ember, SyncProfile::FrostLegacy] {
            let mut p = ProtocolState::default();
            let mut s = SynchronizedOutput::new(profile);
            s.begin_native(&mut p, || at);
            assert!(s.timeout(at + Duration::from_millis(999)).is_none());
            assert_eq!(
                s.timeout(at + Duration::from_secs(1)).is_some(),
                profile == SyncProfile::FrostLegacy
            );
            let mut end = s
                .timeout(at + Duration::from_secs(1) + Duration::from_nanos(1))
                .unwrap();
            assert_eq!(end.step(&mut s, &mut p), Some(SyncEndEffect::Archive));
            assert!(s.active);
            assert_eq!(s.started, Some(at));
            assert!(p.modes().contains(&2026));
            assert_eq!(end.step(&mut s, &mut p), Some(SyncEndEffect::Dirty));
            assert!(!s.active);
            assert!(s.started.is_none());
            assert!(!p.modes().contains(&2026));
            assert!(end.step(&mut s, &mut p).is_none());
        }
    }
    #[test]
    fn empty_and_inconsistent_native_states_keep_original_admission() {
        let at = Instant::now();
        let mut e = SynchronizedOutput::new(SyncProfile::Ember);
        assert!(e
            .timeout_with(|| panic!("inactive clock must stay lazy"))
            .is_none());
        e.active = true;
        assert!(e
            .timeout_with(|| panic!("missing start must stay lazy"))
            .is_none());
        e.started = Some(at);
        e.active = false;
        assert!(e.timeout(at + Duration::from_secs(2)).is_none());
        let mut f = SynchronizedOutput::new(SyncProfile::FrostLegacy);
        f.started = Some(at);
        assert!(f.timeout(at + Duration::from_secs(1)).is_some());
        let mut p = ProtocolState::default();
        let mut end = f.explicit_end();
        assert_eq!(end.step(&mut f, &mut p), Some(SyncEndEffect::Archive));
        assert_eq!(end.step(&mut f, &mut p), Some(SyncEndEffect::Dirty));
    }
    #[test]
    fn hosted_barrier_and_query_mirror_do_not_extend_or_consume_capture_state() {
        let at = Instant::now();
        let mut s = SynchronizedOutput::new(SyncProfile::Hosted);
        let mut p = ProtocolState::default();
        s.begin_hosted(at);
        s.begin_hosted(at + Duration::from_millis(249));
        assert_eq!(s.started, Some(at));
        assert!(s.publication_held_at(at + Duration::from_millis(249)));
        assert!(!s.publication_held_at(at + Duration::from_millis(250)));
        assert!(s.started.is_some());
        s.reflect_query(&mut p, at + Duration::from_millis(249));
        assert!(p.modes().contains(&2026));
        s.finish_query(&mut p);
        assert!(!p.modes().contains(&2026));
        assert!(s.started.is_some());
        s.reflect_query(&mut p, at + Duration::from_millis(250));
        assert!(!p.modes().contains(&2026));
        s.end_hosted();
        assert!(s.started.is_none());
        assert!(!s.publication_held_at(at));
    }
}
