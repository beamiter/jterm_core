use gtk4::{gdk, glib, pango, prelude::*};
use jterm_engine::{frame::Screen, hyperlink::HyperlinkPolicy, terminal::Terminal};
use std::{
    cell::{Cell, Ref, RefCell},
    rc::Rc,
};

#[cfg(target_os = "linux")]
use crate::runtime_shell::RuntimeShell;
use crate::view::{PresentedLink, TerminalView};

pub(crate) const INITIAL_ROWS: usize = 24;
pub(crate) const INITIAL_COLUMNS: usize = 80;

enum Backend<P> {
    Legacy {
        pty: P,
        terminal: RefCell<Terminal>,
    },
    #[cfg(target_os = "linux")]
    Runtime(Rc<RuntimeShell>),
}

/// Existing application-owned PTY transport. Terminal semantics live in this
/// view's single shared engine, while the transport owns the child and descriptors.
pub trait Pty: 'static {
    fn write(&self, bytes: &[u8]) -> Result<(), String>;
    fn resize(&self, cols: u16, rows: u16) -> Result<(), String>;
    fn stop(&self);
    fn start_reader(
        &self,
        output: impl FnMut(Vec<u8>) + 'static,
        exited: impl FnOnce(i32) + 'static,
    ) -> Result<(), String>;
}

pub struct Theme {
    pub font: pango::FontDescription,
    pub foreground: gdk::RGBA,
    pub background: gdk::RGBA,
    pub palette: [gdk::RGBA; 16],
}

pub type HyperlinkOpener = Box<dyn Fn(&str)>;

pub struct LinkActions {
    pub admit: HyperlinkPolicy,
    pub attach_opener: fn(&gtk4::Widget) -> HyperlinkOpener,
}

struct LinkPress {
    buffer: gtk4::TextBuffer,
    target: PresentedLink,
    point: (f64, f64),
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct HistoricalWindow {
    top: u64,
    rows: usize,
    layout: u64,
    history_rows: usize,
    live_revision: Option<u64>,
}

pub struct TerminalPane<P: Pty> {
    backend: Backend<P>,
    status_text: fn(&str, usize) -> String,
    view: TerminalView,
    status: gtk4::Label,
    dimensions: Cell<(usize, usize)>,
    scrollback: gtk4::Adjustment,
    scroll_top: Cell<Option<u64>>,
    scroll_update: Cell<bool>,
    wheel_rows: Cell<f64>,
    painted_history: Cell<Option<HistoricalWindow>>,
    dirty: Cell<bool>,
    exited: Cell<bool>,
    input_focused: Cell<bool>,
    input_method: gtk4::IMMulticontext,
    input_cursor: Cell<Option<gdk::Rectangle>>,
    paste_revision: Cell<u64>,
}

impl<P: Pty> TerminalPane<P> {
    pub fn new(
        pty: P,
        theme: Theme,
        status_text: fn(&str, usize) -> String,
        links: LinkActions,
        close: impl Fn(&gtk4::Widget) + 'static,
    ) -> Result<(gtk4::Widget, Rc<Self>), String> {
        let mut terminal = Terminal::new(INITIAL_ROWS, INITIAL_COLUMNS)?;
        terminal.enable_underline_colors();
        terminal.enable_hyperlinks(links.admit);
        Self::attach(
            Backend::Legacy {
                pty,
                terminal: RefCell::new(terminal),
            },
            theme,
            status_text,
            links,
            close,
        )
    }

    #[cfg(target_os = "linux")]
    pub fn with_runtime(
        runtime: Rc<RuntimeShell>,
        theme: Theme,
        status_text: fn(&str, usize) -> String,
        links: LinkActions,
        close: impl Fn(&gtk4::Widget) + 'static,
    ) -> Result<(gtk4::Widget, Rc<Self>), String> {
        runtime.configure_view(links.admit);
        Self::attach(Backend::Runtime(runtime), theme, status_text, links, close)
    }

