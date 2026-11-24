use std::{
    borrow::Cow,
    ffi::{CString, OsStr, OsString},
    os::unix::ffi::OsStrExt,
    path::Path,
};

use jterm_engine::shell_markers::is_valid_jsh_session_id;
use jterm_runtime::local_shell::wrapped_jsh;

pub(crate) const RUNTIME_OVERRIDE: &str = "JTERM_SHARED_RUNTIME";

pub(crate) fn admit_configured_shell(
    argv: &[String],
    host_bridge: bool,
    runtime_override: Option<&OsStr>,
) -> bool {
    !host_bridge
        && runtime_override.is_none_or(|value| value == "1")
        && argv
            .first()
            .is_some_and(|program| Path::new(program).is_absolute() || program == "sh")
}

pub(crate) fn admit_local_argv(
    argv: &[String],
    host_bridge: bool,
    runtime_override: Option<&OsStr>,
) -> bool {
    !host_bridge
        && runtime_override.is_none_or(|value| value == "1")
        && (matches!(argv, [program, arguments @ ..]
            if matches!(program.as_str(), "/bin/sh" | "/usr/bin/sh" | "/bin/bash" | "/usr/bin/bash")
                && arguments.iter().all(|arg| matches!(arg.as_str(), "-i" | "-l")))
            || local_jsh_profile(argv).is_some())
}

fn local_jsh_profile(argv: &[String]) -> Option<(&Path, &[String])> {
    let (entry, arguments) = argv.split_first()?;
    if !Path::new(entry).is_absolute() {
        return None;
    }
    let (program, args) = wrapped_jsh(argv).unwrap_or((Path::new(entry), arguments));
    (program.is_absolute()
        && program.file_name().is_some_and(|name| name == "jsh")
        && (matches!(args, [])
            || matches!(args, [flag, id] if flag == "--session" && is_valid_jsh_session_id(id))))
    .then_some((program, args))
}

pub(crate) fn candidate_program(argv: &[String]) -> Option<&Path> {
    local_jsh_profile(argv)
        .map(|(program, _)| program)
        .or_else(|| argv.first().map(Path::new))
}

pub(crate) fn with_jsh_session<'a>(
    argv: &'a [String],
    session_id: &str,
) -> Option<Cow<'a, [String]>> {
    match local_jsh_profile(argv) {
        Some(_) if !is_valid_jsh_session_id(session_id) => None,
        Some((_, [_, existing])) if existing != session_id => None,
        Some((_, [])) => {
            let mut argv = argv.to_vec();
            argv.extend(["--session".to_owned(), session_id.to_owned()]);
            Some(Cow::Owned(argv))
        }
        _ => Some(Cow::Borrowed(argv)),
    }
}

