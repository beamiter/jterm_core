//! Terminal Channel frames with inline rasters, row moves, and packed rows.

use crate::terminal::{self, Attachment, CursorShape, RgbColor, SessionStatus, WireFrame};
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use std::{fmt, num::NonZeroU64};

pub use crate::terminal::{MAX_CELLS, MAX_COLUMNS, MAX_ROWS};
pub const VERSION: u8 = 4;
pub const IMAGE_VERSION: u8 = 5;
pub const ROW_MOVE_VERSION: u8 = 6;
pub const PACKED_VERSION: u8 = 7;
pub const RENDITION_VERSION: u8 = 8;
pub const STYLE_CONCEALED: u8 = 1 << 2;
pub const STYLE_OVERLINE: u8 = 1 << 7;
pub const MAX_IMAGES: usize = 32;
pub const MAX_IMAGE_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_IMAGE_DIMENSION: u32 = 4096;
pub const MAX_ENCODED_IMAGE_BYTES: usize = 4 * (MAX_IMAGE_BYTES + 2 * MAX_IMAGES).div_ceil(3);
pub const MAX_ENCODED_FRAME_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_TEXT_BYTES: usize = 512 * 1024;
pub const MAX_CLUSTER_BYTES: usize = 64;
pub const MAX_HYPERLINKS: usize = 4096;
pub const MAX_HYPERLINK_BYTES: usize = 1024 * 1024;
pub const MAX_HYPERLINK_TARGET_BYTES: usize = 2048;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotFormat {
    Compatibility,
    Channel,
    ChannelImages,
    ChannelRowMoves,
    ChannelPacked,
    ChannelRendition,
}

impl SnapshotFormat {
    pub fn negotiate(version: Option<u8>) -> Result<Self, &'static str> {
        match version {
            None | Some(terminal::VERSION) => Ok(Self::Compatibility),
            Some(VERSION) => Ok(Self::Channel),
            Some(IMAGE_VERSION) => Ok(Self::ChannelImages),
            Some(ROW_MOVE_VERSION) => Ok(Self::ChannelRowMoves),
            Some(PACKED_VERSION) => Ok(Self::ChannelPacked),
            Some(RENDITION_VERSION) => Ok(Self::ChannelRendition),
            Some(_) => Err("unsupported terminal snapshot version"),
        }
    }

    pub fn channel_version(self) -> Option<u8> {
        match self {
            Self::Compatibility => None,
            Self::Channel => Some(VERSION),
            Self::ChannelImages => Some(IMAGE_VERSION),
            Self::ChannelRowMoves => Some(ROW_MOVE_VERSION),
            Self::ChannelPacked => Some(PACKED_VERSION),
            Self::ChannelRendition => Some(RENDITION_VERSION),
        }
    }

    pub fn supports_images(self) -> bool {
        matches!(
            self,
            Self::ChannelImages
                | Self::ChannelRowMoves
                | Self::ChannelPacked
                | Self::ChannelRendition
        )
    }

    pub fn history_version(self) -> Option<u8> {
        match self {
            Self::ChannelPacked | Self::ChannelRendition => self.channel_version(),
            Self::ChannelImages | Self::ChannelRowMoves => Some(IMAGE_VERSION),
            _ => None,
        }
    }
}

pub fn supports_packed_rows(version: u8) -> bool {
    matches!(version, PACKED_VERSION | RENDITION_VERSION)
}

pub fn supports_row_moves(version: u8) -> bool {
    version == ROW_MOVE_VERSION || supports_packed_rows(version)
}

pub const MAX_PROJECTED_HISTORY_ROWS: usize = 65_536;

/// One-based physical archive row and its original cell boundary, stable across reflow.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HistorySource {
    pub row: Counter,
    pub column: u16,
}

pub type HistoryRow = HistoryRowData<Cell>;
pub type HistoryRowData<C> = HistoryRowWith<RowData<C>>;

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename = "HistoryRow", deny_unknown_fields)]
pub struct HistoryRowWith<R> {
    pub source: HistorySource,
    pub paint: R,
    /// Physical cell boundaries in the selectable snapshot, measured in UTF-16.
    pub text_offsets: Vec<u32>,
}

pub type HistoryView = HistoryViewData<Cell>;
pub type HistoryViewData<C> = HistoryViewRows<RowData<C>>;

