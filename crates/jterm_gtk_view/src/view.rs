use gtk4::{gdk, pango, prelude::*};
use jterm_engine::{
    colors::ColorState,
    frame::{CellFrame, Cursor, Row},
    grid::{Color, CursorShape, TerminalCell, UnderlineStyle},
    hyperlink::HyperlinkId,
    terminal::HistoryPage,
    UnderlineColorRun,
};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
    sync::Arc,
};

use crate::pane::{PlainLinkFinder, Theme};

const MARGIN: i32 = 6;

/// GTK presentation of an immutable live or historical viewport. The widget owns selection
/// and clipboard state; the session owns the grid and every terminal mode.
pub(crate) struct TerminalView {
    pub(crate) widget: gtk4::TextView,
    preedit: gtk4::Label,
    cursor_offset: Cell<Option<i32>>,
    presentation: RefCell<Option<Presentation>>,
    find_tags: RefCell<Option<[gtk4::TextTag; 2]>>,
    theme: RefCell<Theme>,
    background_style: gtk4::CssProvider,
    background: Cell<Option<gdk::RGBA>>,
    line_height_scale: Cell<f64>,
    appearance_changed: Cell<bool>,
    palette_revision: Cell<u64>,
}

struct Presentation {
    buffer: gtk4::TextBuffer,
    font: gtk4::TextTag,
    rows: Vec<PresentedRow>,
    live: Option<(CellFrame, u128)>,
}

struct PresentedRow {
    tags: Vec<gtk4::TextTag>,
    links: Vec<PresentedLink>,
    column_spans: Vec<ColumnSpan>,
    copy: CopyRow,
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

#[derive(Clone, Copy, PartialEq)]
struct Style {
    foreground: gdk::RGBA,
    background: gdk::RGBA,
    bold: bool,
    italic: bool,
    underline: pango::Underline,
    underline_color: Option<gdk::RGBA>,
    strikethrough: bool,
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
        let widget = gtk4::TextView::builder()
            .buffer(&buffer)
            .editable(false)
            .cursor_visible(false)
            .monospace(true)
            .wrap_mode(gtk4::WrapMode::None)
            .left_margin(MARGIN)
            .right_margin(MARGIN)
            .top_margin(MARGIN)
            .bottom_margin(MARGIN)
            .hexpand(true)
            .vexpand(true)
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
            presentation: RefCell::default(),
            find_tags: RefCell::default(),
            theme: RefCell::new(theme),
            background_style,
            background: Cell::new(None),
            line_height_scale: Cell::new(1.0),
            appearance_changed: Cell::new(false),
            palette_revision: Cell::new(0),
        };
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

    pub(crate) fn set_theme(&self, theme: Theme) {
        self.theme.replace(theme);
        self.appearance_changed.set(true);
        self.update_line_spacing();
    }

    pub(crate) fn set_font(&self, font: &pango::FontDescription) {
        self.theme.borrow_mut().font = font.clone();
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
        let rows = ((height - MARGIN * 2) / cell_height).clamp(1, 128);
        let columns = ((width - MARGIN * 2) / cell_width).clamp(1, 256);
        (rows as usize, columns as usize)
    }

