use std::{
    cell::OnceCell,
    ffi::{CString, OsString},
    os::fd::{BorrowedFd, RawFd},
    path::{Path, PathBuf},
    rc::Rc,
    sync::atomic::{AtomicBool, Ordering},
    sync::{Arc, Mutex, MutexGuard, TryLockError, Weak},
    thread::{self, Thread},
    time::{Duration, Instant},
};

use futures_channel::mpsc::{self, Receiver, Sender};
use jterm_engine::{
    colors::TerminalDefaultColors,
    frame_retention::process_frame_budget,
    history_search::{HistorySearch, HistorySearchProgress},
    hyperlink::HyperlinkPolicy,
    retained_bytes::{RetainedBytes, RetainedBytesBudget},
    terminal::{Terminal, WindowUpdates},
};
use jterm_runtime::{exit_status::PtyExit, pty_session::Session, runtime_wake::WakeSink};

use crate::pane::{INITIAL_COLUMNS, INITIAL_ROWS};

use crate::runtime_launch::{
    admit_configured_shell, admit_local_argv, candidate_program, runtime_environment,
    with_jsh_session, RUNTIME_OVERRIDE,
};
use crate::viewport::{ViewportFrame, ViewportRequest};

/// Main-context handle for the common runtime's sole PTY and terminal owner.
/// A window can retain it past widget removal while the child is being reaped.
pub struct RuntimeShell {
    state: Arc<Mutex<State>>,
    updates: Arc<Mutex<Update>>,
    live_viewport: Arc<AtomicBool>,
    worker: OnceCell<Thread>,
    observers: Arc<crate::runtime_observer::ObserverHub>,
}

struct State {
    session: Session,
    attachment: Option<crate::attachment::ViewAttachment>,
    closed: bool,
    shutdown: bool,
    write_error_reported: bool,
    view_revision: u64,
    viewport_request: Option<ViewportRequest>,
    captured_viewport: Option<CapturedViewport>,
    viewport_budget: Arc<RetainedBytesBudget>,
    viewport_retry_at: Option<Instant>,
}

struct CapturedViewport {
    request: ViewportRequest,
    revision: u64,
    frame: Result<(ViewportFrame, RetainedBytes), String>,
}

type ViewportDelivery = (ViewportFrame, bool, RetainedBytes);

const LIVE_VIEWPORT: ViewportRequest = ViewportRequest {
    top: None,
    painted_history: None,
};

#[derive(Default)]
pub(crate) struct Update {
    pub output_changed: bool,
    pub exit_status: Option<PtyExit>,
    pub failure: Option<String>,
    pub stopped: bool,
    pub window: WindowUpdates,
}

impl Update {
    fn has_activity(&self) -> bool {
        self.output_changed
            || self.exit_status.is_some()
            || self.failure.is_some()
            || self.stopped
            || self.window.title.is_some()
            || self.window.bell
    }

    fn merge(&mut self, update: Self) {
        self.output_changed |= update.output_changed;
        self.exit_status = update.exit_status.or(self.exit_status);
        if let Some(failure) = update.failure {
            self.failure = Some(failure);
        }
        self.stopped |= update.stopped;
        if let Some(title) = update.window.title {
            self.window.title = Some(title);
        }
        self.window.bell |= update.window.bell;
    }
}

impl RuntimeShell {
    /// Whether the frontend's resolved, configured shell can use the ordinary
    /// shared pane. Standalone previews use the narrower candidate profile.
    pub fn supports_local_shell(argv: &[String], host_bridge: bool) -> bool {
        admit_configured_shell(argv, host_bridge, None)
    }

