use gtk4::{gdk, glib, graphene, pango, prelude::*, subclass::prelude::*};
use jterm_engine::{
    colors::ColorState,
    frame::{CellFrame, Cursor, Row},
    grid::{Color, CursorShape, TerminalCell, UnderlineStyle},
    history_reflow::HistoryLayout,
    hyperlink::HyperlinkId,
    terminal::HistoryPage,
    UnderlineColorRun,
};
use jterm_runtime::viewport::HistoryViewport;
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    ops::Range,
    rc::{Rc, Weak},
    sync::Arc,
};

use crate::pane::{PlainLinkFinder, Theme};

const MARGIN: i32 = 6;

#[derive(Clone, Copy, PartialEq)]
struct CursorStroke {
    offset: i32,
    shape: CursorShape,
    color: gdk::RGBA,
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct CursorTextView {
        pub(super) stroke: Cell<Option<CursorStroke>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for CursorTextView {
        const NAME: &'static str = "JtermCursorTextView";
        type Type = super::CursorTextView;
        type ParentType = gtk4::TextView;
    }

    impl ObjectImpl for CursorTextView {}
    impl WidgetImpl for CursorTextView {}

    impl TextViewImpl for CursorTextView {
        fn snapshot_layer(&self, layer: gtk4::TextViewLayer, snapshot: gtk4::Snapshot) {
            let Some(stroke) = self
                .stroke
                .get()
                .filter(|_| layer == gtk4::TextViewLayer::AboveText)
            else {
                self.parent_snapshot_layer(layer, snapshot);
                return;
            };
            self.parent_snapshot_layer(layer, snapshot.clone());
            let view = self.obj();
            let iter = view.buffer().iter_at_offset(stroke.offset);
            let area = view.iter_location(&iter);
            let (x, y) = (area.x() as f32, area.y() as f32);
            let (width, height) = (area.width().max(1) as f32, area.height().max(1) as f32);
            let bounds = match stroke.shape {
                CursorShape::Beam => graphene::Rect::new(x, y, width.min(2.0), height),
                CursorShape::Underline => {
                    let thickness = height.min(2.0);
                    graphene::Rect::new(x, y + height - thickness, width, thickness)
                }
                CursorShape::Block => return,
            };
            snapshot.append_color(&stroke.color, &bounds);
        }
    }
}

glib::wrapper! {
    pub struct CursorTextView(ObjectSubclass<imp::CursorTextView>)
        @extends gtk4::TextView, gtk4::Widget,
        @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget, gtk4::Scrollable;
}

impl CursorTextView {
    fn set_cursor_stroke(&self, stroke: Option<CursorStroke>) {
        if self.imp().stroke.replace(stroke) != stroke {
            self.queue_draw();
        }
    }
}

/// GTK presentation of an immutable live or historical viewport. The widget owns selection
/// and clipboard state; the session owns the grid and every terminal mode.
pub(crate) struct TerminalView {
    pub(crate) widget: CursorTextView,
    preedit: gtk4::Label,
    cursor_offset: Cell<Option<i32>>,
    presentation: Rc<RefCell<Option<Presentation>>>,
    find_tags: RefCell<Option<[gtk4::TextTag; 2]>>,
    theme: RefCell<Theme>,
    background_style: gtk4::CssProvider,
    background: Cell<Option<gdk::RGBA>>,
    line_height_scale: Cell<f64>,
    font_geometry: RefCell<Option<CachedFontGeometry>>,
    appearance_changed: Cell<bool>,
    palette_revision: Cell<u64>,
}

#[derive(Clone, Copy)]
struct FontGeometry {
    cell_width: i32,
    cell_height: i32,
    #[cfg(target_os = "linux")]
    space_advance: f64,
    #[cfg(target_os = "linux")]
    text_height: i32,
}

struct CachedFontGeometry {
    context: pango::Context,
    serial: u32,
    dimensions: FontGeometry,
}

struct Presentation {
    buffer: gtk4::TextBuffer,
    tags: PaintTags,
    rows: Vec<PresentedRow>,
    live: Option<(CellFrame, u128)>,
    history: Option<HistoryPosition>,
}

struct HistoryPosition {
    layout: std::sync::Weak<HistoryLayout>,
    first: usize,
}

#[derive(Clone, Copy)]
struct ViewportRows<'a> {
    frame: &'a CellFrame,
    margin_gaps: u128,
    history: Option<(&'a HistoryViewport, &'a HistoryPage)>,
}

impl<'a> ViewportRows<'a> {
    fn archived_rows(self) -> usize {
        self.history.map_or(0, |(_, page)| page.row_count)
    }

    fn row(self, index: usize) -> RowSource<'a> {
        if let Some((_, page)) = self.history.filter(|(_, page)| index < page.row_count) {
            RowSource::History(page, index)
        } else {
            RowSource::Live(self.frame, index - self.archived_rows(), self.margin_gaps)
        }
    }

    fn caret(self, index: usize) -> Option<Cursor> {
        let cursor = self.frame.cursor();
        (self.history.is_none() && cursor.paint_position().0 == index).then_some(cursor)
    }

    fn live_snapshot(self) -> Option<(CellFrame, u128)> {
        (self.archived_rows() < self.frame.rows().len())
            .then(|| (self.frame.clone(), self.margin_gaps))
    }

    fn history_position(self) -> Option<HistoryPosition> {
        self.history.map(|(history, _)| HistoryPosition {
            layout: Arc::downgrade(&history.layout),
            first: history.first,
        })
    }
}

type LiveSelection = Weak<RefCell<Option<Presentation>>>;

thread_local! {
    // The adapter stays associated with this GTK object even when the host
    // renames its widget. The entry neither owns the widget nor keeps frames alive.
    static LIVE_SELECTIONS: RefCell<HashMap<*mut gtk4::ffi::GtkTextView, LiveSelection>> =
        RefCell::default();
}

fn live_selection(view: &gtk4::TextView) -> Option<Rc<RefCell<Option<Presentation>>>> {
    LIVE_SELECTIONS.with(|entries| entries.borrow().get(&view.as_ptr())?.upgrade())
}

/// Identify a live terminal or a finished command or output text surface.
pub fn is_terminal_text_view(widget: &gtk4::Widget) -> bool {
    widget.downcast_ref::<gtk4::TextView>().is_some_and(|view| {
        !view.is_editable()
            && (live_selection(view).is_some() || crate::command::is_finished_text_view(view))
    })
}

/// Read an explicit selection with the same logical line breaks as terminal copy.
pub fn terminal_view_selection(widget: &gtk4::Widget) -> Option<String> {
    let view = widget.downcast_ref::<gtk4::TextView>()?;
    if view.is_editable() {
        return None;
    }
    if let Some(live) = live_selection(view) {
        let shown = live.borrow();
        let current = shown
            .as_ref()
            .filter(|shown| shown.buffer == view.buffer())?;
        return selected_text(view, Some(current));
    }
    if !crate::command::is_finished_text_view(view) {
        return None;
    }
    let buffer = view.buffer();
    let (start, end) = buffer.selection_bounds()?;
    Some(buffer.text(&start, &end, true).to_string())
}

impl Drop for TerminalView {
    fn drop(&mut self) {
        let _ = LIVE_SELECTIONS.try_with(|entries| {
            entries
                .borrow_mut()
                .remove(&self.widget.upcast_ref::<gtk4::TextView>().as_ptr());
        });
    }
}

struct PresentedRow {
    tags: Vec<Rc<StyleTag>>,
    links: Vec<PresentedLink>,
    column_spans: Vec<ColumnSpan>,
    copy: CopyRow,
    line_separators: Vec<(usize, char)>,
    cursor: Option<i32>,
    blank_tail: Option<usize>,
}

#[derive(Clone, Copy, Default)]
struct CopyRow {
    wrapped: bool,
    margin_gap: Option<usize>,
}

#[derive(Clone, Copy)]
pub(crate) enum RowSource<'a> {
    Live(&'a CellFrame, usize, u128),
    History(&'a HistoryPage, usize),
}

impl<'a> RowSource<'a> {
    pub(crate) fn row(self) -> Option<&'a Row> {
        match self {
            Self::Live(frame, index, _) => frame.rows().get(index),
            Self::History(page, index) => page.row(index),
        }
    }

    fn has_image(self) -> bool {
        match self {
            Self::Live(frame, index, _) => frame.inline_image_rows(index).next().is_some(),
            Self::History(page, index) => page.inline_image_rows(index).next().is_some(),
        }
    }

    pub(crate) fn margin_gap(self) -> Option<usize> {
        match self {
            Self::Live(frame, index, gaps) => (gaps & (1 << index) != 0).then(|| frame.cols() - 1),
            Self::History(page, index) => page.synthetic_margin_gap(index),
        }
    }

    fn underline_colors(self) -> &'a [UnderlineColorRun] {
        match self {
            Self::Live(frame, index, _) => frame.underline_color_runs(index),
            Self::History(page, index) => page.underline_color_runs(index),
        }
    }

    fn cluster(self, column: usize) -> Option<&'a str> {
        match self {
            Self::Live(frame, index, _) => frame.cluster_text(index, column),
            Self::History(page, index) => page.cluster_text(index, column),
        }
    }

    pub(crate) fn character_count(self, column: usize) -> i32 {
        self.cluster(column)
            .map_or(1, |cluster| cluster.chars().count() as i32)
    }

    pub(crate) fn push_text(self, text: &mut String, column: usize, character: char) -> i32 {
        if let Some(cluster) = self.cluster(column) {
            text.push_str(cluster);
            cluster.chars().count() as i32
        } else {
            text.push(character);
            1
        }
    }

    fn target(self, id: HyperlinkId) -> Option<&'a str> {
        match self {
            Self::Live(frame, _, _) => frame.hyperlink_target(id),
            Self::History(page, _) => page.hyperlink_target(id),
        }
    }
}

struct ColumnSpan {
    start: i32,
    end: i32,
    column: i32,
    width: i32,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct PresentedLink {
    id: HyperlinkId,
    start: i32,
    end: i32,
    pub(crate) target: Rc<str>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Style {
    foreground: gdk::RGBA,
    background: gdk::RGBA,
    bold: bool,
    italic: bool,
    underline: pango::Underline,
    underline_color: Option<gdk::RGBA>,
    strikethrough: bool,
    overline: bool,
}

struct StyleTag {
    style: Style,
    tag: gtk4::TextTag,
}

struct PaintTags {
    font: gtk4::TextTag,
    styles: HashMap<Style, Rc<StyleTag>>,
}

struct PaintTheme<'a> {
    configured: &'a Theme,
    dynamic: Option<&'a ColorState>,
}

impl PaintTheme<'_> {
    fn foreground(&self) -> gdk::RGBA {
        self.dynamic
            .and_then(ColorState::foreground_override)
            .map_or(self.configured.foreground, |(r, g, b)| rgba(r, g, b))
    }

