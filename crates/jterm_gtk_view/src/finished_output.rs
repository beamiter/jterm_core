use gtk4::{glib, prelude::*, subclass::prelude::*, ScrolledWindow};
use jterm_engine::archived_text::ArchivedText;
use std::{cell::Cell, rc::Rc};

use crate::FinishedTextView;

/// A read-only card output with a capped viewport and a private scroll range.
#[derive(Clone)]
pub struct FinishedOutputView(Rc<OutputViewport>);

struct OutputViewport {
    text: FinishedTextView,
    widget: OutputAllocation,
    scroll: ScrolledWindow,
    initial_rows: i64,
    collapsed_rows: Cell<i64>,
    expanded_rows: i64,
    expanded: Cell<bool>,
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct OutputAllocation;

    #[glib::object_subclass]
    impl ObjectSubclass for OutputAllocation {
        const NAME: &'static str = "JtermFinishedOutputAllocation";
        type Type = super::OutputAllocation;
        type ParentType = gtk4::Widget;
    }

    impl ObjectImpl for OutputAllocation {
        fn dispose(&self) {
            if let Some(child) = self.obj().first_child() {
                child.unparent();
            }
        }
    }

    impl WidgetImpl for OutputAllocation {
        fn request_mode(&self) -> gtk4::SizeRequestMode {
            gtk4::SizeRequestMode::HeightForWidth
        }

        fn measure(&self, orientation: gtk4::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            let Some(child) = self.obj().first_child() else {
                return (0, 0, -1, -1);
            };
            let (minimum, natural, min_baseline, nat_baseline) =
                child.measure(orientation, for_size);
            if orientation == gtk4::Orientation::Vertical {
                // A history canvas allocates its children's minimum heights.
                // Reserve the document's capped natural height inside it.
                (natural, natural, nat_baseline, nat_baseline)
            } else {
                (minimum, natural, min_baseline, nat_baseline)
            }
        }

        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            if let Some(child) = self.obj().first_child() {
                child.allocate(width, height, baseline, None);
            }
        }

        fn snapshot(&self, snapshot: &gtk4::Snapshot) {
            let widget = self.obj();
            if let Some(child) = widget.first_child() {
                widget.snapshot_child(&child, snapshot);
            }
        }
    }
}

glib::wrapper! {
    pub struct OutputAllocation(ObjectSubclass<imp::OutputAllocation>)
        @extends gtk4::Widget,
        @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget;
}

impl FinishedOutputView {
    pub fn new(text: FinishedTextView, initial_rows: i64, expanded_rows: i64) -> Self {
        let scroll = ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .vscrollbar_policy(gtk4::PolicyType::External)
            .propagate_natural_height(true)
            .kinetic_scrolling(false)
            .hexpand(true)
            .child(text.widget())
            .build();
        let widget = glib::Object::builder::<OutputAllocation>()
            .property("hexpand", true)
            .build();
        scroll.set_parent(&widget);
        let view = OutputViewport {
            text,
            widget,
            scroll,
            initial_rows,
            collapsed_rows: Cell::new(initial_rows),
            expanded_rows,
            expanded: Cell::new(false),
        };
        view.update_height();
        Self(Rc::new(view))
    }

    pub fn widget(&self) -> &gtk4::Widget {
        self.0.widget.upcast_ref()
    }

    pub fn text(&self) -> &FinishedTextView {
        &self.0.text
    }

    pub fn adjustment(&self) -> gtk4::Adjustment {
        self.0.scroll.vadjustment()
    }

    /// Reserve rows for pane chrome and the live prompt at the current scale.
    pub fn refit(&self, pane_height: Option<f64>, reserved_rows: i64) {
        let rows = pane_height
            .filter(|height| *height > 1.0)
            .map(|height| (height / f64::from(self.0.text.line_height())) as i64 - reserved_rows)
            .map_or(self.0.initial_rows, |rows| rows.max(3));
        self.0.collapsed_rows.set(rows);
        self.0.update_height();
    }

