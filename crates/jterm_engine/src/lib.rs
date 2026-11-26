//! Shared terminal semantics, storage, frames and publication.
//! No GUI, PTY, Agent, application identity or host side effects belong here.
pub mod archived_text;
pub mod feed;
pub mod framing;
mod graphemes;
pub mod grid;
pub mod history_reflow;
pub mod history_search;
pub mod hyperlink;
mod row_annotations;
pub use grid::*;
pub use hyperlink::{
    FrostHyperlinkTable, HyperlinkId, MAX_FROST_OSC8_ID_BYTES, MAX_OSC8_URI_BYTES,
};
/// Existing Ember admission bound; unchanged by extraction.
pub const MAX_TERMINAL_COLS: usize = 1024;
pub mod input;
pub mod kitty_graphics;
pub mod mouse;
pub mod protocol_state;
pub mod retained_bytes;
pub mod retained_history;

pub mod display_erase;
pub mod region_ops;
pub mod screen;
pub mod selection;

pub mod parser;

pub mod semantics;

pub mod frame;
pub mod frame_retention;

pub mod publication;

pub mod ascii_write;
pub mod csi_transaction;
pub mod ember_csi;
pub mod frost_controls;
pub mod frost_csi;
pub mod graphic_write;
pub mod mode_mutation;
pub mod native_alternate;
pub mod native_reset;
pub mod native_terminal;
pub mod native_window_ops;
pub mod synchronized_output;
pub mod vt_state;

pub mod colors;

pub mod titles;

pub mod terminal;

pub mod authenticated_boundaries;
pub mod block_contract;
pub mod command_blocks;
pub mod shell_markers;

pub mod clipboard;
pub mod osc;
pub mod osc5522;
