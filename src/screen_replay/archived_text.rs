use super::pen::{apply_sgr, supported_archive_sgr, Color, Pen, BOLD, ITALIC, REVERSE, STRIKE};
use super::{parser::Params, ScreenReplay, MAX_COLS, MAX_COMPLETED_CAPTURE_BYTES};
use jterm_engine::archived_text::{
    is_literal_archive_unicode, ArchiveLink, ArchiveSpan, ArchiveStyle, ArchivedText,
};
use jterm_engine::grid::{Color as GridColor, StyleFlags, UnderlineStyle};
use std::{borrow::Cow, ops::Range};

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
/// Other OSC commands, cursor movement and unsupported SGR require
/// terminal replay. File references stay as literal document text.
pub fn project_bounded_archive(capture: &str) -> Option<ArchivedText> {
    project_literal_archive(capture)
}

/// A finished card's document, including horizontal terminal edits within a
/// retained line. The card's recorded column count owns the edit coordinates;
/// GTK can subsequently wrap and select the resulting logical text.
pub fn project_finished_output(capture: &str, columns: usize) -> Option<ArchivedText> {
    project_literal_archive(capture).or_else(|| {
        if capture.len() > MAX_COMPLETED_CAPTURE_BYTES || !has_safe_horizontal_edits(capture) {
            return None;
        }
        // With only horizontal edits, one screen row provides the same line
        // semantics. Scrolled rows go into the bounded capture history.
        let mut replay = ScreenReplay::with_budget(columns.clamp(1, MAX_COLS), 1, MAX_LAYOUT_UNITS);
        replay.feed(capture.as_bytes());
        let replay = replay.finish();
        if replay.head_dropped {
            return None;
        }
        project_literal_archive(&replay.to_ansi_with_budget(MAX_COMPLETED_CAPTURE_BYTES)?)
    })
}

fn archive_link(fields: &str) -> Option<(Option<&str>, &str)> {
    let end = fields.find(['\x07', '\x1b'])?;
    let (params, uri) = fields[..end].split_once(';')?;
    let tail = fields[end..]
        .strip_prefix('\x07')
        .or_else(|| fields[end..].strip_prefix("\x1b\\"))?;
    let target = if uri.is_empty() && params.is_empty() {
        None
    } else if crate::link::is_openable_osc8(params, uri) {
        Some(uri)
    } else {
        return None;
    };
    Some((target, tail))
}

/// Admit edits only when their meaning is independent of the live viewport
/// height and its modes. Unknown controls keep the existing replay route.
fn has_safe_horizontal_edits(mut capture: &str) -> bool {
    let mut has_edit = false;
    let mut link = None;
    while !capture.is_empty() {
        if let Some(sequence) = capture.strip_prefix("\x1b[") {
            let Some(end) =
                sequence.find(|ch: char| !ch.is_ascii_digit() && ch != ';' && ch != ':')
            else {
                return false;
            };
            let (parameters, final_and_tail) = sequence.split_at(end);
            let Some((&final_byte, _)) = final_and_tail.as_bytes().split_first() else {
                return false;
            };
            match final_byte {
                b'm' if Params::archive_sgr(parameters.as_bytes())
                    .is_some_and(|params| supported_archive_sgr(&params)) => {}
                b'@' | b'C' | b'D' | b'G' | b'K' | b'P' | b'X' | b'\x60'
                    if parameters.bytes().all(|byte| byte.is_ascii_digit()) =>
                {
                    has_edit = true;
                }
                _ => return false,
            }
            capture = &final_and_tail[1..];
        } else if let Some(fields) = capture.strip_prefix("\x1b]8;") {
            let Some((target, tail)) = archive_link(fields) else {
                return false;
            };
            link = target;
            capture = tail;
        } else {
            let end = capture.find('\x1b').unwrap_or(capture.len());
            if end == 0 {
                return false;
            }
            let text = &capture[..end];
            for (index, ch) in text.char_indices() {
                match ch {
                    '\x08' => has_edit = true,
                    '\r' => has_edit |= text.as_bytes().get(index + 1) != Some(&b'\n'),
                    '\n' | '\t' | ' '..='~' => {}
                    ch if is_literal_archive_unicode(ch) => {}
                    _ => return false,
                }
            }
            capture = &capture[end..];
        }
    }
    has_edit && link.is_none()
}