    fn background(&self) -> gdk::RGBA {
        self.dynamic
            .and_then(ColorState::background_override)
            .map_or(self.configured.background, |(r, g, b)| rgba(r, g, b))
    }

    fn cursor(&self) -> Option<gdk::RGBA> {
        self.dynamic
            .and_then(ColorState::cursor_override)
            .map(|(r, g, b)| rgba(r, g, b))
            .or_else(|| self.configured.cursor.map(|cursor| cursor.background))
    }

    fn color(&self, color: Color, default: gdk::RGBA) -> gdk::RGBA {
        resolve_terminal_color(color, default, &self.configured.palette, self.dynamic)
    }
}

impl PresentedLink {
    pub(crate) fn text_span(start: i32, end: i32, target: &str) -> Self {
        Self {
            id: HyperlinkId::NONE,
            start,
            end,
            target: Rc::from(target),
        }
    }
}

impl TerminalView {
    pub(crate) fn new(theme: Theme) -> Self {
        let buffer = gtk4::TextBuffer::builder().enable_undo(false).build();
        let widget = glib::Object::builder::<CursorTextView>()
            .property("buffer", &buffer)
            .property("editable", false)
            .property("cursor-visible", false)
            .property("monospace", true)
            .property("wrap-mode", gtk4::WrapMode::None)
            .property("left-margin", MARGIN)
            .property("right-margin", MARGIN)
            .property("top-margin", MARGIN)
            .property("bottom-margin", MARGIN)
            .property("hexpand", true)
            .property("vexpand", true)
            .build();
        widget.set_widget_name("shared-terminal-screen");
        let background_style = gtk4::CssProvider::new();
        #[allow(deprecated)]
        widget
            .style_context()
            .add_provider(&background_style, gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION);
        widget.update_property(&[gtk4::accessible::Property::Label("Shared terminal screen")]);
        let preedit = gtk4::Label::builder()
            .can_target(false)
            .halign(gtk4::Align::Start)
            .valign(gtk4::Align::Start)
            .visible(false)
            .build();
        preedit.update_property(&[gtk4::accessible::Property::Description(
            "Terminal composition",
        )]);
        let background = theme.background;
        let view = Self {
            widget,
            preedit,
            cursor_offset: Cell::new(None),
            presentation: Rc::default(),
            find_tags: RefCell::default(),
            theme: RefCell::new(theme),
            background_style,
            background: Cell::new(None),
            line_height_scale: Cell::new(1.0),
            font_geometry: RefCell::new(None),
            appearance_changed: Cell::new(false),
            palette_revision: Cell::new(0),
        };
        LIVE_SELECTIONS.with(|entries| {
            entries.borrow_mut().insert(
                view.widget.upcast_ref::<gtk4::TextView>().as_ptr(),
                Rc::downgrade(&view.presentation),
            );
        });
        view.paint_background(background);
        view
    }

    fn paint_background(&self, background: gdk::RGBA) {
        if self.background.replace(Some(background)) != Some(background) {
            // Text tags color cells. The view also owns the paper around
            // them, including the margins and the area below the last row.
            self.background_style.load_from_string(&format!(
                "textview {{ background-color: {background}; }} text {{ background-color: transparent; }}"
            ));
        }
    }

    pub(crate) fn clear(&self) {
        let previous = self.presentation.take();
        self.cursor_offset.set(None);
        self.widget.set_cursor_stroke(None);
        self.widget.buffer().set_text("");
        self.clear_find();
        if let Some(previous) = previous {
            previous.remove_tags();
        }
    }

    pub(crate) fn theme(&self) -> Theme {
        self.theme.borrow().clone()
    }

    pub(crate) fn set_theme(&self, theme: Theme) {
        if self.theme.borrow().font != theme.font {
            self.font_geometry.take();
        }
        self.theme.replace(theme);
        self.appearance_changed.set(true);
        self.update_line_spacing();
    }

    pub(crate) fn set_font(&self, font: &pango::FontDescription) {
        self.theme.borrow_mut().font = font.clone();
        self.font_geometry.take();
        self.appearance_changed.set(true);
        self.update_line_spacing();
    }

    pub(crate) fn set_line_height_scale(&self, scale: f64) {
        if scale.is_finite() {
            self.line_height_scale.set(scale.clamp(1.0, 2.0));
            self.update_line_spacing();
        }
    }

    fn update_line_spacing(&self) {
        let (_, _, padding) = self.cell_geometry();
        self.widget.set_pixels_above_lines(padding / 2);
        self.widget.set_pixels_below_lines(padding - padding / 2);
    }

