use gtk4::{gdk, glib, pango, prelude::*};
use jterm_engine::{
    archived_text::{is_literal_archive_unicode, ArchiveStyle, ArchivedText},
    grid::{Color, UnderlineStyle},
    shell_markers::MAX_OSC133_COMMAND_BYTES,
};
use std::{
    borrow::Cow,
    cell::{Cell, RefCell},
    ops::Range,
    rc::Rc,
    time::Duration,
};

use crate::{
    text_link::connect_links,
    view::{faint_palette_color, link_for_offset, terminal_color, text_at_point, PresentedLink},
    LinkActions, PlainLinkFinder,
};

const FINISHED_HYPERLINK: &str = "jterm-finished-explicit-hyperlink";
const MAX_REFERENCE_DOCUMENT_CHARS: i32 = 256 * 1024;

pub(crate) fn is_finished_text_view(view: &gtk4::TextView) -> bool {
    view.buffer()
        .tag_table()
        .lookup(FINISHED_HYPERLINK)
        .is_some()
}

/// A literal command within the shell integration budget. Tabs retain their
/// terminal stop width, and LF is the only line break.
pub fn supports_finished_command(command: &str) -> bool {
    !command.trim().is_empty()
        && command.len() <= MAX_OSC133_COMMAND_BYTES
        && command.chars().all(|ch| {
            ch.is_ascii_graphic()
                || matches!(ch, ' ' | '\n' | '\t')
                || is_literal_archive_unicode(ch)
        })
}

/// Selectable text and style ranges for a retained command or output.
/// The view owns presentation and selection, with no PTY or mode state.
#[derive(Clone)]
pub struct FinishedTextView {
    widget: gtk4::TextView,
    font: gtk4::TextTag,
    base_font: Rc<RefCell<pango::FontDescription>>,
    scale: Rc<Cell<f64>>,
    colors: [(gtk4::TextTag, Option<usize>); 6],
    styles: Rc<DocumentStyles>,
    hyperlink_tag: gtk4::TextTag,
    links: Rc<RefCell<Vec<PresentedLink>>>,
    background_style: Rc<RefCell<Option<gtk4::CssProvider>>>,
}

struct DocumentStyles {
    tags: RefCell<Vec<(gtk4::TextTag, ArchiveStyle)>>,
    palette: Cell<[gdk::RGBA; 16]>,
    default_colors: Cell<(gdk::RGBA, gdk::RGBA)>,
    blink_hidden: Cell<bool>,
    blink_timer: RefCell<Option<glib::SourceId>>,
}

impl DocumentStyles {
    fn apply(&self, tag: &gtk4::TextTag, style: ArchiveStyle) {
        set_span_paint(
            tag,
            style,
            &self.palette.get(),
            self.default_colors.get(),
            self.blink_hidden.get(),
        );
    }

    fn apply_blink(&self) {
        for (tag, style) in self
            .tags
            .borrow()
            .iter()
            .filter(|(_, style)| style.flags.blink())
        {
            self.apply(tag, *style);
        }
    }

    fn stop_blink(&self) {
        if let Some(timer) = self.blink_timer.borrow_mut().take() {
            timer.remove();
        }
        if self.blink_hidden.replace(false) {
            self.apply_blink();
        }
    }

    fn start_blink(self: &Rc<Self>, widget: &gtk4::TextView) {
        if !widget.is_mapped()
            || self.blink_timer.borrow().is_some()
            || !self
                .tags
                .borrow()
                .iter()
                .any(|(_, style)| style.flags.blink() && !style.flags.concealed())
        {
            return;
        }
        let weak = Rc::downgrade(self);
        self.blink_timer.replace(Some(glib::timeout_add_local(
            Duration::from_millis(600),
            move || {
                let Some(styles) = weak.upgrade() else {
                    return glib::ControlFlow::Break;
                };
                styles.blink_hidden.set(!styles.blink_hidden.get());
                styles.apply_blink();
                glib::ControlFlow::Continue
            },
        )));
    }
}

impl Drop for DocumentStyles {
    fn drop(&mut self) {
        if let Some(timer) = self.blink_timer.get_mut().take() {
            timer.remove();
        }
    }
}

impl FinishedTextView {
    /// Present an already bounded and display-sanitized heading. This path
    /// inserts literal text without interpreting a terminal capture or markup.
    pub fn plain_command(
        command: &str,
        font: pango::FontDescription,
        scale: f64,
        foreground: gdk::RGBA,
        palette: &[gdk::RGBA; 16],
    ) -> Self {
        Self::from_runs(vec![(command, 0)], font, scale, foreground, palette, false)
    }