    fn attach(
        backend: Backend<P>,
        theme: Theme,
        status_text: fn(&str, usize) -> String,
        links: LinkActions,
        close: impl Fn(&gtk4::Widget) + 'static,
    ) -> Result<(gtk4::Widget, Rc<Self>), String> {
        let (rows, columns) = (INITIAL_ROWS, INITIAL_COLUMNS);
        let view = TerminalView::new(
            theme.font,
            theme.foreground,
            theme.background,
            theme.palette,
        );
        let scroll = gtk4::ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Automatic)
            .vscrollbar_policy(gtk4::PolicyType::External)
            .hexpand(true)
            .vexpand(true)
            .child(&view.widget)
            .build();
        // Value and page size count physical viewport rows. Keep the rail's
        // allocation stable so newly arriving history cannot resize the PTY.
        let scrollback =
            gtk4::Adjustment::new(0.0, 0.0, rows as f64, 1.0, rows as f64, rows as f64);
        let scrollbar = gtk4::Scrollbar::new(gtk4::Orientation::Vertical, Some(&scrollback));
        scrollbar.update_property(&[gtk4::accessible::Property::Label("Terminal scrollback")]);
        let viewport = gtk4::Overlay::builder()
            .hexpand(true)
            .vexpand(true)
            .child(&scroll)
            .build();
        view.attach_preedit(&viewport);
        let body = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        body.set_vexpand(true);
        body.append(&viewport);
        body.append(&scrollbar);
        let status = gtk4::Label::new(None);
        status.set_xalign(0.0);
        status.set_hexpand(true);
        status.set_visible(false);
        let close_button = gtk4::Button::from_icon_name("window-close-symbolic");
        close_button.set_tooltip_text(Some("Close shared terminal tab"));
        close_button.update_property(&[gtk4::accessible::Property::Label(
            "Close shared terminal tab",
        )]);
        let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        let title = gtk4::Label::new(Some("Shared terminal"));
        header.append(&title);
        header.append(&status);
        close_button.set_halign(gtk4::Align::End);
        header.append(&close_button);
        let column = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        column.append(&header);
        column.append(&body);
        let page = column.upcast::<gtk4::Widget>();
        let weak_page = page.downgrade();
        close_button.connect_clicked(move |_| {
            if let Some(page) = weak_page.upgrade() {
                close(&page);
            }
        });

        let session = Rc::new(Self {
            backend,
            status_text,
            view,
            status,
            dimensions: Cell::new((rows, columns)),
            scrollback,
            scroll_top: Cell::new(None),
            scroll_update: Cell::new(false),
            wheel_rows: Cell::new(0.0),
            painted_history: Cell::new(None),
            dirty: Cell::new(true),
            exited: Cell::new(false),
            input_focused: Cell::new(false),
            input_method: gtk4::IMMulticontext::new(),
            input_cursor: Cell::new(None),
            paste_revision: Cell::new(0),
        });
        if session.pty().is_some() {
            session.resize(rows, columns);
        }
        session.connect_input();
        let open_link = (links.attach_opener)(session.view.widget.upcast_ref());
        session.connect_links(open_link);
        session.connect_scroll(&scroll);
        let weak = Rc::downgrade(&session);
        let scroll = scroll.downgrade();
        session.view.widget.add_tick_callback(move |_, _| {
            let Some(session) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let Some(scroll) = scroll.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let (rows, columns) = session.view.dimensions(scroll.width(), scroll.height());
            if !session.exited.get() && session.dimensions.get() != (rows, columns) {
                session.resize(rows, columns);
            }
            session.paint();
            session.sync_input_cursor();
            glib::ControlFlow::Continue
        });
        match &session.backend {
            Backend::Legacy { pty, .. } => {
                let for_output = Rc::downgrade(&session);
                let for_exit = Rc::downgrade(&session);
                pty.start_reader(
                    move |bytes| {
                        if let Some(session) = for_output.upgrade() {
                            session.feed_legacy(&bytes);
                        }
                    },
                    move |code| {
                        if let Some(session) = for_exit.upgrade() {
                            session.report_exit(code);
                        }
                    },
                )?;
            }
            #[cfg(target_os = "linux")]
            Backend::Runtime(_) => {
                session
                    .view
                    .widget
                    .update_property(&[gtk4::accessible::Property::Description(
                        "Shared runtime session",
                    )]);
                session.connect_runtime();
            }
        }
        Ok((page, session))
    }