    pub(crate) fn dimensions(&self, width: i32, height: i32) -> (usize, usize) {
        let (cell_width, cell_height, _) = self.cell_geometry();
        fitted_grid(width, height, f64::from(cell_width), cell_height)
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn measured_allocation(
        &self,
        width: i32,
        height: i32,
    ) -> ((usize, usize), Option<(u16, u16)>) {
        let font = self.font_geometry();
        let cell_height = self.scaled_cell_height(font.cell_height);
        let text_height = font.text_height.saturating_add(cell_height - font.cell_height);
        let (rows, columns) = fitted_grid(
            width,
            height,
            font.space_advance.max(f64::from(font.cell_width)),
            text_height.max(cell_height),
        );
        let pixels = self.physical_viewport_pixels(
            columns as f64 * font.space_advance,
            rows as f64 * f64::from(text_height),
        );
        ((rows, columns), pixels)
    }

    #[cfg(target_os = "linux")]
    fn physical_viewport_pixels(&self, width: f64, height: f64) -> Option<(u16, u16)> {
        // GDK also exposes an integer scale factor for buffers. Use the
        // surface's actual scale for fractional-scale output.
        let scale = self.widget.native()?.surface()?.scale();
        let physical = |points: f64| {
            let pixels = (points * scale).round();
            (1.0..=f64::from(u16::MAX))
                .contains(&pixels)
                .then_some(pixels as u16)
        };
        Some((physical(width)?, physical(height)?))
    }

    fn cell_geometry(&self) -> (i32, i32, i32) {
        let font = self.font_geometry();
        let cell_height = self.scaled_cell_height(font.cell_height);
        (font.cell_width, cell_height, cell_height - font.cell_height)
    }

    fn scaled_cell_height(&self, height: i32) -> i32 {
        (f64::from(height) * self.line_height_scale.get()).round() as i32
    }

    fn font_geometry(&self) -> FontGeometry {
        let context = self.widget.pango_context();
        let serial = context.serial();
        if let Some(cached) = self
            .font_geometry
            .borrow()
            .as_ref()
            .filter(|cached| cached.context == context && cached.serial == serial)
        {
            return cached.dimensions;
        }
        let theme = self.theme.borrow();
        let metrics = context.metrics(Some(&theme.font), None);
        let cell_width = font_row_height(metrics.approximate_digit_width());
        let cell_height = font_row_height(metrics.height());
        // GTK draws blank cells with the configured font's space advance
        // and rounds each line's height. The integer grid estimates can
        // differ from those measurements, especially for fractional advances.
        #[cfg(target_os = "linux")]
        let (space_advance, text_height) = {
            let layout = pango::Layout::new(&context);
            layout.set_font_description(Some(&theme.font));
            layout.set_text(" ");
            let (advance, height) = layout.size();
            (
                if advance > 0 {
                    f64::from(advance) / f64::from(pango::SCALE)
                } else {
                    f64::from(cell_width)
                },
                (f64::from(height) / f64::from(pango::SCALE)).round().max(1.0) as i32,
            )
        };
        let dimensions = FontGeometry {
            cell_width,
            cell_height,
            #[cfg(target_os = "linux")]
            space_advance,
            #[cfg(target_os = "linux")]
            text_height,
        };
        self.font_geometry.replace(Some(CachedFontGeometry {
            serial: context.serial(),
            context,
            dimensions,
        }));
        dimensions
    }

    pub(crate) fn cell_metrics(&self) -> (i32, i32, i32, i32) {
        let (width, height, _) = self.cell_geometry();
        (MARGIN, MARGIN, width, height)
    }

    pub(crate) fn row_height(&self) -> f64 {
        f64::from(self.cell_geometry().1)
    }

    pub(crate) fn event_point(&self, event: &gdk::Event) -> Option<(f64, f64)> {
        let (x, y) = event.position()?;
        let native = self.widget.native()?;
        if event.surface() != native.surface() {
            return None;
        }
        let (dx, dy) = native.surface_transform();
        // Match GTK event dispatch for client-side window decorations.
        native
            .dynamic_cast::<gtk4::Widget>()
            .ok()?
            .compute_point(
                &self.widget,
                &gtk4::graphene::Point::new((x - dx) as f32, (y - dy) as f32),
            )
            .map(|point| (f64::from(point.x()), f64::from(point.y())))
    }

    pub(crate) fn cell_at(
        &self,
        (x, y): (f64, f64),
        (rows, columns): (usize, usize),
        clamp: bool,
    ) -> Option<(usize, usize)> {
        let (x, y) = self.widget.window_to_buffer_coords(
            gtk4::TextWindowType::Widget,
            x.floor() as i32,
            y.floor() as i32,
        );
        if let Some((col, row)) = self.text_cell_at(x, y) {
            if row < rows && col < columns {
                return Some((col, row));
            }
        }
        let (width, height, _) = self.cell_geometry();
        let col = (x - MARGIN).div_euclid(width);
        let row = (y - MARGIN).div_euclid(height);
        if clamp {
            Some((
                col.clamp(0, columns as i32 - 1) as usize,
                row.clamp(0, rows as i32 - 1) as usize,
            ))
        } else if (0..columns as i32).contains(&col) && (0..rows as i32).contains(&row) {
            Some((col as usize, row as usize))
        } else {
            None
        }
    }

    pub(crate) fn text_cell_at(&self, x: i32, y: i32) -> Option<(usize, usize)> {
        let (iter, _) = self.widget.iter_at_position(x, y)?;
        let row = iter.line() as usize;
        let offset = iter.line_offset();
        let presented = self.presentation.borrow();
        let spans = &presented.as_ref()?.rows.get(row)?.column_spans;
        let mut adjustment = 0;
        for span in spans {
            if offset < span.start {
                break;
            }
            if offset < span.end {
                let area = self.widget.iter_location(&iter);
                let second_half = span.width == 2 && x >= area.x() + area.width() / 2;
                return Some(((span.column + i32::from(second_half)) as usize, row));
            }
            adjustment = span.column + span.width - span.end;
        }
        Some(((offset + adjustment) as usize, row))
    }

    pub(crate) fn attach_preedit(&self, viewport: &gtk4::Overlay) {
        viewport.add_overlay(&self.preedit);
        viewport.set_clip_overlay(&self.preedit, true);
    }

    pub(crate) fn present(
        &self,
        frame: &CellFrame,
        margin_gaps: u128,
        history: Option<&HistoryViewport>,
        colors: Option<&(u64, Arc<ColorState>)>,
    ) {
        let history = match history {
            Some(history) => {
                let Some(page) = &history.page else { return };
                Some((history, page))
            }
            None => None,
        };
        let viewport = ViewportRows {
            frame,
            margin_gaps,
            history,
        };
        self.clear_find();
        let revision = colors.map_or(0, |(revision, _)| *revision);
        let same_palette = self.palette_revision.replace(revision) == revision;
        let configured = self.theme.borrow().clone();
        let theme = PaintTheme {
            configured: &configured,
            dynamic: colors.map(|(_, palette)| palette.as_ref()),
        };
        self.paint_background(theme.background());
        let mut previous = self.presentation.take();
        let same_appearance = !self.appearance_changed.replace(false) && same_palette;
        let buffer = self.widget.buffer();
        let current = previous
            .take_if(|previous| {
                same_appearance
                    && previous.buffer == buffer
                    && buffer.line_count() as usize == frame.rows().len()
                    && previous.update(viewport, &theme)
            })
            .unwrap_or_else(|| {
                // GTK's input-method callback can retain line layout across a
                // paint. Editing the mounted buffer lets GTK invalidate it.
                buffer.set_text("");
                if let Some(previous) = previous {
                    previous.remove_tags();
                }
                Presentation::new(buffer, viewport, &theme)
            });
        let caret = current.rows.iter().enumerate().find_map(|(row, painted)| {
            current
                .buffer
                .iter_at_line_offset(row as i32, painted.cursor?)
        });
        self.cursor_offset
            .set(caret.as_ref().map(gtk4::TextIter::offset));
        let cursor = frame.cursor();
        let stroke = caret.as_ref().and_then(|iter| {
            if history.is_some() || !cursor.visible || cursor.shape == CursorShape::Block {
                return None;
            }
            let (row, column) = cursor.paint_position();
            let cells = &frame.rows()[row].cells;
            // The text iterator and cursor both anchor at the lead of a wide glyph.
            let column = column - usize::from(cells[column].flags.wide_continuation());
            let color = theme
                .cursor()
                .unwrap_or_else(|| Self::style(&theme, &cells[column], None, false).foreground);
            Some(CursorStroke {
                offset: iter.offset(),
                shape: cursor.shape,
                color,
            })
        });
        self.widget.set_cursor_stroke(stroke);
        if let Some(caret) = caret {
            if current.buffer.iter_at_mark(&current.buffer.get_insert()) != caret {
                current.buffer.place_cursor(&caret);
            }
        }
        self.presentation.replace(Some(current));
    }

    pub(crate) fn painted_frame(&self) -> Option<CellFrame> {
        let presentation = self.presentation.borrow();
        let presentation = presentation.as_ref()?;
        if presentation.history.is_some() {
            return None;
        }
        let (frame, _) = presentation.live.as_ref()?;
        Some(frame.clone())
    }

    pub(crate) fn link_at(
        &self,
        x: f64,
        y: f64,
        find_plain: Option<PlainLinkFinder>,
    ) -> Option<PresentedLink> {
        let widget = self.widget.upcast_ref();
        let iter = text_at_point(widget, x, y)?;
        let shown = self.presentation.borrow();
        let shown = shown.as_ref()?;
        let row = shown.rows.get(iter.line() as usize)?;
        link_for_offset(&row.links, iter.line_offset()).or_else(|| {
            plain_link_at(widget, &iter, find_plain?, |line| {
                shown
                    .rows
                    .get(line)
                    .map_or_else(CopyRow::default, |row| row.copy)
            })
        })
    }

    pub(crate) fn blank_tail(&self, row: usize) -> Option<usize> {
        self.presentation
            .borrow()
            .as_ref()?
            .rows
            .get(row)?
            .blank_tail
    }

    pub(crate) fn clear_find(&self) {
        if let Some(tags) = self.find_tags.take() {
            let table = self.widget.buffer().tag_table();
            for tag in tags.iter().rev() {
                table.remove(tag);
            }
        }
    }

    pub(crate) fn paint_find(&self, top: usize, hits: &[crate::FindMatch], current: Option<usize>) {
        self.clear_find();
        let buffer = self.widget.buffer();
        let bottom = top + buffer.line_count() as usize;
        let table = buffer.tag_table();
        let tags = [
            gtk4::TextTag::builder()
                .name("jterm-screen-find-match")
                .background("#e5ca75")
                .foreground("#201a00")
                .build(),
            gtk4::TextTag::builder()
                .name("jterm-screen-find-current")
                .background("#ffa842")
                .foreground("#201a00")
                .build(),
        ];
        for tag in &tags {
            table.add(tag);
        }
        for (index, hit) in hits.iter().enumerate() {
            if hit.end.row < top || hit.start.row >= bottom {
                continue;
            }
            let start = if hit.start.row < top {
                Some(buffer.start_iter())
            } else {
                buffer.iter_at_line_offset((hit.start.row - top) as i32, hit.start.offset)
            };
            let end = if hit.end.row >= bottom {
                Some(buffer.end_iter())
            } else {
                buffer.iter_at_line_offset((hit.end.row - top) as i32, hit.end.offset)
            };
            if let (Some(start), Some(end)) = (start, end) {
                buffer.apply_tag(&tags[usize::from(Some(index) == current)], &start, &end);
            }
        }
        self.find_tags.replace(Some(tags));
    }

    pub(crate) fn selected_text(&self) -> Option<String> {
        selected_text(
            self.widget.upcast_ref(),
            self.presentation.borrow().as_ref(),
        )
    }

    pub(crate) fn show_preedit(&self, text: &str, attributes: pango::AttrList) {
        let theme = self.theme.borrow();
        attributes.insert_before(pango::AttrFontDesc::new(&theme.font));
        attributes.insert_before(pango::AttrInt::new_underline(pango::Underline::Single));
        let [red, green, blue] = rgb16(theme.background);
        attributes.insert_before(pango::AttrColor::new_background(red, green, blue));
        let [red, green, blue] = rgb16(theme.foreground);
        attributes.insert_before(pango::AttrColor::new_foreground(red, green, blue));
        drop(theme);
        self.preedit.set_attributes(Some(&attributes));
        self.preedit.set_text(text);
    }

    pub(crate) fn clear_preedit(&self) {
        self.preedit.set_visible(false);
        self.preedit.set_text("");
    }

    pub(crate) fn input_cursor(&self) -> Option<gdk::Rectangle> {
        let Some(offset) = self.cursor_offset.get() else {
            self.preedit.set_visible(false);
            return None;
        };
        let iter = self.widget.buffer().iter_at_offset(offset);
        let area = self.widget.iter_location(&iter);
        let (x, y) =
            self.widget
                .buffer_to_window_coords(gtk4::TextWindowType::Widget, area.x(), area.y());
        if !self.preedit.text().is_empty() {
            if let Some(point) = self.preedit.parent().and_then(|plane| {
                self.widget
                    .compute_point(&plane, &gtk4::graphene::Point::new(x as f32, y as f32))
            }) {
                self.preedit
                    .set_margin_start(point.x().round().max(0.0) as i32);
                self.preedit
                    .set_margin_top(point.y().round().max(0.0) as i32);
                self.preedit.set_visible(true);
            }
        }
        Some(gdk::Rectangle::new(
            x,
            y,
            area.width().max(1),
            area.height().max(1),
        ))
    }

    fn style(
        theme: &PaintTheme<'_>,
        cell: &TerminalCell,
        underline_color: Option<Color>,
        block_cursor: bool,
    ) -> Style {
        let mut foreground = theme.color(cell.foreground, theme.foreground());
        let mut background = theme.color(cell.background, theme.background());
        if cell.flags.dim() && !matches!(cell.foreground, Color::Rgb(..)) {
            foreground = faint_palette_color(foreground);
        }
        if cell.flags.inverse() {
            std::mem::swap(&mut foreground, &mut background);
            foreground.set_alpha(1.0);
        }
        if block_cursor {
            std::mem::swap(&mut foreground, &mut background);
            if let Some(cursor) = theme.configured.cursor {
                foreground = cursor.foreground;
            } else {
                foreground.set_alpha(1.0);
            }
            if let Some(cursor_color) = theme.cursor() {
                background = cursor_color;
            }
        }
        let concealed = cell.flags.concealed();
        if concealed {
            foreground = concealed_ink();
        }
        let underline = match (concealed, cell.flags.underline()) {
            (true, _) | (_, UnderlineStyle::None) => pango::Underline::None,
            (false, UnderlineStyle::Double) => pango::Underline::Double,
            (false, UnderlineStyle::Curly) => pango::Underline::Error,
            _ => pango::Underline::Single,
        };
        Style {
            foreground,
            background,
            bold: cell.flags.bold(),
            italic: cell.flags.italic(),
            underline,
            underline_color: underline_color
                .filter(|_| underline != pango::Underline::None)
                .map(|color| theme.color(color, foreground)),
            strikethrough: !concealed && cell.flags.strikethrough(),
            overline: !concealed && cell.flags.overline(),
        }
    }
}

pub(crate) fn text_at_point(view: &gtk4::TextView, x: f64, y: f64) -> Option<gtk4::TextIter> {
    let (x, y) = view.window_to_buffer_coords(
        gtk4::TextWindowType::Widget,
        x.floor() as i32,
        y.floor() as i32,
    );
    let (iter, _) = view.iter_at_position(x, y)?;
    let area = view.iter_location(&iter);
    (x >= area.x() && y >= area.y() && x < area.x() + area.width() && y < area.y() + area.height())
        .then_some(iter)
}

pub(crate) fn link_for_offset(links: &[PresentedLink], offset: i32) -> Option<PresentedLink> {
    let index = links.partition_point(|link| link.end <= offset);
    links
        .get(index)
        .filter(|link| link.start <= offset)
        .cloned()
}

fn plain_link_at(
    view: &gtk4::TextView,
    click: &gtk4::TextIter,
    find_plain: PlainLinkFinder,
    copy_row: impl Fn(usize) -> CopyRow,
) -> Option<PresentedLink> {
    let buffer = view.buffer();
    let mut first = click.line();
    let mut last = first;
    while first > 0 && copy_row((first - 1) as usize).wrapped {
        first -= 1;
    }
    while last + 1 < buffer.line_count() && copy_row(last as usize).wrapped {
        last += 1;
    }
    let start = buffer.iter_at_line(first)?;
    let end = buffer
        .iter_at_line(last + 1)
        .unwrap_or_else(|| buffer.end_iter());
    // Bound work on a long wrapped logical line before copying its text.
    if end.offset() - start.offset() > 8192 {
        return None;
    }
    let raw = buffer.text(&start, &end, false);
    let mut text = String::with_capacity(raw.len());
    let mut positions = Vec::new();
    let mut logical_click = None;
    let mut row = first as usize;
    let mut metadata = copy_row(row);
    let mut offset = 0;
    for (index, character) in raw.chars().enumerate() {
        let absolute = start.offset() + index as i32;
        let omit = if character == '\n' {
            let wrapped = metadata.wrapped;
            row += 1;
            metadata = copy_row(row);
            offset = 0;
            wrapped
        } else {
            let gap = metadata.margin_gap == Some(offset);
            offset += 1;
            gap
        };
        if omit {
            continue;
        }
        if absolute == click.offset() {
            logical_click = Some(text.len());
        }
        positions.push((text.len(), absolute));
        text.push(character);
    }
    positions.push((text.len(), end.offset()));
    let click = logical_click?;
    let target = find_plain(&text, click)?;
    if !target.contains(&click) {
        return None;
    }
    let position = |byte| {
        let index = positions
            .binary_search_by_key(&byte, |&(byte, _)| byte)
            .ok()?;
        Some(positions[index].1)
    };
    Some(PresentedLink::text_span(
        position(target.start)?,
        position(target.end)?,
        text.get(target)?,
    ))
}

fn selected_text(view: &gtk4::TextView, shown: Option<&Presentation>) -> Option<String> {
    let buffer = view.buffer();
    let (start, end) = buffer.selection_bounds()?;
    let selected = buffer.text(&start, &end, true);
    let rows = shown
        .filter(|shown| shown.buffer == buffer)
        .map_or(&[][..], |shown| shown.rows.as_slice());
    Some(logical_selection(
        &selected,
        start.line() as usize,
        start.line_offset() as usize,
        |index| {
            rows.get(index).map_or_else(
                || (CopyRow::default(), &[][..]),
                |row| (row.copy, row.line_separators.as_slice()),
            )
        },
    ))
}

fn logical_selection<'a>(
    selected: &str,
    first_row: usize,
    first_offset: usize,
    copy_row: impl Fn(usize) -> (CopyRow, &'a [(usize, char)]),
) -> String {
    let mut text = String::with_capacity(selected.len());
    for (index, physical_row) in selected.split_inclusive('\n').enumerate() {
        let (row, separators) = copy_row(first_row + index);
        let physical_row = if row.wrapped {
            physical_row.strip_suffix('\n').unwrap_or(physical_row)
        } else {
            physical_row
        };
        let start = if index == 0 { first_offset } else { 0 };
        if !separators.is_empty() {
            let first = separators.partition_point(|&(column, _)| column < start);
            let mut separators = separators[first..].iter().peekable();
            for (index, displayed) in physical_row.chars().enumerate() {
                let column = start + index;
                let character = separators
                    .next_if(|&&(offset, _)| offset == column)
                    .map_or(displayed, |&(_, character)| character);
                if row.margin_gap != Some(column) || character != ' ' {
                    text.push(character);
                }
            }
            continue;
        }
        let gap = row
            .margin_gap
            .and_then(|column| column.checked_sub(start))
            .and_then(|offset| physical_row.char_indices().nth(offset))
            .filter(|&(_, character)| character == ' ');
        if let Some((offset, _)) = gap {
            text.push_str(&physical_row[..offset]);
            text.push_str(&physical_row[offset + 1..]);
        } else {
            text.push_str(physical_row);
        }
    }
    text
}

#[cfg(test)]
#[test]
fn copy_joins_soft_wraps_and_keeps_explicit_newlines_and_spaces() {
    let selected = "👩‍💻- \n  continuation\nnext";
    assert_eq!(
        logical_selection(selected, 1, 4, |row| {
            (
                CopyRow {
                    wrapped: row == 1,
                    margin_gap: None,
                },
                &[],
            )
        }),
        "👩‍💻-   continuation\nnext"
    );
    assert_eq!(
        logical_selection(selected, 1, 4, |row| {
            (
                CopyRow {
                    wrapped: row == 1,
                    margin_gap: (row == 1).then_some(8),
                },
                &[],
            )
        }),
        "👩‍💻-  continuation\nnext"
    );
}

#[cfg(test)]
#[test]
#[ignore = "requires a GTK display"]
fn live_update_preserves_unchanged_buffer_rows_and_moves_the_caret() {
    gtk4::init().expect("GTK display");
    let mut terminal = jterm_engine::terminal::Terminal::new(4, 18).unwrap();
    terminal.enable_grapheme_clusters();
    let view = TerminalView::new(Theme {
        font: pango::FontDescription::from_string("Monospace 12"),
        foreground: gdk::RGBA::WHITE,
        background: gdk::RGBA::BLACK,
        palette: default_ansi_palette(),
        cursor: None,
    });
    terminal
        .feed(b"stable\x1b[2;1Hbefore\x1b[3;1Hcaret")
        .unwrap();
    view.present(
        &terminal.frame().unwrap(),
        terminal.live_margin_gaps(),
        None,
        None,
    );
    let first = view.widget.buffer();
    let window = gtk4::Window::builder()
        .default_width(420)
        .default_height(260)
        .child(&view.widget)
        .build();
    window.present();
    view.widget.grab_focus();
    let context = gtk4::glib::MainContext::default();
    while context.iteration(false) {}
    let unchanged_tags = first.start_iter().tags();
    let initial = first.text(&first.start_iter(), &first.end_iter(), true);
    assert_eq!(initial.lines().next().unwrap().trim_end(), "stable");
    assert!(initial.lines().nth(1).unwrap().starts_with("before"));

    terminal
        .feed("\x1b[2;1H\x1b[2Ke\u{301} 👩‍💻".as_bytes())
        .unwrap();
    view.present(
        &terminal.frame().unwrap(),
        terminal.live_margin_gaps(),
        None,
        None,
    );
    let edited = view.widget.buffer();
    let text = edited.text(&edited.start_iter(), &edited.end_iter(), true);
    assert_eq!(edited, first);
    assert_eq!(edited.start_iter().tags(), unchanged_tags);
    assert!(text.lines().nth(1).unwrap().starts_with("e\u{301} 👩‍💻"));
    assert!(text.lines().nth(2).unwrap().starts_with("caret"));
    let caret = edited.iter_at_mark(&edited.get_insert());
    assert_eq!((caret.line(), caret.line_offset()), (1, 6));

    terminal.feed(b"\x1b[?1049hother").unwrap();
    view.present(
        &terminal.frame().unwrap(),
        terminal.live_margin_gaps(),
        None,
        None,
    );
    while context.iteration(false) {}
    assert_eq!(view.widget.buffer(), first);
    let alternate = first.text(&first.start_iter(), &first.end_iter(), true);
    assert!(alternate.lines().next().unwrap().starts_with("other"));

    terminal.feed(b"\x1b[?1049l").unwrap();
    terminal.resize(6, 16).unwrap();
    view.present(
        &terminal.frame().unwrap(),
        terminal.live_margin_gaps(),
        None,
        None,
    );
    while context.iteration(false) {}
    assert_eq!(view.widget.buffer(), first);
    assert_eq!(first.line_count(), 6);
    let primary = first.text(&first.start_iter(), &first.end_iter(), true);
    assert!(primary.lines().next().unwrap().starts_with("stable"));
    window.close();
}

#[cfg(test)]
#[test]
#[ignore = "requires a GTK display"]
fn literal_unicode_separators_preserve_terminal_rows_and_copy_text() {
    gtk4::init().expect("GTK display");
    let mut terminal = jterm_engine::terminal::Terminal::new(4, 12).unwrap();
    terminal.enable_grapheme_clusters();
    let view = TerminalView::new(Theme {
        font: pango::FontDescription::from_string("Monospace 12"),
        foreground: gdk::RGBA::WHITE,
        background: gdk::RGBA::BLACK,
        palette: default_ansi_palette(),
        cursor: None,
    });
    let paint = |terminal: &jterm_engine::terminal::Terminal| {
        view.present(
            &terminal.frame().unwrap(),
            terminal.live_margin_gaps(),
            None,
            None,
        );
    };
    terminal
        .feed("A\u{2029}B\u{2028}C\r\nkeep\r\nthird".as_bytes())
        .unwrap();
    paint(&terminal);
    let buffer = view.widget.buffer();
    assert_eq!(buffer.line_count(), 4);
    assert!(buffer
        .text(&buffer.start_iter(), &buffer.end_iter(), true)
        .starts_with("A B C"));
    let select = |line, start, expected| {
        buffer.select_range(
            &buffer.iter_at_line_offset(line, start).unwrap(),
            &buffer.iter_at_line_offset(line, 5).unwrap(),
        );
        let text = terminal_view_selection(view.widget.upcast_ref());
        buffer.place_cursor(&buffer.start_iter());
        assert_eq!(text.as_deref(), Some(expected));
    };
    select(0, 1, "\u{2029}B\u{2028}C");
    select(0, 3, "\u{2028}C");
    terminal.feed(b"\x1b[2;1H!").unwrap();
    paint(&terminal);
    assert_eq!(buffer.iter_at_line(1).unwrap().char(), '!');
    assert_eq!(buffer.line_count(), 4);
    terminal.feed(b"\x1b[T").unwrap();
    paint(&terminal);
    assert_eq!(buffer.line_count(), 4);
    select(1, 1, "\u{2029}B\u{2028}C");
}

#[cfg(test)]
#[test]
#[ignore = "requires a GTK display"]
fn live_styles_share_tags_and_release_unused_colors() {
    gtk4::init().expect("GTK display");
    let mut terminal = jterm_engine::terminal::Terminal::new(4, 20).unwrap();
    let view = TerminalView::new(Theme {
        font: pango::FontDescription::from_string("Monospace 12"),
        foreground: gdk::RGBA::WHITE,
        background: gdk::RGBA::BLACK,
        palette: default_ansi_palette(),
        cursor: None,
    });
    let paint = |terminal: &jterm_engine::terminal::Terminal| {
        view.present(
            &terminal.frame().unwrap(),
            terminal.live_margin_gaps(),
            None,
            None,
        );
    };
    terminal
        .feed(b"\x1b[?25l\x1b[31mfirst\r\nsecond\x1b[0m")
        .unwrap();
    paint(&terminal);
    let buffer = view.widget.buffer();
    let table = buffer.tag_table();
    let foreground_tag = |row| {
        buffer
            .iter_at_line(row)
            .unwrap()
            .tags()
            .into_iter()
            .find(|tag| tag.foreground_rgba().is_some())
            .unwrap()
    };
    let red = foreground_tag(0);
    assert_eq!(red, foreground_tag(1));
    let initial_count = table.size();
    let added = Rc::new(Cell::new(0));
    let removed = Rc::new(Cell::new(0));
    table.connect_tag_added({
        let count = added.clone();
        move |_, _| count.set(count.get() + 1)
    });
    table.connect_tag_removed({
        let count = removed.clone();
        move |_, _| count.set(count.get() + 1)
    });
    terminal.feed(b"\x1b[1;1H\x1b[31mX\x1b[0m").unwrap();
    paint(&terminal);
    assert_eq!(red, foreground_tag(0));
    assert_eq!((added.get(), removed.get()), (0, 0));

    terminal
        .feed(b"\x1b[1;1H\x1b[2K\x1b[34mblue\x1b[0m")
        .unwrap();
    paint(&terminal);
    assert_eq!(red, foreground_tag(1));
    assert_eq!(table.size(), initial_count + 1);
    terminal
        .feed(b"\x1b[2;1H\x1b[2K\x1b[34malso blue\x1b[0m")
        .unwrap();
    paint(&terminal);
    assert_eq!(foreground_tag(0), foreground_tag(1));
    assert_eq!(table.size(), initial_count);
    assert_eq!(removed.get(), 1);

    for red_component in 0..64 {
        terminal
            .feed(format!("\x1b[1;1H\x1b[2K\x1b[38;2;{red_component};12;17mX\x1b[0m").as_bytes())
            .unwrap();
        paint(&terminal);
        assert_eq!(table.size(), initial_count + 1);
    }
    view.clear();
    assert_eq!(table.size(), 0);
}

#[cfg(test)]
#[test]
#[ignore = "requires a GTK display"]
fn scrolling_keeps_existing_text_tags_and_moves_copy_and_caret_metadata() {
    gtk4::init().expect("GTK display");
    let mut terminal = jterm_engine::terminal::Terminal::new(7, 12).unwrap();
    terminal.enable_underline_colors();
    terminal.enable_grapheme_clusters();
    terminal.enable_hyperlinks(|_, _| true);
    let theme = Theme {
        font: pango::FontDescription::from_string("Monospace 12"),
        foreground: gdk::RGBA::WHITE,
        background: gdk::RGBA::BLACK,
        palette: default_ansi_palette(),
        cursor: None,
    };
    let view = TerminalView::new(theme.clone());
    let window = gtk4::Window::builder().child(&view.widget).build();
    window.present();
    terminal
        .feed(
            concat!(
                "discard\r\n12345678901界Z\r\n",
                "\x1b]8;;https://example.test/moved\x07\x1b[4;58;5;33me\u{301}界link",
                "\x1b[0m\x1b]8;;\x07\r\nfour\r\nfive\r\nsix",
            )
            .as_bytes(),
        )
        .unwrap();
    let present = |view: &TerminalView, terminal: &jterm_engine::terminal::Terminal| {
        view.present(
            &terminal.frame().unwrap(),
            terminal.live_margin_gaps(),
            None,
            None,
        );
    };
    present(&view, &terminal);
    let buffer = view.widget.buffer();
    let stable = buffer.iter_at_line(1).unwrap().tags();
    let link_tags = buffer.iter_at_line(3).unwrap().tags();
    terminal.feed(b"\x1b[S\x1b[7;1H\x1b[31mnew\x1b[0m").unwrap();
    present(&view, &terminal);
    assert_eq!(view.widget.buffer(), buffer);
    assert_eq!(buffer.line_count(), 7);
    assert_eq!(buffer.start_iter().tags(), stable);
    assert_eq!(buffer.iter_at_line(2).unwrap().tags(), link_tags);
    let caret = buffer.iter_at_mark(&buffer.get_insert());
    assert_eq!((caret.line(), caret.line_offset()), (6, 3));
    let retained = view.presentation.borrow();
    let moved = &retained.as_ref().unwrap().rows[2];
    assert_eq!(moved.column_spans.len(), 2);
    assert_eq!(moved.links[0].target.as_ref(), "https://example.test/moved");
    drop(retained);
    while glib::MainContext::default().iteration(false) {}
    let link = view.widget.iter_location(&buffer.iter_at_line(2).unwrap());
    let (x, y) = view.widget.buffer_to_window_coords(
        gtk4::TextWindowType::Widget,
        link.x() + 1,
        link.y() + link.height() / 2,
    );
    assert_eq!(
        view.link_at(f64::from(x), f64::from(y), None)
            .unwrap()
            .target
            .as_ref(),
        "https://example.test/moved"
    );

    buffer.select_range(
        &buffer.iter_at_line_offset(0, 2).unwrap(),
        &buffer.iter_at_line_offset(1, 2).unwrap(),
    );
    assert_eq!(view.selected_text().as_deref(), Some("345678901界Z"));
    buffer.place_cursor(&caret);

    // Keep rows above and below an edited region while moving its survivors.
    terminal
        .feed(b"\x1b[3;6r\x1b[T\x1b[3;1H\x1b[34mreplacement\x1b[0m")
        .unwrap();
    present(&view, &terminal);
    let reference = TerminalView::new(theme);
    let expected = reference.widget.buffer();
    let rebuild_insertions = Rc::new(Cell::new(0));
    expected.connect_insert_text({
        let count = rebuild_insertions.clone();
        move |_, _, _| count.set(count.get() + 1)
    });
    present(&reference, &terminal);
    assert_eq!(rebuild_insertions.get(), 1);
    assert_eq!(
        buffer.text(&buffer.start_iter(), &buffer.end_iter(), true),
        expected.text(&expected.start_iter(), &expected.end_iter(), true)
    );
    let styles = |tags: Vec<gtk4::TextTag>| {
        tags.iter()
            .map(|tag| {
                (
                    tag.foreground_rgba(),
                    tag.background_rgba(),
                    tag.underline(),
                    tag.underline_rgba(),
                    tag.weight(),
                    tag.style(),
                    tag.is_strikethrough(),
                )
            })
            .collect::<Vec<_>>()
    };
    for offset in 0..buffer.char_count() {
        assert_eq!(
            styles(buffer.iter_at_offset(offset).tags()),
            styles(expected.iter_at_offset(offset).tags()),
            "character {offset}"
        );
    }

    let table = buffer.tag_table();
    let foreign = gtk4::TextTag::builder().name("host-owned-tag").build();
    table.add(&foreign);
    view.clear();
    assert_eq!(table.lookup("host-owned-tag"), Some(foreign));
    assert_eq!(table.size(), 1);
    window.close();
}

#[cfg(test)]
#[test]
#[ignore = "requires a GTK display"]
fn scrolling_history_keeps_projected_rows_and_refreshes_changed_live_rows() {
    gtk4::init().expect("GTK display");
    let mut terminal = jterm_engine::terminal::Terminal::new(5, 12).unwrap();
    terminal.enable_grapheme_clusters();
    terminal.enable_underline_colors();
    terminal.enable_hyperlinks(|_, _| true);
    terminal
        .feed(
            concat!(
                "old0\r\n\r\n",
                "\x1b]8;;https://example.test/archive\x07\x1b[4:3;58;5;33me\u{301}\u{2029}B界",
                "\x1b[0m\x1b]8;;\x07\r\nold3\r\nold4\r\nold5\r\nold6\r\nold7\r\n",
                "live0\r\nlive1\r\nlive2\r\nlive3\r\nlive4",
            )
            .as_bytes(),
        )
        .unwrap();
    let theme = Theme {
        font: pango::FontDescription::from_string("Monospace 12"),
        foreground: gdk::RGBA::WHITE,
        background: gdk::RGBA::BLACK,
        palette: default_ansi_palette(),
        cursor: None,
    };
    let view = TerminalView::new(theme.clone());
    let buffer = view.widget.buffer();
    // Keep the layout between captures as the pane's history rail does.
    let _layout = terminal.history_layout().unwrap();
    let paint = |view: &TerminalView, viewport: &crate::viewport::ViewportFrame| {
        view.present(
            &viewport.frame,
            viewport.live_margin_gaps,
            viewport.history.as_ref(),
            viewport.colors.as_ref(),
        );
    };
    let show = |terminal: &jterm_engine::terminal::Terminal, from_tail| {
        let viewport = capture_scrollback(terminal, from_tail);
        paint(&view, &viewport);
        let reference = TerminalView::new(theme.clone());
        paint(&reference, &viewport);
        let reference = reference.widget.buffer();
        let text =
            |buffer: &gtk4::TextBuffer| buffer.text(&buffer.start_iter(), &buffer.end_iter(), true);
        assert_eq!(text(&buffer), text(&reference));
        assert_eq!(buffer.line_count(), 5);
        assert!(view.painted_frame().is_none());
    };
    let tags = |line| buffer.iter_at_line(line).unwrap().tags();
    show(&terminal, 8);
    let decorated = tags(2);
    assert!(!decorated.is_empty());
    show(&terminal, 7);
    assert!(buffer.start_iter().ends_line());
    assert_eq!(tags(1), decorated);
    buffer.select_range(
        &buffer.iter_at_line(1).unwrap(),
        &buffer.iter_at_line_offset(1, 5).unwrap(),
    );
    assert_eq!(view.selected_text().as_deref(), Some("e\u{301}\u{2029}B界"));
    buffer.place_cursor(&buffer.start_iter());
    let presentation = view.presentation.borrow();
    let link = &presentation.as_ref().unwrap().rows[1].links[0];
    assert_eq!(link.target.as_ref(), "https://example.test/archive");
    drop(presentation);
    show(&terminal, 8);
    assert_eq!(tags(2), decorated);

    show(&terminal, 2);
    let mixed = tags(1);
    show(&terminal, 1);
    assert_eq!(tags(0), mixed);
    terminal.feed(b"\x1b[2;1H\x1b[31mnew\x1b[0m").unwrap();
    show(&terminal, 1);
    assert_eq!(tags(0), mixed);
    assert_eq!(buffer.iter_at_line(2).unwrap().char(), 'n');
    assert!(view.cursor_offset.get().is_none());

    terminal.feed(b"\x1b[5;1H\r\nlast").unwrap();
    show(&terminal, 1);
    assert_ne!(tags(0), mixed);
    view.present(&terminal.frame().unwrap(), 0, None, None);
    assert!(view.painted_frame().is_some());
    assert!(view.cursor_offset.get().is_some());
}

#[cfg(test)]
fn capture_scrollback(
    terminal: &jterm_engine::terminal::Terminal,
    from_tail: u64,
) -> crate::viewport::ViewportFrame {
    crate::viewport::ViewportRequest {
        top: Some(crate::viewport::ScrollTarget::FromTail(from_tail)),
        painted_history: None,
    }
    .capture(terminal, false)
    .unwrap()
    .unwrap()
}

#[cfg(test)]
#[test]
#[ignore = "requires a GTK display"]
fn conceal_keeps_terminal_text_and_suppresses_glyphs_and_decorations() {
    gtk4::init().expect("GTK display");
    let mut terminal = jterm_engine::terminal::Terminal::new(2, 12).unwrap();
    let view = TerminalView::new(Theme {
        font: pango::FontDescription::from_string("Monospace 12"),
        foreground: gdk::RGBA::WHITE,
        background: gdk::RGBA::BLACK,
        palette: default_ansi_palette(),
        cursor: None,
    });
    let window = gtk4::Window::builder()
        .default_width(320)
        .default_height(130)
        .child(&view.widget)
        .build();
    window.present();
    let context = glib::MainContext::default();
    let present = |terminal: &jterm_engine::terminal::Terminal| {
        view.present(
            &terminal.frame().unwrap(),
            terminal.live_margin_gaps(),
            None,
            None,
        );
        while context.iteration(false) {}
    };
    let ink_pixels = || {
        let snapshot = gtk4::Snapshot::new();
        window.snapshot_child(&view.widget, &snapshot);
        let node = snapshot.to_node().expect("painted terminal");
        let mut surface = gtk4::cairo::ImageSurface::create(
            gtk4::cairo::Format::ARgb32,
            window.width(),
            window.height(),
        )
        .unwrap();
        {
            let context = gtk4::cairo::Context::new(&surface).unwrap();
            node.draw(&context);
        }
        let pixels = surface.data().unwrap();
        pixels
            .chunks_exact(4)
            .filter(|pixel| pixel[..3].iter().any(|&value| value != 0))
            .count()
    };

    terminal.feed(b"\x1b[?25l").unwrap();
    present(&terminal);
    let blank_ink = ink_pixels();
    terminal.feed("\x1b[8;53;4;9mM🧪".as_bytes()).unwrap();
    present(&terminal);
    let buffer = view.widget.buffer();
    let text = buffer.text(&buffer.start_iter(), &buffer.end_iter(), true);
    assert!(text.starts_with("M🧪"));
    assert!(buffer.start_iter().tags().iter().any(|tag| {
        tag.foreground_rgba() == Some(concealed_ink())
            && tag.underline() == pango::Underline::None
            && tag.overline() == pango::Overline::None
            && !tag.is_strikethrough()
    }));
    assert_eq!(ink_pixels(), blank_ink);

    terminal.feed("\x1b[H\x1b[28mM🧪".as_bytes()).unwrap();
    present(&terminal);
    assert!(ink_pixels() > blank_ink);
    assert!(buffer
        .start_iter()
        .tags()
        .iter()
        .any(|tag| tag.overline() == pango::Overline::Single));
    window.close();
}

#[cfg(test)]
#[test]
#[ignore = "requires a GTK display"]
fn cursor_strokes_use_text_geometry_and_leave_the_caret_available() {
    gtk4::init().expect("GTK display");
    let mut terminal = jterm_engine::terminal::Terminal::new(3, 8).unwrap();
    let color = gdk::RGBA::new(0.1, 0.9, 0.4, 1.0);
    let view = TerminalView::new(Theme {
        font: pango::FontDescription::from_string("Monospace 12"),
        foreground: gdk::RGBA::WHITE,
        background: gdk::RGBA::BLACK,
        palette: default_ansi_palette(),
        cursor: Some(crate::pane::CursorColors {
            foreground: gdk::RGBA::BLACK,
            background: color,
        }),
    });
    let window = gtk4::Window::builder().child(&view.widget).build();
    window.present();
    let context = glib::MainContext::default();
    let present = |terminal: &jterm_engine::terminal::Terminal,
                   history: Option<&HistoryViewport>| {
        view.present(
            &terminal.frame().unwrap(),
            terminal.live_margin_gaps(),
            history,
            None,
        );
        while context.iteration(false) {}
    };
    let stroke = || {
        let snapshot = gtk4::Snapshot::new();
        view.widget
            .imp()
            .snapshot_layer(gtk4::TextViewLayer::AboveText, snapshot.clone());
        snapshot
            .to_node()
            .map(|node| node.downcast::<gtk4::gsk::ColorNode>().unwrap())
    };

    terminal
        .feed("\x1b[4:3m界\x1b[1;2H\x1b[6 q".as_bytes())
        .unwrap();
    present(&terminal, None);
    let buffer = view.widget.buffer();
    let area = view.widget.iter_location(&buffer.start_iter());
    assert_eq!(view.cursor_offset.get(), Some(0));
    let beam = stroke().unwrap();
    assert_eq!(beam.color(), color);
    assert_eq!(beam.bounds().x(), area.x() as f32);
    assert_eq!(beam.bounds().width(), 2.0);
    assert_eq!(beam.bounds().height(), area.height() as f32);

    terminal.feed(b"\x1b[4 q").unwrap();
    present(&terminal, None);
    let underline = stroke().unwrap();
    assert_eq!(underline.color(), color);
    assert_eq!(underline.bounds().width(), area.width() as f32);
    assert_eq!(underline.bounds().height(), 2.0);
    assert_eq!(
        underline.bounds().y() + 2.0,
        (area.y() + area.height()) as f32
    );

    terminal.feed(b"\x1b[?25l").unwrap();
    present(&terminal, None);
    assert!(stroke().is_none());
    assert_eq!(view.cursor_offset.get(), Some(0));
    terminal.feed(b"\x1b[?25h\r\nX\r\nX\r\nX").unwrap();
    let viewport = capture_scrollback(&terminal, 1);
    assert!(viewport.history.is_some());
    present(&terminal, viewport.history.as_ref());
    assert!(stroke().is_none());
    assert_eq!(view.cursor_offset.get(), None);
    window.close();
}

#[cfg(test)]
#[test]
#[ignore = "requires a GTK display"]
fn selection_joins_wide_wraps_in_live_and_archived_rows() {
    gtk4::init().expect("GTK display");
    let mut terminal = jterm_engine::terminal::Terminal::new(3, 6).unwrap();
    terminal.enable_grapheme_clusters();
    let view = TerminalView::new(Theme {
        font: pango::FontDescription::from_string("Monospace 12"),
        foreground: gdk::RGBA::WHITE,
        background: gdk::RGBA::BLACK,
        palette: default_ansi_palette(),
        cursor: None,
    });
    view.widget.set_widget_name("host-renamed-editor");
    assert!(is_terminal_text_view(view.widget.upcast_ref()));
    assert_eq!(terminal_view_selection(view.widget.upcast_ref()), None);
    let select = || {
        let buffer = view.widget.buffer();
        buffer.select_range(
            &buffer.iter_at_line_offset(0, 1).unwrap(),
            &buffer.iter_at_line_offset(1, 2).unwrap(),
        );
        let selected = view.selected_text().unwrap();
        assert_eq!(
            terminal_view_selection(view.widget.upcast_ref()).as_deref(),
            Some(selected.as_str())
        );
        selected
    };
    terminal.feed("ae\u{301}c\u{2029}e界Z".as_bytes()).unwrap();
    view.present(
        &terminal.frame().unwrap(),
        terminal.live_margin_gaps(),
        None,
        None,
    );
    assert_eq!(select(), "e\u{301}c\u{2029}e界Z");
    terminal.feed(b"\r\nx\r\ny\r\n").unwrap();
    let viewport = capture_scrollback(&terminal, 2);
    assert!(viewport.history.is_some());
    view.present(
        &viewport.frame,
        viewport.live_margin_gaps,
        viewport.history.as_ref(),
        viewport.colors.as_ref(),
    );
    assert_eq!(select(), "e\u{301}c\u{2029}e界Z");

    let widget = view.widget.clone();
    drop(view);
    assert!(!is_terminal_text_view(widget.upcast_ref()));
    assert_eq!(terminal_view_selection(widget.upcast_ref()), None);

    let finished = crate::FinishedTextView::plain_command(
        "printf 界",
        pango::FontDescription::from_string("Monospace 12"),
        1.0,
        gdk::RGBA::WHITE,
        &default_ansi_palette(),
    );
    let finished = finished.widget();
    let buffer = finished.buffer();
    buffer.select_range(&buffer.start_iter(), &buffer.end_iter());
    assert!(is_terminal_text_view(finished.upcast_ref()));
    assert_eq!(
        terminal_view_selection(finished.upcast_ref()).as_deref(),
        Some("printf 界")
    );
    widget.set_widget_name("shared-finished-command");
    assert!(!is_terminal_text_view(widget.upcast_ref()));
}

fn live_updates(before: &CellFrame, after: &CellFrame, changed_gaps: u128) -> Option<Vec<usize>> {
    if before.stamp() == after.stamp() {
        return Some(Vec::new());
    }
    let mut rows = before.changed_row_indices(after).ok()?;
    if changed_gaps != 0 {
        rows.extend((0..after.rows().len()).filter(|row| changed_gaps & (1 << row) != 0));
    }
    if before.cursor() != after.cursor() {
        rows.push(before.cursor().paint_position().0);
        rows.push(after.cursor().paint_position().0);
    }
    rows.sort_unstable();
    rows.dedup();
    Some(rows)
}

fn retained_scroll_rows(
    before: &CellFrame,
    after: &CellFrame,
    before_gaps: u128,
    after_gaps: u128,
    repaint_count: usize,
) -> Option<Vec<(usize, usize)>> {
    if repaint_count < 2 {
        return None;
    }
    let old_cursor = before.cursor().paint_position().0;
    let new_cursor = after.cursor().paint_position().0;
    let mut sources: Vec<_> = before
        .rows()
        .iter()
        .enumerate()
        .filter(|(row, _)| *row != old_cursor)
        .map(|(index, row)| (row.cells.as_ptr(), index))
        .collect();
    sources.sort_unstable();
    // Structural edits preserve the order of surviving rows. Keep that
    // order so the buffer can edit gaps without copying the retained text.
    let mut retained = Vec::new();
    let mut next_source = 0;
    for (target, row) in after.rows().iter().enumerate() {
        if target == new_cursor {
            continue;
        }
        let address = row.cells.as_ptr();
        let start = sources.partition_point(|&(candidate, _)| candidate < address);
        if let Some(source) = sources[start..]
            .iter()
            .take_while(|&&(candidate, _)| candidate == address)
            .map(|&(_, index)| index)
            .find(|&source| {
                source >= next_source
                    && ((before_gaps >> source) ^ (after_gaps >> target)) & 1 == 0
                    && before.shares_row_paint_with(source, after, target)
            })
        {
            retained.push((source, target));
            next_source = source + 1;
        }
    }
    (after.rows().len() - retained.len() < repaint_count
        && retained.iter().any(|(source, target)| source != target))
    .then_some(retained)
}

fn remove_tags<'a>(buffer: &gtk4::TextBuffer, tags: impl Iterator<Item = &'a gtk4::TextTag>) {
    let table = buffer.tag_table();
    let mut tags: Vec<_> = tags.collect();
    // Remove the highest priorities first to avoid reordering the remaining tags.
    tags.sort_unstable_by_key(|tag| std::cmp::Reverse(tag.priority()));
    for tag in tags {
        table.remove(tag);
    }
}

