use std::{
    cell::{Cell, RefCell},
    rc::{Rc, Weak},
    time::Duration,
};

use gtk4::{gdk, glib, prelude::*};
use jterm_engine::history_search::{HistoryMatch, HistorySearch, HistorySearchProgress};

use crate::pane::{Pty, TerminalPane};

const SEARCH_WINDOW_BYTES: usize = 128 * 1024;

pub(crate) struct HistorySearchView {
    window: gtk4::Window,
    query: gtk4::Entry,
    find: gtk4::Button,
    older: gtk4::Button,
    status: gtk4::Label,
    output: gtk4::TextView,
    cursor: RefCell<Option<HistorySearch>>,
    generation: Cell<u64>,
    busy: Cell<bool>,
}

impl HistorySearchView {
    pub(crate) fn new<P: Pty>(pane: &Rc<TerminalPane<P>>) -> Rc<Self> {
        let query = gtk4::Entry::builder()
            .placeholder_text("Find exact text")
            .max_length(1024)
            .hexpand(true)
            .build();
        query.update_property(&[gtk4::accessible::Property::Label("Find exact text")]);
        let find = gtk4::Button::with_label("Find");
        let older = gtk4::Button::with_label("Older match");
        older.set_sensitive(false);
        let commands = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        commands.append(&query);
        commands.append(&find);
        commands.append(&older);

        let status = gtk4::Label::new(None);
        status.set_xalign(0.0);
        status.set_wrap(true);
        let output = gtk4::TextView::builder()
            .editable(false)
            .monospace(true)
            .wrap_mode(gtk4::WrapMode::Char)
            .build();
        output.update_property(&[gtk4::accessible::Property::Label("Terminal history match")]);
        let scroller = gtk4::ScrolledWindow::builder()
            .vexpand(true)
            .child(&output)
            .build();
        let content = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            .spacing(8)
            .margin_top(10)
            .margin_bottom(10)
            .margin_start(10)
            .margin_end(10)
            .build();
        content.append(&commands);
        content.append(&status);
        content.append(&scroller);

        let window = gtk4::Window::builder()
            .title("Find terminal history")
            .modal(true)
            .hide_on_close(true)
            .default_width(660)
            .default_height(400)
            .child(&content)
            .build();
        let view = Rc::new(Self {
            window,
            query,
            find,
            older,
            status,
            output,
            cursor: RefCell::new(None),
            generation: Cell::new(0),
            busy: Cell::new(false),
        });
        for (button, restart) in [(&view.find, true), (&view.older, false)] {
            let (view, pane) = (Rc::downgrade(&view), Rc::downgrade(pane));
            button.connect_clicked(move |_| {
                if let Some(view) = view.upgrade() {
                    view.search(pane.clone(), restart);
                }
            });
        }
        let (weak, pane) = (Rc::downgrade(&view), Rc::downgrade(pane));
        view.query.connect_activate(move |_| {
            if let Some(view) = weak.upgrade() {
                view.search(pane.clone(), true);
            }
        });
        let weak = Rc::downgrade(&view);
        view.query.connect_changed(move |_| {
            if let Some(view) = weak.upgrade() {
                view.clear();
            }
        });
        let weak = Rc::downgrade(&view);
        view.window.connect_hide(move |_| {
            if let Some(view) = weak.upgrade() {
                view.generation.set(view.generation.get().wrapping_add(1));
                view.query.set_text("");
                view.clear();
            }
        });
        let weak = Rc::downgrade(&view);
        let keys = gtk4::EventControllerKey::new();
        keys.set_propagation_phase(gtk4::PropagationPhase::Capture);
        keys.connect_key_pressed(move |_, key, _, _| {
            if let Some(view) = weak.upgrade().filter(|_| key == gdk::Key::Escape) {
                view.hide();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        view.window.add_controller(keys);
        view
    }

    pub(crate) fn toggle(&self, parent: &gtk4::Window) {
        if self.window.is_visible() {
            self.hide();
        } else {
            self.window.set_transient_for(Some(parent));
            self.window.present();
            self.query.grab_focus();
        }
    }

    pub(crate) fn hide(&self) {
        self.window.set_visible(false);
    }

    fn clear(&self) {
        self.cursor.borrow_mut().take();
        self.older.set_sensitive(false);
        self.status.set_text("");
        self.output.buffer().set_text("");
    }

    fn set_busy(&self, busy: bool) {
        self.busy.set(busy);
        self.query.set_sensitive(!busy);
        self.find.set_sensitive(!busy);
        self.older
            .set_sensitive(!busy && self.cursor.borrow().is_some());
    }

    fn search<P: Pty>(self: &Rc<Self>, pane: Weak<TerminalPane<P>>, restart: bool) {
        if self.busy.get() {
            return;
        }
        let mut cursor = if restart {
            self.clear();
            match HistorySearch::new(self.query.text().to_string(), SEARCH_WINDOW_BYTES) {
                Ok(cursor) => cursor,
                Err(error) => {
                    self.status.set_text(&error);
                    return;
                }
            }
        } else {
            let Some(cursor) = self.cursor.borrow_mut().take() else {
                return;
            };
            cursor
        };
        self.set_busy(true);
        self.status.set_text("Searching retained history…");
        let weak = Rc::downgrade(self);
        let generation = self.generation.get();
        glib::MainContext::default().spawn_local(async move {
            loop {
                let response = match pane.upgrade() {
                    Some(pane) => pane.history_search_step(cursor).await,
                    None => return,
                };
                let Some(view) = weak.upgrade() else { return };
                if view.generation.get() != generation {
                    view.set_busy(false);
                    return;
                }
                match response {
                    Ok((next, HistorySearchProgress::More)) => cursor = next,
                    Ok((next, HistorySearchProgress::Match(found))) => {
                        view.cursor.replace(Some(next));
                        view.set_busy(false);
                        view.select(found);
                        return;
                    }
                    Ok((_, HistorySearchProgress::End)) => {
                        view.status.set_text(if restart {
                            "No matches in retained history."
                        } else {
                            "No older match in retained history."
                        });
                        view.set_busy(false);
                        return;
                    }
                    Err(error) => {
                        view.status.set_text(&error);
                        view.set_busy(false);
                        return;
                    }
                }
                drop(view);
                glib::timeout_future(Duration::from_millis(1)).await;
            }
        });
    }

    fn select(&self, found: HistoryMatch) {
        let start = found.text[..found.range.start].chars().count() as i32;
        let end = start + found.text[found.range].chars().count() as i32;
        let buffer = self.output.buffer();
        buffer.set_text(&found.text);
        self.output.grab_focus();
        buffer.select_range(&buffer.iter_at_offset(start), &buffer.iter_at_offset(end));
        self.output
            .scroll_to_mark(&buffer.get_insert(), 0.1, true, 0.0, 0.3);
        self.status.set_text("Match selected in retained history.");
    }
}

impl Drop for HistorySearchView {
    fn drop(&mut self) {
        self.window.destroy();
    }
}
