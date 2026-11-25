#[cfg(target_os = "linux")]
use futures_util::StreamExt;
use gtk4::{gdk, glib, graphene, pango, prelude::*};
use jterm_engine::{
    colors::{Rgb, TerminalDefaultColors},
    frame::{CellFrame, Screen, Stamp},
    grid::{Color, UnderlineStyle},
    history_reflow::HistoryAnchor,
    history_search::{HistorySearch, HistorySearchProgress},
    hyperlink::HyperlinkPolicy,
    mouse::MouseInput,
    terminal::{Terminal, TranscriptAnchor, WindowUpdates},
};
use jterm_runtime::exit_status::PtyExit;
use jterm_runtime::terminal::{
    Pty, TerminalHandle, TerminalOutput, TerminalProfile, TerminalUpdate,
};
use std::{
    cell::{Cell, OnceCell, RefCell},
    io,
    ops::Range,
    rc::{Rc, Weak},
};

use crate::history_search::HistorySearchView;
use crate::inline_images::{InlineImageRequest, InlineImages, PendingInlineImage};
#[cfg(target_os = "linux")]
use crate::runtime_shell::RuntimeShell;
use crate::text_link::{connect_links, link_modifier};
use crate::view::TerminalView;
use crate::viewport::{
    HistoricalWindow, HistoryRail, ScrollTarget, ViewportFrame, ViewportRequest,
};
use crate::{find, FindFrame, FindMatch, FindRegion, FindSnapshot};

pub(crate) const INITIAL_ROWS: usize = 24;
pub(crate) const INITIAL_COLUMNS: usize = 80;

enum Backend<P> {
    Host {
        terminal: TerminalHandle<P>,
        local_reader: Option<TerminalOutput<P>>,
    },
    #[cfg(target_os = "linux")]
    Runtime(Rc<RuntimeShell>),
}

#[derive(Clone)]
pub struct Theme {
    pub font: pango::FontDescription,
    pub foreground: gdk::RGBA,
    pub background: gdk::RGBA,
    pub palette: [gdk::RGBA; 16],
    pub cursor: Option<CursorColors>,
}

#[derive(Clone, Copy)]
pub struct CursorColors {
    pub foreground: gdk::RGBA,
    pub background: gdk::RGBA,
}

impl Theme {
    fn terminal_colors(&self) -> TerminalDefaultColors {
        fn rgb(color: gdk::RGBA) -> Rgb {
            let component = |value: f32| (value * 255.0).round() as u8;
            (
                component(color.red()),
                component(color.green()),
                component(color.blue()),
            )
        }
        TerminalDefaultColors {
            foreground: rgb(self.foreground),
            background: rgb(self.background),
            cursor: rgb(self
                .cursor
                .map_or(self.foreground, |cursor| cursor.background)),
            ansi: self.palette.map(rgb),
        }
    }
}

pub type HyperlinkOpener = Box<dyn Fn(&str)>;
/// Return a host-admitted URL range containing the indicated UTF-8 offset.
pub type PlainLinkFinder = fn(&str, usize) -> Option<std::ops::Range<usize>>;

pub struct LinkActions {
    pub admit: HyperlinkPolicy,
    pub find_plain: Option<PlainLinkFinder>,
    pub attach_opener: fn(&gtk4::Widget) -> HyperlinkOpener,
}

#[derive(Clone)]
pub enum PaneEvent {
    Output,
    TitleChanged(String),
    Bell,
    Exited(PtyExit),
}

type OnClose = Box<dyn Fn(&gtk4::Widget)>;
type OnEvent = Box<dyn Fn(PaneEvent)>;

#[derive(Clone, Copy)]
enum InputStyle {
    Terminal,
    HostedAttachment { inline_history: bool },
    InlineTask { inline_history: bool },
}

#[derive(Clone, Copy)]
enum GridSize {
    Allocation,
    Application { cols: u16, rows: u16 },
}

impl GridSize {
    fn dimensions(self) -> (usize, usize) {
        match self {
            Self::Allocation => (INITIAL_ROWS, INITIAL_COLUMNS),
            Self::Application { cols, rows } => (usize::from(rows), usize::from(cols)),
        }
    }

    fn automatic(self) -> bool {
        matches!(self, Self::Allocation)
    }
}

/// Live terminal for an application that already owns the PTY reader and
/// decides when to resize the full grid. Its widget can be clipped as the
/// application presents the same terminal at a prompt, running or fullscreen.
pub struct EmbeddedTerminal<P: Pty> {
    pane: Rc<TerminalPane<P>>,
    source_owner: Rc<()>,
}

/// Identity of the retained terminal text when selected by a document view.
#[derive(Clone)]
pub struct RetainedTextSource {
    owner: Weak<()>,
    stamp: Stamp,
    history: Range<u64>,
}

impl<P: Pty> EmbeddedTerminal<P> {
    /// Attach the presentation to the runtime's existing terminal grid.
    pub fn attach(
        terminal: TerminalHandle<P>,
        theme: Theme,
        status_text: fn(&str, usize) -> String,
        links: LinkActions,
    ) -> Result<(gtk4::Widget, Rc<Self>), String> {
        let (cols, rows) = terminal.grid_size()?;
        let grid_size = GridSize::Application { cols, rows };
        let (widget, pane) = TerminalPane::attach(
            Backend::Host {
                terminal,
                local_reader: None,
            },
            theme,
            status_text,
            links,
            None,
            InputStyle::Terminal,
            grid_size,
        )?;
        Ok((
            widget,
            Rc::new(Self {
                pane,
                source_owner: Rc::new(()),
            }),
        ))
    }

    pub fn pane(&self) -> &Rc<TerminalPane<P>> {
        &self.pane
    }

    pub fn scrollback_adjustment(&self) -> &gtk4::Adjustment {
        &self.pane.scrollback
    }

    /// The viewport-relative cursor row of the primary grid currently painted.
    pub fn painted_primary_cursor_row(&self) -> Option<usize> {
        Some(self.painted_primary_frame()?.cursor().row)
    }

    /// Test an overlay footprint in the given widget's coordinates, with a
    /// one-cell guard. A pending paint, scrollback or the alternate screen
    /// cannot authorize placing anything over the live grid.
    pub fn painted_primary_rect_is_blank(
        &self,
        source: &impl IsA<gtk4::Widget>,
        rect: graphene::Rect,
    ) -> bool {
        let view = &self.pane.view;
        let screen = &view.widget;
        let Some(origin) = source.compute_point(screen, &graphene::Point::new(rect.x(), rect.y()))
        else {
            return false;
        };
        let (x, y, width, height) = (origin.x(), origin.y(), rect.width(), rect.height());
        let right = x + width;
        let bottom = y + height;
        if !x.is_finite()
            || !y.is_finite()
            || !width.is_finite()
            || !height.is_finite()
            || x < 0.0
            || y < 0.0
            || width <= 0.0
            || height <= 0.0
            || right > screen.width() as f32
            || bottom > screen.height() as f32
        {
            return false;
        }
        let Some(frame) = self.painted_primary_frame() else {
            return false;
        };
        let (_, _, cell_width, cell_height) = view.cell_metrics();
        let guarded = graphene::Rect::new(
            x - cell_width as f32,
            y - cell_height as f32,
            width + 2.0 * cell_width as f32,
            height + 2.0 * cell_height as f32,
        );
        if self.pane.inline_images.borrow().overlaps(screen, guarded) {
            return false;
        }
        let coords = gtk4::TextWindowType::Widget;
        let (left, top) = screen.window_to_buffer_coords(coords, x.floor() as i32, y.floor() as i32);
        let (right, bottom) =
            screen.window_to_buffer_coords(coords, right.ceil() as i32, bottom.ceil() as i32);
        let first_row = (screen.line_at_y(top).0.line() as usize).saturating_sub(1);
        let last_row = (screen.line_at_y(bottom).0.line() as usize + 1).min(frame.rows().len() - 1);
        if first_row > last_row {
            return false;
        }
        let cursor = frame.cursor();
        let buffer = screen.buffer();
        (first_row..=last_row).all(|row| {
            let Some(line) = buffer.iter_at_line(row as i32) else {
                return false;
            };
            // Use each painted row's own layout. A fallback font or a wide
            // glyph can move its pixel columns relative to the next row.
            let (line_top, line_height) = screen.line_yrange(&line);
            let last_col = frame.cols() - 1;
            let column_at = |x| {
                view.text_cell_at(x, line_top + line_height / 2)
                    .filter(|&(_, found_row)| found_row == row)
                    .map(|(col, _)| col.min(last_col))
                    .unwrap_or_else(|| if x <= screen.left_margin() { 0 } else { last_col })
            };
            let (left, right) = (column_at(left), column_at(right));
            if left > right {
                return false;
            }
            let columns = left.saturating_sub(1)..=(right + 1).min(last_col);
            if cursor.visible && cursor.row == row && columns.contains(&cursor.col) {
                return false;
            }
            columns.into_iter().all(|col| {
                let cell = &frame.rows()[row].cells[col];
                cell.character.is_whitespace()
                    && !cell.flags.wide_continuation()
                    && cell.background == Color::Default
                    && !cell.flags.inverse()
                    && cell.flags.underline() == UnderlineStyle::None
                    && !cell.flags.strikethrough()
                    && frame
                        .cluster_text(row, col)
                        .is_none_or(|cluster| cluster.chars().all(char::is_whitespace))
            })
        })
    }

    fn painted_primary_frame(&self) -> Option<CellFrame> {
        let (stamp, history_top) = self.pane.painted_source.get()?;
        if stamp.screen != Screen::Primary
            || history_top.is_some()
            || self.pane.dirty.get()
            || !self.pane.view.widget.is_mapped()
        {
            return None;
        }
        let frame = self.pane.view.painted_frame()?;
        (frame.stamp() == stamp).then_some(frame)
    }

