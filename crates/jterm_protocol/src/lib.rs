//! Transport-independent terminal wire contracts.
#![forbid(unsafe_code)]

#[cfg(feature = "engine-projection")]
pub mod engine_projection;

#[cfg(feature = "engine-projection")]
pub mod finished_output;

pub mod file_reference;
pub mod terminal;
pub mod terminal_v4;
