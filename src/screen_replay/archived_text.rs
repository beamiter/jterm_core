use super::parser::Params;
use super::pen::{apply_sgr, supported_archive_sgr, Color, Pen, BOLD, ITALIC, STRIKE};
use jterm_engine::archived_text::{ArchiveSpan, ArchiveStyle, ArchivedText};
use jterm_engine::grid::{Color as GridColor, StyleFlags, UnderlineStyle};

const MAX_RETAINED_BYTES: usize = 8 * 1024;
const MAX_TEXT_BYTES: usize = 2 * 1024;
const MAX_LINES: usize = 12;
const MAX_SPANS: usize = 128;

/// Project a bounded finalized capture of plain text and supported SGR into
/// immutable text and style ranges. The caller still owns the retained bytes
/// and the legacy replay decision. No terminal state or reply channel is used.
///
/// Link/path punctuation, tabs, cursor movement, OSC and unsupported SGR
/// stay with the existing archive renderer.
pub fn project_bounded_archive(capture: &str) -> Option<ArchivedText> {
    if capture.len() > MAX_RETAINED_BYTES {
        return None;
    }
    let capture = capture
        .strip_suffix("\r\n")
        .or_else(|| capture.strip_suffix('\n'))
        .unwrap_or(capture);
    let mut output = ArchivedText::default();
    let mut pen = Pen::default();
    let mut style_changes = 0;
    let mut rest = capture;
    while !rest.is_empty() {
        if let Some(sequence) = rest.strip_prefix("\x1b[") {
            style_changes += 1;
            if style_changes >= MAX_SPANS {
                return None;
            }
            let (parameters, tail) = sequence.split_once('m')?;
            let parameters = Params::archive_sgr(parameters.as_bytes())?;
            if !supported_archive_sgr(&parameters) {
                return None;
            }
            apply_sgr(&mut pen, &parameters);
            rest = tail;
            continue;
        }

        let end = rest.find('\x1b').unwrap_or(rest.len());
        if end == 0 {
            return None;
        }
        let run = rest[..end].replace("\r\n", "\n");
        if !run
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b" \n_-=+,%!?()[]'\"".contains(&byte))
        {
            return None;
        }
        let start = output.text.len();
        output.text.push_str(&run);
        if output.text.len() > MAX_TEXT_BYTES {
            return None;
        }
        let style = project_style(&pen);
        if style != ArchiveStyle::default() {
            if let Some(last) = output
                .spans
                .last_mut()
                .filter(|last| last.style == style && last.bytes.end == start)
            {
                last.bytes.end = output.text.len();
            } else {
                if output.spans.len() == MAX_SPANS {
                    return None;
                }
                output.spans.push(ArchiveSpan {
                    bytes: start..output.text.len(),
                    style,
                });
            }
        }
        rest = &rest[end..];
    }
    (output.text.split('\n').count() <= MAX_LINES).then_some(output)
}

fn project_style(pen: &Pen) -> ArchiveStyle {
    let mut flags = StyleFlags::new();
    flags.set_bold(pen.flags & BOLD != 0);
    flags.set_italic(pen.flags & ITALIC != 0);
    flags.set_strikethrough(pen.flags & STRIKE != 0);
    flags.set_underline(match pen.underline() {
        1 => UnderlineStyle::Single,
        2 => UnderlineStyle::Double,
        3 => UnderlineStyle::Curly,
        _ => UnderlineStyle::None,
    });
    // Both finished VTE configurations enable bold-is-bright for the legacy
    // color codes. Explicitly indexed colors retain the chosen index.
    let foreground = match pen.fg {
        Color::Legacy(index @ 0..=7) if flags.bold() => GridColor::Indexed(index + 8),
        color => project_color(color),
    };
    ArchiveStyle {
        foreground,
        background: project_color(pen.bg),
        underline_color: project_color(pen.ul),
        flags,
    }
}

fn project_color(color: Color) -> GridColor {
    match color {
        Color::Default => GridColor::Default,
        Color::Legacy(index) | Color::Indexed(index) => GridColor::Indexed(index),
        Color::Rgb(r, g, b) => GridColor::Rgb(r, g, b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_sgr_projects_exact_text_and_style_ranges() {
        let document = project_bounded_archive(
            "a \x1b[1;31mred\x1b[0m\r\n\x1b[38:2::10:20:30;48;5;230;4:2;58;5;33mink\x1b[m\r\n",
        )
        .unwrap();
        assert_eq!(document.text, "a red\nink");
        assert_eq!(document.spans.len(), 2);
        let red = &document.spans[0];
        assert_eq!(&document.text[red.bytes.clone()], "red");
        assert_eq!(red.style.foreground, GridColor::Indexed(9));
        assert!(red.style.flags.bold());
        let ink = &document.spans[1];
        assert_eq!(&document.text[ink.bytes.clone()], "ink");
        assert_eq!(ink.style.foreground, GridColor::Rgb(10, 20, 30));
        assert_eq!(ink.style.background, GridColor::Indexed(230));
        assert_eq!(ink.style.underline_color, GridColor::Indexed(33));
        assert_eq!(ink.style.flags.underline(), UnderlineStyle::Double);
    }

    #[test]
    fn ineligible_legacy_archive_bytes_cannot_enter_the_document() {
        for input in [
            "capture \x1b[2J after repaint",
            "capture \x1b[2m dim",
            "capture \x1b[4:5m dash",
            "capture \x1b[38;2;100m invalid",
            "capture \x1b]8;;https://example.test\x1b\\linked",
            "file.txt",
            "folder/path",
            "unsafe\rreplacement",
            "tab\tcolumn",
            "capture \x1b[",
        ] {
            assert!(project_bounded_archive(input).is_none(), "{input:?}");
        }
        assert!(project_bounded_archive(&"line\r\n".repeat(13)).is_none());
        assert!(project_bounded_archive(&"a".repeat(2049)).is_none());
        assert_eq!(
            project_bounded_archive("").unwrap(),
            ArchivedText::default()
        );
    }
}
