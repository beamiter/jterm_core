use std::path::Path;

/// Reapply the Vim shim after Bash reads the user's interactive startup files.
pub const VIM9_SHELL_EXEC: &str = "if [ -n \"${JTERM_VIM9_SHIM_DIR-}\" ]; then export PATH=\"$JTERM_VIM9_SHIM_DIR:$PATH\"; fi; exec \"$0\" \"$@\"";

/// Recognize the owned Bash bootstrap and return its structured jsh argv.
pub fn wrapped_jsh(argv: &[String]) -> Option<(&Path, &[String])> {
    match argv {
        [shell, mode, script, program, args @ ..]
            if Path::new(shell)
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.trim_start_matches('-') == "bash")
                && mode == "-ic"
                && matches!(script.as_str(), VIM9_SHELL_EXEC | "exec \"$0\" \"$@\"")
                && Path::new(program)
                    .file_name()
                    .is_some_and(|name| name == "jsh") =>
        {
            Some((Path::new(program), args))
        }
        _ => None,
    }
}
