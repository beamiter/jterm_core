//! Portable attributes for the UTF-8 text of a completed output snapshot.
use jterm_engine::{
    archived_text::{
        ArchiveAnnotations, ArchiveLink, ArchiveSpan, ArchiveStyle, MAX_ARCHIVE_LINKS,
        MAX_ARCHIVE_SPANS,
    },
    Color, StyleFlags, UnderlineStyle,
};
use serde::{Deserialize, Serialize};
use std::{mem::size_of, ops::Range};

/// Offsets are UTF-8 bytes into the separate, immutable output text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputAnnotations {
    spans: Vec<Span>,
    links: Vec<Link>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Span {
    bytes: Range<usize>,
    /// Default (zero), indexed (0x01), RGB (0x02), or named ANSI (0x03).
    colors: [u32; 3],
    /// Bold, italic, inverse, strike, dim, and blink in bits 0–5.
    /// Bits 6–8 hold the underline variant (zero through five).
    flags: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Link {
    bytes: Range<usize>,
    target: String,
}

impl OutputAnnotations {
    pub fn new(text: &str, annotations: &ArchiveAnnotations) -> Option<Self> {
        if annotations.is_empty() || !annotations.valid_for(text) {
            return None;
        }
        Some(Self {
            spans: annotations.spans.iter().map(Span::from).collect(),
            links: annotations
                .links
                .iter()
                .map(|link| Link {
                    bytes: link.bytes.clone(),
                    target: link.target.clone(),
                })
                .collect(),
        })
    }

    pub fn restore(self, text: &str) -> Option<ArchiveAnnotations> {
        if self.spans.len() > MAX_ARCHIVE_SPANS || self.links.len() > MAX_ARCHIVE_LINKS {
            return None;
        }
        let restored = ArchiveAnnotations {
            spans: self
                .spans
                .into_iter()
                .map(Span::restore)
                .collect::<Option<_>>()?,
            links: self
                .links
                .into_iter()
                .map(|link| ArchiveLink {
                    bytes: link.bytes,
                    target: link.target,
                })
                .collect(),
        };
        (!restored.is_empty() && restored.valid_for(text)).then_some(restored)
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
}

impl From<&ArchiveSpan> for Span {
    fn from(span: &ArchiveSpan) -> Self {
        let style = span.style;
        let flags = u16::from(style.flags.bold())
            | (u16::from(style.flags.italic()) << 1)
            | (u16::from(style.flags.inverse()) << 2)
            | (u16::from(style.flags.strikethrough()) << 3)
            | (u16::from(style.flags.dim()) << 4)
            | (u16::from(style.flags.blink()) << 5)
            | ((style.flags.underline() as u16) << 6);
        Self {
            bytes: span.bytes.clone(),
            colors: [style.foreground, style.background, style.underline_color].map(encode_color),
            flags,
        }
    }
}

impl Span {
    fn restore(self) -> Option<ArchiveSpan> {
        let mut flags = StyleFlags::default();
        flags.set_bold(self.flags & 1 != 0);
        flags.set_italic(self.flags & 2 != 0);
        flags.set_inverse(self.flags & 4 != 0);
        flags.set_strikethrough(self.flags & 8 != 0);
        flags.set_dim(self.flags & 16 != 0);
        flags.set_blink(self.flags & 32 != 0);
        flags.set_underline(match self.flags >> 6 {
            0 => UnderlineStyle::None,
            1 => UnderlineStyle::Single,
            2 => UnderlineStyle::Double,
            3 => UnderlineStyle::Curly,
            4 => UnderlineStyle::Dotted,
            5 => UnderlineStyle::Dashed,
            _ => return None,
        });
        Some(ArchiveSpan {
            bytes: self.bytes,
            style: ArchiveStyle {
                foreground: decode_color(self.colors[0])?,
                background: decode_color(self.colors[1])?,
                underline_color: decode_color(self.colors[2])?,
                flags,
            },
        })
    }
}

const ANSI: [Color; 16] = [
    Color::Black,
    Color::Red,
    Color::Green,
    Color::Yellow,
    Color::Blue,
    Color::Magenta,
    Color::Cyan,
    Color::White,
    Color::BrightBlack,
    Color::BrightRed,
    Color::BrightGreen,
    Color::BrightYellow,
    Color::BrightBlue,
    Color::BrightMagenta,
    Color::BrightCyan,
    Color::BrightWhite,
];

fn encode_color(color: Color) -> u32 {
    match color {
        Color::Default => 0,
        Color::Indexed(index) => u32::from_be_bytes([1, 0, 0, index]),
        Color::Rgb(r, g, b) => u32::from_be_bytes([2, r, g, b]),
        named => u32::from_be_bytes([
            3,
            0,
            0,
            ANSI.iter()
                .position(|&color| color == named)
                .expect("named ANSI color") as u8,
        ]),
    }
}

fn decode_color(wire: u32) -> Option<Color> {
    match wire.to_be_bytes() {
        [0, 0, 0, 0] => Some(Color::Default),
        [1, 0, 0, index] => Some(Color::Indexed(index)),
        [2, r, g, b] => Some(Color::Rgb(r, g, b)),
        [3, 0, 0, index] => ANSI.get(usize::from(index)).copied(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jterm_engine::archived_text::ArchiveAnnotationBuilder;

    #[test]
    fn utf8_ranges_and_terminal_rendition_round_trip() {
        let text = "中 data\n";
        let mut builder = ArchiveAnnotationBuilder::default();
        let mut flags = StyleFlags::default();
        flags.set_bold(true);
        flags.set_inverse(true);
        flags.set_underline(UnderlineStyle::Curly);
        builder.push(
            0..3,
            ArchiveStyle::from_rendition(
                Color::Red,
                Color::Indexed(1),
                Color::Rgb(11, 22, 33),
                flags,
            ),
            Some("https://example.test/path"),
        );
        let annotations = builder.finish().unwrap();
        let wire = OutputAnnotations::new(text, &annotations).unwrap();
        let json = serde_json::to_string(&wire).unwrap();
        let persisted: OutputAnnotations = serde_json::from_str(&json).unwrap();
        assert_eq!(persisted.restore(text), Some(annotations));

        let invalid = json.replace("\"end\":3", "\"end\":2");
        let invalid: OutputAnnotations = serde_json::from_str(&invalid).unwrap();
        assert!(
            invalid.restore(text).is_none(),
            "a UTF-8 code point was split"
        );
    }
}
