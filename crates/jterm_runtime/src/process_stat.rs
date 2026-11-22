//! Read-only Linux procfs process observation. No signalling or wait ownership.
use std::io;

/// Fields of `/proc/<pid>/stat` the terminals inspect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessStat {
    pub state: char,
    pub process_group: i32,
    pub session: i32,
    /// Foreground process group of the controlling terminal (`tpgid`); `-1`
    /// when the process has no controlling terminal.
    pub foreground_group: i32,
}

impl ProcessStat {
    pub fn is_live(self) -> bool {
        !matches!(self.state, 'Z' | 'X' | 'x')
    }
}

/// Parse `/proc/<pid>/stat` content. The `comm` field may contain spaces and
/// parens, so fields index from the last `)`.
pub fn parse_process_stat(contents: &str) -> Option<ProcessStat> {
    let rparen_pos = contents.rfind(')')?;
    let after_comm = &contents[rparen_pos + 1..];
    let mut fields = after_comm.split_whitespace();
    let state = fields.next()?.chars().next()?;
    fields.next()?; // parent pid
    let process_group = fields.next()?.parse().ok()?;
    let session = fields.next()?.parse().ok()?;
    fields.next()?; // tty_nr
    let foreground_group = fields.next()?.parse().ok()?;
    Some(ProcessStat {
        state,
        process_group,
        session,
        foreground_group,
    })
}

/// Read and parse `/proc/<pid>/stat`, distinguishing a vanished process
/// (`NotFound`) from an unreadable or malformed entry — the distinction a
/// PID-reuse-safe shutdown drain needs.
pub fn process_stat_result(pid: i32) -> io::Result<ProcessStat> {
    if pid <= 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "process id must be positive",
        ));
    }
    let contents = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    parse_process_stat(&contents)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "malformed proc stat"))
}

pub fn process_stat(pid: i32) -> Option<ProcessStat> {
    process_stat_result(pid).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_preserves_group_session_foreground_and_liveness() {
        let stat = parse_process_stat("123 (name with ) parens) S 1 77 88 34816 4242 0 0").unwrap();
        assert_eq!(
            stat,
            ProcessStat {
                state: 'S',
                process_group: 77,
                session: 88,
                foreground_group: 4242,
            }
        );
        assert!(stat.is_live());
        for state in ['Z', 'X', 'x'] {
            let stat = parse_process_stat(&format!("9 (z) {state} 1 9 9 0 -1 0 0")).unwrap();
            assert!(!stat.is_live());
            assert_eq!(stat.foreground_group, -1);
        }
        assert!(parse_process_stat("not proc stat").is_none());
        assert!(parse_process_stat("1 (short) S 1").is_none());
        assert!(parse_process_stat("1 (bad group) R 0 invalid 2 0 3").is_none());
    }

    #[test]
    fn invalid_pid_keeps_its_error_kind() {
        for pid in [0, -1, i32::MIN] {
            assert_eq!(
                process_stat_result(pid).unwrap_err().kind(),
                io::ErrorKind::InvalidInput,
            );
            assert_eq!(process_stat(pid), None);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn reads_this_pid_without_owning_wait_or_signal_state() {
        let pid = i32::try_from(std::process::id()).unwrap();
        let stat = process_stat_result(pid).unwrap();
        // This process cannot migrate to a different session or group here.
        let second =
            parse_process_stat(&std::fs::read_to_string("/proc/self/stat").unwrap()).unwrap();
        assert!(stat.is_live() && second.is_live());
        assert_eq!(stat.process_group, second.process_group);
        assert_eq!(stat.session, second.session);
    }
}
