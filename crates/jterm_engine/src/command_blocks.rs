//! Bounded read-only records from opt-in observed PTY shell markers.
use crate::{
    archived_text::{ArchiveAnnotationBuilder, ArchiveAnnotations, ArchiveStyle},
    block_contract::{classify_completed, CompletedBlockOutcome, CompletionProvenance},
    parser::{Event, StringKind},
    shell_markers::{self, ShellMarker},
};
use std::{collections::VecDeque, ops::Range};
use unicode_width::UnicodeWidthChar;

const RETAINED_BLOCKS: usize = 8;
const MAX_OUTPUT_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputUnavailable {
    ScreenOperation,
    ByteLimit,
}

/// Original bytes between the accepted command-start and command-end marks.
/// The offsets are relative to this terminal's observed PTY stream. They
/// locate the source, even when presentation has omitted control sequences
/// or could not retain any text from that range.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandOutputSource {
    pub bytes: Range<u64>,
    pub start_event: u64,
    pub end_event: u64,
    /// Columns and rows when the corresponding mark was consumed.
    pub start_size: (usize, usize),
    pub end_size: (usize, usize),
    pub resized: bool,
}

/// A marker's location in its source stream, independent of PTY read boundaries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShellMarkerPosition {
    sequence: u64,
    start: u64,
    end: u64,
}

impl ShellMarkerPosition {
    pub fn sequence(self) -> u64 {
        self.sequence
    }

    pub fn packet_bytes(self) -> Range<u64> {
        self.start..self.end
    }
}

/// Position clock for the shell marker observer owned by one PTY.
#[derive(Default)]
pub struct ShellMarkerClock {
    offset: u64,
    sequence: u64,
}

impl ShellMarkerClock {
    /// Advance the observed byte position and return this feed's starting offset.
    pub fn begin_feed(&mut self, len: usize) -> Option<u64> {
        let base = self.offset;
        self.offset = base.saturating_add(len as u64);
        (self.offset < u64::MAX).then_some(base)
    }

    /// Count one recognized OSC lifecycle marker at the exclusive packet end.
    pub fn osc(
        &mut self,
        end: Option<u64>,
        payload_len: usize,
        bel: bool,
    ) -> Option<ShellMarkerPosition> {
        let sequence = self.sequence.checked_add(1)?;
        self.sequence = sequence;
        let end = end.filter(|end| *end <= self.offset)?;
        // Both OSC terminators are accepted across input splits. The parser
        // provides the original, unmodified OSC payload.
        let framed_len = (payload_len as u64).checked_add(if bel { 3 } else { 4 })?;
        Some(ShellMarkerPosition {
            sequence,
            start: end.checked_sub(framed_len)?,
            end,
        })
    }
}

/// A report from the PTY stream. Every field is display data; the PTY does
/// not authenticate the sender of a marker or authorize any follow-up action.
#[derive(Clone, Debug)]
pub struct CommandBlock {
    pub sequence: u64,
    pub execution_id: String,
    pub shell_session: Option<String>,
    pub shell_sequence: Option<u64>,
    pub command: String,
    pub cwd: Option<String>,
    pub outcome: CompletedBlockOutcome,
    pub provenance: CompletionProvenance,
    pub output: Result<String, OutputUnavailable>,
    pub output_annotations: Option<ArchiveAnnotations>,
    pub output_source: Option<CommandOutputSource>,
}

#[derive(Default)]
enum Prompt {
    #[default]
    None,
    Started,
    Ready,
}

struct Active {
    execution_id: String,
    shell_session: Option<String>,
    shell_sequence: Option<u64>,
    command: String,
    cwd: Option<String>,
    output: Result<String, OutputUnavailable>,
    output_annotations: ArchiveAnnotationBuilder,
    carriage_return: bool,
    suspended_alternate: bool,
    start_position: Option<ShellMarkerPosition>,
    start_size: (usize, usize),
    resized: bool,
}

impl Active {
    fn append(&mut self, text: &str, style: ArchiveStyle, target: Option<&str>) {
        if let Ok(output) = &mut self.output {
            if output.len() + text.len() > MAX_OUTPUT_BYTES {
                self.output = Err(OutputUnavailable::ByteLimit);
            } else {
                let start = output.len();
                output.push_str(text);
                self.output_annotations
                    .push(start..output.len(), style, target);
            }
        }
    }

