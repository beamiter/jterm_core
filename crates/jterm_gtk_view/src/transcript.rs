use crate::{EmbeddedTerminal, HistoryAnchor, Pty};
use jterm_engine::terminal::{Terminal, TranscriptAnchor};
use std::{cell::RefCell, collections::BTreeMap, ops::Range, rc::Rc};

/// Whether a zone was admitted from this session's lifecycle or re-rendered
/// from the application's persisted display snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TranscriptZoneOrigin {
    Live,
    Restored,
}

/// Sources of one admitted command zone in the continuous primary screen.
/// An empty half-open range describes a boundary that drew no cells.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranscriptZone {
    pub origin: TranscriptZoneOrigin,
    pub prompt: Range<TranscriptAnchor>,
    pub input: Range<TranscriptAnchor>,
    pub output: Range<TranscriptAnchor>,
}

/// One visible row belonging to a verified transcript source span.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TranscriptRowChrome {
    pub id: u64,
    pub row: usize,
    pub heading: bool,
    pub completed: bool,
    /// First clear column at the right edge. Images and an unknown paint
    /// geometry make a badge inadmissible on the row.
    pub blank_tail: Option<usize>,
}

struct PendingZone {
    origin: TranscriptZoneOrigin,
    id: u64,
    prompt: TranscriptAnchor,
    input: Option<TranscriptAnchor>,
    output: Option<TranscriptAnchor>,
}

#[derive(Default)]
struct Zones {
    pending: Option<PendingZone>,
    completed: BTreeMap<u64, TranscriptZone>,
}

/// The same terminal and PTY remain mounted through prompt, command and
/// alternate-screen activity. The host admits shell lifecycle markers and
/// passes their record IDs here. Guest output cannot insert a zone itself.
pub struct TranscriptView<P: Pty> {
    terminal: Rc<EmbeddedTerminal<P>>,
    zones: RefCell<Zones>,
}

impl<P: Pty> TranscriptView<P> {
    pub fn new(terminal: Rc<EmbeddedTerminal<P>>) -> Self {
        terminal.enable_transcript_sources();
        Self {
            terminal,
            zones: RefCell::default(),
        }
    }

    pub fn cursor(&self) -> Option<TranscriptAnchor> {
        self.terminal.inspect(Terminal::transcript_cursor)
    }

    pub fn begin_prompt(&self, id: u64) {
        self.begin_zone(id, TranscriptZoneOrigin::Live);
    }

    pub fn begin_restored_prompt(&self, id: u64) {
        self.begin_zone(id, TranscriptZoneOrigin::Restored);
    }

    fn begin_zone(&self, id: u64, origin: TranscriptZoneOrigin) {
        let cursor = self.cursor();
        let mut zones = self.zones.borrow_mut();
        zones.pending = cursor.map(|prompt| PendingZone {
            id,
            origin,
            prompt,
            input: None,
            output: None,
        });
    }

    pub fn end_prompt(&self) {
        if let Some(zone) = self.zones.borrow_mut().pending.as_mut() {
            zone.input = self.cursor();
        }
    }

    pub fn begin_output(&self, id: Option<u64>) {
        let mut zones = self.zones.borrow_mut();
        if let Some(zone) = zones.pending.as_mut().filter(|zone| id == Some(zone.id)) {
            zone.output = self.cursor();
        } else {
            zones.pending = None;
        }
    }

    pub fn end_output(&self, id: Option<u64>, limit: usize) {
        let mut zones = self.zones.borrow_mut();
        let Some(pending) = zones.pending.take().filter(|zone| id == Some(zone.id)) else {
            return;
        };
        let (Some(input), Some(output), Some(end)) = (pending.input, pending.output, self.cursor())
        else {
            return;
        };
        let completed = TranscriptZone {
            origin: pending.origin,
            prompt: pending.prompt..input,
            input: input..output,
            output: output..end,
        };
        if limit != 0 && self.coordinates(&completed).is_some() {
            zones.completed.insert(pending.id, completed);
        }
        while zones.completed.len() > limit {
            zones.completed.pop_first();
        }
    }