    pub fn pty(&self) -> Option<&P> {
        match &self.backend {
            Backend::Legacy { pty, .. } => Some(pty),
            #[cfg(target_os = "linux")]
            Backend::Runtime(_) => None,
        }
    }

    #[cfg(target_os = "linux")]
    pub fn runtime(&self) -> Option<&Rc<RuntimeShell>> {
        match &self.backend {
            Backend::Legacy { .. } => None,
            Backend::Runtime(owner) => Some(owner),
        }
    }

    fn terminal(&self) -> Ref<'_, Terminal> {
        match &self.backend {
            Backend::Legacy { terminal, .. } => terminal.borrow(),
            #[cfg(target_os = "linux")]
            Backend::Runtime(owner) => owner.terminal(),
        }
    }

    #[cfg(target_os = "linux")]
    fn connect_runtime(self: &Rc<Self>) {
        let pane = Rc::downgrade(self);
        // This source belongs to the session, including when the tab is hidden.
        // The runtime keeps each drain and write attempt bounded.
        glib::timeout_add_local(std::time::Duration::from_millis(8), move || {
            let Some(pane) = pane.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let owner = pane.runtime().expect("runtime source belongs to its owner");
            if owner.closed() {
                return glib::ControlFlow::Break;
            }
            match owner.drive() {
                Ok(bytes) => {
                    if bytes != 0 {
                        pane.dirty.set(true);
                    }
                }
                Err(error) => {
                    pane.report(&format!("Terminal runtime failed: {error}"));
                    pane.stop();
                    return glib::ControlFlow::Break;
                }
            }
            if let Some(code) = owner.exit_code() {
                pane.report_exit(code);
            }
            if owner.drained() {
                owner.close();
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        });
    }

    fn report_exit(&self, code: i32) {
        if !self.exited.replace(true) {
            self.reset_preedit();
            self.report(&format!("Shell exited ({code})"));
        }
    }

    pub fn grid_size(&self) -> (u16, u16) {
        let (rows, columns) = self.dimensions.get();
        (columns as u16, rows as u16)
    }

    pub fn focus(&self) {
        self.view.widget.grab_focus();
    }

    pub fn stop(&self) {
        self.exited.set(true);
        self.reset_preedit();
        match &self.backend {
            Backend::Legacy { pty, .. } => pty.stop(),
            #[cfg(target_os = "linux")]
            Backend::Runtime(owner) => owner.close(),
        }
    }

    pub fn copy(&self) {
        if !self.view.widget.has_focus() || !self.view.widget.is_mapped() {
            return;
        }
        self.view
            .widget
            .buffer()
            .copy_clipboard(&self.view.widget.clipboard());
    }

    pub fn paste(self: &Rc<Self>) {
        if self.exited.get() || !self.input_focused.get() {
            return;
        }
        let revision = self.advance_paste_revision();
        let session = Rc::downgrade(self);
        let clipboard = self.view.widget.clipboard();
        glib::MainContext::default().spawn_local(async move {
            let result = clipboard.read_text_future().await;
            let Some(session) = session.upgrade() else {
                return;
            };
            if session.exited.get()
                || !session.input_focused.get()
                || !session.view.widget.is_mapped()
                || session.paste_revision.get() != revision
            {
                return;
            }
            match result {
                Ok(Some(text)) => {
                    let bytes = session.terminal().encode_paste(&text).map(|mut bytes| {
                        if !bytes.starts_with(b"\x1b[200~") {
                            // A copied trailing line break is not an explicit
                            // submit. Forge uses first-line-only paste without
                            // negotiated bracketed paste support.
                            if let Some(end) = bytes.iter().position(|byte| *byte == b'\r') {
                                bytes.truncate(end);
                            }
                        }
                        bytes
                    });
                    session.send_input(bytes);
                }
                Ok(None) => {}
                Err(error) => session.report(&format!("Cannot read clipboard: {error}")),
            }
        });
    }

    fn advance_paste_revision(&self) -> u64 {
        let revision = self.paste_revision.get().wrapping_add(1);
        self.paste_revision.set(revision);
        revision
    }

    fn resize(&self, rows: usize, columns: usize) {
        let result = match &self.backend {
            Backend::Legacy { pty, terminal } => {
                terminal
                    .borrow_mut()
                    .resize_with(rows, columns, || pty.resize(columns as u16, rows as u16))
            }
            #[cfg(target_os = "linux")]
            Backend::Runtime(owner) => owner.resize(rows, columns),
        };
        match result {
            Ok(()) => {
                self.dimensions.set((rows, columns));
                self.dirty.set(true);
            }
            Err(error) => self.report(&format!("Cannot resize terminal: {error}")),
        }
    }

    fn feed_legacy(&self, input: &[u8]) {
        let Backend::Legacy { pty, terminal } = &self.backend else {
            return;
        };
        let reply = terminal.borrow_mut().feed(input);
        self.dirty.set(true);
        match reply {
            Ok(bytes) if bytes.is_empty() => {}
            Ok(bytes) => {
                if let Err(error) = pty.write(&bytes) {
                    self.report(&format!("Terminal reply delivery failed: {error}"));
                    self.stop();
                }
            }
            Err(error) => {
                self.report(&format!("Terminal output failed: {error}"));
                self.stop();
            }
        }
    }

    fn paint(&self) {
        if !self.dirty.get() || self.view.widget.buffer().has_selection() {
            return;
        }
        let terminal = self.terminal();
        if terminal.publication_held() && !self.exited.get() {
            return;
        }
        let frame = match terminal.frame() {
            Ok(frame) => frame,
            Err(error) => {
                self.report(&format!("Cannot display terminal output: {error}"));
                return;
            }
        };
        let extent = terminal.history_extent();
        let row_count = frame.rows().len();
        let is_primary = frame.stamp().screen == Screen::Primary;
        let history_top = self.scroll_top.get().filter(|_| is_primary).map(|top| {
            if top < extent.start && !self.exited.get() {
                self.report("Earlier scrollback is no longer retained");
            }
            top.clamp(extent.start, extent.end)
        });
        let history_top = history_top.filter(|top| *top < extent.end);
        if is_primary {
            self.scroll_top.set(history_top);
        }
        self.sync_scrollback(&extent, history_top, row_count);
        let Some(top) = history_top else {
            self.view.present(&frame, None);
            self.painted_history.set(None);
            self.dirty.set(false);
            return;
        };

        let available = (extent.end - top) as usize;
        let history_rows = available.min(row_count);
        let window = HistoricalWindow {
            top,
            rows: row_count,
            layout: frame.stamp().layout,
            history_rows,
            live_revision: (history_rows < row_count).then(|| frame.stamp().revision),
        };
        if self.painted_history.get() != Some(window) {
            let page = match terminal.history_page(available - history_rows, history_rows) {
                Ok(page) => page,
                Err(error) => {
                    self.report(&format!("Cannot display terminal history: {error}"));
                    return;
                }
            };
            self.view.present(&frame, Some(&page));
            self.painted_history.set(Some(window));
        }
        self.dirty.set(false);
    }

    fn sync_scrollback(&self, extent: &std::ops::Range<u64>, top: Option<u64>, rows: usize) {
        self.scroll_update.set(true);
        self.scrollback.configure(
            (top.unwrap_or(extent.end) - extent.start) as f64,
            0.0,
            (extent.end - extent.start) as f64 + rows as f64,
            1.0,
            rows.saturating_sub(1).max(1) as f64,
            rows as f64,
        );
        self.scroll_update.set(false);
    }

    fn set_scroll_top(&self, top: Option<u64>) {
        if self.scroll_top.replace(top) != top {
            if top.is_some() && self.input_focused.get() {
                self.reset_preedit();
            }
            self.clear_selection();
            self.dirty.set(true);
        }
    }

    fn clear_selection(&self) {
        let buffer = self.view.widget.buffer();
        if buffer.has_selection() {
            buffer.place_cursor(&buffer.start_iter());
        }
    }

    fn scroll_rows(&self, rows: i64) -> bool {
        let extent = self.terminal().history_extent();
        if extent.is_empty() {
            return false;
        }
        let top = self.scroll_top.get().unwrap_or(extent.end);
        let top = top
            .saturating_add_signed(rows)
            .clamp(extent.start, extent.end);
        self.set_scroll_top((top < extent.end).then_some(top));
        true
    }

    fn scroll_key(&self, key: gdk::Key, modifiers: gdk::ModifierType) -> bool {
        if !modifiers.contains(gdk::ModifierType::SHIFT_MASK)
            || modifiers.intersects(
                gdk::ModifierType::CONTROL_MASK
                    | gdk::ModifierType::ALT_MASK
                    | gdk::ModifierType::META_MASK
                    | gdk::ModifierType::SUPER_MASK
                    | gdk::ModifierType::HYPER_MASK,
            )
        {
            return false;
        }
        let page = self.dimensions.get().0.saturating_sub(1).max(1) as i64;
        let delta = match key {
            gdk::Key::Page_Up => -page,
            gdk::Key::Page_Down => page,
            gdk::Key::Home => i64::MIN,
            gdk::Key::End => i64::MAX,
            _ => return false,
        };
        self.scroll_rows(delta)
    }

    fn connect_scroll(self: &Rc<Self>, scroll: &gtk4::ScrolledWindow) {
        let weak = Rc::downgrade(self);
        self.scrollback.connect_value_changed(move |adjustment| {
            let Some(session) = weak
                .upgrade()
                .filter(|session| !session.scroll_update.get())
            else {
                return;
            };
            let extent = session.terminal().history_extent();
            if !extent.is_empty() {
                let top = extent
                    .start
                    .saturating_add(adjustment.value().round() as u64)
                    .min(extent.end);
                session.set_scroll_top((top < extent.end).then_some(top));
            }
        });

        let wheel = gtk4::EventControllerScroll::new(gtk4::EventControllerScrollFlags::VERTICAL);
        wheel.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let weak = Rc::downgrade(self);
        wheel.connect_scroll(move |controller, _, dy| {
            let Some(session) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            if controller.current_event_state().intersects(
                gdk::ModifierType::CONTROL_MASK
                    | gdk::ModifierType::ALT_MASK
                    | gdk::ModifierType::META_MASK
                    | gdk::ModifierType::SUPER_MASK
                    | gdk::ModifierType::HYPER_MASK,
            ) || session.terminal().history_rows() == 0
            {
                session.wheel_rows.set(0.0);
                return glib::Propagation::Proceed;
            }
            let lines = if controller.unit() == gdk::ScrollUnit::Surface {
                dy / session.view.row_height()
            } else {
                dy * 3.0
            };
            let position = session.wheel_rows.get() + lines;
            let whole = position.trunc();
            session.wheel_rows.set(position - whole);
            session.scroll_rows(whole as i64);
            glib::Propagation::Stop
        });
        scroll.add_controller(wheel);
    }

    fn send_input(&self, bytes: Result<Vec<u8>, String>) {
        if self.exited.get() {
            return;
        }
        self.clear_selection();
        if bytes.as_ref().is_ok_and(|data| !data.is_empty()) {
            self.set_scroll_top(None);
        }
        let result = bytes.and_then(|bytes| {
            if bytes.is_empty() {
                return Ok(());
            }
            match &self.backend {
                Backend::Legacy { pty, .. } => pty.write(&bytes),
                #[cfg(target_os = "linux")]
                Backend::Runtime(owner) => owner.input(bytes),
            }
        });
        if let Err(error) = result {
            self.report(&format!("Terminal input rejected: {error}"));
        }
    }

    fn report(&self, text: &str) {
        self.status.set_text(&(self.status_text)(text, 256));
        self.status.set_visible(true);
    }

    fn reset_preedit(&self) {
        self.input_method.reset();
        self.view.clear_preedit();
    }

    fn sync_input_cursor(&self) {
        if !self.input_focused.get() || self.exited.get() {
            return;
        }
        if let Some(area) = self.view.input_cursor() {
            if self.input_cursor.replace(Some(area)) != Some(area) {
                self.input_method.set_cursor_location(&area);
            }
        }
    }

    fn connect_input(self: &Rc<Self>) {
        let keys = gtk4::EventControllerKey::new();
        keys.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let im = &self.input_method;
        im.set_client_widget(Some(&self.view.widget));
        im.set_use_preedit(true);
        keys.set_im_context(Some(im));
        let weak = Rc::downgrade(self);
        im.connect_commit(move |_, text| {
            if let Some(session) = weak.upgrade().filter(|session| {
                session.input_focused.get()
                    && session.view.widget.is_mapped()
                    && !session.exited.get()
            }) {
                session.view.clear_preedit();
                let bytes = session.terminal().encode_text(text);
                session.send_input(bytes);
            }
        });
        let weak = Rc::downgrade(self);
        im.connect_preedit_changed(move |im| {
            let Some(session) = weak.upgrade().filter(|session| {
                session.input_focused.get()
                    && session.view.widget.is_mapped()
                    && !session.exited.get()
            }) else {
                return;
            };
            let (text, attributes, _) = im.preedit_string();
            if text.is_empty() {
                session.view.clear_preedit();
            } else {
                session.set_scroll_top(None);
                session.view.show_preedit(&text, attributes);
            }
        });
        let weak = Rc::downgrade(self);
        im.connect_preedit_end(move |_| {
            if let Some(session) = weak.upgrade() {
                session.view.clear_preedit();
            }
        });
        let focus = gtk4::EventControllerFocus::new();
        let weak = Rc::downgrade(self);
        focus.connect_enter(move |_| {
            if let Some(session) = weak.upgrade() {
                session.input_focused.set(true);
                session.input_method.focus_in();
                session.sync_input_cursor();
            }
        });
        let weak = Rc::downgrade(self);
        focus.connect_leave(move |_| {
            if let Some(session) = weak.upgrade() {
                session.input_focused.set(false);
                session.input_cursor.set(None);
                session.advance_paste_revision();
                session.input_method.focus_out();
                session.reset_preedit();
            }
        });
        self.view.widget.add_controller(focus);
        let weak = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, modifiers| {
            let Some(session) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            if session.scroll_key(key, modifiers) {
                return glib::Propagation::Stop;
            }
            let Some(name) = logical_key(key) else {
                return glib::Propagation::Proceed;
            };
            if modifiers.intersects(
                gdk::ModifierType::META_MASK
                    | gdk::ModifierType::SUPER_MASK
                    | gdk::ModifierType::HYPER_MASK,
            ) {
                return glib::Propagation::Proceed;
            }
            let bytes = session.terminal().encode_key(
                &name,
                key == gdk::Key::ISO_Left_Tab || modifiers.contains(gdk::ModifierType::SHIFT_MASK),
                modifiers.contains(gdk::ModifierType::ALT_MASK),
                modifiers.contains(gdk::ModifierType::CONTROL_MASK),
            );
            session.send_input(bytes);
            glib::Propagation::Stop
        });
        self.view.widget.add_controller(keys);
    }

    fn connect_links(self: &Rc<Self>, open: HyperlinkOpener) {
        let gesture = gtk4::GestureClick::builder()
            .button(gdk::BUTTON_PRIMARY)
            .propagation_phase(gtk4::PropagationPhase::Capture)
            .build();
        let pressed = Rc::new(RefCell::new(None));
        let weak = Rc::downgrade(self);
        let for_press = Rc::clone(&pressed);
        gesture.connect_pressed(move |gesture, count, x, y| {
            for_press.borrow_mut().take();
            if count != 1 || !link_modifier(gesture.current_event_state()) {
                return;
            }
            let Some(session) = weak.upgrade() else {
                return;
            };
            if let Some(target) = session.view.link_at(x, y) {
                *for_press.borrow_mut() = Some(LinkPress {
                    buffer: session.view.widget.buffer(),
                    target,
                    point: (x, y),
                });
                gesture.set_state(gtk4::EventSequenceState::Claimed);
            }
        });
        let for_stop = Rc::clone(&pressed);
        gesture.connect_stopped(move |_| {
            for_stop.borrow_mut().take();
        });
        let weak = Rc::downgrade(self);
        gesture.connect_released(move |gesture, count, x, y| {
            let Some(press) = pressed.borrow_mut().take() else {
                return;
            };
            let Some(session) = weak.upgrade() else {
                return;
            };
            if count == 1
                && session.view.widget.is_mapped()
                && link_modifier(gesture.current_event_state())
                && (x - press.point.0).abs() <= 4.0
                && (y - press.point.1).abs() <= 4.0
                && press.buffer == session.view.widget.buffer()
                && session.view.link_at(x, y).as_ref() == Some(&press.target)
            {
                open(&press.target.target);
            }
        });
        self.view.widget.add_controller(gesture);
    }
}

