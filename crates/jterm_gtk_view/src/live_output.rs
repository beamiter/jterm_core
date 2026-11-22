use gtk4::{gdk, glib, graphene, pango, prelude::*, subclass::prelude::*};
use jterm_engine::archived_text::{is_literal_archive_unicode, literal_text_columns};
use std::cell::{Cell, RefCell};

const MAX_LIVE_OUTPUT_ROWS: usize = 8;
pub const MAX_LIVE_OUTPUT_BYTES: usize = MAX_LIVE_OUTPUT_ROWS * 258;

/// Complete, unwrapped rows from the same command capture. The host checks
/// that these rows are visible with default styling in its retained grid.
pub fn captured_live_rows(capture: &str, columns: usize) -> Option<Vec<CapturedLiveRow<'_>>> {
    if capture.len() > MAX_LIVE_OUTPUT_BYTES {
        return None;
    }
    let mut rows = Vec::new();
    for text in capture.strip_suffix("\r\n")?.split("\r\n") {
        let width = literal_text_columns(text);
        if rows.len() == MAX_LIVE_OUTPUT_ROWS
            || width == 0
            || width >= columns
            || text.trim() != text
            || !text.chars().all(|character| {
                character.is_ascii_alphanumeric()
                    || " _-=+,%!?()[]'\"".contains(character)
                    || is_literal_archive_unicode(character)
            })
        {
            return None;
        }
        rows.push(CapturedLiveRow {
            text,
            columns: width,
        });
    }
    Some(rows)
}

#[derive(Debug, PartialEq, Eq)]
pub struct CapturedLiveRow<'a> {
    pub text: &'a str,
    pub columns: usize,
}

pub struct LiveOutputRow<'a> {
    pub text: &'a str,
    pub area: gdk::Rectangle,
}

/// A passive presentation of visible command output. The native terminal
/// remains the source of selection, input and accessibility.
pub struct LiveOutputView {
    native: NativeOutputClip,
    overlay: gtk4::Fixed,
    labels: RefCell<Vec<gtk4::Label>>,
    css: gtk4::CssProvider,
    colors: Cell<Option<(gdk::RGBA, gdk::RGBA)>>,
}

impl LiveOutputView {
    pub fn new(terminal: &impl IsA<gtk4::Widget>) -> Self {
        let native: NativeOutputClip = glib::Object::new();
        terminal.set_parent(&native);
        native
            .imp()
            .terminal
            .replace(Some(terminal.as_ref().clone()));
        let overlay = gtk4::Fixed::builder()
            .accessible_role(gtk4::AccessibleRole::Presentation)
            .can_target(false)
            .focusable(false)
            .hexpand(true)
            .vexpand(true)
            .visible(false)
            .build();
        overlay.set_widget_name("shared-live-command-output");
        let clip = native.downgrade();
        overlay.connect_visible_notify(move |overlay| {
            if !overlay.get_visible() {
                if let Some(clip) = clip.upgrade() {
                    clip.set_excluded_areas(&[]);
                }
            }
        });
        Self {
            native,
            overlay,
            labels: RefCell::new(Vec::new()),
            css: gtk4::CssProvider::new(),
            colors: Cell::new(None),
        }
    }

    pub fn native_surface(&self) -> &gtk4::Widget {
        self.native.upcast_ref()
    }

    pub fn widget(&self) -> &gtk4::Fixed {
        &self.overlay
    }

    pub fn clear(&self) {
        self.overlay.set_visible(false);
    }

    pub fn present(
        &self,
        rows: &[LiveOutputRow<'_>],
        font: &pango::FontDescription,
        foreground: gdk::RGBA,
        background: gdk::RGBA,
    ) {
        if rows.is_empty() || rows.len() > MAX_LIVE_OUTPUT_ROWS {
            self.clear();
            return;
        }
        let attributes = pango::AttrList::new();
        attributes.insert(pango::AttrFontDesc::new(font));
        if self.colors.replace(Some((foreground, background))) != Some((foreground, background)) {
            self.css.load_from_string(&format!(
                "label {{ color: {foreground}; background-color: {background}; }}"
            ));
        }
        let mut labels = self.labels.borrow_mut();
        while labels.len() < rows.len() {
            let label = gtk4::Label::builder()
                .accessible_role(gtk4::AccessibleRole::Presentation)
                .can_target(false)
                .focusable(false)
                .xalign(0.0)
                .build();
            #[allow(deprecated)] // Scope the color provider to this label.
            label
                .style_context()
                .add_provider(&self.css, gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION);
            self.overlay.put(&label, 0.0, 0.0);
            labels.push(label);
        }
        let mut areas = Vec::with_capacity(rows.len());
        let mut previous_bottom = 0;
        for (label, row) in labels.iter().zip(rows) {
            let area = row.area;
            label.set_attributes(Some(&attributes));
            label.set_text(row.text);
            let (width, height) = label.layout().pixel_size();
            if width > area.width() || height > area.height() || area.y() < previous_bottom {
                self.clear();
                return;
            }
            label.set_size_request(area.width(), area.height());
            self.overlay
                .move_(label, f64::from(area.x()), f64::from(area.y()));
            label.set_visible(true);
            previous_bottom = area.y().saturating_add(area.height());
            areas.push(area);
        }
        for label in &labels[rows.len()..] {
            label.set_visible(false);
        }
        self.native.set_excluded_areas(&areas);
        self.overlay.set_visible(true);
    }
}

glib::wrapper! {
    pub struct NativeOutputClip(ObjectSubclass<clip::NativeOutputClip>)
        @extends gtk4::Widget, @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget;
}

impl NativeOutputClip {
    fn set_excluded_areas(&self, areas: &[gdk::Rectangle]) {
        if self.imp().excluded.borrow().as_slice() != areas {
            let mut excluded = self.imp().excluded.borrow_mut();
            excluded.clear();
            excluded.extend_from_slice(areas);
            drop(excluded);
            self.queue_draw();
        }
    }
}

mod clip {
    use super::*;

