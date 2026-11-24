//! Emulator ownership for a host with an existing ordered PTY reader.

use crate::exit_status::PtyExit;
use crate::viewport::{ViewportFrame, ViewportRequest};
use jterm_engine::colors::TerminalDefaultColors;
use jterm_engine::hyperlink::HyperlinkPolicy;
use jterm_engine::terminal::{
    PlacedInlineImage, PreparedInlineImage, Terminal, WindowUpdates, MAX_FEED_BYTES,
};
use std::cell::{Cell, OnceCell, RefCell};
use std::rc::Rc;

/// Input and process operations supplied by a native host.
pub trait Pty: 'static {
    /// Finish any host-held output before encoding a user's next input.
    fn before_input(&self) {}

    fn write(&self, bytes: &[u8]) -> Result<(), String>;

    fn write_reply(&self, bytes: &[u8]) -> Result<(), String> {
        self.write(bytes)
    }

    fn resize(&self, cols: u16, rows: u16) -> Result<(), String>;
    fn stop(&self);

    fn start_reader(
        &self,
        output: impl FnMut(Vec<u8>) + 'static,
        exited: impl FnOnce(PtyExit) + 'static,
    ) -> Result<(), String>;
}

#[derive(Clone, Copy)]
pub enum TerminalProfile {
    Standalone,
    Embedded,
}

pub enum TerminalUpdate {
    Output(WindowUpdates),
    Exited(PtyExit),
}

// Both the native PTY Session and the host-ordered reader enter the same
// emulator at this boundary. Only runtime code may borrow the engine mutably.
pub(crate) struct Emulator {
    pub(crate) engine: Terminal,
}

impl Emulator {
    pub(crate) fn apply(&mut self, chunk: &[u8]) -> Result<Vec<u8>, String> {
        self.engine.feed(chunk)
    }
}

type Listener = Box<dyn Fn(TerminalUpdate)>;

struct State<P> {
    pty: P,
    emulator: RefCell<Emulator>,
    closed: Cell<bool>,
    listener: OnceCell<Listener>,
}

/// The sole ingress for ordered terminal output. The presentation receives a
/// separate handle, so it cannot feed bytes or publish a child exit.
pub struct TerminalOutput<P> {
    state: Rc<State<P>>,
}

/// Configuration, input and immutable publications of a live terminal.
pub struct TerminalHandle<P> {
    state: Rc<State<P>>,
}

impl<P: Pty> TerminalOutput<P> {
    pub fn new(
        pty: P,
        cols: u16,
        rows: u16,
        profile: TerminalProfile,
        links: HyperlinkPolicy,
    ) -> Result<Self, String> {
        let mut engine = Terminal::new(usize::from(rows), usize::from(cols))?;
        engine.enable_underline_colors();
        engine.enable_grapheme_clusters();
        engine.enable_hyperlinks(links);
        engine.enable_window_updates();
        match profile {
            TerminalProfile::Standalone => engine.enable_primary_reflow(),
            TerminalProfile::Embedded => {
                engine.enable_history_preserving_resize();
                engine.enable_device_answers();
                engine.track_printed_spaces();
            }
        }
        Ok(Self {
            state: Rc::new(State {
                pty,
                emulator: RefCell::new(Emulator { engine }),
                closed: Cell::new(false),
                listener: OnceCell::new(),
            }),
        })
    }

    pub fn handle(&self) -> TerminalHandle<P> {
        TerminalHandle {
            state: Rc::clone(&self.state),
        }
    }

    /// Apply one host-admitted span in order. Even a refused protocol reply
    /// must not prevent parsing later bytes, including the final exit tail.
    pub fn apply(&self, input: &[u8]) -> Result<(), String> {
        if self.state.closed.get() {
            return Err("terminal output is closed".into());
        }
        if input.is_empty() {
            return Ok(());
        }
        let mut reply_error = None;
        let parsed = input.chunks(MAX_FEED_BYTES).try_for_each(|chunk| {
            let reply = self
                .state
                .emulator
                .borrow_mut()
                .apply(chunk)
                .map_err(|error| format!("Terminal output failed: {error}"))?;
            if !reply.is_empty() {
                if let Err(error) = self.state.pty.write_reply(&reply) {
                    reply_error.get_or_insert(error);
                }
            }
            Ok(())
        });
        let window = self
            .state
            .emulator
            .borrow_mut()
            .engine
            .take_window_updates();
        self.notify(TerminalUpdate::Output(window));
        parsed.and_then(|()| match reply_error {
            Some(error) => Err(format!("Terminal reply delivery failed: {error}")),
            None => Ok(()),
        })
    }

