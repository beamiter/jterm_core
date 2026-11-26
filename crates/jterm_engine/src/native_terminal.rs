//! Compatibility names for the native profiles of the shared VT state owner.
//! The reexport is the same object and storage used by hosted Terminal.
pub use crate::vt_state::{
    NativeEffect, NativeProfile, NativeTerminalHost, VtState as NativeTerminal,
};
