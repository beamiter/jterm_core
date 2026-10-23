//! Bounded, selection-bound launch checks without filesystem work on the UI.
use super::{AgentSession, SessionLaunch, SessionProvider};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;

#[derive(Clone, Debug)]
pub enum SessionPreflightState {
    Idle,
    Checking,
    Ready(SessionLaunch),
    Unavailable(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Target {
    provider: SessionProvider,
    id: String,
    cwd: PathBuf,
    source: PathBuf,
    archived: bool,
}
impl From<&AgentSession> for Target {
    fn from(session: &AgentSession) -> Self {
        Self {
            provider: session.provider,
            id: session.id.clone(),
            cwd: session.cwd.clone(),
            source: session.source.clone(),
            archived: session.archived,
        }
    }
}

type Check = Arc<dyn Fn(AgentSession) -> Result<SessionLaunch, String> + Send + Sync>;
struct Completion {
    target: Target,
    generation: u64,
    result: Result<SessionLaunch, String>,
}
struct Pending {
    target: Target,
    generation: u64,
    receiver: Receiver<Completion>,
}

/// At most one worker and one replaceable latest request per picker. Changes
/// to titles, activity, and pin metadata do not invalidate the selected target.
/// A changed identity, source, cwd, or archive flag does. `refresh` invalidates
/// a previous result even for the same target, so confirmation can require a
/// fresh check. A check never executes a CLI, shell profile, or model request.
pub struct SessionPreflight {
    target: Option<Target>,
    selected: Option<AgentSession>,
    generation: u64,
    state: SessionPreflightState,
    pending: Option<Pending>,
    latest: Option<(u64, AgentSession)>,
    check: Check,
}
impl Default for SessionPreflight {
    fn default() -> Self {
        Self::new()
    }
}
impl SessionPreflight {
    pub fn new() -> Self {
        Self::with_check(Arc::new(|session| {
            session.resume_launch()?.resolve_for_spawn()
        }))
    }

    fn with_check(check: Check) -> Self {
        Self {
            target: None,
            selected: None,
            generation: 0,
            state: SessionPreflightState::Idle,
            pending: None,
            latest: None,
            check,
        }
    }

    /// Returns true only when the checked target changes. Hosts should cancel
    /// any pending confirmation in that case before accepting another result.
    pub fn select(&mut self, session: Option<&AgentSession>) -> bool {
        let target = session.map(Target::from);
        if self.target == target {
            return false;
        }
        self.target = target;
        self.selected = session.cloned();
        self.generation = self.generation.wrapping_add(1);
        self.latest = None;
        match self.selected.clone() {
            Some(session) => self.enqueue(session),
            None => self.state = SessionPreflightState::Idle,
        }
        true
    }

    /// Start a fresh check for the current selection, coalescing a queued one.
    pub fn refresh(&mut self) -> bool {
        let Some(session) = self.selected.clone() else {
            return false;
        };
        self.generation = self.generation.wrapping_add(1);
        self.enqueue(session);
        true
    }

    pub fn state(&self) -> &SessionPreflightState {
        &self.state
    }

    pub fn is_busy(&self) -> bool {
        self.pending.is_some() || self.latest.is_some()
    }

    pub fn ready_launch(&self, session: &AgentSession) -> Option<&SessionLaunch> {
        if self.target.as_ref() != Some(&Target::from(session)) {
            return None;
        }
        match &self.state {
            SessionPreflightState::Ready(launch) => Some(launch),
            _ => None,
        }
    }

    fn enqueue(&mut self, session: AgentSession) {
        self.state = SessionPreflightState::Checking;
        if self.pending.is_some() {
            self.latest = Some((self.generation, session));
        } else {
            self.start(self.generation, session);
        }
    }

    fn start(&mut self, generation: u64, session: AgentSession) {
        let target = Target::from(&session);
        let thread_target = target.clone();
        let check = self.check.clone();
        let (sender, receiver) = mpsc::sync_channel(1);
        match std::thread::Builder::new()
            .name("agent-session-preflight".into())
            .spawn(move || {
                let result =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| check(session)))
                        .unwrap_or_else(|_| {
                            Err("Agent installation check failed; retry the check.".into())
                        });
                let _ = sender.send(Completion {
                    target: thread_target,
                    generation,
                    result,
                });
            }) {
            Ok(_) => {
                self.pending = Some(Pending {
                    target,
                    generation,
                    receiver,
                })
            }
            Err(error) => {
                self.state = SessionPreflightState::Unavailable(format!(
                    "Could not start the agent installation check: {error}"
                ));
            }
        }
    }

