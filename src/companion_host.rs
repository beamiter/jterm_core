//! One GUI-owned Eva adapter. Transport work never runs on the rendering thread.
//! A native presentation acknowledgement is not a compositor scanout guarantee.
pub use companion_core::geometry::{choose, Placement};
pub use companion_protocol::*;
pub use eva_character_atlas as atlas;
use std::{
    sync::{mpsc, Arc},
    time::Duration,
};

/// Explicit opt-in. A failed shared connection must never revive the legacy pet.
/// Fallback task identity when no globally unique execution ID is available.
/// A restored pane UUID plus a reset terminal counter alone is NOT a task ID.
pub fn host_scoped_task_id(host_id: &str, pane_id: &str, sequence: u64) -> String {
    format!("{host_id}-{pane_id}-{sequence}")
}

pub fn shared_mode_requested() -> bool {
    std::env::var("EVA_COMPANION_MODE").as_deref() == Ok("shared")
}

#[derive(Debug)]
pub struct HostGate {
    host: Host,
    boot: Option<String>,
    token: Option<Token>,
    retired: Option<(Token, u64)>,
    retirement_submitted: bool,
    retirement_accepted: Option<Token>,
    withdrawal_accepted: bool,
    drawn: Option<(Token, u64)>,
    presented: Option<Token>,
    frame: u64,
    revision: u64,
    connection_generation: u64,
    pose: String,
    skin: String,
    motion: String,
    status: String,
    quiet: bool,
    night: bool,
    operation_error: Option<String>,
}
impl HostGate {
    pub fn new(host: Host) -> Self {
        Self {
            host,
            boot: None,
            token: None,
            retired: None,
            retirement_submitted: false,
            retirement_accepted: None,
            withdrawal_accepted: false,
            drawn: None,
            presented: None,
            frame: 0,
            revision: 0,
            connection_generation: 0,
            pose: "idle".into(),
            skin: "cat".into(),
            motion: "calm".into(),
            status: "Eva disconnected — shared body hidden".into(),
            quiet: false,
            night: false,
            operation_error: None,
        }
    }
    pub fn withdrawal_accepted(&self) -> bool {
        self.withdrawal_accepted
    }
    pub fn retirement_accepted(&self, token: &Token) -> bool {
        self.retirement_accepted.as_ref() == Some(token)
    }
    pub fn possible_body_token(&self) -> Option<&Token> {
        self.token.as_ref().or_else(|| {
            self.retired
                .as_ref()
                .map(|(t, _)| t)
                .filter(|t| !self.retirement_accepted(t))
        })
    }
    pub fn host_id(&self) -> &str {
        &self.host.id
    }
    pub fn connection_generation(&self) -> u64 {
        self.connection_generation
    }
    pub fn token(&self) -> Option<&Token> {
        self.token.as_ref()
    }
    pub fn pose(&self) -> &str {
        &self.pose
    }
    pub fn skin(&self) -> &str {
        &self.skin
    }
    pub fn motion(&self) -> &str {
        &self.motion
    }
    pub fn quiet(&self) -> bool {
        self.quiet
    }
    pub fn night(&self) -> bool {
        self.night
    }
    pub fn operation_error(&self) -> Option<&str> {
        self.operation_error.as_deref()
    }
    pub fn status(&self) -> &str {
        &self.status
    }
    pub fn pending_retirement(&self) -> bool {
        self.retired.is_some() && !self.retirement_submitted && self.boot.is_some()
    }
    pub fn begin_frame(&mut self, serial: u64) {
        self.frame = self.frame.max(serial);
    }
    fn matches_host(&self, t: &Token) -> bool {
        t.host_id == self.host.id
            && t.incarnation == self.host.incarnation
            && t.surface_id == self.host.surface_id
    }
    pub fn accepts_callback(&self, t: &Token) -> bool {
        self.token.as_ref() == Some(t) && self.boot.as_deref() == Some(t.boot_id.as_str())
    }
    pub fn body_submitted(&mut self, serial: u64) {
        if let Some(token) = self.token.clone() {
            self.drawn = Some((token, serial));
        }
    }
    pub fn disconnect(&mut self) {
        self.drawn = None;
        self.presented = None;
        if let Some(t) = self.token.take() {
            self.retired = Some((t, self.frame));
        }
        self.retirement_submitted = false;
        self.boot = None;
        self.status = "Eva disconnected — shared body hidden".into();
    }
    pub fn receive(&mut self, reply: Reply) -> Option<Request> {
        match reply {
            Reply::WithdrawalAccepted {
                host_id,
                incarnation,
            } => {
                if host_id == self.host.id && incarnation == self.host.incarnation {
                    self.withdrawal_accepted = true;
                }
            }
            Reply::RetirementAccepted { token } => {
                if self.retired.as_ref().is_some_and(|(old, _)| old == &token)
                    && self.retirement_submitted
                {
                    self.retirement_accepted = Some(token);
                }
            }
            Reply::Comfort { quiet, night } => {
                self.quiet = quiet;
                self.night = night;
            }
            Reply::OperationError { operation, reason } => {
                self.operation_error = Some(format!("{operation}: {reason}"));
            }
            Reply::Appearance { skin, motion } => {
                self.skin = skin;
                self.motion = motion;
            }
            Reply::Accepted { boot_id, .. } => {
                self.withdrawal_accepted = false;
                self.connection_generation = self.connection_generation.saturating_add(1);
                if self.boot.as_ref() != Some(&boot_id) {
                    self.token = None;
                    self.revision = 0;
                }
                self.boot = Some(boot_id);
                self.status = "Eva connected — awaiting residence".into();
            }
            Reply::Rejected { reason } => {
                self.disconnect();
                self.status = format!("Eva unavailable: {reason}");
            }
            Reply::Prepare { epoch } => {
                if self.boot.is_some()
                    && self.host.retirement != RetirementCapability::None
                    && (self.retired.is_none() || self.retirement_submitted)
                {
                    return Some(Request::Prepared { epoch });
                }
            }
            Reply::Grant { token, revision } => {
                if self.host.retirement != RetirementCapability::None
                    && self.matches_host(&token)
                    && self.boot.as_deref() == Some(token.boot_id.as_str())
                    && revision >= self.revision
                    && (self.retired.is_none() || self.retirement_submitted)
                    && self.retired.as_ref().is_none_or(|(old, _)| {
                        old.boot_id != token.boot_id || token.epoch > old.epoch
                    })
                    && self
                        .token
                        .as_ref()
                        .is_none_or(|current| token.epoch >= current.epoch)
                {
                    self.revision = revision;
                    self.withdrawal_accepted = false;
                    self.retired = None;
                    self.retirement_submitted = false;
                    self.token = Some(token);
                    self.status = "Eva · waiting".into();
                }
            }
            Reply::Revoke { token } => {
                if self.matches_host(&token)
                    && (self.token.as_ref() == Some(&token)
                        || self.retired.as_ref().is_some_and(|(old, _)| old == &token))
                {
                    self.token = None;
                    self.drawn = None;
                    self.presented = None;
                    self.retired = Some((token, self.frame));
                    self.retirement_submitted = false;
                    self.status = "Eva moving — awaiting cleared frame".into();
                }
            }
            Reply::Snapshot {
                revision,
                token,
                pose,
                recovery_blocked,
                ..
            } if revision >= self.revision => {
                self.revision = revision;
                self.pose = pose;
                if token.as_ref() != self.token.as_ref() {
                    self.token = None;
                }
                if recovery_blocked {
                    self.status =
                        "Eva recovery blocked — old surface needs retirement proof".into();
                }
            }
            _ => {}
        }
        None
    }
    /// Call ONLY after the native renderer successfully presented the cleared
    /// frame. A tick, message receipt, layout pass, GPU enqueue or heartbeat is
    /// not evidence. The serial must identify a frame begun after revocation.
    pub fn frame_presented(&mut self, serial: u64) -> Option<Request> {
        if self.retired.is_none() {
            let (token, at) = self.drawn.as_ref()?;
            if serial >= *at
                && self.accepts_callback(token)
                && self.presented.as_ref() != Some(token)
            {
                let token = token.clone();
                self.presented = Some(token.clone());
                return Some(Request::Presented { token });
            }
            return None;
        }
        let (token, after) = self.retired.as_ref()?;
        if serial <= *after
            || self.host.retirement == RetirementCapability::None
            || self.retirement_submitted
            || self.boot.is_none()
        {
            return None;
        }
        let token = token.clone();
        self.retirement_submitted = true;
        Some(Request::Retired {
            token,
            presentation_serial: serial,
        })
    }
}

