//! Human-readable interpretation of shell exit codes.

/// Shell convention: exit code 128+n means the process died from signal n.
/// Name the signals a terminal user actually meets so "exit:130" reads as
/// Ctrl-C and "exit:137" as the OOM killer at a glance.
pub fn signal_name_for_exit(exit_code: i32) -> Option<&'static str> {
    match exit_code.checked_sub(128)? {
        1 => Some("SIGHUP"),
        2 => Some("SIGINT"),
        3 => Some("SIGQUIT"),
        4 => Some("SIGILL"),
        5 => Some("SIGTRAP"),
        6 => Some("SIGABRT"),
        7 => Some("SIGBUS"),
        8 => Some("SIGFPE"),
        9 => Some("SIGKILL"),
        10 => Some("SIGUSR1"),
        11 => Some("SIGSEGV"),
        12 => Some("SIGUSR2"),
        13 => Some("SIGPIPE"),
        14 => Some("SIGALRM"),
        15 => Some("SIGTERM"),
        24 => Some("SIGXCPU"),
        25 => Some("SIGXFSZ"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::signal_name_for_exit;

    #[test]
    fn names_common_fatal_signals() {
        assert_eq!(signal_name_for_exit(130), Some("SIGINT"));
        assert_eq!(signal_name_for_exit(137), Some("SIGKILL"));
        assert_eq!(signal_name_for_exit(139), Some("SIGSEGV"));
        assert_eq!(signal_name_for_exit(143), Some("SIGTERM"));
    }

    #[test]
    fn plain_exit_codes_have_no_signal_name() {
        for plain in [0, 1, 2, 100, 127, 128, 255] {
            assert_eq!(signal_name_for_exit(plain), None, "code {plain}");
        }
    }
}
