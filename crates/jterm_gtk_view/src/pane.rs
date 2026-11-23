#[cfg(target_os = "linux")]
use futures_util::StreamExt;
use gtk4::{gdk, glib, pango, prelude::*};
use jterm_engine::{
    colors::{Rgb, TerminalDefaultColors},
    frame::Screen,
    history_search::{HistorySearch, HistorySearchProgress},
    hyperlink::HyperlinkPolicy,
    mouse::MouseInput,
    terminal::{Terminal, WindowUpdates},
};
use std::{
    cell::{Cell, OnceCell, RefCell},
    rc::Rc,
};

use crate::history_search::HistorySearchView;
#[cfg(target_os = "linux")]
use crate::runtime_shell::RuntimeShell;
use crate::text_link::{connect_links, link_modifier};
use crate::view::TerminalView;
use crate::viewport::{
    HistoricalWindow, HistoryRail, ScrollTarget, ViewportFrame, ViewportRequest,
};

pub(crate) const INITIAL_ROWS: usize = 24;
pub(crate) const INITIAL_COLUMNS: usize = 80;

enum Backend<P> {
    Legacy {
        pty: P,
        terminal: Box<RefCell<Terminal>>,
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

#[derive(Clone)]
pub struct Theme {
    pub font: pango::FontDescription,
    pub foreground: gdk::RGBA,
    pub background: gdk::RGBA,
    pub palette: [gdk::RGBA; 16],
}

impl Theme {
    fn terminal_colors(&self) -> TerminalDefaultColors {
        fn rgb(color: gdk::RGBA) -> Rgb {
            let component = |value: f32| (value * 255.0).round() as u8;
            (
                component(color.red()),
                component(color.green()),
                component(color.blue()),
            )
        }
        TerminalDefaultColors {
            foreground: rgb(self.foreground),
            background: rgb(self.background),
            cursor: rgb(self.foreground),
            ansi: self.palette.map(rgb),
        }
    }
}

pub type HyperlinkOpener = Box<dyn Fn(&str)>;
/// Return a host-admitted URL range containing the indicated UTF-8 offset.
pub type PlainLinkFinder = fn(&str, usize) -> Option<std::ops::Range<usize>>;

pub struct LinkActions {
    pub admit: HyperlinkPolicy,
    pub find_plain: Option<PlainLinkFinder>,
    pub attach_opener: fn(&gtk4::Widget) -> HyperlinkOpener,
}

#[derive(Clone)]
pub enum PaneEvent {
    Output,
    TitleChanged(String),
    Bell,
    Exited(i32),
}

type OnClose = Box<dyn Fn(&gtk4::Widget)>;
type OnEvent = Box<dyn Fn(PaneEvent)>;

#[derive(Clone, Copy)]
enum InputStyle {
    Terminal,
    HostedAttachment { inline_history: bool },
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct PointerMotion {
    column: usize,
    row: usize,
    button: Option<u8>,
    modifiers: u8,
}

pub struct TerminalPane<P: Pty> {
    backend: Backend<P>,
    status_text: fn(&str, usize) -> String,
    view: TerminalView,
    plain_links: Option<PlainLinkFinder>,
    status: gtk4::Label,
    event_handler: OnceCell<OnEvent>,
    viewport: glib::WeakRef<gtk4::ScrolledWindow>,
    dimensions: Cell<(usize, usize)>,
    scrollback: gtk4::Adjustment,
    scroll_top: Cell<Option<ScrollTarget>>,
    history_rail: RefCell<HistoryRail>,
    history_search: OnceCell<Rc<HistorySearchView>>,
    scroll_update: Cell<bool>,
    wheel_rows: Cell<f64>,
    reported_buttons: Cell<u8>,
    last_pointer_motion: Cell<Option<PointerMotion>>,
    last_pointer_cell: Cell<Option<(usize, usize)>>,
    pointer_wheel: Cell<f64>,
    painted_history: Cell<Option<HistoricalWindow>>,
    dirty: Cell<bool>,
    exited: Cell<bool>,
    input_focused: Cell<bool>,
    input_method: gtk4::IMMulticontext,
    keys: gtk4::EventControllerKey,
    input_method_delegated: Cell<bool>,
    input_style: InputStyle,
    input_cursor: Cell<Option<gdk::Rectangle>>,
    paste_revision: Cell<u64>,
    paste_reader: RefCell<Option<glib::JoinHandle<()>>>,
    input_revision: Cell<u64>,
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
        terminal.enable_grapheme_clusters();
        terminal.enable_hyperlinks(links.admit);
        terminal.enable_window_updates();
        Self::attach(
            Backend::Legacy {
                pty,
                terminal: Box::new(RefCell::new(terminal)),
            },
            theme,
            status_text,
            links,
            Some(Box::new(close)),
            InputStyle::Terminal,
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
        Self::attach(
            Backend::Runtime(runtime),
            theme,
            status_text,
            links,
            Some(Box::new(close)),
            InputStyle::Terminal,
        )
    }

    #[cfg(target_os = "linux")]
    pub fn with_runtime_embedded(
        runtime: Rc<RuntimeShell>,
        theme: Theme,
        status_text: fn(&str, usize) -> String,
        links: LinkActions,
    ) -> Result<(gtk4::Widget, Rc<Self>), String> {
        Self::attach(
            Backend::Runtime(runtime),
            theme,
            status_text,
            links,
            None,
            InputStyle::Terminal,
        )
    }

    /// Display an attachment to a PTY owned by the Session Host.
    #[cfg(target_os = "linux")]
    pub fn with_hosted_attachment(
        runtime: Rc<RuntimeShell>,
        theme: Theme,
        status_text: fn(&str, usize) -> String,
        links: LinkActions,
    ) -> Result<(gtk4::Widget, Rc<Self>), String> {
        Self::attach(
            Backend::Runtime(runtime),
            theme,
            status_text,
            links,
            None,
            InputStyle::HostedAttachment {
                inline_history: std::env::var_os("JTERM_AGENT_SCROLLBACK")
                    .is_none_or(|value| value != "0"),
            },
        )
    }

    fn attach(
        backend: Backend<P>,
        theme: Theme,
        status_text: fn(&str, usize) -> String,
        links: LinkActions,
        close: Option<OnClose>,
        input_style: InputStyle,
    ) -> Result<(gtk4::Widget, Rc<Self>), String> {
        let (rows, columns) = (INITIAL_ROWS, INITIAL_COLUMNS);
        let colors = theme.terminal_colors();
        let view = TerminalView::new(theme);
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
        let column = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        if let Some(close) = close {
            let close_button = gtk4::Button::from_icon_name("window-close-symbolic");
            close_button.set_tooltip_text(Some("Close shared terminal tab"));
            close_button.update_property(&[gtk4::accessible::Property::Label(
                "Close shared terminal tab",
            )]);
            let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
            header.append(&gtk4::Label::new(Some("Shared terminal")));
            header.append(&status);
            close_button.set_halign(gtk4::Align::End);
            header.append(&close_button);
            column.append(&header);
            let weak_page = column.downgrade();
            close_button.connect_clicked(move |_| {
                if let Some(page) = weak_page.upgrade() {
                    close(page.upcast_ref());
                }
            });
        } else {
            column.append(&status);
        }
        column.append(&body);
        let page = column.upcast::<gtk4::Widget>();

        let session = Rc::new(Self {
            backend,
            status_text,
            view,
            plain_links: links.find_plain,
            status,
            event_handler: OnceCell::new(),
            viewport: scroll.downgrade(),
            dimensions: Cell::new((rows, columns)),
            scrollback,
            scroll_top: Cell::new(None),
            history_rail: RefCell::default(),
            history_search: OnceCell::new(),
            scroll_update: Cell::new(false),
            wheel_rows: Cell::new(0.0),
            reported_buttons: Cell::new(0),
            last_pointer_motion: Cell::new(None),
            last_pointer_cell: Cell::new(None),
            pointer_wheel: Cell::new(0.0),
            painted_history: Cell::new(None),
            dirty: Cell::new(true),
            exited: Cell::new(false),
            input_focused: Cell::new(false),
            input_method: gtk4::IMMulticontext::new(),
            keys: gtk4::EventControllerKey::new(),
            input_method_delegated: Cell::new(false),
            input_style,
            input_cursor: Cell::new(None),
            paste_revision: Cell::new(0),
            paste_reader: RefCell::new(None),
            input_revision: Cell::new(0),
        });
        if session.pty().is_some() {
            session.resize(rows, columns);
        }
        session.connect_input();
        let open_link = (links.attach_opener)(session.view.widget.upcast_ref());
        let weak = Rc::downgrade(&session);
        connect_links(
            &session.view.widget,
            move |x, y| {
                let pane = weak.upgrade()?;
                pane.view.link_at(x, y, pane.plain_links)
            },
            open_link,
        );
        session.connect_pointer();
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
            if !session.exited.get() && scroll.width() > 0 && scroll.height() > 0 {
                let (rows, columns) = session.view.dimensions(scroll.width(), scroll.height());
                if session.dimensions.get() != (rows, columns) {
                    session.resize(rows, columns);
                }
            }
            session.paint();
            session.sync_input_cursor();
            glib::ControlFlow::Continue
        });
        match &session.backend {
            Backend::Legacy { pty, terminal } => {
                terminal.borrow_mut().set_color_defaults(colors);
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
            Backend::Runtime(owner) => {
                session
                    .view
                    .widget
                    .update_property(&[gtk4::accessible::Property::Description(
                        "Shared runtime session",
                    )]);
                let mut updates = owner.configure_view(links.admit, colors)?;
                let pane = Rc::downgrade(&session);
                glib::MainContext::default().spawn_local(async move {
                    while updates.next().await.is_some() {
                        let Some(pane) = pane.upgrade() else { break };
                        pane.observe_runtime();
                    }
                });
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

    fn inspect<T>(&self, read: impl FnOnce(&Terminal) -> T) -> Result<T, String> {
        match &self.backend {
            Backend::Legacy { terminal, .. } => Ok(read(&terminal.borrow())),
            #[cfg(target_os = "linux")]
            Backend::Runtime(owner) => owner.inspect(read),
        }
    }

    #[cfg(target_os = "linux")]
    fn observe_runtime(&self) {
        let Some(owner) = self.runtime() else { return };
        let update = match owner.take_update() {
            Ok(update) => update,
            Err(error) => {
                if !self.exited.replace(true) {
                    self.set_input_focus(false);
                    self.report(&error);
                }
                return;
            }
        };
        if update.output_changed || update.exit_code.is_some() || update.stopped {
            self.dirty.set(true);
        }
        if update.output_changed {
            self.notify(PaneEvent::Output);
        }
        self.notify_window(update.window);
        if let Some(code) = update.exit_code {
            self.report_exit(code);
        }
        if update.stopped && !self.exited.replace(true) {
            self.set_input_focus(false);
        }
        if let Some(error) = update.failure {
            self.report(&format!("Terminal runtime failure: {error}"));
        }
        if self.dirty.get()
            && self.view.widget.is_mapped()
            && !self.view.widget.buffer().has_selection()
        {
            owner.prepare_viewport(self.viewport_request());
        }
    }

    fn report_exit(&self, code: i32) {
        if !self.exited.replace(true) {
            self.set_input_focus(false);
            self.report(&format!("Shell exited ({code})"));
            self.notify(PaneEvent::Exited(code));
        }
    }

    pub fn set_event_handler(&self, handler: impl Fn(PaneEvent) + 'static) -> bool {
        self.event_handler.set(Box::new(handler)).is_ok()
    }

    fn notify(&self, event: PaneEvent) {
        if let Some(handler) = self.event_handler.get() {
            handler(event);
        }
    }

    fn notify_window(&self, update: WindowUpdates) {
        if let Some(title) = update.title {
            self.notify(PaneEvent::TitleChanged(title));
        }
        if update.bell {
            self.notify(PaneEvent::Bell);
        }
    }

    pub fn screen(&self) -> &gtk4::TextView {
        &self.view.widget
    }

    pub fn window_title(&self) -> Option<String> {
        self.inspect(|terminal| terminal.window_title().map(str::to_owned))
            .ok()
            .flatten()
    }

    pub fn set_theme(&self, theme: Theme) {
        let colors = theme.terminal_colors();
        match &self.backend {
            Backend::Legacy { terminal, .. } => terminal.borrow_mut().set_color_defaults(colors),
            #[cfg(target_os = "linux")]
            Backend::Runtime(owner) => {
                if let Err(error) = owner.set_color_defaults(colors) {
                    self.report(&format!("Cannot update terminal colors: {error}"));
                }
            }
        }
        self.view.set_theme(theme);
        // A fully historical viewport can otherwise suppress the repaint.
        self.painted_history.set(None);
        self.dirty.set(true);
    }

    pub fn set_font(&self, font: &pango::FontDescription) {
        self.view.set_font(font);
        self.painted_history.set(None);
        self.dirty.set(true);
    }

    pub fn set_line_height_scale(&self, scale: f64) {
        self.view.set_line_height_scale(scale);
        self.dirty.set(true);
    }

    pub fn set_history_row_limit(&self, rows: usize) {
        match &self.backend {
            Backend::Legacy { terminal, .. } => terminal.borrow_mut().set_history_row_limit(rows),
            #[cfg(target_os = "linux")]
            Backend::Runtime(owner) => {
                if let Err(error) = owner.set_history_row_limit(rows) {
                    self.report(&format!("Cannot set terminal scrollback: {error}"));
                    return;
                }
            }
        }
        self.dirty.set(true);
    }

    /// Local screen origin and cell size in GTK logical pixels.
    pub fn cell_metrics(&self) -> (i32, i32, i32, i32) {
        self.view.cell_metrics()
    }

    pub fn grid_matches_allocation(&self) -> bool {
        self.viewport.upgrade().is_some_and(|viewport| {
            viewport.is_mapped()
                && viewport.width() > 0
                && viewport.height() > 0
                && self.dimensions.get()
                    == self.view.dimensions(viewport.width(), viewport.height())
        })
    }

    pub fn has_selection(&self) -> bool {
        self.view.widget.buffer().has_selection()
    }

    /// Let an overlay place and present composition while this screen keeps
    /// ownership of keys, committed input, and the PTY.
    pub fn delegate_composition(&self, delegated: bool) {
        if self.input_method_delegated.replace(delegated) != delegated && self.input_focused.get() {
            self.set_composition_active(!delegated);
        }
    }

    pub fn write_text(&self, text: &str) -> Result<(), String> {
        self.submit_input(|terminal| terminal.encode_text(text))
            .map(|_| ())
    }

    pub fn input_revision(&self) -> u64 {
        self.input_revision.get()
    }

    pub fn write_input(&self, input: Vec<u8>) -> Result<(), String> {
        if input.len() > jterm_engine::terminal::MAX_INPUT_BYTES {
            return Err("terminal input byte limit".into());
        }
        self.submit_input(|_| Ok(input)).map(|_| ())
    }

    pub fn scroll(&self, lines: i64) {
        self.scroll_rows(lines);
    }

    pub fn toggle_history_search(self: &Rc<Self>) {
        let Some(parent) = self.view.widget.root().and_downcast::<gtk4::Window>() else {
            return;
        };
        self.history_search
            .get_or_init(|| HistorySearchView::new(self))
            .toggle(&parent);
    }

    pub(crate) async fn history_search_step(
        &self,
        mut search: HistorySearch,
    ) -> Result<(HistorySearch, HistorySearchProgress), String> {
        match &self.backend {
            Backend::Legacy { terminal, .. } => {
                let progress = search.step(&terminal.borrow())?;
                Ok((search, progress))
            }
            #[cfg(target_os = "linux")]
            Backend::Runtime(owner) => owner.history_search_step(search).await,
        }
    }

    pub fn follow_output(&self) {
        self.set_scroll_top(None);
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
        self.set_input_focus(false);
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
        if let Some(text) = self.view.selected_text() {
            self.view.widget.clipboard().set_text(&text);
        }
    }

    pub fn paste(self: &Rc<Self>) {
        if self.exited.get() || !self.input_focused.get() {
            return;
        }
        let revision = self.advance_paste_revision();
        let session = Rc::downgrade(self);
        let clipboard = self.view.widget.clipboard();
        let bounded = self.pty().is_none();
        let task = glib::MainContext::default().spawn_local(async move {
            let result = if bounded {
                read_native_clipboard(&clipboard).await
            } else {
                clipboard
                    .read_text_future()
                    .await
                    .map(|text| text.map(String::from))
                    .map_err(|error| error.to_string())
            };
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
            session.paste_reader.borrow_mut().take();
            match result {
                Ok(Some(text)) => {
                    #[cfg(target_os = "linux")]
                    if let Some(owner) = session.runtime() {
                        match owner.paste(&text) {
                            Ok(true) => session.input_admitted(),
                            Ok(false) => (),
                            Err(error) => {
                                session.report(&format!("Terminal paste rejected: {error}"))
                            }
                        }
                        return;
                    }
                    session.send_input(|terminal| {
                        terminal.encode_paste(&text).map(|mut bytes| {
                            if !bytes.starts_with(b"\x1b[200~") {
                                // Copying a trailing line break is common. Preserve the
                                // explicit Enter required by unbracketed shell input.
                                if let Some(end) = bytes.iter().position(|byte| *byte == b'\r') {
                                    bytes.truncate(end);
                                }
                            }
                            bytes
                        })
                    });
                }
                Ok(None) => {}
                Err(error) => session.report(&format!("Cannot read clipboard: {error}")),
            }
        });
        self.paste_reader.replace(Some(task));
    }

    fn advance_paste_revision(&self) -> u64 {
        if let Some(task) = self.paste_reader.borrow_mut().take() {
            task.abort();
        }
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
        let (reply, window) = {
            let mut terminal = terminal.borrow_mut();
            let reply = terminal.feed(input);
            (reply, terminal.take_window_updates())
        };
        self.dirty.set(true);
        self.notify(PaneEvent::Output);
        self.notify_window(window);
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
        let exited = self.exited.get();
        let request = self.viewport_request();
        let snapshot = match &self.backend {
            Backend::Legacy { terminal, .. } => request
                .capture(&terminal.borrow(), exited)
                .map(|frame| frame.map(|frame| (frame, false))),
            #[cfg(target_os = "linux")]
            Backend::Runtime(owner) => owner.poll_viewport(request),
        };
        let (
            ViewportFrame {
                frame,
                live_margin_gaps,
                colors,
                extent,
                history,
                evicted_anchor,
            },
            newer_output,
        ) = match snapshot {
            Ok(Some(snapshot)) => snapshot,
            Ok(None) => return,
            Err(error) => {
                self.report(&format!("Cannot display terminal output: {error}"));
                return;
            }
        };
        let history_top = history.as_ref().map(|history| history.first);
        if frame.stamp().screen == Screen::Primary {
            self.scroll_top.set(
                history
                    .as_ref()
                    .map(|history| ScrollTarget::Source(history.anchor)),
            );
        }
        if evicted_anchor && !exited {
            self.report("Earlier scrollback is no longer retained");
        }
        let rail = HistoryRail {
            extent,
            layout: history.as_ref().map(|history| history.layout.clone()),
        };
        let total = rail.len();
        self.history_rail.replace(rail);
        self.sync_scrollback(total, history_top, frame.rows().len());
        match history {
            Some(history) => {
                if let Some(page) = history.page {
                    self.view
                        .present(&frame, live_margin_gaps, Some(&page), colors.as_ref());
                }
                self.painted_history.set(Some(history.window));
            }
            None => {
                self.view
                    .present(&frame, live_margin_gaps, None, colors.as_ref());
                self.painted_history.set(None);
            }
        }
        self.dirty.set(newer_output);
    }

    fn viewport_request(&self) -> ViewportRequest {
        ViewportRequest {
            top: self.scroll_top.get(),
            painted_history: self.painted_history.get(),
        }
    }

    fn sync_scrollback(&self, total: usize, top: Option<usize>, rows: usize) {
        self.scroll_update.set(true);
        self.scrollback.configure(
            top.unwrap_or(total) as f64,
            0.0,
            total as f64 + rows as f64,
            1.0,
            rows.saturating_sub(1).max(1) as f64,
            rows as f64,
        );
        self.scroll_update.set(false);
    }

    fn set_scroll_top(&self, top: Option<ScrollTarget>) {
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
        let next = {
            let history = self.history_rail.borrow();
            if history.extent.is_empty() {
                return false;
            }
            history.scroll(self.scroll_top.get(), rows)
        };
        self.set_scroll_top(next);
        true
    }

    fn scroll_key(&self, key: gdk::Key, modifiers: gdk::ModifierType) -> bool {
        use gdk::ModifierType;
        if modifiers.intersects(
            ModifierType::CONTROL_MASK
                | ModifierType::ALT_MASK
                | ModifierType::META_MASK
                | ModifierType::SUPER_MASK
                | ModifierType::HYPER_MASK,
        ) {
            return false;
        }
        let shifted = modifiers.contains(ModifierType::SHIFT_MASK);
        if !shifted
            && !(matches!(
                self.input_style,
                InputStyle::HostedAttachment {
                    inline_history: true
                }
            ) && matches!(
                key,
                gdk::Key::Page_Up
                    | gdk::Key::Page_Down
                    | gdk::Key::KP_Page_Up
                    | gdk::Key::KP_Page_Down
            ) && self.inspect(Terminal::alternate_screen_active) == Ok(false))
        {
            return false;
        }
        let page = self.dimensions.get().0.saturating_sub(1).max(1) as i64;
        let delta = match key {
            gdk::Key::Page_Up => -page,
            gdk::Key::Page_Down => page,
            gdk::Key::KP_Page_Up if !shifted => -page,
            gdk::Key::KP_Page_Down if !shifted => page,
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
            let target = session
                .history_rail
                .borrow()
                .target_at(adjustment.value().round() as usize);
            session.set_scroll_top(target);
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
            ) {
                session.wheel_rows.set(0.0);
                session.pointer_wheel.set(0.0);
                return glib::Propagation::Proceed;
            }
            if let Some(event) = controller.current_event() {
                if session.report_wheel(&event, dy, controller.unit()) {
                    return glib::Propagation::Stop;
                }
            }
            if session.history_rail.borrow().extent.is_empty() {
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

    fn submit_input(
        &self,
        encode: impl FnOnce(&Terminal) -> Result<Vec<u8>, String>,
    ) -> Result<bool, String> {
        let accepted = self.queue_input(encode)?;
        if accepted {
            self.input_admitted();
        }
        Ok(accepted)
    }

    fn input_admitted(&self) {
        self.input_revision
            .set(self.input_revision.get().wrapping_add(1));
        self.clear_selection();
        self.set_scroll_top(None);
    }

    fn queue_input(
        &self,
        encode: impl FnOnce(&Terminal) -> Result<Vec<u8>, String>,
    ) -> Result<bool, String> {
        if self.exited.get() {
            return Err("terminal input is closed".into());
        }
        match &self.backend {
            Backend::Legacy { pty, terminal } => encode(&terminal.borrow()).and_then(|bytes| {
                if bytes.is_empty() {
                    return Ok(false);
                }
                pty.write(&bytes).map(|()| true)
            }),
            #[cfg(target_os = "linux")]
            Backend::Runtime(owner) => owner.input(encode),
        }
    }

    fn send_input(&self, encode: impl FnOnce(&Terminal) -> Result<Vec<u8>, String>) {
        if let Err(error) = self.submit_input(encode) {
            self.report(&format!("Terminal input rejected: {error}"));
        }
    }

    fn report_focus(&self, focused: bool) {
        if !self.exited.get() {
            let _ = self.queue_input(|terminal| {
                Ok(terminal.encode_focus(focused).unwrap_or_default().to_vec())
            });
        }
    }

    fn connect_pointer(self: &Rc<Self>) {
        let events = gtk4::EventControllerLegacy::new();
        events.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let weak = Rc::downgrade(self);
        events.connect_event(move |_, event| {
            weak.upgrade()
                .is_some_and(|session| session.report_pointer(event))
                .into()
        });
        self.view.widget.add_controller(events);
    }

    fn report_pointer(&self, event: &gdk::Event) -> bool {
        let state = event.modifier_state();
        let modifiers = mouse_modifiers(state);
        let button = || {
            let event = event.downcast_ref::<gdk::ButtonEvent>()?;
            (1..=3)
                .contains(&event.button())
                .then(|| event.button() as u8 - 1)
        };
        let (input, released) = match event.event_type() {
            gdk::EventType::ButtonPress => {
                let Some(button) = button() else { return false };
                (MouseInput::Press(button), false)
            }
            gdk::EventType::ButtonRelease => {
                let Some(button) = button() else { return false };
                let mask = 1 << button;
                let previous = self.reported_buttons.get();
                self.reported_buttons.set(previous & !mask);
                if previous & mask == 0 {
                    return false;
                }
                (MouseInput::Release(button), true)
            }
            gdk::EventType::MotionNotify => {
                let buttons = self.reported_buttons.get();
                (
                    MouseInput::Motion((0..3).find(|button| buttons & (1 << button) != 0)),
                    false,
                )
            }
            gdk::EventType::GrabBroken => {
                self.reported_buttons.set(0);
                self.last_pointer_motion.set(None);
                self.last_pointer_cell.set(None);
                return false;
            }
            gdk::EventType::LeaveNotify => {
                self.last_pointer_motion.set(None);
                self.last_pointer_cell.set(None);
                return false;
            }
            _ => return false,
        };
        if self.exited.get()
            || (!released
                && (self.scroll_top.get().is_some()
                    || state.contains(gdk::ModifierType::SHIFT_MASK)))
        {
            return released;
        }
        let Some(point) = self.view.event_point(event) else {
            return released;
        };
        if input == MouseInput::Press(0)
            && link_modifier(state)
            && self
                .view
                .link_at(point.0, point.1, self.plain_links)
                .is_some()
        {
            return false;
        }
        let Some((column, row)) = self.view.cell_at(point, self.dimensions.get(), released) else {
            self.last_pointer_cell.set(None);
            return released;
        };
        self.last_pointer_cell.set(Some((column, row)));
        let motion = if let MouseInput::Motion(button) = input {
            Some(PointerMotion {
                column,
                row,
                button,
                modifiers,
            })
        } else {
            self.last_pointer_motion.set(None);
            None
        };
        let already_sent = motion.is_some() && self.last_pointer_motion.get() == motion;
        let mut reporting = false;
        let sent = self
            .queue_input(|terminal| {
                let Some(bytes) = terminal.encode_mouse(input, column, row, modifiers) else {
                    return Ok(Vec::new());
                };
                reporting = true;
                Ok(if already_sent { Vec::new() } else { bytes })
            })
            .unwrap_or(false);
        if sent {
            self.last_pointer_motion.set(motion);
            if let MouseInput::Press(button) = input {
                let buttons = self.reported_buttons.get() | (1 << button);
                self.reported_buttons.set(buttons);
                // The press has already reported this cell. GTK can deliver
                // motion from the same cell when the button grab starts.
                self.last_pointer_motion.set(Some(PointerMotion {
                    column,
                    row,
                    button: (0..3).find(|button| buttons & (1 << button) != 0),
                    modifiers,
                }));
                self.view.widget.grab_focus();
            }
        } else if !reporting {
            self.last_pointer_motion.set(None);
        }
        reporting || released
    }

    fn report_wheel(&self, event: &gdk::Event, dy: f64, unit: gdk::ScrollUnit) -> bool {
        if self.exited.get()
            || self.scroll_top.get().is_some()
            || dy == 0.0
            || event
                .modifier_state()
                .contains(gdk::ModifierType::SHIFT_MASK)
        {
            self.pointer_wheel.set(0.0);
            return false;
        }
        let Some((column, row)) = self
            .view
            .event_point(event)
            .and_then(|point| self.view.cell_at(point, self.dimensions.get(), false))
            .or_else(|| self.last_pointer_cell.get())
        else {
            return false;
        };
        let delta = match event
            .downcast_ref::<gdk::ScrollEvent>()
            .map(|scroll| scroll.direction())
        {
            Some(gdk::ScrollDirection::Up) => -1.0,
            Some(gdk::ScrollDirection::Down) => 1.0,
            _ if unit == gdk::ScrollUnit::Surface => dy / self.view.row_height(),
            _ => dy,
        };
        let pending = self.pointer_wheel.get() + delta;
        let count = pending.trunc().abs().min(8.0) as usize;
        let input = if pending < 0.0 {
            MouseInput::WheelUp
        } else {
            MouseInput::WheelDown
        };
        let mut reporting = false;
        let result = self.queue_input(|terminal| {
            let Some(bytes) =
                terminal.encode_mouse(input, column, row, mouse_modifiers(event.modifier_state()))
            else {
                return Ok(Vec::new());
            };
            reporting = true;
            Ok(bytes.repeat(count))
        });
        self.pointer_wheel
            .set(if reporting { pending.fract() } else { 0.0 });
        reporting && result.is_ok()
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
        if !self.accepts_ime() || self.input_method_delegated.get() {
            return;
        }
        if let Some(area) = self.view.input_cursor() {
            if self.input_cursor.replace(Some(area)) != Some(area) {
                self.input_method.set_cursor_location(&area);
            }
        }
    }

    fn accepts_ime(&self) -> bool {
        !self.exited.get() && self.view.widget.is_mapped() && self.view.widget.has_focus()
    }

    fn set_composition_active(&self, active: bool) {
        if active {
            self.keys.set_im_context(Some(&self.input_method));
            self.input_method.focus_in();
            self.sync_input_cursor();
        } else {
            self.input_cursor.set(None);
            self.input_method.focus_out();
            self.reset_preedit();
            self.keys.set_im_context(None::<&gtk4::IMContext>);
        }
    }

    fn set_input_focus(&self, focused: bool) {
        if self.input_focused.replace(focused) == focused {
            return;
        }
        self.report_focus(focused);
        if !focused {
            self.advance_paste_revision();
        }
        self.set_composition_active(focused && !self.input_method_delegated.get());
    }

    fn connect_input(self: &Rc<Self>) {
        let keys = &self.keys;
        keys.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let im = &self.input_method;
        im.set_client_widget(Some(&self.view.widget));
        im.set_use_preedit(true);
        // Set terminal input properties before mapping. GTK can then bind the
        // Wayland text-input object ahead of the compositor's first focus.
        im.set_input_purpose(gtk4::InputPurpose::Terminal);
        let weak = Rc::downgrade(self);
        im.connect_commit(move |_, text| {
            if let Some(session) = weak
                .upgrade()
                .filter(|session| session.accepts_ime() && !session.input_method_delegated.get())
            {
                session.view.clear_preedit();
                session.send_input(|terminal| terminal.encode_text(text));
            }
        });
        let weak = Rc::downgrade(self);
        im.connect_preedit_changed(move |im| {
            let Some(session) = weak
                .upgrade()
                .filter(|session| session.accepts_ime() && !session.input_method_delegated.get())
            else {
                return;
            };
            let (text, attributes, _) = im.preedit_string();
            if text.is_empty() {
                session.view.clear_preedit();
            } else {
                session.set_scroll_top(None);
                session.view.show_preedit(&text, attributes);
                session.sync_input_cursor();
            }
        });
        let weak = Rc::downgrade(self);
        im.connect_preedit_end(move |_| {
            if let Some(session) = weak.upgrade() {
                session.view.clear_preedit();
            }
        });
        let weak = Rc::downgrade(self);
        self.view.widget.connect_has_focus_notify(move |_| {
            if let Some(session) = weak.upgrade() {
                session.set_input_focus(session.accepts_ime());
            }
        });
        let weak = Rc::downgrade(self);
        self.view.widget.connect_map(move |_| {
            if let Some(session) = weak.upgrade() {
                session.set_input_focus(session.accepts_ime());
            }
        });
        let weak = Rc::downgrade(self);
        self.view.widget.connect_unmap(move |_| {
            if let Some(session) = weak.upgrade() {
                session.set_input_focus(false);
                if let Some(search) = session.history_search.get() {
                    search.hide();
                }
            }
        });
        let weak = Rc::downgrade(self);
        keys.connect_key_pressed(move |controller, key, keycode, modifiers| {
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
            let shift =
                key == gdk::Key::ISO_Left_Tab || modifiers.contains(gdk::ModifierType::SHIFT_MASK);
            let alt = modifiers.contains(gdk::ModifierType::ALT_MASK);
            let ctrl = modifiers.contains(gdk::ModifierType::CONTROL_MASK);
            if matches!(session.input_style, InputStyle::HostedAttachment { .. })
                && key == gdk::Key::bracketright
                && ctrl
                && !shift
                && !alt
            {
                // The relay consumes this byte before writing to the provider.
                // Its meaning is independent of the provider's keyboard mode.
                session.send_input(|_| Ok(vec![0x1d]));
                return glib::Propagation::Stop;
            }
            let base = if (alt || ctrl) && key.to_unicode().is_some() {
                controller
                    .current_event()
                    .and_then(|event| event.downcast::<gdk::KeyEvent>().ok())
                    .and_then(|event| {
                        session.view.widget.display().translate_key(
                            keycode,
                            gdk::ModifierType::empty(),
                            event.layout() as i32,
                        )
                    })
                    .and_then(|(base, _, _, _)| base.to_unicode())
            } else {
                None
            };
            session.send_input(|terminal| {
                terminal.encode_key_with_base(&name, base, shift, alt, ctrl)
            });
            glib::Propagation::Stop
        });
        self.view.widget.add_controller(keys.clone());
    }
}

impl<P: Pty> Drop for TerminalPane<P> {
    fn drop(&mut self) {
        if let Some(search) = self.history_search.get() {
            search.hide();
        }
        if let Some(task) = self.paste_reader.get_mut().take() {
            task.abort();
        }
    }
}

async fn read_native_clipboard(clipboard: &gdk::Clipboard) -> Result<Option<String>, String> {
    use jterm_engine::terminal::MAX_STREAMED_PASTE_BYTES;

    const TEXT_FORMATS: &[&str] = &["text/plain;charset=utf-8", "text/plain"];
    let formats = clipboard.formats();
    if !TEXT_FORMATS
        .iter()
        .any(|mime| formats.contain_mime_type(mime))
    {
        return Ok(None);
    }
    let (stream, _) = clipboard
        .read_future(TEXT_FORMATS, glib::Priority::DEFAULT)
        .await
        .map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    loop {
        let count = 4096.min(MAX_STREAMED_PASTE_BYTES + 1 - bytes.len());
        let chunk = stream
            .read_bytes_future(count, glib::Priority::DEFAULT)
            .await
            .map_err(|error| error.to_string())?;
        if chunk.is_empty() {
            break;
        }
        bytes.extend_from_slice(&chunk);
        if bytes.len() > MAX_STREAMED_PASTE_BYTES {
            return Err("clipboard paste byte limit".into());
        }
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| "clipboard text is not UTF-8".into())
}

fn mouse_modifiers(modifiers: gdk::ModifierType) -> u8 {
    (u8::from(modifiers.contains(gdk::ModifierType::SHIFT_MASK)) << 2)
        | (u8::from(modifiers.contains(gdk::ModifierType::ALT_MASK)) << 3)
        | (u8::from(modifiers.contains(gdk::ModifierType::CONTROL_MASK)) << 4)
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
            Key::Page_Up | Key::KP_Page_Up => "PageUp",
            Key::Page_Down | Key::KP_Page_Down => "PageDown",
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
