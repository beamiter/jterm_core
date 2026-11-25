//! Command metadata preparation for Session Host subprocesses.

use super::{
    descriptor_ceiling, launch_with_inherited_fds, PtyLaunch, PtyLaunchError, WorkingDirectory,
};
use crate::supervised::SupervisedChild;
use std::{
    collections::BTreeMap,
    ffi::{CString, OsStr},
    io,
    os::{
        fd::{AsRawFd, BorrowedFd, OwnedFd},
        unix::ffi::{OsStrExt, OsStringExt},
    },
    process::Command,
};

/// Start a hosted PTY from the program, arguments, environment overrides, and
/// cwd of a metadata-only Command. The parent's environment is inherited.
/// An explicit directory descriptor overrides the cwd. Only the listed
/// descriptors survive exec above standard I/O. The returned child owns wait.
pub fn launch_hosted_pty(
    command: &Command,
    pinned_directory: Option<BorrowedFd<'_>>,
    rows: u16,
    columns: u16,
    inherited: &[BorrowedFd<'_>],
) -> Result<(OwnedFd, SupervisedChild), PtyLaunchError> {
    if inherited.iter().any(|fd| {
        fd.as_raw_fd() <= libc::STDERR_FILENO
            || pinned_directory.is_some_and(|directory| fd.as_raw_fd() == directory.as_raw_fd())
    }) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid inherited PTY fd").into());
    }
    let current_dir = command
        .get_current_dir()
        .filter(|_| pinned_directory.is_none())
        .map(|path| CString::new(path.as_os_str().as_bytes()))
        .transpose()
        .map_err(|error| PtyLaunchError::PrepareWorkingDirectory(error.into()))?;
    let directory = match (pinned_directory, &current_dir) {
        (Some(fd), _) => WorkingDirectory::Descriptor(fd),
        (_, Some(path)) => WorkingDirectory::Path(path),
        _ => WorkingDirectory::Inherit,
    };
    let program = command.get_program();
    if program.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty PTY program").into());
    }
    let argv = std::iter::once(program)
        .chain(command.get_args())
        .map(|arg| CString::new(arg.as_bytes()).map_err(io::Error::from))
        .collect::<io::Result<Vec<_>>>()?;

    let mut environment: BTreeMap<_, _> = std::env::vars_os().collect();
    for (key, value) in command.get_envs() {
        if let Some(value) = value {
            environment.insert(key.to_owned(), value.to_owned());
        } else {
            environment.remove(key);
        }
    }
    let candidates = if program.as_bytes().contains(&b'/') {
        vec![argv[0].clone()]
    } else {
        std::env::split_paths(
            environment
                .get(OsStr::new("PATH"))
                .map(|value| value.as_os_str())
                .unwrap_or(OsStr::new("/bin:/usr/bin")),
        )
        .map(|entry| CString::new(entry.join(program).into_os_string().into_vec()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(io::Error::from)?
    };
    let environment = environment
        .into_iter()
        .map(|(key, value)| {
            if key.is_empty() || key.as_bytes().contains(&b'=') {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid PTY environment key",
                ));
            }
            let mut entry = key.into_vec();
            entry.push(b'=');
            entry.extend_from_slice(value.as_bytes());
            CString::new(entry).map_err(io::Error::from)
        })
        .collect::<io::Result<Vec<_>>>()?;
    let pending = launch_with_inherited_fds(
        PtyLaunch {
            executable: &argv[0],
            argv: &argv,
            environment: &environment,
            directory,
            rows,
            columns,
            parent_death_signal: false,
        },
        super::ChildDescriptors {
            inherited,
            ..super::ChildDescriptors::default()
        },
        Some(descriptor_ceiling()?),
        Some(&candidates),
    )?;
    pending.adopt(|master, pid| {
        let child = SupervisedChild::adopt_pty(pid).map_err(PtyLaunchError::ExecConfirmed)?;
        Ok((master, child))
    })
}
