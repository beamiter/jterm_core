//! Immutable styled history attached to a finished-block record.

use super::archived_text::{is_bounded_document_text, MAX_LINKS, MAX_SPANS};
use jterm_engine::archived_text::{ArchiveLink, ArchiveSpan, ArchiveStyle, ArchivedText};
use jterm_engine::command_blocks::ShellMarkerPosition;
use jterm_engine::grid::{Color, StyleFlags, UnderlineStyle};
use serde::{Deserialize, Serialize};
use std::{borrow::Cow, mem::size_of, ops::Range};
use uuid::Uuid;

// rkyv finds the legacy root at the end of the frame. Prepending a multiple
// of its archive alignment keeps the original root and relative pointers valid.
const MAGIC: &[u8; 10] = b"JTERM-DOC\0";
const HEADER_BYTES: usize = 16;
const ARCHIVE_ALIGNMENT: usize = 16;
const VERSION: u16 = 1;
const MAX_METADATA_BYTES: usize = 1024 * 1024;

/// The accepted lifecycle and raw PTY ranges behind a retained projection.
/// A stream ID identifies one PTY incarnation. Its original byte intervals
/// can include control traffic omitted by the finished document renderer.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FinishedExecutionSource {
    execution_id: Uuid,
    stream_id: Uuid,
    start_event: u64,
    end_event: u64,
    capture_event: u64,
    output_bytes: Range<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tail_bytes: Option<Range<u64>>,
    start_size: (u16, u16),
    pub head_dropped: bool,
    pub tail_dropped: bool,
}

/// The running record's first accepted marker, scoped to one PTY.
#[derive(Clone)]
pub struct RunningExecutionSource {
    execution_id: Uuid,
    start: ShellMarkerPosition,
    reported_end: Option<ShellMarkerPosition>,
    start_size: (u16, u16),
}

impl RunningExecutionSource {
    pub fn new(start: ShellMarkerPosition, start_size: (u16, u16)) -> Self {
        Self {
            execution_id: Uuid::new_v4(),
            start,
            reported_end: None,
            start_size,
        }
    }

    pub fn report_end(&mut self, reported_end: ShellMarkerPosition) {
        self.reported_end = Some(reported_end);
    }

    pub fn finish(
        self,
        stream_id: Uuid,
        capture_end: ShellMarkerPosition,
    ) -> Option<FinishedExecutionSource> {
        let end = self.reported_end.unwrap_or(capture_end);
        let output_bytes = self.start.packet_bytes().end..end.packet_bytes().start;
        let tail_bytes = self
            .reported_end
            .map(|end| end.packet_bytes().end..capture_end.packet_bytes().start);
        let source = FinishedExecutionSource {
            execution_id: self.execution_id,
            stream_id,
            start_event: self.start.sequence(),
            end_event: end.sequence(),
            capture_event: capture_end.sequence(),
            output_bytes,
            tail_bytes,
            start_size: self.start_size,
            head_dropped: false,
            tail_dropped: false,
        };
        source.valid().then_some(source)
    }
}

