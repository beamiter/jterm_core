use gtk4::{gdk, pango, prelude::*};
use jterm_engine::{
    colors::ColorState,
    frame::CellFrame,
    grid::{Color, CursorShape, TerminalCell, UnderlineStyle},
    hyperlink::HyperlinkId,
    terminal::HistoryPage,
};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
};

const MARGIN: i32 = 6;

/// GTK presentation of an immutable live or historical viewport. The widget owns selection
/// and clipboard state; the session owns the grid and every terminal mode.
pub(crate) struct TerminalView {
    pub(crate) widget: gtk4::TextView,
    preedit: gtk4::Label,
    cursor_offset: Cell<Option<i32>>,
    links: RefCell<Vec<PresentedLink>>,
    font: pango::FontDescription,
    foreground: gdk::RGBA,
    background: gdk::RGBA,
    palette: [gdk::RGBA; 16],
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

impl TerminalView {
    pub(crate) fn new(
        font: pango::FontDescription,
        foreground: gdk::RGBA,
        background: gdk::RGBA,
        palette: [gdk::RGBA; 16],
    ) -> Self {
        let widget = gtk4::TextView::builder()
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
        Self {
            widget,
            preedit,
            cursor_offset: Cell::new(None),
            links: RefCell::default(),
            font,
            foreground,
            background,
            palette,
        }
    }

    pub(crate) fn dimensions(&self, width: i32, height: i32) -> (usize, usize) {
        let context = self.widget.pango_context();
        let metrics = context.metrics(Some(&self.font), None);
        let cell_width =
            ((metrics.approximate_digit_width() + pango::SCALE - 1) / pango::SCALE).max(1);
        let cell_height = font_row_height(metrics.height());
        let rows = ((height - MARGIN * 2) / cell_height).clamp(1, 128);
        let columns = ((width - MARGIN * 2) / cell_width).clamp(1, 256);
        (rows as usize, columns as usize)
    }

    pub(crate) fn row_height(&self) -> f64 {
        let metrics = self.widget.pango_context().metrics(Some(&self.font), None);
        f64::from(font_row_height(metrics.height()))
    }

    pub(crate) fn attach_preedit(&self, viewport: &gtk4::Overlay) {
        viewport.add_overlay(&self.preedit);
        viewport.set_clip_overlay(&self.preedit, true);
    }

    pub(crate) fn present(&self, frame: &CellFrame, history: Option<&HistoryPage>) {
        let buffer = gtk4::TextBuffer::new(None);
        let font = gtk4::TextTag::builder().font_desc(&self.font).build();
        buffer.tag_table().add(&font);
        let mut end = buffer.end_iter();
        let cursor = frame.cursor();
        let cursor_position = cursor.paint_position();
        let mut offset = 0;
        let mut links: Vec<PresentedLink> = Vec::new();
        let mut targets = HashMap::<HyperlinkId, Option<Rc<str>>>::new();
        self.cursor_offset.set(None);
        let archived = history.into_iter().flat_map(|page| {
            (0..page.row_count)
                .filter_map(move |index| Some((page.row(index)?, page.underline_color_runs(index))))
        });
        let live = frame
            .rows()
            .iter()
            .enumerate()
            .map(|(index, row)| (row, frame.underline_color_runs(index)));
        let viewport = archived.chain(live).take(frame.rows().len());
        for (row_index, (row, underline_colors)) in viewport.enumerate() {
            let archived_page = history.filter(|page| row_index < page.row_count);
            if row_index != 0 {
                buffer.insert_with_tags(&mut end, "\n", &[&font]);
                offset += 1;
            }
            let mut run = String::new();
            let mut previous = None;
            let mut underline_colors = underline_colors.iter().peekable();
            for (column, cell) in row.cells.iter().enumerate() {
                if cell.flags.wide_continuation() {
                    continue;
                }
                let at_cursor = history.is_none()
                    && cursor_position.0 == row_index
                    && (cursor_position.1 == column
                        || (cell.flags.wide() && cursor_position.1 == column + 1));
                if at_cursor {
                    self.cursor_offset.set(Some(offset));
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
                let style = self.style(
                    cell,
                    underline_color,
                    (at_cursor && cursor.visible).then_some(cursor.shape),
                );
                if let Some(old_style) = previous.filter(|old| *old != style) {
                    insert_run(&buffer, &mut end, &run, &font, old_style);
                    run.clear();
                }
                run.push(cell.character);
                let id = cell.hyperlink_id;
                if !id.is_none() {
                    if let Some(previous) = links
                        .last_mut()
                        .filter(|previous| previous.end == offset && previous.id == id)
                    {
                        previous.end += 1;
                    } else {
                        let target = targets.entry(id).or_insert_with(|| {
                            if let Some(page) = archived_page {
                                page.hyperlink_target(id)
                            } else {
                                frame.hyperlink_target(id)
                            }
                            .map(Rc::from)
                        });
                        if let Some(target) = target {
                            links.push(PresentedLink {
                                id,
                                start: offset,
                                end: offset + 1,
                                target: target.clone(),
                            });
                        }
                    }
                }
                offset += 1;
                previous = Some(style);
            }
            if let Some(style) = previous {
                insert_run(&buffer, &mut end, &run, &font, style);
            }
        }
        self.widget.set_buffer(Some(&buffer));
        self.links.replace(links);
    }

    pub(crate) fn link_at(&self, widget_x: f64, widget_y: f64) -> Option<PresentedLink> {
        let (x, y) = self.widget.window_to_buffer_coords(
            gtk4::TextWindowType::Widget,
            widget_x.floor() as i32,
            widget_y.floor() as i32,
        );
        let (iter, _) = self.widget.iter_at_position(x, y)?;
        let area = self.widget.iter_location(&iter);
        if x < area.x()
            || y < area.y()
            || x >= area.x() + area.width()
            || y >= area.y() + area.height()
        {
            return None;
        }
        let offset = iter.offset();
        let links = self.links.borrow();
        let index = links.partition_point(|link| link.end <= offset);
        links
            .get(index)
            .filter(|link| link.start <= offset)
            .cloned()
    }

    pub(crate) fn show_preedit(&self, text: &str, attributes: pango::AttrList) {
        attributes.insert_before(pango::AttrFontDesc::new(&self.font));
        attributes.insert_before(pango::AttrInt::new_underline(pango::Underline::Single));
        let [red, green, blue] = rgb16(self.background);
        attributes.insert_before(pango::AttrColor::new_background(red, green, blue));
        let [red, green, blue] = rgb16(self.foreground);
        attributes.insert_before(pango::AttrColor::new_foreground(red, green, blue));
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
        &self,
        cell: &TerminalCell,
        underline_color: Option<Color>,
        cursor: Option<CursorShape>,
    ) -> Style {
        let mut foreground = terminal_color(cell.foreground, self.foreground, &self.palette);
        let mut background = terminal_color(cell.background, self.background, &self.palette);
        if cell.flags.inverse() ^ matches!(cursor, Some(CursorShape::Block | CursorShape::Beam)) {
            std::mem::swap(&mut foreground, &mut background);
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
            underline_color: underline_color
                .filter(|_| cell.flags.underline() != UnderlineStyle::None)
                .map(|color| terminal_color(color, foreground, &self.palette)),
            strikethrough: cell.flags.strikethrough(),
        }
    }
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
    palette.get(usize::from(index)).copied().unwrap_or_else(|| {
        let (r, g, b) = ColorState::default_256_color(index);
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
) {
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
}
