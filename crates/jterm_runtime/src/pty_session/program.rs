//! Prepare standalone session launch data before the fork boundary.

use crate::native_pty_launch::{PtyLaunch, WorkingDirectory};
use std::{
    collections::BTreeMap,
    ffi::{CStr, CString, OsStr, OsString},
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    ptr,
};

pub(super) struct Program {
    executable: CString,
    argv: Vec<CString>,
    environment: Vec<CString>,
    directory: CString,
}

impl Program {
    pub(super) fn interactive(
        program: PathBuf,
        overrides: &[(&str, &str)],
    ) -> Result<Self, String> {
        let mut environment: BTreeMap<_, _> = std::env::vars_os().collect();
        environment.extend(
            overrides
                .iter()
                .map(|&(key, value)| (key.into(), value.into())),
        );
        Self::new(&[program.into_os_string(), "-i".into()], None, environment)
    }

    pub(super) fn command(
        argv: &[OsString],
        cwd: &Path,
        environment: &[(OsString, OsString)],
    ) -> Result<Self, String> {
        Self::new(argv, Some(cwd), environment.iter().cloned().collect())
    }

    fn new(
        argv: &[OsString],
        cwd: Option<&Path>,
        mut environment: BTreeMap<OsString, OsString>,
    ) -> Result<Self, String> {
        let first = argv
            .first()
            .filter(|program| !program.is_empty())
            .ok_or("missing PTY command")?;
        let home = environment
            .get(OsStr::new("HOME"))
            .and_then(|path| path.to_str());
        let needs_account =
            (cwd.is_none() && home.is_none()) || !environment.contains_key(OsStr::new("SHELL"));
        let (account_home, account_shell) = if needs_account {
            account_paths()
        } else {
            (PathBuf::from("/"), OsString::from("/bin/sh"))
        };
        let directory = cwd.or_else(|| home.map(Path::new)).unwrap_or(&account_home);
        let executable = resolve(first, directory, environment.get(OsStr::new("PATH")))?;
        let directory = CString::new(directory.as_os_str().as_bytes())
            .map_err(|_| "PTY working directory contains a NUL byte")?;
        environment.entry("SHELL".into()).or_insert(account_shell);

        let argv = argv
            .iter()
            .map(|arg| CString::new(arg.as_bytes()).map_err(|_| "PTY argument contains a NUL byte"))
            .collect::<Result<Vec<_>, _>>()?;
        let environment = environment
            .into_iter()
            .map(|(key, value)| {
                if key.is_empty() || key.as_bytes().contains(&b'=') {
                    return Err("invalid PTY environment key");
                }
                let mut entry = key.into_vec();
                entry.push(b'=');
                entry.extend_from_slice(value.as_bytes());
                CString::new(entry).map_err(|_| "PTY environment contains a NUL byte")
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Self {
            executable,
            argv,
            environment,
            directory,
        })
    }

    pub(super) fn spec(&self, rows: u16, columns: u16) -> PtyLaunch<'_> {
        PtyLaunch {
            executable: &self.executable,
            argv: &self.argv,
            environment: &self.environment,
            directory: WorkingDirectory::Path(&self.directory),
            rows,
            columns,
            parent_death_signal: false,
        }
    }
}

/// Keep command resolution anchored to the child directory and its explicit PATH.
fn resolve(program: &OsStr, directory: &Path, path: Option<&OsString>) -> Result<CString, String> {
    let candidate = if program.as_bytes().contains(&b'/') {
        executable(&directory.join(program))
    } else {
        let path = path
            .map(OsString::as_os_str)
            .unwrap_or(OsStr::new("/bin:/usr/bin"));
        std::env::split_paths(path)
            .find_map(|entry| executable(&directory.join(entry).join(program)))
    };
    candidate
        .ok_or_else(|| format!("PTY executable {program:?} does not exist or is not executable"))
}

fn executable(path: &Path) -> Option<CString> {
    if !path.is_file() {
        return None;
    }
    let path = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: the candidate is NUL-terminated and stays live for access.
    (unsafe { libc::access(path.as_ptr(), libc::X_OK) } == 0).then_some(path)
}

/// Obtain the Unix account defaults without sharing getpwuid's global buffer.
fn account_paths() -> (PathBuf, OsString) {
    let mut storage = vec![0u8; 1024];
    loop {
        let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut result = ptr::null_mut();
        // SAFETY: passwd and result are writable and storage covers the given size.
        let code = unsafe {
            libc::getpwuid_r(
                libc::getuid(),
                entry.as_mut_ptr(),
                storage.as_mut_ptr().cast(),
                storage.len(),
                &mut result,
            )
        };
        if code == libc::ERANGE {
            storage.resize(storage.len() * 2, 0);
            continue;
        }
        if code == 0 && !result.is_null() {
            // SAFETY: success initialized the entry and the returned strings
            // reference storage, which stays live until after copying them.
            let entry = unsafe { entry.assume_init() };
            let home = unsafe { CStr::from_ptr(entry.pw_dir) };
            let shell = unsafe { CStr::from_ptr(entry.pw_shell) };
            let home = PathBuf::from(OsStr::from_bytes(home.to_bytes()));
            let shell = OsStr::from_bytes(shell.to_bytes());
            let shell = executable(Path::new(shell))
                .map(|_| shell.to_owned())
                .unwrap_or_else(|| "/bin/sh".into());
            return (home, shell);
        }
        return (PathBuf::from("/"), "/bin/sh".into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executable_lookup_uses_the_child_directory_and_execvp_default_path() {
        let excluded_path = OsString::from("/dev/null");
        assert_eq!(
            resolve(OsStr::new("bin/sh"), Path::new("/"), Some(&excluded_path))
                .unwrap()
                .as_bytes(),
            b"/bin/sh"
        );
        assert_eq!(
            resolve(OsStr::new("sh"), Path::new("/"), None)
                .unwrap()
                .as_bytes(),
            b"/bin/sh"
        );
        assert!(resolve(OsStr::new("sh"), Path::new("/"), Some(&excluded_path)).is_err());
    }
}