    pub fn fit_grid(&self, width: i32, height: i32) -> (u16, u16) {
        let (rows, cols) = self.pane.view.dimensions(width, height);
        (cols as u16, rows as u16)
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<(), String> {
        if self.pane.exited.get() {
            return Err("terminal session is closed".into());
        }
        self.pane.resize_grid(usize::from(rows), usize::from(cols))
    }

    pub fn enable_transcript_sources(&self) {
        if let Backend::Host { terminal, .. } = &self.pane.backend {
            terminal.track_transcript_rows();
        }
    }

    /// Select the entire published terminal surface, including its history.
    pub fn retained_text_source(&self) -> Option<RetainedTextSource> {
        let (stamp, _) = self.pane.painted_source.get()?;
        let source = RetainedTextSource {
            owner: Rc::downgrade(&self.source_owner),
            stamp,
            history: self.pane.history_rail.borrow().extent.clone(),
        };
        self.inspect(|terminal| {
            self.retains_text_source(terminal, &source)
                .then_some(source)
        })
    }

    /// Copy a whole-surface selection only while its screen and history agree
    /// with the selected publication. The caller provides a bounded sink.
    pub fn write_retained_text(
        &self,
        source: &RetainedTextSource,
        output: &mut impl io::Write,
    ) -> io::Result<()> {
        self.inspect(|terminal| {
            if !self.retains_text_source(terminal, source) {
                return Err(io::Error::other("terminal text selection expired"));
            }
            terminal.write_retained_text(output)
        })
    }

    fn retains_text_source(&self, terminal: &Terminal, source: &RetainedTextSource) -> bool {
        source.owner.ptr_eq(&Rc::downgrade(&self.source_owner))
            && self.pane.screen().is_mapped()
            && self
                .pane
                .painted_source
                .get()
                .is_some_and(|(stamp, _)| stamp == source.stamp)
            && !terminal.publication_held()
            && terminal.history_extent() == source.history
            && terminal
                .frame()
                .is_ok_and(|frame| frame.stamp() == source.stamp)
    }

    pub fn find_snapshot(
        &self,
        region: FindRegion,
        byte_limit: usize,
        expired: &mut dyn FnMut() -> bool,
    ) -> Option<FindSnapshot> {
        let top = self.pane.scroll_top.get();
        self.inspect(|terminal| {
            find::capture(
                terminal,
                Rc::downgrade(&self.source_owner),
                region,
                top,
                byte_limit,
                expired,
            )
        })
    }

    pub fn find_is_current(&self, frame: &FindFrame) -> bool {
        let source = &frame.0;
        if !source.owner.ptr_eq(&Rc::downgrade(&self.source_owner)) {
            return false;
        }
        self.inspect(|terminal| {
            !terminal.publication_held()
                && terminal.history_extent() == (source.history_start..source.history_end)
                && terminal
                    .frame()
                    .is_ok_and(|actual| actual.stamp() == source.stamp)
        })
    }

    /// Reveal the exact occurrence without changing GTK focus or starting a
    /// text selection, so a search does not pause the PTY or own PRIMARY.
    pub fn show_find(&self, frame: &FindFrame, hits: Rc<[FindMatch]>, current: usize) -> bool {
        let Some(found) = hits.get(current) else {
            return false;
        };
        if !self.find_is_current(frame) || !found.frame.same_snapshot(frame) {
            return false;
        }
        self.pane.paint();
        let is_visible = || {
            self.pane.painted_find_top(frame).is_some_and(|top| {
                (top..top + self.pane.dimensions.get().0).contains(&found.start.row)
            })
        };
        if !is_visible() {
            let top = if found.start.row < frame.0.history_rows {
                let Some(anchor) = self
                    .inspect(|terminal| terminal.history_layout().ok()?.anchor_at(found.start.row))
                else {
                    return false;
                };
                Some(ScrollTarget::Source(anchor))
            } else {
                None
            };
            self.pane.set_scroll_top(top);
            self.pane.paint();
        }
        if !self.find_is_current(frame) || !is_visible() {
            return false;
        }
        self.highlight_find(frame, hits, Some(current))
    }

    pub fn highlight_find(
        &self,
        frame: &FindFrame,
        hits: Rc<[FindMatch]>,
        current: Option<usize>,
    ) -> bool {
        if !self.find_is_current(frame) || hits.iter().any(|hit| !hit.frame.same_snapshot(frame)) {
            self.clear_find();
            return false;
        }
        self.pane
            .find_highlights
            .replace(Some(find::FindHighlights {
                frame: frame.clone(),
                hits,
                current,
            }));
        self.pane.paint_find();
        true
    }

    pub fn clear_find(&self) {
        self.pane.find_highlights.take();
        self.pane.view.clear_find();
    }

    pub(crate) fn reveal_transcript_source(&self, point: HistoryAnchor) -> bool {
        if self.pane.view.widget.buffer().has_selection() {
            return false;
        }
        let history = self.inspect(Terminal::history_extent);
        if point.row < history.start {
            return false;
        }
        self.pane
            .set_scroll_top((point.row < history.end).then_some(ScrollTarget::Source(point)));
        self.pane.paint();
        self.pane.view.widget.grab_focus();
        true
    }

    pub(crate) fn painted_transcript_source(&self, x: f64, y: f64) -> Option<TranscriptAnchor> {
        let (stamp, top) = self.pane.painted_source.get()?;
        if stamp.screen != Screen::Primary {
            return None;
        }
        let (column, row) = self
            .pane
            .view
            .cell_at((x, y), self.pane.dimensions.get(), false)?;
        self.inspect(|terminal| {
            if terminal.frame().ok()?.stamp() != stamp {
                return None;
            }
            let rail = self.pane.history_rail.borrow();
            let point = if let (Some(top), Some(layout)) = (top, rail.layout.as_ref()) {
                let index = top.checked_add(row)?;
                if index < layout.len() {
                    terminal.projected_history_source(layout, index, column)?
                } else {
                    HistoryAnchor {
                        row: rail.extent.end + (index - layout.len()) as u64,
                        column: column as u16,
                    }
                }
            } else {
                HistoryAnchor {
                    row: rail.extent.end + row as u64,
                    column: column as u16,
                }
            };
            terminal.transcript_source_at(point)
        })
    }

    pub(crate) fn when_transcript_painted(&self, observer: impl Fn() + 'static) -> bool {
        self.pane.transcript_painted.set(Box::new(observer)).is_ok()
    }

    pub(crate) fn with_painted_transcript<R>(
        &self,
        inspect: impl FnOnce(&Terminal, PaintedTranscript<'_>) -> R,
    ) -> Option<R> {
        let (stamp, top) = self.pane.painted_source.get()?;
        if stamp.screen != Screen::Primary {
            return None;
        }
        self.inspect(|terminal| {
            if terminal.frame().ok()?.stamp() != stamp {
                return None;
            }
            let rail = self.pane.history_rail.borrow();
            Some(inspect(
                terminal,
                PaintedTranscript {
                    top: top.unwrap_or_else(|| rail.len()),
                    rows: self.pane.dimensions.get().0,
                    rail: &rail,
                    view: &self.pane.view,
                },
            ))
        })
    }

    /// Current physical grid coordinates for a graphics-protocol transfer.
    pub fn inline_image_cursor(&self) -> Option<(i64, i64)> {
        self.inspect(|terminal| {
            let cursor = terminal.frame().ok()?.cursor();
            Some((cursor.col.try_into().ok()?, cursor.row.try_into().ok()?))
        })
    }

    /// Check a decoded inline image against the current cursor, grid and
    /// render budget. The adapter can queue its reply before admitting it.
    pub fn prepare_inline_image(
        &self,
        texture: gdk::Texture,
        encoded_source_bytes: usize,
        request: InlineImageRequest,
    ) -> Option<PendingInlineImage> {
        if self.pane.exited.get() {
            return None;
        }
        let (_, _, width, height) = self.pane.view.cell_metrics();
        self.inspect(|terminal| {
            self.pane.inline_images.borrow().prepare(
                terminal,
                texture,
                encoded_source_bytes,
                request,
                (width, height),
            )
        })
    }

    pub fn admit_inline_image(&self, pending: PendingInlineImage) -> bool {
        let Backend::Host { terminal, .. } = &self.pane.backend else {
            return false;
        };
        if self.pane.exited.get()
            || !self
                .pane
                .inline_images
                .borrow_mut()
                .admit(pending, |reservation| {
                    terminal.place_inline_image(reservation)
                })
        {
            return false;
        }
        self.pane.dirty.set(true);
        true
    }

    pub fn inspect<T>(&self, read: impl FnOnce(&Terminal) -> T) -> T {
        let Backend::Host { terminal, .. } = &self.pane.backend else {
            unreachable!("an embedded terminal has its own shared engine")
        };
        terminal.inspect(read)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct PointerMotion {
    column: usize,
    row: usize,
    button: Option<u8>,
    modifiers: u8,
}

/// Coordinates for the same primary rows installed in the GTK text buffer.
pub(crate) struct PaintedTranscript<'a> {
    pub(crate) top: usize,
    pub(crate) rows: usize,
    rail: &'a HistoryRail,
    view: &'a TerminalView,
}

impl PaintedTranscript<'_> {
    pub(crate) fn locate(&self, source: HistoryAnchor) -> Option<(usize, bool)> {
        if source.row < self.rail.extent.start {
            return None;
        }
        if self.rail.extent.contains(&source.row) {
            if let Some(layout) = &self.rail.layout {
                let row = layout.row_at(source);
                return Some((row, layout.anchor_at(row) == Some(source)));
            }
            return Some((
                (source.row - self.rail.extent.start) as usize,
                source.column == 0,
            ));
        }
        let live_row = usize::try_from(source.row - self.rail.extent.end).ok()?;
        (live_row < self.rows).then_some((self.rail.len() + live_row, source.column == 0))
    }

    pub(crate) fn blank_tail(&self, row: usize) -> Option<usize> {
        self.view.blank_tail(row)
    }
}

pub struct TerminalPane<P: Pty> {
    backend: Backend<P>,
    status_text: fn(&str, usize) -> String,
    view: TerminalView,
    inline_images: RefCell<InlineImages>,
    plain_links: Option<PlainLinkFinder>,
    status: gtk4::Label,
    event_handler: OnceCell<OnEvent>,
    viewport: glib::WeakRef<gtk4::ScrolledWindow>,
    dimensions: Cell<(usize, usize)>,
    scrollback: gtk4::Adjustment,
    scroll_top: Cell<Option<ScrollTarget>>,
    history_rail: RefCell<HistoryRail>,
    painted_source: Cell<Option<(Stamp, Option<usize>)>>,
    find_highlights: RefCell<Option<find::FindHighlights>>,
    transcript_painted: OnceCell<Box<dyn Fn()>>,
    history_search: OnceCell<Rc<HistorySearchView>>,
    scroll_update: Cell<bool>,
    wheel_rows: Cell<f64>,
    reported_buttons: Cell<u8>,
    last_pointer_motion: Cell<Option<PointerMotion>>,
    last_pointer_cell: Cell<Option<(usize, usize)>>,
    pointer_wheel: Cell<f64>,
    painted_history: Cell<Option<HistoricalWindow>>,
    dirty: Cell<bool>,
    exited: Cell<bool>,
    input_focused: Cell<bool>,
    input_method: gtk4::IMMulticontext,
    keys: gtk4::EventControllerKey,
    input_method_delegated: Cell<bool>,
    input_style: InputStyle,
    input_cursor: Cell<Option<gdk::Rectangle>>,
    paste_revision: Cell<u64>,
    paste_reader: RefCell<Option<glib::JoinHandle<()>>>,
    input_revision: Cell<u64>,
}

impl<P: Pty> TerminalPane<P> {
    pub fn new(
        pty: P,
        theme: Theme,
        status_text: fn(&str, usize) -> String,
        links: LinkActions,
        close: impl Fn(&gtk4::Widget) + 'static,
    ) -> Result<(gtk4::Widget, Rc<Self>), String> {
        let grid_size = GridSize::Allocation;
        let output = TerminalOutput::new(
            pty,
            INITIAL_COLUMNS as u16,
            INITIAL_ROWS as u16,
            TerminalProfile::Standalone,
            links.admit,
        )?;
        Self::attach(
            Backend::Host {
                terminal: output.handle(),
                local_reader: Some(output),
            },
            theme,
            status_text,
            links,
            Some(Box::new(close)),
            InputStyle::Terminal,
            grid_size,
        )
    }

    #[cfg(target_os = "linux")]
    pub fn with_runtime(
        runtime: Rc<RuntimeShell>,
        theme: Theme,
        status_text: fn(&str, usize) -> String,
        links: LinkActions,
        close: impl Fn(&gtk4::Widget) + 'static,
    ) -> Result<(gtk4::Widget, Rc<Self>), String> {
        runtime.enable_primary_reflow()?;
        Self::attach(
            Backend::Runtime(runtime),
            theme,
            status_text,
            links,
            Some(Box::new(close)),
            InputStyle::Terminal,
            GridSize::Allocation,
        )
    }

    #[cfg(target_os = "linux")]
    pub fn with_runtime_embedded(
        runtime: Rc<RuntimeShell>,
        theme: Theme,
        status_text: fn(&str, usize) -> String,
        links: LinkActions,
    ) -> Result<(gtk4::Widget, Rc<Self>), String> {
        Self::attach(
            Backend::Runtime(runtime),
            theme,
            status_text,
            links,
            None,
            InputStyle::Terminal,
            GridSize::Allocation,
        )
    }

    /// Display an attachment to a PTY owned by the Session Host.
    #[cfg(target_os = "linux")]
    pub fn with_hosted_attachment(
        runtime: Rc<RuntimeShell>,
        theme: Theme,
        status_text: fn(&str, usize) -> String,
        links: LinkActions,
    ) -> Result<(gtk4::Widget, Rc<Self>), String> {
        Self::attach(
            Backend::Runtime(runtime),
            theme,
            status_text,
            links,
            None,
            InputStyle::HostedAttachment {
                inline_history: std::env::var_os("JTERM_AGENT_SCROLLBACK")
                    .is_none_or(|value| value != "0"),
            },
            GridSize::Allocation,
        )
    }

    #[cfg(target_os = "linux")]
    pub fn with_inline_task(
        runtime: Rc<RuntimeShell>,
        theme: Theme,
        status_text: fn(&str, usize) -> String,
        links: LinkActions,
    ) -> Result<(gtk4::Widget, Rc<Self>), String> {
        Self::attach(
            Backend::Runtime(runtime),
            theme,
            status_text,
            links,
            None,
            InputStyle::InlineTask {
                inline_history: std::env::var_os("JTERM_AGENT_SCROLLBACK")
                    .is_none_or(|value| value != "0"),
            },
            GridSize::Allocation,
        )
    }

    fn attach(
        backend: Backend<P>,
        theme: Theme,
        status_text: fn(&str, usize) -> String,
        links: LinkActions,
        close: Option<OnClose>,
        input_style: InputStyle,
        grid_size: GridSize,
    ) -> Result<(gtk4::Widget, Rc<Self>), String> {
        let (rows, columns) = grid_size.dimensions();
        let automatic_grid = grid_size.automatic();
        let colors = theme.terminal_colors();
        let view = TerminalView::new(theme);
        let scroll = gtk4::ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Automatic)
            .vscrollbar_policy(gtk4::PolicyType::External)
            .hexpand(true)
            .vexpand(true)
            .child(&view.widget)
            .build();
        // Value and page size count physical viewport rows. Keep the rail's
        // allocation stable so newly arriving history cannot resize the PTY.
        let scrollback =
            gtk4::Adjustment::new(0.0, 0.0, rows as f64, 1.0, rows as f64, rows as f64);
        let scrollbar = gtk4::Scrollbar::new(gtk4::Orientation::Vertical, Some(&scrollback));
        scrollbar.update_property(&[gtk4::accessible::Property::Label("Terminal scrollback")]);
        let viewport = gtk4::Overlay::builder()
            .hexpand(true)
            .vexpand(true)
            .child(&scroll)
            .build();
        let inline_images = InlineImages::attach(&viewport);
        view.attach_preedit(&viewport);
        let body = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        body.set_vexpand(true);
        body.append(&viewport);
        if automatic_grid {
            body.append(&scrollbar);
        }
        let status = gtk4::Label::new(None);
        status.set_xalign(0.0);
        status.set_hexpand(true);
        status.set_visible(false);
        let column = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        if let Some(close) = close {
            let close_button = gtk4::Button::from_icon_name("window-close-symbolic");
            close_button.set_tooltip_text(Some("Close shared terminal tab"));
            close_button.update_property(&[gtk4::accessible::Property::Label(
                "Close shared terminal tab",
            )]);
            let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
            header.append(&gtk4::Label::new(Some("Shared terminal")));
            header.append(&status);
            close_button.set_halign(gtk4::Align::End);
            header.append(&close_button);
            column.append(&header);
            let weak_page = column.downgrade();
            close_button.connect_clicked(move |_| {
                if let Some(page) = weak_page.upgrade() {
                    close(page.upcast_ref());
                }
            });
        } else {
            column.append(&status);
        }
        column.append(&body);
        let page = column.upcast::<gtk4::Widget>();

        let session = Rc::new(Self {
            backend,
            status_text,
            view,
            inline_images: RefCell::new(inline_images),
            plain_links: links.find_plain,
            status,
            event_handler: OnceCell::new(),
            viewport: scroll.downgrade(),
            dimensions: Cell::new((rows, columns)),
            scrollback,
            scroll_top: Cell::new(None),
            history_rail: RefCell::default(),
            painted_source: Cell::new(None),
            find_highlights: RefCell::default(),
            transcript_painted: OnceCell::new(),
            history_search: OnceCell::new(),
            scroll_update: Cell::new(false),
            wheel_rows: Cell::new(0.0),
            reported_buttons: Cell::new(0),
            last_pointer_motion: Cell::new(None),
            last_pointer_cell: Cell::new(None),
            pointer_wheel: Cell::new(0.0),
            painted_history: Cell::new(None),
            dirty: Cell::new(true),
            exited: Cell::new(false),
            input_focused: Cell::new(false),
            input_method: gtk4::IMMulticontext::new(),
            keys: gtk4::EventControllerKey::new(),
            input_method_delegated: Cell::new(false),
            input_style,
            input_cursor: Cell::new(None),
            paste_revision: Cell::new(0),
            paste_reader: RefCell::new(None),
            input_revision: Cell::new(0),
        });
        if automatic_grid && session.pty().is_some() {
            session.resize(rows, columns);
        }
        session.connect_input();
        let open_link = (links.attach_opener)(session.view.widget.upcast_ref());
        let weak = Rc::downgrade(&session);
        connect_links(
            &session.view.widget,
            move |x, y| {
                let pane = weak.upgrade()?;
                pane.view.link_at(x, y, pane.plain_links)
            },
            open_link,
        );
        session.connect_pointer();
        session.connect_scroll(&scroll);
        let weak = Rc::downgrade(&session);
        let scroll = scroll.downgrade();
        session.view.widget.add_tick_callback(move |_, _| {
            let Some(session) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let Some(scroll) = scroll.upgrade() else {
                return glib::ControlFlow::Break;
            };
            if automatic_grid && !session.exited.get() && scroll.width() > 0 && scroll.height() > 0
            {
                let (rows, columns) = session.view.dimensions(scroll.width(), scroll.height());
                if session.dimensions.get() != (rows, columns) {
                    session.resize(rows, columns);
                }
            }
            session.paint();
            session.sync_input_cursor();
            glib::ControlFlow::Continue
        });
        match &session.backend {
            Backend::Host {
                terminal,
                local_reader,
            } => {
                terminal.set_color_defaults(colors);
                let pane = Rc::downgrade(&session);
                terminal.on_update(move |update| {
                    if let Some(pane) = pane.upgrade() {
                        pane.observe_host(update);
                    }
                })?;
                if local_reader.is_some() {
                    let for_output = Rc::downgrade(&session);
                    let for_exit = Rc::downgrade(&session);
                    terminal.pty().start_reader(
                        move |bytes| {
                            if let Some(session) = for_output.upgrade() {
                                let output = session.local_reader();
                                if let Err(error) = output.apply(&bytes) {
                                    session.report(&error);
                                    session.stop();
                                }
                            }
                        },
                        move |status| {
                            if let Some(session) = for_exit.upgrade() {
                                session.local_reader().finish(status);
                            }
                        },
                    )?;
                }
            }
            #[cfg(target_os = "linux")]
            Backend::Runtime(owner) => {
                session
                    .view
                    .widget
                    .update_property(&[gtk4::accessible::Property::Description(
                        "Shared runtime session",
                    )]);
                let mut updates = owner.configure_view(links.admit, colors)?;
                let pane = Rc::downgrade(&session);
                glib::MainContext::default().spawn_local(async move {
                    while updates.next().await.is_some() {
                        let Some(pane) = pane.upgrade() else { break };
                        pane.observe_runtime();
                    }
                });
            }
        }
        Ok((page, session))
    }

    pub fn pty(&self) -> Option<&P> {
        match &self.backend {
            Backend::Host { terminal, .. } => Some(terminal.pty()),
            #[cfg(target_os = "linux")]
            Backend::Runtime(_) => None,
        }
    }

    #[cfg(target_os = "linux")]
    pub fn runtime(&self) -> Option<&Rc<RuntimeShell>> {
        match &self.backend {
            Backend::Host { .. } => None,
            Backend::Runtime(owner) => Some(owner),
        }
    }

    fn inspect<T>(&self, read: impl FnOnce(&Terminal) -> T) -> Result<T, String> {
        match &self.backend {
            Backend::Host { terminal, .. } => Ok(terminal.inspect(read)),
            #[cfg(target_os = "linux")]
            Backend::Runtime(owner) => owner.inspect(read),
        }
    }

    fn local_reader(&self) -> &TerminalOutput<P> {
        match &self.backend {
            Backend::Host {
                local_reader: Some(output),
                ..
            } => output,
            _ => unreachable!("only a standalone host pane installs a local reader"),
        }
    }

    fn observe_host(&self, update: TerminalUpdate) {
        self.dirty.set(true);
        match update {
            TerminalUpdate::Output(window) => {
                self.notify(PaneEvent::Output);
                self.notify_window(window);
            }
            TerminalUpdate::Exited(status) => self.report_exit(status),
        }
    }

    #[cfg(target_os = "linux")]
    fn observe_runtime(&self) {
        let Some(owner) = self.runtime() else { return };
        let update = match owner.take_update() {
            Ok(update) => update,
            Err(error) => {
                if !self.exited.replace(true) {
                    self.set_input_focus(false);
                    self.report(&error);
                }
                return;
            }
        };
        if update.output_changed || update.exit_status.is_some() || update.stopped {
            self.dirty.set(true);
        }
        if update.output_changed {
            self.notify(PaneEvent::Output);
        }
        self.notify_window(update.window);
        if let Some(error) = update.failure {
            self.report(&format!("Terminal runtime failure: {error}"));
        }
        if let Some(status) = update.exit_status {
            self.report_exit(status);
        }
        if update.stopped && !self.exited.replace(true) {
            self.set_input_focus(false);
        }
        if self.dirty.get()
            && self.view.widget.is_mapped()
            && !self.view.widget.buffer().has_selection()
        {
            owner.prepare_viewport(self.viewport_request());
        }
    }

    fn report_exit(&self, status: PtyExit) {
        if !self.exited.replace(true) {
            self.set_input_focus(false);
            let mut message = format!("Shell exited ({})", status.code);
            if let Some(notice) = status.tail.notice() {
                message.push_str(". ");
                message.push_str(notice);
            }
            self.report(&message);
            self.notify(PaneEvent::Exited(status));
        }
    }

    pub fn set_event_handler(&self, handler: impl Fn(PaneEvent) + 'static) -> bool {
        self.event_handler.set(Box::new(handler)).is_ok()
    }

    fn notify(&self, event: PaneEvent) {
        if let Some(handler) = self.event_handler.get() {
            handler(event);
        }
    }

    fn notify_window(&self, update: WindowUpdates) {
        if let Some(title) = update.title {
            self.notify(PaneEvent::TitleChanged(title));
        }
        if update.bell {
            self.notify(PaneEvent::Bell);
        }
    }

    pub fn screen(&self) -> &gtk4::TextView {
        &self.view.widget
    }

    pub fn window_title(&self) -> Option<String> {
        self.inspect(|terminal| terminal.window_title().map(str::to_owned))
            .ok()
            .flatten()
    }

    pub fn set_theme(&self, theme: Theme) {
        let colors = theme.terminal_colors();
        match &self.backend {
            Backend::Host { terminal, .. } => terminal.set_color_defaults(colors),
            #[cfg(target_os = "linux")]
            Backend::Runtime(owner) => {
                if let Err(error) = owner.set_color_defaults(colors) {
                    self.report(&format!("Cannot update terminal colors: {error}"));
                }
            }
        }
        self.view.set_theme(theme);
        // A fully historical viewport can otherwise suppress the repaint.
        self.painted_history.set(None);
        self.dirty.set(true);
    }

    pub fn set_font(&self, font: &pango::FontDescription) {
        self.view.set_font(font);
        self.painted_history.set(None);
        self.dirty.set(true);
    }

    pub fn set_line_height_scale(&self, scale: f64) {
        self.view.set_line_height_scale(scale);
        self.dirty.set(true);
    }

    pub fn set_history_row_limit(&self, rows: usize) {
        match &self.backend {
            Backend::Host { terminal, .. } => terminal.set_history_row_limit(rows),
            #[cfg(target_os = "linux")]
            Backend::Runtime(owner) => {
                if let Err(error) = owner.set_history_row_limit(rows) {
                    self.report(&format!("Cannot set terminal scrollback: {error}"));
                    return;
                }
            }
        }
        self.dirty.set(true);
    }

    /// Local screen origin and cell size in GTK logical pixels.
    pub fn cell_metrics(&self) -> (i32, i32, i32, i32) {
        self.view.cell_metrics()
    }

    pub fn grid_matches_allocation(&self) -> bool {
        self.viewport.upgrade().is_some_and(|viewport| {
            viewport.is_mapped()
                && viewport.width() > 0
                && viewport.height() > 0
                && self.dimensions.get()
                    == self.view.dimensions(viewport.width(), viewport.height())
        })
    }

    pub fn has_selection(&self) -> bool {
        self.view.widget.buffer().has_selection()
    }

    /// The text selected in the mounted frame, joining terminal soft wraps.
    pub fn selected_text(&self) -> Option<String> {
        self.view.selected_text()
    }

    /// Let an overlay place and present composition while this screen keeps
    /// ownership of keys, committed input, and the PTY.
    pub fn delegate_composition(&self, delegated: bool) {
        if self.input_method_delegated.replace(delegated) != delegated && self.input_focused.get() {
            self.set_composition_active(!delegated);
        }
    }

    pub fn write_text(&self, text: &str) -> Result<(), String> {
        self.submit_input(|terminal| terminal.encode_text(text))
            .map(|_| ())
    }

    pub fn input_revision(&self) -> u64 {
        self.input_revision.get()
    }

    pub fn write_input(&self, input: Vec<u8>) -> Result<(), String> {
        if input.len() > jterm_engine::terminal::MAX_INPUT_BYTES {
            return Err("terminal input byte limit".into());
        }
        self.submit_input(|_| Ok(input)).map(|_| ())
    }

    /// Give the configured shell its usual startup interval before submitting
    /// commands through the same input queue as interactive input.
    #[cfg(target_os = "linux")]
    pub fn schedule_initial_commands(self: &Rc<Self>, commands: &[String]) {
        if commands.is_empty() {
            return;
        }
        let pane = Rc::downgrade(self);
        let commands = commands.to_vec();
        glib::timeout_add_local_once(std::time::Duration::from_millis(500), move || {
            let Some(pane) = pane.upgrade() else { return };
            for command in commands {
                if pane.exited.get() || pane.runtime().and_then(|runtime| runtime.child_pid()).is_none() {
                    break;
                }
                let mut input = command.into_bytes();
                input.push(b'\r');
                if let Err(error) = pane.write_input(input) {
                    pane.report(&format!("Could not send a startup command ({error})"));
                    break;
                }
            }
        });
    }

    pub fn scroll(&self, lines: i64) {
        self.scroll_rows(lines);
    }

    pub fn toggle_history_search(self: &Rc<Self>) {
        let Some(parent) = self.view.widget.root().and_downcast::<gtk4::Window>() else {
            return;
        };
        self.history_search
            .get_or_init(|| HistorySearchView::new(self))
            .toggle(&parent);
    }

    pub(crate) async fn history_search_step(
        &self,
        mut search: HistorySearch,
    ) -> Result<(HistorySearch, HistorySearchProgress), String> {
        match &self.backend {
            Backend::Host { terminal, .. } => {
                let progress = terminal.inspect(|terminal| search.step(terminal))?;
                Ok((search, progress))
            }
            #[cfg(target_os = "linux")]
            Backend::Runtime(owner) => owner.history_search_step(search).await,
        }
    }

    pub fn follow_output(&self) {
        self.set_scroll_top(None);
    }

    pub fn grid_size(&self) -> (u16, u16) {
        let (rows, columns) = self.dimensions.get();
        (columns as u16, rows as u16)
    }

    pub fn focus(&self) {
        self.view.widget.grab_focus();
    }

    pub fn stop(&self) {
        self.exited.set(true);
        self.set_input_focus(false);
        match &self.backend {
            Backend::Host { terminal, .. } => terminal.stop(),
            #[cfg(target_os = "linux")]
            Backend::Runtime(owner) => owner.close(),
        }
    }

    pub fn copy(&self) {
        if !self.view.widget.has_focus() || !self.view.widget.is_mapped() {
            return;
        }
        if let Some(text) = self.selected_text() {
            self.view.widget.clipboard().set_text(&text);
        }
    }

    pub fn paste(self: &Rc<Self>) {
        if self.exited.get() || !self.input_focused.get() {
            return;
        }
        let revision = self.advance_paste_revision();
        let session = Rc::downgrade(self);
        let clipboard = self.view.widget.clipboard();
        let bounded = self.pty().is_none();
        let task = glib::MainContext::default().spawn_local(async move {
            let result = if bounded {
                read_native_clipboard(&clipboard).await
            } else {
                clipboard
                    .read_text_future()
                    .await
                    .map(|text| text.map(String::from))
                    .map_err(|error| error.to_string())
            };
            let Some(session) = session.upgrade() else {
                return;
            };
            if session.exited.get()
                || !session.input_focused.get()
                || !session.view.widget.is_mapped()
                || session.paste_revision.get() != revision
            {
                return;
            }
            session.paste_reader.borrow_mut().take();
            match result {
                Ok(Some(text)) => {
                    #[cfg(target_os = "linux")]
                    if let Some(owner) = session.runtime() {
                        match owner.paste(&text) {
                            Ok(true) => session.input_admitted(),
                            Ok(false) => (),
                            Err(error) => {
                                session.report(&format!("Terminal paste rejected: {error}"))
                            }
                        }
                        return;
                    }
                    session.send_input(|terminal| {
                        terminal.encode_paste(&text).map(|mut bytes| {
                            if !bytes.starts_with(b"\x1b[200~") {
                                // Copying a trailing line break is common. Preserve the
                                // explicit Enter required by unbracketed shell input.
                                if let Some(end) = bytes.iter().position(|byte| *byte == b'\r') {
                                    bytes.truncate(end);
                                }
                            }
                            bytes
                        })
                    });
                }
                Ok(None) => {}
                Err(error) => session.report(&format!("Cannot read clipboard: {error}")),
            }
        });
        self.paste_reader.replace(Some(task));
    }

    fn advance_paste_revision(&self) -> u64 {
        if let Some(task) = self.paste_reader.borrow_mut().take() {
            task.abort();
        }
        let revision = self.paste_revision.get().wrapping_add(1);
        self.paste_revision.set(revision);
        revision
    }

    fn resize(&self, rows: usize, columns: usize) {
        if let Err(error) = self.resize_grid(rows, columns) {
            self.report(&format!("Cannot resize terminal: {error}"));
        }
    }

    fn resize_grid(&self, rows: usize, columns: usize) -> Result<(), String> {
        match &self.backend {
            Backend::Host { terminal, .. } => terminal.resize(rows, columns),
            #[cfg(target_os = "linux")]
            Backend::Runtime(owner) => owner.resize(rows, columns),
        }?;
        self.dimensions.set((rows, columns));
        self.dirty.set(true);
        Ok(())
    }

    fn paint(&self) {
        if !self.dirty.get() || self.view.widget.buffer().has_selection() {
            return;
        }
        let exited = self.exited.get();
        let request = self.viewport_request();
        let snapshot = match &self.backend {
            Backend::Host { terminal, .. } => terminal
                .viewport(request)
                .map(|frame| frame.map(|frame| (frame, false))),
            #[cfg(target_os = "linux")]
            Backend::Runtime(owner) => owner.poll_viewport(request),
        };
        let (
            ViewportFrame {
                frame,
                live_margin_gaps,
                colors,
                extent,
                history,
                evicted_anchor,
            },
            newer_output,
        ) = match snapshot {
            Ok(Some(snapshot)) => snapshot,
            Ok(None) => return,
            Err(error) => {
                self.report(&format!("Cannot display terminal output: {error}"));
                return;
            }
        };
        let history_top = history.as_ref().map(|history| history.first);
        if frame.stamp().screen == Screen::Primary {
            self.scroll_top.set(
                history
                    .as_ref()
                    .map(|history| ScrollTarget::Source(history.anchor)),
            );
        }
        if evicted_anchor && !exited {
            self.report("Earlier scrollback is no longer retained");
        }
        let rail = HistoryRail {
            extent,
            layout: history.as_ref().map(|history| history.layout.clone()),
        };
        let total = rail.len();
        self.history_rail.replace(rail);
        self.sync_scrollback(total, history_top, frame.rows().len());
        match &history {
            Some(history) => {
                if let Some(page) = &history.page {
                    self.view
                        .present(&frame, live_margin_gaps, Some(page), colors.as_ref());
                }
                self.painted_history.set(Some(history.window));
            }
            None => {
                self.view
                    .present(&frame, live_margin_gaps, None, colors.as_ref());
                self.painted_history.set(None);
            }
        }
        self.inline_images.borrow_mut().present(
            &frame,
            history
                .as_ref()
                .map(|history| (history.window.history_rows, history.page.as_ref())),
            || self.view.cell_metrics(),
        );
        self.painted_source.set(Some((frame.stamp(), history_top)));
        self.dirty.set(newer_output);
        self.paint_find();
        if let Some(observer) = self.transcript_painted.get() {
            observer();
        }
    }

    fn painted_find_top(&self, frame: &FindFrame) -> Option<usize> {
        let (stamp, top) = self.painted_source.get()?;
        let rail = self.history_rail.borrow();
        let source = &frame.0;
        (source.stamp == stamp && rail.extent == (source.history_start..source.history_end))
            .then_some(top.unwrap_or(source.history_rows))
    }

    fn paint_find(&self) {
        let Some(find) = self.find_highlights.borrow().clone() else {
            return;
        };
        if let Some(top) = self.painted_find_top(&find.frame) {
            self.view.paint_find(top, &find.hits, find.current);
        } else {
            self.find_highlights.take();
            self.view.clear_find();
        }
    }

    fn viewport_request(&self) -> ViewportRequest {
        ViewportRequest {
            top: self.scroll_top.get(),
            painted_history: self.painted_history.get(),
        }
    }

    fn sync_scrollback(&self, total: usize, top: Option<usize>, rows: usize) {
        self.scroll_update.set(true);
        self.scrollback.configure(
            top.unwrap_or(total) as f64,
            0.0,
            total as f64 + rows as f64,
            1.0,
            rows.saturating_sub(1).max(1) as f64,
            rows as f64,
        );
        self.scroll_update.set(false);
    }

    fn set_scroll_top(&self, top: Option<ScrollTarget>) {
        if self.scroll_top.replace(top) != top {
            if top.is_some() && self.input_focused.get() {
                self.reset_preedit();
            }
            self.clear_selection();
            self.dirty.set(true);
        }
    }

    fn clear_selection(&self) {
        let buffer = self.view.widget.buffer();
        if buffer.has_selection() {
            buffer.place_cursor(&buffer.start_iter());
        }
    }

    fn scroll_rows(&self, rows: i64) -> bool {
        let next = {
            let history = self.history_rail.borrow();
            if history.extent.is_empty() {
                return false;
            }
            history.scroll(self.scroll_top.get(), rows)
        };
        self.set_scroll_top(next);
        true
    }

    fn scroll_key(&self, key: gdk::Key, modifiers: gdk::ModifierType) -> bool {
        use gdk::ModifierType;
        if modifiers.intersects(
            ModifierType::CONTROL_MASK
                | ModifierType::ALT_MASK
                | ModifierType::META_MASK
                | ModifierType::SUPER_MASK
                | ModifierType::HYPER_MASK,
        ) {
            return false;
        }
        let shifted = modifiers.contains(ModifierType::SHIFT_MASK);
        if !shifted
            && !(matches!(
                self.input_style,
                InputStyle::HostedAttachment {
                    inline_history: true
                } | InputStyle::InlineTask {
                    inline_history: true
                }
            ) && matches!(
                key,
                gdk::Key::Page_Up
                    | gdk::Key::Page_Down
                    | gdk::Key::KP_Page_Up
                    | gdk::Key::KP_Page_Down
            ) && self.inspect(Terminal::alternate_screen_active) == Ok(false))
        {
            return false;
        }
        let page = self.dimensions.get().0.saturating_sub(1).max(1) as i64;
        let delta = match key {
            gdk::Key::Page_Up => -page,
            gdk::Key::Page_Down => page,
            gdk::Key::KP_Page_Up if !shifted => -page,
            gdk::Key::KP_Page_Down if !shifted => page,
            gdk::Key::Home => i64::MIN,
            gdk::Key::End => i64::MAX,
            _ => return false,
        };
        self.scroll_rows(delta)
    }

    fn connect_scroll(self: &Rc<Self>, scroll: &gtk4::ScrolledWindow) {
        let weak = Rc::downgrade(self);
        self.scrollback.connect_value_changed(move |adjustment| {
            let Some(session) = weak
                .upgrade()
                .filter(|session| !session.scroll_update.get())
            else {
                return;
            };
            let target = session
                .history_rail
                .borrow()
                .target_at(adjustment.value().round() as usize);
            session.set_scroll_top(target);
        });

        let wheel = gtk4::EventControllerScroll::new(gtk4::EventControllerScrollFlags::VERTICAL);
        wheel.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let weak = Rc::downgrade(self);
        wheel.connect_scroll(move |controller, _, dy| {
            let Some(session) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            if controller.current_event_state().intersects(
                gdk::ModifierType::CONTROL_MASK
                    | gdk::ModifierType::ALT_MASK
                    | gdk::ModifierType::META_MASK
                    | gdk::ModifierType::SUPER_MASK
                    | gdk::ModifierType::HYPER_MASK,
            ) {
                session.wheel_rows.set(0.0);
                session.pointer_wheel.set(0.0);
                return glib::Propagation::Proceed;
            }
            if let Some(event) = controller.current_event() {
                if session.report_wheel(&event, dy, controller.unit()) {
                    return glib::Propagation::Stop;
                }
            }
            if session.history_rail.borrow().extent.is_empty() {
                session.wheel_rows.set(0.0);
                return glib::Propagation::Proceed;
            }
            let lines = if controller.unit() == gdk::ScrollUnit::Surface {
                dy / session.view.row_height()
            } else {
                dy * 3.0
            };
            let position = session.wheel_rows.get() + lines;
            let whole = position.trunc();
            session.wheel_rows.set(position - whole);
            session.scroll_rows(whole as i64);
            glib::Propagation::Stop
        });
        scroll.add_controller(wheel);
    }

    fn submit_input(
        &self,
        encode: impl FnOnce(&Terminal) -> Result<Vec<u8>, String>,
    ) -> Result<bool, String> {
        if !self.exited.get() {
            if let Some(pty) = self.pty() {
                pty.before_input();
            }
        }
        let accepted = self.queue_input(encode)?;
        if accepted {
            self.input_admitted();
        }
        Ok(accepted)
    }

    fn input_admitted(&self) {
        self.input_revision
            .set(self.input_revision.get().wrapping_add(1));
        self.clear_selection();
        self.set_scroll_top(None);
    }

    fn queue_input(
        &self,
        encode: impl FnOnce(&Terminal) -> Result<Vec<u8>, String>,
    ) -> Result<bool, String> {
        if self.exited.get() {
            return Err("terminal input is closed".into());
        }
        match &self.backend {
            Backend::Host { terminal, .. } => terminal.inspect(encode).and_then(|bytes| {
                if bytes.is_empty() {
                    return Ok(false);
                }
                terminal.pty().write(&bytes).map(|()| true)
            }),
            #[cfg(target_os = "linux")]
            Backend::Runtime(owner) => owner.input(encode),
        }
    }

    fn send_input(&self, encode: impl FnOnce(&Terminal) -> Result<Vec<u8>, String>) {
        if let Err(error) = self.submit_input(encode) {
            self.report(&format!("Terminal input rejected: {error}"));
        }
    }

    fn report_focus(&self, focused: bool) {
        if !self.exited.get() {
            let _ = self.queue_input(|terminal| {
                Ok(terminal.encode_focus(focused).unwrap_or_default().to_vec())
            });
        }
    }

    fn connect_pointer(self: &Rc<Self>) {
        let events = gtk4::EventControllerLegacy::new();
        events.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let weak = Rc::downgrade(self);
        events.connect_event(move |_, event| {
            weak.upgrade()
                .is_some_and(|session| session.report_pointer(event))
                .into()
        });
        self.view.widget.add_controller(events);
    }

    fn report_pointer(&self, event: &gdk::Event) -> bool {
        let state = event.modifier_state();
        let modifiers = mouse_modifiers(state);
        let button = || {
            let event = event.downcast_ref::<gdk::ButtonEvent>()?;
            (1..=3)
                .contains(&event.button())
                .then(|| event.button() as u8 - 1)
        };
        let (input, released) = match event.event_type() {
            gdk::EventType::ButtonPress => {
                let Some(button) = button() else { return false };
                (MouseInput::Press(button), false)
            }
            gdk::EventType::ButtonRelease => {
                let Some(button) = button() else { return false };
                let mask = 1 << button;
                let previous = self.reported_buttons.get();
                self.reported_buttons.set(previous & !mask);
                if previous & mask == 0 {
                    return false;
                }
                (MouseInput::Release(button), true)
            }
            gdk::EventType::MotionNotify => {
                let buttons = self.reported_buttons.get();
                (
                    MouseInput::Motion((0..3).find(|button| buttons & (1 << button) != 0)),
                    false,
                )
            }
            gdk::EventType::GrabBroken => {
                self.reported_buttons.set(0);
                self.last_pointer_motion.set(None);
                self.last_pointer_cell.set(None);
                return false;
            }
            gdk::EventType::LeaveNotify => {
                self.last_pointer_motion.set(None);
                self.last_pointer_cell.set(None);
                return false;
            }
            _ => return false,
        };
        if self.exited.get()
            || (!released
                && (self.scroll_top.get().is_some()
                    || state.contains(gdk::ModifierType::SHIFT_MASK)))
        {
            return released;
        }
        let Some(point) = self.view.event_point(event) else {
            return released;
        };
        if input == MouseInput::Press(0)
            && link_modifier(state)
            && self
                .view
                .link_at(point.0, point.1, self.plain_links)
                .is_some()
        {
            return false;
        }
        let Some((column, row)) = self.view.cell_at(point, self.dimensions.get(), released) else {
            self.last_pointer_cell.set(None);
            return released;
        };
        self.last_pointer_cell.set(Some((column, row)));
        let motion = if let MouseInput::Motion(button) = input {
            Some(PointerMotion {
                column,
                row,
                button,
                modifiers,
            })
        } else {
            self.last_pointer_motion.set(None);
            None
        };
        let already_sent = motion.is_some() && self.last_pointer_motion.get() == motion;
        let mut reporting = false;
        let sent = self
            .queue_input(|terminal| {
                let Some(bytes) = terminal.encode_mouse(input, column, row, modifiers) else {
                    return Ok(Vec::new());
                };
                reporting = true;
                Ok(if already_sent { Vec::new() } else { bytes })
            })
            .unwrap_or(false);
        if sent {
            self.last_pointer_motion.set(motion);
            if let MouseInput::Press(button) = input {
                let buttons = self.reported_buttons.get() | (1 << button);
                self.reported_buttons.set(buttons);
                // The press has already reported this cell. GTK can deliver
                // motion from the same cell when the button grab starts.
                self.last_pointer_motion.set(Some(PointerMotion {
                    column,
                    row,
                    button: (0..3).find(|button| buttons & (1 << button) != 0),
                    modifiers,
                }));
                self.view.widget.grab_focus();
            }
        } else if !reporting {
            self.last_pointer_motion.set(None);
        }
        reporting || released
    }

    fn report_wheel(&self, event: &gdk::Event, dy: f64, unit: gdk::ScrollUnit) -> bool {
        if self.exited.get()
            || self.scroll_top.get().is_some()
            || dy == 0.0
            || event
                .modifier_state()
                .contains(gdk::ModifierType::SHIFT_MASK)
        {
            self.pointer_wheel.set(0.0);
            return false;
        }
        let Some((column, row)) = self
            .view
            .event_point(event)
            .and_then(|point| self.view.cell_at(point, self.dimensions.get(), false))
            .or_else(|| self.last_pointer_cell.get())
        else {
            return false;
        };
        let delta = match event
            .downcast_ref::<gdk::ScrollEvent>()
            .map(|scroll| scroll.direction())
        {
            Some(gdk::ScrollDirection::Up) => -1.0,
            Some(gdk::ScrollDirection::Down) => 1.0,
            _ if unit == gdk::ScrollUnit::Surface => dy / self.view.row_height(),
            _ => dy,
        };
        let pending = self.pointer_wheel.get() + delta;
        let count = pending.trunc().abs().min(8.0) as usize;
        let input = if pending < 0.0 {
            MouseInput::WheelUp
        } else {
            MouseInput::WheelDown
        };
        let mut reporting = false;
        let result = self.queue_input(|terminal| {
            let Some(bytes) =
                terminal.encode_mouse(input, column, row, mouse_modifiers(event.modifier_state()))
            else {
                return Ok(Vec::new());
            };
            reporting = true;
            Ok(bytes.repeat(count))
        });
        self.pointer_wheel
            .set(if reporting { pending.fract() } else { 0.0 });
        reporting && result.is_ok()
    }

    fn report(&self, text: &str) {
        self.status.set_text(&(self.status_text)(text, 256));
        self.status.set_visible(true);
    }

    fn reset_preedit(&self) {
        self.input_method.reset();
        self.view.clear_preedit();
    }

    fn sync_input_cursor(&self) {
        if !self.accepts_ime() || self.input_method_delegated.get() {
            return;
        }
        if let Some(area) = self.view.input_cursor() {
            if self.input_cursor.replace(Some(area)) != Some(area) {
                self.input_method.set_cursor_location(&area);
            }
        }
    }

    fn accepts_ime(&self) -> bool {
        !self.exited.get() && self.view.widget.is_mapped() && self.view.widget.has_focus()
    }

    fn set_composition_active(&self, active: bool) {
        if active {
            self.keys.set_im_context(Some(&self.input_method));
            self.input_method.focus_in();
            self.sync_input_cursor();
        } else {
            self.input_cursor.set(None);
            self.input_method.focus_out();
            self.reset_preedit();
            self.keys.set_im_context(None::<&gtk4::IMContext>);
        }
    }

    fn set_input_focus(&self, focused: bool) {
        if self.input_focused.replace(focused) == focused {
            return;
        }
        self.report_focus(focused);
        if !focused {
            self.advance_paste_revision();
        }
        self.set_composition_active(focused && !self.input_method_delegated.get());
    }

    fn connect_input(self: &Rc<Self>) {
        let keys = &self.keys;
        keys.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let im = &self.input_method;
        im.set_client_widget(Some(&self.view.widget));
        im.set_use_preedit(true);
        // Set terminal input properties before mapping. GTK can then bind the
        // Wayland text-input object ahead of the compositor's first focus.
        im.set_input_purpose(gtk4::InputPurpose::Terminal);
        let weak = Rc::downgrade(self);
        im.connect_commit(move |_, text| {
            if let Some(session) = weak
                .upgrade()
                .filter(|session| session.accepts_ime() && !session.input_method_delegated.get())
            {
                session.view.clear_preedit();
                session.send_input(|terminal| terminal.encode_text(text));
            }
        });
        let weak = Rc::downgrade(self);
        im.connect_preedit_changed(move |im| {
            let Some(session) = weak
                .upgrade()
                .filter(|session| session.accepts_ime() && !session.input_method_delegated.get())
            else {
                return;
            };
            let (text, attributes, _) = im.preedit_string();
            if text.is_empty() {
                session.view.clear_preedit();
            } else {
                session.set_scroll_top(None);
                session.view.show_preedit(&text, attributes);
                session.sync_input_cursor();
            }
        });
        let weak = Rc::downgrade(self);
        im.connect_preedit_end(move |_| {
            if let Some(session) = weak.upgrade() {
                session.view.clear_preedit();
            }
        });
        let weak = Rc::downgrade(self);
        self.view.widget.connect_has_focus_notify(move |_| {
            if let Some(session) = weak.upgrade() {
                session.set_input_focus(session.accepts_ime());
            }
        });
        let weak = Rc::downgrade(self);
        self.view.widget.connect_map(move |_| {
            if let Some(session) = weak.upgrade() {
                session.set_input_focus(session.accepts_ime());
            }
        });
        let weak = Rc::downgrade(self);
        self.view.widget.connect_unmap(move |_| {
            if let Some(session) = weak.upgrade() {
                session.set_input_focus(false);
                if let Some(search) = session.history_search.get() {
                    search.hide();
                }
            }
        });
        let weak = Rc::downgrade(self);
        keys.connect_key_pressed(move |controller, key, keycode, modifiers| {
            let Some(session) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            if session.scroll_key(key, modifiers) {
                return glib::Propagation::Stop;
            }
            let Some(name) = logical_key(key) else {
                return glib::Propagation::Proceed;
            };
            if modifiers.intersects(
                gdk::ModifierType::META_MASK
                    | gdk::ModifierType::SUPER_MASK
                    | gdk::ModifierType::HYPER_MASK,
            ) {
                return glib::Propagation::Proceed;
            }
            let shift =
                key == gdk::Key::ISO_Left_Tab || modifiers.contains(gdk::ModifierType::SHIFT_MASK);
            let alt = modifiers.contains(gdk::ModifierType::ALT_MASK);
            let ctrl = modifiers.contains(gdk::ModifierType::CONTROL_MASK);
            if matches!(session.input_style, InputStyle::HostedAttachment { .. })
                && key == gdk::Key::bracketright
                && ctrl
                && !shift
                && !alt
            {
                // The relay consumes this byte before writing to the provider.
                // Its meaning is independent of the provider's keyboard mode.
                session.send_input(|_| Ok(vec![0x1d]));
                return glib::Propagation::Stop;
            }
            let base = if (alt || ctrl) && key.to_unicode().is_some() {
                controller
                    .current_event()
                    .and_then(|event| event.downcast::<gdk::KeyEvent>().ok())
                    .and_then(|event| {
                        session.view.widget.display().translate_key(
                            keycode,
                            gdk::ModifierType::empty(),
                            event.layout() as i32,
                        )
                    })
                    .and_then(|(base, _, _, _)| base.to_unicode())
            } else {
                None
            };
            session.send_input(|terminal| {
                terminal.encode_key_with_base(&name, base, shift, alt, ctrl)
            });
            glib::Propagation::Stop
        });
        self.view.widget.add_controller(keys.clone());
    }
}

impl<P: Pty> Drop for TerminalPane<P> {
    fn drop(&mut self) {
        if let Some(search) = self.history_search.get() {
            search.hide();
        }
        if let Some(task) = self.paste_reader.get_mut().take() {
            task.abort();
        }
    }
}

async fn read_native_clipboard(clipboard: &gdk::Clipboard) -> Result<Option<String>, String> {
    use jterm_engine::terminal::MAX_STREAMED_PASTE_BYTES;

    const TEXT_FORMATS: &[&str] = &["text/plain;charset=utf-8", "text/plain"];
    let formats = clipboard.formats();
    if !TEXT_FORMATS
        .iter()
        .any(|mime| formats.contain_mime_type(mime))
    {
        return Ok(None);
    }
    let (stream, _) = clipboard
        .read_future(TEXT_FORMATS, glib::Priority::DEFAULT)
        .await
        .map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    loop {
        let count = 4096.min(MAX_STREAMED_PASTE_BYTES + 1 - bytes.len());
        let chunk = stream
            .read_bytes_future(count, glib::Priority::DEFAULT)
            .await
            .map_err(|error| error.to_string())?;
        if chunk.is_empty() {
            break;
        }
        bytes.extend_from_slice(&chunk);
        if bytes.len() > MAX_STREAMED_PASTE_BYTES {
            return Err("clipboard paste byte limit".into());
        }
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| "clipboard text is not UTF-8".into())
}

fn mouse_modifiers(modifiers: gdk::ModifierType) -> u8 {
    (u8::from(modifiers.contains(gdk::ModifierType::SHIFT_MASK)) << 2)
        | (u8::from(modifiers.contains(gdk::ModifierType::ALT_MASK)) << 3)
        | (u8::from(modifiers.contains(gdk::ModifierType::CONTROL_MASK)) << 4)
}

fn logical_key(key: gdk::Key) -> Option<std::borrow::Cow<'static, str>> {
    use gdk::Key;
    Some(
        match key {
            Key::Up => "ArrowUp",
            Key::Down => "ArrowDown",
            Key::Left => "ArrowLeft",
            Key::Right => "ArrowRight",
            Key::Home => "Home",
            Key::End => "End",
            Key::Insert => "Insert",
            Key::Delete => "Delete",
            Key::Page_Up | Key::KP_Page_Up => "PageUp",
            Key::Page_Down | Key::KP_Page_Down => "PageDown",
            Key::F1 => "F1",
            Key::F2 => "F2",
            Key::F3 => "F3",
            Key::F4 => "F4",
            Key::F5 => "F5",
            Key::F6 => "F6",
            Key::F7 => "F7",
            Key::F8 => "F8",
            Key::F9 => "F9",
            Key::F10 => "F10",
            Key::F11 => "F11",
            Key::F12 => "F12",
            Key::Return | Key::KP_Enter => "Enter",
            Key::Escape => "Escape",
            Key::BackSpace => "Backspace",
            Key::Tab | Key::ISO_Left_Tab => "Tab",
            _ => {
                return key
                    .to_unicode()
                    .filter(|ch| !ch.is_control())
                    .map(|ch| ch.to_string().into())
            }
        }
        .into(),
    )
}

