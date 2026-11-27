use futures_channel::mpsc::{self, Receiver};
use futures_util::StreamExt;
use gtk4::glib;
use jterm_runtime::child_lifecycle::{ChildLifecycle, EscalationPolicy};
use jterm_runtime::exit_status::{ExitTail, PtyExit};
use jterm_runtime::pty_session::PreparedPty;
use jterm_runtime::runtime_wake::WakeSink;
pub use jterm_runtime::session_actor::WriteError;
use jterm_runtime::session_actor::{SessionEvent, SplitWriteLimits};
use std::io;
use std::os::fd::{OwnedFd, RawFd};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const REPLY_BYTES: usize = 64 * 1024;
const REPLY_ENTRIES: usize = 16;

/// Presents a prepared native PTY to a GTK main-context consumer. The actor
/// owns all I/O, and the view can attach exactly one output consumer.
pub struct PreparedPtySession {
    actor: Arc<Mutex<Option<Arc<PreparedPty>>>>,
    wakeups: Mutex<Option<Receiver<()>>>,
    wake: WakeSink,
}

impl PreparedPtySession {
    pub fn start(
        master: OwnedFd,
        child: Arc<ChildLifecycle>,
        policy: EscalationPolicy,
        poll_reap: impl Fn(&ChildLifecycle) -> Option<i32> + Send + 'static,
        user_bytes: usize,
        user_entries: usize,
    ) -> io::Result<Self> {
        // One producer always has one reserved slot. The source holds the
        // actual events, so additional wake signals can safely coalesce.
        let (sender, wakeups) = mpsc::channel(0);
        let sender = Mutex::new(sender);
        let wake = WakeSink::new(move || {
            if let Ok(mut sender) = sender.lock() {
                let _ = sender.try_send(());
            }
        });
        let actor = PreparedPty::start(
            master,
            child,
            policy,
            poll_reap,
            SplitWriteLimits {
                user_bytes,
                response_bytes: REPLY_BYTES,
                entries: user_entries.saturating_add(REPLY_ENTRIES),
                coalesce_bytes: 4096,
            },
            REPLY_ENTRIES,
            wake.clone(),
        )?;
        Ok(Self {
            actor: Arc::new(Mutex::new(Some(Arc::new(actor)))),
            wakeups: Mutex::new(Some(wakeups)),
            wake,
        })
    }

    fn actor(&self) -> Option<Arc<PreparedPty>> {
        self.actor.lock().ok()?.clone()
    }

    pub fn write_input(&self, bytes: Vec<u8>) -> Result<(), WriteError> {
        self.actor().ok_or(WriteError::Closed)?.write_input(bytes)
    }

    pub fn write_reply(&self, bytes: &[u8]) -> Result<(), WriteError> {
        self.actor()
            .ok_or(WriteError::Closed)?
            .write_reply(bytes.to_vec())
    }

    pub fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        self.actor()
            .ok_or(io::ErrorKind::BrokenPipe)?
            .resize(cols, rows)
    }

    pub fn inspect_fd<R>(&self, inspect: impl FnOnce(RawFd) -> R) -> Option<R> {
        self.actor()?.inspect_fd(inspect)
    }

    pub fn foreground_is_codex(&self) -> bool {
        self.actor().is_some_and(|actor| actor.foreground_is_codex())
    }

    pub fn input_stamp(&self) -> Option<u64> {
        self.actor()?.input_stamp()
    }

    pub fn try_write_control(&self, byte: u8, validate: impl FnOnce(i32) -> bool) -> bool {
        self.actor()
            .is_some_and(|actor| actor.try_write_control(byte, validate))
    }

    pub fn attach(
        &self,
        mut output: impl FnMut(Vec<u8>, Option<Instant>) + 'static,
        exited: impl FnOnce(PtyExit) + 'static,
    ) -> io::Result<()> {
        if self.actor().is_none() {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        let actor = Arc::downgrade(&self.actor);
        let mut wakeups = self
            .wakeups
            .lock()
            .map_err(|_| io::Error::other("PTY output receiver is unavailable"))?
            .take()
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::AlreadyExists, "PTY output already attached")
            })?;
        glib::MainContext::default().spawn_local_with_priority(
            glib::Priority::DEFAULT_IDLE,
            async move {
                let mut tail = ExitTail::Complete;
                while wakeups.next().await.is_some() {
                    loop {
                        let Some(slot) = actor.upgrade() else {
                            return;
                        };
                        let Some(owner) = slot.lock().ok().and_then(|actor| actor.clone()) else {
                            return;
                        };
                        let event = owner.next_event();
                        match event {
                            Some(SessionEvent::Exit(code)) => {
                                owner.freeze_after_exit();
                                if let Ok(mut actor) = slot.lock() {
                                    actor.take();
                                }
                                drop(owner);
                                exited(PtyExit { code, tail });
                                return;
                            }
                            Some(SessionEvent::Output(batch)) => {
                                let read_at = batch
                                    .fragments()
                                    .find_map(|part| part.origin.map(|origin| origin.read_at));
                                output(batch.into_bytes(), read_at);
                            }
                            Some(SessionEvent::ExitTail(status)) => tail = status,
                            Some(SessionEvent::Error(error)) => {
                                glib::g_warning!("jterm-runtime", "{error}");
                            }
                            None => break,
                        }
                        drop(owner);
                        // Long output bursts yield to GTK input and repaint.
                        glib::timeout_future_with_priority(
                            glib::Priority::DEFAULT_IDLE,
                            Duration::ZERO,
                        )
                        .await;
                    }
                }
            },
        );
        Ok(())
    }

    pub fn stop(&self) {
        let actor = self.actor.lock().ok().and_then(|mut actor| actor.take());
        if let Some(actor) = actor {
            actor.stop();
            drop(actor);
            self.wake.wake();
        }
    }
}

impl Drop for PreparedPtySession {
    fn drop(&mut self) {
        self.stop();
    }
}
