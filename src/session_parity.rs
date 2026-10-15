//! Opt-in data and byte partitioning for cross-adapter production-path tests.
//!
//! This is not a lifecycle reducer: adapters feed their existing parser/owner
//! and compare observations with explicit fixture checkpoints. Local block IDs
//! are compared within an adapter, never numerically across implementations.

use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fixture {
    pub id: String,
    pub steps: Vec<Step>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub pty: String,
    pub expected: Expected,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expected {
    /// prompt, running, passthrough, or fullscreen.
    pub surface: String,
    /// Cumulative accepted completions, in order. Missing status stays None.
    pub completed: Vec<Completion>,
    /// Shell execution correlation ID, not a frontend-generated block ID.
    pub running_id: Option<String>,
    /// Exact visible shell draft row, when the fixture exercises draft redraw.
    pub draft_text: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Completion {
    pub id: String,
    pub command: String,
    pub exit_code: Option<i32>,
}

/// One authoritative corpus, embedded so consumers need no sibling checkout.
pub fn fixtures() -> Vec<Fixture> {
    serde_json::from_str(include_str!("../tests/fixtures/session_parity/corpus.json"))
        .expect("checked-in session parity fixtures are valid")
}

#[derive(Clone, Copy, Debug)]
pub enum Chunking {
    Whole,
    Bytewise,
    /// Alternating small byte widths deliberately split OSC/CSI and UTF-8.
    Staggered,
}

pub const CHUNKINGS: [Chunking; 3] = [Chunking::Whole, Chunking::Bytewise, Chunking::Staggered];

/// Partition bytes only. No decoding, event synthesis, or expected-state logic.
pub fn chunks(bytes: &[u8], chunking: Chunking) -> Vec<&[u8]> {
    match chunking {
        Chunking::Whole => vec![bytes],
        Chunking::Bytewise => bytes.chunks(1).collect(),
        Chunking::Staggered => {
            let mut rest = bytes;
            let mut result = Vec::new();
            for width in [2, 5, 1, 3].into_iter().cycle() {
                if rest.is_empty() {
                    break;
                }
                let (head, tail) = rest.split_at(width.min(rest.len()));
                result.push(head);
                rest = tail;
            }
            result
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corpus_is_unique_and_partitioning_is_lossless() {
        let fixtures = fixtures();
        assert_eq!(fixtures.len(), 8);
        let mut ids = std::collections::HashSet::new();
        for fixture in fixtures {
            assert!(ids.insert(fixture.id));
            assert!(!fixture.steps.is_empty());
            for step in fixture.steps {
                assert!(["prompt", "running", "passthrough", "fullscreen"]
                    .contains(&step.expected.surface.as_str()));
                for chunking in CHUNKINGS {
                    assert_eq!(
                        chunks(step.pty.as_bytes(), chunking).concat(),
                        step.pty.as_bytes()
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod parser_tests {
    use super::*;
    use crate::parser::{Parser, ParserEvent};

    fn observed(chunks: Vec<&[u8]>) -> Vec<ParserEvent> {
        let mut parser = Parser::new();
        let mut events = Vec::new();
        for chunk in chunks {
            parser.feed(chunk, &mut events);
        }
        // Chunk boundaries may split display Bytes events, but not their data.
        let mut merged = Vec::new();
        for event in events {
            match (merged.last_mut(), event) {
                (Some(ParserEvent::Bytes(previous)), ParserEvent::Bytes(bytes)) => {
                    previous.extend(bytes);
                }
                (_, event) => merged.push(event),
            }
        }
        merged
    }

    #[test]
    fn corpus_metadata_is_recognized_by_production_parser() {
        for fixture in fixtures() {
            let bytes: Vec<u8> = fixture
                .steps
                .iter()
                .flat_map(|step| step.pty.bytes())
                .collect();
            let events = observed(chunks(&bytes, Chunking::Whole));
            for completion in &fixture.steps.last().unwrap().expected.completed {
                assert!(
                    events.iter().any(|event| matches!(event,
                    ParserEvent::CommandStart(meta)
                    if meta.id.as_deref() == Some(completion.id.as_str())
                        && meta.command.as_deref() == Some(completion.command.as_str())
                        && meta.session_id.as_deref() == Some("parity-session")
                        && meta.seq == Some(1)
                        && meta.started_at_ms == Some(1700000000000))),
                    "{}: valid exact Start envelope",
                    fixture.id
                );
                assert!(
                    events.iter().any(|event| matches!(event,
                    ParserEvent::CommandEnd { exit, meta }
                    if meta.id.as_deref() == Some(completion.id.as_str())
                        && *exit == completion.exit_code)),
                    "{}: correlated End with exact status",
                    fixture.id
                );
            }
        }
    }

    #[test]
    fn shared_production_parser_is_chunk_invariant_for_corpus() {
        for fixture in fixtures() {
            let bytes: Vec<u8> = fixture
                .steps
                .iter()
                .flat_map(|step| step.pty.bytes())
                .collect();
            let baseline = observed(chunks(&bytes, Chunking::Whole));
            for chunking in CHUNKINGS {
                assert_eq!(
                    observed(chunks(&bytes, chunking)),
                    baseline,
                    "{} {chunking:?}",
                    fixture.id
                );
            }
        }
    }
}