    pub fn command(
        command: &str,
        highlighted: &[u8],
        font: pango::FontDescription,
        scale: f64,
        foreground: gdk::RGBA,
        palette: &[gdk::RGBA; 16],
    ) -> Option<Self> {
        if !supports_finished_command(command) {
            return None;
        }
        let highlighted = std::str::from_utf8(highlighted).ok()?;
        // Both GTK hosts prepare terminal line breaks for their VTE renderer.
        // The document keeps the original shell command's logical newlines.
        let highlighted = if highlighted.contains('\r') {
            Cow::Owned(highlighted.replace("\r\n", "\n"))
        } else {
            Cow::Borrowed(highlighted)
        };
        let mut source = command;
        let mut remaining = highlighted.as_ref();
        let mut runs = Vec::new();
        let mut style = 0;
        while !remaining.is_empty() {
            if let Some(escape) = remaining.strip_prefix("\x1b[") {
                let (code, tail) = escape.split_once('m')?;
                style = match code {
                    "0" => 0,
                    "32" => 1,
                    "35" => 2,
                    "90" => 3,
                    "36" => 4,
                    "1;36" => 5,
                    _ => return None,
                };
                remaining = tail;
            } else {
                let end = remaining.find('\x1b').unwrap_or(remaining.len());
                if end == 0 {
                    return None;
                }
                let text = &remaining[..end];
                source = source.strip_prefix(text)?;
                runs.push((text, style));
                remaining = &remaining[end..];
            }
        }
        if !source.is_empty() {
            return None;
        }

        Some(Self::from_runs(
            runs, font, scale, foreground, palette, false,
        ))
    }

    /// Display a historical ANSI projection only when its complete text
    /// agrees with the retained command. The caller owns the record's provenance.
    pub fn archived_command(
        command: &str,
        document: &ArchivedText,
        font: pango::FontDescription,
        scale: f64,
        foreground: gdk::RGBA,
        palette: &[gdk::RGBA; 16],
    ) -> Option<Self> {
        if document.text != command || !supports_finished_command(command) {
            return None;
        }
        let view = Self::from_runs(Vec::new(), font, scale, foreground, palette, false);
        view.present_output(document);
        Some(view)
    }

    pub fn output(
        document: &ArchivedText,
        font: pango::FontDescription,
        scale: f64,
        foreground: gdk::RGBA,
        palette: &[gdk::RGBA; 16],
    ) -> Self {
        let view = Self::from_runs(Vec::new(), font, scale, foreground, palette, true);
        view.present_output(document);
        view
    }

    fn from_runs(
        runs: Vec<(&str, usize)>,
        font: pango::FontDescription,
        scale: f64,
        foreground: gdk::RGBA,
        palette: &[gdk::RGBA; 16],
        is_output: bool,
    ) -> Self {
        let buffer = gtk4::TextBuffer::new(None);
        let font_tag = gtk4::TextTag::builder().build();
        buffer.tag_table().add(&font_tag);
        let hyperlink_tag = gtk4::TextTag::builder().name(FINISHED_HYPERLINK).build();
        buffer.tag_table().add(&hyperlink_tag);
        let colors = [None, Some(2), Some(5), Some(8), Some(6), Some(6)]
            .map(|color| (gtk4::TextTag::builder().build(), color));
        for (tag, _) in &colors {
            buffer.tag_table().add(tag);
        }
        colors[5].0.set_weight(700);
        for (text, style) in runs {
            buffer.insert_with_tags(&mut buffer.end_iter(), text, &[&font_tag, &colors[style].0]);
        }
        let widget = gtk4::TextView::builder()
            .buffer(&buffer)
            .editable(false)
            .cursor_visible(false)
            .monospace(true)
            .wrap_mode(gtk4::WrapMode::Char)
            .hexpand(true)
            .build();
        let (name, label) = if is_output {
            ("shared-finished-output", "Completed command output")
        } else {
            ("shared-finished-command", "Completed command")
        };
        widget.set_widget_name(name);
        widget.update_property(&[gtk4::accessible::Property::Label(label)]);
        install_command_style(&widget.display());
        let view = Self {
            widget,
            font: font_tag,
            base_font: Rc::new(RefCell::new(font)),
            scale: Rc::new(Cell::new(scale)),
            colors,
            styles: Rc::new(DocumentStyles {
                tags: RefCell::new(Vec::new()),
                palette: Cell::new(*palette),
                default_colors: Cell::new((foreground, gdk::RGBA::BLACK)),
                blink_hidden: Cell::new(false),
                blink_timer: RefCell::new(None),
            }),
            hyperlink_tag,
            links: Rc::new(RefCell::new(Vec::new())),
            background_style: Rc::new(RefCell::new(None)),
        };
        let styles = view.styles.clone();
        view.widget
            .connect_map(move |widget| styles.start_blink(widget));
        let styles = view.styles.clone();
        view.widget.connect_unmap(move |_| styles.stop_blink());
        view.apply_font();
        view.set_colors(foreground, palette);
        view
    }