    /// Preserve the pane's saved jsh session before the frontend applies a
    /// host bridge that changes the outer executable and its argument list.
    pub fn configured_shell_argv<'a>(
        argv: &'a [String],
        session_id: &str,
    ) -> Option<std::borrow::Cow<'a, [String]>> {
        with_jsh_session(argv, session_id)
    }

    pub fn admits_candidate(argv: &[String], host_bridge: bool) -> bool {
        admit_local_argv(
            argv,
            host_bridge,
            std::env::var_os(RUNTIME_OVERRIDE).as_deref(),
        )
    }

    pub fn spawn_candidate(
        argv: &[String],
        cwd: Option<&str>,
        host_bridge: bool,
        directory_available: fn(&str) -> bool,
        prepare_environment: impl FnOnce() -> Result<Vec<CString>, String>,
    ) -> Result<Option<Rc<Self>>, String> {
        if !Self::admits_candidate(argv, host_bridge) {
            return Ok(None);
        }
        Self::spawn_interactive(argv, cwd, directory_available, prepare_environment).map(Some)
    }

    fn spawn_interactive(
        argv: &[String],
        cwd: Option<&str>,
        directory_available: fn(&str) -> bool,
        prepare_environment: impl FnOnce() -> Result<Vec<CString>, String>,
    ) -> Result<Rc<Self>, String> {
        let cwd = local_directory(cwd, directory_available)?;
        let runtime = Self::spawn_program(argv, &cwd, prepare_environment()?)?;
        runtime.enable_primary_reflow()?;
        Ok(runtime)
    }

    /// Start an ordinary local shell in the shared runtime. Executable lookup
    /// uses the prepared child PATH and directory. Preserve a valid jsh session
    /// identity when the caller supplies one.
    pub fn spawn_configured_shell(
        argv: &[String],
        cwd: Option<&str>,
        session_id: Option<&str>,
        directory_available: fn(&str) -> bool,
        prepare_environment: impl FnOnce() -> Result<Vec<CString>, String>,
    ) -> Result<Rc<Self>, String> {
        let argv = match session_id {
            Some(id) => with_jsh_session(argv, id).ok_or_else(|| {
                "configured jsh session identity is invalid or conflicts with its arguments"
                    .to_owned()
            })?,
            None => std::borrow::Cow::Borrowed(argv),
        };
        Self::spawn_interactive(&argv, cwd, directory_available, prepare_environment)
    }

    /// Open a new ordinary pane with the host's saved or freshly minted jsh id.
    pub fn spawn_session_candidate(
        argv: &[String],
        cwd: Option<&str>,
        session_id: &str,
        host_bridge: bool,
        directory_available: fn(&str) -> bool,
        prepare_environment: impl FnOnce() -> Result<Vec<CString>, String>,
    ) -> Result<Option<Rc<Self>>, String> {
        if !admit_configured_shell(
            argv,
            host_bridge,
            std::env::var_os(RUNTIME_OVERRIDE).as_deref(),
        ) {
            return Ok(None);
        }
        let Some(argv) = with_jsh_session(argv, session_id) else {
            return Ok(None);
        };
        Self::spawn_interactive(&argv, cwd, directory_available, prepare_environment).map(Some)
    }

    /// The image expected at the prompt after any admitted shell bootstrap.
    pub fn candidate_program(argv: &[String]) -> Option<&Path> {
        candidate_program(argv)
    }

    /// Start a host-selected program in a fresh PTY. File and project arguments
    /// are passed verbatim to the executable by the owning application.
    pub fn spawn_program(
        argv: &[String],
        cwd: &Path,
        environment: Vec<CString>,
    ) -> Result<Rc<Self>, String> {
        let argv = argv.iter().map(OsString::from).collect::<Vec<_>>();
        let env = runtime_environment(environment)?;
        let session =
            Session::spawn_command(INITIAL_ROWS, INITIAL_COLUMNS, &argv, Some(cwd), &env)?;
        Ok(Self::new(session))
    }

    pub fn admits_explicit_command(host_bridge: bool) -> bool {
        !host_bridge && std::env::var_os(RUNTIME_OVERRIDE).is_none_or(|value| value == "1")
    }

    /// Open the prepared command with a local GTK viewport. The argument list
    /// is already complete and the directory is the command's lookup base.
    pub fn spawn_explicit_command(
        argv: &[String],
        cwd: Option<&str>,
        host_bridge: bool,
        prepare_environment: impl FnOnce() -> Result<Vec<CString>, String>,
    ) -> Result<Option<Rc<Self>>, String> {
        if !Self::admits_explicit_command(host_bridge) {
            return Ok(None);
        }
        Self::spawn_pane_command(argv, cwd, None, prepare_environment()?).map(Some)
    }

    /// Launch the command approved by the application in its own PTY. An
    /// open directory takes precedence over the path used for pane metadata.
    pub fn spawn_pane_command(
        argv: &[String],
        cwd: Option<&str>,
        pinned_directory: Option<BorrowedFd<'_>>,
        environment: Vec<CString>,
    ) -> Result<Rc<Self>, String> {
        let argv = argv.iter().map(OsString::from).collect::<Vec<_>>();
        let env = runtime_environment(environment)?;
        let mut session = match pinned_directory {
            Some(directory) => {
                Session::spawn_pinned_command(INITIAL_ROWS, INITIAL_COLUMNS, &argv, directory, &env)
            }
            None => Session::spawn_command(
                INITIAL_ROWS,
                INITIAL_COLUMNS,
                &argv,
                cwd.map(Path::new),
                &env,
            ),
        }?;
        session.enable_primary_reflow();
        Ok(Self::new(session))
    }

    /// Open the shared PTY for a host bridge that forwards one directory fd.
    pub fn spawn_forwarded_pane_command(
        argv: &[String],
        directory: BorrowedFd<'_>,
        child_fd: RawFd,
        environment: Vec<CString>,
    ) -> Result<Rc<Self>, String> {
        let argv = argv.iter().map(OsString::from).collect::<Vec<_>>();
        let env = runtime_environment(environment)?;
        let mut session = Session::spawn_command_with_forwarded_fd(
            INITIAL_ROWS,
            INITIAL_COLUMNS,
            &argv,
            directory,
            child_fd,
            &env,
        )?;
        session.enable_primary_reflow();
        Ok(Self::new(session))
    }

    /// Start the frontend's host-approved attachment relay in a local PTY.
    /// The Session Host continues to own the provider or validation process.
    pub fn spawn_hosted_attachment(
        argv: &[String],
        cwd: &Path,
        prepare_environment: impl FnOnce() -> Result<Vec<CString>, String>,
    ) -> Result<Option<Rc<Self>>, String> {
        if std::env::var_os(RUNTIME_OVERRIDE).is_some_and(|value| value != "1") {
            return Ok(None);
        }
        Self::spawn_program(argv, cwd, prepare_environment()?).map(Some)
    }

    pub fn spawn_anvil_bash_candidate(
        argv: &[String],
        cwd: Option<&str>,
        host_bridge: bool,
        directory_available: fn(&str) -> bool,
        startup_script: &[u8],
        prepare_environment: impl FnOnce() -> Result<Vec<CString>, String>,
    ) -> Result<Option<Rc<Self>>, String> {
        let [program] = argv else {
            return Ok(None);
        };
        if host_bridge
            || std::env::var_os(RUNTIME_OVERRIDE).as_deref() != Some(std::ffi::OsStr::new("1"))
            || !matches!(program.as_str(), "/bin/bash" | "/usr/bin/bash")
        {
            return Ok(None);
        }
        Self::spawn_bundled_bash(
            Path::new(program),
            cwd,
            directory_available,
            startup_script,
            prepare_environment,
        )
        .map(Some)
    }

    /// Launch the application's bundled Bash startup script under the shared
    /// runtime, retaining its private shell identity channel.
    pub fn spawn_bundled_bash(
        program: &Path,
        cwd: Option<&str>,
        directory_available: fn(&str) -> bool,
        startup_script: &[u8],
        prepare_environment: impl FnOnce() -> Result<Vec<CString>, String>,
    ) -> Result<Rc<Self>, String> {
        let cwd = local_directory(cwd, directory_available)?
            .canonicalize()
            .map_err(|error| format!("invalid PTY working directory: {error}"))?;
        let env = runtime_environment(prepare_environment()?)?;
        let mut session = Session::spawn_anvil_bash(
            INITIAL_ROWS,
            INITIAL_COLUMNS,
            program,
            startup_script,
            &cwd,
            &env,
        )?;
        session.enable_primary_reflow();
        Ok(Self::new(session))
    }

    fn new(session: Session) -> Rc<Self> {
        let attachment = session
            .terminal()
            .frame()
            .ok()
            .and_then(|frame| u32::try_from(frame.stamp().epoch).ok())
            .map(crate::attachment::ViewAttachment::new);
        Rc::new(Self {
            state: Arc::new(Mutex::new(State {
                session,
                attachment,
                closed: false,
                shutdown: false,
                write_error_reported: false,
                view_revision: 0,
                viewport_request: None,
                captured_viewport: None,
                viewport_budget: Arc::clone(process_frame_budget()),
                viewport_retry_at: None,
            })),
            updates: Arc::new(Mutex::new(Update::default())),
            live_viewport: Arc::new(AtomicBool::new(false)),
            worker: OnceCell::new(),
            observers: Arc::default(),
        })
    }

    /// Identity of the current view; detach preserves its session and actor epoch.
    pub fn view_attachment(&self) -> Result<crate::Attachment, String> {
        self.lock()?
            .attachment
            .as_ref()
            .map(|view| view.ticket())
            .ok_or_else(|| "terminal epoch cannot be represented".into())
    }

    pub fn view_is_attached(&self) -> bool {
        self.lock().ok().is_some_and(|state| {
            state
                .attachment
                .as_ref()
                .is_some_and(|view| view.attached())
        })
    }

    pub(crate) fn switch_view(
        &self,
        expected: &crate::Attachment,
        attach: bool,
    ) -> Result<crate::Attachment, String> {
        let mut state = self.lock()?;
        if state.shutdown || state.closed {
            return Err("terminal session is closed".into());
        }
        let ticket = state
            .attachment
            .as_mut()
            .ok_or("terminal attachment unavailable")?
            .switch(expected, attach)?;
        state.viewport_request = None;
        state.captured_viewport = None;
        state.viewport_retry_at = None;
        self.live_viewport.store(false, Ordering::Relaxed);
        Ok(ticket)
    }

    pub(crate) fn input_for(
        &self,
        expected: &crate::Attachment,
        bytes: Vec<u8>,
    ) -> Result<bool, String> {
        let mut state = self.lock()?;
        if !state
            .attachment
            .as_ref()
            .is_some_and(|view| view.admits(expected))
        {
            return Err("stale terminal view attachment".into());
        }
        if bytes.len() > jterm_engine::terminal::MAX_INPUT_BYTES {
            return Err("terminal input byte limit".into());
        }
        if bytes.is_empty() {
            return Ok(false);
        }
        state.session.submit_input(bytes)?;
        if let Some(worker) = self.worker.get() {
            worker.unpark();
        }
        Ok(true)
    }

    pub fn child_pid(&self) -> Option<i32> {
        self.lock().ok()?.session.child_pid()
    }

    /// Observe a live child while its lifecycle is retained by this session.
    pub fn inspect_child<T>(&self, read: impl FnOnce(i32) -> Option<T>) -> Option<T> {
        let state = self.lock().ok()?;
        let pid = state.session.child_pid()?;
        let value = read(pid)?;
        (state.session.child_pid() == Some(pid)).then_some(value)
    }

    pub(crate) fn foreground_is_inline_codex(&self) -> bool {
        self.state.try_lock().is_ok_and(|state| {
            !state.session.terminal().alternate_screen_active() && state.session.foreground_is_codex()
        })
    }

    pub fn exit_code(&self) -> Option<i32> {
        self.lock().ok()?.session.exit_code()
    }

    /// Subscribe without acquiring input, geometry, or lifecycle authority.
    pub fn observe(&self) -> Result<crate::RuntimeObserver, String> {
        if self.worker.get().is_none() {
            return Err("The terminal owner is not attached".into());
        }
        let observer = self.observers.subscribe()?;
        if let Some(worker) = self.worker.get() {
            worker.unpark();
        }
        Ok(observer)
    }

    pub fn close(&self) {
        if let Ok(mut state) = self.lock() {
            state.shutdown = true;
            state.viewport_request = None;
            state.captured_viewport = None;
            state.close();
            if self.observers.active() {
                if let Ok(Some(frame)) = LIVE_VIEWPORT.capture(state.session.terminal(), true) {
                    self.observers
                        .publish(frame, true, state.session.exit_code());
                }
            }
            self.observers.close(state.session.exit_code());
        } else {
            self.observers.close(None);
        }
        if let Some(worker) = self.worker.get() {
            worker.unpark();
        }
    }

    pub fn ready_to_discard(&self) -> bool {
        self.lock().is_ok_and(|state| {
            (state.closed || state.session.is_drained()) && !state.session.cleanup_pending()
        })
    }

    pub(crate) fn enable_primary_reflow(&self) -> Result<(), String> {
        self.lock()?.session.enable_primary_reflow();
        Ok(())
    }

    pub(crate) fn configure_view(
        &self,
        hyperlinks: HyperlinkPolicy,
        colors: TerminalDefaultColors,
    ) -> Result<Receiver<()>, String> {
        let mut state = self.lock()?;
        if state.closed || self.worker.get().is_some() {
            return Err("terminal runtime already attached or closed".into());
        }
        state.session.enable_underline_colors();
        state.session.enable_grapheme_clusters();
        state.session.enable_hyperlinks(hyperlinks);
        state.session.enable_window_updates();
        state.session.set_color_defaults(colors);
        state.session.enable_keyboard_disambiguation();
        state.session.enable_history_preserving_resize();
        let owner = Arc::downgrade(&self.state);
        let pending = self.updates.clone();
        let live_viewport = self.live_viewport.clone();
        let observers = self.observers.clone();
        // A single producer always has one reserved slot. Wakes coalesce while
        // GTK is busy because State owns the actual presentation status.
        let (updates, receiver) = mpsc::channel(0);
        let worker = thread::Builder::new()
            .name("gtk-terminal-runtime".into())
            .spawn(move || drive(owner, pending, updates, live_viewport, observers))
            .map_err(|error| format!("terminal worker unavailable: {error}"))?
            .thread()
            .clone();
        self.worker
            .set(worker.clone())
            .map_err(|_| "terminal worker already installed".to_owned())?;
        state
            .session
            .set_output_wake(WakeSink::new(move || worker.unpark()))?;
        Ok(receiver)
    }

    pub(crate) fn inspect<T>(&self, read: impl FnOnce(&Terminal) -> T) -> Result<T, String> {
        Ok(read(self.lock()?.session.terminal()))
    }

    pub(crate) async fn history_search_step(
        &self,
        mut search: HistorySearch,
    ) -> Result<(HistorySearch, HistorySearchProgress), String> {
        let owner = Arc::downgrade(&self.state);
        gtk4::gio::spawn_blocking(move || {
            let owner = owner.upgrade().ok_or("terminal pane is closed")?;
            let state = owner
                .lock()
                .map_err(|_| "terminal runtime lost its owner")?;
            if state.shutdown {
                return Err("terminal pane is closed".into());
            }
            let progress = search.step(state.session.terminal())?;
            Ok((search, progress))
        })
        .await
        .map_err(|_| "terminal history search failed".to_owned())?
    }

    pub(crate) fn input(
        &self,
        encode: impl FnOnce(&Terminal) -> Result<Vec<u8>, String>,
    ) -> Result<bool, String> {
        let mut state = self.lock()?;
        if !state
            .attachment
            .as_ref()
            .is_some_and(|view| view.attached())
        {
            return Err("terminal view is detached".into());
        }
        let bytes = encode(state.session.terminal())?;
        if bytes.is_empty() {
            return Ok(false);
        }
        state.session.submit_input(bytes)?;
        if let Some(worker) = self.worker.get() {
            worker.unpark();
        }
        Ok(true)
    }

    pub(crate) fn paste(&self, text: &str) -> Result<bool, String> {
        let mut state = self.lock()?;
        if !state
            .attachment
            .as_ref()
            .is_some_and(|view| view.attached())
        {
            return Err("terminal view is detached".into());
        }
        let accepted = state.session.submit_native_paste(text)?;
        drop(state);
        if accepted {
            if let Some(worker) = self.worker.get() {
                worker.unpark();
            }
        }
        Ok(accepted)
    }

    pub(crate) fn resize(
        &self,
        rows: usize,
        columns: usize,
        pixels: Option<(u16, u16)>,
    ) -> Result<(), String> {
        let mut state = self.lock()?;
        if !state
            .attachment
            .as_ref()
            .is_some_and(|view| view.attached())
        {
            return Err("terminal view is detached".into());
        }
        let pixels = pixels.map(|(width, height)| {
            jterm_engine::native_window_ops::NativeWindowPixels::Viewport {
                width: u32::from(width),
                height: u32::from(height),
            }
        });
        let grid_changed = pixels.is_none()
            || state.session.terminal().window_geometry().is_none_or(|old| {
                (old.rows, old.columns) != (rows, columns)
            });
        state.session.resize_with_window_pixels(rows, columns, pixels)?;
        if grid_changed {
            state.view_revision = state.view_revision.wrapping_add(1);
            state.captured_viewport = None;
            self.observers.request_refresh();
        }
        if let Some(worker) = self.worker.get() {
            worker.unpark();
        }
        Ok(())
    }

    pub(crate) fn set_history_row_limit(&self, rows: usize) -> Result<(), String> {
        let mut state = self.lock()?;
        state.session.set_history_row_limit(rows);
        state.view_revision = state.view_revision.wrapping_add(1);
        state.captured_viewport = None;
        self.observers.request_refresh();
        if let Some(worker) = self.worker.get() {
            worker.unpark();
        }
        Ok(())
    }

    pub(crate) fn set_color_defaults(&self, defaults: TerminalDefaultColors) -> Result<(), String> {
        let mut state = self.lock()?;
        state.session.set_color_defaults(defaults);
        state.view_revision = state.view_revision.wrapping_add(1);
        state.captured_viewport = None;
        self.observers.request_refresh();
        if let Some(worker) = self.worker.get() {
            worker.unpark();
        }
        Ok(())
    }

    /// Tick callbacks never wait for parsing or history decompression. Keep
    /// only the latest request and one immutable, coalesced response.
    pub(crate) fn poll_viewport(
        &self,
        request: ViewportRequest,
    ) -> Result<Option<ViewportDelivery>, String> {
        let mut state = match self.state.try_lock() {
            Ok(state) => state,
            Err(TryLockError::WouldBlock) => return Ok(None),
            Err(TryLockError::Poisoned(_)) => return Err("terminal runtime lost its owner".into()),
        };
        if state.shutdown {
            return Ok(None);
        }
        if let Some(captured) = state
            .captured_viewport
            .take()
            .filter(|captured| captured.request == request)
        {
            let newer_output = captured.revision != state.view_revision;
            if newer_output {
                self.queue_viewport(&mut state, request);
            }
            return captured
                .frame
                .map(|(frame, delivery)| Some((frame, newer_output, delivery)));
        }
        self.queue_viewport(&mut state, request);
        Ok(None)
    }

    pub(crate) fn prepare_viewport(&self, request: ViewportRequest) {
        if let Ok(mut state) = self.state.try_lock() {
            self.queue_viewport(&mut state, request);
        }
    }

    /// A mapped, unselected pane at the live edge can use a snapshot captured
    /// in the same worker turn as its child output. This is a hint; explicit
    /// viewport requests still own resize, scroll and selection transitions.
    pub(crate) fn set_live_viewport(&self, visible: bool) {
        self.live_viewport.store(visible, Ordering::Relaxed);
    }

    fn queue_viewport(&self, state: &mut State, request: ViewportRequest) {
        if state.shutdown
            || state.viewport_request == Some(request)
            || state
                .captured_viewport
                .as_ref()
                .is_some_and(|captured| captured.request == request)
        {
            return;
        }
        state.viewport_request = Some(request);
        if let Some(worker) = self.worker.get() {
            worker.unpark();
        }
    }

    pub(crate) fn take_update(&self) -> Result<Update, String> {
        let mut pending = self
            .updates
            .lock()
            .map_err(|_| "terminal runtime lost its notifications")?;
        Ok(std::mem::take(&mut *pending))
    }

    fn lock(&self) -> Result<MutexGuard<'_, State>, String> {
        self.state
            .lock()
            .map_err(|_| "terminal runtime lost its owner".into())
    }
}

