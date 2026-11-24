//! A terminal view of the Host's canonical viewport and input encoder.

use super::{
    client::connect,
    host::{Control, RuntimeHandle, RuntimeOutput},
    model::descriptor,
    ownership::Paths,
    protocol::{
        read_frame, read_frame_bounded, write_frame, Request, Response, MAX_ATTACHMENT_FRAME_BYTES,
    },
    RuntimeSnapshot, RuntimeState, PROTOCOL_VERSION,
};
use crate::agent_sessions::AgentSession;
use crossbeam_channel::{bounded, Receiver};
use jterm_engine::{frame::CellFrame, terminal::Terminal};
use jterm_protocol::{
    engine_projection,
    terminal::{self as wire, Update, WireFrame},
};
use jterm_runtime::session_actor::{HostedPty, SessionActor};
use serde::{Deserialize, Serialize};
use std::{
    io::BufReader,
    net::Shutdown,
    os::unix::net::UnixStream,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, tag = "operation", rename_all = "snake_case")]
pub enum WorkspaceViewportOperation {
    Poll,
    Ack {
        revision: u32,
    },
    Key {
        name: String,
        shift: bool,
        alt: bool,
        ctrl: bool,
    },
    Text {
        text: String,
    },
    Paste {
        text: String,
    },
    Detach,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct WorkspaceViewportPoll {
    pub frame: Option<Update>,
    pub writable: bool,
    pub status: String,
    pub finished: bool,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkspaceViewportReply {
    Receipt,
    Polled { update: WorkspaceViewportPoll },
}

/// A serialized request channel for one attached view. The Host encodes input
/// from its live modes and sends immutable viewport frames.
pub struct WorkspaceViewportAttachment {
    stream: BufReader<UnixStream>,
    client: String,
    epoch: String,
}

impl WorkspaceViewportAttachment {
    pub fn attach(
        session: &AgentSession,
        rows: u16,
        cols: u16,
        generation: u32,
    ) -> Result<(Self, RuntimeSnapshot, WireFrame), String> {
        if generation == 0 || !(1..=120).contains(&rows) || !(1..=256).contains(&cols) {
            return Err("Workspace viewport dimensions or generation are out of bounds".into());
        }
        let mut launch = descriptor(session)?;
        let id = launch.row.id.clone();
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
            &Request::AttachViewport {
                version: PROTOCOL_VERSION,
                launch: Box::new(launch),
                client: client.clone(),
                generation,
            },
        )?;
        let mut stream = BufReader::new(stream);
        let (snapshot, frame) = match read_frame::<Response>(&mut stream)? {
            Some(Response::ViewportAttached { snapshot, frame }) => (snapshot, *frame),
            Some(Response::Error(error)) => return Err(error),
            _ => return Err("Session Host did not confirm the viewport attachment".into()),
        };
        if snapshot.id != id
            || snapshot.pid == 0
            || uuid::Uuid::parse_str(&snapshot.epoch).is_err()
            || !matches!(
                snapshot.state,
                RuntimeState::Attached | RuntimeState::Exited
            )
            || frame.session != wire_session(&snapshot)
            || frame.epoch != generation
            || frame.version != wire::VERSION
            || frame.cols != usize::from(snapshot.cols)
            || frame.rows.len() != usize::from(snapshot.rows)
            || (snapshot.state == RuntimeState::Attached
                && (snapshot.rows, snapshot.cols) != (rows, cols))
        {
            return Err("Invalid Workspace viewport attachment receipt".into());
        }
        let connection = Self {
            stream,
            client,
            epoch: snapshot.epoch.clone(),
        };
        Ok((connection, snapshot, frame))
    }

    pub fn execute(
        &mut self,
        operation: WorkspaceViewportOperation,
    ) -> Result<WorkspaceViewportReply, String> {
        let request = Request::Viewport {
            client: self.client.clone(),
            epoch: self.epoch.clone(),
            operation,
        };
        write_frame(self.stream.get_mut(), &request)?;
        match read_frame::<Response>(&mut self.stream)? {
            Some(Response::Viewport(result)) => result.map(|reply| *reply),
            Some(Response::Error(error)) => Err(error),
            _ => Err("Session Host closed the viewport operation".into()),
        }
    }
}

impl Drop for WorkspaceViewportAttachment {
    fn drop(&mut self) {
        let _ = self.stream.get_mut().shutdown(Shutdown::Both);
    }
}

fn wire_session(snapshot: &RuntimeSnapshot) -> String {
    snapshot.epoch.replace('-', "")
}

pub(super) struct ViewportDelivery {
    generation: u32,
    accepted: Option<CellFrame>,
    flight: Option<CellFrame>,
}

impl ViewportDelivery {
    pub(super) fn attach(
        generation: u32,
        terminal: &Terminal,
        snapshot: &RuntimeSnapshot,
    ) -> Result<(Self, WireFrame), String> {
        let frame = terminal.frame()?;
        let wire = engine_projection::from_engine(&frame, &wire_session(snapshot), generation)?;
        Ok((
            Self {
                generation,
                accepted: None,
                flight: Some(frame),
            },
            wire,
        ))
    }

