//! Extracted terminal primitives, not yet a complete terminal emulator.
//! No GUI, PTY, Agent, application identity or host side effects belong here.
pub mod framing;
pub mod grid;
pub mod hyperlink;
pub use grid::*;
pub use hyperlink::HyperlinkId;
/// Existing Ember admission bound; unchanged by extraction.
pub const MAX_TERMINAL_COLS: usize = 1024;
pub mod input;
pub mod mouse;
pub mod protocol_state;

pub mod screen;

pub mod parser;

pub mod semantics;

pub mod frame;

pub mod publication;

pub mod mode_mutation;
