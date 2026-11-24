use std::{
    cell::OnceCell,
    ffi::{CString, OsString},
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, Mutex, MutexGuard, TryLockError, Weak},
    thread::{self, Thread},
    time::Duration,
};

use futures_channel::mpsc::{self, Receiver, Sender};
use jterm_engine::{
    colors::TerminalDefaultColors,
    history_search::{HistorySearch, HistorySearchProgress},
    hyperlink::HyperlinkPolicy,
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
    worker: OnceCell<Thread>,
}

struct State {
    session: Session,
    closed: bool,
    shutdown: bool,
    write_error_reported: bool,
    view_revision: u64,
    viewport_request: Option<ViewportRequest>,
    captured_viewport: Option<CapturedViewport>,
}

struct CapturedViewport {
    request: ViewportRequest,
    revision: u64,
    frame: Result<ViewportFrame, String>,
}

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
        let cwd = local_directory(cwd, directory_available)?
            .canonicalize()
            .map_err(|error| format!("invalid PTY working directory: {error}"))?;
        let env = runtime_environment(prepare_environment()?)?;
        let mut session = Session::spawn_anvil_bash(
            INITIAL_ROWS,
            INITIAL_COLUMNS,
            Path::new(program),
            startup_script,
            &cwd,
            &env,
        )?;
        session.enable_primary_reflow();
        Ok(Some(Self::new(session)))
    }

    fn new(session: Session) -> Rc<Self> {
        Rc::new(Self {
            state: Arc::new(Mutex::new(State {
                session,
                closed: false,
                shutdown: false,
                write_error_reported: false,
                view_revision: 0,
                viewport_request: None,
                captured_viewport: None,
            })),
            updates: Arc::new(Mutex::new(Update::default())),
            worker: OnceCell::new(),
        })
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

    pub fn exit_code(&self) -> Option<i32> {
        self.lock().ok()?.session.exit_code()
    }

    pub fn close(&self) {
        if let Ok(mut state) = self.lock() {
            state.shutdown = true;
            state.viewport_request = None;
            state.captured_viewport = None;
            state.close();
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
        // A single producer always has one reserved slot. Wakes coalesce while
        // GTK is busy because State owns the actual presentation status.
        let (updates, receiver) = mpsc::channel(0);
        let worker = thread::Builder::new()
            .name("gtk-terminal-runtime".into())
            .spawn(move || drive(owner, pending, updates))
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
        let accepted = self.lock()?.session.submit_native_paste(text)?;
        if accepted {
            if let Some(worker) = self.worker.get() {
                worker.unpark();
            }
        }
        Ok(accepted)
    }

    pub(crate) fn resize(&self, rows: usize, columns: usize) -> Result<(), String> {
        let mut state = self.lock()?;
        state.session.resize(rows, columns)?;
        state.view_revision = state.view_revision.wrapping_add(1);
        state.captured_viewport = None;
        Ok(())
    }

    pub(crate) fn set_history_row_limit(&self, rows: usize) -> Result<(), String> {
        let mut state = self.lock()?;
        state.session.set_history_row_limit(rows);
        state.view_revision = state.view_revision.wrapping_add(1);
        state.captured_viewport = None;
        Ok(())
    }

    pub(crate) fn set_color_defaults(&self, defaults: TerminalDefaultColors) -> Result<(), String> {
        let mut state = self.lock()?;
        state.session.set_color_defaults(defaults);
        state.view_revision = state.view_revision.wrapping_add(1);
        state.captured_viewport = None;
        Ok(())
    }

    /// Tick callbacks never wait for parsing or history decompression. Keep
    /// only the latest request and one immutable, coalesced response.
    pub(crate) fn poll_viewport(
        &self,
        request: ViewportRequest,
    ) -> Result<Option<(ViewportFrame, bool)>, String> {
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
            return captured
                .frame
                .map(|frame| Some((frame, captured.revision != state.view_revision)));
        }
        self.queue_viewport(&mut state, request);
        Ok(None)
    }

    pub(crate) fn prepare_viewport(&self, request: ViewportRequest) {
        if let Ok(mut state) = self.state.try_lock() {
            self.queue_viewport(&mut state, request);
        }
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

    fn capture_viewport(&mut self) {
        let Some(request) = self.viewport_request else {
            return;
        };
        let frame = match request.capture(self.session.terminal(), self.closed) {
            Ok(Some(frame)) => Ok(frame),
            Ok(None) => return,
            Err(error) => Err(error),
        };
        self.captured_viewport = Some(CapturedViewport {
            request,
            revision: self.view_revision,
            frame,
        });
        self.viewport_request = None;
    }
}

impl Drop for RuntimeShell {
    fn drop(&mut self) {
        self.close();
    }
}

fn drive(owner: Weak<Mutex<State>>, pending: Arc<Mutex<Update>>, mut notifications: Sender<()>) {
    loop {
        let (read, retry, update) = {
            let Some(owner) = owner.upgrade() else { return };
            let Ok(mut state) = owner.lock() else { return };
            if state.shutdown {
                return;
            }
            let (read, update) = state.poll_session();
            state.capture_viewport();
            (
                read,
                state.session.needs_write_retry() || state.viewport_request.is_some(),
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
        let (viewport, newer_output) = loop {
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
}
