//! Read-only, bounded process argv observation. No launch or signal authority.
use std::io::Read;

const MAX_PROC_CMDLINE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_PROC_ARG_COUNT: usize = 4 * 1024;

/// Read `/proc/<pid>/cmdline` as a NUL-separated, strictly UTF-8 argv vector.
///
/// The probe rejects nonpositive PIDs, over-budget data, and an empty `argv[0]`.
/// It does not establish identity, observation freshness, or permission to
/// signal or replay anything. Consumers still apply their own policy.
pub fn read_proc_cmdline(pid: i32) -> Option<Vec<String>> {
    if pid <= 0 {
        return None;
    }
    let file = std::fs::File::open(format!("/proc/{pid}/cmdline")).ok()?;
    read_cmdline_from(file)
}

fn read_cmdline_from(reader: impl Read) -> Option<Vec<String>> {
    let mut raw = Vec::new();
    reader
        .take(MAX_PROC_CMDLINE_BYTES + 1)
        .read_to_end(&mut raw)
        .ok()?;
    if raw.len() as u64 > MAX_PROC_CMDLINE_BYTES {
        return None;
    }
    parse_proc_cmdline_bytes(&raw)
}

fn parse_proc_cmdline_bytes(raw: &[u8]) -> Option<Vec<String>> {
    if raw.is_empty() {
        return None;
    }
    // Procfs terminates every argv element with NUL. Remove exactly the final
    // delimiter, not every empty field: an empty argument is a real boundary
    // that session restoration must preserve.
    let body = raw.strip_suffix(&[0]).unwrap_or(raw);
    let mut args = Vec::new();
    for bytes in body.split(|byte| *byte == 0) {
        if args.len() >= MAX_PROC_ARG_COUNT {
            return None;
        }
        args.push(std::str::from_utf8(bytes).ok()?.to_string());
    }
    if args.first().is_none_or(String::is_empty) {
        None
    } else {
        Some(args)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn boundaries_and_invalid_utf8_are_not_reinterpreted() {
        for raw in [
            b"tool\0empty follows\0\0tail\0".as_slice(),
            b"tool\0empty follows\0\0tail",
        ] {
            assert_eq!(
                parse_proc_cmdline_bytes(raw).unwrap(),
                ["tool", "empty follows", "", "tail"].map(str::to_owned)
            );
        }
        assert_eq!(
            parse_proc_cmdline_bytes(b"tool\0\0").unwrap(),
            ["tool", ""].map(str::to_owned)
        );
        for invalid in [
            b"".as_slice(),
            b"\0",
            b"\0argv0-empty\0",
            b"tool\0bad\xff\0",
        ] {
            assert_eq!(parse_proc_cmdline_bytes(invalid), None);
        }
    }

    #[test]
    fn argv_count_accepts_exact_budget_and_rejects_one_more() {
        let mut raw = b"tool\0".to_vec();
        raw.resize(raw.len() + MAX_PROC_ARG_COUNT - 1, 0);
        let args = parse_proc_cmdline_bytes(&raw).unwrap();
        assert_eq!(args.len(), MAX_PROC_ARG_COUNT);
        assert_eq!(args[0], "tool");
        assert!(args[1..].iter().all(String::is_empty));
        raw.push(0);
        assert_eq!(parse_proc_cmdline_bytes(&raw), None);
    }

    #[test]
    fn byte_probe_reads_at_most_one_sentinel_byte_and_keeps_exact_budget() {
        let budget = usize::try_from(MAX_PROC_CMDLINE_BYTES).unwrap();
        assert_eq!(
            read_cmdline_from(Cursor::new(vec![b'x'; budget])).unwrap()[0].len(),
            budget
        );
        let mut over = Cursor::new(vec![b'x'; budget + 17]);
        assert_eq!(read_cmdline_from(&mut over), None);
        assert_eq!(over.position(), MAX_PROC_CMDLINE_BYTES + 1);
    }

    #[test]
    fn invalid_pids_are_rejected() {
        for pid in [0, -1, i32::MIN] {
            assert!(read_proc_cmdline(pid).is_none());
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn owned_child_argv_is_structured_and_lossless_or_rejected() {
        use std::{
            ffi::OsStr,
            os::unix::{ffi::OsStrExt, process::CommandExt},
            process::{Command, Stdio},
        };
        fn inspect(value: &OsStr) -> Option<Vec<String>> {
            // Fixed inert command; the extra argument is argv data, never
            // interpreted by the shell. stdin stays open until after reading.
            const SCRIPT: &str = "printf ready; IFS= read -r line";
            let mut child = Command::new("/bin/sh")
                .env_clear()
                .arg0("owned-runtime-fixture")
                .args(["-c", SCRIPT, "placeholder", ""])
                .arg(value)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            let mut readiness = [0; 5];
            let ready = child.stdout.take().unwrap().read_exact(&mut readiness);
            let args = read_proc_cmdline(i32::try_from(child.id()).unwrap());
            drop(child.stdin.take());
            child.wait().unwrap();
            ready.unwrap();
            assert_eq!(&readiness, b"ready");
            args
        }
        let harmless = "two words ' ; $ !";
        assert_eq!(
            inspect(OsStr::new(harmless)).unwrap(),
            [
                "owned-runtime-fixture",
                "-c",
                "printf ready; IFS= read -r line",
                "placeholder",
                "",
                harmless
            ]
            .map(str::to_owned)
        );
        assert_eq!(inspect(OsStr::from_bytes(b"lossy\xff")), None);
    }
}