    pub fn widget(&self) -> &gtk4::TextView {
        &self.widget
    }

    /// For a finished document, the plain link finder receives the logical
    /// document and a byte offset. Soft wrapping in GTK does not change either.
    pub fn install_links(&self, actions: LinkActions) {
        let view = self.widget.downgrade();
        let links = self.links.clone();
        let open = (actions.attach_opener)(self.widget.upcast_ref());
        connect_links(
            &self.widget,
            move |x, y| {
                let target =
                    finished_link_at(&view.upgrade()?, &links.borrow(), actions.find_plain, x, y)?;
                (actions.admit)("", &target.target).then_some(target)
            },
            open,
        );
    }

    /// Line labels share the wrapped text's allocated row geometry.
    #[allow(deprecated)]
    pub fn output_gutter(&self) -> gtk4::DrawingArea {
        let gutter = gtk4::DrawingArea::new();
        gutter.set_content_width(28);
        gutter.set_can_target(false);
        gutter.add_css_class("shared-finished-output-gutter");
        let view = self.widget.downgrade();
        let font = self.font.clone();
        gutter.set_draw_func(move |area, context, width, height| {
            let Some(view) = view.upgrade() else {
                return;
            };
            let visible = view.visible_rect();
            let (mut iter, _) = view.line_at_y(visible.y());
            loop {
                let (y, _) = view.line_yrange(&iter);
                let (_, y) = view.buffer_to_window_coords(gtk4::TextWindowType::Widget, 0, y);
                if y >= height {
                    break;
                }
                let layout = area.create_pango_layout(Some(&(iter.line() + 1).to_string()));
                layout.set_font_description(font.font_desc().as_ref());
                let (label_width, _) = layout.pixel_size();
                gtk4::render_layout(
                    &area.style_context(),
                    context,
                    f64::from((width - label_width).max(0)),
                    f64::from(y),
                    &layout,
                );
                if !iter.forward_line() {
                    break;
                }
            }
        });
        let weak = gutter.downgrade();
        self.widget.buffer().connect_changed(move |_| {
            if let Some(gutter) = weak.upgrade() {
                gutter.queue_draw();
            }
        });
        if let Some(adjustment) = self.widget.vadjustment() {
            let weak = gutter.downgrade();
            adjustment.connect_value_changed(move |_| {
                if let Some(gutter) = weak.upgrade() {
                    gutter.queue_draw();
                }
            });
        }
        gutter
    }

    /// Replace the view with a filtered projection of the retained capture.
    pub fn present_output(&self, document: &ArchivedText) {
        self.styles.stop_blink();
        let buffer = self.widget.buffer();
        buffer.set_text("");
        let mut tags = self.styles.tags.borrow_mut();
        let table = buffer.tag_table();
        for (tag, _) in tags.drain(..) {
            table.remove(&tag);
        }
        buffer.insert_with_tags(
            &mut buffer.end_iter(),
            &document.text,
            &[&self.font, &self.colors[0].0],
        );
        let mut offsets = (0, 0);
        let links = document
            .links
            .iter()
            .filter_map(|link| {
                let (start, end) =
                    ordered_character_range(&document.text, link.bytes.clone(), &mut offsets)?;
                buffer.apply_tag(
                    &self.hyperlink_tag,
                    &buffer.iter_at_offset(start),
                    &buffer.iter_at_offset(end),
                );
                Some(PresentedLink::text_span(start, end, &link.target))
            })
            .collect();
        self.links.replace(links);
        let mut offsets = (0, 0);
        for span in &document.spans {
            let Some((start, end)) =
                ordered_character_range(&document.text, span.bytes.clone(), &mut offsets)
            else {
                continue;
            };
            let tag = gtk4::TextTag::builder()
                .weight(if span.style.flags.bold() { 700 } else { 400 })
                .style(if span.style.flags.italic() {
                    pango::Style::Italic
                } else {
                    pango::Style::Normal
                })
                .build();
            self.styles.apply(&tag, span.style);
            table.add(&tag);
            buffer.apply_tag(
                &tag,
                &buffer.iter_at_offset(start),
                &buffer.iter_at_offset(end),
            );
            tags.push((tag, span.style));
        }
        drop(tags);
        self.styles.start_blink(&self.widget);
    }

    pub fn set_font(&self, font: &pango::FontDescription) {
        *self.base_font.borrow_mut() = font.clone();
        self.apply_font();
    }

