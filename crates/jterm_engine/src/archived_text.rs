//! Immutable styled text for a retained terminal capture.

use crate::grid::{Color, StyleFlags};
use std::ops::Range;

/// The exact logical text presented by an archive view and the byte ranges
/// carrying presentation attributes. Ranges are sorted and nonoverlapping.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ArchivedText {
    pub text: String,
    pub spans: Vec<ArchiveSpan>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchiveSpan {
    pub bytes: Range<usize>,
    pub style: ArchiveStyle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArchiveStyle {
    pub foreground: Color,
    pub background: Color,
    pub underline_color: Color,
    pub flags: StyleFlags,
}

impl Default for ArchiveStyle {
    fn default() -> Self {
        Self {
            foreground: Color::Default,
            background: Color::Default,
            underline_color: Color::Default,
            flags: StyleFlags::default(),
        }
    }
}
