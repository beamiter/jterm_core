//! Bounded, separately captured output from a supervised piped process.

use crate::supervised::SupervisedChild;
use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const POLL_DELAY: Duration = Duration::from_millis(10);
const READ_SIZE: usize = 8 * 1024;
const READS_PER_TICK: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobOutcome {
    Running,
    Exited {
        code: Option<i32>,
        signal: Option<i32>,
    },
    Stopped {
        code: Option<i32>,
        signal: Option<i32>,
    },
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct JobSnapshot {
    pub revision: u64,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub truncated: bool,
    pub outcome: JobOutcome,
}

struct JobState {
    snapshot: Mutex<JobSnapshot>,
    stop: AtomicBool,
    output_limit: usize,
}

/// Owns one non-interactive process group and its bounded output. Dropping the
/// handle stops and joins the worker, including on application shutdown.
pub struct JobHandle {
    state: Arc<JobState>,
    worker: Option<JoinHandle<()>>,
}

impl JobHandle {
    pub fn start(mut command: Command, input: Vec<u8>, output_limit: usize) -> io::Result<Self> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = SupervisedChild::spawn(&mut command)?;
        let stdin = child
            .take_stdin()
            .ok_or_else(|| io::Error::other("missing stdin"))?;
        let mut stdout = child
            .take_stdout()
            .ok_or_else(|| io::Error::other("missing stdout"))?;
        let mut stderr = child
            .take_stderr()
            .ok_or_else(|| io::Error::other("missing stderr"))?;
        nonblocking(&stdin)?;
        nonblocking(&stdout)?;
        nonblocking(&stderr)?;
        let state = Arc::new(JobState {
            snapshot: Mutex::new(JobSnapshot {
                revision: 0,
                stdout: Vec::new(),
                stderr: Vec::new(),
                truncated: false,
                outcome: JobOutcome::Running,
            }),
            stop: AtomicBool::new(false),
            output_limit,
        });
        let shared = state.clone();
        let worker = thread::Builder::new()
            .name("jterm-pipe-job".into())
            .spawn(move || {
                let outcome = run_job(&mut child, stdin, &mut stdout, &mut stderr, &input, &shared)
                    .unwrap_or_else(|error| JobOutcome::Failed(error.to_string()));
                // Complete cleanup before publishing the terminal receipt, also
                // on read/probe failure. Later runs can now safely use the slot.
                drop(child);
                let mut snapshot = shared.snapshot.lock().unwrap();
                snapshot.outcome = outcome;
                snapshot.revision += 1;
            })?;
        Ok(Self {
            state,
            worker: Some(worker),
        })
    }

    pub fn snapshot(&self) -> JobSnapshot {
        self.state.snapshot.lock().unwrap().clone()
    }

    pub fn snapshot_if_newer(&self, revision: u64) -> Option<JobSnapshot> {
        let snapshot = self.state.snapshot.lock().unwrap();
        (snapshot.revision > revision).then(|| snapshot.clone())
    }

    pub fn is_running(&self) -> bool {
        self.state.snapshot.lock().unwrap().outcome == JobOutcome::Running
    }

    pub fn stop(&self) {
        self.state.stop.store(true, Ordering::Release);
    }
}

impl Drop for JobHandle {
    fn drop(&mut self) {
        self.stop();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn nonblocking(fd: &impl AsFd) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let fd = fd.as_fd().as_raw_fd();
    // SAFETY: the borrowed descriptor remains live, and these requests only
    // read or update its file status flags.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn run_job(
    child: &mut SupervisedChild,
    stdin: impl Write,
    stdout: &mut impl Read,
    stderr: &mut impl Read,
    mut input: &[u8],
    state: &JobState,
) -> io::Result<JobOutcome> {
    // This is an owned pipe. Dropping it after the final write gives the
    // interpreter a real EOF with ordinary non-interactive stdin semantics.
    let mut stdin = Some(stdin);
    let mut stdout_eof = false;
    let mut stderr_eof = false;
    loop {
        drain(stdout, &mut stdout_eof, false, state)?;
        drain(stderr, &mut stderr_eof, true, state)?;
        let exited = child.root_has_exited()?;
        if exited || state.stop.load(Ordering::Acquire) {
            let status = child.reap_after_group_kill()?;
            // The root has been reaped. Drain buffered output through the
            // first pending read or until the shared budget is exceeded.
            // An escaped writer that kept a pipe open cannot keep us here.
            loop {
                let more_stdout = drain(stdout, &mut stdout_eof, false, state)?;
                let more_stderr = drain(stderr, &mut stderr_eof, true, state)?;
                if (!more_stdout && !more_stderr) || state.snapshot.lock().unwrap().truncated {
                    break;
                }
            }
            return Ok(outcome(status, !exited));
        }
        if let Some(pipe) = &mut stdin {
            if input.is_empty() {
                stdin.take();
            } else {
                match pipe.write(input) {
                    Ok(count) => input = &input[count..],
                    Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {
                        stdin.take();
                    }
                    Err(error) if is_pending(&error) => {}
                    Err(error) => return Err(error),
                }
            }
        }
        thread::sleep(POLL_DELAY);
    }
}

fn is_pending(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
    )
}

