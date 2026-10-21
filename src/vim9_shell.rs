//! Child-local shell integration. The GUI does not parse shell command strings
//! or trust terminal output to enter an editor. A tiny PATH shim forwards the
//! actual shell-expanded argv to the same frontend and waits for its exit.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

const SHIM: crate::vendored_script::VendoredScript = crate::vendored_script::VendoredScript {
    name: "vim",
    source: include_str!("../runtime/vim9/vim-shell.sh"),
};

/// Used only by the hosts' owned Bash -ic launcher for jsh. Bash startup files
/// can reset PATH; reapply the child-local integration after those files run.
pub const VIM9_SHELL_EXEC: &str = "if [ -n \"${JTERM_VIM9_SHIM_DIR-}\" ]; then export PATH=\"$JTERM_VIM9_SHIM_DIR:$PATH\"; fi; exec \"$0\" \"$@\"";

/// Persistent, private cache entries survive this value being dropped. A
/// terminal passes these overrides only to its interactive local shell child.
#[derive(Clone, Debug)]
pub struct Vim9ShellIntegration {
    environment: Vec<(String, String)>,
}

impl Vim9ShellIntegration {
    pub fn prepare(frontend: &Path) -> io::Result<Self> {
        let cache = dirs::cache_dir()
            .ok_or_else(|| io::Error::other("no cache directory for native Vim"))?
            .join("jterm/vim9-shell/bin");
        Self::materialize(frontend, &cache, std::env::var_os("PATH").as_deref())
    }

    fn materialize(
        frontend: &Path,
        cache: &Path,
        path: Option<&std::ffi::OsStr>,
    ) -> io::Result<Self> {
        let frontend = std::fs::canonicalize(frontend)?;
        let text = |p: &Path| {
            p.to_str().map(str::to_owned).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "native Vim shell paths must be UTF-8",
                )
            })
        };
        SHIM.materialize(cache)?;
        let cache = std::fs::canonicalize(cache)?;
        let mut paths = vec![cache.clone()];
        if let Some(path) = path {
            paths.extend(std::env::split_paths(path).filter(|entry| entry != &cache));
        }
        let path = std::env::join_paths(paths)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?
            .into_string()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "PATH must be UTF-8"))?;
        Ok(Self {
            environment: vec![
                ("PATH".into(), path),
                ("JTERM_VIM9_FRONTEND".into(), text(&frontend)?),
                ("JTERM_VIM9_SHIM_DIR".into(), text(&cache)?),
                // Bash imports this before its normal startup files. It survives
                // login profiles that reset PATH. A user's later vim alias/function
                // keeps precedence, and `command vim` remains an explicit bypass.
                (
                    "BASH_FUNC_vim%%".into(),
                    "() { \"$JTERM_VIM9_SHIM_DIR/vim\" \"$@\"; }".into(),
                ),
            ],
        })
    }

    pub fn env(&self) -> Vec<(String, String)> {
        self.environment.clone()
    }
}

/// Conservative guard shared by VTE and custom PTY hosts. An explicit `-c`,
/// script, remote command, validation subprocess or restored application must
/// not acquire PATH injection merely because its executable resembles a shell.
pub fn shell_argv_eligible(argv: &[String]) -> bool {
    let Some(program) = argv
        .first()
        .and_then(|p| Path::new(p).file_name())
        .and_then(|p| p.to_str())
    else {
        return false;
    };
    if !matches!(
        program.trim_start_matches('-'),
        "bash" | "zsh" | "fish" | "jsh" | "sh" | "dash" | "ksh" | "pwsh"
    ) {
        return false;
    }
    // The GTK hosts start jsh after the user's interactive Bash rc. Recognize
    // only this exact owned exec wrapper; arbitrary -c strings stay excluded.
    if program.trim_start_matches('-') == "bash"
        && argv.len() == 4
        && argv[1] == "-ic"
        && (argv[2] == "exec \"$0\" \"$@\"" || argv[2] == VIM9_SHELL_EXEC)
    {
        return Path::new(&argv[3])
            .file_name()
            .is_some_and(|name| name == "jsh");
    }
    if program.trim_start_matches('-') == "jsh" && argv.len() == 3 && argv[1] == "--session" {
        return !argv[2].is_empty() && !argv[2].contains('\0');
    }
    argv[1..].iter().all(|arg| {
        matches!(
            arg.as_str(),
            "-i" | "-l"
                | "-il"
                | "-li"
                | "--login"
                | "--noprofile"
                | "--norc"
                | "-NoLogo"
                | "-NoProfile"
        )
    })
}

pub(super) fn path_without_shim() -> Option<OsString> {
    let path = std::env::var_os("PATH")?;
    let Some(shim) = std::env::var_os("JTERM_VIM9_SHIM_DIR").map(PathBuf::from) else {
        return Some(path);
    };
    std::env::join_paths(std::env::split_paths(&path).filter(|entry| entry != &shim)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_interactive_local_shell_argv_gets_a_shim() {
        for words in [
            vec!["/bin/bash"],
            vec!["zsh", "-il"],
            vec!["fish", "-l"],
            vec!["jsh"],
            vec!["bash", "--noprofile", "--norc", "-i"],
        ] {
            assert!(shell_argv_eligible(
                &words.into_iter().map(str::to_owned).collect::<Vec<_>>()
            ));
        }
        for words in [
            vec![],
            vec!["bash", "-c", "vim"],
            vec!["bash", "-lic", "vim"],
            vec!["sh", "script"],
            vec!["ssh", "host"],
            vec!["vim"],
            vec!["env", "bash"],
        ] {
            assert!(!shell_argv_eligible(
                &words.into_iter().map(str::to_owned).collect::<Vec<_>>()
            ));
        }
        assert!(shell_argv_eligible(
            &[
                "/bin/bash",
                "-ic",
                "exec \"$0\" \"$@\"",
                "/usr/local/bin/jsh"
            ]
            .map(str::to_owned)
        ));
        assert!(!shell_argv_eligible(
            &[
                "/bin/bash",
                "-ic",
                "exec \"$0\" \"$@\"",
                "/usr/local/bin/vim"
            ]
            .map(str::to_owned)
        ));
        assert!(shell_argv_eligible(&[
            "/bin/bash".into(),
            "-ic".into(),
            VIM9_SHELL_EXEC.into(),
            "/usr/local/bin/jsh".into()
        ]));
    }

    #[test]
    fn shell_environment_is_child_local_durable_and_does_not_duplicate_path() {
        let directory = std::env::temp_dir().join(format!("vim-shell-{}", uuid::Uuid::new_v4()));
        let before = std::env::var_os("PATH");
        let first = Vim9ShellIntegration::materialize(
            Path::new("/bin/sh"),
            &directory,
            Some("/bin:/usr/bin".as_ref()),
        )
        .unwrap();
        let env = first.env();
        assert_eq!(std::env::var_os("PATH"), before);
        assert!(directory.join("vim").is_file());
        drop(first);
        let second = Vim9ShellIntegration::materialize(
            Path::new("/bin/sh"),
            &directory,
            Some(env[0].1.as_ref()),
        )
        .unwrap();
        assert_eq!(second.env()[0], env[0]);
        assert!(directory.join("vim").is_file());
        if Path::new("/bin/bash").is_file() {
            let output = std::process::Command::new("/bin/bash")
                .args(["--noprofile", "--norc", "-c", "PATH=/bin; type -t vim"])
                .envs(second.env())
                .output()
                .unwrap();
            assert!(output.status.success());
            assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), "function");
        }
        std::fs::remove_dir_all(directory).unwrap();
    }
}