impl Presentation {
    fn new(buffer: gtk4::TextBuffer, viewport: ViewportRows<'_>, theme: &PaintTheme<'_>) -> Self {
        let font = gtk4::TextTag::builder()
            .font_desc(&theme.configured.font)
            .build();
        buffer.tag_table().add(&font);
        let mut tags = PaintTags {
            font,
            styles: HashMap::new(),
        };
        let height = viewport.frame.rows().len();
        let archived = viewport.history.into_iter().flat_map(|(_, page)| {
            (0..page.row_count).map(move |index| RowSource::History(page, index))
        });
        let live =
            (0..height).map(|index| RowSource::Live(viewport.frame, index, viewport.margin_gaps));
        let mut rows = Vec::with_capacity(height);
        let mut targets = HashMap::new();
        let mut text = String::with_capacity(height * (viewport.frame.cols() + 1));
        let mut run_lengths = Vec::with_capacity(height);
        for (index, source) in archived.chain(live).take(height).enumerate() {
            if index != 0 {
                text.push('\n');
            }
            rows.push(format_row(
                source,
                theme,
                viewport.caret(index),
                &mut targets,
                &mut text,
                |_, length, style| {
                    run_lengths.push(length);
                    tags.style_tag(&buffer, style)
                },
            ));
        }
        if !text.is_empty() {
            // Publish the complete text and its shared font before applying
            // the per-run attributes at GTK character offsets.
            buffer.insert_with_tags(&mut buffer.end_iter(), &text, &[&tags.font]);
            let mut end = buffer.start_iter();
            let mut run_lengths = run_lengths.into_iter();
            for (index, row) in rows.iter().enumerate() {
                if index != 0 {
                    end.forward_char();
                }
                for style in &row.tags {
                    let start = end;
                    end.forward_chars(run_lengths.next().expect("length of the styled run"));
                    buffer.apply_tag(&style.tag, &start, &end);
                }
            }
            debug_assert!(run_lengths.next().is_none());
            debug_assert!(end.is_end());
        }
        Self {
            buffer,
            tags,
            rows,
            live: viewport.live_snapshot(),
            history: viewport.history_position(),
        }
    }

