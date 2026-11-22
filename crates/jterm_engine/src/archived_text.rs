//! Immutable styled text for a retained terminal capture.

use crate::grid::{Color, StyleFlags};
use std::ops::Range;
use unicode_width::UnicodeWidthStr;

/// The exact logical text presented by an archive view and the byte ranges
/// carrying presentation attributes. Ranges are sorted and nonoverlapping.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ArchivedText {
    pub text: String,
    pub spans: Vec<ArchiveSpan>,
    pub links: Vec<ArchiveLink>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchiveLink {
    pub bytes: Range<usize>,
    pub target: String,
}

/// Printable Unicode used in completed captures. Reject line and direction
/// controls that could change document structure, while keeping modifiers
/// that belong to a grapheme such as an emoji joiner or a variation selector.
pub fn is_literal_archive_unicode(character: char) -> bool {
    !character.is_ascii()
        && !character.is_control()
        && !matches!(character,
            '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
        )
}

/// Number of display columns occupied by literal text under the engine's
/// width policy, including combining marks and joined emoji.
pub fn literal_text_columns(text: &str) -> usize {
    text.width()
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