    fn cell_geometry(&self) -> (i32, i32, i32) {
        let context = self.widget.pango_context();
        let metrics = context.metrics(Some(&self.theme.borrow().font), None);
        let cell_width =
            ((metrics.approximate_digit_width() + pango::SCALE - 1) / pango::SCALE).max(1);
        let unscaled = font_row_height(metrics.height());
        let cell_height = (f64::from(unscaled) * self.line_height_scale.get()).round() as i32;
        (cell_width, cell_height, cell_height - unscaled)
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
        history: Option<&HistoryPage>,
        colors: Option<&(u64, Arc<ColorState>)>,
    ) {
        self.clear_find();
        let revision = colors.map_or(0, |(revision, _)| *revision);
        let same_palette = self.palette_revision.replace(revision) == revision;
        let configured = self.theme.borrow().clone();
        let theme = PaintTheme {
            configured: &configured,
            dynamic: colors.map(|(_, palette)| palette.as_ref()),
        };
        self.paint_background(theme.background());
        let previous = self.presentation.take();
        let updates =
            if !self.appearance_changed.replace(false) && same_palette && history.is_none() {
                previous.as_ref().and_then(|previous| {
                    if previous.buffer != self.widget.buffer()
                        || previous.buffer.line_count() as usize != frame.rows().len()
                    {
                        return None;
                    }
                    let (before, before_gaps) = previous.live.as_ref()?;
                    live_updates(before, frame, *before_gaps ^ margin_gaps)
                })
            } else {
                None
            };
        let current = match (previous, updates) {
            (Some(mut previous), Some(updates)) => {
                previous.update_live(frame, margin_gaps, &theme, updates);
                previous
            }
            (previous, _) => {
                // GTK's input-method callback can retain line layout across a
                // paint. Editing the mounted buffer lets GTK invalidate it.
                let buffer = self.widget.buffer();
                buffer.set_text("");
                if let Some(previous) = previous {
                    previous.remove_tags();
                }
                Presentation::new(buffer, frame, margin_gaps, history, &theme)
            }
        };
        let caret = current.rows.iter().enumerate().find_map(|(row, painted)| {
            current
                .buffer
                .iter_at_line_offset(row as i32, painted.cursor?)
        });
        self.cursor_offset
            .set(caret.as_ref().map(gtk4::TextIter::offset));
        if let Some(caret) = caret {
            if current.buffer.iter_at_mark(&current.buffer.get_insert()) != caret {
                current.buffer.place_cursor(&caret);
            }
        }
        self.presentation.replace(Some(current));
    }

    pub(crate) fn painted_frame(&self) -> Option<CellFrame> {
        let presentation = self.presentation.borrow();
        let (frame, _) = presentation.as_ref()?.live.as_ref()?;
        Some(frame.clone())
    }