    pub fn finish(&self, status: PtyExit) {
        if !self.state.closed.replace(true) {
            self.notify(TerminalUpdate::Exited(status));
        }
    }

    fn notify(&self, update: TerminalUpdate) {
        if let Some(listener) = self.state.listener.get() {
            listener(update);
        }
    }
}

impl<P: Pty> TerminalHandle<P> {
    pub fn pty(&self) -> &P {
        &self.state.pty
    }

    pub fn inspect<T>(&self, read: impl FnOnce(&Terminal) -> T) -> T {
        read(&self.state.emulator.borrow().engine)
    }

    pub fn grid_size(&self) -> Result<(u16, u16), String> {
        let frame = self.inspect(Terminal::frame)?;
        Ok((frame.cols() as u16, frame.rows().len() as u16))
    }

    pub fn on_update(&self, listener: impl Fn(TerminalUpdate) + 'static) -> Result<(), String> {
        self.state
            .listener
            .set(Box::new(listener))
            .map_err(|_| "terminal presentation already attached".into())
    }

    pub fn viewport(&self, request: ViewportRequest) -> Result<Option<ViewportFrame>, String> {
        self.inspect(|terminal| request.capture(terminal, self.state.closed.get()))
    }

    pub fn resize(&self, rows: usize, cols: usize) -> Result<(), String> {
        if self.state.closed.get() {
            return Err("terminal session is closed".into());
        }
        self.state
            .emulator
            .borrow_mut()
            .engine
            .resize_with(rows, cols, || self.pty().resize(cols as u16, rows as u16))
    }

    pub fn set_color_defaults(&self, colors: TerminalDefaultColors) {
        self.state
            .emulator
            .borrow_mut()
            .engine
            .set_color_defaults(colors);
    }

    pub fn set_history_row_limit(&self, rows: usize) {
        self.state
            .emulator
            .borrow_mut()
            .engine
            .set_history_row_limit(rows);
    }

    pub fn track_transcript_rows(&self) {
        self.state
            .emulator
            .borrow_mut()
            .engine
            .track_transcript_rows();
    }

    pub fn place_inline_image(&self, image: PreparedInlineImage) -> Option<PlacedInlineImage> {
        self.state
            .emulator
            .borrow_mut()
            .engine
            .place_inline_image(image)
    }

    pub fn stop(&self) {
        self.state.closed.set(true);
        self.pty().stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ReplySink(Rc<RefCell<Vec<&'static str>>>);

    impl Pty for ReplySink {
        fn write(&self, bytes: &[u8]) -> Result<(), String> {
            assert_eq!(bytes, b"\x1b[0n");
            self.0.borrow_mut().push("reply refused");
            Err("queue busy".into())
        }

        fn resize(&self, _cols: u16, _rows: u16) -> Result<(), String> {
            Ok(())
        }

        fn stop(&self) {}

        fn start_reader(
            &self,
            _output: impl FnMut(Vec<u8>) + 'static,
            _exited: impl FnOnce(PtyExit) + 'static,
        ) -> Result<(), String> {
            panic!("the host supplies ordered output");
        }
    }

    #[test]
    fn refused_reply_does_not_hide_later_output_or_the_exit() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let output = TerminalOutput::new(
            ReplySink(Rc::clone(&events)),
            20,
            4,
            TerminalProfile::Embedded,
            |_, _| false,
        )
        .unwrap();
        let view = output.handle();
        let observed = Rc::clone(&events);
        view.on_update(move |update| {
            observed.borrow_mut().push(match update {
                TerminalUpdate::Output(_) => "output",
                TerminalUpdate::Exited(status) => {
                    assert_eq!(status.code, 17);
                    "exit"
                }
            });
        })
        .unwrap();

        let mut batch = b"\x1b[5n".to_vec();
        batch.resize(MAX_FEED_BYTES, b'\r');
        batch.extend_from_slice(b"exit-tail");
        assert_eq!(
            output.apply(&batch).unwrap_err(),
            "Terminal reply delivery failed: queue busy",
        );
        output.finish(17.into());
        output.finish(99.into());
        assert!(output.apply(b"late output").is_err());
        assert!(view.inspect(|terminal| terminal.visible_text().contains("exit-tail")));
        assert_eq!(*events.borrow(), ["reply refused", "output", "exit"]);
    }
}
