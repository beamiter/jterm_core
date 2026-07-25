//! Markdown text layer for notebook surfaces: fence splitting and rendering
//! to Pango markup. Pure string processing — the notebook UI and cell
//! execution stay in each app.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    /// Plain markdown text (may contain inline formatting we render with
    /// pango markup).
    Text(String),
    /// A fenced code block. `lang` is the optional info string after the
    /// opening fence (e.g. "bash", "rust", "" for an unlabeled fence).
    Code { lang: String, src: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Fence {
    marker: u8,
    length: usize,
}

fn leading_indent(line: &str) -> Option<&str> {
    let stripped = line.trim_start_matches(' ');
    (line.len() - stripped.len() <= 3).then_some(stripped)
}

fn opening_fence(line: &str) -> Option<(Fence, &str)> {
    let stripped = leading_indent(line)?;
    let marker = *stripped.as_bytes().first()?;
    if marker != b'`' && marker != b'~' {
        return None;
    }
    let length = stripped
        .as_bytes()
        .iter()
        .take_while(|byte| **byte == marker)
        .count();
    if length < 3 {
        return None;
    }
    let info = stripped[length..].trim();
    if marker == b'`' && info.as_bytes().contains(&b'`') {
        return None;
    }
    Some((Fence { marker, length }, info))
}

fn is_closing_fence(line: &str, opening: Fence) -> bool {
    let Some(stripped) = leading_indent(line) else {
        return false;
    };
    let marker_count = stripped
        .as_bytes()
        .iter()
        .take_while(|byte| **byte == opening.marker)
        .count();
    marker_count >= opening.length && stripped[marker_count..].trim().is_empty()
}

fn push_text(segments: &mut Vec<Segment>, text: String) {
    if text.is_empty() {
        return;
    }
    if let Some(Segment::Text(previous)) = segments.last_mut() {
        previous.push_str(&text);
    } else {
        segments.push(Segment::Text(text));
    }
}

/// Split a markdown source into text + code segments. We recognise backtick
/// and tilde fences of length three or greater, including CommonMark's three
/// allowed leading spaces. Unterminated fences remain visible as text.
pub fn parse_segments(input: &str) -> Vec<Segment> {
    let mut segments = Vec::new();
    let mut text = String::new();
    let mut lines = input.lines();

    while let Some(line) = lines.next() {
        let Some((fence, info)) = opening_fence(line) else {
            text.push_str(line);
            text.push('\n');
            continue;
        };

        push_text(&mut segments, std::mem::take(&mut text));
        let mut source = String::new();
        let mut closed = false;
        for inner in lines.by_ref() {
            if is_closing_fence(inner, fence) {
                closed = true;
                break;
            }
            source.push_str(inner);
            source.push('\n');
        }

        if closed {
            if source.ends_with('\n') {
                source.pop();
            }
            segments.push(Segment::Code {
                lang: info.to_owned(),
                src: source,
            });
        } else {
            text.push_str(line);
            text.push('\n');
            text.push_str(&source);
        }
    }

    push_text(&mut segments, text);
    segments
}

/// Render the markdown body of a `Text` segment to a Pango-markup string.
/// Just enough to look like markdown without pulling in a full crate:
/// `#/##/###` → bolded sized text, `**x**` → bold, `*x*` → italic,
/// `` `x` `` → monospace span. All other text passes through after XML
/// escaping.
pub fn render_text_to_pango(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for raw_line in text.lines() {
        let line = raw_line.trim_end();
        if line.is_empty() {
            out.push('\n');
            continue;
        }
        // Heading detection — only if the # appears at line start.
        let (open, body, close) = if let Some(rest) = line.strip_prefix("### ") {
            ("<span weight=\"bold\" size=\"large\">", rest, "</span>")
        } else if let Some(rest) = line.strip_prefix("## ") {
            ("<span weight=\"bold\" size=\"x-large\">", rest, "</span>")
        } else if let Some(rest) = line.strip_prefix("# ") {
            ("<span weight=\"bold\" size=\"xx-large\">", rest, "</span>")
        } else {
            ("", line, "")
        };
        out.push_str(open);
        out.push_str(&render_inline(body));
        out.push_str(close);
        out.push('\n');
    }
    out
}

/// Apply inline-format rules. Conservative: only matches the simplest
/// form. Nested or overlapping markers fall through unchanged.
fn render_inline(s: &str) -> String {
    let escaped = escape_pango(s);
    // Backtick spans first so subsequent ** / * passes don't see their
    // interior. We do these as separate scans rather than one big regex
    // to avoid the regex dep and to keep failure modes obvious.
    let with_code = wrap_marker(&escaped, "`", "<tt>", "</tt>");
    let with_bold = wrap_marker(&with_code, "**", "<b>", "</b>");
    wrap_marker(&with_bold, "*", "<i>", "</i>")
}

fn wrap_marker(s: &str, marker: &str, open: &str, close: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    loop {
        let Some(start) = rest.find(marker) else {
            out.push_str(rest);
            return out;
        };
        out.push_str(&rest[..start]);
        let after = &rest[start + marker.len()..];
        match after.find(marker) {
            Some(end) => {
                out.push_str(open);
                out.push_str(&after[..end]);
                out.push_str(close);
                rest = &after[end + marker.len()..];
            }
            None => {
                // No closing marker — leave the original alone.
                out.push_str(&rest[start..]);
                return out;
            }
        }
    }
}

fn escape_pango(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_text_and_code_fences() {
        let md = "Intro line\n```bash\necho hi\n```\nMiddle\n```\nls\n```\ntail";
        let segs = parse_segments(md);
        assert_eq!(segs.len(), 5);
        assert!(matches!(segs[0], Segment::Text(_)));
        assert!(matches!(segs[1], Segment::Code { .. }));
        assert!(matches!(segs[2], Segment::Text(_)));
        assert!(matches!(segs[3], Segment::Code { .. }));
        assert!(matches!(segs[4], Segment::Text(_)));
        if let Segment::Code { lang, src } = &segs[1] {
            assert_eq!(lang, "bash");
            assert_eq!(src, "echo hi");
        }
        if let Segment::Code { lang, src } = &segs[3] {
            assert_eq!(lang, "");
            assert_eq!(src, "ls");
        }
    }

    #[test]
    fn tilde_fences_recognised() {
        let md = "~~~sh\nwhoami\n~~~\n";
        let segs = parse_segments(md);
        assert_eq!(segs.len(), 1);
        assert!(matches!(segs[0], Segment::Code { .. }));
    }

    #[test]
    fn longer_fence_does_not_close_on_a_shorter_marker() {
        let md = "````bash\necho ``` literal\n```\n````\n";
        assert_eq!(
            parse_segments(md),
            vec![Segment::Code {
                lang: "bash".to_owned(),
                src: "echo ``` literal\n```".to_owned(),
            }]
        );
    }

    #[test]
    fn unterminated_fence_falls_back_to_text() {
        let md = "before\n```bash\necho oops\nno closing fence here\n";
        let segs = parse_segments(md);
        // Should be one big text segment, not a code segment.
        assert!(segs.iter().all(|s| matches!(s, Segment::Text(_))));
    }

    #[test]
    fn empty_input_yields_no_segments() {
        assert!(parse_segments("").is_empty());
    }

    #[test]
    fn escape_pango_handles_ampersand_and_angles() {
        assert_eq!(escape_pango("a & b < c > d"), "a &amp; b &lt; c &gt; d");
    }

    #[test]
    fn render_inline_bolds_and_italicises() {
        let r = render_inline("look at **this** and *that*");
        assert!(r.contains("<b>this</b>"), "got {r}");
        assert!(r.contains("<i>that</i>"), "got {r}");
    }

    #[test]
    fn render_inline_wraps_backtick_code() {
        let r = render_inline("run `ls -la` please");
        assert!(r.contains("<tt>ls -la</tt>"), "got {r}");
    }

    #[test]
    fn render_inline_leaves_unmatched_markers_alone() {
        // A single unmatched ** must not produce dangling tags or panic.
        let r = render_inline("oops **forgot to close");
        assert!(!r.contains("<b>"), "got {r}");
    }

    #[test]
    fn render_text_to_pango_handles_headings() {
        let out = render_text_to_pango("# Title\n\nbody");
        assert!(out.contains("Title</span>"), "got {out}");
    }

    #[test]
    fn fence_with_leading_spaces_recognised() {
        let md = "  ```bash\necho ok\n  ```\n";
        let segs = parse_segments(md);
        assert!(
            matches!(segs.first(), Some(Segment::Code { .. })),
            "got {segs:?}"
        );
    }
}