impl State {
    fn close(&mut self) {
        if !self.closed {
            self.closed = true;
            self.view_revision = self.view_revision.wrapping_add(1);
            let _ = self.session.close();
        }
    }

    fn poll_session(&mut self) -> (usize, Update) {
        let mut update = Update::default();
        if self.closed {
            return (0, update);
        }
        let read = match self.session.drive() {
            Ok(read) => read,
            Err(error) => {
                update.failure = Some(error);
                update.stopped = true;
                self.close();
                0
            }
        };
        if read != 0 {
            update.output_changed = true;
            self.view_revision = self.view_revision.wrapping_add(1);
        }
        update.window = self.session.take_window_updates();
        if !self.write_error_reported {
            if let Some(error) = self.session.write_error() {
                update.failure.get_or_insert_with(|| error.to_owned());
                self.write_error_reported = true;
            }
        }
        if self.session.is_drained() {
            update.exit_status = self.session.exit_status();
            self.close();
        }
        (read, update)
    }

    fn capture_viewport(&mut self, prefetch_live: bool) {
        let request = self.viewport_request.or_else(|| {
            (prefetch_live && self.captured_viewport.is_none()).then_some(LIVE_VIEWPORT)
        });
        let Some(request) = request else {
            return;
        };
        let now = Instant::now();
        if self.viewport_retry_at.is_some_and(|retry| now < retry) {
            return;
        }
        let frame = match request.capture(self.session.terminal(), self.closed) {
            Ok(Some(frame)) => {
                let Some(delivery) = frame.reserve_delivery(&self.viewport_budget) else {
                    // The PTY can keep advancing while this request waits for space.
                    self.viewport_retry_at = Some(Instant::now() + Duration::from_millis(64));
                    return;
                };
                Ok((frame, delivery))
            }
            Ok(None) => return,
            Err(error) => Err(error),
        };
        self.captured_viewport = Some(CapturedViewport {
            request,
            revision: self.view_revision,
            frame,
        });
        self.viewport_request = None;
        self.viewport_retry_at = None;
    }
}