    pub fn set_font_scale(&self, scale: f64) {
        self.scale.set(scale);
        self.apply_font();
    }

    pub fn set_foreground(&self, foreground: gdk::RGBA) {
        self.colors[0].0.set_foreground_rgba(Some(&foreground));
        self.styles
            .default_colors
            .set((foreground, self.styles.default_colors.get().1));
        self.update_archive_colors();
    }

    /// The terminal backdrop used for inverse video. The document can still
    /// have a transparent widget background inside its card.
    pub fn set_terminal_background(&self, background: gdk::RGBA) {
        self.styles
            .default_colors
            .set((self.styles.default_colors.get().0, background));
        self.update_archive_colors();
    }

    #[allow(deprecated)]
    pub fn set_background(&self, background: Option<gdk::RGBA>) {
        let mut style = self.background_style.borrow_mut();
        if let Some(background) = background {
            let provider = style.get_or_insert_with(|| {
                let provider = gtk4::CssProvider::new();
                self.widget
                    .style_context()
                    .add_provider(&provider, gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION + 1);
                provider
            });
            provider.load_from_string(&format!("textview {{ background-color: {background}; }}"));
        } else if let Some(provider) = style.take() {
            self.widget.style_context().remove_provider(&provider);
        }
    }

    pub fn natural_width(&self) -> i32 {
        let buffer = self.widget.buffer();
        let text = buffer.text(&buffer.start_iter(), &buffer.end_iter(), true);
        let layout = self.widget.create_pango_layout(Some(&text));
        layout.set_font_description(self.font.font_desc().as_ref());
        layout.set_tabs(self.widget.tabs().as_ref());
        layout.pixel_size().0
    }

    /// Pixel height of one row in the view's scaled terminal font.
    pub fn line_height(&self) -> i32 {
        let metrics = self
            .widget
            .pango_context()
            .metrics(self.font.font_desc().as_ref(), None);
        ((metrics.ascent() + metrics.descent() + pango::SCALE - 1) / pango::SCALE).max(1)
    }

    pub fn set_colors(&self, foreground: gdk::RGBA, palette: &[gdk::RGBA; 16]) {
        self.styles.palette.set(*palette);
        self.styles
            .default_colors
            .set((foreground, self.styles.default_colors.get().1));
        for (tag, color) in &self.colors {
            let color = color.map_or(foreground, |index| palette[index]);
            tag.set_foreground_rgba(Some(&color));
        }
        self.update_archive_colors();
    }

    fn update_archive_colors(&self) {
        for (tag, style) in self.styles.tags.borrow().iter() {
            self.styles.apply(tag, *style);
        }
    }

    fn apply_font(&self) {
        let mut font = self.base_font.borrow().clone();
        let scaled = (f64::from(font.size()) * self.scale.get()).round();
        if font.is_size_absolute() {
            font.set_absolute_size(scaled);
        } else {
            font.set_size(scaled as i32);
        }
        let tab_layout = self.widget.create_pango_layout(Some("        "));
        tab_layout.set_font_description(Some(&font));
        let tab_width = tab_layout.size().0.max(1);
        let mut tabs = pango::TabArray::new(2, false);
        tabs.set_tab(0, pango::TabAlign::Left, tab_width);
        tabs.set_tab(1, pango::TabAlign::Left, tab_width.saturating_mul(2));
        self.widget.set_tabs(&tabs);
        self.font.set_font_desc(Some(&font));
        self.widget.queue_resize();
    }
}

/// Locate ordinary text in a finished command or output. Explicit hyperlinks
/// belong to the link handler even when their displayed label looks like a file.
pub fn finished_text_at(view: &gtk4::TextView, x: f64, y: f64) -> Option<(String, usize)> {
    let tag = view.buffer().tag_table().lookup(FINISHED_HYPERLINK)?;
    let iter = text_at_point(view, x, y)?;
    if iter.has_tag(&tag) {
        return None;
    }
    document_offset(view, &iter)
}

fn document_offset(view: &gtk4::TextView, iter: &gtk4::TextIter) -> Option<(String, usize)> {
    let buffer = view.buffer();
    if buffer.char_count() > MAX_REFERENCE_DOCUMENT_CHARS {
        return None;
    }
    let text = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false);
    let (byte, _) = text.char_indices().nth(iter.offset() as usize)?;
    Some((text.to_string(), byte))
}

