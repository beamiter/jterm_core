//! Transport-independent terminal wire contracts.
#![forbid(unsafe_code)]

#[cfg(feature = "engine-projection")]
pub mod engine_projection;

pub mod file_reference;
pub mod terminal;
pub mod terminal_v4;
