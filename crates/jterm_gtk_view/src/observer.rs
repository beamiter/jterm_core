//! A presentation-only window. It cannot resize, stop or write to a PTY.
use crate::{
    inline_images::InlineImages,
    pane::Theme,
    runtime_observer::{ObserverFrame, RuntimeObserver},
    view::TerminalView,
};
use gtk4::{glib, prelude::*};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::Arc,
};

type ImageSource = Box<dyn Fn(&mut InlineImages)>;
struct ObserverView {
    capability: RefCell<Option<RuntimeObserver>>,
    view: TerminalView,
    images: RefCell<InlineImages>,
    image_source: ImageSource,
    status: gtk4::Label,
    painted: RefCell<Option<Arc<ObserverFrame>>>,
    detached: Cell<bool>,
    missing_images: Cell<bool>,
}

pub(crate) fn open(
    capability: RuntimeObserver,
    theme: Theme,
    parent: &gtk4::Window,
    images: impl Fn(&mut InlineImages) + 'static,
) -> gtk4::Window {
    let view = TerminalView::new(theme);
    view.widget.update_property(&[
        gtk4::accessible::Property::Label("Read-only terminal observer"),
        gtk4::accessible::Property::Description(
            "Observes the same terminal; input and geometry belong to its owner",
        ),
    ]);
    let scroll = gtk4::ScrolledWindow::builder()
        .hexpand(true)
        .vexpand(true)
        .child(&view.widget)
        .build();
    let overlay = gtk4::Overlay::builder()
        .hexpand(true)
        .vexpand(true)
        .child(&scroll)
        .build();
    let inline_images = InlineImages::attach(&overlay);
    let status = gtk4::Label::new(Some("Read only · waiting for owner frame"));
    status.set_xalign(0.0);
    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    content.append(&status);
    content.append(&overlay);
    let window = gtk4::Window::builder()
        .title("Read-only terminal observer")
        .transient_for(parent)
        .modal(false)
        .default_width(700)
        .default_height(450)
        .child(&content)
        .build();
    let observer = Rc::new(ObserverView {
        capability: RefCell::new(Some(capability)),
        view,
        images: RefCell::new(inline_images),
        image_source: Box::new(images),
        status,
        painted: RefCell::new(None),
        detached: Cell::new(false),
        missing_images: Cell::new(false),
    });
    let weak = Rc::downgrade(&observer);
    observer.view.widget.add_tick_callback(move |_, _| {
        let Some(observer) = weak.upgrade() else {
            return glib::ControlFlow::Break;
        };
        if observer.detached.get() {
            return glib::ControlFlow::Break;
        }
        observer.paint();
        glib::ControlFlow::Continue
    });
    // The window retains the view, which retains no Window or RuntimeShell.
    // Disconnect the capability at destruction even if a caller keeps a GTK
    // object reference after closing the native window.
    let weak = Rc::downgrade(&observer);
    window.connect_unmap(move |_| {
        if let Some(view) = weak.upgrade() {
            view.detach();
        }
    });
    let weak = Rc::downgrade(&observer);
    window.connect_close_request(move |_| {
        if let Some(view) = weak.upgrade() {
            view.detach();
        }
        glib::Propagation::Proceed
    });
    window.connect_destroy(move |_| observer.detach());
    window.present();
    window
}

