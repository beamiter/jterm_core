//! Versioned completed-card checkpoints owned by a restorable pane. This is
//! independent of the jsh JSONL journal, whose v1 compactor cannot retain
//! extra fields. A checkpoint contains immutable records and their reducer
//! cursor; it never contains PTY state, pending events, or input authority.

use crate::screen_replay::CapturedExecutionSource;
use jterm_engine::block_contract::CompletionProvenance;
pub use jterm_protocol::finished_output::OutputAnnotations;
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, io, path::Path};

const TEXT_VERSION: u32 = 1;
const STYLED_VERSION: u32 = 2;
pub const MAX_RECORDS: usize = 1024;
pub const MAX_RETAINED_TEXT_BYTES: usize = 2 * 1024 * 1024;
const MAX_CARD_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Completion {
    ShellReported,
    JournalRecovered,
    BoundaryInferred,
    Unknown,
}

impl From<CompletionProvenance> for Completion {
    fn from(value: CompletionProvenance) -> Self {
        match value {
            CompletionProvenance::ShellReported => Self::ShellReported,
            CompletionProvenance::JournalRecovered => Self::JournalRecovered,
            CompletionProvenance::BoundaryInferred => Self::BoundaryInferred,
            CompletionProvenance::Unknown => Self::Unknown,
        }
    }
}

