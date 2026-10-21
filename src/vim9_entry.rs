//! Native GUI entry consumes a user action or an exact shell argv request.
//! Passive titles, process names, restored data and wire events are not intent.
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Vim9EntryTrigger {
    UserAction,
    CommandLine,
    Cwd,
    Title,
    ShellCommand,
    ForegroundProcess,
    Bridge,
    LateEvent,
    OtherTab,
    SessionRestore,
}

/// Deliberately neither Clone nor serializable. A constructor must consume a
/// request from the explicit UI/CLI boundary; restored or wire data cannot
/// manufacture one. Each new request has a different lifetime identity.
#[derive(Debug)]
#[must_use]
pub struct Vim9EntryRequest {
    id: uuid::Uuid,
    target: PathBuf,
    shell_launch: Option<(String, Vec<String>)>,
}

impl Vim9EntryRequest {
    pub fn authorize(target: impl Into<PathBuf>, trigger: Vim9EntryTrigger) -> Option<Self> {
        matches!(
            trigger,
            Vim9EntryTrigger::UserAction | Vim9EntryTrigger::CommandLine
        )
        .then(|| Self {
            id: uuid::Uuid::new_v4(),
            target: target.into(),
            shell_launch: None,
        })
    }

    pub fn user_action(target: impl Into<PathBuf>) -> Self {
        Self::authorize(target, Vim9EntryTrigger::UserAction).expect("explicit IDE action")
    }

    pub fn command_line(target: impl Into<PathBuf>) -> Self {
        Self::authorize(target, Vim9EntryTrigger::CommandLine).expect("explicit --ide")
    }

    pub(super) fn shell_cwd(&mut self, cwd: PathBuf) -> Result<(), String> {
        if !self.is_shell_command() || !cwd.is_absolute() || !cwd.is_dir() {
            return Err("Invalid shell Vim working directory".into());
        }
        self.target = cwd;
        Ok(())
    }

    pub fn target(&self) -> &Path {
        &self.target
    }

    pub fn is_shell_command(&self) -> bool {
        self.shell_launch.is_some()
    }

    pub(super) fn into_parts(self) -> (uuid::Uuid, PathBuf, Option<(String, Vec<String>)>) {
        (self.id, self.target, self.shell_launch)
    }
}