impl ObserverView {
    fn detach(&self) {
        self.detached.set(true);
        self.capability.borrow_mut().take();
        self.painted.borrow_mut().take();
        self.images.borrow_mut().clear();
        self.view.clear();
    }
    fn paint(&self) {
        let capability = self.capability.borrow();
        let Some(capability) = capability.as_ref() else {
            return;
        };
        let closed = capability.is_closed();
        if let Some(error) = capability.failure() {
            self.status
                .set_text(&format!("Read only · cannot display owner frame: {error}"));
            return;
        }
        let selected = self.view.widget.buffer().has_selection();
        self.status.set_text(if closed {
            "Read only · owner closed"
        } else if selected {
            "Read only · paused while selecting"
        } else if self.missing_images.get() {
            "Read only · image backing unavailable in this view"
        } else {
            "Read only · owner controls input and size"
        });
        if selected {
            return;
        }
        let Some(frame) = capability.latest() else {
            return;
        };
        if self
            .painted
            .borrow()
            .as_ref()
            .is_some_and(|last| Arc::ptr_eq(last, &frame))
        {
            return;
        }
        self.view.present(
            &frame.frame,
            frame.live_margin_gaps,
            None,
            frame.colors.as_ref(),
        );
        let mut images = self.images.borrow_mut();
        (self.image_source)(&mut images);
        self.missing_images
            .set(images.missing_backing(&frame.frame));
        images.present(&frame.frame, None, || self.view.cell_metrics());
        *self.painted.borrow_mut() = Some(frame);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LinkActions, Pty, PtyExit, RuntimeShell, TerminalPane};
    use std::{
        ffi::CString,
        path::Path,
        time::{Duration, Instant},
    };
    struct NoTransport;
    impl Pty for NoTransport {
        fn write(&self, _: &[u8]) -> Result<(), String> {
            panic!("second transport")
        }
        fn resize(&self, _: u16, _: u16) -> Result<(), String> {
            panic!("second transport")
        }
        fn stop(&self) {
            panic!("second transport")
        }
        fn start_reader(
            &self,
            _: impl FnMut(Vec<u8>) + 'static,
            _: impl FnOnce(PtyExit) + 'static,
        ) -> Result<(), String> {
            panic!("second reader")
        }
    }
    #[track_caller]
    fn wait(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(6);
        loop {
            while glib::MainContext::default().iteration(false) {}
            if condition() {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "native observer condition timed out"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    fn text(view: &gtk4::TextView) -> String {
        let buffer = view.buffer();
        buffer
            .text(&buffer.start_iter(), &buffer.end_iter(), true)
            .to_string()
    }
    fn find_text(widget: &gtk4::Widget) -> Option<gtk4::TextView> {
        if let Some(view) = widget.downcast_ref::<gtk4::TextView>() {
            return Some(view.clone());
        }
        let mut child = widget.first_child();
        while let Some(widget) = child {
            if let Some(found) = find_text(&widget) {
                return Some(found);
            }
            child = widget.next_sibling();
        }
        None
    }
    fn masters() -> std::collections::BTreeSet<String> {
        std::fs::read_dir("/proc/self/fdinfo")
            .unwrap()
            .filter_map(|entry| {
                let text = std::fs::read_to_string(entry.ok()?.path()).ok()?;
                text.lines()
                    .find_map(|line| line.strip_prefix("tty-index:").map(|s| s.trim().to_owned()))
            })
            .collect()
    }
    fn kernel_size(pid: i32) -> String {
        let output = std::process::Command::new("/usr/bin/stty")
            .args(["-F", &format!("/proc/{pid}/fd/0"), "size"])
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap()
    }
    #[test]
    #[ignore = "requires a private GTK display, native input tools and a fresh process"]
    fn concurrent_observer_preserves_one_pty_owner_geometry_input_and_close() {
        gtk4::init().unwrap();
        let runtime = RuntimeShell::spawn_program(&["/bin/sh".into(), "-c".into(),
            "stty -echo; printf 'OBSERVER_READY\\n'; while IFS= read -r value; do sleep 0.1; printf 'OWNER:%s\\n' \"$value\"; done".into()],
            Path::new("/"), vec![CString::new("PATH=/usr/bin:/bin").unwrap()]).unwrap();
        let theme = Theme {
            font: gtk4::pango::FontDescription::from_string("Monospace 12"),
            foreground: gtk4::gdk::RGBA::WHITE,
            background: gtk4::gdk::RGBA::BLACK,
            palette: crate::default_ansi_palette(),
            cursor: None,
        };
        let links = LinkActions {
            admit: |_, _| false,
            find_plain: None,
            attach_opener: |_| Box::new(|_| {}),
        };
        let (widget, pane) = TerminalPane::<NoTransport>::with_runtime(
            runtime.clone(),
            theme,
            |s, _| s.into(),
            links,
            |_| {},
        )
        .unwrap();
        let owner = gtk4::Window::builder()
            .title("Observer owner fixture")
            .default_width(800)
            .default_height(500)
            .child(&widget)
            .build();
        owner.present();
        wait(|| text(pane.screen()).contains("OBSERVER_READY"));
        wait(|| pane.grid_matches_allocation());
        let owner_bounds = (owner.width(), owner.height());
        let pid = runtime.child_pid().unwrap();
        let tty = std::fs::read_link(format!("/proc/{pid}/fd/0")).unwrap();
        let size = kernel_size(pid);
        let owned_masters = masters();
        assert_eq!(owned_masters.len(), 1);
        let observer = pane.open_observer().unwrap();
        let screen = find_text(observer.upcast_ref()).unwrap();
        wait(|| text(&screen).contains("OBSERVER_READY"));
        assert!(!screen.is_editable());
        observer.set_default_size(330, 220);
        if std::env::var("GDK_BACKEND").as_deref() == Ok("wayland") {
            assert!(std::process::Command::new("swaymsg")
                .args([
                    "[title=\"Read-only terminal observer\"]",
                    "resize",
                    "set",
                    "width",
                    "330",
                    "px",
                    "height",
                    "220",
                    "px"
                ])
                .status()
                .unwrap()
                .success());
        }
        wait(|| observer.width() < 500);
        assert_eq!((owner.width(), owner.height()), owner_bounds);
        assert_eq!(
            kernel_size(pid),
            size,
            "observer allocation resized owner's kernel PTY"
        );
        println!("owner_pid={pid}; tty={tty:?}; masters={owned_masters:?}; owner_bounds_before={owner_bounds:?}; owner_bounds_after={:?}; kernel_before={:?}; kernel_after={:?}; observer_bounds={:?}",
            (owner.width(), owner.height()), size.trim(), kernel_size(pid).trim(), (observer.width(), observer.height()));
        assert_eq!(runtime.child_pid(), Some(pid));
        assert_eq!(masters(), owned_masters, "observer opened another PTY");
        if std::env::var("GDK_BACKEND").as_deref() == Ok("wayland") {
            assert!(std::process::Command::new("swaymsg")
                .args(["[title=\"Read-only terminal observer\"]", "focus"])
                .status()
                .unwrap()
                .success());
            screen.grab_focus();
            assert!(std::process::Command::new("wtype")
                .args(["--", "observer-denied"])
                .status()
                .unwrap()
                .success());
            assert!(std::process::Command::new("wtype")
                .args(["-k", "Return"])
                .status()
                .unwrap()
                .success());
        } else {
            let xid = std::process::Command::new("xdotool")
                .args([
                    "search",
                    "--onlyvisible",
                    "--pid",
                    &std::process::id().to_string(),
                    "--name",
                    "^Read-only terminal observer$",
                ])
                .output()
                .unwrap();
            assert!(xid.status.success());
            let xid = String::from_utf8(xid.stdout).unwrap();
            assert!(std::process::Command::new("xdotool")
                .args(["windowfocus", "--sync", xid.trim()])
                .status()
                .unwrap()
                .success());
            screen.grab_focus();
            assert!(std::process::Command::new("xdotool")
                .args([
                    "type",
                    "--clearmodifiers",
                    "--delay",
                    "2",
                    "observer-denied"
                ])
                .status()
                .unwrap()
                .success());
            assert!(std::process::Command::new("xdotool")
                .args(["key", "Return"])
                .status()
                .unwrap()
                .success());
        }
        let until = Instant::now() + Duration::from_millis(250);
        wait(|| Instant::now() >= until);
        assert!(
            !text(pane.screen()).contains("observer-denied")
                && !text(&screen).contains("observer-denied"),
            "read-only view forwarded native input"
        );
        assert_eq!(
            std::fs::read_link(format!("/proc/{pid}/fd/0")).unwrap(),
            tty
        );
        let reserved: Vec<_> = (0..7).map(|_| runtime.observe().unwrap()).collect();
        assert!(runtime.observe().is_err());
        observer.destroy();
        while glib::MainContext::default().iteration(false) {}
        let released = runtime
            .observe()
            .expect("destroyed observer did not release its subscription");
        drop(released);
        drop(reserved);
        assert_eq!(
            runtime.child_pid(),
            Some(pid),
            "observer detach stopped owner"
        );
        pane.write_input(b"after-detach\n".to_vec()).unwrap();
        wait(|| text(pane.screen()).contains("OWNER:after-detach"));
        let observer = pane.open_observer().unwrap();
        let screen = find_text(observer.upcast_ref()).unwrap();
        wait(|| text(&screen).contains("OWNER:after-detach"));
        pane.write_input(b"while-owner-selected\n".to_vec())
            .unwrap();
        let buffer = pane.screen().buffer();
        buffer.select_range(&buffer.start_iter(), &buffer.end_iter());
        wait(|| text(&screen).contains("OWNER:while-owner-selected"));
        assert!(
            !text(pane.screen()).contains("OWNER:while-owner-selected"),
            "owner selection was unexpectedly changed"
        );
        let probe = runtime.observe().unwrap();
        pane.stop();
        wait(|| probe.is_closed());
        wait(|| probe.latest().is_some_and(|frame| frame.closed));
        wait(|| runtime.ready_to_discard());
        assert!(
            observer.is_visible(),
            "owner close destroyed the read-only view"
        );
        fn closed_label(widget: &gtk4::Widget) -> bool {
            if widget
                .downcast_ref::<gtk4::Label>()
                .is_some_and(|label| label.text() == "Read only · owner closed")
            {
                return true;
            }
            let mut child = widget.first_child();
            while let Some(widget) = child {
                if closed_label(&widget) {
                    return true;
                }
                child = widget.next_sibling();
            }
            false
        }
        wait(|| closed_label(observer.upcast_ref()));
        assert!(text(&screen).contains("OWNER:while-owner-selected"));
        println!("native_input_denied=true; observer_detach_released=true; owner_input_continued=true; owner_selection_independent=true; owner_closed_visible=true; child_reaped=true");
        observer.destroy();
        owner.destroy();
    }
}