pub(crate) fn runtime_environment(env: Vec<CString>) -> Result<Vec<(OsString, OsString)>, String> {
    let mut pairs = Vec::with_capacity(env.len());
    for entry in env {
        let bytes = entry.as_bytes();
        let boundary = bytes
            .iter()
            .position(|byte| *byte == b'=')
            .ok_or("invalid child environment entry")?;
        if &bytes[..boundary] != RUNTIME_OVERRIDE.as_bytes() {
            pairs.push((
                OsStr::from_bytes(&bytes[..boundary]).to_owned(),
                OsStr::from_bytes(&bytes[boundary + 1..]).to_owned(),
            ));
        }
    }
    Ok(pairs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_panes_accept_resolved_shells_and_keep_the_rollout_override() {
        let shell = ["/opt/configured shell/bin/fish", "--login"].map(str::to_owned);
        assert!(admit_configured_shell(&shell, false, None));
        assert!(!admit_local_argv(&shell, false, None));
        assert!(!admit_configured_shell(&shell, true, None));
        assert!(!admit_configured_shell(&shell, false, Some(OsStr::new("0"))));
        assert!(admit_configured_shell(&["sh".into()], false, None));
        assert!(!admit_configured_shell(&["./fish".into()], false, None));
        assert!(!admit_configured_shell(&[], false, None));
    }

    #[test]
    fn local_jsh_defaults_to_shared_and_preserves_session_and_exec_image() {
        use jterm_runtime::local_shell::VIM9_SHELL_EXEC;

        let program = "/absolute/path with spaces/jsh";
        let wrapped = ["/usr/bin/bash", "-ic", VIM9_SHELL_EXEC, program].map(str::to_owned);
        let direct = [program.to_owned()];
        let enabled = Some(OsStr::new("1"));
        for argv in [wrapped.as_slice(), direct.as_slice()] {
            assert!(admit_local_argv(argv, false, enabled));
            assert!(!admit_local_argv(argv, true, enabled));
            assert!(admit_local_argv(argv, false, None));
            assert!(!admit_local_argv(argv, false, Some(OsStr::new("0"))));
            let session = with_jsh_session(argv, "owned-session_4").unwrap();
            assert!(admit_local_argv(&session, false, enabled));
            assert_eq!(&session[..argv.len()], argv);
            assert_eq!(&session[argv.len()..], ["--session", "owned-session_4"]);
            assert_eq!(candidate_program(&session), Some(Path::new(program)));
            assert_eq!(
                with_jsh_session(&session, "owned-session_4").unwrap(),
                session
            );
            assert!(with_jsh_session(&session, "other").is_none());
            assert!(with_jsh_session(argv, "../other").is_none());
        }

        for argv in [
            vec!["jsh"],
            vec![program, "--session", "../other"],
            vec![program, "-c", "exit"],
            vec!["/usr/bin/bash", "-ic", VIM9_SHELL_EXEC, "./jsh"],
            vec!["/usr/bin/bash", "-ic", "exec arbitrary", program],
            vec![
                "/usr/bin/bash",
                "-ic",
                VIM9_SHELL_EXEC,
                program,
                "--version",
            ],
        ] {
            let argv = argv.into_iter().map(str::to_owned).collect::<Vec<_>>();
            assert!(!admit_local_argv(&argv, false, enabled), "{argv:?}");
        }
    }

    #[test]
    fn local_shell_profile_defaults_to_shared_unless_overridden() {
        let enabled = Some(OsStr::new("1"));
        let args = ["/bin/sh".to_owned(), "-i".to_owned(), "-l".to_owned()];
        assert!(admit_local_argv(&args, false, enabled));
        assert!(!admit_local_argv(&args, true, enabled));
        assert!(admit_local_argv(&args, false, None));
        assert!(!admit_local_argv(&args, true, None));
        assert!(!admit_local_argv(&args, false, Some(OsStr::new("0"))));
        assert!(!admit_local_argv(&args, false, Some(OsStr::new("yes"))));
        let bash = ["/usr/bin/bash".to_owned(), "-l".to_owned()];
        assert!(admit_local_argv(&bash, false, enabled));
        for refused in [
            vec![],
            vec!["sh"],
            vec!["/bin/bash", "custom-script"],
            vec!["/bin/sh", "-c", "exit"],
        ] {
            let refused = refused.into_iter().map(str::to_owned).collect::<Vec<_>>();
            assert!(!admit_local_argv(&refused, false, enabled));
            assert!(!admit_local_argv(&refused, false, None));
        }
    }

    #[test]
    fn environment_transfer_preserves_bytes_and_only_scrubs_the_route_flag() {
        let entries = [
            b"TERM=dumb".as_slice(),
            b"JTERM_SHARED_RUNTIME=1",
            b"TEMPLATE=a=b",
            b"RAW=\xff",
            b"EMPTY=",
        ];
        let pairs = runtime_environment(
            entries
                .into_iter()
                .map(|entry| CString::new(entry).unwrap())
                .collect(),
        )
        .unwrap();
        let bytes = pairs
            .iter()
            .map(|(key, value)| (key.as_bytes(), value.as_bytes()))
            .collect::<Vec<_>>();
        assert_eq!(
            bytes,
            [
                (b"TERM".as_slice(), b"dumb".as_slice()),
                (b"TEMPLATE", b"a=b"),
                (b"RAW", b"\xff"),
                (b"EMPTY", b"")
            ]
        );
        assert!(runtime_environment(vec![CString::new("malformed").unwrap()]).is_err());
    }
}
