use gtk4::{gdk, glib, graphene, pango, prelude::*, subclass::prelude::*};
use std::cell::{Cell, RefCell};

pub const MAX_LIVE_OUTPUT_BYTES: usize = 82;

/// A complete, short physical row from command capture. The host still has
/// to prove that this exact row is visible in the terminal's current grid.
pub fn captured_live_row(capture: &str, columns: usize) -> Option<&str> {
    if capture.len() > MAX_LIVE_OUTPUT_BYTES {
        return None;
    }
    let text = capture.strip_suffix("\r\n")?;
    (!text.is_empty()
        && text.len() < columns
        && text.trim() == text
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b" _-=+,%!?()[]'\"".contains(&byte)))
    .then_some(text)
}

/// A passive row presentation for the actual running card. It holds neither
/// terminal state nor a PTY and does not participate in input or selection.
pub struct LiveOutputView {
    native: NativeOutputClip,
    label: gtk4::Label,
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
        let label = gtk4::Label::builder()
            .accessible_role(gtk4::AccessibleRole::Presentation)
            .can_target(false)
            .focusable(false)
            .halign(gtk4::Align::Start)
            .valign(gtk4::Align::Start)
            .xalign(0.0)
            .visible(false)
            .build();
        label.set_widget_name("shared-live-command-output");
        let css = gtk4::CssProvider::new();
        #[allow(deprecated)] // Local provider, without a display-global selector.
        label
            .style_context()
            .add_provider(&css, gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION);
        let clip = native.downgrade();
        label.connect_visible_notify(move |label| {
            if !label.get_visible() {
                if let Some(clip) = clip.upgrade() {
                    clip.set_excluded_area(None);
                }
            }
        });
        Self {
            native,
            label,
            css,
            colors: Cell::new(None),
        }
    }

    pub fn native_surface(&self) -> &gtk4::Widget {
        self.native.upcast_ref()
    }

    pub fn widget(&self) -> &gtk4::Label {
        &self.label
    }

    pub fn clear(&self) {
        self.label.set_visible(false);
    }

    pub fn present(
        &self,
        text: &str,
        area: gdk::Rectangle,
        font: &pango::FontDescription,
        foreground: gdk::RGBA,
        background: gdk::RGBA,
    ) {
        let attributes = pango::AttrList::new();
        attributes.insert(pango::AttrFontDesc::new(font));
        self.label.set_attributes(Some(&attributes));
        self.label.set_text(text);
        let layout = self.label.layout();
        let (text_width, text_height) = layout.pixel_size();
        if text_width > area.width() || text_height > area.height() {
            self.clear();
            return;
        }
        if self.colors.replace(Some((foreground, background))) != Some((foreground, background)) {
            self.css.load_from_string(&format!(
                "label {{ color: {foreground}; background-color: {background}; }}"
            ));
        }
        self.label.set_margin_start(area.x());
        self.label.set_margin_top(area.y());
        self.label.set_size_request(area.width(), area.height());
        self.native.set_excluded_area(Some(area));
        self.label.set_visible(true);
    }
}

glib::wrapper! {
    pub struct NativeOutputClip(ObjectSubclass<clip::NativeOutputClip>)
        @extends gtk4::Widget, @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget;
}

impl NativeOutputClip {
    fn set_excluded_area(&self, area: Option<gdk::Rectangle>) {
        if self.imp().excluded.replace(area) != area {
            self.queue_draw();
        }
    }
}

mod clip {
    use super::*;

    #[derive(Default)]
    pub struct NativeOutputClip {
        pub(super) terminal: RefCell<Option<gtk4::Widget>>,
        pub(super) excluded: Cell<Option<gdk::Rectangle>>,
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
            let Some(area) = self.excluded.get() else {
                widget.snapshot_child(terminal, snapshot);
                return;
            };
            // Reuse the native render node around the projected cells. This
            // leaves the real GTK backdrop visible even on a transparent tab.
            let original = gtk4::Snapshot::new();
            widget.snapshot_child(terminal, &original);
            let Some(original) = original.to_node() else {
                return;
            };
            let width = widget.width();
            let height = widget.height();
            let left = area.x().clamp(0, width);
            let top = area.y().clamp(0, height);
            let right = (area.x() + area.width()).clamp(left, width);
            let bottom = (area.y() + area.height()).clamp(top, height);
            for (x, y, width, height) in [
                (0, 0, width, top),
                (0, bottom, width, height - bottom),
                (0, top, left, bottom - top),
                (right, top, width - right, bottom - top),
            ] {
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
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::captured_live_row;

    #[test]
    fn only_projects_a_complete_unwrapped_plain_row() {
        assert_eq!(
            captured_live_row("work complete\r\n", 40),
            Some("work complete")
        );
        for capture in [
            "work in progress",
            "one\r\ntwo\r\n",
            "https://example.test\r\n",
            "\x1b[31mcolored\r\n",
            "progress\rrewritten\r\n",
            "misspaced \r\n",
            "\r\n",
            "测试\r\n",
            "tabs\tmove\r\n",
        ] {
            assert_eq!(captured_live_row(capture, 40), None, "{capture:?}");
        }
        assert_eq!(captured_live_row("full\r\n", 4), None);
    }
}