/// Project a historical command heading whose logical text must match the
/// retained command, including shell punctuation and paths.
pub fn project_bounded_command_archive(capture: &str, command: &str) -> Option<ArchivedText> {
    if command.is_empty() || command.len() > jterm_engine::shell_markers::MAX_OSC133_COMMAND_BYTES {
        return None;
    }
    let archive = project_literal_archive(capture)?;
    (archive.text == command).then_some(archive)
}

fn project_literal_archive(capture: &str) -> Option<ArchivedText> {
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
            (link, rest) = archive_link(fields)?;
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
        layout_units += run.len();
        for byte in run.bytes() {
            layout_units += match byte {
                b'\n' => UNITS_PER_PARAGRAPH,
                b'\t' => 7, // one visible tab can occupy eight cells
                _ => 0,
            };
        }
        if layout_units > MAX_LAYOUT_UNITS {
            return None;
        }
        if !run
            .chars()
            .all(|ch| matches!(ch, '\n' | '\t' | ' '..='~') || is_literal_archive_unicode(ch))
        {
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
    link.is_none().then_some(output)
}

fn project_style(pen: &Pen) -> ArchiveStyle {
    let mut flags = StyleFlags::new();
    flags.set_bold(pen.flags & BOLD != 0);
    flags.set_italic(pen.flags & ITALIC != 0);
    flags.set_inverse(pen.flags & REVERSE != 0);
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

/// Filter the same logical lines that the document presents. Offsets are
/// translated from the immutable source, so a removed line cannot reset
/// inherited colors or leave an OSC 8 link open over another result.
pub fn filter_archived_output(
    document: &ArchivedText,
    query: &str,
    use_regex: bool,
    case_sensitive: bool,
    invert: bool,
    context: usize,
) -> Result<Option<ArchivedText>, regex::Error> {
    if query.is_empty() || document.text.is_empty() {
        return Ok(None);
    }
    let matcher = use_regex
        .then(|| {
            regex::RegexBuilder::new(query)
                .case_insensitive(!case_sensitive)
                .build()
        })
        .transpose()?;
    let lower_query = (!use_regex && !case_sensitive).then(|| query.to_lowercase());
    let lines: Vec<_> = document.text.split('\n').collect();
    let mut keep = vec![false; lines.len()];
    for (index, line) in lines.iter().enumerate() {
        let matched = match (&matcher, &lower_query) {
            (Some(matcher), _) => matcher.is_match(line),
            (_, Some(query)) => line.to_lowercase().contains(query),
            _ => line.contains(query),
        };
        if matched ^ invert {
            let first = index.saturating_sub(context);
            let last = index.saturating_add(context).min(lines.len() - 1);
            keep[first..=last].fill(true);
        }
    }
    if keep.iter().all(|&kept| kept) {
        return Ok(None);
    }

    let mut filtered = ArchivedText::default();
    let mut retained: Vec<Range<usize>> = Vec::new();
    let mut offset = 0;
    for (index, (line, kept)) in lines.into_iter().zip(keep.iter()).enumerate() {
        let end = offset + line.len();
        if *kept {
            if index > 0 && keep[index - 1] {
                retained.last_mut().expect("previous retained line").end = end;
            } else {
                retained.push(offset..end);
            }
        }
        offset = end + 1;
    }
    for (index, source) in retained.into_iter().enumerate() {
        if index > 0 {
            filtered.text.push('\n');
        }
        let dest_start = filtered.text.len();
        filtered.text.push_str(&document.text[source.clone()]);
        let remap = |range: &Range<usize>| {
            let start = range.start.max(source.start);
            let end = range.end.min(source.end);
            (start < end)
                .then(|| dest_start + start - source.start..dest_start + end - source.start)
        };
        let start = document
            .spans
            .partition_point(|span| span.bytes.end <= source.start);
        for span in &document.spans[start..] {
            if span.bytes.start >= source.end {
                break;
            }
            if let Some(bytes) = remap(&span.bytes) {
                if let Some(last) = filtered.spans.last_mut().filter(|last| {
                    last.style == span.style
                        && filtered.text.as_bytes().get(last.bytes.end..bytes.start) == Some(b"\n")
                }) {
                    last.bytes.end = bytes.end;
                } else {
                    filtered.spans.push(ArchiveSpan {
                        bytes,
                        style: span.style,
                    });
                }
            }
        }
        let start = document
            .links
            .partition_point(|link| link.bytes.end <= source.start);
        for link in &document.links[start..] {
            if link.bytes.start >= source.end {
                break;
            }
            if let Some(bytes) = remap(&link.bytes) {
                filtered.links.push(ArchiveLink {
                    bytes,
                    target: link.target.clone(),
                });
            }
        }
    }
    Ok(Some(filtered))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_archive_keeps_shell_punctuation_and_requires_exact_text() {
        let command = "$ printf '%s' ./notes.txt";
        let marked = format!("\x1b[1;32m{command}\x1b[0m");
        let archive = project_bounded_command_archive(&marked, command).unwrap();
        assert_eq!(archive.text, command);
        assert_eq!(archive.spans[0].bytes, 0..command.len());
        assert_eq!(archive.spans[0].style.foreground, GridColor::Indexed(10));
        assert_eq!(project_bounded_archive(&marked).unwrap(), archive);
        assert!(project_bounded_command_archive(&marked, "printf changed").is_none());
        assert!(project_bounded_command_archive(&format!("{marked}\x1b[2J"), command).is_none());
    }

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
    fn plain_urls_and_paths_can_cross_sgr_runs() {
        let document =
            project_bounded_archive("see (https://example.test/\x1b[32ma?b=c\x1b[m).\r\n").unwrap();
        let url = "https://example.test/a?b=c";
        let offset = document.text.find(url).unwrap();
        let hit = crate::link::plain_url_at(&document.text, offset + 22).unwrap();
        assert_eq!(&document.text[hit], url);
        assert_eq!(&document.text[document.spans[0].bytes.clone()], "a?b=c");
        let text = "https://example.test/ and folder/path";
        assert_eq!(project_bounded_archive(text).unwrap().text, text);
        let text = "https://user:token@example.test";
        assert_eq!(project_bounded_archive(text).unwrap().text, text);
        assert!(crate::link::document_url_at(text, 1).is_none());
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
            "unsafe\rreplacement",
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
    fn finished_output_replays_rewrites_and_preserves_tabs_and_color() {
        let capture = "heading\tvalue\r\nprogress 0%\r\x1b[2K\x1b[31mready\x1b[0m!\r\n\
                       paylod\x08ad\r\nstart \x1b[7mhighlight\x1b[27m end";
        assert!(project_bounded_archive(capture).is_none());
        let document = project_finished_output(capture, 80).unwrap();
        assert_eq!(
            document.text,
            "heading\tvalue\nready!\npayload\nstart highlight end"
        );
        let [ready, highlight] = document.spans.as_slice() else {
            panic!("unexpected styles: {:?}", document.spans);
        };
        assert_eq!(&document.text[ready.bytes.clone()], "ready");
        assert_eq!(ready.style.foreground, GridColor::Indexed(1));
        assert_eq!(&document.text[highlight.bytes.clone()], "highlight");
        assert!(highlight.style.flags.inverse());
        assert!(project_finished_output("\r\x1b[2Jmust use screen geometry", 80).is_none());
        assert!(project_finished_output("\r\x1b]0;title\x07outside the archive", 80).is_none());
        assert!(project_finished_output("\r\u{202e}spoofed", 80).is_none());
        assert!(project_finished_output("\r\x1b[2munsupported dim", 80).is_none());
    }

    #[test]
    fn filtering_uses_replayed_text_and_remaps_spans_and_multiline_links() {
        let capture = "\x1b]8;;https://example.test\x07\x1b[31m保留 一\r\nremove this\r\n保留 二\
              \x1b[0m\x1b]8;;\x07\r\nplain";
        let archive = project_finished_output(capture, 80).unwrap();
        let filtered = filter_archived_output(&archive, "^保留", true, true, false, 0)
            .unwrap()
            .unwrap();
        assert_eq!(filtered.text, "保留 一\n保留 二");
        assert_eq!(filtered.spans.len(), 1);
        assert_eq!(filtered.spans[0].bytes, 0..filtered.text.len());
        assert_eq!(filtered.spans[0].style.foreground, GridColor::Indexed(1));
        assert_eq!(filtered.links.len(), 2);
        assert_eq!(&filtered.text[filtered.links[0].bytes.clone()], "保留 一");
        assert_eq!(&filtered.text[filtered.links[1].bytes.clone()], "保留 二");
        assert!(filtered
            .links
            .iter()
            .all(|link| link.target == "https://example.test"));
        assert!(filter_archived_output(&archive, ".", true, true, false, 0)
            .unwrap()
            .is_none());
        assert!(filter_archived_output(&archive, "[", true, true, false, 0).is_err());
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