impl FinishedExecutionSource {
    fn valid(&self) -> bool {
        !self.execution_id.is_nil()
            && !self.stream_id.is_nil()
            && self.start_size.0 > 0
            && self.start_size.1 > 0
            && self.start_event > 0
            && self.end_event > self.start_event
            && self.output_bytes.start <= self.output_bytes.end
            && match &self.tail_bytes {
                Some(tail) => {
                    self.capture_event > self.end_event
                        && tail.start > self.output_bytes.end
                        && tail.start <= tail.end
                }
                None => self.capture_event == self.end_event,
            }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FinishedDocument {
    pub archive: ArchivedText,
    /// Last geometry sent to the child, in columns and rows.
    pub capture_size: Option<(u16, u16)>,
    pub execution_source: Option<FinishedExecutionSource>,
}

impl FinishedDocument {
    pub fn capture(output: &str, size: (u16, u16)) -> Option<Self> {
        Some(Self {
            archive: super::project_finished_output(output, usize::from(size.0))?,
            capture_size: (size.0 != 0 && size.1 != 0).then_some(size),
            execution_source: None,
        })
    }

    /// Keep provenance on an admissible plain projection even when the rich
    /// output contains a style or operation the document renderer cannot keep.
    pub fn capture_with_source(
        output: &str,
        plain_output: &str,
        size: (u16, u16),
        source: Option<FinishedExecutionSource>,
    ) -> Option<Self> {
        let mut document = Self::capture(output, size).or_else(|| {
            source.as_ref()?;
            Self::plain(plain_output, size)
        })?;
        document.execution_source = source;
        Some(document)
    }

    fn plain(text: &str, size: (u16, u16)) -> Option<Self> {
        if !is_bounded_document_text(text) {
            return None;
        }
        Some(Self {
            archive: ArchivedText {
                text: text.to_owned(),
                spans: Vec::new(),
                links: Vec::new(),
            },
            capture_size: (size.0 != 0 && size.1 != 0).then_some(size),
            execution_source: None,
        })
    }

    /// The document can be shared by the view and the background writer.
    /// Charge the full owned backing storage to each independently bounded queue.
    pub fn retained_bytes(&self) -> usize {
        let archive = &self.archive;
        size_of::<Self>()
            .saturating_add(2 * size_of::<usize>())
            .saturating_add(archive.text.capacity())
            .saturating_add(
                archive
                    .spans
                    .capacity()
                    .saturating_mul(size_of::<ArchiveSpan>()),
            )
            .saturating_add(
                archive
                    .links
                    .capacity()
                    .saturating_mul(size_of::<ArchiveLink>()),
            )
            .saturating_add(
                archive
                    .links
                    .iter()
                    .map(|link| link.target.capacity())
                    .sum::<usize>(),
            )
    }

    /// The unchanged block archive already owns the exact plain text. Place
    /// only the document's attributes and recorded geometry in its prefix.
    pub fn attach<'a>(&self, plain: &str, record: &'a [u8]) -> Cow<'a, [u8]> {
        if self.archive.text != plain {
            return Cow::Borrowed(record);
        }
        let Some(metadata) = MetadataV1::encode(self) else {
            return Cow::Borrowed(record);
        };
        let padded = (HEADER_BYTES + metadata.len()).next_multiple_of(ARCHIVE_ALIGNMENT);
        let mut frame = Vec::with_capacity(padded + record.len());
        frame.extend_from_slice(MAGIC);
        frame.extend_from_slice(&VERSION.to_le_bytes());
        frame.extend_from_slice(&(metadata.len() as u32).to_le_bytes());
        frame.extend_from_slice(&metadata);
        frame.resize(padded, 0);
        frame.extend_from_slice(record);
        Cow::Owned(frame)
    }
}

/// A borrowed view of a decompressed history frame. Older frames have no
/// document metadata. A malformed or newer extension leaves the plain record
/// available and cannot add styling or an opener target.
pub struct FinishedHistoryFrame<'a> {
    pub record: &'a [u8],
    metadata: Option<&'a [u8]>,
}

impl<'a> FinishedHistoryFrame<'a> {
    pub fn read(frame: &'a [u8]) -> Self {
        let legacy = Self {
            record: frame,
            metadata: None,
        };
        let Some(header) = frame
            .get(..HEADER_BYTES)
            .filter(|header| header.starts_with(MAGIC))
        else {
            return legacy;
        };
        let version = u16::from_le_bytes([header[10], header[11]]);
        let len = u32::from_le_bytes([header[12], header[13], header[14], header[15]]) as usize;
        if len > MAX_METADATA_BYTES {
            return legacy;
        }
        let end = HEADER_BYTES + len;
        let prefix_len = end.next_multiple_of(ARCHIVE_ALIGNMENT);
        let Some(record) = frame.get(prefix_len..).filter(|record| !record.is_empty()) else {
            return legacy;
        };
        Self {
            record,
            metadata: (version == VERSION).then_some(&frame[HEADER_BYTES..end]),
        }
    }

    pub fn document(&self, plain: &str) -> Option<FinishedDocument> {
        MetadataV1::decode(self.metadata?, plain)
    }
}

/// Version 1 stores UTF-8 byte offsets into the legacy text field. Use an
/// explicit color/flag representation independent of the live cell layout.
#[derive(Serialize, Deserialize)]
struct MetadataV1 {
    capture_size: Option<(u16, u16)>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "read_execution_source"
    )]
    execution_source: Option<FinishedExecutionSource>,
    spans: Vec<SpanV1>,
    links: Vec<(Range<usize>, String)>,
}

fn read_execution_source<'de, D>(
    deserializer: D,
) -> Result<Option<FinishedExecutionSource>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value::<FinishedExecutionSource>(value)
        .ok()
        .filter(FinishedExecutionSource::valid))
}

#[derive(Serialize, Deserialize)]
struct SpanV1 {
    bytes: Range<usize>,
    colors: [u32; 3],
    flags: u8,
}

