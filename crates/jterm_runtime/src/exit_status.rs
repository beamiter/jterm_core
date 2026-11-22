//! Human-readable interpretation of shell exit codes.

/// Shell convention: exit code 128+n means the process died from signal n.
/// Name the signals a terminal user actually meets so "exit:130" reads as
/// Ctrl-C and "exit:137" as the OOM killer at a glance.
///
/// The job-control stops (19-22) are named too. A shell reports a stopped
/// foreground job the same way (bash and zsh return 128+n once `waitpid`
/// says the job stopped; jsh returns 148), so Ctrl+Z on an agent TUI
/// surfaces as "exit:148". That is a suspension the user resumes with `fg`,
/// not a death, and it must at least say SIGTSTP rather than look like a
/// bare failure.
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
        19 => Some("SIGSTOP"),
        20 => Some("SIGTSTP"),
        21 => Some("SIGTTIN"),
        22 => Some("SIGTTOU"),
        24 => Some("SIGXCPU"),
        25 => Some("SIGXFSZ"),
        _ => None,
    }
}

/// Whether `exit_code` is a shell's report of a stopped foreground job
/// (128 + SIGSTOP/SIGTSTP/SIGTTIN/SIGTTOU). The job is suspended, not dead:
/// surfaces should show it neutrally rather than as a failure.
pub const fn is_job_stop(exit_code: i32) -> bool {
    matches!(exit_code, 147..=150)
}

/// The exit codes a user causes on purpose rather than a program failing:
/// Ctrl+C (130), a closed pipe (141), a polite kill (143) and Ctrl+Z (148).
/// Returns the signal name for those, `None` for every other code. Frontends
/// treat these as "interrupted", not "failed".
pub const fn interrupt_signal(exit_code: i32) -> Option<&'static str> {
    match exit_code {
        130 => Some("SIGINT"),
        141 => Some("SIGPIPE"),
        143 => Some("SIGTERM"),
        148 => Some("SIGTSTP"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{interrupt_signal, is_job_stop, signal_name_for_exit};

    #[test]
    fn job_stops_are_the_four_stop_signals() {
        for code in 147..=150 {
            assert!(is_job_stop(code), "code {code}");
        }
        for code in [0, 1, 130, 137, 143, 146, 151] {
            assert!(!is_job_stop(code), "code {code}");
        }
    }

    #[test]
    fn interrupts_are_the_user_caused_signals() {
        assert_eq!(interrupt_signal(130), Some("SIGINT"));
        assert_eq!(interrupt_signal(141), Some("SIGPIPE"));
        assert_eq!(interrupt_signal(143), Some("SIGTERM"));
        assert_eq!(interrupt_signal(148), Some("SIGTSTP"));
        for code in [0, 1, 2, 137, 139, 147] {
            assert_eq!(interrupt_signal(code), None, "code {code}");
        }
    }

    #[test]
    fn names_common_fatal_signals() {
        assert_eq!(signal_name_for_exit(130), Some("SIGINT"));
        assert_eq!(signal_name_for_exit(137), Some("SIGKILL"));
        assert_eq!(signal_name_for_exit(139), Some("SIGSEGV"));
        assert_eq!(signal_name_for_exit(143), Some("SIGTERM"));
    }

    #[test]
    fn names_job_control_stops() {
        // Ctrl+Z on a foreground job: the shell reports 128 + SIGTSTP.
        assert_eq!(signal_name_for_exit(148), Some("SIGTSTP"));
        assert_eq!(signal_name_for_exit(147), Some("SIGSTOP"));
        assert_eq!(signal_name_for_exit(149), Some("SIGTTIN"));
        assert_eq!(signal_name_for_exit(150), Some("SIGTTOU"));
        // The Linux numbers between them that no shell user meets stay unnamed.
        for unnamed in [144, 145, 146, 151] {
            assert_eq!(signal_name_for_exit(unnamed), None, "code {unnamed}");
        }
    }

    #[test]
    fn plain_exit_codes_have_no_signal_name() {
        for plain in [0, 1, 2, 100, 127, 128, 255] {
            assert_eq!(signal_name_for_exit(plain), None, "code {plain}");
        }
    }
}
