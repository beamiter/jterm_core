use super::pen::{apply_sgr, supported_archive_sgr, Color, Pen, BOLD, ITALIC, STRIKE};
use super::{parser::Params, MAX_COMPLETED_CAPTURE_BYTES};
use jterm_engine::archived_text::{
    is_literal_archive_unicode, ArchiveLink, ArchiveSpan, ArchiveStyle, ArchivedText,
};
use jterm_engine::grid::{Color as GridColor, StyleFlags, UnderlineStyle};
use std::borrow::Cow;

// The view retains a text buffer plus indexing and layout for each paragraph.
// Charge empty paragraphs too, so a small newline flood cannot build an
// unbounded GTK document. The host retains the original capture separately.
const MAX_LAYOUT_UNITS: usize = 256 * 1024;
const UNITS_PER_PARAGRAPH: usize = 64;
const MAX_SPANS: usize = 512;
const MAX_LINKS: usize = 128;

/// Project a bounded finalized capture of plain text and supported SGR into
/// immutable text and style ranges. The caller still owns the retained bytes
/// and the legacy replay decision. No terminal state or reply channel is used.
///
/// Accept explicit OSC 8 targets through the same opener policy as the live
/// terminal. Plain HTTP(S) targets remain in the text for exact hit-testing.
/// Other OSC commands, cursor movement, tabs, path hints and unsupported SGR
/// require the original renderer.
pub fn project_bounded_archive(capture: &str) -> Option<ArchivedText> {
    if capture.len() > MAX_COMPLETED_CAPTURE_BYTES {
        return None;
    }
    let capture = capture
        .strip_suffix("\r\n")
        .or_else(|| capture.strip_suffix('\n'))
        .unwrap_or(capture);
    let mut output = ArchivedText::default();
    let mut pen = Pen::default();
    let mut style_changes = 0;
    let mut layout_units = UNITS_PER_PARAGRAPH;
    let mut link = None;
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
        if let Some(fields) = rest.strip_prefix("\x1b]8;") {
            let end = fields.find(['\x07', '\x1b'])?;
            let (params, uri) = fields[..end].split_once(';')?;
            rest = fields[end..]
                .strip_prefix('\x07')
                .or_else(|| fields[end..].strip_prefix("\x1b\\"))?;
            link = if uri.is_empty() && params.is_empty() {
                None
            } else if crate::link::is_openable_osc8(params, uri) {
                Some(uri)
            } else {
                return None;
            };
            continue;
        }

        let end = rest.find('\x1b').unwrap_or(rest.len());
        if end == 0 {
            return None;
        }
        let raw = &rest[..end];
        // Normalizing CRLF can at most halve the length of the run.
        if raw.len() > 2 * (MAX_LAYOUT_UNITS - layout_units) {
            return None;
        }
        let run = if raw.contains('\r') {
            Cow::Owned(raw.replace("\r\n", "\n"))
        } else {
            Cow::Borrowed(raw)
        };
        layout_units += run.len()
            + run.as_bytes().iter().filter(|&&byte| byte == b'\n').count() * UNITS_PER_PARAGRAPH;
        if layout_units > MAX_LAYOUT_UNITS {
            return None;
        }
        // The card filter can drop entire raw lines. Keep each link's open
        // and close in one logical line so filtering cannot extend its target.
        if link.is_some() && run.contains('\n') {
            return None;
        }
        if !run.chars().all(|ch| {
            ch == '\n' || (ch.is_ascii() && !ch.is_control()) || is_literal_archive_unicode(ch)
        }) {
            return None;
        }
        let start = output.text.len();
        output.text.push_str(&run);
        if let Some(target) = link {
            if let Some(previous) = output
                .links
                .last_mut()
                .filter(|previous| previous.target == target && previous.bytes.end == start)
            {
                previous.bytes.end = output.text.len();
            } else {
                if output.links.len() >= MAX_LINKS {
                    return None;
                }
                output.links.push(ArchiveLink {
                    bytes: start..output.text.len(),
                    target: target.to_owned(),
                });
            }
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
    (link.is_none()
        && output.text.split_whitespace().all(|token| {
            token.chars().all(|ch| {
                ch.is_ascii_alphanumeric() || "_-=+,%!?()[]'\"".contains(ch) || !ch.is_ascii()
            }) || crate::link::plain_url_span(token).is_some()
        }))
    .then_some(output)
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
    fn completed_capture_preserves_unicode_clusters_and_style_byte_ranges() {
        let document =
            project_bounded_archive("结果 \x1b[32me\u{301} 👩‍💻\x1b[m\r\n翌日\r\n").unwrap();
        assert_eq!(document.text, "结果 e\u{301} 👩‍💻\n翌日");
        assert_eq!(document.spans.len(), 1);
        let span = &document.spans[0];
        assert_eq!(&document.text[span.bytes.clone()], "e\u{301} 👩‍💻");
        assert_eq!(span.style.foreground, GridColor::Indexed(2));
    }

    #[test]
    fn archive_link_ranges_preserve_the_exact_label_across_sgr_and_unicode() {
        let document = project_bounded_archive(
            "指南 \x1b]8;id=one;https://example.test/a?q=b\x1b\\查看\x1b[32m页面\x1b[m\x1b]8;;\x07 today",
        ).unwrap();
        assert_eq!(document.text, "指南 查看页面 today");
        assert_eq!(document.links.len(), 1);
        let link = &document.links[0];
        assert_eq!(&document.text[link.bytes.clone()], "查看页面");
        assert_eq!(link.target, "https://example.test/a?q=b");
    }

    #[test]
    fn a_plain_url_can_cross_sgr_runs_without_admitting_path_hints() {
        let document =
            project_bounded_archive("see (https://example.test/\x1b[32ma?b=c\x1b[m).\r\n").unwrap();
        let url = "https://example.test/a?b=c";
        let offset = document.text.find(url).unwrap();
        let hit = crate::link::plain_url_at(&document.text, offset + 22).unwrap();
        assert_eq!(&document.text[hit], url);
        assert_eq!(&document.text[document.spans[0].bytes.clone()], "a?b=c");
        assert!(project_bounded_archive("https://example.test/ and folder/path").is_none());
        assert!(project_bounded_archive("https://user:token@example.test").is_none());
    }

    #[test]
    fn ineligible_legacy_archive_bytes_cannot_enter_the_document() {
        for input in [
            "capture \x1b[2J after repaint",
            "capture \x1b[2m dim",
            "capture \x1b[4:5m dash",
            "capture \x1b[38;2;100m invalid",
            "capture \x1b]8;;file:///tmp/item\x1b\\linked\x1b]8;;\x07",
            "capture \x1b]8;;https://user@example.test\x07linked",
            "capture \x1b]8;;https://example.test\x1b[0mlinked",
            "capture \x1b]8;;https://example.test\x07unclosed",
            "capture \x1b]8;;https://example.test\x07link\r\nnext\x1b]8;;\x07",
            "file.txt",
            "folder/path",
            "unsafe\rreplacement",
            "tab\tcolumn",
            "spoof \u{202e}name",
            "line\u{2028}separator",
            "capture \x1b[",
        ] {
            assert!(project_bounded_archive(input).is_none(), "{input:?}");
        }
        assert_eq!(
            project_bounded_archive("").unwrap(),
            ArchivedText::default()
        );
    }

    #[test]
    fn long_outputs_keep_their_tail_and_charge_blank_paragraphs() {
        let capture = format!(
            "{}\x1b[31mtail witness\x1b[0m\r\n",
            "visible line\r\n".repeat(1200)
        );
        let document = project_bounded_archive(&capture).expect("full archived text");
        assert_eq!(document.text.lines().count(), 1201);
        assert!(document.text.ends_with("\ntail witness"));
        assert_eq!(
            &document.text[document.spans[0].bytes.clone()],
            "tail witness"
        );

        let empty_paragraphs = "\n".repeat(MAX_LAYOUT_UNITS / UNITS_PER_PARAGRAPH + 1);
        assert!(project_bounded_archive(&empty_paragraphs).is_none());
        assert!(project_bounded_archive(&"a".repeat(MAX_LAYOUT_UNITS + 1)).is_none());
    }
}
