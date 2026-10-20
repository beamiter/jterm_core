//! Native IDE entry is an explicit, one-shot user request, never metadata.
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
        })
    }

    pub fn user_action(target: impl Into<PathBuf>) -> Self {
        Self::authorize(target, Vim9EntryTrigger::UserAction).expect("explicit IDE action")
    }

    pub fn command_line(target: impl Into<PathBuf>) -> Self {
        Self::authorize(target, Vim9EntryTrigger::CommandLine).expect("explicit --ide")
    }

    pub fn target(&self) -> &Path {
        &self.target
    }

    pub(super) fn into_parts(self) -> (uuid::Uuid, PathBuf) {
        (self.id, self.target)
    }
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
}
