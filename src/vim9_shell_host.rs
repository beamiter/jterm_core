//! Exact-argv handoff to the GUI that owns the invoking shell. This private
//! local socket is separate from terminal output and from Vim's bridge.
use super::{shell_vim_request, Vim9EntryRequest};
use std::ffi::OsString;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

const MAX_REQUEST: u64 = 1024 * 1024;

#[derive(serde::Serialize, serde::Deserialize)]
struct Launch {
    token: String,
    cwd: PathBuf,
    argv: Vec<OsString>,
    tty: PathBuf,
}

/// Dropping an unhandled request releases its shell with a failure status.
/// Completion never closes the host GUI, and a refused :quit does not complete.
#[derive(Debug)]
pub struct Vim9ShellCompletion(Option<UnixStream>);
impl Vim9ShellCompletion {
    pub fn finish(mut self, status: i32) {
        if let Some(mut stream) = self.0.take() {
            let _ = writeln!(stream, "{}", status.clamp(0, 255));
        }
    }
}
impl Drop for Vim9ShellCompletion {
    fn drop(&mut self) {
        if let Some(mut stream) = self.0.take() {
            let _ = stream.write_all(b"1\n");
        }
    }
}

#[derive(Debug)]
pub struct Vim9ShellInvocation {
    pub request: Vim9EntryRequest,
    pub completion: Vim9ShellCompletion,
    tty: PathBuf,
}
impl Vim9ShellInvocation {
    pub fn matches_pid(&self, pid: i32) -> bool {
        pid > 0 && std::fs::read_link(format!("/proc/{pid}/fd/0")).is_ok_and(|tty| tty == self.tty)
    }

    /// Identify the source pane from its PTY, even if focus changed while the
    /// request was travelling. Never infer ownership from cwd or window titles.
    pub fn matches_pty(&self, master_fd: std::os::fd::RawFd) -> bool {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            let mut number: libc::c_uint = 0;
            // Query a Unix PTY master without changing terminal state.
            (unsafe { libc::ioctl(master_fd, libc::TIOCGPTN, &mut number) == 0 })
                && self.tty == PathBuf::from(format!("/dev/pts/{number}"))
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            let _ = master_fd;
            false
        }
    }
}

struct Host {
    environment: Vec<(String, String)>,
    pending: Mutex<crossbeam_channel::Receiver<Vim9ShellInvocation>>,
}
static HOST: OnceLock<io::Result<Host>> = OnceLock::new();

pub(super) fn host_environment() -> io::Result<Vec<(String, String)>> {
    match HOST.get_or_init(start_host) {
        Ok(host) => Ok(host.environment.clone()),
        Err(error) => Err(io::Error::new(error.kind(), error.to_string())),
    }
}

fn start_host() -> io::Result<Host> {
    let directory = std::env::temp_dir().join(format!("jterm-vim-{}", uuid::Uuid::new_v4()));
    std::fs::DirBuilder::new().mode(0o700).create(&directory)?;
    let socket = directory.join("host.sock");
    let listener = UnixListener::bind(&socket)?;
    let token = uuid::Uuid::new_v4().to_string();
    let credential = token.clone();
    let (sender, receiver) = crossbeam_channel::bounded(16);
    std::thread::Builder::new()
        .name("vim9-shell-host".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                // Read only one bounded request, with a deadline. No GUI objects or
                // shell command strings cross this worker boundary.
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let result = read_launch(&stream, &credential);
                let _ = stream.set_read_timeout(None);
                if let Ok((request, tty)) = result {
                    let invocation = Vim9ShellInvocation {
                        request,
                        tty,
                        completion: Vim9ShellCompletion(Some(stream)),
                    };
                    let _ = sender.try_send(invocation);
                }
            }
        })?;
    Ok(Host {
        environment: vec![
            (
                "JTERM_VIM9_HOST_SOCKET".into(),
                socket.to_string_lossy().into_owned(),
            ),
            ("JTERM_VIM9_HOST_TOKEN".into(), token),
        ],
        pending: Mutex::new(receiver),
    })
}

fn read_launch(stream: &UnixStream, token: &str) -> io::Result<(Vim9EntryRequest, PathBuf)> {
    let mut line = String::new();
    BufReader::new(stream.take(MAX_REQUEST + 1)).read_line(&mut line)?;
    if line.len() as u64 > MAX_REQUEST || !line.ends_with('\n') {
        return Err(io::Error::other("Invalid Vim launch size"));
    }
    let launch: Launch = serde_json::from_str(&line)?;
    if launch.token != token || !launch.tty.starts_with("/dev/pts") {
        return Err(io::Error::other(
            "Invalid Vim launch credential or terminal",
        ));
    }
    let mut request = shell_vim_request(&launch.argv)
        .map_err(io::Error::other)?
        .ok_or_else(|| io::Error::other("Expected exact shell Vim argv"))?;
    request.shell_cwd(launch.cwd).map_err(io::Error::other)?;
    Ok((request, launch.tty))
}