impl From<Completion> for CompletionProvenance {
    fn from(value: Completion) -> Self {
        match value {
            Completion::ShellReported => Self::ShellReported,
            Completion::JournalRecovered => Self::JournalRecovered,
            Completion::BoundaryInferred => Self::BoundaryInferred,
            Completion::Unknown => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Output {
    pub text: String,
    pub truncated: bool,
    pub total_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<OutputAnnotations>,
}

impl Output {
    pub fn capture(
        text: &str,
        truncated: bool,
        total_bytes: u64,
        annotations: Option<&jterm_engine::archived_text::ArchiveAnnotations>,
        budget: usize,
    ) -> Option<Self> {
        let remaining = budget.checked_sub(text.len())?;
        let annotations = annotations
            .filter(|attributes| attributes.retained_bytes() <= remaining)
            .and_then(|attributes| OutputAnnotations::new(text, attributes));
        Some(Self {
            text: text.to_owned(),
            truncated,
            total_bytes,
            annotations,
        })
    }

    pub fn retained_bytes(&self) -> usize {
        self.text.len()
            + self
                .annotations
                .as_ref()
                .map_or(0, OutputAnnotations::retained_bytes)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Card {
    pub sequence: u64,
    pub shell_id: String,
    pub shell_session_id: Option<String>,
    pub shell_sequence: Option<u64>,
    pub shell_started_at_ms: Option<u64>,
    pub command: Option<String>,
    pub command_exact: bool,
    pub command_truncated: bool,
    pub cwd: Option<String>,
    pub cwd_after: Option<String>,
    pub exit_code: Option<i32>,
    pub duration_ms: Option<u64>,
    pub started_at_ms: Option<u64>,
    pub finished_at_ms: Option<u64>,
    pub start_mark_seen: bool,
    pub completion: Completion,
    pub source: CapturedExecutionSource,
    /// None means the original output is unavailable. An observed empty
    /// capture has an empty text field and retains its own provenance.
    pub output: Option<Output>,
}

impl Card {
    pub fn retained_bytes(&self) -> usize {
        self.shell_id.len()
            + self.shell_session_id.as_ref().map_or(0, String::len)
            + self.command.as_ref().map_or(0, String::len)
            + self.cwd.as_ref().map_or(0, String::len)
            + self.cwd_after.as_ref().map_or(0, String::len)
            + self.output.as_ref().map_or(0, Output::retained_bytes)
    }

    fn is_valid(&self) -> bool {
        use crate::execution_journal::{
            is_valid_jsh_cwd, MAX_COMMAND_BYTES, MAX_EXECUTION_ID_BYTES,
        };

        self.sequence > 0
            && self.source.is_valid()
            && !self.shell_id.is_empty()
            && self.shell_id.len() <= MAX_EXECUTION_ID_BYTES
            && !self.shell_id.chars().any(|character| {
                character.is_control()
                    || crate::review_input::is_terminal_visual_spoofing_character(character)
            })
            && self
                .shell_session_id
                .as_deref()
                .is_none_or(crate::execution_journal::is_valid_jsh_session_id)
            && self
                .command
                .as_ref()
                .is_none_or(|text| text.len() <= MAX_COMMAND_BYTES)
            && self.cwd.as_deref().is_none_or(is_valid_jsh_cwd)
            && self.cwd_after.as_deref().is_none_or(is_valid_jsh_cwd)
            && (matches!(
                self.completion,
                Completion::ShellReported | Completion::JournalRecovered
            ) || self.exit_code.is_none())
            && self.output.as_ref().is_none_or(|output| {
                output.text.len() <= MAX_CARD_OUTPUT_BYTES
                    && (output.truncated || output.text.len() as u64 <= output.total_bytes)
                    && output
                        .annotations
                        .as_ref()
                        .is_none_or(|attributes| attributes.clone().restore(&output.text).is_some())
            })
    }
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Checkpoint {
    version: u32,
    pane: String,
    next_sequence: u64,
    #[serde(deserialize_with = "decode_cards")]
    records: Vec<Card>,
}

fn decode_cards<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Vec<Card>, D::Error> {
    use serde::de::{Error, SeqAccess, Visitor};

    struct Cards;
    impl<'de> Visitor<'de> for Cards {
        type Value = Vec<Card>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a bounded sequence of completed cards")
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Self::Value, A::Error> {
            let mut cards = Vec::new();
            let mut budget = crate::bounded_json::TextBudget::new(MAX_RETAINED_TEXT_BYTES);
            while let Some(card) = sequence.next_element::<Card>()? {
                if cards.len() >= MAX_RECORDS {
                    return Err(A::Error::custom("too many completed cards"));
                }
                budget.charge("records", card.retained_bytes())?;
                cards.push(card);
            }
            Ok(cards)
        }
    }

    deserializer.deserialize_seq(Cards)
}

impl Checkpoint {
    pub fn new(pane: String, next_sequence: u64, records: Vec<Card>) -> io::Result<Self> {
        let checkpoint = Self {
            version: if records.iter().any(|record| {
                record
                    .output
                    .as_ref()
                    .is_some_and(|output| output.annotations.is_some())
            }) {
                STYLED_VERSION
            } else {
                TEXT_VERSION
            },
            pane,
            next_sequence,
            records,
        };
        checkpoint.validate(&checkpoint.pane)?;
        Ok(checkpoint)
    }

    pub fn into_records(self) -> (u64, Vec<Card>) {
        (self.next_sequence, self.records)
    }

    fn validate(&self, pane: &str) -> io::Result<()> {
        if !matches!(self.version, TEXT_VERSION | STYLED_VERSION) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "unsupported completed history version",
            ));
        }
        let mut executions = HashSet::new();
        let mut last_sequence = 0;
        let mut bytes = 0;
        let records_valid = self.records.len() <= MAX_RECORDS
            && self.records.iter().all(|record| {
                bytes += record.retained_bytes();
                let in_order =
                    record.sequence > last_sequence && record.sequence < self.next_sequence;
                last_sequence = record.sequence;
                record.is_valid()
                    && in_order
                    && executions.insert(record.source.execution_id())
                    && (self.version == STYLED_VERSION
                        || record
                            .output
                            .as_ref()
                            .is_none_or(|output| output.annotations.is_none()))
                    && bytes <= MAX_RETAINED_TEXT_BYTES
            });
        if self.pane != pane
            || !crate::execution_journal::is_valid_jsh_session_id(pane)
            || self.next_sequence == 0
            || !records_valid
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid completed history checkpoint",
            ));
        }
        Ok(())
    }

