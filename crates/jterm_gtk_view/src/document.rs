use gtk4::{gdk, glib, prelude::*};

/// Selectable read-only presentation of a caller-owned history snapshot.
/// The caller retains provenance, retention, and the original output bytes.
pub struct DocumentView {
    root: gtk4::Box,
}

impl DocumentView {
    pub fn new(output: &str) -> Self {
        let root = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
        let buffer = gtk4::TextBuffer::new(None);
        buffer.set_text(output);
        let text = gtk4::TextView::builder()
            .buffer(&buffer)
            .editable(false)
            .cursor_visible(false)
            .monospace(true)
            .left_margin(8)
            .right_margin(8)
            .top_margin(8)
            .bottom_margin(8)
            .build();
        text.set_widget_name("shared-terminal-document");
        text.update_property(&[gtk4::accessible::Property::Label("Retained output")]);
        let scrolled = gtk4::ScrolledWindow::builder()
            .vexpand(true)
            .child(&text)
            .build();

        let search = gtk4::SearchEntry::builder()
            .placeholder_text("Find in output")
            .hexpand(true)
            .build();
        search.update_property(&[gtk4::accessible::Property::Label("Find in output")]);
        let status = gtk4::Label::new(None);
        status.add_css_class("dim-label");
        let next = search_button("go-down-symbolic", "Next match");
        let previous = search_button("go-up-symbolic", "Previous match");
        for button in [&previous, &next] {
            button.set_sensitive(false);
        }
        let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        row.append(&search);
        row.append(&status);
        row.append(&previous);
        row.append(&next);
        root.append(&row);
        root.append(&scrolled);

        let navigate = {
            let (search, text, status, next, previous) = (
                search.downgrade(),
                text.downgrade(),
                status.downgrade(),
                next.downgrade(),
                previous.downgrade(),
            );
            move |direction| {
                let (Some(search), Some(text), Some(status), Some(next), Some(previous)) = (
                    search.upgrade(),
                    text.upgrade(),
                    status.upgrade(),
                    next.upgrade(),
                    previous.upgrade(),
                ) else {
                    return;
                };
                let query = search.text();
                let buffer = text.buffer();
                if query.is_empty() {
                    status.set_text("");
                    next.set_sensitive(false);
                    previous.set_sensitive(false);
                    buffer.place_cursor(&buffer.start_iter());
                    return;
                }
                let hit = find_match(&buffer, &query, direction);
                next.set_sensitive(hit.is_some());
                previous.set_sensitive(hit.is_some());
                if let Some((mut start, end)) = hit {
                    status.set_text("");
                    buffer.select_range(&end, &start);
                    text.scroll_to_iter(&mut start, 0.1, false, 0.0, 0.0);
                } else {
                    status.set_text("No matches");
                    buffer.place_cursor(&buffer.start_iter());
                }
            }
        };
        search.connect_changed({
            let navigate = navigate.clone();
            move |_| navigate(Direction::First)
        });
        next.connect_clicked({
            let navigate = navigate.clone();
            move |_| navigate(Direction::Next)
        });
        previous.connect_clicked({
            let navigate = navigate.clone();
            move |_| navigate(Direction::Previous)
        });
        search.connect_activate(move |_| navigate(Direction::Next));

        let keys = gtk4::EventControllerKey::new();
        keys.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let search = search.downgrade();
        keys.connect_key_pressed(move |_, key, _, modifiers| {
            if key == gdk::Key::f && modifiers.contains(gdk::ModifierType::CONTROL_MASK) {
                if let Some(search) = search.upgrade() {
                    search.grab_focus();
                    return glib::Propagation::Stop;
                }
            }
            glib::Propagation::Proceed
        });
        root.add_controller(keys);
        Self { root }
    }

    pub fn widget(&self) -> &gtk4::Box {
        &self.root
    }
}

#[derive(Clone, Copy)]
enum Direction {
    First,
    Next,
    Previous,
}

fn search_button(icon: &str, label: &str) -> gtk4::Button {
    let button = gtk4::Button::from_icon_name(icon);
    button.set_tooltip_text(Some(label));
    button.update_property(&[gtk4::accessible::Property::Label(label)]);
    button
}

fn find_match(
    buffer: &gtk4::TextBuffer,
    query: &str,
    direction: Direction,
) -> Option<(gtk4::TextIter, gtk4::TextIter)> {
    let flags = gtk4::TextSearchFlags::TEXT_ONLY | gtk4::TextSearchFlags::CASE_INSENSITIVE;
    let start = buffer.start_iter();
    let end = buffer.end_iter();
    let selected = buffer.selection_bounds();
    match direction {
        Direction::First => start.forward_search(query, flags, None),
        Direction::Next => selected
            .and_then(|(_, previous_end)| previous_end.forward_search(query, flags, None))
            .or_else(|| start.forward_search(query, flags, None)),
        Direction::Previous => selected
            .and_then(|(previous_start, _)| previous_start.backward_search(query, flags, None))
            .or_else(|| end.backward_search(query, flags, None)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires a GTK display and clipboard"]
    fn retained_output_is_read_only_searchable_and_selectable() {
        gtk4::init().expect("initialize GTK");
        let output = "match first\n你好\nMATCH last\n";
        let view = DocumentView::new(output);
        let row = view.widget().first_child().unwrap();
        let search = row
            .first_child()
            .unwrap()
            .downcast::<gtk4::SearchEntry>()
            .unwrap();
        let previous = row
            .last_child()
            .unwrap()
            .prev_sibling()
            .unwrap()
            .downcast::<gtk4::Button>()
            .unwrap();
        let next = row
            .last_child()
            .unwrap()
            .downcast::<gtk4::Button>()
            .unwrap();
        let text = view
            .widget()
            .last_child()
            .unwrap()
            .first_child()
            .unwrap()
            .downcast::<gtk4::TextView>()
            .unwrap();
        let window = gtk4::Window::builder()
            .default_width(320)
            .default_height(240)
            .child(view.widget())
            .build();
        window.present();
        let context = glib::MainContext::default();
        while context.pending() {
            context.iteration(false);
        }
        assert!(text.is_mapped());
        assert!(!text.is_editable());
        let buffer = text.buffer();
        let match_start = || buffer.selection_bounds().expect("matched text").0.offset();
        search.set_text("match");
        assert_eq!(match_start(), 0);
        next.emit_clicked();
        assert_eq!(match_start(), 15);
        next.emit_clicked();
        assert_eq!(match_start(), 0);
        previous.emit_clicked();
        assert_eq!(match_start(), 15);
        search.set_text("missing");
        assert!(buffer.selection_bounds().is_none());
        assert!(!next.is_sensitive());
        search.set_text("你好");
        text.grab_focus();
        text.emit_by_name::<()>("copy-clipboard", &[]);
        let copied = context
            .block_on(text.clipboard().read_text_future())
            .unwrap()
            .unwrap();
        assert_eq!(copied, "你好");
        assert_eq!(
            buffer.text(&buffer.start_iter(), &buffer.end_iter(), true),
            output
        );
        window.close();
    }
}