/// An immutable primary-history window. It does not participate in live ACKs.
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename = "HistoryView", deny_unknown_fields)]
pub struct HistoryViewRows<R> {
    pub version: u8,
    pub owner: Attachment,
    pub revision: Counter,
    pub offset: u32,
    pub total_rows: u32,
    pub cols: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_background: Option<RgbColor>,
    pub text: String,
    pub rows: Vec<HistoryRowWith<R>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hyperlinks: Vec<Hyperlink>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ImageResource>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum LocalHistorySnapshot {
    Compatibility(terminal::HistorySnapshot),
    Projection(String),
}

/// The selected local snapshot shape. The v3 decoder is also used by SSH.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum LocalSnapshot {
    Compatibility(WireFrame),
    Channel(String),
}

#[derive(Deserialize)]
#[serde(untagged)]
pub enum SnapshotRevision {
    Compatibility(u32),
    Channel(Counter),
}

impl SnapshotRevision {
    pub fn get(self) -> u64 {
        match self {
            Self::Compatibility(value) => u64::from(value),
            Self::Channel(value) => value.get(),
        }
    }
}

/// JSON-safe, canonical, positive 64-bit identifier. JSON numbers cannot
/// distinguish all successive revisions beyond `Number.MAX_SAFE_INTEGER`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Counter(NonZeroU64);

impl Counter {
    pub const fn new(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

impl Serialize for Counter {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Counter {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl de::Visitor<'_> for Visitor {
            type Value = Counter;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a canonical positive u64 decimal string")
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Counter, E> {
                if value.len() <= 20
                    && value
                        .as_bytes()
                        .first()
                        .is_some_and(|b| (b'1'..=b'9').contains(b))
                    && value.bytes().all(|b| b.is_ascii_digit())
                {
                    if let Ok(value) = value.parse::<NonZeroU64>() {
                        return Ok(Counter(value));
                    }
                }
                Err(E::invalid_value(de::Unexpected::Str(value), &self))
            }
        }
        deserializer.deserialize_str(Visitor)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Screen {
    Primary,
    Alternate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Underline {
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

pub type Cell = CellData<String, String>;

/// The Channel cell shape shared by decoded frames and compact encoders.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename = "Cell", deny_unknown_fields)]
pub struct CellData<Text, Paint> {
    /// UTF-8 cluster in the lead cell. A continuation uses one ASCII blank.
    pub text: Text,
    pub foreground: Paint,
    pub background: Paint,
    /// 0 = wide continuation, 1 = single cell, 2 = wide lead.
    pub width: u8,
    /// bold=1, italic=2, strike=8, dim=16, blink=32, protected=64.
    /// Version 8 additionally uses bits 2 and 7 for conceal and overline.
    pub style: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub underline: Option<Underline>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub underline_color: Option<Paint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hyperlink_id: Option<u16>,
}

pub type Row = RowData<Cell>;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename = "Row", deny_unknown_fields)]
pub struct RowData<C> {
    /// The row continues on the next physical row.
    pub wrapped: bool,
    pub cells: Vec<C>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub image_slices: Vec<ImageSlice>,
}

/// Versions 7 and 8 can use a row-local style table and runs or ordinary rows.
#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PackedRow<Text, Paint> {
    pub wrapped: bool,
    pub columns: u16,
    pub styles: Vec<PackedStyle<Paint>>,
    pub runs: Vec<PackedRun<Text>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub image_slices: Vec<ImageSlice>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(deny_unknown_fields, bound(deserialize = "Paint: Deserialize<'de>"))]
pub struct PackedStyle<Paint> {
    pub foreground: Paint,
    pub background: Paint,
    pub style: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub underline: Option<Underline>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub underline_color: Option<Paint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hyperlink_id: Option<u16>,
}

/// Two fields encode one single-column cell per scalar. Three fields
/// encode one cell with explicit text and width, including wide continuations.
#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum PackedRun<Text> {
    Scalars(u16, String),
    Cell(u16, Text, u8),
}

/// Row-relative location of one vertical slice of a retained image.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ImageSlice {
    pub column: u16,
    pub id: Counter,
    pub slice: u16,
}

/// Immutable unassociated RGBA8 pixels in canonical base64 and cell geometry.
#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ImageResource {
    pub id: Counter,
    pub width: u32,
    pub height: u32,
    pub columns: u16,
    pub rows: u16,
    pub rgba: String,
}

pub type ChangedRow = ChangedRowData<Cell>;
pub type ChangedRowData<C> = ChangedRowWith<RowData<C>>;

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename = "ChangedRow", deny_unknown_fields)]
pub struct ChangedRowWith<R> {
    pub index: u16,
    pub row: R,
}