    fn remove_tags(&self) {
        remove_tags(
            &self.buffer,
            self.tags
                .styles
                .values()
                .map(|style| &style.tag)
                .chain(std::iter::once(&self.tags.font)),
        );
    }

    fn update(&mut self, viewport: ViewportRows<'_>, theme: &PaintTheme<'_>) -> bool {
        if viewport.history.is_some() {
            if let Some(retained) = self.retained_history_rows(viewport) {
                return self.update_scroll(viewport, theme, &retained);
            }
        } else if self.history.is_none() {
            let (before, old_gaps) = self.live.as_ref().expect("presented live frame");
            let frame = viewport.frame;
            let gaps = viewport.margin_gaps;
            if let Some(indices) = live_updates(before, frame, *old_gaps ^ gaps) {
                if let Some(retained) =
                    retained_scroll_rows(before, frame, *old_gaps, gaps, indices.len())
                {
                    return self.update_scroll(viewport, theme, &retained);
                }
                return self.update_live(viewport, theme, indices);
            }
        }
        false
    }

    fn retained_history_rows(&self, viewport: ViewportRows<'_>) -> Option<Vec<(usize, usize)>> {
        let before = self.history.as_ref()?;
        let (after, _) = viewport.history?;
        // One layout identifies immutable archive rows in their projected order.
        let layout = before.layout.upgrade()?;
        if !Arc::ptr_eq(&layout, &after.layout) {
            return None;
        }
        let height = viewport.frame.rows().len();
        let start = before.first.max(after.first);
        let end = before.first.min(after.first) + height;
        let live_top = after.layout.len();
        let mut retained = Vec::with_capacity(end.saturating_sub(start));
        for line in start..end {
            let unchanged = line < live_top
                || self.live.as_ref().is_some_and(|(frame, gaps)| {
                    let row = line - live_top;
                    (gaps ^ viewport.margin_gaps) & (1 << row) == 0
                        && frame.shares_row_paint_with(row, viewport.frame, row)
                });
            if unchanged {
                retained.push((line - before.first, line - after.first));
            }
        }
        (!retained.is_empty()).then_some(retained)
    }

