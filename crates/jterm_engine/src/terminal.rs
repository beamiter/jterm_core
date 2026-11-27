//! Bounded terminal owner for standalone sessions.

use crate::{
    colors::{ColorState, TerminalDefaultColors},
    command_blocks::{CommandBlock, CommandBlocks},
    frame::{underline_color_at, CellFrame, Cursor, Limits, Row, Screen, Stamp},
    graphemes::{self, MAX_CLUSTER_BYTES},
    grid::{PrintCheckpoint, WrapCheckpoint},
    history_reflow::HistoryLayout,
    hyperlink::{resolve_resource, HyperlinkPolicy, HyperlinkResources, HyperlinkTable},
    kitty_graphics::{self, inline::InlineGraphics},
    mouse::MouseInput,
    parser::{Event, Parser, ParserLimits, ParserProfile, StringKind},
    protocol_state::ProtocolState,
    retained_history::RetainedHistory,
    row_annotations::RowAnnotations,
    screen::{
        rows_to_archive_on_height_shrink, AsciiWriteStep, RegionDirection, RegionMutation,
        ScreenState, VerticalMotion,
    },
    semantics::{self, Dispatch, Effect, Effects, UnderlinePen},
    titles::TitleState,
    Color, HyperlinkId, RawRowId, ScrollbackLine, UnderlineColorRun,
};
#[cfg(test)]
use std::time::Duration;
use std::{
    collections::VecDeque,
    num::NonZeroU64,
    sync::{Arc, Mutex, Weak},
    time::Instant,
};
use unicode_width::UnicodeWidthChar;

mod history_view;
mod inline_image;
pub(crate) use inline_image::ImageAnnotation;
pub use inline_image::{
    InlineImage, InlineImageId, InlineImageRow, InlineRaster, PlacedInlineImage,
    PreparedInlineImage,
};
pub use kitty_graphics::inline::PngDecoder;
mod primary_reflow;
mod row_sources;
use row_sources::RowSources;
pub use row_sources::TranscriptAnchor;

const MAX_ROWS: usize = 128;
const MAX_COLS: usize = 256;
const HOST_MAX_COLS: usize = 400;
/// Maximum UTF-8 bytes admitted by one encoded input operation.
pub const MAX_INPUT_BYTES: usize = 4096;
/// Largest native bracketed clipboard record, including both markers.
pub const MAX_STREAMED_PASTE_BYTES: usize = u16::MAX as usize;
/// Maximum bytes consumed or returned as replies by a single feed.
pub const MAX_FEED_BYTES: usize = 65536;

/// Ordered whole replies retained for one feed, plus bytes refused once the
/// reply budget was exhausted. Reply pressure does not stop parsing PTY bytes.
#[derive(Default)]
pub struct FeedReplies {
    pub bytes: Vec<u8>,
    pub refused_bytes: usize,
    limit_error: Option<&'static str>,
}

impl FeedReplies {
    fn push(&mut self, reply: &[u8], error: &'static str) {
        if self.limit_error.is_some() || reply.len() > MAX_FEED_BYTES - self.bytes.len() {
            self.limit_error.get_or_insert(error);
            self.refused_bytes = self.refused_bytes.saturating_add(reply.len());
        } else {
            self.bytes.extend_from_slice(reply);
        }
    }
}

fn dimensions(rows: usize, cols: usize, max_cols: usize) -> Result<(), String> {
    if rows == 0 || cols == 0 || rows > MAX_ROWS || cols > max_cols {
        Err("viewport out of bounds".into())
    } else {
        Ok(())
    }
}

fn initial_protocol() -> ProtocolState {
    let mut protocol = ProtocolState::default();
    protocol.modes_mut().insert(7);
    protocol.modes_mut().insert(25);
    protocol
}

fn device_answer(event: &Event<'_>) -> Option<&'static [u8]> {
    let Event::Csi {
        private_prefix,
        params,
        intermediates,
        final_byte,
        ..
    } = event
    else {
        return None;
    };
    if !intermediates.is_empty() {
        return None;
    }
    match (*private_prefix, *final_byte, params.as_slice()) {
        (None, b'c', [] | [0]) => Some(b"\x1b[?1;2c"),
        (Some(b'>'), b'c', [] | [0]) => Some(b"\x1b[>0;0;0c"),
        (Some(b'='), b'c', [] | [0]) => Some(b"\x1bP!|00000000\x1b\\"),
        (Some(b'>'), b'q', [] | [0]) => Some(b"\x1bP>|jterm\x1b\\"),
        _ => None,
    }
}

const MAX_HISTORY_ROWS: usize = 1024;
const MAX_COPY_BYTES: usize = MAX_ROWS * (MAX_COLS * MAX_CLUSTER_BYTES + 1);
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
    paint_colors: Option<Arc<ColorState>>,
    hyperlinks: Option<HyperlinkResources>,
}
struct HistoryCopyRow {
    // Physical addressing is unchanged. Only extraction may omit this slot.
    row: Row,
    underline_colors: Option<Arc<[UnderlineColorRun]>>,
    annotations: Option<Arc<RowAnnotations>>,
    synthetic_gap: Option<usize>,
}
impl HistoryPage {
    /// Current colors captured when this history page was requested.
    pub fn paint_colors(&self) -> Option<&ColorState> {
        self.paint_colors.as_deref()
    }

    /// Decoded page backing, including copy text and row annotations.
    /// The image store accounts for referenced rasters.
    pub fn retained_delivery_bytes(&self) -> usize {
        let header = 2 * size_of::<usize>();
        let rows = self.rows.capacity() * size_of::<HistoryCopyRow>();
        let contents = self.rows.iter().map(|row| {
            header
                + row.row.cells.len() * size_of::<crate::TerminalCell>()
                + row
                    .underline_colors
                    .as_ref()
                    .map_or(0, |runs| header + std::mem::size_of_val(runs.as_ref()))
                + row
                    .annotations
                    .as_ref()
                    .map_or(0, |data| data.retained_bytes())
        });
        let links = self.hyperlinks.as_ref().map_or(0, |table| {
            header
                + std::mem::size_of_val(table.as_ref())
                + table
                    .iter()
                    .map(|(_, uri)| header + uri.len())
                    .sum::<usize>()
        });
        size_of::<Self>()
            + self.text.capacity()
            + rows
            + contents.sum::<usize>()
            + links
            + self
                .paint_colors
                .as_ref()
                .map_or(0, |_| header + size_of::<ColorState>())
    }

    /// Physical row and style snapshot for a viewport. Width can differ
    /// between rows retained before and after a resize.
    pub fn row(&self, index: usize) -> Option<&Row> {
        self.rows.get(index).map(|row| &row.row)
    }

    /// Physical column skipped when extracting a logical line from this row.
    pub fn synthetic_margin_gap(&self, index: usize) -> Option<usize> {
        self.rows.get(index)?.synthetic_gap
    }

    /// Resolve a validated target referenced by this immutable history page.
    pub fn hyperlink_target(&self, id: HyperlinkId) -> Option<&str> {
        resolve_resource(&self.hyperlinks, id)
    }

    /// Complete text at a multi-scalar grapheme's original physical column.
    pub fn cluster_text(&self, row: usize, column: usize) -> Option<&str> {
        self.rows.get(row)?.annotations.as_ref()?.text(column)
    }

    /// Retained inline image rows in this projected history row.
    pub fn inline_image_rows(
        &self,
        row: usize,
    ) -> impl Iterator<Item = (usize, InlineImageRow)> + '_ {
        self.rows
            .get(row)
            .and_then(|row| row.annotations.as_ref())
            .into_iter()
            .flat_map(|row| row.image_rows())
    }

    /// Image resources and slices share the projected row's paint coordinates.
    pub fn inline_images(
        &self,
        row: usize,
    ) -> impl Iterator<Item = (usize, &InlineImage, u16)> + '_ {
        self.rows
            .get(row)
            .and_then(|row| row.annotations.as_ref())
            .into_iter()
            .flat_map(|row| row.images())
    }

    /// One UTF-16 offset per physical cell boundary in the page's copy text.
    /// Blank padding and wide continuation cells can share an offset.
    pub fn copy_offsets_utf16(&self) -> Result<Vec<Vec<u32>>, String> {
        let mut offsets = Vec::with_capacity(self.row_count);
        // Release offset delivery needs only lengths and trimming boundaries.
        // Debug builds retain the original single text pass and consistency check.
        let text = extract_selection::<{ cfg!(debug_assertions) }>(
            &self.rows,
            (0, 0),
            (self.row_count, 0),
            false,
            Some(&mut offsets),
        )?;
        debug_assert_eq!(text, self.text);
        Ok(offsets)
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
        selection_text(
            &self.rows,
            (start_row, start_col),
            (end_row, end_col),
            false,
        )
    }
}

fn selection_text(
    rows: &[HistoryCopyRow],
    start: (usize, usize),
    end: (usize, usize),
    require_literal: bool,
) -> Result<String, String> {
    extract_selection::<true>(rows, start, end, require_literal, None)
}

fn append_copy_cell(
    text: &mut String,
    cell: &crate::TerminalCell,
    annotations: Option<&RowAnnotations>,
    col: usize,
) {
    if let Some(cluster) = annotations.and_then(|row| row.text(col)) {
        text.push_str(cluster);
    } else {
        text.push(cell.character);
    }
}

