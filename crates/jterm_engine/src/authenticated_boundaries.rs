//! Correlation of shell boundaries admitted by the PTY host.

/// An outer OSC 133 `C` admitted by the host, bound to the private identity
/// passed to that session's shell integration. Treat the identity as a secret.
#[derive(Default)]
pub struct AuthenticatedBoundaries {
    active_id: Option<Box<str>>,
}

impl AuthenticatedBoundaries {
    /// Replace the previous boundary even when the accepted start was bare.
    /// The caller supplies its private session token, never one parsed from OSC.
    pub fn observe_start(&mut self, id: Option<&str>, private_token: &str) -> bool {
        self.active_id = id
            .filter(|id| private_command_id(id, private_token))
            .map(Into::into);
        self.active_id.is_some()
    }

    /// An admitted outer `D` corroborates this `C` only once, with the exact
    /// same id and a positive foreground snapshot from the PTY host.
    pub fn observe_end(&mut self, id: Option<&str>, shell_owns_foreground: bool) -> bool {
        if shell_owns_foreground && self.active_id.is_some() && self.active_id.as_deref() == id {
            self.clear();
            true
        } else {
            false
        }
    }

    pub fn clear(&mut self) {
        self.active_id = None;
    }
}

/// The id shape used by the bundled shell shims. A textual id without the
/// caller's private token carries no shell proof.
pub fn private_command_id(id: &str, token: &str) -> bool {
    !token.is_empty()
        && id
            .strip_prefix(token)
            .and_then(|suffix| suffix.strip_prefix('-'))
            .is_some_and(|sequence| {
                !sequence.is_empty() && sequence.bytes().all(|byte| byte.is_ascii_digit())
            })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authenticated_completion_requires_order_foreground_and_exact_identity_once() {
        let mut boundaries = AuthenticatedBoundaries::default();
        assert!(!boundaries.observe_end(Some("pane-08"), true));
        assert!(!boundaries.observe_start(Some("other-08"), "pane"));
        assert!(!boundaries.observe_end(Some("other-08"), true));
        assert!(boundaries.observe_start(Some("pane-08"), "pane"));
        assert!(!boundaries.observe_end(Some("pane-08"), false));
        assert!(!boundaries.observe_end(Some("pane-8"), true));
        assert!(!boundaries.observe_end(None, true));
        assert!(boundaries.observe_end(Some("pane-08"), true));
        assert!(!boundaries.observe_end(Some("pane-08"), true));
    }

    #[test]
    fn new_start_or_clear_invalidates_previous_private_boundary() {
        let mut boundaries = AuthenticatedBoundaries::default();
        assert!(boundaries.observe_start(Some("pane-1"), "pane"));
        assert!(!boundaries.observe_start(None, "pane"));
        assert!(!boundaries.observe_end(Some("pane-1"), true));
        assert!(boundaries.observe_start(Some("pane-2"), "pane"));
        boundaries.clear();
        assert!(!boundaries.observe_end(Some("pane-2"), true));
        assert!(!boundaries.observe_start(Some("pane-2"), ""));
        assert!(!boundaries.observe_start(Some("pane-"), "pane"));
        assert!(!boundaries.observe_start(Some("pane-١"), "pane"));
    }
}