    fn update_scroll(
        &mut self,
        viewport: ViewportRows<'_>,
        theme: &PaintTheme<'_>,
        retained: &[(usize, usize)],
    ) -> bool {
        let mut targets = HashMap::new();
        let mut old_end = self.rows.len();
        let height = viewport.frame.rows().len();
        let mut new_end = height;
        // Edit from the bottom so the old line indices above each edit stay valid.
        for &(source, target) in retained.iter().rev() {
            if self
                .replace_scroll_gap(
                    viewport,
                    theme,
                    source + 1..old_end,
                    target + 1..new_end,
                    &mut targets,
                )
                .is_none()
            {
                return false;
            }
            old_end = source;
            new_end = target;
        }
        if self
            .replace_scroll_gap(viewport, theme, 0..old_end, 0..new_end, &mut targets)
            .is_none()
            || self.buffer.line_count() as usize != height
        {
            return false;
        }
        self.live = viewport.live_snapshot();
        self.history = viewport.history_position();
        true
    }

    fn replace_scroll_gap(
        &mut self,
        viewport: ViewportRows<'_>,
        theme: &PaintTheme<'_>,
        old: Range<usize>,
        new: Range<usize>,
        targets: &mut HashMap<HyperlinkId, Option<Rc<str>>>,
    ) -> Option<()> {
        if old.is_empty() && new.is_empty() {
            return Some(());
        }
        let tail = old.end == self.rows.len();
        // Middle and leading gaps own the newline after each of their rows.
        // A tail gap owns the newline immediately after the last retained row.
        let (mut start, mut end) = if tail {
            let mut start = self.buffer.iter_at_line(old.start.checked_sub(1)? as i32)?;
            if !start.ends_line() {
                start.forward_to_line_end();
            }
            (start, self.buffer.end_iter())
        } else {
            (
                self.buffer.iter_at_line(old.start as i32)?,
                self.buffer.iter_at_line(old.end as i32)?,
            )
        };
        if start != end {
            self.buffer.delete(&mut start, &mut end);
        }
        let mut painted = Vec::with_capacity(new.len());
        for index in new {
            if tail {
                self.buffer
                    .insert_with_tags(&mut start, "\n", &[&self.tags.font]);
            }
            painted.push(paint_row(
                &self.buffer,
                &mut start,
                viewport.row(index),
                theme,
                &mut self.tags,
                viewport.caret(index),
                targets,
            ));
            if !tail {
                self.buffer
                    .insert_with_tags(&mut start, "\n", &[&self.tags.font]);
            }
        }
        self.tags.retire(
            &self.buffer,
            self.rows.splice(old, painted).flat_map(|row| row.tags),
        );
        Some(())
    }