#[cfg(test)]
mod transcript_tests {
    use super::*;
    use crate::{TranscriptView, TranscriptZoneOrigin};

    struct TestPty;

    impl Pty for TestPty {
        fn write(&self, _bytes: &[u8]) -> Result<(), String> {
            Ok(())
        }

        fn resize(&self, _cols: u16, _rows: u16) -> Result<(), String> {
            Ok(())
        }

        fn stop(&self) {}

        fn start_reader(
            &self,
            _output: impl FnMut(Vec<u8>) + 'static,
            _exited: impl FnOnce(PtyExit) + 'static,
        ) -> Result<(), String> {
            panic!("the application owns the embedded reader")
        }
    }

    fn attach_test(
        cols: u16,
        rows: u16,
        theme: Theme,
        links: LinkActions,
    ) -> (
        gtk4::Widget,
        Rc<EmbeddedTerminal<TestPty>>,
        TerminalOutput<TestPty>,
    ) {
        let output =
            TerminalOutput::new(TestPty, cols, rows, TerminalProfile::Embedded, links.admit)
                .unwrap();
        let (widget, terminal) = EmbeddedTerminal::attach(
            output.handle(),
            theme,
            |s, _| s.to_owned(),
            links,
        )
        .unwrap();
        (widget, terminal, output)
    }