    pub fn present_output(&self, document: &ArchivedText) {
        self.0.text.present_output(document);
        let adjustment = self.adjustment();
        adjustment.set_value(adjustment.lower());
    }

    pub fn set_font(&self, font: &gtk4::pango::FontDescription) {
        self.0.text.set_font(font);
        self.0.update_height();
    }

    pub fn set_font_scale(&self, scale: f64) {
        self.0.text.set_font_scale(scale);
        self.0.update_height();
    }

    pub fn bind_controls(
        &self,
        scrollbar: &gtk4::Scrollbar,
        expand: &gtk4::Button,
        jump: &gtk4::Button,
    ) {
        let adjustment = self.adjustment();
        scrollbar.set_adjustment(Some(&adjustment));
        let weak = Rc::downgrade(&self.0);
        let scrollbar = scrollbar.downgrade();
        let expand_button = expand.downgrade();
        let jump = jump.downgrade();
        let sync = Rc::new(move || {
            let (Some(view), Some(expand), Some(scrollbar), Some(jump)) = (
                weak.upgrade(),
                expand_button.upgrade(),
                scrollbar.upgrade(),
                jump.upgrade(),
            ) else {
                return;
            };
            let adjustment = view.scroll.vadjustment();
            let page = adjustment.page_size();
            let overflows =
                page > 0.0 && adjustment.upper() - adjustment.lower() > page + f64::EPSILON;
            scrollbar.set_visible(overflows);
            let can_expand = page > 0.0
                && view.expanded_rows > view.collapsed_rows.get()
                && view.text_height() > view.collapsed_height();
            if page > 0.0 && !can_expand && view.expanded.replace(false) {
                view.update_height();
                set_expand_button(&expand, false);
            }
            expand.set_visible(can_expand);
            jump.set_visible(overflows || (can_expand && view.expanded.get()));
        });
        sync();
        let pending = Rc::new(Cell::new(false));
        adjustment.connect_changed(move |_| {
            // TextView updates its scroll range during allocation. Changing
            // its sibling's visibility there invalidates the active layout.
            if !pending.replace(true) {
                let sync = sync.clone();
                let pending = pending.clone();
                glib::idle_add_local_once(move || {
                    pending.set(false);
                    sync();
                });
            }
        });

        let view = Rc::downgrade(&self.0);
        expand.connect_clicked(move |button| {
            let Some(view) = view.upgrade() else {
                return;
            };
            let expanded = !view.expanded.get();
            view.expanded.set(expanded);
            view.update_height();
            set_expand_button(button, expanded);
        });
    }
}

fn set_expand_button(button: &gtk4::Button, expanded: bool) {
    let (icon, label) = if expanded {
        ("view-restore-symbolic", "Collapse to viewport height")
    } else {
        ("view-fullscreen-symbolic", "Expand block")
    };
    button.set_icon_name(icon);
    button.set_tooltip_text(Some(label));
    button.update_property(&[gtk4::accessible::Property::Label(label)]);
}

impl OutputViewport {
    fn collapsed_height(&self) -> i32 {
        self.height_for_rows(self.collapsed_rows.get())
    }

    fn height_for_rows(&self, rows: i64) -> i32 {
        (rows.clamp(1, i64::from(i32::MAX)) as i32).saturating_mul(self.text.line_height())
    }

    fn text_height(&self) -> i32 {
        let (_, height, ..) = self
            .text
            .widget()
            .measure(gtk4::Orientation::Vertical, self.scroll.width().max(1));
        height
    }

    fn update_height(&self) {
        let rows = if self.expanded.get() {
            self.expanded_rows.max(self.collapsed_rows.get())
        } else {
            self.collapsed_rows.get()
        };
        let height = self.height_for_rows(rows);
        if self.scroll.max_content_height() != height {
            // TextView defers adjustment sizing during a scroll animation.
            // Preserve the current offset and let it size the new viewport.
            let adjustment = self.scroll.vadjustment();
            adjustment.set_value(adjustment.value());
            self.scroll.set_max_content_height(height);
        }
    }
}