    fn unavailable(&mut self) {
        if self.output.is_ok() {
            self.output = Err(OutputUnavailable::ScreenOperation);
        }
    }

    fn observe_output(&mut self, event: &Event<'_>, style: ArchiveStyle, target: Option<&str>) {
        if self.output.is_err() {
            return;
        }
        match event {
            Event::Ascii(bytes) if !self.carriage_return => {
                self.append(
                    std::str::from_utf8(bytes).expect("ASCII parser run"),
                    style,
                    target,
                );
            }
            Event::Print(ch) if !self.carriage_return && matches!(ch.width(), Some(1 | 2)) => {
                self.append(ch.encode_utf8(&mut [0; 4]), style, target);
            }
            Event::Control(b'\t') if !self.carriage_return => self.append("\t", style, target),
            Event::Control(b'\n') => {
                self.append("\n", ArchiveStyle::default(), None);
                self.carriage_return = false;
            }
            Event::Control(b'\r') => {
                if self
                    .output
                    .as_ref()
                    .is_ok_and(|text| !text.is_empty() && !text.ends_with('\n'))
                {
                    self.carriage_return = true;
                }
            }
            Event::Boundary | Event::Control(0 | 7) => {}
            Event::Csi {
                private_prefix: None,
                intermediates,
                final_byte: b'm',
                ..
            } if intermediates.is_empty() => {}
            Event::String {
                kind: StringKind::Osc,
                payload,
                ..
            } if payload.starts_with(b"1337;CurrentDir=")
                || [b"0;", b"1;", b"2;", b"7;", b"8;"]
                    .iter()
                    .any(|prefix| payload.starts_with(*prefix)) => {}
            // Cursor visibility, keyboard protocol and title-stack cleanup
            // emitted after Vim leaves 1049 do not rewrite main-screen text.
            Event::Csi {
                private_prefix: Some(b'?'),
                params,
                intermediates,
                final_byte: b'h' | b'l',
                ..
            } if intermediates.is_empty() && params.as_slice() == [25] => {}
            Event::Csi {
                private_prefix: Some(b'>'),
                params,
                intermediates,
                final_byte: b'm',
                ..
            } if intermediates.is_empty()
                && params.first() == Some(&4)
                && params.len() <= 2
                && params.get(1).copied().unwrap_or(0) <= 2 => {}
            Event::Csi {
                private_prefix: None,
                params,
                intermediates,
                final_byte: b't',
                ..
            } if intermediates.is_empty() && matches!(params.as_slice(), [22 | 23, 0, 0]) => {}
            // Cursor edits, graphics, fullscreen and other operations cannot
            // be represented faithfully by this line-oriented output view.
            _ => self.unavailable(),
        }
    }
}

#[derive(Default)]
pub(crate) struct CommandBlocks {
    prompt: Prompt,
    prompt_generation: u64,
    input_submitted: bool,
    active: Option<Active>,
    completed: VecDeque<CommandBlock>,
    sequence: u64,
    markers: ShellMarkerClock,
}

impl CommandBlocks {
    pub(crate) fn prompt_ready(&self) -> bool {
        matches!(self.prompt, Prompt::Ready) && !self.input_submitted && self.active.is_none()
    }

    pub(crate) fn prompt_generation(&self) -> Option<u64> {
        (self.prompt_ready() && self.prompt_generation < u64::MAX).then_some(self.prompt_generation)
    }

    pub(crate) fn command_running(&self) -> bool {
        self.active.is_some()
    }

    pub(crate) fn input_submitted(&mut self) {
        self.input_submitted = true;
    }

    pub(crate) fn completed(&self) -> &VecDeque<CommandBlock> {
        &self.completed
    }

    pub(crate) fn resized(&mut self) {
        if let Some(active) = &mut self.active {
            active.resized = true;
            active.unavailable();
        }
    }

    pub(crate) fn begin_feed(&mut self, len: usize) -> Option<u64> {
        self.markers.begin_feed(len)
    }

    pub(crate) fn reset_live(&mut self) {
        self.prompt = Prompt::None;
        self.input_submitted = false;
        self.active = None;
    }