    pub(crate) fn link_at(
        &self,
        x: f64,
        y: f64,
        find_plain: Option<PlainLinkFinder>,
    ) -> Option<PresentedLink> {
        let iter = text_at_point(&self.widget, x, y)?;
        let shown = self.presentation.borrow();
        let shown = shown.as_ref()?;
        let row = shown.rows.get(iter.line() as usize)?;
        link_for_offset(&row.links, iter.line_offset()).or_else(|| {
            plain_link_at(&self.widget, &iter, find_plain?, |line| {
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
        let buffer = self.widget.buffer();
        let (start, end) = buffer.selection_bounds()?;
        let selected = buffer.text(&start, &end, true);
        let shown = self.presentation.borrow();
        let rows = shown.as_ref().map_or(&[][..], |view| view.rows.as_slice());
        Some(logical_selection(
            &selected,
            start.line() as usize,
            start.line_offset() as usize,
            |index| {
                rows.get(index)
                    .map_or_else(CopyRow::default, |row| row.copy)
            },
        ))
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
        cursor: Option<CursorShape>,
    ) -> Style {
        let mut foreground = theme.color(cell.foreground, theme.foreground());
        let mut background = theme.color(cell.background, theme.background());
        if cell.flags.inverse() {
            std::mem::swap(&mut foreground, &mut background);
            foreground.set_alpha(1.0);
        }
        if matches!(cursor, Some(CursorShape::Block | CursorShape::Beam)) {
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
        let underline = match (cell.flags.underline(), cursor) {
            (UnderlineStyle::Double, _) => pango::Underline::Double,
            (UnderlineStyle::Curly, _) => pango::Underline::Error,
            (UnderlineStyle::None, Some(CursorShape::Underline)) => pango::Underline::Single,
            (UnderlineStyle::None, _) => pango::Underline::None,
            _ => pango::Underline::Single,
        };
        Style {
            foreground,
            background,
            bold: cell.flags.bold(),
            italic: cell.flags.italic(),
            underline,
            underline_color: if cell.flags.underline() != UnderlineStyle::None {
                underline_color.map(|color| theme.color(color, foreground))
            } else if cursor == Some(CursorShape::Underline) {
                theme.cursor()
            } else {
                None
            },
            strikethrough: cell.flags.strikethrough(),
        }
    }
}

fn text_at_point(view: &gtk4::TextView, x: f64, y: f64) -> Option<gtk4::TextIter> {
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

fn link_for_offset(links: &[PresentedLink], offset: i32) -> Option<PresentedLink> {
    let index = links.partition_point(|link| link.end <= offset);
    links
        .get(index)
        .filter(|link| link.start <= offset)
        .cloned()
}

pub(crate) fn link_at(
    view: &gtk4::TextView,
    links: &[PresentedLink],
    find_plain: Option<PlainLinkFinder>,
    x: f64,
    y: f64,
) -> Option<PresentedLink> {
    let iter = text_at_point(view, x, y)?;
    link_for_offset(links, iter.offset())
        .or_else(|| plain_link_at(view, &iter, find_plain?, |_| CopyRow::default()))
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

fn logical_selection(
    selected: &str,
    first_row: usize,
    first_offset: usize,
    copy_row: impl Fn(usize) -> CopyRow,
) -> String {
    let mut text = String::with_capacity(selected.len());
    for (index, physical_row) in selected.split_inclusive('\n').enumerate() {
        let row = copy_row(first_row + index);
        let physical_row = if row.wrapped {
            physical_row.strip_suffix('\n').unwrap_or(physical_row)
        } else {
            physical_row
        };
        let start = if index == 0 { first_offset } else { 0 };
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
        logical_selection(selected, 1, 4, |row| CopyRow {
            wrapped: row == 1,
            margin_gap: None,
        }),
        "👩‍💻-   continuation\nnext"
    );
    assert_eq!(
        logical_selection(selected, 1, 4, |row| CopyRow {
            wrapped: row == 1,
            margin_gap: (row == 1).then_some(8),
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
    let select = || {
        let buffer = view.widget.buffer();
        buffer.select_range(
            &buffer.iter_at_line_offset(0, 1).unwrap(),
            &buffer.iter_at_line_offset(1, 2).unwrap(),
        );
        view.selected_text().unwrap()
    };
    terminal.feed("ae\u{301}cde界Z".as_bytes()).unwrap();
    view.present(
        &terminal.frame().unwrap(),
        terminal.live_margin_gaps(),
        None,
        None,
    );
    assert_eq!(select(), "e\u{301}cde界Z");
    terminal.feed(b"\r\nx\r\ny\r\n").unwrap();
    let history = terminal.history_page(0, 2).unwrap();
    view.present(
        &terminal.frame().unwrap(),
        terminal.live_margin_gaps(),
        Some(&history),
        None,
    );
    assert_eq!(select(), "e\u{301}cde界Z");
}

fn live_updates(before: &CellFrame, after: &CellFrame, changed_gaps: u128) -> Option<Vec<usize>> {
    if before.stamp() == after.stamp() {
        return Some(Vec::new());
    }
    let delta = before.delta_to(after).ok()?;
    let mut rows = delta
        .rows
        .into_iter()
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    rows.extend((0..after.rows().len()).filter(|row| changed_gaps & (1 << row) != 0));
    if before.cursor() != after.cursor() {
        rows.push(before.cursor().paint_position().0);
        rows.push(after.cursor().paint_position().0);
    }
    rows.sort_unstable();
    rows.dedup();
    Some(rows)
}

impl Presentation {
    fn new(
        buffer: gtk4::TextBuffer,
        frame: &CellFrame,
        margin_gaps: u128,
        history: Option<&HistoryPage>,
        theme: &PaintTheme<'_>,
    ) -> Self {
        let font = gtk4::TextTag::builder()
            .font_desc(&theme.configured.font)
            .build();
        buffer.tag_table().add(&font);
        let mut end = buffer.end_iter();
        let archived = history
            .into_iter()
            .flat_map(|page| (0..page.row_count).map(move |index| RowSource::History(page, index)));
        let live = (0..frame.rows().len()).map(|index| RowSource::Live(frame, index, margin_gaps));
        let mut rows = Vec::with_capacity(frame.rows().len());
        let cursor = frame.cursor();
        let mut targets = HashMap::new();
        for (index, source) in archived.chain(live).take(frame.rows().len()).enumerate() {
            if index != 0 {
                buffer.insert_with_tags(&mut end, "\n", &[&font]);
            }
            let caret = (history.is_none() && cursor.paint_position().0 == index).then_some(cursor);
            rows.push(paint_row(
                &buffer,
                &mut end,
                source,
                theme,
                &font,
                caret,
                &mut targets,
            ));
        }
        Self {
            buffer,
            font,
            rows,
            live: history.is_none().then(|| (frame.clone(), margin_gaps)),
        }
    }

    fn remove_tags(&self) {
        let table = self.buffer.tag_table();
        let mut tags: Vec<_> = self
            .rows
            .iter()
            .flat_map(|row| &row.tags)
            .chain(std::iter::once(&self.font))
            .collect();
        // Removing the highest priority first avoids reordering the tags
        // that are still in the table.
        tags.sort_unstable_by_key(|tag| std::cmp::Reverse(tag.priority()));
        for tag in tags {
            table.remove(tag);
        }
    }

    fn update_live(
        &mut self,
        frame: &CellFrame,
        margin_gaps: u128,
        theme: &PaintTheme<'_>,
        indices: Vec<usize>,
    ) {
        let table = self.buffer.tag_table();
        let cursor = frame.cursor();
        let mut targets = HashMap::new();
        for index in indices {
            let Some(mut start) = self.buffer.iter_at_line(index as i32) else {
                self.buffer.set_text("");
                self.remove_tags();
                *self = Self::new(self.buffer.clone(), frame, margin_gaps, None, theme);
                return;
            };
            let mut end = start;
            end.forward_to_line_end();
            self.buffer.delete(&mut start, &mut end);
            for tag in &self.rows[index].tags {
                table.remove(tag);
            }
            let caret = (cursor.paint_position().0 == index).then_some(cursor);
            self.rows[index] = paint_row(
                &self.buffer,
                &mut start,
                RowSource::Live(frame, index, margin_gaps),
                theme,
                &self.font,
                caret,
                &mut targets,
            );
        }
        self.live = Some((frame.clone(), margin_gaps));
    }
}

fn paint_row(
    buffer: &gtk4::TextBuffer,
    end: &mut gtk4::TextIter,
    source: RowSource<'_>,
    theme: &PaintTheme<'_>,
    font: &gtk4::TextTag,
    cursor: Option<Cursor>,
    targets: &mut HashMap<HyperlinkId, Option<Rc<str>>>,
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
    let mut run = String::new();
    let mut previous = None;
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
        let style = TerminalView::style(
            theme,
            cell,
            underline_color,
            caret
                .filter(|cursor| cursor.visible)
                .map(|cursor| cursor.shape),
        );
        if let Some(old_style) = previous.filter(|old| *old != style) {
            result
                .tags
                .push(insert_run(buffer, end, &run, font, old_style));
            run.clear();
        }
        let length = source.push_text(&mut run, column, cell.character);
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
        result.tags.push(insert_run(buffer, end, &run, font, style));
    }
    result
}

fn font_row_height(pango_height: i32) -> i32 {
    ((pango_height + pango::SCALE - 1) / pango::SCALE).max(1)
}

fn rgb16(color: gdk::RGBA) -> [u16; 3] {
    [color.red(), color.green(), color.blue()]
        .map(|component| (component * f32::from(u16::MAX)).round() as u16)
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

fn insert_run(
    buffer: &gtk4::TextBuffer,
    end: &mut gtk4::TextIter,
    text: &str,
    font: &gtk4::TextTag,
    style: Style,
) -> gtk4::TextTag {
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
        .build();
    if let Some(color) = style.underline_color {
        tag.set_underline_rgba(Some(&color));
    }
    buffer.tag_table().add(&tag);
    buffer.insert_with_tags(end, text, &[font, &tag]);
    tag
}