impl Drop for RuntimeShell {
    fn drop(&mut self) {
        self.close();
    }
}

fn drive(
    owner: Weak<Mutex<State>>,
    pending: Arc<Mutex<Update>>,
    mut notifications: Sender<()>,
    live_viewport: Arc<AtomicBool>,
    observers: Arc<crate::runtime_observer::ObserverHub>,
) {
    struct CloseObservers(Arc<crate::runtime_observer::ObserverHub>);
    impl Drop for CloseObservers {
        fn drop(&mut self) {
            self.0.close(None);
        }
    }
    // A disconnected/poisoned owner must not leave live-looking observers.
    let _close_observers = CloseObservers(observers.clone());
    let mut observer_dirty = false;
    let mut observer_deadline = Instant::now();
    loop {
        let (read, retry, update) = {
            let Some(owner) = owner.upgrade() else { return };
            let Ok(mut state) = owner.lock() else { return };
            if state.shutdown {
                return;
            }
            let (read, update) = state.poll_session();
            let prefetch_live = live_viewport.load(Ordering::Relaxed)
                && (update.output_changed || update.exit_status.is_some() || update.stopped);
            state.capture_viewport(prefetch_live);
            observer_dirty |=
                update.output_changed || update.exit_status.is_some() || update.stopped;
            if observers.active()
                && (observers.requested() || observer_dirty)
                && ((state.closed && !observers.is_closed()) || Instant::now() >= observer_deadline)
            {
                match LIVE_VIEWPORT.capture(state.session.terminal(), state.closed) {
                    Ok(Some(frame)) => {
                        let published =
                            observers.publish(frame, state.closed, state.session.exit_code());
                        observer_dirty = !published;
                        observer_deadline =
                            Instant::now() + Duration::from_millis(if published { 16 } else { 64 });
                    }
                    Ok(None) => observers.request_refresh(),
                    Err(error) => {
                        observers.report_failure(&error);
                        observer_dirty = false;
                    }
                }
            }
            (
                read,
                state.session.needs_write_retry()
                    || state.viewport_request.is_some()
                    || (observers.active() && (observer_dirty || observers.requested())),
                update,
            )
        };
        if update.has_activity() {
            let Ok(mut pending) = pending.lock() else {
                return;
            };
            pending.merge(update);
            drop(pending);
            let _ = notifications.try_send(());
        }
        if read != 0 {
            // One reader message can exceed a bounded drive call.
            thread::yield_now();
        } else if retry {
            thread::park_timeout(Duration::from_millis(8));
        } else {
            thread::park();
        }
    }
}

