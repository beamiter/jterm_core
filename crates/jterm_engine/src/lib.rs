//! Shared terminal semantics, storage, frames and publication.
//! No GUI, PTY, Agent, application identity or host side effects belong here.
pub mod archived_text;
pub mod framing;
mod graphemes;
pub mod grid;
pub mod history_reflow;
pub mod history_search;
pub mod hyperlink;
pub use grid::*;
pub use hyperlink::HyperlinkId;
/// Existing Ember admission bound; unchanged by extraction.
pub const MAX_TERMINAL_COLS: usize = 1024;
pub mod input;
pub mod mouse;
pub mod protocol_state;
pub mod retained_history;

pub mod screen;

pub mod parser;

pub mod semantics;

pub mod frame;

pub mod publication;

pub mod mode_mutation;

pub mod colors;

pub mod titles;

pub mod terminal;

pub mod authenticated_boundaries;
pub mod block_contract;
pub mod command_blocks;
pub mod shell_markers;
