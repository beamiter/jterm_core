//! Bounded terminal owner for standalone sessions.

use crate::{
    command_blocks::{CommandBlock, CommandBlocks},
    frame::{underline_color_at, CellFrame, Cursor, Limits, Row, Screen, Stamp},
    graphemes::{self, RowClusters, MAX_CLUSTER_BYTES, MAX_ROW_CLUSTER_BYTES},
    grid::{PrintCheckpoint, WrapCheckpoint},
    hyperlink::{resolve_resource, HyperlinkPolicy, HyperlinkResources, HyperlinkTable},
    parser::{Event, Parser, ParserLimits, ParserProfile, StringKind},
    protocol_state::ProtocolState,
    screen::{RegionDirection, RegionMutation, ScreenState, VerticalMotion},
    semantics::{self, Dispatch, Effect, UnderlinePen},
    Color, HyperlinkId, RawRowId, ScrollbackLine, UnderlineColorRun,
};
use std::{
    collections::VecDeque,
    num::NonZeroU64,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use unicode_width::UnicodeWidthChar;

const MAX_ROWS: usize = 128;
const MAX_COLS: usize = 256;
/// Maximum UTF-8 bytes admitted by one encoded input operation.
pub const MAX_INPUT_BYTES: usize = 4096;
/// Maximum bytes consumed or returned as replies by a single feed.
pub const MAX_FEED_BYTES: usize = 65536;
fn dimensions(rows: usize, cols: usize) -> Result<(), String> {
    if rows == 0 || cols == 0 || rows > MAX_ROWS || cols > MAX_COLS {
        Err("viewport out of bounds".into())
    } else {
        Ok(())
    }
}

const MAX_HISTORY_ROWS: usize = 1024;
const MAX_COPY_BYTES: usize = MAX_ROWS * (MAX_COLS * 4 + MAX_ROW_CLUSTER_BYTES + 1);
const MAX_HYPERLINK_URL_BYTES: usize = 1024 * 1024;
/// Owned immutable primary-history snapshot. Coordinates address original
/// physical columns, not UTF-8 byte indices. It remains valid after live output.
pub struct HistoryPage {
    pub revision: u64,
    pub offset: usize,
    pub total_rows: usize,
    pub row_count: usize,
    pub text: String,
    rows: Vec<HistoryCopyRow>,
    hyperlinks: Option<HyperlinkResources>,
}
struct HistoryCopyRow {
    // Physical addressing is unchanged. Only extraction may omit this slot.
    row: Row,
    underline_colors: Option<Arc<[UnderlineColorRun]>>,
    clusters: Option<Arc<RowClusters>>,
    synthetic_gap: Option<usize>,
}
impl HistoryPage {
    /// Physical row and style snapshot for a viewport. Width can differ
    /// between rows retained before and after a resize.
    pub fn row(&self, index: usize) -> Option<&Row> {
        self.rows.get(index).map(|row| &row.row)
    }

    /// Resolve a validated target referenced by this immutable history page.
    pub fn hyperlink_target(&self, id: HyperlinkId) -> Option<&str> {
        resolve_resource(&self.hyperlinks, id)
    }

    /// Complete text at a multi-scalar grapheme's original physical column.
    pub fn cluster_text(&self, row: usize, column: usize) -> Option<&str> {
        self.rows.get(row)?.clusters.as_ref()?.text(column)
    }

    /// Explicit underline color at an original physical history coordinate.
    pub fn underline_color(&self, row: usize, col: usize) -> Option<Color> {
        underline_color_at(self.underline_color_runs(row), col)
    }

    pub fn underline_color_runs(&self, row: usize) -> &[UnderlineColorRun] {
        self.rows
            .get(row)
            .and_then(|row| row.underline_colors.as_deref())
            .unwrap_or_default()
    }

    /// Half-open lexicographic range. End may be (row_count, 0) for whole page.
    /// Wide continuations and certified synthetic margin gaps emit no text.
    pub fn selection_text(
        &self,
        start_row: usize,
        start_col: usize,
        end_row: usize,
        end_col: usize,
    ) -> Result<String, String> {
        let valid = |r: usize, c: usize| {
            r < self.rows.len() && c <= self.rows[r].row.cells.len()
                || r == self.rows.len() && c == 0
        };
        if !valid(start_row, start_col)
            || !valid(end_row, end_col)
            || (start_row, start_col) > (end_row, end_col)
        {
            return Err("selection out of bounds".into());
        }
        let mut text = String::new();
        for row in start_row..self.rows.len().min(end_row.saturating_add(1)) {
            let cells = &self.rows[row].row.cells;
            let start = if row == start_row { start_col } else { 0 };
            let end = if row == end_row { end_col } else { cells.len() };
            for (offset, cell) in cells[start..end].iter().enumerate() {
                if !cell.flags.wide_continuation()
                    && self.rows[row].synthetic_gap != Some(start + offset)
                {
                    if let Some(cluster) = self.cluster_text(row, start + offset) {
                        text.push_str(cluster);
                    } else {
                        text.push(cell.character);
                    }
                }
            }
            if row < end_row && !self.rows[row].row.wrapped {
                // Physical trailing blank padding is not copied across a hard line.
                while text.ends_with(' ') {
                    text.pop();
                }
                if row + 1 < self.rows.len() {
                    text.push('\n');
                }
            }
            if text.len() > MAX_COPY_BYTES {
                return Err("selection byte limit".into());
            }
        }
        Ok(text)
    }
}

/// Single owner of an incremental parser, screen, modes, history and frame revisions.
///
/// This initial profile preserves the Aether standalone terminal contract. It
/// accepts bounded control strings, applies ordered local effects, and returns
/// reply bytes to the caller. PTY I/O and effects with external side effects
/// belong to the owning session.
pub struct Terminal {
    epoch: NonZeroU64,
    parser: Parser,
    screen: ScreenState,
    protocol: ProtocolState,
    underline_pen: Option<UnderlinePen>,
    hyperlinks: Option<(HyperlinkPolicy, HyperlinkTable)>,
    graphemes: Option<GraphemeInput>,
    revision: u64,
    layout: u64,
    unsupported: u64,
    unsupported_events: Vec<String>,
    negative_query_replies: u64,
    history: VecDeque<ScrollbackLine>,
    history_end: u64,
    command_blocks: Option<CommandBlocks>,
    sync_started: Option<Instant>,
    capture_sync_end: bool,
    completed_sync_frame: Option<CellFrame>,
    last_frame: Mutex<Option<CellFrame>>,
}

#[derive(Default)]
struct GraphemeInput {
    active: Option<GraphicPosition>,
    provisional: Option<ProvisionalPrint>,
    last_cluster: Option<RetainedGrapheme>,
}

struct ProvisionalPrint {
    row: usize,
    col: usize,
    footprint: PrintCheckpoint,
    departure: Option<WrapCheckpoint>,
}

#[derive(Clone, Copy)]
struct GraphicPosition {
    row: usize,
    col: usize,
    width: usize,
}

#[derive(Clone)]
struct RetainedGrapheme {
    text: Arc<str>,
    width: usize,
}

impl Terminal {
    /// Construct a standalone terminal. An attached session should pass its epoch.
    pub fn new(rows: usize, cols: usize) -> Result<Self, String> {
        Self::with_epoch(rows, cols, NonZeroU64::MIN)
    }

    pub fn with_epoch(rows: usize, cols: usize, epoch: NonZeroU64) -> Result<Self, String> {
        dimensions(rows, cols)?;
        let mut protocol = ProtocolState::default();
        protocol.modes_mut().insert(7);
        protocol.modes_mut().insert(25);
        Ok(Self {
            epoch,
            parser: Parser::with_bounded_profile(
                ParserLimits {
                    pending_bytes: 4096,
                    kitty_recovery_bytes: 4096,
                },
                ParserProfile::EmberV1,
            )?,
            screen: ScreenState::new(rows, cols),
            protocol,
            underline_pen: None,
            hyperlinks: None,
            graphemes: None,
            revision: 1,
            layout: 1,
            unsupported: 0,
            unsupported_events: Vec::new(),
            negative_query_replies: 0,
            history: VecDeque::new(),
            history_end: 0,
            command_blocks: None,
            sync_started: None,
            capture_sync_end: false,
            completed_sync_frame: None,
            last_frame: Mutex::new(None),
        })
    }

    /// Retain SGR 58/59 and expose explicit colors in immutable paint/history.
    /// A session that can render these colors opts in before its first feed.
    pub fn enable_underline_colors(&mut self) {
        self.underline_pen.get_or_insert_with(UnderlinePen::default);
    }

    /// Retain validated OSC 8 targets for this session before its first feed.
    /// The first installed policy remains active for the lifetime of the session.
    pub fn enable_hyperlinks(&mut self, policy: HyperlinkPolicy) {
        self.hyperlinks.get_or_insert_with(|| {
            (
                policy,
                HyperlinkTable::with_url_byte_limit(MAX_HYPERLINK_URL_BYTES),
            )
        });
    }

    /// Preserve streaming grapheme continuations in paint and history.
    /// Opt in before feeding output. Presentation selectors can resize a
    /// provisional glyph while keeping committed scrolling effects.
    pub fn enable_grapheme_clusters(&mut self) {
        self.graphemes.get_or_insert_with(GraphemeInput::default);
    }

    fn finish_grapheme(&mut self) {
        if let Some(input) = &mut self.graphemes {
            input.active = None;
            input.provisional = None;
        }
    }

    fn hyperlink_snapshot<'a>(
        &self,
        rows: impl Iterator<Item = &'a Row>,
    ) -> Option<HyperlinkResources> {
        let (_, table) = self.hyperlinks.as_ref()?;
        table.snapshot(rows.flat_map(|row| row.cells.iter().map(|cell| cell.hyperlink_id)))
    }

    /// Paint publication only; parsing, PTY draining and replies remain live.
    /// Repeated begin does not extend the first-begin 250ms safety deadline.
    pub fn publication_held(&self) -> bool {
        self.publication_held_at(Instant::now())
    }

    /// Evaluate the publish barrier using the session's monotonic clock.
    pub fn publication_held_at(&self, now: Instant) -> bool {
        self.sync_started
            .is_some_and(|at| now.saturating_duration_since(at) < Duration::from_millis(250))
    }

    /// Begin a batch of feeds, optionally retaining the first completed
    /// synchronized-output frame separately from the final live frame.
    pub fn begin_update(&mut self, capture_completed_sync: bool) {
        self.completed_sync_frame = None;
        self.capture_sync_end = capture_completed_sync;
    }

    pub fn take_completed_sync_frame(&mut self) -> Option<CellFrame> {
        self.completed_sync_frame.take()
    }
    /// Legacy and xterm modifyOtherKeys encoding; Kitty remains unsupported. `name` is a logical browser key or exactly
    /// one already layout-resolved printable Unicode scalar (not a physical key).
    pub fn encode_key(
        &self,
        name: &str,
        shift: bool,
        alt: bool,
        ctrl: bool,
    ) -> Result<Vec<u8>, String> {
        use crate::input::{
            legacy_function_key_sequence, xterm_encode_modify_other_keys, Key, Modifiers,
        };
        if self.protocol.keyboard_enhancement_flags() != 0 {
            return Err("Kitty keyboard flags are unsupported".into());
        }
        let key = match name {
            "ArrowUp" => Key::ArrowUp,
            "ArrowDown" => Key::ArrowDown,
            "ArrowLeft" => Key::ArrowLeft,
            "ArrowRight" => Key::ArrowRight,
            "Home" => Key::Home,
            "End" => Key::End,
            "Insert" => Key::Insert,
            "Delete" => Key::Delete,
            "PageUp" => Key::PageUp,
            "PageDown" => Key::PageDown,
            "F1" => Key::F1,
            "F2" => Key::F2,
            "F3" => Key::F3,
            "F4" => Key::F4,
            "F5" => Key::F5,
            "F6" => Key::F6,
            "F7" => Key::F7,
            "F8" => Key::F8,
            "F9" => Key::F9,
            "F10" => Key::F10,
            "F11" => Key::F11,
            "F12" => Key::F12,
            "Enter" => Key::Enter,
            "Escape" => Key::Escape,
            "Backspace" => Key::Backspace,
            "Tab" => Key::Tab,
            _ => {
                let mut chars = name.chars();
                let c = chars.next().ok_or("empty key")?;
                if chars.next().is_some() || c.is_control() {
                    return Err("unsupported key".into());
                }
                Key::Character(c)
            }
        };
        let modifiers = Modifiers {
            shift,
            alt,
            ctrl,
            super_key: false,
        };
        if let Some(sequence) =
            legacy_function_key_sequence(key, modifiers, self.protocol.modes().contains(&1))
        {
            return Ok(sequence.as_bytes().to_vec());
        }
        // Frozen core helper includes Shift at level 2. This host receives
        // layout-resolved text, so follow Frost's committed-text policy: Shift
        // alone does not re-encode uppercase/punctuation. Ctrl/Alt retain Shift
        // in the modifier parameter. Function keys already used legacy CSI above.
        if ctrl || alt {
            let text_character = match key {
                Key::Character(c) => Some(c),
                Key::Enter => Some('\r'),
                Key::Escape => Some('\x1b'),
                Key::Backspace => Some('\x7f'),
                Key::Tab => Some('\t'),
                _ => None,
            };
            if let Some(sequence) = xterm_encode_modify_other_keys(
                key,
                modifiers,
                self.protocol.xterm_modify_other_keys(),
                self.protocol.xterm_format_other_keys(),
                false,
                text_character,
            ) {
                return Ok(sequence.into_bytes());
            }
        }
        let mut bytes = match key {
            Key::Enter if !ctrl && !shift => vec![b'\r'],
            Key::Escape if !ctrl && !shift => vec![0x1b],
            Key::Backspace if !shift => vec![if ctrl { 8 } else { 127 }],
            Key::Tab if !ctrl => {
                if shift {
                    b"\x1b[Z".to_vec()
                } else {
                    vec![b'\t']
                }
            }
            Key::Character(c) if ctrl => {
                let c = c.to_ascii_uppercase();
                let byte = match c {
                    '@' | ' ' | '2' => 0,
                    'A'..='Z' => (c as u8) - b'A' + 1,
                    '[' | '3' => 27,
                    '\\' | '4' => 28,
                    ']' | '5' => 29,
                    '^' | '6' => 30,
                    '_' | '7' => 31,
                    '?' | '8' => 127,
                    _ => return Err("unsupported Ctrl key".into()),
                };
                vec![byte]
            }
            Key::Character(c) => c.to_string().into_bytes(),
            _ => return Err("unsupported legacy modifier combination".into()),
        };
        if alt {
            bytes.insert(0, 0x1b);
        }
        Ok(bytes)
    }
    /// Already-committed IME/text input, not a key event or paste. Frost's
    /// Message::Ime / Ime::Commit path writes committed UTF-8 bytes directly;
    /// xterm modifyOtherKeys only applies to modified key events. Preserve the
    /// complete commit, with no scalar splitting, normalization, modifiers or
    /// bracket markers. The host must suppress duplicate key/input events.
    /// Kitty flags remain unsupported by this spike, including report-all;
    /// do not silently emit a legacy text fallback after that negotiation.
    pub fn encode_text(&self, text: &str) -> Result<Vec<u8>, String> {
        if text.len() > MAX_INPUT_BYTES {
            return Err("committed text byte limit".into());
        }
        if text.chars().any(char::is_control) {
            return Err("committed text contains control characters".into());
        }
        if text.is_empty() {
            return Ok(Vec::new());
        }
        if self.protocol.keyboard_enhancement_flags() != 0 {
            return Err("IME committed text with Kitty keyboard flags is unsupported".into());
        }
        Ok(text.as_bytes().to_vec())
    }
    /// Wire-size bound includes bracket markers. CRLF/CR/LF become CR. All
    /// other controls except TAB are rejected, including ESC and C1 controls.
    /// In unbracketed mode CR may execute shell lines: host must obtain paste intent.
    pub fn encode_paste(&self, text: &str) -> Result<Vec<u8>, String> {
        if text.len() > MAX_INPUT_BYTES {
            return Err("paste byte limit".into());
        }
        if text
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\r' | '\n' | '\t'))
        {
            return Err("paste contains control characters".into());
        }
        let normalized = text
            .replace("\r\n", "\n")
            .replace('\r', "\n")
            .replace('\n', "\r");
        let bytes = if self.protocol.modes().contains(&2004) {
            format!("\x1b[200~{normalized}\x1b[201~").into_bytes()
        } else {
            normalized.into_bytes()
        };
        if bytes.len() > MAX_INPUT_BYTES {
            return Err("encoded paste byte limit".into());
        }
        Ok(bytes)
    }
    /// Number of explicit negative capability replies, not supported features.
    pub fn negative_query_replies(&self) -> u64 {
        self.negative_query_replies
    }
    pub fn unsupported_count(&self) -> u64 {
        self.unsupported
    }
    /// Up to 16 unique structural identifiers; never stores printable output,
    /// OSC/DCS payloads, clipboard content, or full raw escape sequences.
    pub fn unsupported_events(&self) -> &[String] {
        &self.unsupported_events
    }
    fn record_unsupported_event(&mut self, event: &Event<'_>) {
        if self.unsupported_events.len() >= 16 {
            return;
        }
        let label = match event {
            Event::Csi {
                private_prefix,
                intermediates,
                final_byte,
                params,
                ..
            } => format!(
                "CSI prefix={private_prefix:?} intermediate={:?} final={final_byte} params={:?}",
                &intermediates[..intermediates.len().min(2)],
                &params[..params.len().min(4)]
            ),
            Event::Escape {
                intermediates,
                final_byte,
            } => format!(
                "ESC intermediate={:?} final={final_byte}",
                &intermediates[..intermediates.len().min(2)]
            ),
            Event::Control(byte) => format!("control={byte}"),
            Event::String {
                kind: StringKind::Dcs,
                payload,
                ..
            } => {
                if payload.starts_with(b"+q") {
                    "DCS XTGETTCAP".into()
                } else if payload.starts_with(b"$q") {
                    "DCS DECRQSS".into()
                } else {
                    if payload.as_ref() == b"zz" {
                        "DCS Vim compatibility probe".into()
                    } else {
                        "DCS unknown".into()
                    }
                }
            }
            Event::String { kind, .. } => format!("string={kind:?}"),
            Event::RejectApc { .. } => "rejected APC".into(),
            Event::Print(_) => "zero-width scalar / grapheme composition unsupported".into(),
            _ => "other unsupported event".into(),
        };
        if !self.unsupported_events.contains(&label) {
            self.unsupported_events.push(label);
        }
    }
    fn unsupported(&mut self) {
        self.unsupported = self.unsupported.saturating_add(1);
    }
    fn capture_history(&mut self, effect: Option<RegionMutation>) {
        if let Some(effect) = effect {
            if !self.screen.use_alt_buffer
                && effect.direction == RegionDirection::Up
                && effect.top == 0
                && effect.bottom + 1 == self.screen.grid.rows()
            {
                for removed in effect.removed {
                    if let Some(line) = removed.captured {
                        if self.history.len() == MAX_HISTORY_ROWS {
                            self.history.pop_front();
                        }
                        self.history.push_back(line);
                        self.history_end = self
                            .history_end
                            .checked_add(1)
                            .expect("terminal history row identity exhausted");
                    }
                }
            }
        }
    }
    fn history_admitted(&self) -> bool {
        !self.screen.use_alt_buffer
            && self.screen.scroll_region_top == 0
            && self.screen.scroll_region_bottom + 1 == self.screen.grid.rows()
    }
    fn motion(&mut self, m: VerticalMotion) {
        let capture = self.history_admitted();
        let effect = self
            .screen
            .vertical_motion(m, capture, || RawRowId::UNTRACKED);
        self.capture_history(effect.region);
    }
    pub fn history_rows(&self) -> usize {
        if self.screen.use_alt_buffer {
            0
        } else {
            self.history.len()
        }
    }
    /// Absolute coordinates of retained physical history rows, oldest first.
    /// Appending advances the exclusive end. Eviction and explicit history
    /// erasure advance the start, so a saved primary-row anchor cannot name
    /// a different row. Alternate screen exposes an empty range at the same end.
    pub fn history_extent(&self) -> std::ops::Range<u64> {
        self.history_end - self.history_rows() as u64..self.history_end
    }

    /// Newest-relative history pagination, oldest-to-newest within each page.
    /// Historical widths are retained across resize; there is no history reflow.
    /// Resize truncations are not archived. Alternate screen cannot read history.
    pub fn history_page(&self, offset: usize, limit: usize) -> Result<HistoryPage, String> {
        let total_rows = self.history_rows();
        if limit == 0 || limit > MAX_ROWS || offset > MAX_HISTORY_ROWS {
            return Err("history page out of bounds".into());
        }
        let offset = offset.min(total_rows);
        let end = total_rows - offset;
        let start = end.saturating_sub(limit);
        let rows: Vec<HistoryCopyRow> = self
            .history
            .iter()
            .skip(start)
            .take(end - start)
            .map(|line| HistoryCopyRow {
                row: Row {
                    cells: Arc::from(line.decompress()),
                    wrapped: line.is_wrapped,
                },
                underline_colors: (!line.underline_color_runs().is_empty())
                    .then(|| Arc::from(line.underline_color_runs())),
                clusters: line.cluster_row().cloned(),
                synthetic_gap: line.synthetic_margin_gap(),
            })
            .collect();
        let mut page = HistoryPage {
            revision: self.revision,
            offset,
            total_rows,
            row_count: rows.len(),
            text: String::new(),
            hyperlinks: self.hyperlink_snapshot(rows.iter().map(|row| &row.row)),
            rows,
        };
        page.text = page.selection_text(0, 0, page.row_count, 0)?;
        Ok(page)
    }

    /// Opt in before launching a shell with compatible OSC 133 integration.
    pub fn observe_command_blocks(&mut self) {
        self.command_blocks
            .get_or_insert_with(CommandBlocks::default);
    }

    /// Completed records for this exact PTY incarnation. None means the
    /// shell was launched without observation.
    pub fn completed_command_blocks(&self) -> Option<&VecDeque<CommandBlock>> {
        self.command_blocks.as_ref().map(CommandBlocks::completed)
    }

    /// The integrated shell has reported its primary-screen input prompt.
    pub fn command_prompt_ready(&self) -> bool {
        !self.screen.use_alt_buffer
            && self
                .command_blocks
                .as_ref()
                .is_some_and(CommandBlocks::prompt_ready)
    }

    /// A submitted line can start a foreground command before its PTY markers arrive.
    pub fn observe_prompt_input(&mut self, bytes: &[u8]) {
        if let Some(blocks) = &mut self.command_blocks {
            if bytes.iter().any(|b| matches!(b, 3 | 4 | 10 | 13 | 26)) {
                blocks.input_submitted();
            }
        }
    }

    fn extend_grapheme(&mut self, ch: char) -> bool {
        let Some(input) = &self.graphemes else {
            return false;
        };
        let Some(mut anchor) = input.active else {
            return false;
        };
        let Some(base) = self.screen.last_printed_char else {
            return false;
        };
        if base.is_ascii() && ch.is_ascii() {
            return false;
        }
        let mut scalar = [0; 4];
        let preceding = self
            .screen
            .grid
            .cluster_text(anchor.row, anchor.col)
            .unwrap_or_else(|| base.encode_utf8(&mut scalar));
        let mut buf = [0; MAX_CLUSTER_BYTES + 4];
        let Some(cluster) =
            graphemes::extension(preceding, self.screen.translate_char(ch), &mut buf)
        else {
            return false;
        };
        if cluster.len() > MAX_CLUSTER_BYTES {
            self.unsupported();
            return true;
        }
        let mut logical_width = input
            .last_cluster
            .as_ref()
            .map_or(anchor.width, |last| last.width);
        if anchor.width == 1 && graphemes::has_wide_emoji_presentation(cluster) {
            if input.provisional.is_some() {
                let Some(wide) = self.replace_provisional_print(anchor, base, 2, cluster.len())
                else {
                    self.unsupported();
                    return true;
                };
                anchor = wide;
            }
            logical_width = 2;
        } else if anchor.width == 2 && graphemes::has_narrow_text_presentation(cluster) {
            if let Some(narrow) = self.replace_provisional_print(anchor, base, 1, cluster.len()) {
                anchor = narrow;
                logical_width = 1;
            }
        }
        if let Some(stored) = self
            .screen
            .grid
            .set_cluster_text(anchor.row, anchor.col, cluster)
        {
            if let Some(input) = &mut self.graphemes {
                input.last_cluster = Some(RetainedGrapheme {
                    text: stored,
                    width: logical_width,
                });
            }
        } else {
            self.unsupported();
        }
        true
    }

    fn replace_provisional_print(
        &mut self,
        anchor: GraphicPosition,
        graphic: char,
        width: usize,
        bytes: usize,
    ) -> Option<GraphicPosition> {
        let input = self.graphemes.as_mut()?;
        let checkpoint = input.provisional.as_ref()?;
        let cols = self.screen.grid.cols();
        let wrap = self.protocol.modes().contains(&7);
        let (row, col, cleared) = if checkpoint.col + width <= cols {
            (checkpoint.row, checkpoint.col, false)
        } else if wrap {
            // A preflight wrap here is growth from a provisional narrow
            // print, so the live row is still the original departure row.
            debug_assert_eq!(self.screen.cursor_row, checkpoint.row);
            let (row, cleared) = self.screen.wrap_target();
            (row, 0, cleared)
        } else {
            (checkpoint.row, cols - width, false)
        };
        if !cleared {
            let line = if row == anchor.row {
                checkpoint.footprint.cluster_row()
            } else {
                self.screen.grid.cluster_row(row).map(Arc::as_ref)
            };
            let displaced = if self.screen.insert_mode {
                cols - width..cols
            } else {
                col..col + width
            };
            if line.is_some_and(|line| !line.can_store_after_displacing(displaced, bytes)) {
                return None;
            }
        }

        let checkpoint = input.provisional.take()?;
        self.screen
            .grid
            .restore_print(anchor.row, checkpoint.footprint);
        if let Some(departure) = checkpoint.departure {
            self.screen.grid.restore_wrap(departure);
        }
        self.screen.cursor_row = checkpoint.row;
        self.screen.cursor_col = checkpoint.col;
        self.screen.pending_wrap = false;
        self.write_graphic(graphic, width);
        let placed = self.graphemes.as_ref()?.active?;
        debug_assert_eq!((placed.row, placed.col), (row, col));
        Some(placed)
    }

    fn print(&mut self, ch: char) {
        if !self.extend_grapheme(ch) {
            self.print_cell(ch, self.screen.translate_char(ch));
        }
    }

    fn print_cell(&mut self, ch: char, translated: char) {
        let width = ch.width().unwrap_or(0);
        if width == 0 || width > 2 {
            self.finish_grapheme();
            self.record_unsupported_event(&Event::Print(ch));
            self.unsupported();
            return;
        }
        self.write_graphic(translated, width);
    }

    fn write_graphic(&mut self, translated: char, width: usize) {
        let wrap = self.protocol.modes().contains(&7);
        if self.screen.take_pending_wrap(wrap) {
            self.motion(VerticalMotion::Wrap);
        }
        let origin_row = self.screen.cursor_row;
        let origin_col = self.screen.cursor_col;
        let presentation_can_change = self.graphemes.is_some()
            && self.screen.grid.cols() >= 2
            && if width == 1 {
                graphemes::may_grow(translated)
            } else {
                graphemes::may_contract(translated)
            };
        let early_wrap = self.screen.prepare_character_width(width, wrap);
        let departure = (early_wrap && presentation_can_change && !self.screen.wrap_target().1)
            .then(|| self.screen.grid.checkpoint_wrap(origin_row));
        if early_wrap {
            let capture = self.history_admitted();
            let effect = self
                .screen
                .vertical_early_wide_wrap(width, capture, || RawRowId::UNTRACKED);
            self.capture_history(effect.region);
        }
        // A committed region scroll can displace retained content and advance
        // history. Only an in-row print or a non-scrolling wrap is reversible.
        let provisional =
            (presentation_can_change && (!early_wrap || departure.is_some())).then(|| {
                ProvisionalPrint {
                    row: origin_row,
                    col: origin_col,
                    footprint: self.screen.grid.checkpoint_print(
                        self.screen.cursor_row,
                        self.screen.cursor_col,
                        width,
                        self.screen.insert_mode,
                    ),
                    departure,
                }
            });
        let written = self.screen.write_char_in_row(translated, width, wrap);
        if let Some(input) = &mut self.graphemes {
            input.active = written.printed.as_ref().map(|cols| GraphicPosition {
                row: written.row,
                col: cols.start,
                width,
            });
            input.provisional = provisional;
            input.last_cluster = None;
        }
        if let (Some(underline), Some(cols)) = (&self.underline_pen, written.printed) {
            self.screen
                .grid
                .set_underline_color(written.row, cols, underline.current);
        }
        self.screen.last_printed_char = Some(translated);
    }

    fn repeat_print(&mut self, count: usize) {
        let Some(ch) = self.screen.last_printed_char else {
            return;
        };
        let cluster = self
            .graphemes
            .as_ref()
            .and_then(|input| input.last_cluster.clone());
        if let Some(cluster) = cluster {
            let width = cluster.width.min(self.screen.grid.cols());
            for _ in 0..count {
                // The retained text already includes the translated base.
                self.write_graphic(ch, width);
                let anchor = self.graphemes.as_ref().and_then(|input| input.active);
                if let Some(anchor) = anchor {
                    if self
                        .screen
                        .grid
                        .set_cluster_text(anchor.row, anchor.col, &cluster.text)
                        .is_none()
                    {
                        self.unsupported();
                    }
                }
            }
            if let Some(input) = &mut self.graphemes {
                input.last_cluster = Some(cluster);
            }
        } else {
            let translated = self.screen.translate_char(ch);
            for _ in 0..count {
                self.print_cell(ch, translated);
            }
        }
    }

    // Apply profile-specific screen operations after shared local dispatch.
    fn apply_profile_event(&mut self, event: &Event<'_>, now: Instant) -> bool {
        match event {
            Event::Escape {
                intermediates,
                final_byte,
            } if intermediates.is_empty() => match final_byte {
                b'D' => self.motion(VerticalMotion::LineFeed),
                b'E' => {
                    self.screen.begin_next_line();
                    self.motion(VerticalMotion::LineFeed);
                }
                b'M' => self.motion(VerticalMotion::ReverseIndex),
                _ => return false,
            },
            Event::Escape {
                intermediates,
                final_byte: b'8',
            } if intermediates == b"#" => {
                self.screen.screen_alignment_test();
            }
            Event::Csi {
                params,
                private_prefix,
                intermediates,
                final_byte,
                ..
            } if intermediates.is_empty() => {
                let first = params.first().copied().unwrap_or(0);
                let count = usize::from(first.max(1));
                match (*private_prefix, *final_byte) {
                    (Some(b'?'), b'h' | b'l') => {
                        let set = *final_byte == b'h';
                        for &mode in params.iter() {
                            match mode {
                                47 | 1047 | 1049 => {
                                    if set {
                                        if let Some(entry) = self.screen.enter_alternate_screen(
                                            mode == 1049,
                                            mode != 47,
                                            &self.protocol,
                                        ) {
                                            if let Some(rows) = entry.cleared_rows {
                                                self.screen.grid.row_wrapped[rows].fill(false);
                                            }
                                            if mode == 1049 {
                                                if let Some(underline) = &mut self.underline_pen {
                                                    underline.primary = Some(underline.current);
                                                }
                                            }
                                            self.protocol.switch_screen();
                                        }
                                    } else if let Some(effect) =
                                        self.screen.leave_alternate_screen(mode == 1049)
                                    {
                                        self.protocol.switch_screen();
                                        if let Some(underline) = &mut self.underline_pen {
                                            underline.current =
                                                underline.primary.take().unwrap_or(Color::Default);
                                        }
                                        if let Some(wrap) = effect.restore_autowrap {
                                            if wrap {
                                                self.protocol.modes_mut().insert(7);
                                            } else {
                                                self.protocol.modes_mut().remove(&7);
                                            }
                                        }
                                    }
                                }
                                1048 => {
                                    if set {
                                        semantics::save_cursor_state(
                                            &mut self.screen,
                                            &self.protocol,
                                            self.underline_pen.as_mut(),
                                        );
                                    } else {
                                        semantics::restore_cursor_state(
                                            &mut self.screen,
                                            &mut self.protocol,
                                            self.underline_pen.as_mut(),
                                        );
                                    }
                                }
                                6 => {
                                    self.screen.origin_mode = set;
                                    self.screen.set_cursor_position(1, 1);
                                }
                                7 => {
                                    if !set {
                                        self.screen.pending_wrap = false;
                                    }
                                }
                                2026 => {
                                    if set {
                                        self.sync_started.get_or_insert(now);
                                    } else {
                                        self.sync_started = None;
                                    }
                                    continue;
                                }
                                1 | 25 | 2004 => (),
                                _ => {
                                    self.unsupported();
                                    continue;
                                }
                            }
                            if set {
                                self.protocol.modes_mut().insert(mode);
                            } else {
                                self.protocol.modes_mut().remove(&mode);
                            }
                        }
                    }
                    (None, b'h' | b'l') => {
                        for &mode in params.iter() {
                            if mode == 4 {
                                self.screen.insert_mode = *final_byte == b'h';
                            } else {
                                self.unsupported();
                            }
                        }
                    }
                    (None, b'J') if first == 2 => {
                        self.screen.erase_screen();
                        // Erased physical rows no longer continue the previous
                        // text in the standalone terminal's logical projection.
                        self.screen.grid.row_wrapped.fill(false);
                    }
                    (None | Some(b'?'), b'J') => {
                        let selective = *private_prefix == Some(b'?');
                        let rows = self.screen.grid.rows();
                        let cols = self.screen.grid.cols();
                        let cursor = self.screen.cursor_row;
                        match first {
                            0..=2 => {
                                for row in 0..rows {
                                    let span = match first {
                                        0 if row > cursor => Some((0, cols)),
                                        0 if row == cursor => Some((self.screen.cursor_col, cols)),
                                        1 if row < cursor => Some((0, cols)),
                                        1 if row == cursor => Some((0, self.screen.cursor_col + 1)),
                                        2 => Some((0, cols)),
                                        _ => None,
                                    };
                                    if let Some((start, end)) = span {
                                        let erased =
                                            self.screen.erase_span(row, start, end, selective);
                                        // A fully cleared physical row is no longer part of
                                        // the logical line previously wrapped at its margin.
                                        if matches!(erased.as_slice(), [change] if change.changed == (0..cols))
                                        {
                                            self.screen.grid.row_wrapped[row] = false;
                                        }
                                    }
                                }
                                self.screen.pending_wrap = false;
                            }
                            // Alternate applications cannot erase primary history.
                            3 if !selective => {
                                if !self.screen.use_alt_buffer {
                                    self.history.clear();
                                }
                            }
                            _ => self.unsupported(),
                        }
                    }
                    (None, b'L' | b'M') => {
                        let top = self.screen.cursor_row;
                        let bottom = self.screen.scroll_region_bottom;
                        if top >= self.screen.scroll_region_top && top <= bottom {
                            let direction = if *final_byte == b'L' {
                                RegionDirection::Down
                            } else {
                                RegionDirection::Up
                            };
                            self.screen
                                .edit_lines(direction, top, bottom, count, || RawRowId::UNTRACKED);
                        }
                    }
                    (None, b'S' | b'T') => {
                        let top = self.screen.scroll_region_top;
                        let bottom = self.screen.scroll_region_bottom;
                        let direction = if *final_byte == b'S' {
                            RegionDirection::Up
                        } else {
                            RegionDirection::Down
                        };
                        for _ in 0..count.min(bottom - top + 1) {
                            let capture =
                                self.history_admitted() && direction == RegionDirection::Up;
                            let effect = self.screen.scroll_region_one(
                                direction,
                                top,
                                bottom,
                                capture,
                                || RawRowId::UNTRACKED,
                            );
                            self.capture_history(effect);
                        }
                    }
                    (None, b'b') => {
                        self.repeat_print(count);
                    }
                    _ => return false,
                }
            }
            _ => return false,
        }
        true
    }
    /// Feed bytes and return any ordered replies for the owning PTY.
    pub fn feed(&mut self, input: &[u8]) -> Result<Vec<u8>, String> {
        self.feed_at(input, Instant::now())
    }

    /// Apply bytes sampled at a monotonic instant supplied by the session.
    pub fn feed_at(&mut self, input: &[u8], now: Instant) -> Result<Vec<u8>, String> {
        if input.len() > MAX_FEED_BYTES {
            return Err("feed byte limit".into());
        }
        self.revision = self.revision.checked_add(1).ok_or("revision exhausted")?;
        self.parser.begin_feed();
        let mut at = 0;
        let mut replies = Vec::new();
        let mut steps = 0;
        while at < input.len() {
            steps += 1;
            if steps > input.len() * 2 + 16 {
                return Err("parser made no progress".into());
            }
            let rejected_before = self.parser.rejected_count();
            let step = self.parser.next(&input[at..]);
            if self.parser.rejected_count() != rejected_before {
                if self.hyperlinks.is_some() {
                    self.screen.current_hyperlink = HyperlinkId::NONE;
                }
                self.finish_grapheme();
            }
            at += step.consumed;
            let Some(event) = step.event else { continue };
            if !matches!(&event, Event::Print(_) | Event::Ascii(_)) {
                self.finish_grapheme();
            }
            if self
                .command_blocks
                .as_mut()
                .is_some_and(|blocks| blocks.observe(&event, self.screen.use_alt_buffer))
            {
                continue;
            }
            if let (
                Some((policy, table)),
                Event::String {
                    kind: StringKind::Osc,
                    payload,
                    ..
                },
            ) = (&mut self.hyperlinks, &event)
            {
                if payload.as_ref() == b"8" {
                    self.screen.current_hyperlink = HyperlinkId::NONE;
                    continue;
                }
                if let Some(fields) = payload.strip_prefix(b"8;") {
                    self.screen.current_hyperlink = std::str::from_utf8(fields)
                        .map_or(HyperlinkId::NONE, |fields| {
                            table.apply_osc8(fields, *policy)
                        });
                    continue;
                }
            }
            // Aether has no DA identity contract. A newer shared legacy
            // dispatcher answers DA1/DA2 as Ember; do not inherit that identity.
            // Count unsupported CSI c separately from ESC c (RIS).
            if matches!(
                &event,
                Event::Csi {
                    final_byte: b'c',
                    ..
                }
            ) {
                let before = self.unsupported;
                self.unsupported();
                if self.unsupported != before {
                    self.record_unsupported_event(&event);
                }
                continue;
            }
            // Vim sends 0%m as an unknown-sequence probe. This profile
            // preserves drawing state when SGR has a nonempty intermediate.
            if matches!(&event, Event::Csi { intermediates, final_byte: b'm', .. } if !intermediates.is_empty())
            {
                self.unsupported();
                self.record_unsupported_event(&event);
                continue;
            }
            // Reflect the effective hold only for the shared mode-query reducer;
            // do not sample the clock or touch the mode set on ordinary output.
            let sync_query = matches!(&event, Event::Csi {
                params,
                private_prefix: Some(b'?'),
                intermediates,
                final_byte: b'p',
                ..
            } if intermediates == b"$" && params.first() == Some(&2026));
            if sync_query && self.publication_held_at(now) {
                self.protocol.modes_mut().insert(2026);
            }
            let capture_end = self.capture_sync_end
                && self.sync_started.is_some()
                && matches!(&event, Event::Csi { private_prefix: Some(b'?'), intermediates, final_byte: b'l', params, .. }
                    if intermediates.is_empty() && params.contains(&2026));
            let dispatch = semantics::dispatch_local_with_underline(
                &mut self.screen,
                &mut self.protocol,
                &event,
                self.underline_pen.as_mut(),
            );
            if sync_query {
                self.protocol.modes_mut().remove(&2026);
            }
            match dispatch {
                Dispatch::Handled(effects) => {
                    for effect in effects {
                        // ScreenState has already applied row edits. Aether
                        // captures paint rows from the resulting grid.
                        if let Effect::Reply(bytes) = effect {
                            if replies.len() + bytes.len() > MAX_FEED_BYTES {
                                return Err("reply limit".into());
                            }
                            replies.extend(bytes);
                        }
                    }
                }
                Dispatch::Unhandled => match event {
                    Event::Print(ch) => self.print(ch),
                    Event::Ascii(bytes) => {
                        for &byte in bytes {
                            self.print(byte as char);
                        }
                    }
                    Event::Control(10..=12) => self.motion(VerticalMotion::LineFeed),
                    Event::Control(0) | Event::Boundary => (),
                    _ => {
                        let before = self.unsupported;
                        if matches!(&event, Event::String { kind: StringKind::Dcs, payload, .. } if payload.starts_with(b"+q") && payload.len() <= 4096)
                        {
                            // XTerm ctlseqs XTGETTCAP: an unknown first name
                            // ends the list with DCS 0+r ST. No capability table
                            // is advertised here, and no payload is reflected.
                            const NEGATIVE: &[u8] = b"\x1bP0+r\x1b\\";
                            if replies.len() + NEGATIVE.len() > MAX_FEED_BYTES {
                                return Err("reply limit".into());
                            }
                            replies.extend_from_slice(NEGATIVE);
                            self.negative_query_replies =
                                self.negative_query_replies.saturating_add(1);
                            self.unsupported();
                        } else if !self.apply_profile_event(&event, now) {
                            self.unsupported();
                        }
                        if self.unsupported != before {
                            self.record_unsupported_event(&event);
                        }
                    }
                },
            }
            if capture_end {
                // Exactly one bounded immutable frame per opted-in PTY poll.
                // Reserve a distinct live revision even when no more bytes arrive.
                let next = self.revision.checked_add(1).ok_or("revision exhausted")?;
                self.completed_sync_frame = Some(self.frame()?);
                self.revision = next;
                self.capture_sync_end = false;
            }
        }
        Ok(replies)
    }
    pub fn resize(&mut self, rows: usize, cols: usize) -> Result<(), String> {
        self.resize_with(rows, cols, || Ok(()))
    }
    /// Validate geometry and frame counters before invoking the session's resize.
    /// Commit screen geometry only after that operation succeeds.
    pub fn resize_with(
        &mut self,
        rows: usize,
        cols: usize,
        resize_pty: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        dimensions(rows, cols)?;
        let layout = self.layout.checked_add(1).ok_or("layout exhausted")?;
        let revision = self.revision.checked_add(1).ok_or("revision exhausted")?;
        resize_pty()?;
        self.finish_grapheme();
        if let Some(blocks) = &mut self.command_blocks {
            blocks.screen_changed();
        }
        self.screen
            .resize_geometry(rows, cols, 0, || RawRowId::UNTRACKED);
        self.layout = layout;
        self.revision = revision;
        Ok(())
    }
    pub fn frame(&self) -> Result<CellFrame, String> {
        let mut last = self.last_frame.lock().map_err(|_| "frame cache poisoned")?;
        if let Some(frame) = last
            .as_ref()
            .filter(|frame| frame.stamp().revision == self.revision)
        {
            return Ok(frame.clone());
        }
        let screen = if self.screen.use_alt_buffer {
            Screen::Alternate
        } else {
            Screen::Primary
        };
        let grid = &self.screen.grid;
        let capture_row = |index| Row {
            cells: Arc::from(&grid[index][..]),
            wrapped: grid.row_wrapped[index],
        };
        // Aether's current grid has no universal dirty-row signal. Compare
        // contents against the last capture and allocate only changed rows.
        let previous = last.as_ref().filter(|frame| {
            let stamp = frame.stamp();
            stamp.layout == self.layout && stamp.screen == screen
        });
        let rows = if let Some(previous) = previous {
            let mut changed = None;
            for (index, row) in previous.rows().iter().enumerate() {
                if row.wrapped != grid.row_wrapped[index] || row.cells.as_ref() != &grid[index][..]
                {
                    changed.get_or_insert_with(|| previous.rows().to_vec())[index] =
                        capture_row(index);
                }
            }
            changed.map_or_else(|| previous.shared_rows(), Arc::from)
        } else {
            (0..grid.rows()).map(capture_row).collect::<Vec<_>>().into()
        };
        let underline_colors = if self.underline_pen.is_some() {
            let mut changed = None;
            for row in 0..grid.rows() {
                let before = previous.map_or(&[][..], |frame| frame.underline_color_runs(row));
                if grid.underline_color_runs(row).eq(before.iter().copied()) {
                    continue;
                }
                let table = changed.get_or_insert_with(|| {
                    previous
                        .and_then(CellFrame::shared_underline_rows)
                        .map_or_else(|| vec![None; grid.rows()], |colors| colors.to_vec())
                });
                let runs: Vec<_> = grid.underline_color_runs(row).collect();
                table[row] = (!runs.is_empty()).then(|| runs.into());
            }
            match changed {
                Some(colors) => colors.iter().any(Option::is_some).then(|| colors.into()),
                None => previous.and_then(CellFrame::shared_underline_rows),
            }
        } else {
            None
        };
        let hyperlinks = match previous {
            Some(frame) if std::ptr::eq(frame.rows(), rows.as_ref()) => frame.shared_hyperlinks(),
            _ => self.hyperlink_snapshot(rows.iter()),
        };
        let clusters = if self.graphemes.is_some() {
            let mut changed = None;
            for row in 0..grid.rows() {
                let current = grid.cluster_row(row);
                if current != previous.and_then(|frame| frame.cluster_row(row)) {
                    let rows = changed.get_or_insert_with(|| {
                        previous
                            .and_then(CellFrame::shared_clusters)
                            .map_or_else(|| vec![None; grid.rows()], |rows| rows.to_vec())
                    });
                    rows[row] = current.cloned();
                }
            }
            match changed {
                Some(rows) => rows.iter().any(Option::is_some).then(|| rows.into()),
                None => previous.and_then(CellFrame::shared_clusters),
            }
        } else {
            None
        };
        let frame = CellFrame::new_with_underline_colors(
            Stamp {
                epoch: self.epoch.get(),
                revision: self.revision,
                layout: self.layout,
                screen,
            },
            grid.cols(),
            rows,
            Cursor {
                row: self.screen.cursor_row,
                col: self.screen.cursor_col,
                pending_wrap: self.screen.pending_wrap,
                visible: self.protocol.modes().contains(&25),
                shape: self.screen.cursor_shape,
            },
            Limits {
                max_rows: MAX_ROWS,
                max_cols: MAX_COLS,
                max_cells: MAX_ROWS * MAX_COLS,
            },
            underline_colors,
        )
        .map_err(|e| format!("{e:?}"))?
        .with_hyperlinks(hyperlinks)
        .with_clusters(clusters);
        *last = Some(frame.clone());
        Ok(frame)
    }
    pub fn visible_text(&self) -> String {
        (0..self.screen.grid.rows())
            .map(|r| {
                self.screen.grid[r].iter().enumerate().fold(
                    String::new(),
                    |mut line, (col, cell)| {
                        if let Some(cluster) = self.screen.grid.cluster_text(r, col) {
                            line.push_str(cluster);
                        } else {
                            line.push(cell.character);
                        }
                        line
                    },
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[cfg(test)]
mod tests;