    pub(super) fn execute(
        &mut self,
        terminal: &Terminal,
        snapshot: &RuntimeSnapshot,
        actor: &SessionActor<HostedPty>,
        operation: WorkspaceViewportOperation,
    ) -> Result<WorkspaceViewportReply, String> {
        use WorkspaceViewportOperation as Operation;
        let bytes = match operation {
            Operation::Ack { revision } => {
                let matches = |frame: &CellFrame| frame.stamp().revision == u64::from(revision);
                if self.flight.as_ref().is_some_and(matches) {
                    self.accepted = self.flight.take();
                } else if self.flight.is_some() || !self.accepted.as_ref().is_some_and(matches) {
                    return Err("Wrong Workspace terminal frame acknowledgement".into());
                }
                return Ok(WorkspaceViewportReply::Receipt);
            }
            Operation::Poll => {
                let mut frame = None;
                if self.flight.is_none() && !terminal.publication_held() {
                    let latest = terminal.frame()?;
                    if !self
                        .accepted
                        .as_ref()
                        .is_some_and(|frame| frame.stamp() == latest.stamp())
                    {
                        frame = Some(engine_projection::update(
                            self.accepted.as_ref(),
                            &latest,
                            &wire_session(snapshot),
                            self.generation,
                        )?);
                        self.flight = Some(latest);
                    }
                }
                let writable = snapshot.state != RuntimeState::Stopping
                    && self.accepted.is_some()
                    && actor.input_backlog().is_ok();
                let status = if snapshot.state == RuntimeState::Stopping {
                    "Stopping the Workspace provider PTY.".into()
                } else if writable {
                    snapshot.detail.clone()
                } else {
                    "Workspace input lease is unavailable".into()
                };
                return Ok(WorkspaceViewportReply::Polled {
                    update: WorkspaceViewportPoll {
                        frame,
                        writable,
                        status,
                        finished: false,
                    },
                });
            }
            Operation::Detach => return Ok(WorkspaceViewportReply::Receipt),
            Operation::Key {
                name,
                shift,
                alt,
                ctrl,
            } => terminal.encode_key(&name, shift, alt, ctrl)?,
            Operation::Text { text } => terminal.encode_text(&text)?,
            Operation::Paste { text } => {
                self.require_input(snapshot)?;
                return actor
                    .write_paste(terminal.encode_paste(&text)?)
                    .map(|()| WorkspaceViewportReply::Receipt)
                    .map_err(|error| error.to_string());
            }
        };
        self.require_input(snapshot)?;
        actor
            .write_sender()
            .try_send(bytes)
            .map(|()| WorkspaceViewportReply::Receipt)
            .map_err(|error| error.to_string())
    }