fn local_directory(cwd: Option<&str>, available: fn(&str) -> bool) -> Result<PathBuf, String> {
    match cwd.filter(|directory| available(directory)) {
        Some(cwd) => Ok(PathBuf::from(cwd)),
        None => std::env::current_dir().map_err(|error| error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn updating_pixels_keeps_the_current_grid_snapshot() {
        let owner = RuntimeShell::spawn_pane_command(
            &["/bin/sh".into(), "-c".into(), "read line".into()],
            None,
            None,
            Vec::new(),
        )
        .unwrap();
        owner.resize(12, 40, Some((379, 204))).unwrap();
        let revision = owner.lock().unwrap().view_revision;
        owner.resize(12, 40, Some((758, 408))).unwrap();
        let state = owner.lock().unwrap();
        assert_eq!(state.view_revision, revision);
        let geometry = state.session.terminal().window_geometry().unwrap();
        assert_eq!((geometry.rows, geometry.columns), (12, 40));
        assert_eq!(geometry.text_pixels(), (408, 758));
        drop(state);
        owner.resize(12, 41, Some((778, 408))).unwrap();
        assert_ne!(owner.lock().unwrap().view_revision, revision);
    }

    #[test]
    fn two_detached_views_keep_real_actors_drain_replies_and_reject_stale_input() {
        use std::os::unix::fs::DirBuilderExt;
        let root = std::env::temp_dir().join(format!(
            "gtk-view-attachment-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&root)
            .unwrap();
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(root.clone());
        let program = r#"stty raw -echo
printf '\033[6;12H\033[5n\033[6n'
head -c 11 > "$1/$2.first"
printf 'READY'
while [ ! -f "$1/advance" ]; do sleep .01; done
yes detached | head -c 262144
printf '\r\nDETACHEDTAIL-%s\r\n\033[6;12H\033[5n\033[6n' "$2"
head -c 11 > "$1/$2.detached"
head -c 1 > "$1/$2.input"
printf '\r\nFINALTAIL-%s\r\n' "$2"
exit 17"#;
        let mut owners = Vec::new();
        let mut notices = Vec::new();
        for role in ["A", "B"] {
            let argv = [
                OsString::from("/bin/sh"),
                "-c".into(),
                program.into(),
                "fixture".into(),
                root.as_os_str().to_owned(),
                role.into(),
            ];
            let session = Session::spawn_command(
                INITIAL_ROWS,
                INITIAL_COLUMNS,
                &argv,
                None,
                &[("PATH".into(), "/usr/bin:/bin".into())],
            )
            .unwrap();
            let owner = RuntimeShell::new(session);
            notices.push(
                owner
                    .configure_view(|_, _| false, TerminalDefaultColors::default())
                    .unwrap(),
            );
            owners.push(owner);
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        let wait_file = |path: &Path| {
            while std::fs::metadata(path).map_or(true, |m| m.len() != 11) {
                assert!(Instant::now() < deadline, "missing query reply {path:?}");
                thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(std::fs::read(path).unwrap(), b"\x1b[0n\x1b[6;12R");
        };
        for role in ["A", "B"] {
            wait_file(&root.join(format!("{role}.first")));
        }
        let pids: Vec<_> = owners
            .iter()
            .map(|owner| owner.child_pid().unwrap())
            .collect();
        assert_ne!(pids[0], pids[1]);
        let initial: Vec<_> = owners
            .iter()
            .map(|owner| owner.view_attachment().unwrap())
            .collect();
        assert_ne!(initial[0].session, initial[1].session);
        let detached: Vec<_> = owners
            .iter()
            .zip(&initial)
            .map(|(owner, ticket)| owner.switch_view(ticket, false).unwrap())
            .collect();
        for (owner, ticket) in owners.iter().zip(&initial) {
            assert!(owner.input_for(ticket, b"stale".to_vec()).is_err());
            assert!(owner.input(|_| Ok(b"detached".to_vec())).is_err());
            assert!(owner.paste("detached").is_err());
            assert!(owner.resize(10, 10, None).is_err());
        }
        std::fs::write(root.join("advance"), b"go").unwrap();
        for role in ["A", "B"] {
            wait_file(&root.join(format!("{role}.detached")));
        }
        for (i, owner) in owners.iter().enumerate() {
            assert_eq!(owner.child_pid(), Some(pids[i]));
            let resumed = owner.switch_view(&detached[i], true).unwrap();
            assert_eq!(resumed.session, initial[i].session);
            assert_eq!(resumed.epoch, initial[i].epoch);
            assert!(owner.input_for(&initial[i], b"stale".to_vec()).is_err());
            assert!(owner.input_for(&detached[i], b"stale".to_vec()).is_err());
            assert!(owner
                .input_for(&owners[1 - i].view_attachment().unwrap(), b"peer".to_vec())
                .is_err());
            assert!(owner.input_for(&resumed, b"X".to_vec()).unwrap());
        }
        for (i, owner) in owners.iter().enumerate() {
            loop {
                let update = owner.take_update().unwrap();
                if let Some(status) = update.exit_status {
                    assert_eq!(status.code, 17);
                    break;
                }
                assert!(Instant::now() < deadline, "child did not finish");
                thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(
                std::fs::read(root.join(format!("{}.input", ["A", "B"][i]))).unwrap(),
                b"X"
            );
        }
    }

    #[test]
    fn configured_shell_resolves_child_path_and_recovers_an_unavailable_directory() {
        let cwd = std::env::current_dir().unwrap();
        let argv = vec![
            "bash".to_owned(),
            "--noprofile".to_owned(),
            "--norc".to_owned(),
            "-c".to_owned(),
            "test -t 0 && test \"$PWD\" = \"$1\" && test \"$2\" = 'two words' || exit 18; exit 17"
                .to_owned(),
            "configured-fixture".to_owned(),
            cwd.to_str().unwrap().to_owned(),
            "two words".to_owned(),
        ];
        assert!(!RuntimeShell::supports_local_shell(&argv, false));
        let owner = RuntimeShell::spawn_configured_shell(
            &argv,
            Some("/unavailable-restored-directory"),
            None,
            |_| false,
            || Ok(vec![CString::new("PATH=/usr/bin:/bin").unwrap()]),
        )
        .expect("start the configured relative shell in the shared PTY");
        owner
            .configure_view(|_, _| false, TerminalDefaultColors::default())
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut update = Update::default();
        while update.exit_status.is_none() && std::time::Instant::now() < deadline {
            update.merge(owner.take_update().unwrap());
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            update.exit_status,
            Some(PtyExit::from(17)),
            "{:?}",
            update.failure
        );
        assert!(owner.child_pid().is_none());
    }

    #[test]
    fn hosted_attachment_uses_the_prepared_argv_and_local_directory() {
        let program = [
            "/bin/sh",
            "-c",
            "test \"$PWD\" = / && test \"$1\" = '--directory=/host project' && test -t 0 || exit 18; \
             stty raw -echo; printf '\\033[5n'; reply=$(head -c 4); stty sane; \
             test \"$reply\" = \"$(printf '\\033[0n')\" || exit 19; exit 17",
            "bridge-fixture",
            "--directory=/host project",
        ]
        .map(str::to_owned);
        let owner = RuntimeShell::spawn_hosted_attachment(&program, Path::new("/"), || {
            Ok(vec![CString::new("PATH=/usr/bin:/bin").unwrap()])
        })
        .expect("start prepared attachment PTY");
        if std::env::var_os(RUNTIME_OVERRIDE).is_some_and(|value| value != "1") {
            assert!(owner.is_none());
            return;
        }
        let owner = owner.expect("shared hosted attachment");
        owner
            .configure_view(|_, _| false, TerminalDefaultColors::default())
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut update = Update::default();
        while update.exit_status.is_none() && std::time::Instant::now() < deadline {
            update.merge(owner.take_update().unwrap());
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            update.exit_status,
            Some(PtyExit::from(17)),
            "{:?}",
            update.failure
        );
        assert!(owner.child_pid().is_none());
    }

    #[test]
    fn background_runtime_processes_query_then_drains_exit_without_gtk() {
        let program = [
            "/bin/sh",
            "-c",
            "stty raw -echo; printf '\\033[5n'; reply=$(head -c 4); stty sane; \
             test \"$reply\" = \"$(printf '\\033[0n')\" || exit 18; \
             yes active | head -c 524288; printf '\\r\\nFINAL GTK PTY TAIL\\r\\n'; exit 17",
        ]
        .map(OsString::from);
        let env = [
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("TERM".into(), "dumb".into()),
        ];
        let session = Session::spawn_command(INITIAL_ROWS, INITIAL_COLUMNS, &program, None, &env)
            .expect("real PTY");
        let owner = RuntimeShell::new(session);
        owner
            .configure_view(|_, _| false, TerminalDefaultColors::default())
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut update = Update::default();
        while update.exit_status.is_none() && std::time::Instant::now() < deadline {
            update.merge(owner.take_update().unwrap());
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            update.exit_status,
            Some(PtyExit::from(17)),
            "{:?}",
            update.failure
        );
        assert!(update.output_changed);
        let request = ViewportRequest {
            top: None,
            painted_history: None,
        };
        let (viewport, newer_output, _delivery) = loop {
            if let Some(captured) = owner.poll_viewport(request).unwrap() {
                break captured;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "final viewport unavailable"
            );
            thread::sleep(Duration::from_millis(10));
        };
        assert!(!newer_output);
        assert!(viewport.frame.rows().iter().any(|row| {
            row.cells
                .iter()
                .map(|cell| cell.character)
                .collect::<String>()
                .contains("FINAL GTK PTY TAIL")
        }));
    }

    #[test]
    fn full_viewport_budget_defers_paint_but_drains_the_real_pty_reply() {
        let command = "stty raw -echo; printf '\\033[5n'; head -c 4 >/dev/null; printf 'VIEWPORT-AFTER-REPLY'; head -c 1 >/dev/null";
        let arguments = ["/bin/sh", "-c", command].map(OsString::from);
        let env = [("PATH".into(), "/usr/bin:/bin".into())];
        let session = Session::spawn_command(INITIAL_ROWS, INITIAL_COLUMNS, &arguments, None, &env)
            .expect("real PTY");
        let owner = RuntimeShell::new(session);
        let capacity = 1024 * 1024;
        let budget = Arc::new(RetainedBytesBudget::new(capacity));
        let busy = budget.reserve(capacity).unwrap();
        owner.lock().unwrap().viewport_budget = Arc::clone(&budget);
        let pid = owner.child_pid();
        let _notifications = owner
            .configure_view(|_, _| false, TerminalDefaultColors::default())
            .unwrap();
        let has_marker = |frame: &jterm_engine::frame::CellFrame| {
            frame.rows().iter().any(|row| {
                row.cells
                    .iter()
                    .map(|cell| cell.character)
                    .collect::<String>()
                    .contains("VIEWPORT-AFTER-REPLY")
            })
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(owner.poll_viewport(LIVE_VIEWPORT).unwrap().is_none());
            let state = owner.lock().unwrap();
            if state.viewport_retry_at.is_some()
                && has_marker(&state.session.terminal().frame().unwrap())
            {
                break;
            }
            drop(state);
            assert!(
                Instant::now() < deadline,
                "PTY did not receive the reply while paint waited"
            );
            thread::sleep(Duration::from_millis(4));
        }
        drop(busy);
        let (viewport, stale, delivery) = loop {
            if let Some(captured) = owner.poll_viewport(LIVE_VIEWPORT).unwrap() {
                break captured;
            }
            assert!(Instant::now() < deadline, "viewport did not recover");
            thread::sleep(Duration::from_millis(4));
        };
        assert!(!stale && has_marker(&viewport.frame));
        assert_eq!(owner.child_pid(), pid);
        assert_eq!(
            budget.usage().retained_bytes,
            viewport.frame.retained_delivery_bytes()
        );
        drop(viewport);
        drop(delivery);
        assert_eq!(budget.usage().retained_bytes, 0);
        owner.close();
    }

    #[test]
    fn visible_output_prefetch_is_bounded_and_hidden_output_needs_a_request() {
        let arguments = [
            "/bin/sh",
            "-c",
            "stty -echo; printf 'fixture ready\\r\\n'; while IFS= read -r line; do printf 'paint-%s\\r\\n' \"$line\"; done",
        ]
        .map(OsString::from);
        let env = [("PATH".into(), "/usr/bin:/bin".into())];
        let session = Session::spawn_command(INITIAL_ROWS, INITIAL_COLUMNS, &arguments, None, &env)
            .expect("real PTY");
        let owner = RuntimeShell::new(session);
        let _notifications = owner
            .configure_view(|_, _| false, TerminalDefaultColors::default())
            .unwrap();
        let contains = |frame: &jterm_engine::frame::CellFrame, text: &str| {
            frame.rows().iter().any(|row| {
                row.cells
                    .iter()
                    .map(|cell| cell.character)
                    .collect::<String>()
                    .contains(text)
            })
        };
        let wait_for_marker = |marker: &str| {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                let state = owner.lock().unwrap();
                if contains(&state.session.terminal().frame().unwrap(), marker) {
                    break;
                }
                drop(state);
                assert!(std::time::Instant::now() < deadline, "missing {marker}");
                thread::sleep(Duration::from_millis(4));
            }
        };
        let send = |line: &str| {
            owner
                .input(|_| Ok(format!("{line}\n").into_bytes()))
                .unwrap();
            wait_for_marker(&format!("paint-{line}"));
        };
        let receive = || {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                if let Some(snapshot) = owner.poll_viewport(LIVE_VIEWPORT).unwrap() {
                    break snapshot;
                }
                assert!(std::time::Instant::now() < deadline, "viewport unavailable");
                thread::sleep(Duration::from_millis(4));
            }
        };

        wait_for_marker("fixture ready");
        owner.set_live_viewport(true);
        send("first");
        assert!(owner.lock().unwrap().captured_viewport.is_some());
        send("second");
        let (first, stale, _delivery) = receive();
        assert!(stale);
        assert!(contains(&first.frame, "paint-first"));
        assert!(!contains(&first.frame, "paint-second"));
        let (second, stale, _delivery) = receive();
        assert!(!stale);
        assert!(contains(&second.frame, "paint-second"));

        owner.set_live_viewport(false);
        send("hidden");
        assert!(owner.lock().unwrap().captured_viewport.is_none());
        let (explicit, stale, _delivery) = receive();
        assert!(!stale);
        assert!(contains(&explicit.frame, "paint-hidden"));
    }
}
