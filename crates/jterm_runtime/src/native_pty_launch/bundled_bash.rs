//! Linux bundled Bash policy for an authenticated shell integration.

use super::{launch_with_inherited_fds, PendingPty, PtyLaunch, PtyLaunchError, WorkingDirectory};
use crate::{pty_reader::move_above_stdio, shell_identity::ShellIdentityChannel};
use std::{
    ffi::{CString, OsString},
    fs::File,
    io::{self, Seek, Write},
    os::{
        fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::ffi::OsStrExt,
    },
    path::Path,
};

pub(crate) struct BundledBash<'a> {
    pub(crate) executable: &'a Path,
    /// The caller supplies its embedded bundled startup script.
    pub(crate) startup_script: &'a [u8],
    pub(crate) cwd: &'a Path,
    pub(crate) environment: &'a [(OsString, OsString)],
    pub(crate) identity_fd_key: &'static str,
    pub(crate) identity_token_key: &'static str,
    pub(crate) rows: u16,
    pub(crate) columns: u16,
}

pub(crate) fn launch_bundled_bash(spec: BundledBash<'_>) -> Result<PendingPty, PtyLaunchError> {
    if !matches!(
        spec.executable.as_os_str().as_bytes(),
        b"/bin/bash" | b"/usr/bin/bash"
    ) || spec.startup_script.is_empty()
        || !spec.cwd.is_absolute()
        || spec.rows == 0
        || spec.columns == 0
    {
        return Err(
            io::Error::new(io::ErrorKind::InvalidInput, "invalid bundled Bash launch").into(),
        );
    }
    let executable =
        CString::new(spec.executable.as_os_str().as_bytes()).map_err(io::Error::from)?;
    let script_file = sealed_bash_script(spec.startup_script)?;
    let script = CString::new(format!("/proc/self/fd/{}", script_file.as_raw_fd()))
        .map_err(io::Error::from)?;
    let cwd = CString::new(spec.cwd.as_os_str().as_bytes()).map_err(io::Error::from)?;
    let argv = [
        executable.clone(),
        c"--noprofile".to_owned(),
        c"--rcfile".to_owned(),
        script,
        c"-i".to_owned(),
    ];

    let (token, token_reader, token_writer) = ShellIdentityChannel::allocate()?
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "shell identity entropy unavailable",
            )
        })?
        .into_parts();
    let environment = prepare_environment(&spec, token_reader.as_raw_fd())?;
    let pending = launch_with_inherited_fds(
        PtyLaunch {
            executable: &executable,
            argv: &argv,
            environment: &environment,
            directory: WorkingDirectory::Path(&cwd),
            rows: spec.rows,
            columns: spec.columns,
            parent_death_signal: false,
        },
        super::ChildDescriptors {
            inherited: &[token_reader.as_fd(), script_file.as_fd()],
            close_in_child: Some(token_writer.as_fd()),
            ..super::ChildDescriptors::default()
        },
        None,
        None,
    )?;
    drop(script_file);
    drop(token_reader);
    let mut writer = File::from(token_writer);
    writeln!(writer, "{token}").map_err(PtyLaunchError::ExecConfirmed)?;
    Ok(pending)
}

fn sealed_bash_script(script: &[u8]) -> io::Result<OwnedFd> {
    // SAFETY: the name is NUL-terminated and the call returns a new descriptor.
    let fd = unsafe {
        libc::memfd_create(
            c"jterm-bundled-bash".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = move_above_stdio(unsafe { OwnedFd::from_raw_fd(fd) })?;
    let descriptor = fd.as_raw_fd();
    let mut file = File::from(fd);
    // Bash already opened the procfd path before interpreting this line.
    writeln!(file, "exec {descriptor}<&-")?;
    file.write_all(script)?;
    file.rewind()?;
    let seals = libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE;
    // SAFETY: the file owns this memfd. Sealing prevents mutation before exec.
    if unsafe { libc::fcntl(descriptor, libc::F_ADD_SEALS, seals) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(file.into())
}

fn prepare_environment(spec: &BundledBash<'_>, token_read_fd: RawFd) -> io::Result<Vec<CString>> {
    let mut environment = Vec::with_capacity(spec.environment.len() + 1);
    for (key, value) in spec.environment {
        let key = key.as_os_str().as_bytes();
        if key == spec.identity_fd_key.as_bytes() || key == spec.identity_token_key.as_bytes() {
            continue;
        }
        if key.is_empty() || key.contains(&b'=') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid shell environment key",
            ));
        }
        let mut entry = key.to_vec();
        entry.push(b'=');
        entry.extend_from_slice(value.as_os_str().as_bytes());
        environment.push(CString::new(entry)?);
    }
    environment.push(CString::new(format!(
        "{}={token_read_fd}",
        spec.identity_fd_key
    ))?);
    Ok(environment)
}
