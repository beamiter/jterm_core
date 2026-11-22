//! Bounded read-only records from opt-in observed PTY shell markers.
use crate::{
    block_contract::{classify_completed, CompletedBlockOutcome, CompletionProvenance},
    parser::{Event, StringKind},
    shell_markers::{self, ShellMarker},
};
use std::collections::VecDeque;
use unicode_width::UnicodeWidthChar;

const RETAINED_BLOCKS: usize = 8;
const MAX_OUTPUT_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputUnavailable {
    ScreenOperation,
    ByteLimit,
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
    carriage_return: bool,
}

impl Active {
    fn append(&mut self, text: &str) {
        if let Ok(output) = &mut self.output {
            if output.len() + text.len() > MAX_OUTPUT_BYTES {
                self.output = Err(OutputUnavailable::ByteLimit);
            } else {
                output.push_str(text);
            }
        }
    }

    fn unavailable(&mut self) {
        if self.output.is_ok() {
            self.output = Err(OutputUnavailable::ScreenOperation);
        }
    }

    fn observe_output(&mut self, event: &Event<'_>) {
        if self.output.is_err() {
            return;
        }
        match event {
            Event::Ascii(bytes) if !self.carriage_return => {
                self.append(std::str::from_utf8(bytes).expect("ASCII parser run"));
            }
            Event::Print(ch) if !self.carriage_return && matches!(ch.width(), Some(1 | 2)) => {
                self.append(ch.encode_utf8(&mut [0; 4]));
            }
            Event::Control(b'\t') if !self.carriage_return => self.append("\t"),
            Event::Control(b'\n') => {
                self.append("\n");
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
            // Cursor edits, graphics, fullscreen and other operations cannot
            // be represented faithfully by this line-oriented output view.
            _ => self.unavailable(),
        }
    }
}

#[derive(Default)]
pub(crate) struct CommandBlocks {
    prompt: Prompt,
    input_submitted: bool,
    active: Option<Active>,
    completed: VecDeque<CommandBlock>,
    sequence: u64,
}

impl CommandBlocks {
    pub(crate) fn prompt_ready(&self) -> bool {
        matches!(self.prompt, Prompt::Ready) && !self.input_submitted && self.active.is_none()
    }

    pub(crate) fn input_submitted(&mut self) {
        self.input_submitted = true;
    }

    pub(crate) fn completed(&self) -> &VecDeque<CommandBlock> {
        &self.completed
    }

    pub(crate) fn screen_changed(&mut self) {
        if let Some(active) = &mut self.active {
            active.unavailable();
        }
    }

    /// Return true when the view consumed a lifecycle packet.
    pub(crate) fn observe(&mut self, event: &Event<'_>, alternate: bool) -> bool {
        if alternate {
            self.prompt = Prompt::None;
            self.screen_changed();
            return false;
        }
        if let Event::String {
            kind: StringKind::Osc,
            payload,
            ..
        } = event
        {
            if let Some(marker) = shell_markers::decode(payload, hidden_label_character) {
                self.observe_marker(marker);
                return true;
            }
        }
        if let Some(active) = &mut self.active {
            active.observe_output(event);
        }
        false
    }

    fn observe_marker(&mut self, marker: ShellMarker) {
        match marker {
            ShellMarker::PromptStart => {
                self.active = None;
                self.prompt = Prompt::Started;
            }
            ShellMarker::PromptEnd => {
                if matches!(self.prompt, Prompt::Started) {
                    self.prompt = Prompt::Ready;
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
                                carriage_return: false,
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
                self.completed.push_back(CommandBlock {
                    sequence,
                    execution_id: active.execution_id,
                    shell_session: active.shell_session,
                    shell_sequence: active.shell_sequence,
                    outcome: classify_completed(Some(&active.command), exit),
                    command: active.command,
                    cwd: active.cwd,
                    provenance: CompletionProvenance::ShellReported,
                    output: active.output,
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