    fn require_input(&self, snapshot: &RuntimeSnapshot) -> Result<(), String> {
        if snapshot.state == RuntimeState::Stopping || self.accepted.is_none() {
            return Err(
                "Workspace input lease is unavailable or its initial frame is unacknowledged"
                    .into(),
            );
        }
        Ok(())
    }
}

pub(super) fn retained_viewport(
    output: &RuntimeOutput,
    snapshot: &RuntimeSnapshot,
    generation: u32,
) -> Result<WireFrame, String> {
    let frame = output
        .viewport
        .as_ref()
        .ok_or("Final viewport is unavailable")?;
    engine_projection::from_engine(frame, &wire_session(snapshot), generation)
}

pub(super) fn viewport_connection(
    mut stream: BufReader<UnixStream>,
    runtime: RuntimeHandle,
    client: String,
    generation: u32,
    events: Receiver<Response>,
    closed: Arc<AtomicBool>,
) -> Result<(), String> {
    use WorkspaceViewportOperation as Operation;
    let initial = events
        .recv_timeout(Duration::from_secs(3))
        .map_err(|_| "Session Host viewport attachment is unavailable")?;
    let mut offered = match &initial {
        Response::ViewportAttached { frame, .. } => Some(frame.revision),
        _ => return Err("Missing initial Workspace viewport".into()),
    };
    write_frame(stream.get_mut(), &initial)?;
    stream
        .get_mut()
        .set_read_timeout(None)
        .map_err(|error| error.to_string())?;
    let epoch = runtime.snapshot.lock().epoch.clone();
    let mut accepted = None;

    while let Some(Request::Viewport {
        client: sender,
        epoch: owner,
        operation,
    }) = read_frame_bounded::<Request>(&mut stream, MAX_ATTACHMENT_FRAME_BYTES)?
    {
        if sender != client || owner != epoch {
            return Err("Stale Workspace viewport attachment".into());
        }
        if matches!(operation, Operation::Detach) {
            closed.store(true, Ordering::Relaxed);
            let _ = runtime.control.try_send(Control::Detach { client, epoch });
            return write_frame(
                stream.get_mut(),
                &Response::Viewport(Ok(Box::new(WorkspaceViewportReply::Receipt))),
            );
        }
        let mut result = if runtime.snapshot.lock().state.is_live() {
            let (reply, receipt) = bounded(1);
            runtime
                .control
                .try_send(Control::Viewport {
                    client: client.clone(),
                    epoch: epoch.clone(),
                    operation: operation.clone(),
                    reply,
                })
                .map_err(|_| "Workspace viewport is busy or closed".to_string())
                .and_then(|()| {
                    receipt.recv_timeout(Duration::from_secs(3)).map_err(|_| {
                        "Session Host did not confirm the viewport operation".to_string()
                    })
                })
                .and_then(|result| result)
        } else {
            Err("Workspace viewport has closed".into())
        };
        if result.is_err() {
            let snapshot = runtime.snapshot.lock().clone();
            if snapshot.state == RuntimeState::Exited {
                result = match operation {
                    Operation::Poll => retained_viewport(
                        &runtime.output.lock(),
                        &snapshot,
                        generation,
                    )
                    .map(|frame| {
                        let awaiting_ack = offered.is_some();
                        let frame = (!awaiting_ack && accepted != Some(frame.revision))
                            .then_some(Update::Snapshot { frame });
                        let status = match snapshot.exit_code {
                            Some(code) => format!("Workspace provider exited with status {code}."),
                            None => "Workspace provider exited.".into(),
                        };
                        WorkspaceViewportReply::Polled {
                            update: WorkspaceViewportPoll {
                                frame,
                                writable: false,
                                status,
                                finished: !awaiting_ack,
                            },
                        }
                    }),
                    Operation::Ack { revision }
                        if offered == Some(revision)
                            || (offered.is_none() && accepted == Some(revision)) =>
                    {
                        Ok(WorkspaceViewportReply::Receipt)
                    }
                    _ => result,
                };
            } else if let Ok(Response::Error(error)) = events.try_recv() {
                result = Err(error);
            }
        }
        if let (Operation::Ack { revision }, Ok(WorkspaceViewportReply::Receipt)) =
            (&operation, &result)
        {
            accepted = Some(*revision);
            offered = None;
        }
        if let Ok(WorkspaceViewportReply::Polled { update }) = &result {
            if let Some(frame) = &update.frame {
                offered = Some(frame.revision());
            }
        }
        write_frame(stream.get_mut(), &Response::Viewport(result.map(Box::new)))?;
    }
    let _ = runtime.control.try_send(Control::Detach { client, epoch });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_workspace::ConversationId;
    use parking_lot::Mutex;

    #[test]
    fn completed_pty_accepts_in_flight_ack_before_offering_final_viewport() {
        use WorkspaceViewportOperation as Operation;
        let mut terminal = Terminal::new(4, 10).unwrap();
        terminal.feed(b"live").unwrap();
        let first = terminal.frame().unwrap();
        terminal.feed(b" final").unwrap();
        let final_frame = terminal.frame().unwrap();
        let snapshot = RuntimeSnapshot {
            cleanup_verified: true,
            id: ConversationId("viewport-test".into()),
            state: RuntimeState::Exited,
            pid: 17,
            process_start: "test".into(),
            epoch: uuid::Uuid::new_v4().to_string(),
            event_sequence: 3,
            cols: 10,
            rows: 4,
            exit_code: Some(7),
            detail: String::new(),
        };
        let initial = engine_projection::from_engine(&first, &wire_session(&snapshot), 1).unwrap();
        let initial_revision = initial.revision;
        let (control, _worker) = bounded(1);
        let (output, events) = bounded(1);
        let runtime = RuntimeHandle {
            control,
            snapshot: Arc::new(Mutex::new(snapshot.clone())),
            output: Arc::new(Mutex::new(RuntimeOutput {
                viewport: Some(final_frame),
                ..RuntimeOutput::default()
            })),
        };
        output
            .send(Response::ViewportAttached {
                snapshot: snapshot.clone(),
                frame: Box::new(initial),
            })
            .unwrap();
        let (client, server) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let worker = std::thread::spawn(move || {
            viewport_connection(
                BufReader::new(server),
                runtime,
                "view".into(),
                1,
                events,
                Arc::new(AtomicBool::new(false)),
            )
        });
        let mut client = BufReader::new(client);
        assert!(matches!(
            read_frame::<Response>(&mut client),
            Ok(Some(Response::ViewportAttached { .. }))
        ));
        let mut execute = |operation| {
            write_frame(
                client.get_mut(),
                &Request::Viewport {
                    client: "view".into(),
                    epoch: snapshot.epoch.clone(),
                    operation,
                },
            )
            .unwrap();
            match read_frame::<Response>(&mut client).unwrap().unwrap() {
                Response::Viewport(result) => result.map(|reply| *reply),
                other => panic!("unexpected viewport response {other:?}"),
            }
        };
        assert!(matches!(
            execute(Operation::Poll),
            Ok(WorkspaceViewportReply::Polled {
                update: WorkspaceViewportPoll {
                    frame: None,
                    finished: false,
                    ..
                },
            })
        ));
        assert!(execute(Operation::Ack {
            revision: initial_revision + 1
        })
        .is_err());
        assert!(matches!(
            execute(Operation::Ack {
                revision: initial_revision
            }),
            Ok(WorkspaceViewportReply::Receipt)
        ));
        let update = execute(Operation::Poll).unwrap();
        let WorkspaceViewportReply::Polled { update } = update else {
            panic!("expected final viewport");
        };
        assert!(update.finished && !update.writable);
        let revision = update.frame.unwrap().revision();
        assert!(revision > initial_revision);
        assert!(matches!(
            execute(Operation::Ack { revision }),
            Ok(WorkspaceViewportReply::Receipt)
        ));
        assert!(matches!(
            execute(Operation::Poll),
            Ok(WorkspaceViewportReply::Polled {
                update: WorkspaceViewportPoll {
                    frame: None,
                    finished: true,
                    ..
                },
            })
        ));
        assert!(matches!(
            execute(Operation::Detach),
            Ok(WorkspaceViewportReply::Receipt)
        ));
        worker.join().unwrap().unwrap();
    }
}