    #[derive(Default)]
    pub struct NativeOutputClip {
        pub(super) terminal: RefCell<Option<gtk4::Widget>>,
        pub(super) excluded: RefCell<Vec<gdk::Rectangle>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for NativeOutputClip {
        const NAME: &'static str = "JtermNativeOutputClip";
        type Type = super::NativeOutputClip;
        type ParentType = gtk4::Widget;

        fn class_init(class: &mut Self::Class) {
            class.set_layout_manager_type::<gtk4::BinLayout>();
        }
    }

    impl ObjectImpl for NativeOutputClip {
        fn dispose(&self) {
            if let Some(terminal) = self.terminal.take() {
                terminal.unparent();
            }
        }
    }

    impl WidgetImpl for NativeOutputClip {
        fn snapshot(&self, snapshot: &gtk4::Snapshot) {
            let child = self.terminal.borrow();
            let Some(terminal) = child.as_ref() else {
                return;
            };
            let widget = self.obj();
            let excluded = self.excluded.borrow();
            if excluded.is_empty() {
                widget.snapshot_child(terminal, snapshot);
                return;
            }
            // Reuse the native render node around the projected cells. This
            // leaves the real GTK backdrop visible even on a transparent tab.
            let original = gtk4::Snapshot::new();
            widget.snapshot_child(terminal, &original);
            let Some(original) = original.to_node() else {
                return;
            };
            let width = widget.width();
            let height = widget.height();
            let draw = |x, y, width, height| {
                if width > 0 && height > 0 {
                    snapshot.push_clip(&graphene::Rect::new(
                        x as f32,
                        y as f32,
                        width as f32,
                        height as f32,
                    ));
                    snapshot.append_node(&original);
                    snapshot.pop();
                }
            };
            let mut previous_bottom = 0;
            for area in excluded.iter() {
                let left = area.x().clamp(0, width);
                let top = area.y().clamp(previous_bottom, height);
                let right = area.x().saturating_add(area.width()).clamp(left, width);
                let bottom = area.y().saturating_add(area.height()).clamp(top, height);
                draw(0, previous_bottom, width, top - previous_bottom);
                draw(0, top, left, bottom - top);
                draw(right, top, width - right, bottom - top);
                previous_bottom = bottom;
            }
            draw(0, previous_bottom, width, height - previous_bottom);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{captured_live_rows, CapturedLiveRow};

    #[test]
    fn only_projects_complete_unwrapped_plain_rows() {
        assert_eq!(
            captured_live_rows("work in progress\r\n42 files\r\n", 40),
            Some(vec![
                CapturedLiveRow {
                    text: "work in progress",
                    columns: 16
                },
                CapturedLiveRow {
                    text: "42 files",
                    columns: 8
                },
            ])
        );
        assert_eq!(
            captured_live_rows("测试 e\u{301} 👩‍💻\r\n", 20),
            Some(vec![CapturedLiveRow {
                text: "测试 e\u{301} 👩‍💻",
                columns: 9
            }]),
        );
        for capture in [
            "work in progress",
            "one\r\ntwo",
            "https://example.test\r\n",
            "\x1b[31mcolored\r\n",
            "progress\rrewritten\r\n",
            "misspaced \r\n",
            "\r\n",
            "spoof\u{202e}text\r\n",
            "tabs\tmove\r\n",
        ] {
            assert_eq!(captured_live_rows(capture, 40), None, "{capture:?}");
        }
        assert_eq!(captured_live_rows("full\r\n", 4), None);
        assert_eq!(captured_live_rows("测试\r\n", 4), None);
        assert!(captured_live_rows(&"a\r\n".repeat(super::MAX_LIVE_OUTPUT_ROWS + 1), 40).is_none());
    }
}
