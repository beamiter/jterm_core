use std::{
    cell::{Cell, Ref, RefCell},
    ffi::{CString, OsString},
    path::{Path, PathBuf},
    rc::Rc,
};

use jterm_engine::{hyperlink::HyperlinkPolicy, terminal::Terminal};
use jterm_runtime::pty_session::Session;

use crate::pane::{INITIAL_COLUMNS, INITIAL_ROWS};

use crate::runtime_launch::{admit_local_argv, runtime_environment, OPT_IN};

/// Main-context handle for the common runtime's sole PTY and terminal owner.
/// A window can retain it past widget removal while the child is being reaped.
pub struct RuntimeShell {
    session: RefCell<Session>,
    closed: Cell<bool>,
}

impl RuntimeShell {
    pub fn spawn_candidate(
        argv: &[String],
        cwd: Option<&str>,
        host_bridge: bool,
        directory_available: fn(&str) -> bool,
        prepare_environment: impl FnOnce() -> Result<Vec<CString>, String>,
    ) -> Result<Option<Rc<Self>>, String> {
        if !admit_local_argv(argv, host_bridge, std::env::var_os(OPT_IN).as_deref()) {
            return Ok(None);
        }
        let cwd = local_directory(cwd, directory_available)?;
        let argv = argv.iter().map(OsString::from).collect::<Vec<_>>();
        let env = runtime_environment(prepare_environment()?)?;
        let session =
            Session::spawn_command(INITIAL_ROWS, INITIAL_COLUMNS, &argv, Some(&cwd), &env)?;
        Ok(Some(Self::new(session)))
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
            || std::env::var_os(OPT_IN).as_deref() != Some(std::ffi::OsStr::new("1"))
            || !matches!(program.as_str(), "/bin/bash" | "/usr/bin/bash")
        {
            return Ok(None);
        }
        let cwd = local_directory(cwd, directory_available)?
            .canonicalize()
            .map_err(|error| format!("invalid PTY working directory: {error}"))?;
        let env = runtime_environment(prepare_environment()?)?;
        let session = Session::spawn_anvil_bash(
            INITIAL_ROWS,
            INITIAL_COLUMNS,
            Path::new(program),
            startup_script,
            &cwd,
            &env,
        )?;
        Ok(Some(Self::new(session)))
    }

    fn new(session: Session) -> Rc<Self> {
        Rc::new(Self {
            session: RefCell::new(session),
            closed: Cell::new(false),
        })
    }

    pub fn child_pid(&self) -> Option<i32> {
        self.session.borrow().child_pid()
    }

    pub fn exit_code(&self) -> Option<i32> {
        self.session.borrow().exit_code()
    }

    pub fn close(&self) {
        if !self.closed.replace(true) {
            let _ = self.session.borrow_mut().close();
        }
    }

    pub fn ready_to_discard(&self) -> bool {
        let session = self.session.borrow();
        (self.closed.get() || session.is_drained()) && !session.cleanup_pending()
    }

    pub(crate) fn configure_view(&self, hyperlinks: HyperlinkPolicy) {
        let mut session = self.session.borrow_mut();
        session.enable_underline_colors();
        session.enable_hyperlinks(hyperlinks);
    }

    pub(crate) fn terminal(&self) -> Ref<'_, Terminal> {
        Ref::map(self.session.borrow(), Session::terminal)
    }

    pub(crate) fn input(&self, bytes: Vec<u8>) -> Result<(), String> {
        self.session.borrow_mut().submit_input(bytes)
    }

    pub(crate) fn resize(&self, rows: usize, columns: usize) -> Result<(), String> {
        self.session.borrow_mut().resize(rows, columns)
    }

    pub(crate) fn drive(&self) -> Result<usize, String> {
        self.session.borrow_mut().drive()
    }

    pub(crate) fn closed(&self) -> bool {
        self.closed.get()
    }

    pub(crate) fn drained(&self) -> bool {
        self.session.borrow().is_drained()
    }
}

fn local_directory(cwd: Option<&str>, available: fn(&str) -> bool) -> Result<PathBuf, String> {
    match cwd.filter(|directory| available(directory)) {
        Some(cwd) => Ok(PathBuf::from(cwd)),
        None => std::env::current_dir().map_err(|error| error.to_string()),
    }
}