impl MetadataV1 {
    fn encode(document: &FinishedDocument) -> Option<Vec<u8>> {
        let archive = &document.archive;
        if !valid_document(archive) {
            return None;
        }
        let metadata = Self {
            capture_size: document.capture_size,
            execution_source: document.execution_source.clone(),
            spans: archive
                .spans
                .iter()
                .map(SpanV1::encode)
                .collect::<Option<_>>()?,
            links: archive
                .links
                .iter()
                .map(|link| (link.bytes.clone(), link.target.clone()))
                .collect(),
        };
        let bytes = serde_json::to_vec(&metadata).ok()?;
        (bytes.len() <= MAX_METADATA_BYTES).then_some(bytes)
    }

    fn decode(metadata: &[u8], plain: &str) -> Option<FinishedDocument> {
        if !is_bounded_document_text(plain) {
            return None;
        }
        let Self {
            capture_size,
            execution_source,
            spans,
            links,
        } = serde_json::from_slice(metadata).ok()?;
        if capture_size.is_some_and(|(cols, rows)| cols == 0 || rows == 0)
            || spans.len() > MAX_SPANS
            || links.len() > MAX_LINKS
        {
            return None;
        }
        let archive = ArchivedText {
            text: plain.to_owned(),
            spans: spans
                .into_iter()
                .map(SpanV1::decode)
                .collect::<Option<_>>()?,
            links: links
                .into_iter()
                .map(|(bytes, target)| ArchiveLink { bytes, target })
                .collect(),
        };
        valid_document(&archive).then_some(FinishedDocument {
            archive,
            capture_size,
            execution_source,
        })
    }
}

fn valid_document(archive: &ArchivedText) -> bool {
    is_bounded_document_text(&archive.text)
        && archive.spans.len() <= MAX_SPANS
        && archive.links.len() <= MAX_LINKS
        && valid_ranges(&archive.text, archive.spans.iter().map(|span| &span.bytes))
        && valid_ranges(&archive.text, archive.links.iter().map(|link| &link.bytes))
        && archive
            .links
            .iter()
            .all(|link| crate::link::is_openable_osc8("", &link.target))
}

fn valid_ranges<'a>(text: &str, mut ranges: impl Iterator<Item = &'a Range<usize>>) -> bool {
    let mut previous_end = 0;
    ranges.all(|range| {
        let valid = range.start >= previous_end
            && range.start < range.end
            && text.is_char_boundary(range.start)
            && text.is_char_boundary(range.end);
        previous_end = range.end;
        valid
    })
}

impl SpanV1 {
    fn encode(span: &ArchiveSpan) -> Option<Self> {
        let style = span.style;
        let flags = u8::from(style.flags.bold())
            | (u8::from(style.flags.italic()) << 1)
            | (u8::from(style.flags.inverse()) << 2)
            | (u8::from(style.flags.strikethrough()) << 3)
            | match style.flags.underline() {
                UnderlineStyle::None => 0,
                UnderlineStyle::Single => 1 << 4,
                UnderlineStyle::Double => 2 << 4,
                UnderlineStyle::Curly => 3 << 4,
                _ => return None,
            };
        if Self::decode_flags(flags)? != style.flags {
            return None;
        }
        Some(Self {
            bytes: span.bytes.clone(),
            colors: [
                encode_color(style.foreground)?,
                encode_color(style.background)?,
                encode_color(style.underline_color)?,
            ],
            flags,
        })
    }

    fn decode(self) -> Option<ArchiveSpan> {
        Some(ArchiveSpan {
            bytes: self.bytes,
            style: ArchiveStyle {
                foreground: decode_color(self.colors[0])?,
                background: decode_color(self.colors[1])?,
                underline_color: decode_color(self.colors[2])?,
                flags: Self::decode_flags(self.flags)?,
            },
        })
    }

    fn decode_flags(bits: u8) -> Option<StyleFlags> {
        let mut flags = StyleFlags::new();
        flags.set_bold(bits & 1 != 0);
        flags.set_italic(bits & 2 != 0);
        flags.set_inverse(bits & 4 != 0);
        flags.set_strikethrough(bits & 8 != 0);
        flags.set_underline(match bits >> 4 {
            0 => UnderlineStyle::None,
            1 => UnderlineStyle::Single,
            2 => UnderlineStyle::Double,
            3 => UnderlineStyle::Curly,
            _ => return None,
        });
        Some(flags)
    }
}