    /// Return true when the view consumed a lifecycle packet.
    pub(crate) fn observe(
        &mut self,
        event: &Event<'_>,
        alternate: bool,
        event_end: Option<u64>,
        size: (usize, usize),
        style: ArchiveStyle,
        target: Option<&str>,
    ) -> bool {
        // Only the cursor-saving 1049 excursion is a lossless boundary for
        // this main-screen line archive. Alternate bytes (including forged
        // lifecycle OSCs) never belong to the completed command output.
        let excursion = match event {
            Event::Csi {
                private_prefix: Some(b'?'),
                params,
                intermediates,
                final_byte,
                ..
            } if intermediates.is_empty() && params.as_slice() == [1049] => Some(*final_byte),
            _ => None,
        };
        if alternate {
            self.prompt = Prompt::None;
            if let Some(active) = &mut self.active {
                if !active.suspended_alternate {
                    active.unavailable();
                } else if excursion == Some(b'l') {
                    active.suspended_alternate = false;
                }
            }
            return false;
        }
        if let Some(active) = &mut self.active {
            if active.suspended_alternate {
                // The terminal returned through an unsupported switch/reset.
                active.suspended_alternate = false;
                active.unavailable();
            }
            if excursion == Some(b'h') {
                active.suspended_alternate = true;
                return false;
            }
        }
        if let Event::String {
            kind: StringKind::Osc,
            payload,
            bel_terminated,
        } = event
        {
            if let Some(marker) = shell_markers::decode(payload, hidden_label_character) {
                let position = self.markers.osc(event_end, payload.len(), *bel_terminated);
                self.observe_marker(marker, position, size);
                return true;
            }
        }
        if let Some(active) = &mut self.active {
            active.observe_output(event, style, target);
        }
        false
    }

    fn observe_marker(
        &mut self,
        marker: ShellMarker,
        position: Option<ShellMarkerPosition>,
        size: (usize, usize),
    ) {
        match marker {
            ShellMarker::PromptStart => {
                self.active = None;
                self.prompt = Prompt::Started;
            }
            ShellMarker::PromptEnd => {
                if matches!(self.prompt, Prompt::Started) {
                    self.prompt = Prompt::Ready;
                    self.prompt_generation = self.prompt_generation.saturating_add(1);
                    self.input_submitted = false;
                }
            }
            ShellMarker::CommandStart(meta) => {
                self.active = None;
                if matches!(std::mem::take(&mut self.prompt), Prompt::Ready) {
                    if let (Some(execution_id), Some(command)) = (meta.id, meta.command) {
                        if !command.trim().is_empty() {
                            self.active = Some(Active {
                                execution_id,
                                shell_session: meta.session_id,
                                shell_sequence: meta.seq,
                                command,
                                cwd: meta.cwd,
                                output: Ok(String::new()),
                                output_annotations: ArchiveAnnotationBuilder::default(),
                                carriage_return: false,
                                suspended_alternate: false,
                                start_position: position,
                                start_size: size,
                                resized: false,
                            });
                        }
                    }
                }
            }
            ShellMarker::CommandEnd { exit, meta } => {
                self.prompt = Prompt::None;
                let Some(active) = self.active.take() else {
                    return;
                };
                if meta.id.as_deref() != Some(active.execution_id.as_str()) {
                    return;
                }
                let Some(sequence) = self.sequence.checked_add(1) else {
                    return;
                };
                self.sequence = sequence;
                if self.completed.len() >= RETAINED_BLOCKS {
                    self.completed.pop_front();
                }
                let output_source = active
                    .start_position
                    .zip(position)
                    .filter(|(start, end)| start.end <= end.start && start.sequence < end.sequence)
                    .map(|(start, end)| CommandOutputSource {
                        bytes: start.end..end.start,
                        start_event: start.sequence,
                        end_event: end.sequence,
                        start_size: active.start_size,
                        end_size: size,
                        resized: active.resized,
                    });
                self.completed.push_back(CommandBlock {
                    sequence,
                    execution_id: active.execution_id,
                    shell_session: active.shell_session,
                    shell_sequence: active.shell_sequence,
                    outcome: classify_completed(Some(&active.command), exit),
                    command: active.command,
                    cwd: active.cwd,
                    provenance: CompletionProvenance::ShellReported,
                    output_annotations: active
                        .output
                        .is_ok()
                        .then(|| active.output_annotations.finish())
                        .flatten(),
                    output: active.output,
                    output_source,
                });
            }
        }
    }
}

fn hidden_label_character(ch: char) -> bool {
    // The standalone renderer cannot display multiple scalars in one cell.
    // Omit ambiguous packet labels rather than rendering a partial command.
    ch != ' ' && (ch.is_whitespace() || !matches!(ch.width(), Some(1 | 2)))
}
