use std::{
    ffi::{CString, OsStr, OsString},
    os::unix::ffi::OsStrExt,
};

pub(crate) const OPT_IN: &str = "JTERM_SHARED_RUNTIME";

pub(crate) fn admit_local_argv(argv: &[String], host_bridge: bool, opt_in: Option<&OsStr>) -> bool {
    !host_bridge
        && opt_in == Some(OsStr::new("1"))
        && matches!(argv, [program, arguments @ ..]
            if matches!(program.as_str(), "/bin/sh" | "/usr/bin/sh" | "/bin/bash" | "/usr/bin/bash")
                && arguments.iter().all(|arg| matches!(arg.as_str(), "-i" | "-l")))
}

pub(crate) fn runtime_environment(env: Vec<CString>) -> Result<Vec<(OsString, OsString)>, String> {
    let mut pairs = Vec::with_capacity(env.len());
    for entry in env {
        let bytes = entry.as_bytes();
        let boundary = bytes
            .iter()
            .position(|byte| *byte == b'=')
            .ok_or("invalid child environment entry")?;
        if &bytes[..boundary] != OPT_IN.as_bytes() {
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
    fn candidate_profile_requires_explicit_local_shell_admission() {
        let enabled = Some(OsStr::new("1"));
        let args = ["/bin/sh".to_owned(), "-i".to_owned(), "-l".to_owned()];
        assert!(admit_local_argv(&args, false, enabled));
        assert!(!admit_local_argv(&args, true, enabled));
        assert!(!admit_local_argv(&args, false, None));
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