/// Apply pixel or discrete-wheel motion and return the part for the outer canvas.
pub fn scroll_finished_output(adjustment: &gtk4::Adjustment, delta: f64, surface: bool) -> f64 {
    let pixels_per_unit = if surface {
        1.0
    } else {
        adjustment.step_increment().max(1.0)
    };
    let bottom = (adjustment.upper() - adjustment.page_size()).max(adjustment.lower());
    let requested = adjustment.value() + delta * pixels_per_unit;
    let next = requested.clamp(adjustment.lower(), bottom);
    adjustment.set_value(next);
    (requested - next) / pixels_per_unit
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    #[ignore = "requires a GTK display"]
    fn retained_output_uses_private_scroll_and_reveals_tail_matches() {
        use gtk4::{gdk, pango};

        gtk4::init().expect("GTK display");
        let capture = ArchivedText {
            text: format!("{}尾部 witness", "visible output row\n".repeat(900)),
            ..ArchivedText::default()
        };
        let text = FinishedTextView::output(
            &capture,
            pango::FontDescription::from_string("Monospace 12"),
            1.0,
            gdk::RGBA::WHITE,
            &[gdk::RGBA::RED; 16],
        );
        let view = FinishedOutputView::new(text, 8, 16);
        let scrollbar =
            gtk4::Scrollbar::new(gtk4::Orientation::Vertical, None::<&gtk4::Adjustment>);
        let expand = gtk4::Button::new();
        let jump = gtk4::Button::new();
        view.bind_controls(&scrollbar, &expand, &jump);
        let output_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        output_row.append(view.widget());
        output_row.append(&scrollbar);
        let root = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        root.append(&output_row);
        root.append(&gtk4::Box::builder().height_request(700).build());
        let history = gtk4::ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .child(&root)
            .build();
        let window = gtk4::Window::builder()
            .default_width(480)
            .default_height(500)
            .child(&history)
            .build();
        window.present();
        let adjustment = view.adjustment();
        let wait_for = |label: &str, check: &dyn Fn() -> bool| {
            let main = gtk4::glib::MainContext::default();
            let deadline = Instant::now() + Duration::from_secs(4);
            loop {
                while main.pending() {
                    main.iteration(false);
                }
                if check() {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "{label}: value={}, upper={}, page={}, limit={}, expand={}, scrollbar={}",
                    adjustment.value(),
                    adjustment.upper(),
                    adjustment.page_size(),
                    view.0.scroll.max_content_height(),
                    expand.is_visible(),
                    scrollbar.is_visible(),
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        wait_for("initial layout", &|| {
            expand.is_visible()
                && scrollbar.is_visible()
                && adjustment.page_size() == f64::from(view.0.scroll.max_content_height())
        });
        let collapsed = adjustment.page_size();
        assert_eq!(scroll_finished_output(&adjustment, 2.0, false), 0.0);
        assert!(adjustment.value() > 0.0);
        let remaining = scroll_finished_output(&adjustment, 100_000.0, true);
        assert!(remaining > 0.0);
        assert_eq!(adjustment.value(), adjustment.upper() - collapsed);

        adjustment.set_value(0.0);
        let start = capture.text.find("尾部 witness").unwrap();
        let text = view.text().widget();
        assert!(crate::select_finished_text_bytes(
            text,
            start..capture.text.len()
        ));
        wait_for("find near end", &|| {
            adjustment.value() >= adjustment.upper() - collapsed * 2.0
        });
        let buffer = text.buffer();
        let (start, end) = buffer.selection_bounds().unwrap();
        assert_eq!(buffer.text(&start, &end, true), "尾部 witness");

        expand.emit_clicked();
        wait_for("expanded layout", &|| adjustment.page_size() > collapsed);
        view.present_output(&ArchivedText {
            text: "filtered".into(),
            ..ArchivedText::default()
        });
        wait_for("filtered layout", &|| {
            !scrollbar.is_visible() && !expand.is_visible()
        });
        assert_eq!(adjustment.value(), adjustment.lower());
        view.present_output(&capture);
        wait_for("restored layout", &|| {
            scrollbar.is_visible() && adjustment.page_size() == collapsed
        });
        window.close();
    }
}