    pub fn retire(&self, ids: &[u64]) {
        let mut zones = self.zones.borrow_mut();
        for id in ids {
            zones.completed.remove(id);
        }
    }

    pub fn source_zone(&self, id: u64) -> Option<TranscriptZone> {
        let zone = self.zones.borrow().completed.get(&id)?.clone();
        self.coordinates(&zone)?;
        Some(zone)
    }

    pub fn output_text(&self, id: u64) -> Option<String> {
        let zone = self.zones.borrow().completed.get(&id)?.clone();
        let [_, _, start, end] = self.coordinates(&zone)?;
        self.terminal
            .inspect(|terminal| terminal.primary_text_range(start, end).ok())
    }

    /// The exact visual row where the retained command began. The heading
    /// can still be shown when other ends of the range have been clipped or
    /// discarded. The full source range independently verifies all endpoints.
    pub fn zone_scroll_row(&self, id: u64) -> Option<usize> {
        let start = self.heading(id)?;
        self.terminal
            .inspect(|terminal| Some(terminal.history_layout().ok()?.row_at(start)))
    }

    pub fn reveal_zone(&self, id: u64) -> bool {
        self.heading(id)
            .is_some_and(|start| self.terminal.reveal_transcript_source(start))
    }

    /// Redraw decorations after the same frame has reached the text and image layers.
    pub fn on_painted(&self, observer: impl Fn() + 'static) -> bool {
        self.terminal.when_transcript_painted(observer)
    }

    pub fn painted_zone_rows(&self) -> Vec<TranscriptRowChrome> {
        self.terminal
            .with_painted_transcript(|terminal, viewport| {
                let zones = self.zones.borrow();
                let mut rows = vec![None; viewport.rows];
                let mut mark =
                    |id, completed, start: TranscriptAnchor, end: Option<TranscriptAnchor>| {
                        let Some(start) = terminal.locate_transcript_source(start) else {
                            return;
                        };
                        let Some((first, _)) = viewport.locate(start) else {
                            return;
                        };
                        let last = end
                            .and_then(|end| terminal.locate_transcript_source(end))
                            .filter(|end| *end >= start)
                            .and_then(|end| viewport.locate(end))
                            .map_or(first, |(row, boundary)| {
                                row.saturating_sub(usize::from(boundary)).max(first)
                            });
                        let top = viewport.top;
                        let bottom = top + viewport.rows;
                        for row in first.max(top)..last.saturating_add(1).min(bottom) {
                            let row_index = row - top;
                            rows[row_index].get_or_insert_with(|| TranscriptRowChrome {
                                id,
                                row: row_index,
                                heading: row == first,
                                completed,
                                blank_tail: viewport.blank_tail(row_index),
                            });
                        }
                    };
                // Later command boundaries own a row shared by two zones.
                if let Some(pending) = &zones.pending {
                    mark(
                        pending.id,
                        false,
                        pending.prompt,
                        terminal.transcript_cursor(),
                    );
                }
                for (&id, zone) in zones.completed.iter().rev() {
                    mark(id, true, zone.prompt.start, Some(zone.output.end));
                }
                rows.into_iter().flatten().collect()
            })
            .unwrap_or_default()
    }

    fn heading(&self, id: u64) -> Option<HistoryAnchor> {
        let start = self.zones.borrow().completed.get(&id)?.prompt.start;
        self.terminal
            .inspect(|terminal| terminal.locate_transcript_source(start))
    }

    /// GTK widget-relative hit testing for the actual painted frame. A
    /// pending repaint has no source result until the matching frame arrives.
    pub fn source_at(&self, x: f64, y: f64) -> Option<TranscriptAnchor> {
        self.terminal.painted_transcript_source(x, y)
    }

    fn coordinates(&self, zone: &TranscriptZone) -> Option<[HistoryAnchor; 4]> {
        self.terminal.inspect(|terminal| {
            let start = terminal.locate_transcript_source(zone.prompt.start)?;
            let input = terminal.locate_transcript_source(zone.input.start)?;
            let output = terminal.locate_transcript_source(zone.output.start)?;
            let end = terminal.locate_transcript_source(zone.output.end)?;
            (start <= input && input <= output && output <= end)
                .then_some([start, input, output, end])
        })
    }
}