fn encode_color(color: Color) -> Option<u32> {
    match color {
        Color::Default => Some(0),
        Color::Indexed(index) => Some(u32::from_be_bytes([1, 0, 0, index])),
        Color::Rgb(r, g, b) => Some(u32::from_be_bytes([2, r, g, b])),
        _ => None,
    }
}

fn decode_color(value: u32) -> Option<Color> {
    match value.to_be_bytes() {
        [0, 0, 0, 0] => Some(Color::Default),
        [1, 0, 0, index] => Some(Color::Indexed(index)),
        [2, r, g, b] => Some(Color::Rgb(r, g, b)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finished_document_round_trip_keeps_exact_text_style_links_and_geometry() {
        let capture = "transient\r\x1b[K  \x1b[1;4:3;7;38;2;14;25;36m中\tvalue\x1b[0m\r\n\
                       \x1b]8;;https://example.test/a\x07label\x1b]8;;\x07\r\n\r\n";
        let document = FinishedDocument::capture(capture, (90, 28)).unwrap();
        assert_eq!(document.archive.text, "  中\tvalue\nlabel");
        let record = b"existing rkyv payload";
        let frame = document.attach(&document.archive.text, record);
        assert_eq!((frame.len() - record.len()) % ARCHIVE_ALIGNMENT, 0);
        let decoded = FinishedHistoryFrame::read(&frame);
        assert_eq!(decoded.record, record);
        assert_eq!(decoded.document(&document.archive.text), Some(document));
    }

    #[test]
    fn execution_source_survives_the_legacy_document_frame_and_invalid_source_degrades() {
        use jterm_engine::command_blocks::ShellMarkerClock;

        let mut clock = ShellMarkerClock::default();
        clock.begin_feed(128).unwrap();
        let start = clock.osc(Some(32), 10, true).unwrap();
        let end = clock.osc(Some(68), 10, false).unwrap();
        let prompt = clock.osc(Some(88), 6, true).unwrap();
        let mut running = RunningExecutionSource::new(start, (80, 24));
        running.report_end(end);
        let mut source = running.finish(Uuid::new_v4(), prompt).unwrap();
        source.head_dropped = true;
        assert_eq!(source.output_bytes, 32..54);
        assert_eq!(source.tail_bytes, Some(68..79));
        let mut document =
            FinishedDocument::capture("\x1b[1mdurable output\x1b[0m", (90, 30)).unwrap();
        document.execution_source = Some(source.clone());
        let frame = document.attach(&document.archive.text, b"legacy record");
        let loaded = FinishedHistoryFrame::read(&frame);
        assert_eq!(
            loaded.document(&document.archive.text),
            Some(document.clone())
        );
        assert_eq!(loaded.record, b"legacy record");

        let mut metadata: serde_json::Value =
            serde_json::from_slice(loaded.metadata.unwrap()).unwrap();
        metadata["execution_source"]["end_event"] = serde_json::json!(1);
        let metadata = serde_json::to_vec(&metadata).unwrap();
        let loaded = MetadataV1::decode(&metadata, &document.archive.text).unwrap();
        assert_eq!(loaded.archive, document.archive);
        assert_eq!(loaded.capture_size, document.capture_size);
        assert!(loaded.execution_source.is_none());
    }

    #[test]
    fn invalid_or_unknown_document_extensions_leave_the_plain_record_available() {
        let document = FinishedDocument::capture("  first\r\n\r\n", (80, 24)).unwrap();
        let record = b"existing rkyv payload";
        assert!(matches!(
            document.attach("unrelated", record),
            Cow::Borrowed(_)
        ));
        let plain = &document.archive.text;

        let mut frame = document.attach(plain, record).into_owned();
        frame[HEADER_BYTES] = b'!';
        let decoded = FinishedHistoryFrame::read(&frame);
        assert_eq!(decoded.record, record);
        assert!(decoded.document(plain).is_none());

        frame[10] = 2;
        let decoded = FinishedHistoryFrame::read(&frame);
        assert_eq!(decoded.record, record);
        assert!(decoded.document(plain).is_none());

        let bad_ranges = br#"{"capture_size":[80,24],"spans":[],"links":[[{"start":1,"end":3},"https://example.test/"]]}"#;
        assert!(MetadataV1::decode(bad_ranges, "中").is_none());
        let bad_target = br#"{"capture_size":null,"spans":[],"links":[[{"start":0,"end":3},"file:///tmp/path"]]}"#;
        assert!(MetadataV1::decode(bad_target, "中").is_none());
        assert!(MetadataV1::decode(bad_target, "a\x1bK").is_none());
    }
}