/// Copy consecutive rows from the acknowledged base into the new frame.
/// Sources refer to the immutable base even when the ranges overlap.
#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RowMove {
    pub from: u16,
    pub to: u16,
    pub count: u16,
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Stamp {
    pub version: u8,
    pub session: String,
    pub epoch: Counter,
    pub revision: Counter,
    pub layout: Counter,
    pub screen: Screen,
    /// Hyperlink and image IDs are immutable for one owner incarnation.
    pub resource_generation: Counter,
    pub cursor_row: u16,
    pub cursor_col: u16,
    pub cursor_visible: bool,
    pub cursor_shape: CursorShape,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor_color: Option<RgbColor>,
    pub pending_wrap: bool,
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Hyperlink {
    pub id: u16,
    pub target: String,
}

pub type Update = UpdateData<Cell>;
pub type UpdateData<C> = UpdateRows<RowData<C>>;

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(
    rename = "Update",
    tag = "kind",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum UpdateRows<R> {
    Snapshot {
        stamp: Stamp,
        cols: u16,
        rows: Vec<R>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        hyperlinks: Vec<Hyperlink>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<ImageResource>,
    },
    Delta {
        stamp: Stamp,
        base_revision: Counter,
        rows: Vec<ChangedRowWith<R>>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        moves: Vec<RowMove>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        hyperlinks: Vec<Hyperlink>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<ImageResource>,
    },
}

impl<R> UpdateRows<R> {
    pub fn revision(&self) -> Counter {
        match self {
            Self::Snapshot { stamp, .. } | Self::Delta { stamp, .. } => stamp.revision,
        }
    }
}

/// A delivery carries one stage of the shared frame stream and the host's
/// session status snapshot. The default retains the base terminal status.
#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum StreamEvent<Status = SessionStatus> {
    Update {
        owner: Attachment,
        sequence: Counter,
        /// Separately bounded JSON paint data. The WebView checks its byte
        /// size before parsing and validates the entire update before apply.
        frame: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preview: Option<Box<PeerUpdate>>,
        status: Status,
    },
    Stopped {
        owner: Attachment,
        message: String,
    },
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PeerUpdate {
    pub layout: u32,
    pub frame: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub sequence: Counter,
    pub revision: Option<Counter>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_moves_negotiate_as_a_distinct_image_capable_format() {
        let images = SnapshotFormat::negotiate(Some(IMAGE_VERSION)).unwrap();
        let moves = SnapshotFormat::negotiate(Some(ROW_MOVE_VERSION)).unwrap();
        assert_eq!(images, SnapshotFormat::ChannelImages);
        assert_eq!(moves, SnapshotFormat::ChannelRowMoves);
        assert!(images.supports_images() && moves.supports_images());
        assert_eq!(moves.channel_version(), Some(ROW_MOVE_VERSION));
        let packed = SnapshotFormat::negotiate(Some(PACKED_VERSION)).unwrap();
        assert_eq!(packed, SnapshotFormat::ChannelPacked);
        assert_eq!(packed.channel_version(), Some(PACKED_VERSION));
        assert!(packed.supports_images());
        let rendition = SnapshotFormat::negotiate(Some(RENDITION_VERSION)).unwrap();
        assert_eq!(rendition, SnapshotFormat::ChannelRendition);
        assert_eq!(rendition.history_version(), Some(RENDITION_VERSION));
        assert!(rendition.supports_images() && supports_packed_rows(RENDITION_VERSION));
        assert!(supports_row_moves(RENDITION_VERSION));
        assert!(SnapshotFormat::negotiate(Some(RENDITION_VERSION + 1)).is_err());
    }

    #[test]
    fn counter_has_one_wire_type_and_exact_full_width_order() {
        for n in [1, u32::MAX as u64, 9_007_199_254_740_993, u64::MAX] {
            let counter = Counter::new(n).unwrap();
            let wire = format!("\"{n}\"");
            assert_eq!(serde_json::to_string(&counter).unwrap(), wire);
            assert_eq!(serde_json::from_str::<Counter>(&wire).unwrap(), counter);
        }
        for wire in [
            "0",
            "1",
            "\"0\"",
            "\"01\"",
            "\"+1\"",
            "\"1e2\"",
            "\"１２\"",
            "\"\"",
            "\"18446744073709551616\"",
            "\"100000000000000000000\"",
        ] {
            assert!(serde_json::from_str::<Counter>(wire).is_err(), "{wire}");
        }
    }
}