    /// Drain one completion without waiting. A stale completion cannot make
    /// another target ready, including two checks of the same target.
    pub fn poll(&mut self) -> bool {
        let result = match self.pending.as_ref() {
            Some(pending) => pending.receiver.try_recv(),
            None => return false,
        };
        let mut changed = false;
        match result {
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => {
                let pending = self.pending.take().expect("pending was just inspected");
                if self.target.as_ref() == Some(&pending.target)
                    && self.generation == pending.generation
                {
                    self.state = SessionPreflightState::Unavailable(
                        "The agent installation check stopped; retry it.".into(),
                    );
                    changed = true;
                }
            }
            Ok(completion) => {
                self.pending = None;
                if self.target.as_ref() == Some(&completion.target)
                    && self.generation == completion.generation
                {
                    self.state = match completion.result {
                        Ok(launch) => SessionPreflightState::Ready(launch),
                        Err(error) => SessionPreflightState::Unavailable(error),
                    };
                    changed = true;
                }
            }
        }
        if let Some((generation, session)) = self.latest.take() {
            self.start(generation, session);
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_sessions::SessionStatus;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Condvar, Mutex};
    use std::time::{Duration, Instant};

    fn session(index: usize) -> AgentSession {
        AgentSession {
            provider: SessionProvider::Codex,
            id: format!("{index:08x}-1111-4111-8111-111111111111"),
            title: "Fixture".into(),
            cwd: "/tmp/project".into(),
            source: "/tmp/session.jsonl".into(),
            updated_at_ms: 0,
            status: SessionStatus::Idle,
            preview: String::new(),
            branch: None,
            archived: false,
        }
    }
    fn launch(session: AgentSession) -> SessionLaunch {
        SessionLaunch {
            program: "codex".into(),
            args: vec!["resume".into(), session.id],
            cwd: session.cwd,
        }
    }
    fn until(preflight: &mut SessionPreflight, ready: impl Fn(&SessionPreflight) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            preflight.poll();
            if ready(preflight) {
                return;
            }
            assert!(Instant::now() < deadline, "check did not finish");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn rapid_navigation_coalesces_and_stale_completion_never_authorizes_next_target() {
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let thread_gate = gate.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let thread_calls = calls.clone();
        let (started, receiver) = mpsc::channel();
        let mut checks = SessionPreflight::with_check(Arc::new(move |session| {
            thread_calls.fetch_add(1, Ordering::SeqCst);
            started.send(session.id.clone()).unwrap();
            let (lock, wake) = &*thread_gate;
            let mut open = lock.lock().unwrap();
            while !*open {
                open = wake.wait(open).unwrap();
            }
            Ok(launch(session))
        }));
        let first = session(1);
        checks.select(Some(&first));
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(3)).unwrap(),
            first.id
        );
        for index in 2..=100 {
            checks.select(Some(&session(index)));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let last = session(100);
        assert!(checks.ready_launch(&first).is_none());
        assert!(checks.ready_launch(&last).is_none());
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        until(&mut checks, |checks| checks.ready_launch(&last).is_some());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(checks.ready_launch(&first).is_none());
        assert_eq!(checks.ready_launch(&last).unwrap().args[1], last.id);
    }

    #[test]
    fn confirmation_refresh_invalidates_the_old_ready_result() {
        let calls = Arc::new(AtomicUsize::new(0));
        let thread_calls = calls.clone();
        let mut checks = SessionPreflight::with_check(Arc::new(move |session| {
            thread_calls.fetch_add(1, Ordering::SeqCst);
            Ok(launch(session))
        }));
        let selected = session(1);
        assert!(checks.select(Some(&selected)));
        until(&mut checks, |checks| {
            checks.ready_launch(&selected).is_some()
        });
        assert!(!checks.select(Some(&selected)));
        assert!(checks.refresh());
        assert!(checks.ready_launch(&selected).is_none());
        until(&mut checks, |checks| {
            checks.ready_launch(&selected).is_some()
        });
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        checks.select(None);
        checks.poll();
        assert!(matches!(checks.state(), SessionPreflightState::Idle));
        assert!(checks.ready_launch(&selected).is_none());
    }

    #[test]
    fn readiness_is_bound_to_paths_and_archive_state_but_not_preview_text() {
        let mut checks = SessionPreflight::with_check(Arc::new(|session| Ok(launch(session))));
        let mut selected = session(1);
        checks.select(Some(&selected));
        until(&mut checks, |checks| {
            checks.ready_launch(&selected).is_some()
        });
        selected.preview = "new output".into();
        selected.updated_at_ms = 123;
        assert!(!checks.select(Some(&selected)));
        assert!(checks.ready_launch(&selected).is_some());
        selected.cwd = "/tmp/other-project".into();
        assert!(checks.ready_launch(&selected).is_none());
        assert!(checks.select(Some(&selected)));
        until(&mut checks, |checks| {
            checks.ready_launch(&selected).is_some()
        });
        selected.archived = true;
        assert!(checks.ready_launch(&selected).is_none());
    }

    #[test]
    fn unavailable_check_can_be_retried_without_starting_a_cli() {
        let calls = Arc::new(AtomicUsize::new(0));
        let thread_calls = calls.clone();
        let mut checks = SessionPreflight::with_check(Arc::new(move |session| {
            if thread_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Err("Project directory is unavailable".into())
            } else {
                Ok(launch(session))
            }
        }));
        let selected = session(1);
        checks.select(Some(&selected));
        until(&mut checks, |checks| {
            matches!(checks.state(), SessionPreflightState::Unavailable(_))
        });
        assert!(checks.ready_launch(&selected).is_none());
        checks.refresh();
        until(&mut checks, |checks| {
            checks.ready_launch(&selected).is_some()
        });
    }
}