    fn update_live(
        &mut self,
        viewport: ViewportRows<'_>,
        theme: &PaintTheme<'_>,
        indices: Vec<usize>,
    ) -> bool {
        let mut targets = HashMap::new();
        for index in indices {
            let Some(mut start) = self.buffer.iter_at_line(index as i32) else {
                return false;
            };
            let mut end = start;
            end.forward_to_line_end();
            self.buffer.delete(&mut start, &mut end);
            let painted = paint_row(
                &self.buffer,
                &mut start,
                viewport.row(index),
                theme,
                &mut self.tags,
                viewport.caret(index),
                &mut targets,
            );
            let replaced = std::mem::replace(&mut self.rows[index], painted);
            self.tags.retire(&self.buffer, replaced.tags.into_iter());
        }
        self.live = viewport.live_snapshot();
        true
    }
}

fn paint_row(
    buffer: &gtk4::TextBuffer,
    end: &mut gtk4::TextIter,
    source: RowSource<'_>,
    theme: &PaintTheme<'_>,
    tags: &mut PaintTags,
    cursor: Option<Cursor>,
    targets: &mut HashMap<HyperlinkId, Option<Rc<str>>>,
) -> PresentedRow {
    format_row(
        source,
        theme,
        cursor,
        targets,
        &mut String::new(),
        |text, _, style| {
            let tag = tags.insert_run(buffer, end, text, style);
            text.clear();
            tag
        },
    )
}