/// The managed shell shim forwards the real executable and every already
/// expanded argument separately. No shell command string is parsed or run.
/// Frontends call this before their normal CLI parser, with `argv[0]` removed.
pub fn shell_vim_request(args: &[std::ffi::OsString]) -> Result<Option<Vim9EntryRequest>, String> {
    if args.first().is_none_or(|arg| arg != "--vim-shell") {
        return Ok(None);
    }
    if args.len() < 3 || args[2] != "--" {
        return Err("Expected --vim-shell /absolute/path/to/vim -- [Vim arguments]".into());
    }
    let program = PathBuf::from(&args[1]);
    if !program.is_absolute() || !program.is_file() {
        return Err("The original Vim executable must be an existing absolute path".into());
    }
    use std::os::unix::fs::PermissionsExt;
    if program
        .metadata()
        .map_err(|e| e.to_string())?
        .permissions()
        .mode()
        & 0o111
        == 0
    {
        return Err("The original Vim program is not executable".into());
    }
    let program = program
        .to_str()
        .ok_or("Vim GUI currently requires UTF-8 executable paths")?
        .to_owned();
    let arguments = args[3..]
        .iter()
        .map(|arg| {
            arg.to_str()
                .filter(|text| !text.contains('\0'))
                .map(str::to_owned)
                .ok_or_else(|| {
                    "Vim GUI currently requires UTF-8 arguments without NUL bytes".to_owned()
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Some(Vim9EntryRequest {
        id: uuid::Uuid::new_v4(),
        target: std::env::current_dir().map_err(|e| e.to_string())?,
        shell_launch: Some((program, arguments)),
    }))
}

/// Strict standalone IDE flag grammar for frontends without a full CLI
/// parser. A flag inside an ordinary command's arguments is not IDE intent.
pub fn explicit_ide_target(args: &[std::ffi::OsString]) -> Result<Option<PathBuf>, String> {
    if args.first().is_none_or(|arg| arg != "--ide") {
        return Ok(None);
    }
    let rest = &args[1..];
    let rest = if rest.first().is_some_and(|arg| arg == "--") {
        &rest[1..]
    } else {
        if rest
            .first()
            .is_some_and(|arg| arg.to_string_lossy().starts_with('-'))
        {
            return Err("Use --ide -- PATH for a filename beginning with '-'".into());
        }
        rest
    };
    if rest.len() > 1 {
        return Err("--ide accepts one project directory or file".into());
    }
    Ok(Some(
        rest.first()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(".")),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_explicit_ui_or_cli_can_mint_entry() {
        use Vim9EntryTrigger::*;
        for trigger in [
            Cwd,
            Title,
            ShellCommand,
            ForegroundProcess,
            Bridge,
            LateEvent,
            OtherTab,
            SessionRestore,
        ] {
            for path in ["/tmp/vim", "/tmp/nvim codex", "/tmp/空 格"] {
                assert!(
                    Vim9EntryRequest::authorize(path, trigger).is_none(),
                    "{trigger:?}"
                );
            }
        }
        let first = Vim9EntryRequest::user_action("/tmp");
        let next = Vim9EntryRequest::command_line("/tmp");
        assert_ne!(first.id, next.id);
    }

    #[test]
    fn ordinary_argv_and_restored_commands_do_not_select_ide() {
        for args in [
            vec!["vim", "file"],
            vec!["--execute", "vim", "--ide"],
            vec!["--", "--ide"],
            vec!["cd", "/tmp/vim"],
        ] {
            let args = args.into_iter().map(Into::into).collect::<Vec<_>>();
            assert_eq!(explicit_ide_target(&args).unwrap(), None);
        }
        assert_eq!(
            explicit_ide_target(&["--ide".into(), "空 格".into()]).unwrap(),
            Some(PathBuf::from("空 格"))
        );
        assert!(explicit_ide_target(&["--ide".into(), "--execute".into(), "vim".into()]).is_err());
    }

    #[test]
    fn shell_entry_preserves_every_argument_and_the_actual_working_directory() {
        let request = shell_vim_request(
            &[
                "--vim-shell",
                "/bin/sh",
                "--",
                "-d",
                "+12",
                "one space.txt",
                "中'|$().rs",
                "--",
                "-es",
            ]
            .map(Into::into),
        )
        .unwrap()
        .unwrap();
        assert!(request.is_shell_command());
        assert_eq!(request.target(), std::env::current_dir().unwrap());
        let (_, _, (program, args)) = {
            let (id, cwd, launch) = request.into_parts();
            (id, cwd, launch.unwrap())
        };
        assert_eq!(program, "/bin/sh");
        assert_eq!(
            args,
            ["-d", "+12", "one space.txt", "中'|$().rs", "--", "-es"]
        );
        assert!(!Vim9EntryRequest::user_action(".").is_shell_command());
    }

    #[test]
    fn shell_entry_requires_its_exact_cli_boundary() {
        for args in [
            vec!["--execute", "--vim-shell"],
            vec!["--", "--vim-shell"],
            vec!["vim", "file"],
        ] {
            assert!(
                shell_vim_request(&args.into_iter().map(Into::into).collect::<Vec<_>>())
                    .unwrap()
                    .is_none()
            );
        }
        for args in [
            vec!["--vim-shell"],
            vec!["--vim-shell", "vim", "--"],
            vec!["--vim-shell", "/bin/sh", "file"],
            vec!["--vim-shell", "/no/such/vim", "--"],
        ] {
            assert!(
                shell_vim_request(&args.into_iter().map(Into::into).collect::<Vec<_>>()).is_err()
            );
        }
    }
}
