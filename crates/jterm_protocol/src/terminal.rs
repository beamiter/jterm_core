//! Terminal wire version 3 as used by the native bridge and browser receiver.

use serde::{Deserialize, Serialize};

pub const VERSION: u8 = 3;
pub const MAX_ROWS: usize = 128;
pub const MAX_COLUMNS: usize = 256;
pub const MAX_CELLS: usize = 32_768;

/// The session ID names a PTY during its lifetime. The generation names
/// the receiver's current attachment to that PTY.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Attachment {
    pub session: String,
    pub epoch: u32,
    pub generation: u32,
}

/// Names the retained PTY independently of any writable view generation.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSelector {
    pub session: String,
    pub epoch: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CursorShape {
    Block,
    Underline,
    Beam,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct WireCell {
    pub text: String,
    pub foreground: String,
    pub background: String,
    pub width: u8,
    /// bold=1, italic=2, underline=4, strike=8, dim=16.
    pub style: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct WireFrame {
    pub version: u8,
    pub session: String,
    pub epoch: u32,
    pub revision: u32,
    pub layout: u32,
    pub cols: usize,
    pub rows: Vec<Vec<WireCell>>,
    pub cursor_row: usize,
    pub cursor_col: usize,
    pub cursor_visible: bool,
    pub cursor_shape: CursorShape,
    pub pending_wrap: bool,
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct Resume<F = WireFrame> {
    pub owner: Attachment,
    pub frame: F,
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Update {
    Snapshot { frame: WireFrame },
    Delta { frame: WireDelta },
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct WireDelta {
    pub version: u8,
    pub session: String,
    pub epoch: u32,
    pub base_revision: u32,
    pub revision: u32,
    pub layout: u32,
    pub cursor_row: usize,
    pub cursor_col: usize,
    pub cursor_visible: bool,
    pub cursor_shape: CursorShape,
    pub pending_wrap: bool,
    pub rows: Vec<ChangedRow>,
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct ChangedRow {
    pub index: usize,
    pub cells: Vec<WireCell>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct SessionStatus {
    pub epoch: u32,
    pub exited: bool,
    pub drained: bool,
    pub write_error: Option<String>,
    pub dropped_reply_bytes: String,
    pub unsupported: String,
    #[serde(default)]
    pub command_blocks_enabled: bool,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct HistorySnapshot {
    pub epoch: u32,
    pub revision: String,
    pub offset: usize,
    pub total_rows: usize,
    pub text: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct CloseStatus {
    pub cleanup_pending: bool,
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StreamEvent {
    Update {
        owner: Attachment,
        sequence: u32,
        frame: Option<Update>,
        #[serde(skip_serializing_if = "Option::is_none")]
        preview: Option<Box<PeerUpdate>>,
        status: SessionStatus,
    },
    Stopped {
        owner: Attachment,
        message: String,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObserverTarget {
    pub epoch: u32,
    pub layout: u32,
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct PeerUpdate {
    pub layout: u32,
    pub frame: WireFrame,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub sequence: u32,
    pub revision: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    #[test]
    fn existing_stream_shape_round_trips_snapshot_delta_and_view_identity() {
        let owner = json!({"session": "a1", "epoch": 2, "generation": 7});
        let view = json!({
            "version": 3, "session": "a1", "epoch": 2, "revision": 12, "layout": 4,
            "cols": 2,
            "rows": [[
                {"text": "中", "foreground": "#ffffff", "background": "#000000", "width": 2, "style": 5},
                {"text": " ", "foreground": "#ffffff", "background": "#000000", "width": 0, "style": 0}
            ]],
            "cursor_row": 0, "cursor_col": 1, "cursor_visible": true,
            "cursor_shape": "beam", "pending_wrap": true
        });
        let status = json!({
            "epoch": 2, "exited": false, "drained": false, "write_error": null,
            "dropped_reply_bytes": "0", "unsupported": "0", "command_blocks_enabled": true
        });
        let snapshot = json!({
            "kind": "update", "owner": owner, "sequence": 5,
            "frame": {"kind": "snapshot", "frame": view},
            "status": status
        });
        let mut delta = snapshot.clone();
        delta["sequence"] = json!(6);
        delta["preview"] = json!({ "layout": 5, "frame": view });
        let rows = view["rows"][0].clone();
        let mut changed = view.clone();
        let fields = changed.as_object_mut().unwrap();
        fields.remove("cols");
        fields.insert("base_revision".into(), json!(12));
        fields.insert("revision".into(), json!(13));
        fields.insert("rows".into(), json!([{"index": 0, "cells": rows}]));
        delta["frame"] = json!({"kind": "delta", "frame": changed});
        for fixture in [snapshot, delta] {
            let parsed = serde_json::from_value::<StreamEvent>(fixture.clone()).unwrap();
            assert_eq!(serde_json::to_value(parsed).unwrap(), fixture);
        }
    }

    #[test]
    fn attachment_and_acknowledgement_keep_exact_v3_integer_bounds() {
        let fixture = json!({"session": "a1", "epoch": 4294967295u32, "generation": 3});
        assert_eq!(
            serde_json::to_value(serde_json::from_value::<Attachment>(fixture.clone()).unwrap())
                .unwrap(),
            fixture,
        );
        for invalid in [
            json!({"session": "a1", "epoch": 4294967296u64, "generation": 3}),
            json!({"session": "a1", "epoch": 1, "generation": 1.5}),
            json!({"session": "a1", "epoch": 1, "generation": 3, "owner": "other"}),
        ] {
            assert!(serde_json::from_value::<Attachment>(invalid).is_err());
        }
        let ack = Receipt {
            sequence: u32::MAX,
            revision: None,
        };
        assert_eq!(
            serde_json::to_value(ack).unwrap(),
            json!({"sequence": 4294967295u32, "revision": Value::Null}),
        );
    }
}
