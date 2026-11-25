//! Immutable styled text for a retained terminal capture.

use crate::grid::{Color, StyleFlags};
use std::{mem::size_of, ops::Range};
use unicode_width::UnicodeWidthStr;

pub const MAX_ARCHIVE_SPANS: usize = 512;
pub const MAX_ARCHIVE_LINKS: usize = 128;
const MAX_ARCHIVE_LINK_BYTES: usize = 32 * 1024;

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

/// Annotations over a retained text value. They share its UTF-8 byte
/// coordinates so copying and search can use the source text unchanged.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ArchiveAnnotations {
    pub spans: Vec<ArchiveSpan>,
    pub links: Vec<ArchiveLink>,
}

impl ArchiveAnnotations {
    pub fn is_empty(&self) -> bool {
        self.spans.is_empty() && self.links.is_empty()
    }

    pub fn retained_bytes(&self) -> usize {
        self.spans.len() * size_of::<ArchiveSpan>()
            + self.links.len() * size_of::<ArchiveLink>()
            + self
                .links
                .iter()
                .map(|link| link.target.len())
                .sum::<usize>()
    }

    pub fn valid_for(&self, text: &str) -> bool {
        self.spans.len() <= MAX_ARCHIVE_SPANS
            && self.links.len() <= MAX_ARCHIVE_LINKS
            && self
                .links
                .iter()
                .map(|link| link.target.len())
                .sum::<usize>()
                <= MAX_ARCHIVE_LINK_BYTES
            && valid_ranges(text, self.spans.iter().map(|span| &span.bytes))
            && valid_ranges(text, self.links.iter().map(|link| &link.bytes))
            && self
                .spans
                .iter()
                .all(|span| span.style.flags == span.style.flags.archive_rendition())
            && self.links.iter().all(|link| valid_target(&link.target))
    }

    /// Keep annotations for the retained prefix and suffix when the middle
    /// was replaced with a truncation notice. The notice has default style.
    pub fn retain_head_tail(&mut self, head: usize, suffix: Range<usize>, suffix_offset: usize) {
        self.spans = self
            .spans
            .iter()
            .flat_map(|span| {
                relocated_ranges(&span.bytes, head, &suffix, suffix_offset).map(|bytes| {
                    ArchiveSpan {
                        bytes,
                        style: span.style,
                    }
                })
            })
            .collect();
        self.links = self
            .links
            .iter()
            .flat_map(|link| {
                relocated_ranges(&link.bytes, head, &suffix, suffix_offset).map(|bytes| {
                    ArchiveLink {
                        bytes,
                        target: link.target.clone(),
                    }
                })
            })
            .collect();
    }
}

fn relocated_ranges(
    bytes: &Range<usize>,
    head: usize,
    suffix: &Range<usize>,
    suffix_offset: usize,
) -> impl Iterator<Item = Range<usize>> {
    let prefix_end = bytes.end.min(head);
    let start = bytes.start.max(suffix.start);
    let end = bytes.end.min(suffix.end);
    (bytes.start < prefix_end)
        .then_some(bytes.start..prefix_end)
        .into_iter()
        .chain(
            (start < end).then(|| {
                suffix_offset + (start - suffix.start)..suffix_offset + (end - suffix.start)
            }),
        )
}

fn valid_ranges<'a>(text: &str, mut ranges: impl Iterator<Item = &'a Range<usize>>) -> bool {
    let mut end = 0;
    ranges.all(|range| {
        let valid = range.start >= end
            && range.start < range.end
            && text.is_char_boundary(range.start)
            && text.is_char_boundary(range.end);
        end = range.end;
        valid
    })
}

fn valid_target(target: &str) -> bool {
    !target.is_empty()
        && target.len() <= crate::hyperlink::MAX_OSC8_URI_BYTES
        && target
            .chars()
            .all(|ch| !ch.is_control() && ch != '\u{fffd}')
}

/// Collect compact runs while a renderer extracts the corresponding text.
/// Excessively fragmented attributes are dropped as a unit, leaving the
/// independent text budget and selection coordinates intact.
pub struct ArchiveAnnotationBuilder {
    annotations: Option<ArchiveAnnotations>,
    link_bytes: usize,
}

impl Default for ArchiveAnnotationBuilder {
    fn default() -> Self {
        Self {
            annotations: Some(ArchiveAnnotations::default()),
            link_bytes: 0,
        }
    }
}

impl ArchiveAnnotationBuilder {
    pub fn push(&mut self, bytes: Range<usize>, style: ArchiveStyle, target: Option<&str>) {
        let Some(annotations) = &mut self.annotations else {
            return;
        };
        if bytes.is_empty() {
            return;
        }
        if style != ArchiveStyle::default() {
            let spans = &mut annotations.spans;
            if let Some(tail) = spans
                .last_mut()
                .filter(|tail| tail.bytes.end == bytes.start && tail.style == style)
            {
                tail.bytes.end = bytes.end;
            } else if spans.len() < MAX_ARCHIVE_SPANS {
                spans.push(ArchiveSpan {
                    bytes: bytes.clone(),
                    style,
                });
            } else {
                self.annotations = None;
                return;
            }
        }
        if let Some(target) = target {
            let links = &mut annotations.links;
            if let Some(tail) = links
                .last_mut()
                .filter(|tail| tail.bytes.end == bytes.start && tail.target == target)
            {
                tail.bytes.end = bytes.end;
            } else if links.len() < MAX_ARCHIVE_LINKS
                && self.link_bytes.saturating_add(target.len()) <= MAX_ARCHIVE_LINK_BYTES
                && valid_target(target)
            {
                self.link_bytes += target.len();
                links.push(ArchiveLink {
                    bytes,
                    target: target.to_owned(),
                });
            } else {
                self.annotations = None;
            }
        }
    }

    pub fn finish(self) -> Option<ArchiveAnnotations> {
        self.annotations
            .filter(|annotations| !annotations.is_empty())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArchiveStyle {
    pub foreground: Color,
    pub background: Color,
    pub underline_color: Color,
    pub flags: StyleFlags,
}

impl ArchiveStyle {
    pub fn from_rendition(
        foreground: Color,
        background: Color,
        underline_color: Color,
        flags: StyleFlags,
    ) -> Self {
        Self {
            foreground,
            background,
            underline_color,
            flags: flags.archive_rendition(),
        }
    }
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