fn format_row(
    source: RowSource<'_>,
    theme: &PaintTheme<'_>,
    cursor: Option<Cursor>,
    targets: &mut HashMap<HyperlinkId, Option<Rc<str>>>,
    text: &mut String,
    mut style_run: impl FnMut(&mut String, i32, Style) -> Rc<StyleTag>,
) -> PresentedRow {
    let row = source
        .row()
        .expect("frame and history indices are admitted");
    let mut result = PresentedRow {
        tags: Vec::new(),
        links: Vec::new(),
        column_spans: Vec::new(),
        copy: CopyRow {
            wrapped: row.wrapped,
            margin_gap: None,
        },
        line_separators: Vec::new(),
        cursor: None,
        blank_tail: (!source.has_image()).then(|| {
            let trailing = row
                .cells
                .iter()
                .rposition(|cell| {
                    !cell.is_reflow_trimmable_blank() || !cell.flags.is_default_style()
                })
                .map_or(0, |column| column + 1);
            let caret = cursor
                .filter(|cursor| cursor.visible)
                .map_or(0, |cursor| cursor.paint_position().1 + 1);
            trailing.max(caret)
        }),
    };
    let mut offset = 0;
    let mut run_offset = 0;
    let mut previous = None;
    // Repeated styles are common in text rows. Diverse rows retain the
    // ordinary resolver, including rows whose first cells only happen to match.
    let mut cache_styles = row
        .cells
        .first()
        .zip(row.cells.get(1))
        .is_some_and(|(a, b)| {
            a.foreground == b.foreground && a.background == b.background && a.flags == b.flags
        });
    let mut style_cache = None;
    let mut style_misses = 0u8;
    let mut underline_colors = source.underline_colors().iter().peekable();
    let margin_gap = source.margin_gap();
    for (column, cell) in row.cells.iter().enumerate() {
        if cell.flags.wide_continuation() {
            continue;
        }
        if margin_gap == Some(column) {
            result.copy.margin_gap = Some(offset as usize);
        }
        let caret = cursor.filter(|cursor| {
            let position = cursor.paint_position().1;
            position == column || (cell.flags.wide() && position == column + 1)
        });
        if caret.is_some() {
            result.cursor = Some(offset);
        }
        while underline_colors
            .peek()
            .is_some_and(|color| usize::from(color.end) <= column)
        {
            underline_colors.next();
        }
        let underline_color = underline_colors
            .peek()
            .filter(|color| usize::from(color.start) <= column)
            .map(|color| color.color);
        let block_cursor =
            caret.is_some_and(|cursor| cursor.visible && cursor.shape == CursorShape::Block);
        let style = if cache_styles {
            let key = (
                cell.foreground,
                cell.background,
                cell.flags,
                underline_color,
                block_cursor,
            );
            if let Some((_, style)) = style_cache.filter(|(previous, _)| *previous == key) {
                style_misses = 0;
                style
            } else {
                if style_cache.is_some() {
                    style_misses += 1;
                    // Two misses can surround an isolated cursor or underline
                    // change. Three indicate that caching is no longer useful.
                    cache_styles = style_misses < 3;
                }
                let style = TerminalView::style(theme, cell, underline_color, block_cursor);
                if cache_styles {
                    style_cache = Some((key, style));
                }
                style
            }
        } else {
            TerminalView::style(theme, cell, underline_color, block_cursor)
        };
        if let Some(old_style) = previous.filter(|old| *old != style) {
            result
                .tags
                .push(style_run(text, offset - run_offset, old_style));
            run_offset = offset;
        }
        let length = if let Some(cluster) = source.cluster(column) {
            let mut length = 0;
            for character in cluster.chars() {
                push_painted_character(
                    text,
                    &mut result.line_separators,
                    offset + length,
                    character,
                );
                length += 1;
            }
            length
        } else {
            push_painted_character(text, &mut result.line_separators, offset, cell.character);
            1
        };
        let end_offset = offset + length;
        if length != 1 || cell.flags.wide() {
            result.column_spans.push(ColumnSpan {
                start: offset,
                end: end_offset,
                column: column as i32,
                width: if cell.flags.wide() { 2 } else { 1 },
            });
        }
        let id = cell.hyperlink_id;
        if !id.is_none() {
            if let Some(previous) = result
                .links
                .last_mut()
                .filter(|previous| previous.end == offset && previous.id == id)
            {
                previous.end = end_offset;
            } else {
                let target = targets
                    .entry(id)
                    .or_insert_with(|| source.target(id).map(Rc::from));
                if let Some(target) = target {
                    result.links.push(PresentedLink {
                        id,
                        start: offset,
                        end: end_offset,
                        target: target.clone(),
                    });
                }
            }
        }
        offset = end_offset;
        previous = Some(style);
    }
    if let Some(style) = previous {
        result
            .tags
            .push(style_run(text, offset - run_offset, style));
    }
    result
}

fn push_painted_character(
    run: &mut String,
    separators: &mut Vec<(usize, char)>,
    offset: i32,
    character: char,
) {
    // Keep terminal cells inside one GTK display line and restore these characters on copy.
    if matches!(character, '\u{2028}' | '\u{2029}') {
        separators.push((offset as usize, character));
        run.push(' ');
    } else {
        run.push(character);
    }
}

fn fitted_grid(width: i32, height: i32, cell_width: f64, cell_height: i32) -> (usize, usize) {
    let rows = (height.saturating_sub(MARGIN * 2) / cell_height).clamp(1, 128) as usize;
    let columns =
        (f64::from(width.saturating_sub(MARGIN * 2)) / cell_width).clamp(1.0, 256.0) as usize;
    (rows, columns)
}

fn font_row_height(pango_height: i32) -> i32 {
    ((pango_height + pango::SCALE - 1) / pango::SCALE).max(1)
}

fn rgb16(color: gdk::RGBA) -> [u16; 3] {
    [color.red(), color.green(), color.blue()]
        .map(|component| (component * f32::from(u16::MAX)).round() as u16)
}

pub(crate) fn faint_palette_color(color: gdk::RGBA) -> gdk::RGBA {
    let intensity = 2.0 / 3.0;
    gdk::RGBA::new(
        color.red() * intensity,
        color.green() * intensity,
        color.blue() * intensity,
        color.alpha(),
    )
}

pub(crate) fn terminal_color(
    color: Color,
    default: gdk::RGBA,
    palette: &[gdk::RGBA; 16],
) -> gdk::RGBA {
    resolve_terminal_color(color, default, palette, None)
}

fn resolve_terminal_color(
    color: Color,
    default: gdk::RGBA,
    palette: &[gdk::RGBA; 16],
    dynamic: Option<&ColorState>,
) -> gdk::RGBA {
    let index = match color {
        Color::Default => return default,
        Color::Rgb(r, g, b) => return rgba(r, g, b),
        Color::Indexed(index) => index,
        Color::Black => 0,
        Color::Red => 1,
        Color::Green => 2,
        Color::Yellow => 3,
        Color::Blue => 4,
        Color::Magenta => 5,
        Color::Cyan => 6,
        Color::White => 7,
        Color::BrightBlack => 8,
        Color::BrightRed => 9,
        Color::BrightGreen => 10,
        Color::BrightYellow => 11,
        Color::BrightBlue => 12,
        Color::BrightMagenta => 13,
        Color::BrightCyan => 14,
        Color::BrightWhite => 15,
    };
    if let Some((red, green, blue)) =
        dynamic.and_then(|state| state.palette_overrides()[usize::from(index)])
    {
        return rgba(red, green, blue);
    }
    palette.get(usize::from(index)).copied().unwrap_or_else(|| {
        let (r, g, b) = ColorState::default_256_color(index);
        rgba(r, g, b)
    })
}
pub fn default_ansi_palette() -> [gdk::RGBA; 16] {
    std::array::from_fn(|index| {
        let (r, g, b) = ColorState::default_256_color(index as u8);
        rgba(r, g, b)
    })
}

fn rgba(r: u8, g: u8, b: u8) -> gdk::RGBA {
    gdk::RGBA::new(
        f32::from(r) / 255.0,
        f32::from(g) / 255.0,
        f32::from(b) / 255.0,
        1.0,
    )
}

pub(crate) fn concealed_ink() -> gdk::RGBA {
    // Pango reserves alpha zero to unset the override. Its smallest nonzero
    // 16-bit alpha preserves shaping and rounds away in 8-bit output.
    gdk::RGBA::new(0., 0., 0., 1. / f32::from(u16::MAX))
}

impl PaintTags {
    fn insert_run(
        &mut self,
        buffer: &gtk4::TextBuffer,
        end: &mut gtk4::TextIter,
        text: &str,
        style: Style,
    ) -> Rc<StyleTag> {
        let tag = self.style_tag(buffer, style);
        buffer.insert_with_tags(end, text, &[&self.font, &tag.tag]);
        tag
    }

    fn style_tag(&mut self, buffer: &gtk4::TextBuffer, style: Style) -> Rc<StyleTag> {
        let entry = self.styles.entry(style).or_insert_with(|| {
            let tag = gtk4::TextTag::builder()
                .foreground_rgba(&style.foreground)
                .background_rgba(&style.background)
                .weight(if style.bold { 700 } else { 400 })
                .style(if style.italic {
                    pango::Style::Italic
                } else {
                    pango::Style::Normal
                })
                .underline(style.underline)
                .strikethrough(style.strikethrough)
                .overline(if style.overline {
                    pango::Overline::Single
                } else {
                    pango::Overline::None
                })
                .build();
            if let Some(color) = style.underline_color {
                tag.set_underline_rgba(Some(&color));
            }
            buffer.tag_table().add(&tag);
            Rc::new(StyleTag { style, tag })
        });
        Rc::clone(entry)
    }

    fn retire(&mut self, buffer: &gtk4::TextBuffer, runs: impl Iterator<Item = Rc<StyleTag>>) {
        let mut unused = Vec::new();
        for run in runs {
            // Only the cache and this retired run still own the tag. Process
            // retired runs after painting so replacement text can reuse it.
            if Rc::strong_count(&run) == 2 {
                let Some(entry) = self.styles.remove(&run.style) else {
                    continue;
                };
                unused.push(entry);
            }
        }
        if !unused.is_empty() {
            remove_tags(buffer, unused.iter().map(|entry| &entry.tag));
        }
    }
}