    #[test]
    #[ignore = "requires a GTK display"]
    fn painted_primary_footprint_tracks_the_visible_grid() {
        gtk4::init().unwrap();
        let theme = Theme {
            font: pango::FontDescription::from_string("Monospace 12"),
            foreground: gdk::RGBA::WHITE,
            background: gdk::RGBA::BLACK,
            palette: crate::default_ansi_palette(),
            cursor: None,
        };
        let links = LinkActions {
            admit: |_, _| false,
            find_plain: None,
            attach_opener: |_| Box::new(|_| {}),
        };
        let (widget, terminal, output) = attach_test(40, 12, theme, links);
        let window = gtk4::Window::builder()
            .default_width(700)
            .default_height(400)
            .child(&widget)
            .build();
        window.present();
        let context = glib::MainContext::default();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while terminal.pane.screen().height() < 300 && std::time::Instant::now() < deadline {
            while context.iteration(false) {}
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let screen = terminal.pane.screen();
        let (left, top, cell_width, cell_height) = terminal.pane.cell_metrics();
        let blank = |col, row| {
            terminal.painted_primary_rect_is_blank(
                screen,
                graphene::Rect::new(
                    (left + col * cell_width + 1) as f32,
                    (top + row * cell_height + 1) as f32,
                    (cell_width - 2) as f32,
                    (cell_height - 2) as f32,
                ),
            )
        };
        let painted_blank = |line, offset, far_half| {
            let iter = screen.buffer().iter_at_line_offset(line, offset).unwrap();
            let area = screen.iter_location(&iter);
            let (x, y) = screen.buffer_to_window_coords(
                gtk4::TextWindowType::Widget,
                area.x() + if far_half { area.width() / 2 } else { 0 },
                area.y(),
            );
            terminal.painted_primary_rect_is_blank(
                screen,
                graphene::Rect::new(x as f32 + 1.0, y as f32 + 1.0, 3.0, 3.0),
            )
        };
        terminal.pane.paint();
        assert!(blank(12, 5));
        output
            .apply("\x1b[6;12H界\x1b[6;22H\x1b[41m \x1b[0m\x1b[9;1H".as_bytes())
            .unwrap();
        assert!(!blank(29, 5), "pending output cannot authorize a seat");
        terminal.pane.paint();
        assert_eq!(terminal.painted_primary_cursor_row(), Some(8));
        assert!(
            !painted_blank(5, 11, true),
            "a wide glyph occupies both cells"
        );
        assert!(
            !painted_blank(5, 20, false),
            "a colored space occupies a cell"
        );
        assert!(blank(29, 5));

        output.apply(b"\x1b[7;11H").unwrap();
        let texture = gdk::MemoryTexture::new(
            2,
            2,
            gdk::MemoryFormat::R8g8b8a8,
            &glib::Bytes::from_static(&[255; 16]),
            8,
        )
        .upcast();
        let image = terminal
            .prepare_inline_image(
                texture,
                0,
                InlineImageRequest {
                    columns: Some(8),
                    rows: 1,
                    move_cursor: false,
                    cursor: terminal.inline_image_cursor().unwrap(),
                },
            )
            .unwrap();
        assert!(terminal.admit_inline_image(image));
        terminal.pane.paint();
        assert!(
            !blank(17, 6),
            "the far edge of an inline image occupies pixels"
        );

        output.apply(b"\x1b[?1049h").unwrap();
        terminal.pane.paint();
        assert_eq!(terminal.painted_primary_cursor_row(), None);
        assert!(!blank(29, 5));
        output.apply(b"\x1b[?1049l").unwrap();
        terminal.pane.paint();
        assert!(blank(29, 5));

        output.apply(b"\x1b[12;1H\r\n\r\n").unwrap();
        terminal.pane.paint();
        terminal.pane.scroll(-2);
        terminal.pane.paint();
        assert_eq!(terminal.painted_primary_cursor_row(), None);
        assert!(!blank(29, 5));
        window.close();
    }

    #[test]
    #[ignore = "requires a GTK display"]
    fn find_paints_archived_unicode_without_stealing_focus_or_selection() {
        gtk4::init().unwrap();
        let theme = Theme {
            font: pango::FontDescription::from_string("Monospace 12"),
            foreground: gdk::RGBA::WHITE,
            background: gdk::RGBA::BLACK,
            palette: crate::default_ansi_palette(),
            cursor: None,
        };
        let links = || LinkActions {
            admit: |_, _| false,
            find_plain: None,
            attach_opener: |_| Box::new(|_| {}),
        };
        let (widget, terminal, output) = attach_test(9, 4, theme.clone(), links());
        let (_, other_terminal, other_output) = attach_test(9, 4, theme, links());
        let content = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        let entry = gtk4::Entry::new();
        content.append(&entry);
        content.append(&widget);
        let window = gtk4::Window::builder().child(&content).build();
        window.present();
        entry.grab_focus();
        let focus_before = gtk4::prelude::RootExt::focus(&window).expect("find entry focus");
        output
            .apply("prefix界e\u{301}target\r\n1\r\n2\r\n3\r\n4\r\ncurrent".as_bytes())
            .unwrap();
        terminal.pane.paint();

        let query = "界e\u{301}target";
        let find = || {
            let snapshot = terminal
                .find_snapshot(FindRegion::Transcript, 8192, &mut || false)
                .unwrap();
            assert!(!snapshot.incomplete);
            let hits: Rc<[FindMatch]> = snapshot
                .windows
                .iter()
                .flat_map(|window| {
                    window
                        .text()
                        .match_indices(query)
                        .filter_map(|(offset, _)| window.match_at(offset..offset + query.len()))
                })
                .collect();
            assert_eq!(hits.len(), 1);
            (snapshot.frame, hits)
        };
        let (frame, hits) = find();
        other_output
            .apply("prefix界e\u{301}target\r\n1\r\n2\r\n3\r\n4\r\ncurrent".as_bytes())
            .unwrap();
        assert_eq!(
            terminal.inspect(|t| t.frame().unwrap().stamp()),
            other_terminal.inspect(|t| t.frame().unwrap().stamp())
        );
        assert!(!other_terminal.find_is_current(&frame));
        let other_frame = other_terminal
            .find_snapshot(FindRegion::Transcript, 8192, &mut || false)
            .unwrap()
            .frame;
        assert!(!other_terminal.highlight_find(&other_frame, hits.clone(), Some(0)));
        let starts_with_highlight = |character| {
            let screen = terminal.pane.screen();
            let buffer = screen.buffer();
            let tag = buffer
                .tag_table()
                .lookup("jterm-screen-find-current")
                .unwrap();
            let mut iter = buffer.start_iter();
            while !iter.is_end() && !iter.has_tag(&tag) {
                iter.forward_char();
            }
            assert_eq!(iter.char(), character);
            assert!(!buffer.has_selection());
            assert_eq!(
                gtk4::prelude::RootExt::focus(&window),
                Some(focus_before.clone())
            );
        };
        assert!(terminal.show_find(&frame, hits.clone(), 0));
        starts_with_highlight('界');
        terminal.resize(7, 4).unwrap();
        assert!(!terminal.show_find(&frame, hits, 0));
        // Return to the live grid so the search owns the complete retained prefix.
        terminal.pane.set_scroll_top(None);
        let (frame, hits) = find();
        assert!(terminal.show_find(&frame, hits, 0));
        starts_with_highlight('界');
        terminal.pane.set_history_row_limit(0);
        terminal.pane.paint();
        assert!(!terminal.find_is_current(&frame));
        assert!(terminal
            .pane
            .screen()
            .buffer()
            .tag_table()
            .lookup("jterm-screen-find-current")
            .is_none());
        window.close();
    }

    #[test]
    #[ignore = "requires a GTK display"]
    fn inline_image_paint_obeys_sync_and_screen_transitions() {
        gtk4::init().unwrap();
        let theme = Theme {
            font: pango::FontDescription::from_string("Monospace 12"),
            foreground: gdk::RGBA::WHITE,
            background: gdk::RGBA::BLACK,
            palette: crate::default_ansi_palette(),
            cursor: None,
        };
        let links = LinkActions {
            admit: |_, _| false,
            find_plain: None,
            attach_opener: |_| Box::new(|_| {}),
        };
        let (_, terminal, output) = attach_test(8, 4, theme, links);
        let texture = gdk::MemoryTexture::new(
            2,
            2,
            gdk::MemoryFormat::R8g8b8a8,
            &glib::Bytes::from_static(&[255; 16]),
            8,
        )
        .upcast();
        let images = || terminal.pane.inline_images.borrow().painted_slices();

        output.apply(b"\x1b[2;2H\x1b[?2026h").unwrap();
        let image = terminal
            .prepare_inline_image(
                texture,
                0,
                InlineImageRequest {
                    columns: Some(3),
                    rows: 2,
                    move_cursor: false,
                    cursor: terminal.inline_image_cursor().unwrap(),
                },
            )
            .unwrap();
        assert!(terminal.admit_inline_image(image));
        terminal.pane.paint();
        assert_eq!(images(), 0);

        output.apply(b"\x1b[?2026l").unwrap();
        terminal.pane.paint();
        assert_eq!(images(), 2);
        output.apply(b"\x1b[?1049h").unwrap();
        terminal.pane.paint();
        assert_eq!(images(), 0);
        output.apply(b"\x1b[?1049l").unwrap();
        terminal.pane.paint();
        assert_eq!(images(), 2);
        output.apply(b"\x1b[2J").unwrap();
        terminal.pane.paint();
        assert_eq!(images(), 0);
    }

    #[test]
    #[ignore = "requires a GTK display"]
    fn transcript_jump_and_hit_share_the_reflowed_row_source() {
        gtk4::init().unwrap();
        let theme = Theme {
            font: pango::FontDescription::from_string("Monospace 12"),
            foreground: gdk::RGBA::WHITE,
            background: gdk::RGBA::BLACK,
            palette: crate::default_ansi_palette(),
            cursor: None,
        };
        let links = LinkActions {
            admit: |_, _| false,
            find_plain: None,
            attach_opener: |_| Box::new(|_| {}),
        };
        let (widget, terminal, output) = attach_test(12, 4, theme, links);
        let window = gtk4::Window::new();
        window.set_default_size(400, 240);
        window.set_child(Some(&widget));
        window.present();

        let view = TranscriptView::new(terminal.clone());
        view.begin_prompt(7);
        output.apply(b"context\r\nprompt> ").unwrap();
        view.end_prompt();
        output
            .apply(b"\x1b[1G\x1b[1A\x1b[Jcontext\r\nprompt> echo\r\n")
            .unwrap();
        view.begin_output(Some(7));
        output.apply(b"witness-old\r\n").unwrap();
        view.end_output(Some(7), 64);
        output
            .apply(b"filler\r\nnext\r\nmore\r\nagain\r\n")
            .unwrap();
        let zone = view.source_zone(7).unwrap();
        assert_eq!(zone.origin, TranscriptZoneOrigin::Live);
        assert_eq!(view.output_text(7).unwrap(), "witness-old\n");
        assert!(view.reveal_zone(7));

        let next_frames = || {
            let context = glib::MainContext::default();
            for _ in 0..8 {
                while context.iteration(false) {}
                std::thread::sleep(std::time::Duration::from_millis(8));
            }
        };
        let hit_first_cell = || {
            let screen = terminal.pane.screen();
            let buffer = screen.buffer();
            let iter = buffer.start_iter();
            let area = screen.iter_location(&iter);
            let (x, y) = screen.buffer_to_window_coords(
                gtk4::TextWindowType::Widget,
                area.x() + area.width() / 4,
                area.y() + area.height() / 2,
            );
            view.source_at(f64::from(x), f64::from(y))
        };
        next_frames();
        assert_eq!(hit_first_cell(), Some(zone.prompt.start));
        let painted_heading = |id| {
            view.painted_zone_rows()
                .into_iter()
                .find(|row| row.id == id && row.heading && row.completed)
        };
        let heading = painted_heading(7).unwrap();
        assert_eq!(heading.row, 0);
        assert_eq!(heading.blank_tail, Some(7));

        terminal.resize(6, 4).unwrap();
        assert!(hit_first_cell().is_none());
        assert!(painted_heading(7).is_none());
        assert!(view.reveal_zone(7));
        next_frames();
        assert_eq!(hit_first_cell(), Some(zone.prompt.start));
        let heading = painted_heading(7).unwrap();
        assert_eq!(heading.row, 0);
        assert_eq!(heading.blank_tail, Some(6));
        output.apply(b"\x1b[3J").unwrap();
        assert!(view.source_zone(7).is_none());
        assert!(!view.reveal_zone(7));
        assert!(painted_heading(7).is_none());

        terminal.resize(12, 4).unwrap();
        output.apply(b"\r\n").unwrap();
        view.begin_restored_prompt(8);
        output.apply(b"prompt> ").unwrap();
        view.end_prompt();
        output.apply(b"yes\r\n").unwrap();
        view.begin_output(Some(8));
        output
            .apply(b"one\r\ntwo\r\nthree\r\nfour\r\nclipped row")
            .unwrap();
        view.end_output(Some(8), 64);
        let restored = view.source_zone(8).unwrap();
        assert_eq!(restored.origin, TranscriptZoneOrigin::Restored);
        terminal.resize(6, 4).unwrap();
        assert!(view.source_zone(8).is_none());
        assert!(view.output_text(8).is_none());
        assert!(view.reveal_zone(8));
        next_frames();
        assert_eq!(hit_first_cell(), Some(restored.prompt.start));
        assert_eq!(painted_heading(8).unwrap().row, 0);
        window.close();
    }
}