fn drain(pipe: &mut impl Read, eof: &mut bool, stderr: bool, state: &JobState) -> io::Result<bool> {
    if *eof {
        return Ok(false);
    }
    let mut buffer = [0; READ_SIZE];
    for _ in 0..READS_PER_TICK {
        let bytes = match pipe.read(&mut buffer) {
            Ok(0) => {
                *eof = true;
                return Ok(false);
            }
            Ok(count) => &buffer[..count],
            Err(error) if is_pending(&error) => return Ok(false),
            Err(error) => return Err(error),
        };
        let mut snapshot = state.snapshot.lock().unwrap();
        let remaining = state.output_limit - snapshot.stdout.len() - snapshot.stderr.len();
        let retained = bytes.len().min(remaining);
        if retained > 0 {
            let stream = if stderr {
                &mut snapshot.stderr
            } else {
                &mut snapshot.stdout
            };
            stream.extend_from_slice(&bytes[..retained]);
        }
        if retained > 0 || (!snapshot.truncated && retained < bytes.len()) {
            snapshot.revision += 1;
        }
        snapshot.truncated |= retained < bytes.len();
    }
    Ok(true)
}

fn outcome(status: ExitStatus, stopped: bool) -> JobOutcome {
    let (code, signal) = (status.code(), status.signal());
    if stopped {
        JobOutcome::Stopped { code, signal }
    } else {
        JobOutcome::Exited { code, signal }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn wait_for(job: &JobHandle, predicate: impl Fn(&JobSnapshot) -> bool) -> JobSnapshot {
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            let snapshot = job.snapshot();
            if predicate(&snapshot) {
                return snapshot;
            }
            assert!(Instant::now() < until, "last receipt: {snapshot:?}");
            thread::sleep(POLL_DELAY);
        }
    }

    #[test]
    fn separates_streams_preserves_exit_and_closes_stdin() {
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "read -r line; printf '%s' \"$line\"; cat >/dev/null; printf 'err' >&2; exit 17",
        ]);
        let job = JobHandle::start(command, b"out\n".to_vec(), 1024).unwrap();
        let result = wait_for(&job, |snapshot| snapshot.outcome != JobOutcome::Running);
        assert_eq!(result.stdout, b"out");
        assert_eq!(result.stderr, b"err");
        assert_eq!(
            result.outcome,
            JobOutcome::Exited {
                code: Some(17),
                signal: None
            }
        );
        assert!(job.snapshot_if_newer(result.revision).is_none());
    }

    #[test]
    fn stop_preserves_partial_output_and_cancels_group() {
        let output_dir =
            std::env::temp_dir().join(format!("jterm-pipe-job-{}", std::process::id()));
        std::fs::create_dir(&output_dir).unwrap();
        let mut command = Command::new("/bin/sh");
        command.current_dir(&output_dir).args([
            "-c",
            "(sleep 1; touch escaped) & printf ready; printf warning >&2; wait",
        ]);
        let job = JobHandle::start(command, Vec::new(), 1024).unwrap();
        wait_for(&job, |snapshot| {
            snapshot.stdout == b"ready" && snapshot.stderr == b"warning"
        });
        job.stop();
        let result = wait_for(&job, |snapshot| snapshot.outcome != JobOutcome::Running);
        let child_claimed_later_write = output_dir.join("escaped");
        thread::sleep(Duration::from_millis(1100));
        let escaped = child_claimed_later_write.exists();
        std::fs::remove_dir_all(output_dir).unwrap();
        assert!(!escaped);
        assert_eq!(result.stdout, b"ready");
        assert_eq!(result.stderr, b"warning");
        assert_eq!(
            result.outcome,
            JobOutcome::Stopped {
                code: None,
                signal: Some(libc::SIGKILL)
            }
        );
    }

    #[test]
    fn output_budget_does_not_block_a_flood_or_root_reap() {
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "head -c 131072 /dev/zero; head -c 131072 /dev/zero >&2",
        ]);
        let job = JobHandle::start(command, Vec::new(), 513).unwrap();
        let result = wait_for(&job, |snapshot| snapshot.outcome != JobOutcome::Running);
        assert_eq!(result.stdout.len() + result.stderr.len(), 513);
        assert!(result.truncated);
        assert_eq!(
            result.outcome,
            JobOutcome::Exited {
                code: Some(0),
                signal: None
            }
        );
    }
}