pub fn take_shell_vim_requests() -> Vec<Vim9ShellInvocation> {
    match HOST.get() {
        Some(Ok(host)) => host
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .try_iter()
            .collect(),
        _ => Vec::new(),
    }
}

/// Call before toolkit/window initialization. None keeps the standalone CLI
/// path; Some waits for the owning GUI and returns the real editor exit code.
/// Once handed off, transport failure is an error, never a duplicate editor.
pub fn forward_shell_vim(args: &[OsString]) -> Option<Result<i32, String>> {
    if args.first().is_none_or(|arg| arg != "--vim-shell") {
        return None;
    }
    let socket = std::env::var_os("JTERM_VIM9_HOST_SOCKET")?;
    let token = std::env::var("JTERM_VIM9_HOST_TOKEN").ok()?;
    Some(
        (|| -> io::Result<i32> {
            // Validate locally before connecting; the host repeats this validation.
            shell_vim_request(args).map_err(io::Error::other)?;
            let tty = std::fs::read_link("/proc/self/fd/0")?;
            let mut stream = UnixStream::connect(socket)?;
            stream.set_write_timeout(Some(Duration::from_secs(2)))?;
            let launch = Launch {
                token,
                cwd: std::env::current_dir()?,
                argv: args.to_vec(),
                tty,
            };
            let mut bytes = serde_json::to_vec(&launch)?;
            bytes.push(b'\n');
            if bytes.len() as u64 > MAX_REQUEST {
                return Err(io::Error::other("Vim launch exceeds size limit"));
            }
            stream.write_all(&bytes)?;
            let mut status = String::new();
            BufReader::new(stream.take(16)).read_line(&mut status)?;
            status
                .trim()
                .parse::<i32>()
                .ok()
                .filter(|code| (0..=255).contains(code))
                .ok_or_else(|| io::Error::other("Vim host disconnected before editor exit"))
        })()
        .map_err(|error| error.to_string()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    #[test]
    fn authenticated_handoff_preserves_cwd_argv_pty_and_exit_status() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let launch = Launch {
            token: "private".into(),
            cwd: std::env::temp_dir(),
            argv: ["--vim-shell", "/bin/sh", "--", "-d", "+12", "空 格'|$().rs"]
                .map(Into::into)
                .to_vec(),
            tty: "/dev/pts/2".into(),
        };
        writeln!(client, "{}", serde_json::to_string(&launch).unwrap()).unwrap();
        let (request, tty) = read_launch(&server, "private").unwrap();
        assert!(request.is_shell_command());
        assert_eq!(request.target(), std::env::temp_dir());
        let invocation = Vim9ShellInvocation {
            request,
            tty,
            completion: Vim9ShellCompletion(Some(server)),
        };
        assert!(!invocation.matches_pty(client.as_raw_fd()));
        let (_, _, argv) = invocation.request.into_parts();
        assert_eq!(argv.unwrap().1, ["-d", "+12", "空 格'|$().rs"]);
        invocation.completion.finish(7);
        let mut status = String::new();
        BufReader::new(client).read_line(&mut status).unwrap();
        assert_eq!(status, "7\n");
    }

    #[test]
    fn output_or_wrong_credentials_cannot_mint_an_editor_request() {
        for (token, argv) in [
            ("wrong", vec!["--vim-shell", "/bin/sh", "--"]),
            ("private", vec!["vim", "file"]),
        ] {
            let (mut client, server) = UnixStream::pair().unwrap();
            let launch = Launch {
                token: token.into(),
                cwd: std::env::temp_dir(),
                argv: argv.into_iter().map(Into::into).collect(),
                tty: "/dev/pts/1".into(),
            };
            writeln!(client, "{}", serde_json::to_string(&launch).unwrap()).unwrap();
            assert!(read_launch(&server, "private").is_err());
        }
    }

    #[test]
    fn abandoned_request_releases_waiting_shell_with_failure() {
        let (client, server) = UnixStream::pair().unwrap();
        drop(Vim9ShellCompletion(Some(server)));
        let mut status = String::new();
        BufReader::new(client).read_line(&mut status).unwrap();
        assert_eq!(status, "1\n");
    }
}