fn finished_link_at(
    view: &gtk4::TextView,
    links: &[PresentedLink],
    find_plain: Option<PlainLinkFinder>,
    x: f64,
    y: f64,
) -> Option<PresentedLink> {
    let iter = text_at_point(view, x, y)?;
    if let Some(link) = link_for_offset(links, iter.offset()) {
        return Some(link);
    }
    let (text, byte) = document_offset(view, &iter)?;
    let range = find_plain?(&text, byte)?;
    if !range.contains(&byte) {
        return None;
    }
    let start = text.get(..range.start)?.chars().count() as i32;
    let target = text.get(range)?;
    let end = start + target.chars().count() as i32;
    Some(PresentedLink::text_span(start, end, target))
}

fn set_span_paint(
    tag: &gtk4::TextTag,
    style: ArchiveStyle,
    palette: &[gdk::RGBA; 16],
    defaults: (gdk::RGBA, gdk::RGBA),
    blink_hidden: bool,
) {
    let color =
        |value| (value != Color::Default).then(|| terminal_color(value, gdk::RGBA::BLACK, palette));
    let mut foreground = color(style.foreground);
    let mut background = color(style.background);
    if style.flags.dim() && !matches!(style.foreground, Color::Rgb(..)) {
        foreground = Some(faint_palette_color(foreground.unwrap_or(defaults.0)));
    }
    if style.flags.inverse() {
        let mut ink = background.unwrap_or(defaults.1);
        ink.set_alpha(1.0);
        background = Some(foreground.unwrap_or(defaults.0));
        foreground = Some(ink);
    }
    let hidden = style.flags.concealed() || (style.flags.blink() && blink_hidden);
    let transparent = gdk::RGBA::new(0., 0., 0., 0.);
    tag.set_foreground_rgba(if hidden {
        Some(&transparent)
    } else {
        foreground.as_ref()
    });
    tag.set_background_rgba(background.as_ref());
    tag.set_underline_rgba(color(style.underline_color).as_ref());
    tag.set_underline(match (hidden, style.flags.underline()) {
        (false, UnderlineStyle::Single) => pango::Underline::Single,
        (false, UnderlineStyle::Double) => pango::Underline::Double,
        (false, UnderlineStyle::Curly) => pango::Underline::Error,
        _ => pango::Underline::None,
    });
    tag.set_strikethrough(!hidden && style.flags.strikethrough());
    tag.set_overline(if !hidden && style.flags.overline() {
        pango::Overline::Single
    } else {
        pango::Overline::None
    });
}