fn extract_selection<const MATERIALIZE: bool>(
    rows: &[HistoryCopyRow],
    start: (usize, usize),
    end: (usize, usize),
    require_literal: bool,
    mut offsets: Option<&mut Vec<Vec<u32>>>,
) -> Result<String, String> {
    let valid = |(row, col): (usize, usize)| {
        rows.get(row).is_some_and(|row| col <= row.row.cells.len()) || row == rows.len() && col == 0
    };
    if !valid(start) || !valid(end) || start > end {
        return Err("selection out of bounds".into());
    }
    let mut text = String::new();
    let mut text_bytes = 0usize;
    let mut trailing_spaces = 0usize;
    let mut printed_space_end = 0;
    let mut units = 0u32;
    for (index, source) in rows
        .iter()
        .enumerate()
        .skip(start.0)
        .take(end.0 - start.0 + 1)
    {
        let cells = &source.row.cells;
        let first_col = if index == start.0 { start.1 } else { 0 };
        let last_col = if index == end.0 { end.1 } else { cells.len() };
        let mut boundaries = offsets
            .as_ref()
            .map(|_| Vec::with_capacity(last_col - first_col + 1));
        for (offset, cell) in cells[first_col..last_col].iter().enumerate() {
            if let Some(boundaries) = &mut boundaries {
                boundaries.push(units);
            }
            let col = first_col + offset;
            if cell.flags.wide_continuation() || source.synthetic_gap == Some(col) {
                continue;
            }
            if require_literal && cell.flags.tab_padding() {
                return Err("terminal range has tab expansion".into());
            }
            if let Some(cluster) = source.annotations.as_ref().and_then(|row| row.text(col)) {
                if MATERIALIZE {
                    text.push_str(cluster);
                } else {
                    text_bytes += cluster.len();
                    let trailing = cluster.len() - cluster.trim_end_matches(' ').len();
                    trailing_spaces = if trailing == cluster.len() {
                        trailing_spaces + trailing
                    } else {
                        trailing
                    };
                }
                if boundaries.is_some() {
                    units += cluster.encode_utf16().count() as u32;
                }
            } else {
                if MATERIALIZE {
                    text.push(cell.character);
                } else {
                    text_bytes += cell.character.len_utf8();
                    trailing_spaces = if cell.character == ' ' {
                        trailing_spaces + 1
                    } else {
                        0
                    };
                }
                if boundaries.is_some() {
                    units += cell.character.len_utf16() as u32;
                }
            }
            if cell.flags.printed_space() {
                printed_space_end = if MATERIALIZE { text.len() } else { text_bytes };
            }
        }
        if let (Some(offsets), Some(mut boundaries)) = (offsets.as_deref_mut(), boundaries) {
            boundaries.push(units);
            offsets.push(boundaries);
        }
        if index < end.0 && !source.row.wrapped {
            // Physical trailing blank padding is not copied across a hard line.
            let length = if MATERIALIZE { text.len() } else { text_bytes };
            let trimmed = if MATERIALIZE {
                text.trim_end_matches(' ').len()
            } else {
                text_bytes - trailing_spaces
            };
            let end = trimmed.max(printed_space_end);
            if let Some(offsets) = offsets.as_deref_mut() {
                units -= (length - end) as u32;
                for boundary in offsets
                    .iter_mut()
                    .rev()
                    .flat_map(|row| row.iter_mut().rev())
                {
                    if *boundary <= units {
                        break;
                    }
                    *boundary = units;
                }
            }
            if MATERIALIZE {
                text.truncate(end);
            } else {
                trailing_spaces = trailing_spaces.saturating_sub(length - end);
                text_bytes = end;
            }
            if index + 1 < rows.len() {
                if MATERIALIZE {
                    text.push('\n');
                } else {
                    text_bytes += 1;
                    trailing_spaces = 0;
                }
                units += 1;
            }
        }
        if (if MATERIALIZE { text.len() } else { text_bytes }) > MAX_COPY_BYTES {
            return Err("selection byte limit".into());
        }
    }
    Ok(text)
}

/// Single owner of an incremental parser, screen, modes, history and frame revisions.
///
/// This initial profile preserves the Aether standalone terminal contract. It
/// accepts bounded control strings, applies ordered local effects, and returns
/// reply bytes to the caller. PTY I/O and effects with external side effects
/// belong to the owning session.
pub struct Terminal {
    mouse_report_generation: u64,
    epoch: NonZeroU64,
    max_cols: usize,
    vt: crate::vt_state::VtState,
    underline_pen: Option<UnderlinePen>,
    hyperlinks: Option<(HyperlinkPolicy, HyperlinkTable)>,
    graphemes: Option<GraphemeInput>,
    window: Option<WindowState>,
    device_answers: bool,
    colors: Option<Arc<ColorState>>,
    color_revision: u64,
    revision: u64,
    layout: u64,
    unsupported: u64,
    unsupported_events: Vec<String>,
    negative_query_replies: u64,
    history: RetainedHistory<ScrollbackLine>,
    history_row_limit: usize,
    history_end: u64,
    row_sources: Option<RowSources>,
    next_inline_image_id: u64,
    inline_graphics: Option<InlineGraphics>,
    retained_layout: Mutex<Weak<HistoryLayout>>,
    preserve_primary_on_resize: bool,
    reflow_primary_on_resize: bool,
    command_blocks: Option<CommandBlocks>,
    capture_sync_end: bool,
    completed_sync_frame: Option<CellFrame>,
    last_frame: Mutex<FrameCache>,
}

#[derive(Default)]
struct FrameCache {
    frame: Option<CellFrame>,
    observed_revision: u64,
    color_revision: u64,
    history_extent: std::ops::Range<u64>,
    row_identity_revision: u64,
    row_links: Vec<Vec<HyperlinkId>>,
    row_link_scratch: Vec<HyperlinkId>,
}

fn visible_hyperlinks(
    table: &HyperlinkTable,
    row_links: &mut Vec<Vec<HyperlinkId>>,
    scratch: &mut Vec<HyperlinkId>,
    previous: Option<&CellFrame>,
    rows: &[Row],
) -> Option<HyperlinkResources> {
    let reuse = previous.is_some() && row_links.len() == rows.len();
    if let Some(frame) = previous.filter(|frame| reuse && std::ptr::eq(frame.rows(), rows)) {
        return frame.shared_hyperlinks();
    }
    row_links.resize_with(rows.len(), Vec::new);
    let mut links_changed = !reuse;
    for (index, (row, ids)) in rows.iter().zip(row_links.iter_mut()).enumerate() {
        if reuse
            && previous.is_some_and(|frame| Arc::ptr_eq(&frame.rows()[index].cells, &row.cells))
        {
            continue;
        }
        scratch.clear();
        for cell in row.cells.iter() {
            let id = cell.hyperlink_id;
            if !id.is_none() && scratch.last() != Some(&id) {
                scratch.push(id);
            }
        }
        scratch.sort_unstable();
        scratch.dedup();
        if *ids != *scratch {
            std::mem::swap(ids, scratch);
            links_changed = true;
        }
    }
    if !links_changed {
        // IDs belong to this append-only table and keep their targets.
        return previous.and_then(CellFrame::shared_hyperlinks);
    }
    table.snapshot(row_links.iter().flatten().copied())
}

#[derive(Default)]
pub struct WindowUpdates {
    pub title: Option<String>,
    pub bell: bool,
}

#[derive(Default)]
struct WindowState {
    titles: TitleState,
    title_changed: bool,
    bell: bool,
    directory_uri: Option<String>,
}

impl WindowState {
    fn handle(
        &mut self,
        event: &Event<'_>,
        osc: Option<crate::osc::OscPacket<'_>>,
    ) -> Option<Effects> {
        match event {
            Event::String {
                kind: StringKind::Osc,
                ..
            } => {
                use crate::osc::OscCode;

                let packet = osc?;
                packet.value_bytes()?; // Window OSCs require a delimiter.
                let value = packet.text()?;
                match packet.code() {
                    OscCode::Directory => {
                        // Keep the URI authority for local and remote panes to check.
                        let accepts_host = |host: &str| {
                            host.len() <= 255
                                && host.bytes().all(|byte| {
                                    byte.is_ascii_alphanumeric() || b".-_:[]".contains(&byte)
                                })
                        };
                        if value.starts_with("file://")
                            && crate::shell_markers::decode_osc7_cwd(value, accepts_host, |_| false)
                                .is_some()
                        {
                            self.directory_uri = Some(value.to_owned());
                        }
                    }
                    OscCode::Title(command) => {
                        let title = TitleState::reportable_title(value);
                        self.title_changed |= command != "1" && title != self.titles.window_title;
                        self.titles.set_osc(command, &title);
                    }
                    _ => return None,
                }
                Some(Effects::new())
            }
            Event::Csi {
                params,
                private_prefix: None,
                intermediates,
                final_byte: b't',
                ..
            } if intermediates.is_empty() && matches!(params.first(), Some(20..=23)) => {
                let previous = self.titles.window_title.clone();
                let effects = self.titles.apply_window_op(params);
                self.title_changed |= previous != self.titles.window_title;
                Some(effects)
            }
            _ => None,
        }
    }