fn requires_reconnect(reply: &Reply) -> bool {
    matches!(reply, Reply::Rejected { .. })
}

// The channel has a fixed 64-event bound; keeping replies inline avoids a heap
// allocation for every event. The rare disconnect marker shares that bounded storage.
#[allow(clippy::large_enum_variant)]
enum WorkerEvent {
    Reply(Reply),
    Disconnected,
}
/// GUI owner; do not create one per split, block, history entry or preview.
pub struct SharedHost {
    pub gate: HostGate,
    tx: mpsc::SyncSender<Request>,
    rx: mpsc::Receiver<WorkerEvent>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    failed: bool,
    metadata: Arc<std::sync::Mutex<[Option<Request>; 3]>>,
    dialogue: std::collections::VecDeque<Reply>,
    memory: std::collections::VecDeque<Reply>,
    context_access: std::collections::VecDeque<Reply>,
}
impl SharedHost {
    pub fn from_env(
        app: &str,
        capability: RetirementCapability,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        let host = Host {
            id: format!("{app}-{}", uuid::Uuid::new_v4()),
            incarnation: 1,
            surface_id: "eva-overlay".into(),
            retirement: capability,
        };
        let (tx, requests) = mpsc::sync_channel(64);
        let (events, rx) = mpsc::sync_channel(64);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopping = stop.clone();
        let metadata = Arc::new(std::sync::Mutex::new([None, None, None]));
        let worker_metadata = metadata.clone();
        let worker_host = host.clone();
        let wake = Arc::new(wake);
        let spawned = std::thread::Builder::new()
            .name(format!("{app}-eva-ipc"))
            .spawn(move || {
                let session = std::env::var("XDG_SESSION_ID").unwrap_or_default();
                let path = std::env::var_os("EVA_COMPANION_SOCKET")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| {
                        std::path::PathBuf::from(
                            std::env::var_os("XDG_RUNTIME_DIR").unwrap_or_default(),
                        )
                        .join("jcompanion/eva.sock")
                    });
                let mut disconnected_notice = false;
                while !stopping.load(std::sync::atomic::Ordering::Relaxed) {
                    let result = (|| -> std::io::Result<()> {
                        let mut client = companion_client::Client::connect(
                            &path,
                            &session,
                            worker_host.clone(),
                        )?;
                        while !stopping.load(std::sync::atomic::Ordering::Relaxed) {
                            if !client.boot_id().is_empty() {
                                let pending = if let Ok(mut latest) = worker_metadata.lock() {
                                    std::mem::take(&mut *latest)
                                } else {
                                    return Err(std::io::Error::other("metadata lock poisoned"));
                                };
                                // Geometry precedes focus, but task results and user intents remain ordered controls.
                                let [focus, geometry, input] = pending;
                                for request in [geometry, focus, input].into_iter().flatten() {
                                    client.send(request)?;
                                }
                                while let Ok(request) = requests.try_recv() {
                                    client.send(request)?;
                                }
                            }
                            let replies = client.poll()?;
                            if !replies.is_empty() {
                                let reconnect = replies.iter().any(requires_reconnect);
                                for reply in replies {
                                    events.try_send(WorkerEvent::Reply(reply)).map_err(|_| {
                                        std::io::Error::other("GUI event queue saturated")
                                    })?;
                                }
                                disconnected_notice = false;
                                wake();
                                if reconnect {
                                    return Err(std::io::Error::new(
                                        std::io::ErrorKind::ConnectionAborted,
                                        "daemon rejected session; reconnect required",
                                    ));
                                }
                            }
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Ok(())
                    })();
                    if result.is_ok() {
                        break;
                    }
                    if !disconnected_notice {
                        // If delivery itself fails, terminate and let the GUI observe channel closure.
                        if events.try_send(WorkerEvent::Disconnected).is_err() {
                            wake();
                            break;
                        }
                        wake();
                        disconnected_notice = true;
                    }
                    for _ in 0..20 {
                        if stopping.load(std::sync::atomic::Ordering::Relaxed) {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
            });
        Self {
            gate: HostGate::new(host),
            tx,
            rx,
            stop,
            failed: spawned.is_err(),
            metadata,
            dialogue: Default::default(),
            memory: Default::default(),
            context_access: Default::default(),
        }
    }
    pub fn begin_frame(&mut self, serial: u64) {
        self.gate.begin_frame(serial)
    }
    /// Register actual body/compact drawing in this native frame (not a grant).
    pub fn body_submitted(&mut self, serial: u64) {
        self.gate.body_submitted(serial)
    }
    pub fn poll(&mut self) -> bool {
        if self.failed {
            return false;
        }
        let mut changed = false;
        loop {
            match self.rx.try_recv() {
                Ok(WorkerEvent::Reply(r)) => {
                    changed = true;
                    if matches!(&r, Reply::ContextAccessReview { .. }) {
                        if self.context_access.len() >= 8 {
                            self.gate.disconnect();
                            self.failed = true;
                            self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
                            return true;
                        }
                        self.context_access.push_back(r);
                        continue;
                    }
                    if matches!(&r, Reply::Dialogue { .. }) {
                        if self.dialogue.len() >= 32 {
                            self.gate.disconnect();
                            self.failed = true;
                            self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
                            return true;
                        }
                        self.dialogue.push_back(r);
                        continue;
                    }
                    if matches!(
                        &r,
                        Reply::MemoryList { .. }
                            | Reply::MemoryReview { .. }
                            | Reply::MemoryResult { .. }
                            | Reply::MemoryScopeReview { .. }
                            | Reply::TaskScopes { .. }
                            | Reply::MemoryScopes { .. }
                    ) {
                        if self.memory.len() >= 32 {
                            self.gate.disconnect();
                            self.failed = true;
                            self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
                            return true;
                        }
                        self.memory.push_back(r);
                        continue;
                    }
                    if let Some(r) = self.gate.receive(r) {
                        self.send(r);
                        if self.failed {
                            return true;
                        }
                    }
                }
                Ok(WorkerEvent::Disconnected) => {
                    self.gate.disconnect();
                    changed = true;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.gate.disconnect();
                    self.failed = true;
                    return true;
                }
                Err(mpsc::TryRecvError::Empty) => break,
            }
        }
        changed
    }
    pub fn take_context_access(&mut self) -> Option<Reply> {
        self.context_access.pop_front()
    }
    pub fn take_dialogue(&mut self) -> Option<Reply> {
        self.dialogue.pop_front()
    }
    pub fn take_memory(&mut self) -> Option<Reply> {
        self.memory.pop_front()
    }
    pub fn send(&mut self, request: Request) {
        if self.failed {
            return;
        }
        let slot = match &request {
            Request::Focus { .. } => Some(0),
            Request::Geometry { .. } => Some(1),
            Request::Input { .. } => Some(2),
            _ => None,
        };
        if let Some(slot) = slot {
            if let Ok(mut latest) = self.metadata.lock() {
                latest[slot] = Some(request);
                return;
            }
        }
        if self.tx.try_send(request).is_err() {
            self.gate.disconnect();
            self.failed = true;
            self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
    pub fn frame_presented(&mut self, serial: u64) {
        if let Some(request) = self.gate.frame_presented(serial) {
            self.send(request)
        }
    }
}
impl Drop for SharedHost {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn gate() -> HostGate {
        HostGate::new(Host {
            id: "h".into(),
            incarnation: 1,
            surface_id: "s".into(),
            retirement: RetirementCapability::NativePresented,
        })
    }
    fn token() -> Token {
        Token {
            companion_id: "eva".into(),
            host_id: "h".into(),
            incarnation: 1,
            surface_id: "s".into(),
            boot_id: "b".into(),
            epoch: 1,
        }
    }
    fn active() -> HostGate {
        let mut g = gate();
        g.receive(Reply::Accepted {
            boot_id: "b".into(),
            companion_id: "eva".into(),
            session_id: "s".into(),
        });
        g.receive(Reply::Grant {
            token: token(),
            revision: 1,
        });
        g
    }
    #[test]
    fn revoked_pixels_need_later_real_frame() {
        let mut g = active();
        g.begin_frame(5);
        g.receive(Reply::Revoke { token: token() });
        assert!(g.token().is_none());
        assert!(g.frame_presented(5).is_none());
        assert!(matches!(
            g.frame_presented(6),
            Some(Request::Retired { .. })
        ));
        assert!(g.frame_presented(7).is_none());
    }
    #[test]
    fn disconnect_never_falls_back() {
        let mut g = active();
        g.disconnect();
        assert!(g.token().is_none());
        assert!(!g.accepts_callback(&token()));
        assert!(g.retired.is_some());
    }
    #[test]
    fn stale_callbacks_and_grants_rejected() {
        let mut g = active();
        let mut t = token();
        t.boot_id = "old".into();
        g.receive(Reply::Grant {
            token: t.clone(),
            revision: 9,
        });
        assert!(!g.accepts_callback(&t));
        assert_eq!(g.token(), Some(&token()));
    }
    #[test]
    fn no_presentation_capability_never_proves_retirement() {
        let mut g = active();
        g.host.retirement = RetirementCapability::None;
        g.receive(Reply::Revoke { token: token() });
        assert!(g.frame_presented(999).is_none());
    }
    #[test]
    fn reconnect_retires_old_boot_without_reviving_body() {
        let mut g = active();
        g.begin_frame(4);
        g.disconnect();
        assert!(g.frame_presented(5).is_none());
        g.receive(Reply::Accepted {
            boot_id: "new-boot".into(),
            companion_id: "eva".into(),
            session_id: "s".into(),
        });
        g.receive(Reply::Revoke { token: token() });
        assert!(!g.accepts_callback(&token()));
        assert!(
            matches!(g.frame_presented(6),Some(Request::Retired{token:Token{boot_id,..},..}) if boot_id=="b")
        );
    }
    #[test]
    fn completed_retirement_allows_later_prepare_not_old_callbacks() {
        let mut g = active();
        g.receive(Reply::Revoke { token: token() });
        g.frame_presented(1).unwrap();
        assert!(matches!(
            g.receive(Reply::Prepare { epoch: 2 }),
            Some(Request::Prepared { epoch: 2 })
        ));
        assert!(!g.accepts_callback(&token()));
    }
    #[test]
    fn wrong_incarnation_and_surface_do_not_grant() {
        for wrong in ["incarnation", "surface"] {
            let mut g = active();
            let mut t = token();
            if wrong == "incarnation" {
                t.incarnation += 1;
            } else {
                t.surface_id = "other".into();
            }
            g.receive(Reply::Grant {
                token: t.clone(),
                revision: 99,
            });
            assert!(!g.accepts_callback(&t));
        }
    }
    #[test]
    fn grant_receipt_is_not_actual_presentation() {
        let mut g = active();
        assert!(g.frame_presented(1).is_none());
        g.body_submitted(2);
        assert!(g.frame_presented(1).is_none());
        assert!(matches!(
            g.frame_presented(2),
            Some(Request::Presented { .. })
        ));
        assert!(g.frame_presented(3).is_none());
    }
    #[test]
    fn duplicate_old_grant_cannot_resurrect_retired_body() {
        let mut g = active();
        g.receive(Reply::Revoke { token: token() });
        g.frame_presented(1).unwrap();
        g.receive(Reply::Grant {
            token: token(),
            revision: 1,
        });
        assert!(g.token().is_none());
        let mut fresh = token();
        fresh.epoch = 2;
        g.receive(Reply::Grant {
            token: fresh.clone(),
            revision: 2,
        });
        assert_eq!(g.token(), Some(&fresh));
    }
    #[test]
    fn rejection_reconnects_and_preserves_retirement_obligation() {
        let mut g = active();
        g.begin_frame(7);
        let rejected = Reply::Rejected {
            reason: "session_binding".into(),
        };
        assert!(requires_reconnect(&rejected));
        g.receive(rejected);
        assert!(g.token().is_none());
        assert!(g.receive(Reply::Prepare { epoch: 2 }).is_none());
        assert!(g.frame_presented(8).is_none());
        g.receive(Reply::Accepted {
            boot_id: "b".into(),
            companion_id: "eva".into(),
            session_id: "s".into(),
        });
        g.receive(Reply::Revoke { token: token() });
        assert!(matches!(
            g.frame_presented(9),
            Some(Request::Retired { .. })
        ));
    }
    #[test]
    fn operation_error_does_not_disconnect_or_hide_body() {
        let mut g = active();
        let error = Reply::OperationError {
            operation: "recall".into(),
            reason: "handoff_pending".into(),
        };
        assert!(!requires_reconnect(&error));
        g.receive(error);
        assert!(g.accepts_callback(&token()));
        assert!(g.operation_error().unwrap().contains("handoff_pending"));
    }
    #[test]
    fn metadata_coalesces_without_dropping_ordered_controls() {
        let (tx, requests) = mpsc::sync_channel(64);
        let (_events, rx) = mpsc::sync_channel(64);
        let mut h = SharedHost {
            gate: active(),
            tx,
            rx,
            stop: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            failed: false,
            metadata: Arc::new(std::sync::Mutex::new([None, None, None])),
            dialogue: Default::default(),
            memory: Default::default(),
            context_access: Default::default(),
        };
        for n in 0..1000 {
            h.send(Request::Focus {
                focused: n % 2 == 0,
            });
            h.send(Request::Input {
                active: n % 2 == 0,
                selecting: false,
            });
        }
        h.send(Request::Recall);
        h.send(Request::Presence {
            mode: Presence::Hidden,
        });
        assert!(!h.failed);
        assert!(matches!(requests.try_recv(), Ok(Request::Recall)));
        assert!(matches!(
            requests.try_recv(),
            Ok(Request::Presence {
                mode: Presence::Hidden
            })
        ));
        assert!(matches!(
            h.metadata.lock().unwrap()[0],
            Some(Request::Focus { focused: false })
        ));
    }
    #[test]
    fn persisted_scope_discovery_is_bounded_and_preserves_review_origin() {
        let (tx, _requests) = mpsc::sync_channel(64);
        let (events, rx) = mpsc::sync_channel(64);
        let mut h = SharedHost {
            gate: active(), tx, rx,
            stop: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            failed: false,
            metadata: Arc::new(std::sync::Mutex::new([None, None, None])),
            dialogue: Default::default(), memory: Default::default(),
            context_access: Default::default(),
        };
        let discovery = Reply::MemoryScopes {
            scopes: vec![MemoryScope::Project("project-without-task".into())],
            next_after: Some("opaque-cursor".into()),
        };
        let review = Reply::MemoryScopeReview {
            context: ContextRef { host_id: "h".into(), pane_id: "original-pane".into(),
                task_id: None, project_id: Some("project-without-task".into()), revision: 7 },
            scope: MemoryScope::Project("project-without-task".into()),
            review_id: "review".into(), digest: "exact-digest".into(),
        };
        events.send(WorkerEvent::Reply(discovery.clone())).unwrap();
        events.send(WorkerEvent::Reply(review.clone())).unwrap();
        assert!(h.poll());
        assert_eq!(h.take_memory(), Some(discovery.clone()));
        assert_eq!(h.take_memory(), Some(review));
        assert!(h.take_memory().is_none());
        assert!(h.gate.accepts_callback(&token()));
        for _ in 0..33 { events.send(WorkerEvent::Reply(discovery.clone())).unwrap(); }
        h.poll();
        assert!(h.failed);
        assert_eq!(h.memory.len(), 32);
        assert!(h.gate.token().is_none());
    }
    #[test]
    fn healthy_close_waits_for_exact_durable_retirement_ack() {
        let mut g = active();
        assert_eq!(g.possible_body_token(), Some(&token()));
        g.receive(Reply::RetirementAccepted { token: token() });
        assert!(!g.retirement_accepted(&token()));
        g.receive(Reply::Revoke { token: token() });
        g.frame_presented(1).unwrap();
        assert!(!g.retirement_accepted(&token()));
        let mut stale = token();
        stale.epoch += 1;
        g.receive(Reply::RetirementAccepted { token: stale });
        assert!(!g.retirement_accepted(&token()));
        g.receive(Reply::RetirementAccepted { token: token() });
        assert!(g.retirement_accepted(&token()));
        assert!(g.possible_body_token().is_none());
    }
    #[test]
    fn restored_pane_counter_cannot_alias_an_earlier_window_task() {
        assert_ne!(
            host_scoped_task_id("window-a", "restored-pane", 1),
            host_scoped_task_id("window-b", "restored-pane", 1)
        );
        assert_ne!(
            host_scoped_task_id("window-a", "restored-pane", 1),
            host_scoped_task_id("window-a", "restored-pane", 2)
        );
        assert_eq!(
            host_scoped_task_id("window-a", "restored-pane", 1),
            host_scoped_task_id("window-a", "restored-pane", 1)
        );
    }
    #[test]
    fn withdrawal_ack_is_host_bound_and_new_grant_invalidates_it() {
        let mut g = active();
        g.receive(Reply::WithdrawalAccepted {
            host_id: "other".into(),
            incarnation: 1,
        });
        assert!(!g.withdrawal_accepted());
        g.receive(Reply::WithdrawalAccepted {
            host_id: "h".into(),
            incarnation: 1,
        });
        assert!(g.withdrawal_accepted());
        g.receive(Reply::Grant {
            token: token(),
            revision: 2,
        });
        assert!(!g.withdrawal_accepted());
    }
}
