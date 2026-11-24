use gtk4::{gdk, pango, prelude::*};
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
};

use crate::{
    text_link::connect_links,
    view::{link_at, terminal_color, PresentedLink},
    LinkActions,
};

/// A literal command within the shell integration budget, with LF as the only
/// permitted line break.
pub fn supports_finished_command(command: &str) -> bool {
    !command.is_empty()
        && command == command.trim()
        && command.len() <= MAX_OSC133_COMMAND_BYTES
        && command.chars().all(|ch| {
            ch.is_ascii_graphic() || matches!(ch, ' ' | '\n') || is_literal_archive_unicode(ch)
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
    output_tags: Rc<RefCell<Vec<(gtk4::TextTag, ArchiveStyle)>>>,
    links: Rc<RefCell<Vec<PresentedLink>>>,
    palette: Rc<Cell<[gdk::RGBA; 16]>>,
    background_style: Rc<RefCell<Option<gtk4::CssProvider>>>,
}

impl FinishedTextView {
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
            output_tags: Rc::new(RefCell::new(Vec::new())),
            links: Rc::new(RefCell::new(Vec::new())),
            palette: Rc::new(Cell::new(*palette)),
            background_style: Rc::new(RefCell::new(None)),
        };
        view.apply_font();
        view.set_colors(foreground, palette);
        view
    }

    pub fn widget(&self) -> &gtk4::TextView {
        &self.widget
    }

    pub fn install_links(&self, actions: LinkActions) {
        let view = self.widget.downgrade();
        let links = self.links.clone();
        let open = (actions.attach_opener)(self.widget.upcast_ref());
        connect_links(
            &self.widget,
            move |x, y| {
                let target = link_at(&view.upgrade()?, &links.borrow(), actions.find_plain, x, y)?;
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
        let buffer = self.widget.buffer();
        let mut offsets = (0, 0);
        let links = document
            .links
            .iter()
            .filter_map(|link| {
                let (start, end) =
                    ordered_character_range(&document.text, link.bytes.clone(), &mut offsets)?;
                Some(PresentedLink::text_span(start, end, &link.target))
            })
            .collect();
        self.links.replace(links);
        buffer.set_text("");
        let mut tags = self.output_tags.borrow_mut();
        let table = buffer.tag_table();
        for (tag, _) in tags.drain(..) {
            table.remove(&tag);
        }
        buffer.insert_with_tags(
            &mut buffer.end_iter(),
            &document.text,
            &[&self.font, &self.colors[0].0],
        );
        let palette = self.palette.get();
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
                .underline(match span.style.flags.underline() {
                    UnderlineStyle::Single => pango::Underline::Single,
                    UnderlineStyle::Double => pango::Underline::Double,
                    UnderlineStyle::Curly => pango::Underline::Error,
                    _ => pango::Underline::None,
                })
                .strikethrough(span.style.flags.strikethrough())
                .build();
            set_span_colors(&tag, span.style, &palette);
            table.add(&tag);
            buffer.apply_tag(
                &tag,
                &buffer.iter_at_offset(start),
                &buffer.iter_at_offset(end),
            );
            tags.push((tag, span.style));
        }
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
        self.palette.set(*palette);
        for (tag, color) in &self.colors {
            let color = color.map_or(foreground, |index| palette[index]);
            tag.set_foreground_rgba(Some(&color));
        }
        for (tag, style) in self.output_tags.borrow().iter() {
            set_span_colors(tag, *style, palette);
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
        self.font.set_font_desc(Some(&font));
        self.widget.queue_resize();
    }
}

fn set_span_colors(tag: &gtk4::TextTag, style: ArchiveStyle, palette: &[gdk::RGBA; 16]) {
    let color =
        |value| (value != Color::Default).then(|| terminal_color(value, gdk::RGBA::BLACK, palette));
    tag.set_foreground_rgba(color(style.foreground).as_ref());
    tag.set_background_rgba(color(style.background).as_ref());
    tag.set_underline_rgba(color(style.underline_color).as_ref());
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
        assert!(supports_finished_command("echo 好 e\u{301} 👩‍💻"));
        assert!(supports_finished_command(
            "cd ./src && \\\nrg file.rs:9 https://example.test"
        ));
        for command in [
            "",
            "echo one\rtwo",
            "echo one\ttwo",
            "echo \x1b[31mcolor",
            "echo \u{202e}unsafe",
            "echo trailing ",
        ] {
            assert!(!supports_finished_command(command), "{command:?}");
        }
    }

    #[test]
    fn command_profile_matches_the_shell_integration_budget() {
        let command = "x".repeat(MAX_OSC133_COMMAND_BYTES);
        assert!(supports_finished_command(&command));
        assert!(!supports_finished_command(&(command + "x")));
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
        let link = link_at(
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
        window.close();
    }
}