fn install_command_style(display: &gdk::Display) {
    thread_local! {
        static DISPLAYS: std::cell::RefCell<Vec<gdk::Display>> = const { std::cell::RefCell::new(Vec::new()) };
    }
    DISPLAYS.with(|displays| {
        let mut displays = displays.borrow_mut();
        if displays.contains(display) {
            return;
        }
        let css = gtk4::CssProvider::new();
        css.load_from_string(
            "#shared-finished-command, #shared-finished-command text, #shared-finished-output, #shared-finished-output text { background-color: transparent; } .shared-finished-output-gutter { color: #8da0bb; }",
        );
        gtk4::style_context_add_provider_for_display(
            display,
            &css,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
        displays.push(display.clone());
    });
}

const MATCH_TAG: &str = "shared-finished-text-match";

pub fn clear_finished_text_matches(view: &gtk4::TextView) {
    let buffer = view.buffer();
    if let Some(tag) = buffer.tag_table().lookup(MATCH_TAG) {
        buffer.remove_tag(&tag, &buffer.start_iter(), &buffer.end_iter());
    }
}

pub fn paint_finished_text_matches(
    view: &gtk4::TextView,
    matches: impl Iterator<Item = Range<usize>>,
) {
    clear_finished_text_matches(view);
    let buffer = view.buffer();
    let tag = buffer.tag_table().lookup(MATCH_TAG).unwrap_or_else(|| {
        let tag = gtk4::TextTag::builder()
            .name(MATCH_TAG)
            .background("#82511d")
            .foreground("#ffffff")
            .build();
        buffer.tag_table().add(&tag);
        tag
    });
    tag.set_priority(buffer.tag_table().size() - 1);
    let text = buffer.text(&buffer.start_iter(), &buffer.end_iter(), true);
    for range in matches {
        if let Some((start, end)) = character_range(&text, range) {
            buffer.apply_tag(
                &tag,
                &buffer.iter_at_offset(start),
                &buffer.iter_at_offset(end),
            );
        }
    }
}

fn character_range(text: &str, range: Range<usize>) -> Option<(i32, i32)> {
    let start = text.get(..range.start)?.chars().count() as i32;
    let end = start + text.get(range)?.chars().count() as i32;
    Some((start, end))
}

fn ordered_character_range(
    text: &str,
    range: Range<usize>,
    cursor: &mut (usize, i32),
) -> Option<(i32, i32)> {
    let (byte, character) = if range.start >= cursor.0 {
        *cursor
    } else {
        (0, 0)
    };
    let start = character + text.get(byte..range.start)?.chars().count() as i32;
    let last_byte = range.end;
    let end = start + text.get(range)?.chars().count() as i32;
    *cursor = (last_byte, end);
    Some((start, end))
}

/// Translate byte ranges of the source snapshot into GTK character offsets.
/// The source is immutable while presented, so ordinals survive wrapping.
pub fn select_finished_text_bytes(view: &gtk4::TextView, range: Range<usize>) -> bool {
    let buffer = view.buffer();
    let text = buffer.text(&buffer.start_iter(), &buffer.end_iter(), true);
    let Some((start, end)) = character_range(&text, range) else {
        return false;
    };
    let mut start = buffer.iter_at_offset(start);
    // Keep Find selections independent of X11 PRIMARY ownership changes.
    // Restore GTK's normal publication before the next user selection.
    let primary = view.is_realized().then(|| view.primary_clipboard());
    if let Some(clipboard) = &primary {
        buffer.remove_selection_clipboard(clipboard);
    }
    buffer.select_range(&buffer.iter_at_offset(end), &start);
    if let Some(clipboard) = &primary {
        buffer.add_selection_clipboard(clipboard);
    }
    if view
        .vadjustment()
        .is_some_and(|adjustment| adjustment.upper() - adjustment.lower() > adjustment.page_size())
    {
        view.scroll_to_iter(&mut start, 0.05, false, 0.0, 0.0);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_profile_accepts_literal_paths_and_multiline_commands() {
        assert!(supports_finished_command("echo needle needle"));
        assert!(supports_finished_command("printf '%s' value"));
        assert!(supports_finished_command("echo one\ttwo"));
        assert!(supports_finished_command("echo 好 e\u{301} 👩‍💻"));
        assert!(supports_finished_command(
            "  echo leading and trailing \t\n"
        ));
        assert!(supports_finished_command(
            "cd ./src && \\\nrg file.rs:9 https://example.test"
        ));
        for command in [
            "",
            " \t\n",
            "echo one\rtwo",
            "echo \x1b[31mcolor",
            "echo \u{202e}unsafe",
        ] {
            assert!(!supports_finished_command(command), "{command:?}");
        }
    }

    #[test]
    #[ignore = "requires a GTK display"]
    fn finished_documents_keep_tab_stops_and_recolor_inverse_video() {
        use jterm_engine::archived_text::ArchiveSpan;

        gtk4::init().expect("GTK display");
        let palette = crate::default_ansi_palette();
        let font = pango::FontDescription::from_string("Monospace 13");
        let foreground = gdk::RGBA::new(0.82, 0.81, 0.80, 1.0);
        let background = gdk::RGBA::new(0.15, 0.25, 0.35, 1.0);
        let tabbed =
            FinishedTextView::command("A\tB", b"A\tB", font.clone(), 1.0, foreground, &palette)
                .expect("tab is part of the literal command");
        let spaces = FinishedTextView::command(
            "A       B",
            b"A       B",
            font.clone(),
            1.0,
            foreground,
            &palette,
        )
        .unwrap();
        assert_eq!(tabbed.natural_width(), spaces.natural_width());
        tabbed.set_font_scale(1.6);
        spaces.set_font_scale(1.6);
        assert_eq!(tabbed.natural_width(), spaces.natural_width());
        let buffer = tabbed.widget().buffer();
        assert_eq!(
            buffer.text(&buffer.start_iter(), &buffer.end_iter(), true),
            "A\tB"
        );

        let mut inverse = ArchiveStyle::default();
        inverse.flags.set_inverse(true);
        let archive = ArchivedText {
            text: "selection".to_owned(),
            spans: vec![ArchiveSpan {
                bytes: 0..9,
                style: inverse,
            }],
            ..ArchivedText::default()
        };
        let view = FinishedTextView::output(&archive, font, 1.0, foreground, &palette);
        view.set_terminal_background(background);
        let tag = view.styles.tags.borrow()[0].0.clone();
        assert_eq!(tag.foreground_rgba(), Some(background));
        assert_eq!(tag.background_rgba(), Some(foreground));
        let new_foreground = gdk::RGBA::WHITE;
        view.set_colors(new_foreground, &palette);
        assert_eq!(tag.foreground_rgba(), Some(background));
        assert_eq!(tag.background_rgba(), Some(new_foreground));
    }

    #[test]
    #[ignore = "requires a GTK display"]
    fn finished_documents_dim_default_and_indexed_ink_before_inverse() {
        use jterm_engine::archived_text::ArchiveSpan;

        gtk4::init().expect("GTK display");
        let palette = crate::default_ansi_palette();
        let font = pango::FontDescription::from_string("Monospace 13");
        let background = gdk::RGBA::new(0.1, 0.2, 0.3, 1.0);
        let mut style = ArchiveStyle::default();
        style.flags.set_dim(true);
        style.flags.set_inverse(true);
        let archive = ArchivedText {
            text: "abc".to_owned(),
            spans: [Color::Default, Color::Indexed(196), Color::Rgb(77, 88, 99)]
                .into_iter()
                .enumerate()
                .map(|(start, foreground)| ArchiveSpan {
                    bytes: start..start + 1,
                    style: ArchiveStyle {
                        foreground,
                        ..style
                    },
                })
                .collect(),
            ..ArchivedText::default()
        };
        let view = FinishedTextView::output(&archive, font, 1.0, gdk::RGBA::WHITE, &palette);
        view.set_terminal_background(background);
        let tags = view.styles.tags.borrow();
        assert!(tags
            .iter()
            .all(|(tag, _)| tag.foreground_rgba() == Some(background)));
        assert_eq!(
            tags[0].0.background_rgba(),
            Some(gdk::RGBA::new(2.0 / 3.0, 2.0 / 3.0, 2.0 / 3.0, 1.0))
        );
        assert_eq!(
            tags[1].0.background_rgba(),
            Some(gdk::RGBA::new(2.0 / 3.0, 0.0, 0.0, 1.0))
        );
        assert_eq!(
            tags[2].0.background_rgba(),
            Some(terminal_color(
                Color::Rgb(77, 88, 99),
                gdk::RGBA::WHITE,
                &palette
            ))
        );
    }

    #[test]
    fn command_profile_matches_the_shell_integration_budget() {
        let command = "x".repeat(MAX_OSC133_COMMAND_BYTES);
        assert!(supports_finished_command(&command));
        assert!(!supports_finished_command(&(command + "x")));
    }

    #[test]
    #[ignore = "requires a GTK display"]
    fn oversized_plain_heading_remains_a_selectable_command_document() {
        gtk4::init().expect("GTK display");
        let command = format!(
            "echo {}\n\t查找 \\x1b[31m",
            "x".repeat(MAX_OSC133_COMMAND_BYTES)
        );
        assert!(!supports_finished_command(&command));
        let view = FinishedTextView::plain_command(
            &command,
            pango::FontDescription::from_string("Monospace 12"),
            1.0,
            gdk::RGBA::WHITE,
            &crate::default_ansi_palette(),
        );
        assert_eq!(view.widget().widget_name(), "shared-finished-command");
        assert!(!view.widget().is_editable());
        let buffer = view.widget().buffer();
        assert_eq!(
            buffer.text(&buffer.start_iter(), &buffer.end_iter(), true),
            command
        );
        let word = command.find("查找").unwrap();
        assert!(select_finished_text_bytes(
            view.widget(),
            word..word + "查找".len()
        ));
        let (start, end) = buffer.selection_bounds().unwrap();
        assert_eq!(buffer.text(&start, &end, true), "查找");
    }

    #[test]
    #[ignore = "requires a GTK display"]
    fn archive_blink_and_conceal_keep_selectable_output_and_stop_when_unmapped() {
        use jterm_engine::archived_text::ArchiveSpan;
        use std::time::Instant;

        gtk4::init().expect("GTK display");
        let mut blink = ArchiveStyle::default();
        blink.foreground = Color::Indexed(33);
        blink.flags.set_blink(true);
        blink.flags.set_overline(true);
        let mut conceal = ArchiveStyle::default();
        conceal.flags.set_concealed(true);
        let archive = ArchivedText {
            text: "ab".into(),
            spans: vec![
                ArchiveSpan {
                    bytes: 0..1,
                    style: blink,
                },
                ArchiveSpan {
                    bytes: 1..2,
                    style: conceal,
                },
            ],
            ..ArchivedText::default()
        };
        let view = FinishedTextView::output(
            &archive,
            pango::FontDescription::from_string("Monospace 12"),
            1.0,
            gdk::RGBA::WHITE,
            &crate::default_ansi_palette(),
        );
        let blinking = view.styles.tags.borrow()[0].0.clone();
        let concealed = view.styles.tags.borrow()[1].0.clone();
        assert_eq!(blinking.overline(), pango::Overline::Single);
        assert_eq!(concealed.foreground_rgba().unwrap().alpha(), 0.0);
        assert!(view.styles.blink_timer.borrow().is_none());
        let window = gtk4::Window::builder().child(view.widget()).build();
        window.present();
        let context = glib::MainContext::default();
        let limit = Instant::now() + Duration::from_secs(3);
        while !view.styles.blink_hidden.get() {
            assert!(Instant::now() < limit, "mapped blink phase did not advance");
            context.iteration(false);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(blinking.foreground_rgba().unwrap().alpha(), 0.0);
        assert_eq!(blinking.overline(), pango::Overline::None);
        assert!(select_finished_text_bytes(view.widget(), 0..2));
        let buffer = view.widget().buffer();
        let (start, end) = buffer.selection_bounds().unwrap();
        assert_eq!(buffer.text(&start, &end, true), "ab");
        window.set_visible(false);
        assert!(view.styles.blink_timer.borrow().is_none());
        assert_eq!(blinking.overline(), pango::Overline::Single);
    }

    #[test]
    #[ignore = "requires a GTK display"]
    fn mapped_command_preserves_styled_source_and_range_offsets() {
        gtk4::init().expect("GTK display");
        let font = pango::FontDescription::from_string("Monospace 12");
        let foreground = gdk::RGBA::WHITE;
        let palette = [gdk::RGBA::RED; 16];
        let command = "echo hit hit \\\n./script https://example.test";
        let view = FinishedTextView::command(
            command,
            b"\x1b[1;36mecho\x1b[0m hit hit \\\r\n./script https://example.test",
            font.clone(),
            1.0,
            foreground,
            &palette,
        )
        .expect("reproduce the original command");
        assert!(FinishedTextView::command(
            "echo source",
            b"\x1b[1;36mecho\x1b[0m mismatch",
            font,
            1.0,
            foreground,
            &palette,
        )
        .is_none());
        let window = gtk4::Window::builder()
            .default_width(320)
            .default_height(100)
            .child(view.widget())
            .build();
        window.present();
        let main = gtk4::glib::MainContext::default();
        while main.pending() {
            main.iteration(false);
        }
        let buffer = view.widget().buffer();
        assert!(view.widget().is_mapped());
        assert_eq!(
            buffer.text(&buffer.start_iter(), &buffer.end_iter(), true),
            command
        );
        const URL: &str = "https://example.test";
        let offset = command.find(URL).unwrap() as i32;
        let rect = view.widget().iter_location(&buffer.iter_at_offset(offset));
        let (x, y) = view.widget().buffer_to_window_coords(
            gtk4::TextWindowType::Widget,
            rect.x() + rect.width() / 2,
            rect.y() + rect.height() / 2,
        );
        let link = finished_link_at(
            view.widget(),
            &[],
            Some(|line, offset| {
                let start = line.find(URL)?;
                let span = start..start + URL.len();
                span.contains(&offset).then_some(span)
            }),
            f64::from(x),
            f64::from(y),
        )
        .expect("resolve a link in the second logical line");
        assert_eq!(link.target.as_ref(), URL);
        assert_eq!(
            finished_text_at(view.widget(), f64::from(x), f64::from(y)),
            Some((command.to_owned(), offset as usize))
        );
        assert!(buffer
            .start_iter()
            .tags()
            .iter()
            .any(|tag| tag.weight() == 700));
        paint_finished_text_matches(view.widget(), [5..8, 9..12].into_iter());
        let painted = buffer.tag_table().lookup(MATCH_TAG).unwrap();
        assert!(buffer.iter_at_offset(5).has_tag(&painted));
        assert!(select_finished_text_bytes(view.widget(), 9..12));
        let (start, end) = buffer.selection_bounds().unwrap();
        assert_eq!((start.offset(), end.offset()), (9, 12));
        assert_eq!(buffer.text(&start, &end, true), "hit");
        clear_finished_text_matches(view.widget());
        assert!(!buffer.iter_at_offset(5).has_tag(&painted));
        view.set_font_scale(1.5);
        assert_eq!(view.font.font_desc().unwrap().size(), 18 * pango::SCALE);
        let source = "文档 ./notes.md";
        view.present_output(&ArchivedText {
            text: source.into(),
            links: vec![jterm_engine::archived_text::ArchiveLink {
                bytes: source.find("./").unwrap()..source.len(),
                target: URL.into(),
            }],
            ..ArchivedText::default()
        });
        while main.pending() {
            main.iteration(false);
        }
        let rect = view.widget().iter_location(&buffer.iter_at_offset(4));
        let (x, y) = view.widget().buffer_to_window_coords(
            gtk4::TextWindowType::Widget,
            rect.x() + rect.width() / 2,
            rect.y() + rect.height() / 2,
        );
        assert!(finished_text_at(view.widget(), f64::from(x), f64::from(y)).is_none());
        let link = finished_link_at(
            view.widget(),
            &view.links.borrow(),
            None,
            x.into(),
            y.into(),
        )
        .expect("explicit hyperlink covering a path-like label");
        assert_eq!(link.target.as_ref(), URL);
        window.close();
    }
}