    fn take_updates(&mut self) -> WindowUpdates {
        let title =
            std::mem::take(&mut self.title_changed).then(|| self.titles.window_title.clone());
        let bell = std::mem::take(&mut self.bell);
        WindowUpdates { title, bell }
    }
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
        Self::with_grid_bound(rows, cols, epoch, MAX_COLS)
    }

    /// Keep the native host's existing width limit separate from wire limits.
    pub fn new_host_viewport(rows: usize, cols: usize) -> Result<Self, String> {
        Self::with_grid_bound(rows, cols, NonZeroU64::MIN, HOST_MAX_COLS)
    }

    fn with_grid_bound(
        rows: usize,
        cols: usize,
        epoch: NonZeroU64,
        max_cols: usize,
    ) -> Result<Self, String> {
        dimensions(rows, cols, max_cols)?;
        let revision = 1;
        let mut screen = ScreenState::new(rows, cols);
        screen.grid.begin_capture_revision(revision);
        screen.alt_grid.begin_capture_revision(revision);
        Ok(Self {
            mouse_report_generation: 1,
            epoch,
            max_cols,
            vt: crate::vt_state::VtState::adopt_hosted(
                screen,
                initial_protocol(),
                Parser::with_bounded_profile(
                    ParserLimits {
                        pending_bytes: 4096,
                        kitty_recovery_bytes: 4096,
                    },
                    ParserProfile::EmberV1,
                )?,
            ),
            underline_pen: None,
            hyperlinks: None,
            graphemes: None,
            window: None,
            device_answers: false,
            colors: None,
            color_revision: 0,
            revision,
            layout: 1,
            unsupported: 0,
            unsupported_events: Vec::new(),
            negative_query_replies: 0,
            history: RetainedHistory::default(),
            history_row_limit: MAX_HISTORY_ROWS,
            history_end: 0,
            row_sources: None,
            next_inline_image_id: 0,
            inline_graphics: None,
            retained_layout: Mutex::new(Weak::new()),
            preserve_primary_on_resize: false,
            reflow_primary_on_resize: false,
            command_blocks: None,
            capture_sync_end: false,
            completed_sync_frame: None,
            last_frame: Mutex::new(FrameCache::default()),
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

    /// Enable bounded direct Kitty graphics on this terminal's own parser.
    pub fn enable_inline_graphics(&mut self, cell_pixels: (u16, u16), decode_png: PngDecoder) {
        self.vt.parser.enable_inline_graphics_packets();
        self.inline_graphics
            .get_or_insert_with(|| InlineGraphics::new(cell_pixels, decode_png));
    }

    fn cancel_inline_upload(&mut self) {
        if let Some(graphics) = &mut self.inline_graphics {
            graphics.cancel();
        }
    }

    /// Preserve streaming grapheme continuations in paint and history.
    /// Opt in before feeding output. Presentation selectors can resize a
    /// provisional glyph while keeping committed scrolling effects.
    pub fn enable_grapheme_clusters(&mut self) {
        self.graphemes.get_or_insert_with(GraphemeInput::default);
    }

    /// Opt in to bounded Bell and OSC title notifications for the owning view.
    pub fn enable_window_updates(&mut self) {
        self.window.get_or_insert_with(WindowState::default);
    }

    /// Enable terminal identification for interactive frontends that advertise
    /// the shared terminal to applications in their own PTY.
    pub fn enable_device_answers(&mut self) {
        self.device_answers = true;
    }

    /// Native key controllers supply the base key in the active layout and
    /// deliver committed IME text separately from physical key presses.
    pub fn enable_keyboard_disambiguation(&mut self) {
        let before = self.alternate_scroll_profile();
        self.vt
            .protocol
            .limit_keyboard_enhancements(u16::from(crate::input::KITTY_DISAMBIGUATE));
        if self.alternate_scroll_profile() != before {
            self.retire_mouse_reports();
        }
    }

    pub fn take_window_updates(&mut self) -> WindowUpdates {
        self.window
            .as_mut()
            .map_or_else(WindowUpdates::default, WindowState::take_updates)
    }

    /// Supply the native theme and admit OSC palette and default-color operations.
    pub fn set_color_defaults(&mut self, defaults: TerminalDefaultColors) {
        if self
            .colors
            .as_ref()
            .is_some_and(|colors| colors.default_colors == defaults)
        {
            return;
        }
        let Some(revision) = self.revision.checked_add(1) else {
            return;
        };
        let mut next = self
            .colors
            .as_deref()
            .cloned()
            .unwrap_or_else(|| ColorState::new(defaults));
        next.default_colors = defaults;
        self.begin_revision(revision);
        self.publish_colors(next);
    }

    pub fn paint_colors(&self) -> Option<(u64, Arc<ColorState>)> {
        self.colors
            .as_ref()
            .map(|colors| (self.color_revision, Arc::clone(colors)))
    }

    fn publish_colors(&mut self, state: ColorState) {
        if self.colors.as_deref() != Some(&state) {
            self.colors = Some(Arc::new(state));
            self.color_revision = self.color_revision.wrapping_add(1);
        }
    }

    fn color_event(&mut self, packet: crate::osc::OscPacket<'_>) -> Option<Effects> {
        let colors = self.colors.as_deref()?;
        let crate::osc::OscCode::Color(command) = packet.code() else {
            return None;
        };
        let value = packet.text()?;
        if packet.payload_len() > 4096 {
            return Some(Effects::new());
        }
        if let Some(effects) = colors.query_ember_osc(command, value, packet.terminator()) {
            return Some(effects);
        }
        let mut next = colors.clone();
        let effects = next.apply_ember_osc(command, value, packet.terminator());
        self.publish_colors(next);
        Some(effects)
    }

    pub fn window_title(&self) -> Option<&str> {
        self.window
            .as_ref()
            .map(|window| window.titles.window_title.as_str())
    }

    /// The last terminal-reported directory URI. This is PTY output, so
    /// applications still need pane and host authority before using it.
    pub fn reported_directory_uri(&self) -> Option<&str> {
        self.window.as_ref()?.directory_uri.as_deref()
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
        self.vt.sync_output.publication_held_at(now)
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
    /// Encode a logical key or one layout-resolved printable Unicode scalar.
    pub fn encode_key(
        &self,
        name: &str,
        shift: bool,
        alt: bool,
        ctrl: bool,
    ) -> Result<Vec<u8>, String> {
        self.encode_key_with_base(name, None, shift, alt, ctrl)
    }

    /// Encode the pressed key and its unshifted value in the same layout.
    pub fn encode_key_with_base(
        &self,
        name: &str,
        base: Option<char>,
        shift: bool,
        alt: bool,
        ctrl: bool,
    ) -> Result<Vec<u8>, String> {
        use crate::input::{
            encode_kitty_disambiguated_key, legacy_function_key_sequence,
            xterm_encode_modify_other_keys, Key, KittyKey, Modifiers,
        };
        let flags = self.vt.protocol.keyboard_enhancement_flags();
        if flags != 0 && self.vt.protocol.keyboard_enhancement_limit().is_none() {
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
        if flags != 0 {
            let kitty_key = match key {
                Key::Enter => KittyKey::Enter,
                Key::Escape => KittyKey::Escape,
                Key::Backspace => KittyKey::Backspace,
                Key::Tab => KittyKey::Tab,
                Key::Character(' ') => KittyKey::Space,
                Key::Character(ch) => KittyKey::Unicode(
                    base.filter(|ch| !ch.is_control())
                        .unwrap_or_else(|| ch.to_lowercase().next().unwrap_or(ch)),
                ),
                _ => KittyKey::Functional,
            };
            if let Some(bytes) = encode_kitty_disambiguated_key(kitty_key, modifiers, flags as u8) {
                return Ok(bytes);
            }
        }
        if let Some(sequence) =
            legacy_function_key_sequence(key, modifiers, self.vt.protocol.modes().contains(&1))
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
                self.vt.protocol.xterm_modify_other_keys(),
                self.vt.protocol.xterm_format_other_keys(),
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
    /// Encode the focus notification requested by DECSET 1004.
    pub fn encode_focus(&self, focused: bool) -> Option<&'static [u8]> {
        self.vt
            .protocol
            .modes()
            .contains(&1004)
            .then_some(if focused { b"\x1b[I" } else { b"\x1b[O" })
    }

    /// Effective pointer tracking and encoding from the current live modes.
    pub fn mouse_report_profile(&self) -> Option<crate::mouse::MouseReportProfile> {
        self.vt.protocol.modes().mouse_report_profile()
    }

    /// Wheel input is routed to unmodified cursor keys on the alternate
    /// screen only when tracking is off and the active keyboard encoder can
    /// admit those keys. Unsupported keyboard profiles never claim this route.
    pub fn alternate_scroll_active(&self) -> bool {
        self.vt.screen.use_alt_buffer
            && self.vt.protocol.modes().contains(&1007)
            && !self.vt.protocol.modes().is_mouse_enabled()
            && (self.vt.protocol.keyboard_enhancement_flags() == 0
                || self.vt.protocol.keyboard_enhancement_limit().is_some())
    }

    fn alternate_scroll_profile(&self) -> Option<(bool, u16)> {
        self.alternate_scroll_active().then(|| {
            (
                self.vt.protocol.modes().contains(&1),
                self.vt.protocol.keyboard_enhancement_flags(),
            )
        })
    }

    /// Identity of the current pointer negotiation and active screen.
    /// A round trip inside one output batch still retires old gestures.
    pub fn mouse_report_generation(&self) -> u64 {
        self.mouse_report_generation
    }

    fn retire_mouse_reports(&mut self) {
        self.mouse_report_generation = self.mouse_report_generation.saturating_add(1);
    }

    pub fn encode_mouse(
        &self,
        input: MouseInput,
        column: usize,
        row: usize,
        modifiers: u8,
    ) -> Option<Vec<u8>> {
        if column >= self.vt.screen.grid.cols() || row >= self.vt.screen.grid.rows() {
            return None;
        }
        let modes = self.vt.protocol.modes();
        let modifiers = modifiers & 0x1c;
        match input {
            MouseInput::Press(button) => modes.get_mouse_report(button | modifiers, column, row),
            MouseInput::Release(button) => {
                modes.get_mouse_release_report(button | modifiers, column, row)
            }
            MouseInput::Motion(button) => {
                let code = button.unwrap_or(3) | modifiers | 32;
                modes
                    .should_report_mouse_motion(button.is_some())
                    .then(|| modes.get_mouse_report(code, column, row))
                    .flatten()
            }
            MouseInput::WheelUp | MouseInput::WheelDown => {
                let up = input == MouseInput::WheelUp;
                if self.alternate_scroll_active() {
                    return self
                        .encode_key(
                            if up { "ArrowUp" } else { "ArrowDown" },
                            false,
                            false,
                            false,
                        )
                        .ok();
                }
                modes.get_mouse_report((if up { 64 } else { 65 }) | modifiers, column, row)
            }
        }
    }

    /// Preserve a complete IME commit as UTF-8. The host pairs key and IME
    /// delivery so each committed string reaches the PTY exactly once.
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
        if self.vt.protocol.keyboard_enhancement_flags() != 0
            && self.vt.protocol.keyboard_enhancement_limit().is_none()
        {
            return Err("IME committed text with Kitty keyboard flags is unsupported".into());
        }
        Ok(text.as_bytes().to_vec())
    }
    /// Wire-size bound includes bracket markers. CRLF/CR/LF become CR. All
    /// other controls except TAB are rejected, including ESC and C1 controls.
    /// In unbracketed mode CR may execute shell lines: host must obtain paste intent.
    pub fn encode_paste(&self, text: &str) -> Result<Vec<u8>, String> {
        self.encode_paste_bounded(text, self.bracketed_paste_active(), MAX_INPUT_BYTES)
    }

    pub fn bracketed_paste_active(&self) -> bool {
        self.vt.protocol.modes().contains(&2004)
    }

    /// The native clipboard shortcut preserves an explicit Enter in a shell
    /// that does not request bracketed paste.
    pub fn encode_native_paste(&self, text: &str) -> Result<Vec<u8>, String> {
        if self.bracketed_paste_active() {
            self.encode_paste_bounded(text, true, MAX_STREAMED_PASTE_BYTES)
        } else {
            let mut bytes = self.encode_paste_bounded(text, false, MAX_INPUT_BYTES)?;
            if let Some(first_line_end) = bytes.iter().position(|byte| *byte == b'\r') {
                bytes.truncate(first_line_end);
            }
            Ok(bytes)
        }
    }

    fn encode_paste_bounded(
        &self,
        text: &str,
        bracketed: bool,
        byte_limit: usize,
    ) -> Result<Vec<u8>, String> {
        if text.len() > byte_limit {
            return Err("paste byte limit".into());
        }
        let mut bytes = Vec::with_capacity(text.len() + if bracketed { 12 } else { 0 });
        if bracketed {
            bytes.extend_from_slice(b"\x1b[200~");
        }
        let input = text.as_bytes();
        let mut start = 0;
        for (position, character) in text.char_indices() {
            if matches!(character, '\r' | '\n') {
                bytes.extend_from_slice(&input[start..position]);
                if character == '\r' || position == 0 || input[position - 1] != b'\r' {
                    bytes.push(b'\r');
                }
                start = position + 1;
            } else if character != '\t' && character.is_control() {
                return Err("paste contains control characters".into());
            }
        }
        bytes.extend_from_slice(&input[start..]);
        if bracketed {
            bytes.extend_from_slice(b"\x1b[201~");
        }
        if bytes.len() > byte_limit {
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
            if !self.vt.screen.use_alt_buffer
                && effect.direction == RegionDirection::Up
                && effect.top == 0
            {
                for removed in effect.removed {
                    if let Some(line) = removed.captured {
                        self.append_history(line);
                    }
                }
            }
        }
    }

    fn append_history(&mut self, line: ScrollbackLine) {
        if self.history_row_limit == 0 {
            return;
        }
        let row_id = line.raw_row_id();
        let retained = self
            .history
            .retain_latest(line, self.history_row_limit, drop);
        if let Some(sources) = &mut self.row_sources {
            if retained {
                sources.appended(row_id, self.history_end);
            }
            sources.trim_to(self.history.len());
        }
        self.history_end = self
            .history_end
            .checked_add(1)
            .expect("terminal history row identity exhausted");
    }
    fn history_admitted(&self) -> bool {
        // Inline TUIs can reserve bottom rows for their editor while committed
        // output scrolls off the top of the primary screen.
        self.history_row_limit != 0
            && !self.vt.screen.use_alt_buffer
            && self.vt.screen.scroll_region_top == 0
    }
    fn motion(&mut self, m: VerticalMotion) {
        let capture = self.history_admitted();
        let effect = self
            .vt
            .screen
            .vertical_motion(m, capture, || RowSources::allocate(&mut self.row_sources));
        self.capture_history(effect.region);
    }
    pub fn history_rows(&self) -> usize {
        if self.vt.screen.use_alt_buffer {
            0
        } else {
            self.history.len()
        }
    }

    /// Apply the frontend's existing scrollback preference. Physical history
    /// remains constrained by the shared byte budget.
    pub fn set_history_row_limit(&mut self, rows: usize) {
        if self.history_row_limit == rows {
            return;
        }
        self.history_row_limit = rows;
        self.trim_history();
        if self.history.capacity() > rows {
            self.history.shrink_to_fit();
        }
    }

    fn trim_history(&mut self) {
        self.history.trim_oldest(self.history_row_limit);
        if let Some(sources) = &mut self.row_sources {
            sources.trim_to(self.history.len());
        }
    }

    /// Approximate bytes charged by retained primary history, including when
    /// the alternate screen is active. Immutable pages have their own bounds.
    pub fn history_retained_bytes(&self) -> usize {
        self.history.retained_bytes()
    }
    /// Absolute coordinates of retained physical history rows, oldest first.
    /// Appending advances the exclusive end. Eviction and explicit history
    /// erasure advance the start, so a saved primary-row anchor cannot name
    /// a different row. Alternate screen exposes an empty range at the same end.
    pub fn history_extent(&self) -> std::ops::Range<u64> {
        self.history_end - self.history_rows() as u64..self.history_end
    }

    /// Bits identify live rows whose final physical cell was skipped when a
    /// wide glyph wrapped early. Read alongside the matching paint frame.
    pub fn live_margin_gaps(&self) -> u128 {
        let grid = &self.vt.screen.grid;
        (0..grid.rows())
            .filter(|&row| grid.synthetic_margin_gap(row).is_some())
            .fold(0, |gaps, row| gaps | (1 << row))
    }

    /// Newest-relative history pagination, oldest-to-newest within each page.
    /// Historical widths are retained across resize; there is no history reflow.
    /// Transferred primary rows retain their original width. The alternate
    /// screen exposes no history.
    pub fn history_page(&self, offset: usize, limit: usize) -> Result<HistoryPage, String> {
        let total_rows = self.history_rows();
        if limit == 0 || limit > MAX_ROWS || offset > self.history_row_limit {
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
                annotations: line.row_annotations().cloned(),
                synthetic_gap: line.synthetic_margin_gap(),
            })
            .collect();
        self.make_history_page(offset, total_rows, rows)
    }

    fn make_history_page(
        &self,
        offset: usize,
        total_rows: usize,
        rows: Vec<HistoryCopyRow>,
    ) -> Result<HistoryPage, String> {
        let mut page = HistoryPage {
            revision: self.revision,
            offset,
            total_rows,
            row_count: rows.len(),
            text: String::new(),
            hyperlinks: self.hyperlink_snapshot(rows.iter().map(|row| &row.row)),
            paint_colors: self.colors.clone(),
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
        !self.vt.screen.use_alt_buffer
            && self
                .command_blocks
                .as_ref()
                .is_some_and(CommandBlocks::prompt_ready)
    }

    /// Identifies a single ready prompt in this PTY incarnation. A submitted
    /// line, reset, or alternate screen invalidates the current generation.
    pub fn command_prompt_generation(&self) -> Option<u64> {
        if self.alternate_screen_active() {
            return None;
        }
        self.command_blocks.as_ref()?.prompt_generation()
    }

    /// A command with a usable start marker is still in progress.
    pub fn command_running(&self) -> bool {
        self.command_blocks
            .as_ref()
            .is_some_and(CommandBlocks::command_running)
    }

    pub fn alternate_screen_active(&self) -> bool {
        self.vt.screen.use_alt_buffer
    }

    /// A submitted line can start a foreground command before its PTY markers arrive.
    pub fn observe_prompt_input(&mut self, bytes: &[u8]) {
        if let Some(blocks) = &mut self.command_blocks {
            if bytes.iter().any(|b| matches!(b, 3 | 4 | 10 | 13 | 26)) {
                blocks.input_submitted();
            }
        }
    }

    fn extend_grapheme(&mut self, ch: char, translated: char) -> bool {
        let Some(input) = &self.graphemes else {
            return false;
        };
        let Some(mut anchor) = input.active else {
            return false;
        };
        let Some(base) = self.vt.screen.last_printed_char else {
            return false;
        };
        if base.is_ascii() && ch.is_ascii() {
            return false;
        }
        let mut scalar = [0; 4];
        let preceding = self
            .vt
            .screen
            .grid
            .cluster_text(anchor.row, anchor.col)
            .unwrap_or_else(|| base.encode_utf8(&mut scalar));
        let mut buf = [0; MAX_CLUSTER_BYTES + 4];
        let Some(cluster) = graphemes::extension(preceding, translated, &mut buf) else {
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
            .vt
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
        let cols = self.vt.screen.grid.cols();
        let wrap = self.vt.protocol.modes().contains(&7);
        let (row, col, cleared) = if checkpoint.col + width <= cols {
            (checkpoint.row, checkpoint.col, false)
        } else if wrap {
            // A preflight wrap here is growth from a provisional narrow
            // print, so the live row is still the original departure row.
            debug_assert_eq!(self.vt.screen.cursor_row, checkpoint.row);
            let (row, cleared) = self.vt.screen.wrap_target();
            (row, 0, cleared)
        } else {
            (checkpoint.row, cols - width, false)
        };
        if !cleared {
            let line = if row == anchor.row {
                checkpoint.footprint.row_annotations()
            } else {
                self.vt.screen.grid.row_annotations(row).map(Arc::as_ref)
            };
            let displaced = if self.vt.screen.insert_mode {
                cols - width..cols
            } else {
                col..col + width
            };
            if line.is_some_and(|line| !line.can_store_after_displacing(displaced, bytes)) {
                return None;
            }
        }

        let checkpoint = input.provisional.take()?;
        self.vt
            .screen
            .grid
            .restore_print(anchor.row, checkpoint.footprint);
        if let Some(departure) = checkpoint.departure {
            self.vt.screen.grid.restore_wrap(departure);
        }
        self.vt.screen.cursor_row = checkpoint.row;
        self.vt.screen.cursor_col = checkpoint.col;
        self.vt.screen.pending_wrap = false;
        self.write_graphic(graphic, width);
        let placed = self.graphemes.as_ref()?.active?;
        debug_assert_eq!((placed.row, placed.col), (row, col));
        Some(placed)
    }

    fn print(&mut self, ch: char) {
        let translated = self.vt.screen.translate_graphic(ch);
        if !self.extend_grapheme(ch, translated) {
            self.print_cell(ch, translated);
        }
    }

    fn print_ascii(&mut self, bytes: &[u8]) {
        if bytes.len() < 4 || self.vt.screen.insert_mode || !self.vt.screen.ascii_graphics() {
            for &byte in bytes {
                self.print(char::from(byte));
            }
            return;
        }

        let (&first, tail) = bytes.split_first().expect("nonempty ASCII run");
        self.print(char::from(first));
        if !self
            .vt
            .screen
            .last_printed_char
            .is_some_and(|ch| ch.is_ascii())
        {
            for &byte in tail {
                self.print(char::from(byte));
            }
            return;
        }

        // Keep both boundaries in the scalar path. The leading byte can join
        // a previous grapheme and the trailing byte owns any later extension.
        let (&last, mut middle) = tail.split_last().expect("ASCII run has a tail");
        let autowrap = self.vt.protocol.modes().contains(&7);
        while !middle.is_empty() {
            match self.vt.screen.step_ascii_write(middle, autowrap) {
                AsciiWriteStep::NeedsWrap => self.motion(VerticalMotion::Wrap),
                AsciiWriteStep::Written { consumed, mutation } => {
                    if let (Some(pen), Some(columns)) = (&self.underline_pen, mutation.printed) {
                        self.vt
                            .screen
                            .grid
                            .set_underline_color(mutation.row, columns, pen.current);
                    }
                    middle = &middle[consumed..];
                }
                AsciiWriteStep::Stopped => break,
            }
        }
        self.finish_grapheme();
        for &byte in middle {
            self.print(char::from(byte));
        }
        self.print(char::from(last));
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
        let width = if self.reflow_primary_on_resize && !self.vt.screen.use_alt_buffer {
            width.min(self.vt.screen.grid.cols())
        } else {
            width
        };
        let wrap = self.vt.protocol.modes().contains(&7);
        if self.vt.screen.take_pending_wrap(wrap) {
            self.motion(VerticalMotion::Wrap);
        }
        let origin_row = self.vt.screen.cursor_row;
        let origin_col = self.vt.screen.cursor_col;
        let presentation_can_change = self.graphemes.is_some()
            && self.vt.screen.grid.cols() >= 2
            && if width == 1 {
                graphemes::may_grow(translated)
            } else {
                graphemes::may_contract(translated)
            };
        let early_wrap = self.vt.screen.prepare_character_width(width, wrap);
        let departure = (early_wrap && presentation_can_change && !self.vt.screen.wrap_target().1)
            .then(|| self.vt.screen.grid.checkpoint_wrap(origin_row));
        if early_wrap {
            let capture = self.history_admitted();
            let effect = self.vt.screen.vertical_early_wide_wrap(width, capture, || {
                RowSources::allocate(&mut self.row_sources)
            });
            self.capture_history(effect.region);
        }
        // A committed region scroll can displace retained content and advance
        // history. Only an in-row print or a non-scrolling wrap is reversible.
        let provisional =
            (presentation_can_change && (!early_wrap || departure.is_some())).then(|| {
                ProvisionalPrint {
                    row: origin_row,
                    col: origin_col,
                    footprint: self.vt.screen.grid.checkpoint_print(
                        self.vt.screen.cursor_row,
                        self.vt.screen.cursor_col,
                        width,
                        self.vt.screen.insert_mode,
                    ),
                    departure,
                }
            });
        let written = self.vt.screen.write_char_in_row(translated, width, wrap);
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
            self.vt
                .screen
                .grid
                .set_underline_color(written.row, cols, underline.current);
        }
        self.vt.screen.last_printed_char = Some(translated);
    }

    fn repeat_print(&mut self, count: usize) {
        let Some(ch) = self.vt.screen.last_printed_char else {
            return;
        };
        let cluster = self
            .graphemes
            .as_ref()
            .and_then(|input| input.last_cluster.clone());
        if let Some(cluster) = cluster {
            let width = cluster.width.min(self.vt.screen.grid.cols());
            for _ in 0..count {
                // The retained text already includes the translated base.
                self.write_graphic(ch, width);
                let anchor = self.graphemes.as_ref().and_then(|input| input.active);
                if let Some(anchor) = anchor {
                    if self
                        .vt
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
            let translated = self.vt.screen.translate_char(ch);
            for _ in 0..count {
                self.print_cell(ch, translated);
            }
        }
    }

    fn apply_dec_private_mode(&mut self, mode: u16, set: bool, now: Instant) {
        match mode {
            47 | 1047 | 1049 => {
                let was_alternate = self.vt.screen.use_alt_buffer;
                if set != was_alternate {
                    self.cancel_inline_upload();
                }
                if set {
                    if let Some(entry) = self.vt.screen.enter_alternate_screen(
                        mode == 1049,
                        mode != 47,
                        &self.vt.protocol,
                    ) {
                        if let Some(rows) = entry.cleared_rows {
                            self.vt.screen.grid.row_wrapped[rows].fill(false);
                        }
                        if mode == 1049 {
                            if let Some(underline) = &mut self.underline_pen {
                                underline.primary = Some(underline.current);
                            }
                        }
                        self.vt.protocol.switch_screen();
                    }
                } else if let Some(effect) = self.vt.screen.leave_alternate_screen(mode == 1049) {
                    if self.vt.protocol.keyboard_enhancement_limit().is_some() {
                        self.vt.protocol.reset_keyboard_enhancement();
                    }
                    self.vt.protocol.switch_screen();
                    if let Some(underline) = &mut self.underline_pen {
                        underline.current = underline.primary.take().unwrap_or(Color::Default);
                    }
                    if let Some(wrap) = effect.restore_autowrap {
                        if wrap {
                            self.vt.protocol.modes_mut().insert(7);
                        } else {
                            self.vt.protocol.modes_mut().remove(&7);
                        }
                    }
                }
                // These DEC modes report the same active buffer.
                let alternate = self.vt.screen.use_alt_buffer;
                if alternate != was_alternate {
                    self.retire_mouse_reports();
                }
                let modes = self.vt.protocol.modes_mut();
                for alias in [47, 1047, 1049] {
                    if alternate {
                        modes.insert(alias);
                    } else {
                        modes.remove(&alias);
                    }
                }
                return;
            }
            1048 => {
                if set {
                    semantics::save_cursor_state(
                        &mut self.vt.screen,
                        &self.vt.protocol,
                        self.underline_pen.as_mut(),
                    );
                } else {
                    semantics::restore_cursor_state(
                        &mut self.vt.screen,
                        &mut self.vt.protocol,
                        self.underline_pen.as_mut(),
                    );
                }
            }
            6 => {
                self.vt.screen.origin_mode = set;
                self.vt.screen.set_cursor_position(1, 1);
            }
            7 => {
                if !set {
                    self.vt.screen.pending_wrap = false;
                }
            }
            2026 => {
                if set {
                    self.vt.sync_output.begin_hosted(now);
                } else {
                    self.vt.sync_output.end_hosted();
                }
                return;
            }
            42 => self.vt.screen.national_replacement_mode = set,
            1 | 9 | 25 | 45 | 66 | 1000 | 1002..=1007 | 1015 | 2004 => (),
            _ => {
                self.unsupported();
                return;
            }
        }
        let previous_mouse = matches!(mode, 9 | 1000 | 1002 | 1003 | 1005 | 1006 | 1015)
            .then(|| self.mouse_report_profile());
        let previous_alternate_scroll = matches!(mode, 1 | 1007 | 9 | 1000 | 1002 | 1003)
            .then(|| self.alternate_scroll_profile());
        if set {
            self.vt.protocol.modes_mut().insert(mode);
        } else {
            self.vt.protocol.modes_mut().remove(&mode);
        }
        if previous_mouse.is_some_and(|old| self.mouse_report_profile() != old)
            || previous_alternate_scroll.is_some_and(|old| self.alternate_scroll_profile() != old)
        {
            self.retire_mouse_reports();
        }
    }

    // Apply profile-specific screen operations after shared local dispatch.
    fn reset_terminal(&mut self) {
        self.retire_mouse_reports();
        self.cancel_inline_upload();
        let track_printed_spaces = self.vt.screen.grid.track_printed_spaces;
        self.vt.screen = ScreenState::new(self.vt.screen.grid.rows(), self.vt.screen.grid.cols());
        if track_printed_spaces {
            self.track_printed_spaces();
        }
        self.begin_revision(self.revision);
        let keyboard_limit = self.vt.protocol.keyboard_enhancement_limit();
        let mut protocol = initial_protocol();
        protocol.take_rendition_stack_from(&mut self.vt.protocol);
        self.vt.protocol = protocol;
        if let Some(limit) = keyboard_limit {
            self.vt.protocol.limit_keyboard_enhancements(limit);
        }
        if let Some(pen) = &mut self.underline_pen {
            *pen = UnderlinePen::default();
        }
        if let Some(graphemes) = &mut self.graphemes {
            *graphemes = GraphemeInput::default();
        }
        if let Some(colors) = &self.colors {
            self.publish_colors(ColorState::new(colors.defaults()));
        }
        self.history.clear();
        self.clear_transcript_archive();
        self.renew_live_source_rows();
        self.vt.sync_output.end_hosted();
        if let Some(blocks) = &mut self.command_blocks {
            blocks.reset_live();
        }
    }

    fn apply_profile_event(&mut self, event: &Event<'_>, now: Instant) -> bool {
        match event {
            Event::Escape {
                intermediates,
                final_byte,
            } if intermediates.is_empty() => match final_byte {
                b'D' => self.motion(VerticalMotion::LineFeed),
                b'E' => {
                    self.vt.screen.begin_next_line();
                    self.motion(VerticalMotion::LineFeed);
                }
                b'6' | b'9' => {
                    if let Some(mut columns) = crate::column_edit::prepare_column_index(
                        &mut self.vt.screen,
                        *final_byte == b'6',
                        &self.vt.protocol,
                    ) {
                        while columns.step(&mut self.vt.screen).is_some() {}
                    }
                }
                b'M' => self.motion(VerticalMotion::ReverseIndex),
                b'c' => self.reset_terminal(),
                _ => return false,
            },
            Event::Escape {
                intermediates,
                final_byte: b'8',
            } if intermediates == b"#" => {
                // Hosted DECALN restores absolute addressing before homing.
                self.vt.screen.origin_mode = false;
                self.vt.protocol.modes_mut().remove(&6);
                let protected = self.vt.screen.current_flags.protected();
                self.vt.screen.current_flags = crate::StyleFlags::default();
                self.vt.screen.current_flags.set_protected(protected);
                self.vt.screen.screen_alignment_test();
                self.renew_active_source_rows();
            }
            Event::Csi {
                params,
                colon_flags,
                private_prefix: None,
                intermediates,
                final_byte: final_byte @ (b'r' | b't'),
                ..
            } if intermediates == b"$" && !colon_flags.iter().any(|flag| *flag) => {
                if let Some(mut rectangle) = crate::rectangular_edit::prepare_rectangular_attributes(
                    &self.vt.screen,
                    params,
                    *final_byte as char,
                    None,
                    intermediates,
                    self.vt.protocol.rectangular_attribute_extent(),
                ) {
                    while rectangle.step(&mut self.vt.screen).is_some() {}
                }
            }
            Event::Csi {
                params,
                colon_flags,
                private_prefix: None,
                intermediates,
                final_byte: final_byte @ (b'z' | b'{' | b'x' | b'v'),
                ..
            } if intermediates == b"$" && !colon_flags.iter().any(|flag| *flag) => {
                if let Some(mut rectangle) = crate::rectangular_edit::prepare_rectangular_edit(
                    &self.vt.screen,
                    params,
                    *final_byte as char,
                    None,
                    intermediates,
                ) {
                    if let Some(pen) = &self.underline_pen {
                        rectangle.set_fill_underline_color(pen.current);
                    }
                    while rectangle.step(&mut self.vt.screen).is_some() {}
                }
            }
            Event::Csi {
                params,
                colon_flags,
                private_prefix: None,
                intermediates,
                final_byte: final_byte @ (b'}' | b'~' | b'@' | b'A'),
                ..
            } if ((intermediates == b"'" && matches!(final_byte, b'}' | b'~'))
                || (intermediates == b" " && matches!(final_byte, b'@' | b'A')))
                && !colon_flags.iter().any(|flag| *flag) =>
            {
                if let Some(mut columns) = crate::column_edit::prepare_column_edit(
                    &self.vt.screen,
                    params,
                    *final_byte as char,
                    None,
                    intermediates,
                ) {
                    while columns.step(&mut self.vt.screen).is_some() {}
                }
            }
            Event::Csi {
                params,
                colon_flags,
                private_prefix,
                intermediates,
                final_byte,
                ..
            } if intermediates.is_empty() => {
                let first = params.first().copied().unwrap_or(0);
                let count = usize::from(first.max(1));
                match (*private_prefix, *final_byte) {
                    (Some(b'?'), b'h' | b'l') => {
                        for &mode in params.iter() {
                            self.apply_dec_private_mode(mode, *final_byte == b'h', now);
                        }
                    }
                    (Some(b'?'), b's') if !colon_flags.iter().any(|flag| *flag) => {
                        crate::mode_mutation::save_dec_private_modes(
                            &self.vt.screen,
                            &mut self.vt.protocol,
                            crate::mode_mutation::SavedDecModeProfile::Hosted,
                            params,
                        );
                    }
                    (Some(b'?'), b'r') if !colon_flags.iter().any(|flag| *flag) => {
                        let modes = crate::mode_mutation::restore_dec_private_modes(
                            &self.vt.protocol,
                            crate::mode_mutation::SavedDecModeProfile::Hosted,
                            params,
                        );
                        for (mode, enabled) in modes {
                            self.apply_dec_private_mode(mode, enabled, now);
                        }
                    }
                    (None, b'h' | b'l') => {
                        for &mode in params.iter() {
                            if mode == 4 {
                                self.vt.screen.insert_mode = *final_byte == b'h';
                            } else {
                                self.unsupported();
                            }
                        }
                    }
                    (None, b'J') if first == 2 => {
                        self.vt.screen.erase_screen();
                        self.renew_active_source_rows();
                        // Erased physical rows no longer continue the previous
                        // text in the standalone terminal's logical projection.
                        self.vt.screen.grid.row_wrapped.fill(false);
                    }
                    (None | Some(b'?'), b'J') => {
                        let selective = *private_prefix == Some(b'?');
                        let rows = self.vt.screen.grid.rows();
                        let cols = self.vt.screen.grid.cols();
                        let cursor = self.vt.screen.cursor_row;
                        match first {
                            0..=2 => {
                                for row in 0..rows {
                                    let span = match first {
                                        0 if row > cursor => Some((0, cols)),
                                        0 if row == cursor => {
                                            Some((self.vt.screen.cursor_col, cols))
                                        }
                                        1 if row < cursor => Some((0, cols)),
                                        1 if row == cursor => {
                                            Some((0, self.vt.screen.cursor_col + 1))
                                        }
                                        2 => Some((0, cols)),
                                        _ => None,
                                    };
                                    if let Some((start, end)) = span {
                                        let erased =
                                            self.vt.screen.erase_span(row, start, end, selective);
                                        // A fully cleared physical row is no longer part of
                                        // the logical line previously wrapped at its margin.
                                        if matches!(erased.as_slice(), [change] if change.changed == (0..cols))
                                        {
                                            self.vt.screen.grid.row_wrapped[row] = false;
                                            // A shell's line editor redraws from its
                                            // prompt with ED0. Clearing cells in place
                                            // does not replace the physical source row.
                                        }
                                    }
                                }
                                self.vt.screen.pending_wrap = false;
                            }
                            // XTerm extends ED and DECSED with saved-line erasure.
                            // Alternate applications cannot erase primary history.
                            3 => {
                                if !self.vt.screen.use_alt_buffer {
                                    self.history.clear();
                                    self.clear_transcript_archive();
                                }
                            }
                            _ => self.unsupported(),
                        }
                    }
                    (None, b'L' | b'M') => {
                        let top = self.vt.screen.cursor_row;
                        let bottom = self.vt.screen.scroll_region_bottom;
                        if top >= self.vt.screen.scroll_region_top && top <= bottom {
                            let direction = if *final_byte == b'L' {
                                RegionDirection::Down
                            } else {
                                RegionDirection::Up
                            };
                            self.vt
                                .screen
                                .edit_lines(direction, top, bottom, count, || {
                                    RowSources::allocate(&mut self.row_sources)
                                });
                        }
                    }
                    (None, b'S' | b'T' | b'^') => {
                        let top = self.vt.screen.scroll_region_top;
                        let bottom = self.vt.screen.scroll_region_bottom;
                        let direction = if *final_byte == b'S' {
                            RegionDirection::Up
                        } else {
                            RegionDirection::Down
                        };
                        for _ in 0..count.min(bottom - top + 1) {
                            let capture = self.history_admitted()
                                && bottom + 1 == self.vt.screen.grid.rows()
                                && direction == RegionDirection::Up;
                            let effect = self.vt.screen.scroll_region_one(
                                direction,
                                top,
                                bottom,
                                capture,
                                || RowSources::allocate(&mut self.row_sources),
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

    fn begin_revision(&mut self, revision: u64) {
        self.revision = revision;
        self.vt.screen.grid.begin_capture_revision(revision);
        self.vt.screen.alt_grid.begin_capture_revision(revision);
    }

    /// Apply bytes sampled at a monotonic instant supplied by the session.
    /// A reply-limit error still consumes the entire input span.
    pub fn feed_at(&mut self, input: &[u8], now: Instant) -> Result<Vec<u8>, String> {
        let replies = self.feed_bounded_at(input, now)?;
        match replies.limit_error {
            Some(error) => Err(error.into()),
            None => Ok(replies.bytes),
        }
    }

    /// Apply one bounded span and report reply pressure separately from parser
    /// failure. Once the reply budget is exhausted, keep the admitted prefix at
    /// whole-reply boundaries and continue processing output and effects.
    pub fn feed_bounded_at(&mut self, input: &[u8], now: Instant) -> Result<FeedReplies, String> {
        if input.len() > MAX_FEED_BYTES {
            return Err("feed byte limit".into());
        }
        self.begin_revision(self.revision.checked_add(1).ok_or("revision exhausted")?);
        use crate::feed::FeedError;
        let source_base = self
            .command_blocks
            .as_mut()
            .and_then(|blocks| blocks.begin_feed(input.len()));
        let mut replies = FeedReplies::default();
        crate::vt_state::VtState::drive(
            self,
            input,
            |terminal| &mut terminal.vt,
            |terminal, step| terminal.dispatch_feed_step(step, source_base, now, &mut replies),
        )
        .map_err(|error| match error {
            FeedError::NoProgress => "parser made no progress".into(),
            FeedError::Dispatch(error) => error,
        })?;
        Ok(replies)
    }

    fn dispatch_feed_step(
        &mut self,
        step: crate::feed::FeedStep<'_>,
        source_base: Option<u64>,
        now: Instant,
        replies: &mut FeedReplies,
    ) -> Result<(), String> {
        if step.abandoned_apc {
            self.cancel_inline_upload();
        }
        if step.rejected_resource {
            if self.hyperlinks.is_some() {
                self.vt.screen.current_hyperlink = HyperlinkId::NONE;
            }
            self.finish_grapheme();
        }
        let Some(event) = step.event else {
            return Ok(());
        };
        if !matches!(&event, Event::Print(_) | Event::Ascii(_)) {
            self.finish_grapheme();
        }
        if let Event::String {
            kind: StringKind::Apc,
            payload,
            ..
        } = &event
        {
            if kitty_graphics::is_graphics_payload(payload) {
                if let Some(mut graphics) = self.inline_graphics.take() {
                    let cursor = (self.vt.screen.cursor_col, self.vt.screen.cursor_row);
                    let reply = graphics.apply(self, cursor, payload);
                    self.inline_graphics = Some(graphics);
                    if let Some(reply) = reply {
                        replies.push(reply.as_bytes(), "reply byte limit");
                    }
                    return Ok(());
                }
            }
        }
        if self.command_blocks.as_mut().is_some_and(|blocks| {
            let event_end = source_base.map(|base| base + step.end_offset as u64);
            let geometry = (self.vt.screen.grid.cols(), self.vt.screen.grid.rows());
            let style = crate::archived_text::ArchiveStyle::from_rendition(
                self.vt.screen.current_fg,
                self.vt.screen.current_bg,
                self.underline_pen
                    .as_ref()
                    .map_or(Color::Default, |pen| pen.current),
                self.vt.screen.current_flags,
            );
            let target = self
                .hyperlinks
                .as_ref()
                .and_then(|(_, table)| table.resolve(self.vt.screen.current_hyperlink));
            blocks.observe(
                &event,
                self.vt.screen.use_alt_buffer,
                event_end,
                geometry,
                style,
                target,
            )
        }) {
            return Ok(());
        }
        let osc = match &event {
            Event::String {
                kind: StringKind::Osc,
                payload,
                bel_terminated,
            } => Some(crate::osc::OscPacket::decode(payload, *bel_terminated)),
            _ => None,
        };
        if let (Some((policy, table)), Some(packet)) = (&mut self.hyperlinks, osc) {
            if packet.code() == crate::osc::OscCode::Hyperlink {
                // In this profile invalid UTF-8, or a command without fields,
                // closes the active link rather than leaving it on subsequent text.
                self.vt.screen.current_hyperlink =
                    packet.text().map_or(HyperlinkId::NONE, |fields| {
                        table.apply_osc8(fields, *policy)
                    });
                return Ok(());
            }
        }
        // Unsupported SCS designations leave the selected bank intact.
        // The legacy native profiles retain their own designation policy.
        if matches!(&event, Event::Charset { designator, .. }
            if !matches!(designator, b'0' | b'A' | b'B'))
        {
            self.unsupported();
            self.record_unsupported_event(&event);
            return Ok(());
        }
        if self.device_answers {
            if let Some(reply) = device_answer(&event) {
                replies.push(reply, "reply byte limit");
                return Ok(());
            }
        }
        if let Event::String {
            kind: StringKind::Dcs,
            payload,
            ..
        } = &event
        {
            if let Some(selector) = payload.strip_prefix(b"$q") {
                let reply = semantics::restorable_decrqss_reply(
                    &self.vt.screen,
                    &self.vt.protocol,
                    selector,
                    self.underline_pen.as_ref().map(|pen| pen.current),
                );
                replies.push(&reply, "reply limit");
                return Ok(());
            }
            if let Some(reply) = crate::termcap_query::termcap_query(payload, &self.vt.protocol) {
                replies.push(&reply, "reply limit");
                if reply.as_ref() == crate::termcap_query::NEGATIVE_TERMCAP_REPLY {
                    if replies.limit_error.is_none() {
                        self.negative_query_replies = self.negative_query_replies.saturating_add(1);
                    }
                    self.unsupported();
                    self.record_unsupported_event(&event);
                }
                return Ok(());
            }
        }
        // DA3 defines an anonymous unit ID. Hosted DA1/DA2 have no identity
        // contract. Keep unsupported CSI c distinct from ESC c (RIS).
        if let Event::Csi {
            raw_params,
            private_prefix,
            intermediates,
            final_byte: b'c',
            ..
        } = &event
        {
            if let Some(reply) = semantics::tertiary_device_attributes_reply(
                *private_prefix,
                raw_params,
                intermediates,
            ) {
                replies.push(reply, "reply limit");
            } else {
                let before = self.unsupported;
                self.unsupported();
                if self.unsupported != before {
                    self.record_unsupported_event(&event);
                }
            }
            return Ok(());
        }
        // Vim sends 0%m as an unknown-sequence probe. This profile
        // preserves drawing state when SGR has a nonempty intermediate.
        if matches!(&event, Event::Csi { intermediates, final_byte: b'm', .. } if !intermediates.is_empty())
        {
            self.unsupported();
            self.record_unsupported_event(&event);
            return Ok(());
        }
        if matches!(&event, Event::Csi {
            params,
            private_prefix: Some(b'?'),
            intermediates,
            final_byte: b'p',
            ..
        } if intermediates == b"$" && params.first() == Some(&1048))
        {
            // DECSC/DECRC share a retained slot across all save aliases.
            let reply = if self.vt.screen.saved_state.is_some() {
                b"\x1b[?1048;1$y"
            } else {
                b"\x1b[?1048;2$y"
            };
            replies.push(reply, "reply limit");
            return Ok(());
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
        if sync_query {
            self.vt
                .sync_output
                .reflect_query(&mut self.vt.protocol, now);
        }
        let capture_end = self.capture_sync_end
            && self.vt.sync_output.started.is_some()
            && matches!(&event, Event::Csi { private_prefix: Some(b'?'), intermediates, final_byte: b'l', params, .. }
                if intermediates.is_empty() && params.contains(&2026));
        // Only admitted keyboard-stack edits and DECSTR can change the
        // alternate wheel route outside the ordered DEC mode executor.
        let watch_alternate_keyboard = matches!(&event, Event::Csi { private_prefix: Some(b'=' | b'>' | b'<'), final_byte: b'u', intermediates, .. } if intermediates.is_empty())
            || matches!(&event, Event::Csi { private_prefix: None, final_byte: b'p', intermediates, .. } if intermediates == b"!");
        let previous_alternate_keyboard =
            watch_alternate_keyboard.then(|| self.alternate_scroll_profile());
        let dispatch = self
            .window
            .as_mut()
            .and_then(|window| window.handle(&event, osc))
            .or_else(|| self.color_event(osc?))
            .map_or_else(
                || {
                    semantics::dispatch_local_with_underline(
                        &mut self.vt.screen,
                        &mut self.vt.protocol,
                        &event,
                        self.underline_pen.as_mut(),
                    )
                },
                Dispatch::Handled,
            );
        if sync_query {
            self.vt.sync_output.finish_query(&mut self.vt.protocol);
        }
        match dispatch {
            Dispatch::Handled(effects) => {
                for effect in effects {
                    // ScreenState has already applied row edits. Aether
                    // captures paint rows from the resulting grid.
                    match effect {
                        Effect::Reply(bytes) => {
                            replies.push(&bytes, "reply limit");
                        }
                        Effect::Bell => {
                            if let Some(window) = &mut self.window {
                                window.bell = true;
                            }
                        }
                        _ => {}
                    }
                }
            }
            Dispatch::Unhandled => match event {
                Event::Print(ch) => self.print(ch),
                Event::Ascii(bytes) => self.print_ascii(bytes),
                Event::Control(10..=12) => self.motion(VerticalMotion::LineFeed),
                Event::Control(0) | Event::Boundary => (),
                _ => {
                    let before = self.unsupported;
                    if !self.apply_profile_event(&event, now) {
                        self.unsupported();
                    }
                    if self.unsupported != before {
                        self.record_unsupported_event(&event);
                    }
                }
            },
        }
        if previous_alternate_keyboard.is_some_and(|old| self.alternate_scroll_profile() != old) {
            self.retire_mouse_reports();
        }
        if capture_end {
            // Exactly one bounded immutable frame per opted-in PTY poll.
            // Give any later screen mutations a separate revision.
            let next = self.revision.checked_add(1).ok_or("revision exhausted")?;
            self.completed_sync_frame = Some(self.frame()?);
            self.begin_revision(next);
            self.capture_sync_end = false;
        }
        Ok(())
    }

    pub fn resize(&mut self, rows: usize, cols: usize) -> Result<(), String> {
        self.resize_with(rows, cols, || Ok(()))
    }

    /// Retain primary rows moved above the viewport by a height reduction.
    pub fn enable_history_preserving_resize(&mut self) {
        self.preserve_primary_on_resize = true;
    }

    /// Preserve logical primary lines when an ordinary terminal changes width.
    /// Enable this before feeding output to distinguish written spaces from padding.
    pub fn enable_primary_reflow(&mut self) {
        self.reflow_primary_on_resize = true;
        self.enable_history_preserving_resize();
        self.track_printed_spaces();
    }

    /// Validate geometry and any new frame counters before invoking the PTY
    /// resize. Commit screen geometry only after that operation succeeds.
    pub fn resize_with(
        &mut self,
        rows: usize,
        cols: usize,
        resize_pty: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        dimensions(rows, cols, self.max_cols)?;
        if (rows, cols) == (self.vt.screen.grid.rows(), self.vt.screen.grid.cols()) {
            // The PTY may still need notification, but the screen has no
            // geometry change to commit.
            return resize_pty();
        }
        let layout = self.layout.checked_add(1).ok_or("layout exhausted")?;
        let revision = self.revision.checked_add(1).ok_or("revision exhausted")?;
        resize_pty()?;
        self.finish_grapheme();
        if let Some(blocks) = &mut self.command_blocks {
            blocks.resized();
        }
        self.begin_revision(revision);
        if self.reflow_primary_on_resize && self.reflow_primary(rows, cols) {
            self.layout = layout;
            self.retire_mouse_reports();
            return Ok(());
        }
        let transferred = if self.preserve_primary_on_resize && !self.vt.screen.use_alt_buffer {
            let grid = &self.vt.screen.grid;
            rows_to_archive_on_height_shrink(grid.rows(), rows, self.vt.screen.cursor_row, |row| {
                grid[row]
                    .iter()
                    .all(crate::TerminalCell::is_reflow_trimmable_blank)
                    && grid.row_annotations(row).is_none()
                    && grid.underline_color_runs(row).next().is_none()
            })
        } else {
            0
        };
        self.invalidate_clipped_sources(cols, transferred);
        if let Some(change) = self.vt.screen.resize_geometry(rows, cols, transferred, || {
            RowSources::allocate(&mut self.row_sources)
        }) {
            for line in change.transferred {
                self.append_history(line);
            }
        }
        self.layout = layout;
        self.retire_mouse_reports();
        Ok(())
    }
    pub fn frame(&self) -> Result<CellFrame, String> {
        let mut cache = self.last_frame.lock().map_err(|_| "frame cache poisoned")?;
        let FrameCache {
            frame: last,
            observed_revision,
            color_revision,
            history_extent,
            row_identity_revision,
            row_links,
            row_link_scratch,
        } = &mut *cache;
        if let Some(frame) = last
            .as_ref()
            .filter(|_| *observed_revision == self.revision)
        {
            return Ok(frame.clone());
        }
        let screen = if self.vt.screen.use_alt_buffer {
            Screen::Alternate
        } else {
            Screen::Primary
        };
        let grid = &self.vt.screen.grid;
        let capture_row = |index| Row {
            cells: Arc::from(&grid[index][..]),
            wrapped: grid.row_wrapped[index],
        };
        // Raw cells and decorations carry the last mutation revision. Wrap
        // bits may be edited by legacy callers, so compare those independently.
        let previous = last.as_ref().filter(|frame| {
            let stamp = frame.stamp();
            stamp.layout == self.layout && stamp.screen == screen
        });
        let cursor = Cursor {
            row: self.vt.screen.cursor_row,
            col: self.vt.screen.cursor_col,
            pending_wrap: self.vt.screen.pending_wrap,
            visible: self.vt.protocol.modes().contains(&25),
            shape: self.vt.screen.cursor_shape,
        };
        let extent = self.history_extent();
        let identity = grid.identity_revision();
        let last_observation = *observed_revision;
        let may_have_changed =
            |row| previous.is_none() || grid.row_changed_after(row, last_observation);
        let unchanged_paint = previous.filter(|frame| {
            *history_extent == extent
                && *row_identity_revision == identity
                && frame.cols() == grid.cols()
                && frame.rows().len() == grid.rows()
                && !grid.row_changed_after(cursor.row, last_observation)
                && grid.captured_rows_unchanged_after(last_observation)
                && frame
                    .rows()
                    .iter()
                    .enumerate()
                    .all(|(row, old)| old.wrapped == grid.row_wrapped[row])
        });
        if let Some(previous) = unchanged_paint.filter(|frame| {
            frame.cursor() == cursor
                && *color_revision == self.color_revision
                && *history_extent == extent
                && *row_identity_revision == identity
        }) {
            // Parser carry, queries, and input modes may advance independently
            // of paint. Keep the acknowledgement base when the view is unchanged.
            debug_assert!(previous.rows().iter().enumerate().all(|(row, old)| {
                old.cells.as_ref() == &grid[row][..]
                    && grid
                        .underline_color_runs(row)
                        .eq(previous.underline_color_runs(row).iter().copied())
                    && grid.row_annotations(row) == previous.row_annotations(row)
            }));
            *observed_revision = self.revision;
            return Ok(previous.clone());
        }
        let rows = if let Some(previous) = unchanged_paint {
            previous.shared_rows()
        } else if let Some(previous) = previous {
            let mut changed = None;
            for (index, row) in previous.rows().iter().enumerate() {
                let cells_changed = if may_have_changed(index) {
                    row.cells.as_ref() != &grid[index][..]
                } else {
                    debug_assert_eq!(row.cells.as_ref(), &grid[index][..]);
                    false
                };
                let wrapped = grid.row_wrapped[index];
                if row.wrapped != wrapped || cells_changed {
                    changed.get_or_insert_with(|| previous.rows().to_vec())[index] = Row {
                        cells: if cells_changed {
                            Arc::from(&grid[index][..])
                        } else {
                            row.cells.clone()
                        },
                        wrapped,
                    };
                }
            }
            changed.map_or_else(|| previous.shared_rows(), Arc::from)
        } else {
            (0..grid.rows()).map(capture_row).collect::<Vec<_>>().into()
        };
        let underline_colors = if self.underline_pen.is_some() {
            if let Some(previous) = unchanged_paint {
                previous.shared_underline_rows()
            } else {
                let mut changed = None;
                for row in 0..grid.rows() {
                    let before = previous.map_or(&[][..], |frame| frame.underline_color_runs(row));
                    if !may_have_changed(row) {
                        debug_assert!(grid.underline_color_runs(row).eq(before.iter().copied()));
                        continue;
                    }
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
            }
        } else {
            None
        };
        let annotations = if self.graphemes.is_some() || self.next_inline_image_id != 0 {
            if let Some(previous) = unchanged_paint {
                previous.shared_annotations()
            } else {
                let mut changed = None;
                for row in 0..grid.rows() {
                    let current = grid.row_annotations(row);
                    let before = previous.and_then(|frame| frame.row_annotations(row));
                    if !may_have_changed(row) {
                        debug_assert_eq!(current, before);
                        continue;
                    }
                    if current != before {
                        let rows = changed.get_or_insert_with(|| {
                            previous
                                .and_then(CellFrame::shared_annotations)
                                .map_or_else(|| vec![None; grid.rows()], |rows| rows.to_vec())
                        });
                        rows[row] = current.cloned();
                    }
                }
                match changed {
                    Some(rows) => rows.iter().any(Option::is_some).then(|| rows.into()),
                    None => previous.and_then(CellFrame::shared_annotations),
                }
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
            cursor,
            Limits {
                max_rows: MAX_ROWS,
                max_cols: self.max_cols,
                max_cells: MAX_ROWS * self.max_cols,
            },
            underline_colors,
            previous,
        )
        .map_err(|e| format!("{e:?}"))?
        .with_annotations(annotations);
        let hyperlinks = self
            .hyperlinks
            .as_ref()
            .filter(|(_, table)| !table.is_empty())
            .and_then(|(_, table)| {
                visible_hyperlinks(table, row_links, row_link_scratch, previous, frame.rows())
            });
        let frame = frame
            .with_hyperlinks(hyperlinks)
            .with_paint_colors(self.colors.clone());
        *last = Some(frame.clone());
        *observed_revision = self.revision;
        *color_revision = self.color_revision;
        *history_extent = extent;
        *row_identity_revision = identity;
        Ok(frame)
    }
    pub fn visible_text(&self) -> String {
        (0..self.vt.screen.grid.rows())
            .map(|r| {
                self.vt.screen.grid[r].iter().enumerate().fold(
                    String::new(),
                    |mut line, (col, cell)| {
                        if let Some(cluster) = self.vt.screen.grid.cluster_text(r, col) {
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

#[cfg(test)]
mod offset_specialization_tests {
    use super::*;
    use crate::grid::{TerminalCell, TerminalGrid};

    #[test]
    fn offset_only_matches_materialized_boundaries_across_copy_metadata() {
        for seed in 0..24 {
            let mut grid = TerminalGrid::new(4, 12);
            grid.track_printed_spaces = true;
            for row in 0..4 {
                for col in 0..12 {
                    let cell = grid.get_mut(row, col);
                    cell.character = ['a', ' ', '界', '🦀'][(seed + row + col) % 4];
                    cell.flags.set_wide(col == 4);
                    cell.flags.set_wide_continuation(col == 5);
                }
                grid.mark_tab_spaces(row, 0..12);
            }
            let rows: Vec<_> = (0..4)
                .map(|row| {
                    let mut annotations = RowAnnotations::default();
                    annotations.set(7, Arc::from("e\u{301}"));
                    annotations.set(11, Arc::from(if seed % 2 == 0 { "x  " } else { " " }));
                    HistoryCopyRow {
                        row: Row {
                            cells: Arc::from(&grid[row][..]),
                            wrapped: (seed + row) % 2 == 0,
                        },
                        underline_colors: None,
                        annotations: Some(Arc::new(annotations)),
                        synthetic_gap: (row % 2 == 0).then_some(10),
                    }
                })
                .collect();
            for require_literal in [false, true] {
                let mut expected = Vec::new();
                let mut actual = Vec::new();
                let materialized = extract_selection::<true>(
                    &rows,
                    (0, 0),
                    (4, 0),
                    require_literal,
                    Some(&mut expected),
                );
                let counted = extract_selection::<false>(
                    &rows,
                    (0, 0),
                    (4, 0),
                    require_literal,
                    Some(&mut actual),
                );
                assert_eq!(
                    materialized.as_ref().map(|_| ()),
                    counted.as_ref().map(|_| ())
                );
                assert_eq!(actual, expected);
            }
        }
    }

    #[test]
    fn offset_only_preserves_exact_copy_byte_limit() {
        for count in [MAX_COPY_BYTES / 4, MAX_COPY_BYTES / 4 + 1] {
            let rows = [HistoryCopyRow {
                row: Row {
                    cells: vec![
                        TerminalCell {
                            character: '🦀',
                            ..TerminalCell::default()
                        };
                        count
                    ]
                    .into(),
                    wrapped: true,
                },
                underline_colors: None,
                annotations: None,
                synthetic_gap: None,
            }];
            let expected = extract_selection::<true>(&rows, (0, 0), (1, 0), false, None);
            let actual = extract_selection::<false>(&rows, (0, 0), (1, 0), false, None);
            assert_eq!(expected.as_ref().map(|_| ()), actual.as_ref().map(|_| ()));
            assert_eq!(actual.is_ok(), count == MAX_COPY_BYTES / 4);
        }
    }
}

#[cfg(test)]
mod frame_certificate_regressions {
    use super::*;
    use crate::Color;
    fn advance(terminal: &mut Terminal) {
        terminal.revision += 1;
        terminal
            .vt
            .screen
            .grid
            .begin_capture_revision(terminal.revision);
        terminal
            .vt
            .screen
            .alt_grid
            .begin_capture_revision(terminal.revision);
    }
    #[test]
    fn legacy_wrap_row_flat_and_metadata_mutations_keep_exact_frames() {
        let mut t = Terminal::new(3, 8).unwrap();
        t.set_color_defaults(TerminalDefaultColors::default());
        t.enable_underline_colors();
        t.enable_grapheme_clusters();
        t.enable_hyperlinks(|_, _| true);
        t.feed(
            "\x1b]8;;https://example.test/a\x1b\\X\u{301}\x1b]8;;\x1b\\\x1b[3;1HX\x1b[1;1H"
                .as_bytes(),
        )
        .unwrap();
        let initial = t.frame().unwrap();
        t.feed(b"\x1b[2;2H").unwrap();
        let cursor = t.frame().unwrap();
        assert!(Arc::ptr_eq(&initial.shared_rows(), &cursor.shared_rows()));
        advance(&mut t);
        t.vt.screen.grid.row_wrapped[1] = true;
        assert!(t.frame().unwrap().rows()[1].wrapped);
        advance(&mut t);
        t.vt.screen.grid.get_mut(2, 0).character = 'Y';
        assert_eq!(t.frame().unwrap().rows()[2].cells[0].character, 'Y');
        advance(&mut t);
        t.vt.screen.grid[2][1].character = 'Z';
        assert_eq!(t.frame().unwrap().rows()[2].cells[1].character, 'Z');
        let before = t.frame().unwrap();
        advance(&mut t);
        t.vt.screen
            .grid
            .set_underline_color(2, 0..1, Color::Rgb(1, 2, 3));
        let underline = t.frame().unwrap();
        assert_eq!(before.rows(), underline.rows());
        assert_eq!(underline.underline_color(2, 0), Some(Color::Rgb(1, 2, 3)));
        advance(&mut t);
        let _ = t.vt.screen.grid.set_cluster_text(2, 0, "Y\u{302}");
        let annotated = t.frame().unwrap();
        assert_eq!(annotated.cluster_text(2, 0), Some("Y\u{302}"));
        let colors = TerminalDefaultColors {
            foreground: (11, 22, 33),
            ..TerminalDefaultColors::default()
        };
        t.set_color_defaults(colors);
        let recolored = t.frame().unwrap();
        assert!(Arc::ptr_eq(
            &annotated.shared_rows(),
            &recolored.shared_rows()
        ));
        assert_eq!(
            recolored.paint_colors().unwrap().effective_foreground(),
            (11, 22, 33)
        );
        assert_eq!(recolored.cluster_text(2, 0), Some("Y\u{302}"));
        assert_eq!(recolored.underline_color(2, 0), Some(Color::Rgb(1, 2, 3)));
        assert_eq!(
            recolored.hyperlink_target(HyperlinkId::from_raw(1)),
            Some("https://example.test/a")
        );
        advance(&mut t);
        t.vt.screen.grid.cells_mut()[7].character = 'W';
        let flat = t.frame().unwrap();
        assert_eq!(flat.rows()[0].cells[7].character, 'W');
        assert_eq!(flat.cluster_text(0, 0), None);
        assert_eq!(flat.cluster_text(2, 0), None);
        assert!(flat.underline_color_runs(2).is_empty());
        advance(&mut t);
        t.vt.screen.grid.row_wrapped[1] = false;
        assert!(!t.frame().unwrap().rows()[1].wrapped);
    }
}
