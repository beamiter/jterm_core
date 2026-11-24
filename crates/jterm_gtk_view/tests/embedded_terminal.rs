use gtk4::{gdk, glib, pango, prelude::*};
use jterm_engine::terminal::MAX_FEED_BYTES;
use jterm_gtk_view::{default_ansi_palette, EmbeddedTerminal, LinkActions, PaneEvent, Pty, Theme};
use std::{cell::RefCell, rc::Rc, time::Duration};

#[derive(Debug, PartialEq, Eq)]
enum TransportEvent {
    BeforeInput,
    Input(Vec<u8>),
    Reply(Vec<u8>),
    Resize(u16, u16),
    Output,
}

struct Transport(Rc<RefCell<Vec<TransportEvent>>>);

impl Pty for Transport {
    fn before_input(&self) {
        self.0.borrow_mut().push(TransportEvent::BeforeInput);
    }
    fn write(&self, bytes: &[u8]) -> Result<(), String> {
        self.0
            .borrow_mut()
            .push(TransportEvent::Input(bytes.to_vec()));
        Ok(())
    }

    fn write_reply(&self, bytes: &[u8]) -> Result<(), String> {
        self.0
            .borrow_mut()
            .push(TransportEvent::Reply(bytes.to_vec()));
        Ok(())
    }

    fn resize(&self, cols: u16, rows: u16) -> Result<(), String> {
        self.0.borrow_mut().push(TransportEvent::Resize(cols, rows));
        Ok(())
    }

    fn stop(&self) {}

    fn start_reader(
        &self,
        _output: impl FnMut(Vec<u8>) + 'static,
        _exited: impl FnOnce(jterm_gtk_view::PtyExit) + 'static,
    ) -> Result<(), String> {
        panic!("the application owns the only reader");
    }
}

#[test]
#[ignore = "requires a GTK display and a fresh process"]
fn one_embedded_view_keeps_the_app_grid_and_orders_the_fullscreen_reply() {
    gtk4::init().unwrap();
    let events = Rc::new(RefCell::new(Vec::new()));
    let output = jterm_gtk_view::TerminalOutput::new(
        Transport(events.clone()),
        12,
        6,
        jterm_gtk_view::TerminalProfile::Embedded,
        |_, _| false,
    )
    .unwrap();
    let (widget, embedded) = EmbeddedTerminal::attach(
        output.handle(),
        Theme {
            font: pango::FontDescription::from_string("Monospace 10"),
            foreground: gdk::RGBA::WHITE,
            background: gdk::RGBA::BLACK,
            palette: default_ansi_palette(),
            cursor: None,
        },
        |message, _| message.to_owned(),
        LinkActions {
            admit: |_, _| false,
            find_plain: None,
            attach_opener: |_| Box::new(|_| {}),
        },
    )
    .unwrap();
    let pane = embedded.pane();
    let window = gtk4::Window::builder()
        .default_width(240)
        .default_height(48)
        .child(&widget)
        .build();
    window.present();
    let screen = pane.screen().clone();
    let main = glib::MainContext::default();
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !screen.is_mapped() {
        assert!(std::time::Instant::now() < deadline);
        main.iteration(false);
    }
    assert_eq!(pane.grid_size(), (12, 6));
    assert!(events.borrow().is_empty());

    output.apply(b"shell> ").unwrap();
    pane.write_text("run").unwrap();
    output.apply(b"run\r\nworking").unwrap();
    let observing = events.clone();
    assert!(pane.set_event_handler(move |event| {
        if matches!(event, PaneEvent::Output) {
            observing.borrow_mut().push(TransportEvent::Output);
        }
    }));
    assert!(embedded.resize(0, 10).is_err());
    embedded.resize(18, 8).unwrap();
    let mut fullscreen = vec![b'\r'; MAX_FEED_BYTES - 1];
    fullscreen
        .extend_from_slice(b"\x1b[?1049h\x1b[5;7H\x1b[6n\x1b[c\x1b[>31u\x1b[?u\x1b[?4m\x1b[>q");
    output.apply(&fullscreen).unwrap();
    assert_eq!(pane.grid_size(), (18, 8));
    assert!(embedded.inspect(|terminal| terminal.alternate_screen_active()));
    pane.write_text("你e\u{301}").unwrap();
    assert_eq!(
        *events.borrow(),
        [
            TransportEvent::BeforeInput,
            TransportEvent::Input(b"run".to_vec()),
            TransportEvent::Resize(18, 8),
            TransportEvent::Reply(
                b"\x1b[5;7R\x1b[?1;2c\x1b[?1u\x1b[>4;0m\x1bP>|jterm\x1b\\".to_vec()
            ),
            TransportEvent::Output,
            TransportEvent::BeforeInput,
            TransportEvent::Input("你e\u{301}".as_bytes().to_vec()),
        ]
    );
    output.apply(b"\x1b[?1049l").unwrap();
    assert!(embedded.inspect(|terminal| terminal.visible_text().contains("shell> run")));
    assert_eq!(&screen, pane.screen());

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let source = loop {
        if let Some(source) = embedded.retained_text_source() {
            break source;
        }
        assert!(std::time::Instant::now() < deadline);
        main.iteration(false);
    };
    let mut copied = Vec::new();
    embedded.write_retained_text(&source, &mut copied).unwrap();
    assert_eq!(copied, b"shell> run\nworking");
    embedded.resize(17, 8).unwrap();
    copied.clear();
    assert!(embedded.write_retained_text(&source, &mut copied).is_err());
    assert!(copied.is_empty(), "an expired source produced partial text");

    output.finish(17.into());
    assert!(pane.write_text("ignored").is_err());
    assert!(output.apply(b"ignored").is_err());
    window.close();
}