    pub fn load(path: &Path, pane: &str) -> io::Result<Option<Self>> {
        let contents = match crate::snapshot_file::read_bounded_private(path, MAX_FILE_BYTES) {
            Ok(contents) => contents,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let checkpoint: Self = serde_json::from_str(&contents).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "invalid completed history JSON")
        })?;
        checkpoint.validate(pane)?;
        Ok(Some(checkpoint))
    }

    /// The application holds the corresponding session-layout writer lock.
    /// Validate an existing file before replacement so versions that cannot
    /// be restored keep their original bytes even after new commands finish.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if Self::load(path, &self.pane)?
            .is_some_and(|current| current.next_sequence > self.next_sequence)
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "completed history cursor has advanced",
            ));
        }
        let contents = serde_json::to_vec(self).map_err(io::Error::other)?;
        if contents.len() as u64 > MAX_FILE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "completed history exceeds its byte budget",
            ));
        }
        crate::snapshot_file::write_atomic_private(path, &contents)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screen_replay::RunningExecutionSource;
    use jterm_engine::command_blocks::ShellMarkerClock;
    use uuid::Uuid;

    fn sample() -> Card {
        let mut clock = ShellMarkerClock::default();
        clock.begin_feed(160).unwrap();
        let start = clock.osc(Some(30), 13, true).unwrap();
        let end = clock.osc(Some(115), 12, false).unwrap();
        let mut source = RunningExecutionSource::new(start, (96, 32))
            .finish(Uuid::new_v4(), end)
            .unwrap();
        source.head_dropped = true;
        Card {
            sequence: 5,
            shell_id: "command-1".into(),
            shell_session_id: Some("persistent-session".into()),
            shell_sequence: Some(3),
            shell_started_at_ms: Some(1000),
            command: Some("printf hello".into()),
            command_exact: true,
            command_truncated: false,
            cwd: Some("/tmp".into()),
            cwd_after: None,
            exit_code: Some(1),
            duration_ms: Some(20),
            started_at_ms: Some(1000),
            finished_at_ms: Some(1020),
            start_mark_seen: true,
            completion: Completion::ShellReported,
            source: CapturedExecutionSource {
                execution: source,
                capture_size: (100, 32),
            },
            output: Some(Output {
                text: "last line\n".into(),
                truncated: true,
                total_bytes: 200,
                annotations: None,
            }),
        }
    }

    #[test]
    fn checkpoint_preserves_source_and_cursor_and_refuses_wrong_owner_or_future_version() {
        struct Scratch(std::path::PathBuf);
        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let directory =
            Scratch(std::env::temp_dir().join(format!("jterm-cards-{}", Uuid::new_v4())));
        let path = directory.0.join("pane.json");
        let pane = "persistent-session";
        let card = sample();
        let checkpoint = Checkpoint::new(pane.into(), 8, vec![card.clone()]).unwrap();
        checkpoint.save(&path).unwrap();
        assert_eq!(
            Checkpoint::load(&path, pane)
                .unwrap()
                .unwrap()
                .into_records(),
            (8, vec![card])
        );
        assert!(Checkpoint::load(&path, "different-pane").is_err());

        let stale = Checkpoint::new(pane.into(), 7, Vec::new()).unwrap();
        assert_eq!(
            stale.save(&path).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        let mut future = checkpoint;
        future.version = STYLED_VERSION + 1;
        let original = serde_json::to_vec(&future).unwrap();
        crate::snapshot_file::write_atomic_private(&path, &original).unwrap();
        assert_eq!(
            stale.save(&path).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[test]
    fn duplicate_execution_and_invalid_cursor_are_inadmissible() {
        let card = sample();
        let mut repeated = card.clone();
        repeated.sequence += 1;
        assert!(Checkpoint::new("pane-id".into(), 8, vec![card.clone(), repeated]).is_err());
        assert!(Checkpoint::new("pane-id".into(), card.sequence, vec![card]).is_err());
    }

    #[test]
    fn frost_sized_capture_remains_a_complete_projection() {
        let mut card = sample();
        let text = "x".repeat(MAX_CARD_OUTPUT_BYTES);
        card.output = Some(Output {
            total_bytes: text.len() as u64,
            text,
            truncated: false,
            annotations: None,
        });
        let checkpoint = Checkpoint::new("pane-id".into(), 8, vec![card]).unwrap();
        let decoded: Checkpoint =
            serde_json::from_slice(&serde_json::to_vec(&checkpoint).unwrap()).unwrap();
        assert_eq!(decoded, checkpoint);
    }

    #[test]
    fn annotated_snapshot_has_a_new_version_with_the_original_source() {
        use jterm_engine::{
            archived_text::{ArchiveAnnotationBuilder, ArchiveStyle},
            Color, StyleFlags,
        };
        let mut card = sample();
        let output = card.output.as_mut().unwrap();
        let mut attributes = ArchiveAnnotationBuilder::default();
        attributes.push(
            0..4,
            ArchiveStyle::from_rendition(
                Color::Rgb(21, 76, 191),
                Color::Default,
                Color::Default,
                StyleFlags::default(),
            ),
            None,
        );
        let attributes = attributes.finish().unwrap();
        output.annotations = OutputAnnotations::new(&output.text, &attributes);
        let styled = Checkpoint::new("pane-id".into(), 8, vec![card]).unwrap();
        assert_eq!(styled.version, STYLED_VERSION);
        let decoded: Checkpoint =
            serde_json::from_slice(&serde_json::to_vec(&styled).unwrap()).unwrap();
        decoded.validate("pane-id").unwrap();
        assert_eq!(decoded, styled);
        let unstyled_version = Checkpoint {
            version: TEXT_VERSION,
            ..decoded
        };
        assert!(unstyled_version.validate("pane-id").is_err());
    }
}
