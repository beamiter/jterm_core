use super::model::*;
use super::terminal_viewport::{WorkspaceViewportOperation, WorkspaceViewportReply};
use jterm_protocol::terminal::WireFrame;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};

pub(super) const MAX_FRAME_BYTES: usize = 12 * 1024 * 1024;
pub(super) const OUTPUT_CHUNK_BYTES: usize = 16 * 1024;
pub(super) const INPUT_CHUNK_BYTES: usize = 16 * 1024;
pub(super) const REPLY_CHUNK_BYTES: usize = 64 * 1024;
// A byte is at most three decimal digits plus one separator in the wire
// array. Leave room for the bounded attachment identity and frame fields.
pub(super) const MAX_ATTACHMENT_FRAME_BYTES: usize = REPLY_CHUNK_BYTES * 4 + 2048;

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum InputKind {
    #[default]
    User,
    Reply,
    Paste,
}

impl InputKind {
    pub(super) fn limit(self) -> usize {
        match self {
            Self::User | Self::Paste => INPUT_CHUNK_BYTES,
            Self::Reply => REPLY_CHUNK_BYTES,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum WorkspaceCommand {
    SaveNativeDraft {
        draft: Box<super::native::NativeDraft>,
    },
    StartNative {
        launch: Box<super::native::NativeLaunch>,
    },
    NativeCatalog {
        after: Option<ConversationId>,
        limit: usize,
    },
    InspectNative {
        id: ConversationId,
    },
    AttachNative {
        id: ConversationId,
        incarnation: String,
        view: String,
    },
    DetachNative {
        control: super::native::NativeControlLease,
    },
    DetachNativeView {
        view: String,
    },
    ControlNative {
        control: super::native::NativeControlLease,
        operation: String,
        action: Box<super::native::NativeAction>,
    },
    StartApi {
        launch: Box<super::api::ApiLaunch>,
    },
    InspectApi {
        id: ConversationId,
    },
    /// Resolve an uncertain admission without ever submitting HTTP again.
    ResolveApi {
        receipt: SubmissionReceipt,
    },
    AttachApi {
        id: ConversationId,
        epoch: u64,
        view: String,
    },
    DetachApi {
        control: super::api::ApiControlLease,
    },
    CancelApi {
        control: super::api::ApiControlLease,
    },
    Refresh,
    /// Stable numeric cursor, independent of changing activity timestamps.
    CatalogPage {
        before: Option<u64>,
        limit: usize,
    },
    Search {
        request: super::search::WorkspaceSearchRequest,
    },
    /// Read the exact indexed revision. This never attaches to a provider.
    ReadSearchDocument {
        conversation: ConversationId,
        event_id: String,
        revision: String,
    },
    /// Explicit read-only history import, with a bounded batch and provenance.
    IndexText {
        documents: Vec<super::search::WorkspaceTextDocument>,
    },
    ReadContext {
        id: ConversationId,
    },
    SaveContext {
        id: ConversationId,
        expected_revision: u64,
        context: super::WorkspaceContext,
    },
    BuildHandoff {
        id: ConversationId,
        expected_revision: u64,
        target_provider: String,
    },
    QueueUpdate {
        update: super::WorkspaceQueueUpdate,
    },
    QueueSnapshot,
    /// Bounded catalog of saved delivery plans; no execution is started.
    MissionList,
    ReadMission {
        id: String,
    },
    /// Plan-only compare-and-swap. Inspection evidence is host-owned.
    SaveMission {
        expected_revision: u64,
        mission: super::WorkspaceMission,
    },
    InspectMission {
        id: String,
    },
    InspectResult {
        id: ConversationId,
    },
    /// Bounded provider discovery projection; no transcript or credentials.
    /// Existing local organization takes precedence over refreshed metadata.
    IndexSessions {
        sessions: Vec<WorkspaceRow>,
    },
    ImportChats {
        namespace: String,
        snapshot_json: String,
    },
    LoadChat {
        id: ConversationId,
    },
    CreateChat {
        title: String,
    },
    SaveChat {
        id: ConversationId,
        expected_revision: u64,
        snapshot_json: String,
    },
    /// Compare against StoredChat::draft_revision, not conversation revision.
    SetDraft {
        id: ConversationId,
        expected_revision: u64,
        text: String,
    },
    Rename {
        id: ConversationId,
        title: String,
    },
    Archive {
        id: ConversationId,
        archived: bool,
    },
    Pin {
        id: ConversationId,
        pinned: bool,
    },
    Tags {
        id: ConversationId,
        tags: Vec<String>,
    },
    Organize {
        changes: Vec<WorkspaceOrganization>,
    },
    Delete {
        id: ConversationId,
    },
    /// Remove local workspace evidence for one source-qualified CLI session.
    DeleteCliSession {
        id: ConversationId,
        session: Box<crate::agent_sessions::AgentSession>,
    },
    History {
        id: ConversationId,
        before: Option<u64>,
        limit: usize,
    },
    SetReadPosition {
        id: ConversationId,
        view: String,
        position: f64,
    },
    BeginSubmission {
        id: ConversationId,
        expected_revision: u64,
        draft_revision: u64,
        text: String,
    },
    CompleteSubmission {
        receipt: SubmissionReceipt,
        text: String,
    },
    CancelSubmission {
        receipt: SubmissionReceipt,
        restore_draft: bool,
    },
    Stop {
        id: ConversationId,
    },
    Runtime {
        id: ConversationId,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum WorkspaceReply {
    Native {
        snapshot: Box<super::native::NativeSnapshot>,
        control: Option<super::native::NativeControlLease>,
    },
    NativeCatalog {
        entries: Vec<super::native::NativeCatalogEntry>,
        next_after: Option<ConversationId>,
    },
    Api {
        snapshot: super::api::ApiSnapshot,
        control: Option<super::api::ApiControlLease>,
    },
    Catalog {
        rows: Vec<WorkspaceRow>,
    },
    CatalogPage {
        rows: Vec<WorkspaceRow>,
        next_before: Option<u64>,
        total: usize,
    },
    Search {
        page: super::search::WorkspaceSearchPage,
    },
    SearchDocument {
        document: super::search::WorkspaceTextDocument,
    },
    Context {
        context: super::WorkspaceContext,
    },
    Handoff {
        packet: super::WorkspaceHandoff,
    },
    Queue {
        snapshot: super::WorkspaceQueueSnapshot,
    },
    Missions {
        missions: Vec<super::WorkspaceMission>,
    },
    Mission {
        mission: super::WorkspaceMission,
        report: Option<super::WorkspaceMissionReport>,
    },
    Result {
        report: Box<super::WorkspaceResultReport>,
    },
    Imported {
        mappings: Vec<LegacyChatMapping>,
    },
    Chat {
        chat: StoredChat,
    },
    Submission {
        receipt: SubmissionReceipt,
    },
    Messages {
        messages: Vec<StoredMessage>,
        next_before: Option<u64>,
    },
    Runtime {
        snapshot: RuntimeSnapshot,
    },
    Ack,
}

#[derive(Clone, Debug)]
pub enum WorkspaceEvent {
    Reply {
        request_id: u64,
        result: Result<WorkspaceReply, String>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) enum Request {
    Command {
        version: u32,
        command: WorkspaceCommand,
    },
    Attach {
        version: u32,
        launch: Box<ManagedLaunch>,
        client: String,
    },
    AttachViewport {
        version: u32,
        launch: Box<ManagedLaunch>,
        client: String,
        generation: u32,
    },
    AttachOwned {
        version: u32,
        terminal: super::native::OwnedTerminalDescriptor,
        client: String,
        cols: u16,
        rows: u16,
    },
    Input {
        client: String,
        epoch: String,
        bytes: Vec<u8>,
        #[serde(default, skip_serializing_if = "is_user_input")]
        kind: InputKind,
    },
    Resize {
        client: String,
        epoch: String,
        cols: u16,
        rows: u16,
    },
    Viewport {
        client: String,
        epoch: String,
        operation: WorkspaceViewportOperation,
    },
    Detach {
        client: String,
        epoch: String,
    },
}

fn is_user_input(kind: &InputKind) -> bool {
    matches!(kind, InputKind::User)
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) enum Response {
    Reply(Box<Result<WorkspaceReply, String>>),
    Attached {
        snapshot: RuntimeSnapshot,
        screen: Vec<u8>,
        truncated: bool,
    },
    ViewportAttached {
        snapshot: RuntimeSnapshot,
        frame: Box<WireFrame>,
    },
    Viewport(Result<Box<WorkspaceViewportReply>, String>),
    Output {
        sequence: u64,
        bytes: Vec<u8>,
    },
    Exited {
        sequence: u64,
        code: Option<i32>,
    },
    Error(String),
}

pub(super) fn write_frame(writer: &mut impl Write, value: &impl Serialize) -> Result<(), String> {
    let bytes = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    if bytes.len() > MAX_FRAME_BYTES {
        return Err("Workspace protocol frame exceeds its limit".into());
    }
    writer
        .write_all(&bytes)
        .and_then(|_| writer.write_all(b"\n"))
        .and_then(|_| writer.flush())
        .map_err(|e| e.to_string())
}

pub(super) fn read_frame<T: serde::de::DeserializeOwned>(
    reader: &mut impl BufRead,
) -> Result<Option<T>, String> {
    read_frame_bounded(reader, MAX_FRAME_BYTES)
}

pub(super) fn read_frame_bounded<T: serde::de::DeserializeOwned>(
    reader: &mut impl BufRead,
    max_bytes: usize,
) -> Result<Option<T>, String> {
    let mut bytes = Vec::new();
    loop {
        let buf = reader.fill_buf().map_err(|e| e.to_string())?;
        if buf.is_empty() {
            return if bytes.is_empty() {
                Ok(None)
            } else {
                Err("Incomplete workspace frame".into())
            };
        }
        let count = buf
            .iter()
            .position(|b| *b == b'\n')
            .map_or(buf.len(), |i| i + 1);
        if bytes.len() + count > max_bytes {
            return Err("Workspace protocol frame exceeds its limit".into());
        }
        let done = buf[count - 1] == b'\n';
        bytes.extend_from_slice(&buf[..count]);
        reader.consume(count);
        if done {
            return serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| format!("Invalid workspace frame: {e}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachment_input_preserves_the_legacy_wire_and_bounds_protocol_replies() {
        let legacy = serde_json::json!({"Input": {
            "client": "client",
            "epoch": "epoch",
            "bytes": [1, 2, 3]
        }});
        let decoded = serde_json::from_value::<Request>(legacy.clone()).unwrap();
        assert!(matches!(
            &decoded,
            Request::Input {
                kind: InputKind::User,
                ..
            }
        ));
        assert_eq!(serde_json::to_value(decoded).unwrap(), legacy);

        let reply = Request::Input {
            client: "client".into(),
            epoch: "epoch".into(),
            bytes: vec![255; REPLY_CHUNK_BYTES],
            kind: InputKind::Reply,
        };
        let mut encoded = Vec::new();
        write_frame(&mut encoded, &reply).unwrap();
        let rejected = read_frame_bounded::<Request>(&mut encoded.as_slice(), INPUT_CHUNK_BYTES);
        assert!(rejected.is_err());
        assert!(matches!(
            read_frame_bounded::<Request>(&mut encoded.as_slice(), MAX_ATTACHMENT_FRAME_BYTES)
                .unwrap(),
            Some(Request::Input { bytes, kind: InputKind::Reply, .. })
                if bytes.len() == REPLY_CHUNK_BYTES
        ));
    }

    #[test]
    fn boxed_result_replies_preserve_the_legacy_wire_frame() {
        // Protocol 2 clients must still read result replies from a new host.
        let legacy = serde_json::json!({"Reply": {"Ok": {"Result": {"report": {
            "conversation": "task:00000000-0000-4000-8000-000000000001",
            "task_id": "00000000-0000-4000-8000-000000000001",
            "run_id": "run-1",
            "source_sequence": 7,
            "baseline": null,
            "current": null,
            "validation": null,
            "validation_current": false,
            "validation_status": "Not run",
            "validation_output": "",
            "validation_output_truncated": false,
            "error": null
        }}}}});
        let mut frame = serde_json::to_vec(&legacy).unwrap();
        frame.push(b'\n');
        let response: Response = read_frame(&mut frame.as_slice()).unwrap().unwrap();
        let Response::Reply(result) = &response else {
            panic!("legacy result reply was not recognized");
        };
        let Ok(WorkspaceReply::Result { report }) = result.as_ref() else {
            panic!("legacy result evidence was not recognized");
        };
        assert_eq!(report.source_sequence, 7);
        let mut emitted = Vec::new();
        write_frame(&mut emitted, &response).unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&emitted).unwrap(),
            legacy
        );
    }
}