fn link_modifier(modifiers: gdk::ModifierType) -> bool {
    modifiers.contains(gdk::ModifierType::CONTROL_MASK)
        && !modifiers.intersects(
            gdk::ModifierType::SHIFT_MASK
                | gdk::ModifierType::ALT_MASK
                | gdk::ModifierType::META_MASK
                | gdk::ModifierType::SUPER_MASK
                | gdk::ModifierType::HYPER_MASK,
        )
}

fn logical_key(key: gdk::Key) -> Option<std::borrow::Cow<'static, str>> {
    use gdk::Key;
    Some(
        match key {
            Key::Up => "ArrowUp",
            Key::Down => "ArrowDown",
            Key::Left => "ArrowLeft",
            Key::Right => "ArrowRight",
            Key::Home => "Home",
            Key::End => "End",
            Key::Insert => "Insert",
            Key::Delete => "Delete",
            Key::Page_Up => "PageUp",
            Key::Page_Down => "PageDown",
            Key::F1 => "F1",
            Key::F2 => "F2",
            Key::F3 => "F3",
            Key::F4 => "F4",
            Key::F5 => "F5",
            Key::F6 => "F6",
            Key::F7 => "F7",
            Key::F8 => "F8",
            Key::F9 => "F9",
            Key::F10 => "F10",
            Key::F11 => "F11",
            Key::F12 => "F12",
            Key::Return | Key::KP_Enter => "Enter",
            Key::Escape => "Escape",
            Key::BackSpace => "Backspace",
            Key::Tab | Key::ISO_Left_Tab => "Tab",
            _ => {
                return key
                    .to_unicode()
                    .filter(|ch| !ch.is_control())
                    .map(|ch| ch.to_string().into())
            }
        }
        .into(),
    )
}
