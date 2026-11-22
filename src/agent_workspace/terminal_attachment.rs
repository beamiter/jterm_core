//! Headless JS/GUI attach path for the existing Workspace CLI PTY owner.
use super::{
    client::connect,
    model::descriptor,
    ownership::Paths,
    protocol::{read_frame, write_frame, Request, Response, OUTPUT_CHUNK_BYTES},
    RuntimeSnapshot, RuntimeState, PROTOCOL_VERSION,
};
use crate::agent_sessions::AgentSession;
use crossbeam_channel::{bounded, Receiver, TryRecvError};
use std::{
    io::BufReader, net::Shutdown, os::unix::net::UnixStream, thread::JoinHandle, time::Duration,
};

const SNAPSHOT_BYTES: usize = 2 * 1024 * 1024;
const OUTPUT_QUEUE: usize = 64;

/// A verified event on one exact host attachment, in sequence order.
pub enum WorkspaceTerminalEvent {
    Output(Vec<u8>),
    Exited { code: Option<i32> },
}

/// The connection owns the host input lease. Dropping it releases the view
/// while the provider runtime remains under the Workspace host.
pub struct WorkspaceTerminalAttachment {
    stream: UnixStream,
    reader: Option<JoinHandle<()>>,
    pending: Receiver<Result<Response, String>>,
    client: String,
    epoch: String,
    sequence: u64,
    exited: bool,
}

impl WorkspaceTerminalAttachment {
    /// Connect on a worker; returns the host receipt and exact initial screen.
    /// The source is validated by the same path as the embedded CLI helper.
    pub fn attach(
        session: &AgentSession,
        rows: u16,
        cols: u16,
    ) -> Result<(Self, RuntimeSnapshot, Vec<u8>), String> {
        if !(1..=120).contains(&rows) || !(1..=400).contains(&cols) {
            return Err("Workspace terminal dimensions are out of bounds".into());
        }
        let mut launch = descriptor(session)?;
        let requested = launch.row.id.clone();
        launch.rows = rows;
        launch.cols = cols;
        let client = uuid::Uuid::new_v4().to_string();
        let mut stream = connect(&Paths::discover()?, true, None)?;
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .and_then(|_| stream.set_write_timeout(Some(Duration::from_secs(3))))
            .map_err(|error| error.to_string())?;
        write_frame(
            &mut stream,
            &Request::Attach {
                version: PROTOCOL_VERSION,
                launch: Box::new(launch),
                client: client.clone(),
            },
        )?;
        let mut input = BufReader::new(stream.try_clone().map_err(|error| error.to_string())?);
        let (snapshot, screen) = match read_frame::<Response>(&mut input)?
            .ok_or("Session host closed the attachment")?
        {
            Response::Attached {
                snapshot, screen, ..
            } => (snapshot, screen),
            Response::Error(error) => return Err(error),
            _ => return Err("Unexpected Workspace terminal attachment receipt".into()),
        };
        if snapshot.id != requested
            || snapshot.pid == 0
            || snapshot.state != RuntimeState::Attached
            || snapshot.rows != rows
            || snapshot.cols != cols
            || uuid::Uuid::parse_str(&snapshot.epoch).is_err()
            || screen.len() > SNAPSHOT_BYTES
        {
            return Err("Invalid or inactive Workspace terminal attachment receipt".into());
        }
        input
            .get_mut()
            .set_read_timeout(None)
            .map_err(|error| error.to_string())?;
        let (send, pending) = bounded(OUTPUT_QUEUE);
        let reader = std::thread::Builder::new()
            .name("workspace-terminal-output".into())
            .spawn(move || {
                loop {
                    let event = read_frame::<Response>(&mut input).and_then(|value| match value {
                        Some(Response::Output { bytes, .. })
                            if bytes.len() > OUTPUT_CHUNK_BYTES =>
                        {
                            Err("Workspace terminal output exceeded its chunk limit".into())
                        }
                        Some(event) => Ok(event),
                        None => Err("Workspace terminal disconnected".into()),
                    });
                    let finished = !matches!(event, Ok(Response::Output { .. }));
                    // The host owns continued drainage. If this view falls
                    // behind, close its lease and recover with a new snapshot.
                    if send.try_send(event).is_err() || finished {
                        break;
                    }
                }
                let _ = input.get_mut().shutdown(Shutdown::Both);
            })
            .map_err(|error| error.to_string())?;
        let attachment = Self {
            stream,
            reader: Some(reader),
            pending,
            client,
            epoch: snapshot.epoch.clone(),
            sequence: snapshot.event_sequence,
            exited: false,
        };
        Ok((attachment, snapshot, screen))
    }

    pub fn try_event(&mut self) -> Result<Option<WorkspaceTerminalEvent>, String> {
        if self.exited {
            return Ok(None);
        }
        let next = self
            .sequence
            .checked_add(1)
            .ok_or("Workspace sequence exhausted")?;
        let event = match self.pending.try_recv() {
            Ok(Ok(Response::Output { sequence, bytes })) if sequence == next => {
                WorkspaceTerminalEvent::Output(bytes)
            }
            Ok(Ok(Response::Exited { sequence, code })) if sequence == next => {
                self.exited = true;
                WorkspaceTerminalEvent::Exited { code }
            }
            Ok(Ok(Response::Error(error))) | Ok(Err(error)) => {
                self.disconnect();
                return Err(error);
            }
            Err(TryRecvError::Empty) => return Ok(None),
            Err(TryRecvError::Disconnected) => {
                self.disconnect();
                return Err(
                    "Workspace terminal fell behind or closed. Reattach to recover.".into(),
                );
            }
            _ => {
                self.disconnect();
                return Err(
                    "Invalid Workspace terminal event sequence. Reattach to recover.".into(),
                );
            }
        };
        self.sequence = next;
        Ok(Some(event))
    }

    /// Send one bounded operation to the actual lease. A successful send is
    /// delivery to the socket; the provider supplies application-level output.
    pub fn input(&mut self, bytes: &[u8]) -> Result<(), String> {
        if bytes.is_empty() || bytes.len() > OUTPUT_CHUNK_BYTES {
            return Err("Workspace terminal input exceeds its bound".into());
        }
        self.send(Request::Input {
            client: self.client.clone(),
            epoch: self.epoch.clone(),
            bytes: bytes.to_vec(),
        })
    }

    fn send(&mut self, request: Request) -> Result<(), String> {
        if self.exited || self.reader.as_ref().is_none_or(JoinHandle::is_finished) {
            return Err("Workspace terminal input lease is unavailable".into());
        }
        if let Err(error) = write_frame(&mut self.stream, &request) {
            self.disconnect();
            return Err(error);
        }
        Ok(())
    }

    fn disconnect(&mut self) {
        self.exited = true;
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

impl Drop for WorkspaceTerminalAttachment {
    fn drop(&mut self) {
        self.disconnect();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}
