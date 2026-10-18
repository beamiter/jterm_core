//! UI-independent presentation policy for all native companions.
//!
//! Hosts own system observation, visibility and wake scheduling. Unknown
//! system information is distinct from a successfully observed no-preference.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Motion {
    Full,
    Calm,
    Static,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SystemMotion {
    #[default]
    Unavailable,
    NoPreference,
    Reduce,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionReason {
    Explicit,
    System,
    Fallback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolution {
    pub effective: Motion,
    pub reason: ResolutionReason,
}

/// `None` is Automatic. This function never changes the configured choice.
pub const fn resolve(configured: Option<Motion>, observed: SystemMotion) -> Resolution {
    if let Some(effective) = configured {
        return Resolution {
            effective,
            reason: ResolutionReason::Explicit,
        };
    }
    match observed {
        SystemMotion::Unavailable => Resolution {
            effective: Motion::Calm,
            reason: ResolutionReason::Fallback,
        },
        SystemMotion::NoPreference => Resolution {
            effective: Motion::Full,
            reason: ResolutionReason::System,
        },
        SystemMotion::Reduce => Resolution {
            effective: Motion::Calm,
            reason: ResolutionReason::System,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_twelve_cell_golden_matrix() {
        let observed = [
            SystemMotion::Unavailable,
            SystemMotion::NoPreference,
            SystemMotion::Reduce,
        ];
        for explicit in [Motion::Full, Motion::Calm, Motion::Static] {
            for system in observed {
                assert_eq!(
                    resolve(Some(explicit), system),
                    Resolution {
                        effective: explicit,
                        reason: ResolutionReason::Explicit,
                    }
                );
            }
        }
        for (system, effective, reason) in [
            (
                SystemMotion::Unavailable,
                Motion::Calm,
                ResolutionReason::Fallback,
            ),
            (
                SystemMotion::NoPreference,
                Motion::Full,
                ResolutionReason::System,
            ),
            (SystemMotion::Reduce, Motion::Calm, ResolutionReason::System),
        ] {
            assert_eq!(resolve(None, system), Resolution { effective, reason });
        }
    }

    #[test]
    fn system_changes_never_modify_explicit_configuration() {
        for configured in [Some(Motion::Full), Some(Motion::Calm), Some(Motion::Static)] {
            let before = configured;
            for observed in [
                SystemMotion::NoPreference,
                SystemMotion::Reduce,
                SystemMotion::Unavailable,
            ] {
                assert_eq!(resolve(configured, observed).effective, configured.unwrap());
                assert_eq!(configured, before);
            }
        }
    }

    #[test]
    fn observation_reason_can_change_without_effective_motion_change() {
        let unavailable = resolve(None, SystemMotion::Unavailable);
        let reduced = resolve(None, SystemMotion::Reduce);
        assert_eq!(unavailable.effective, reduced.effective);
        assert_ne!(unavailable.reason, reduced.reason);
    }

    #[test]
    fn unavailable_after_full_fails_back_without_stale_full() {
        assert_eq!(
            resolve(None, SystemMotion::NoPreference).effective,
            Motion::Full
        );
        assert_eq!(
            resolve(None, SystemMotion::Unavailable).effective,
            Motion::Calm
        );
    }
}
