//! Bounded, read-only comparison of two retained command-output snapshots.
//!
//! Hosts own command identities, capture availability, and native presentation.
//! A missing snapshot must not be supplied as an empty string. The comparison
//! never reads a terminal, runs a command, or changes a persisted history format.

/// Maximum UTF-8 source bytes examined from either snapshot.
pub const MAX_INPUT_BYTES: usize = 256 * 1024;
/// Maximum physical lines examined from either snapshot.
pub const MAX_INPUT_LINES: usize = 4096;
/// Maximum cells allocated and visited for exact middle-region alignment.
pub const MAX_ALIGNMENT_CELLS: usize = 1024 * 1024;
/// Maximum bytes in the rendered comparison, including its limitation notice.
pub const MAX_RENDER_BYTES: usize = 512 * 1024;

const RENDER_LIMIT_NOTICE: &str =
    "\n[Comparison display clipped; counts describe the bounded input, not only the lines shown.]\n";

/// Already-captured, plain terminal output. An empty *available* capture is valid.
#[derive(Clone, Copy, Debug)]
pub struct OutputSnapshot<'a> {
    pub text: &'a str,
    /// The host knows that part of this command's output was not retained.
    pub truncated: bool,
}

/// An owned report that remains stable while the originating terminal changes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputComparison {
    pub text: String,
    pub added_lines: usize,
    pub removed_lines: usize,
    pub unchanged_lines: usize,
    pub before_limited: bool,
    pub after_limited: bool,
    pub alignment_limited: bool,
    pub rendering_limited: bool,
}

impl OutputComparison {
    /// False whenever capture, input, alignment, or display limits apply.
    pub fn is_complete(&self) -> bool {
        !self.before_limited
            && !self.after_limited
            && !self.alignment_limited
            && !self.rendering_limited
    }

    /// Compact native-dialog heading. Never claims equality of missing output.
    pub fn summary(&self) -> String {
        let mut summary = format!(
            "+{} added / -{} removed / {} unchanged lines",
            self.added_lines, self.removed_lines, self.unchanged_lines
        );
        if self.before_limited || self.after_limited {
            summary.push_str("; partial captured output");
        }
        if self.alignment_limited {
            summary.push_str("; coarse alignment");
        }
        if self.rendering_limited {
            summary.push_str("; display clipped");
        }
        summary
    }
}

struct BoundedLines<'a> {
    lines: Vec<&'a str>,
    limited: bool,
}

fn bounded_lines(snapshot: OutputSnapshot<'_>) -> BoundedLines<'_> {
    let mut end = snapshot.text.len().min(MAX_INPUT_BYTES);
    while !snapshot.text.is_char_boundary(end) {
        end -= 1;
    }
    let mut remaining = snapshot.text[..end].split_inclusive('\n');
    let lines = remaining.by_ref().take(MAX_INPUT_LINES).collect();
    BoundedLines {
        lines,
        limited: snapshot.truncated || end < snapshot.text.len() || remaining.next().is_some(),
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Same,
    Removed,
    Added,
}

fn align<'a>(before: &[&'a str], after: &[&'a str]) -> (Vec<(Kind, &'a str)>, bool) {
    let common = before.len().min(after.len());
    let mut prefix = 0;
    while prefix < common && before[prefix] == after[prefix] {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < common - prefix
        && before[before.len() - 1 - suffix] == after[after.len() - 1 - suffix]
    {
        suffix += 1;
    }
    let mut edits = Vec::with_capacity(before.len().saturating_add(after.len()));
    edits.extend(before[..prefix].iter().map(|line| (Kind::Same, *line)));
    let old = &before[prefix..before.len() - suffix];
    let new = &after[prefix..after.len() - suffix];
    let mut alignment_limited = false;

    if old.is_empty() {
        edits.extend(new.iter().map(|line| (Kind::Added, *line)));
    } else if new.is_empty() {
        edits.extend(old.iter().map(|line| (Kind::Removed, *line)));
    } else if let Some(cells) = (old.len() + 1)
        .checked_mul(new.len() + 1)
        .filter(|cells| *cells <= MAX_ALIGNMENT_CELLS)
    {
        // Each LCS length is at most MAX_INPUT_LINES (4096), well below u16::MAX.
        // At most 2 MiB is allocated; input size cannot trigger a quadratic blowup.
        let width = new.len() + 1;
        let mut lengths = vec![0_u16; cells];
        for i in (0..old.len()).rev() {
            for j in (0..new.len()).rev() {
                lengths[i * width + j] = if old[i] == new[j] {
                    lengths[(i + 1) * width + j + 1] + 1
                } else {
                    lengths[(i + 1) * width + j].max(lengths[i * width + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < old.len() && j < new.len() {
            if old[i] == new[j] {
                edits.push((Kind::Same, old[i]));
                i += 1;
                j += 1;
            } else if lengths[(i + 1) * width + j] >= lengths[i * width + j + 1] {
                edits.push((Kind::Removed, old[i]));
                i += 1;
            } else {
                edits.push((Kind::Added, new[j]));
                j += 1;
            }
        }
        edits.extend(old[i..].iter().map(|line| (Kind::Removed, *line)));
        edits.extend(new[j..].iter().map(|line| (Kind::Added, *line)));
    } else {
        // Preserve every bounded line, with an explicit non-minimal alignment
        // notice. Common prefix/suffix still provide useful surrounding context.
        alignment_limited = true;
        edits.extend(old.iter().map(|line| (Kind::Removed, *line)));
        edits.extend(new.iter().map(|line| (Kind::Added, *line)));
    }
    edits.extend(
        before[before.len() - suffix..]
            .iter()
            .map(|line| (Kind::Same, *line)),
    );
    (edits, alignment_limited)
}

struct ReportWriter {
    text: String,
    clipped: bool,
}

impl ReportWriter {
    fn new() -> Self {
        Self {
            text: String::new(),
            clipped: false,
        }
    }

    fn push(&mut self, text: &str) {
        if self.clipped {
            return;
        }
        let remaining =
            (MAX_RENDER_BYTES - RENDER_LIMIT_NOTICE.len()).saturating_sub(self.text.len());
        let mut end = text.len().min(remaining);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        self.text.push_str(&text[..end]);
        self.clipped = end < text.len();
    }

    fn line(&mut self, kind: Kind, line: &str) {
        if self.clipped {
            return;
        }
        self.push(match kind {
            Kind::Same => " ",
            Kind::Removed => "-",
            Kind::Added => "+",
        });
        let contents = line.strip_suffix('\n').unwrap_or(line);
        for character in contents.chars() {
            if self.clipped {
                break;
            }
            if character == '\\'
                || character.is_control()
                || crate::review_input::is_terminal_visual_spoofing_character(character)
            {
                // escape_default is bounded (at most ten ASCII bytes). Unicode
                // letters remain readable; control/bidi text cannot hide edits.
                for escaped in character.escape_default() {
                    let mut buffer = [0_u8; 4];
                    self.push(escaped.encode_utf8(&mut buffer));
                }
            } else {
                let mut buffer = [0_u8; 4];
                self.push(character.encode_utf8(&mut buffer));
            }
        }
        self.push("\n");
        if !line.ends_with('\n') {
            self.push("\\ No newline at end of compared text\n");
        }
    }

    fn finish(mut self) -> (String, bool) {
        if self.clipped {
            self.text.push_str(RENDER_LIMIT_NOTICE);
        }
        (self.text, self.clipped)
    }
}

/// Compare two available snapshots, earlier first. Exact line endings matter:
/// CR is shown as an escape and a missing final newline is explicitly marked.
///
/// The common prefix/suffix is found in linear time. The middle is aligned by
/// LCS only within a fixed cell budget; larger middles use marked coarse edits.
/// All limits apply before expensive alignment or expanded display allocation.
pub fn compare(before: OutputSnapshot<'_>, after: OutputSnapshot<'_>) -> OutputComparison {
    let before = bounded_lines(before);
    let after = bounded_lines(after);
    let (edits, alignment_limited) = align(&before.lines, &after.lines);
    let mut report = OutputComparison {
        text: String::new(),
        added_lines: 0,
        removed_lines: 0,
        unchanged_lines: 0,
        before_limited: before.limited,
        after_limited: after.limited,
        alignment_limited,
        rendering_limited: false,
    };
    let mut writer = ReportWriter::new();
    writer.push("--- Earlier retained output\n+++ Later retained output\n");
    if before.limited {
        writer.push("[Earlier output is partial: capture or comparison input limit reached.]\n");
    }
    if after.limited {
        writer.push("[Later output is partial: capture or comparison input limit reached.]\n");
    }
    if alignment_limited {
        writer.push(
            "[Coarse comparison: alignment budget reached; changed-region counts are not a minimal edit distance.]\n",
        );
    }
    writer.push(&format!(
        "@@ -{},{} +{},{} @@\n",
        usize::from(!before.lines.is_empty()),
        before.lines.len(),
        usize::from(!after.lines.is_empty()),
        after.lines.len()
    ));
    for (kind, line) in edits {
        match kind {
            Kind::Same => report.unchanged_lines += 1,
            Kind::Removed => report.removed_lines += 1,
            Kind::Added => report.added_lines += 1,
        }
        writer.line(kind, line);
    }
    if report.added_lines == 0 && report.removed_lines == 0 {
        writer.push(if before.limited || after.limited {
            "[No differences in the compared text; uncaptured or unexamined output may differ.]\n"
        } else {
            "[The two available output snapshots are identical.]\n"
        });
    }
    (report.text, report.rendering_limited) = writer.finish();
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(text: &str) -> OutputSnapshot<'_> {
        OutputSnapshot {
            text,
            truncated: false,
        }
    }

    #[test]
    fn replacement_keeps_unchanged_context() {
        let report = compare(snapshot("start\nold\nend\n"), snapshot("start\nnew\nend\n"));
        assert_eq!(report.added_lines, 1);
        assert_eq!(report.removed_lines, 1);
        assert_eq!(report.unchanged_lines, 2);
        assert!(report.text.contains(" start\n-old\n+new\n end\n"));
        assert!(report.is_complete());
    }

    #[test]
    fn insertion_does_not_shift_every_following_line() {
        let report = compare(snapshot("a\nb\nc\n"), snapshot("a\ninserted\nb\nc\n"));
        assert_eq!((report.added_lines, report.removed_lines), (1, 0));
        assert_eq!(report.unchanged_lines, 3);
        assert!(report.text.contains("+inserted\n b\n"));
    }

    #[test]
    fn empty_available_captures_and_blank_lines_are_distinct() {
        let empty = compare(snapshot(""), snapshot(""));
        assert_eq!(empty.unchanged_lines, 0);
        assert!(empty.text.contains("identical"));
        assert!(empty.text.contains("@@ -0,0 +0,0 @@"));
        let blank = compare(snapshot(""), snapshot("\n"));
        assert_eq!((blank.added_lines, blank.removed_lines), (1, 0));
        let removed = compare(snapshot("\n"), snapshot(""));
        assert_eq!((removed.added_lines, removed.removed_lines), (0, 1));
    }

    #[test]
    fn final_newline_and_crlf_differences_are_visible() {
        let report = compare(snapshot("same"), snapshot("same\n"));
        assert_eq!((report.added_lines, report.removed_lines), (1, 1));
        assert!(report
            .text
            .contains("\\ No newline at end of compared text"));
        let crlf = compare(snapshot("line\r\n"), snapshot("line\n"));
        assert_eq!((crlf.added_lines, crlf.removed_lines), (1, 1));
        assert!(crlf.text.contains("-line\\r\n+line\n"));
    }

    #[test]
    fn unicode_is_readable_and_controls_cannot_spoof_diff_markers() {
        let report = compare(
            snapshot("\u{1b}[2J\u{202e}你好\t\u{fff9}\n"),
            snapshot("世界\n"),
        );
        assert!(!report.text.contains('\u{1b}'));
        assert!(!report.text.contains('\u{202e}'));
        assert!(!report.text.contains('\u{fff9}'));
        assert!(report.text.contains("你好\\t"));
        assert!(report.text.contains("+世界"));
        assert!(report.text.contains("\\u{202e}"));
    }

    #[test]
    fn upstream_truncation_does_not_claim_full_equality() {
        let report = compare(
            OutputSnapshot {
                text: "same\n",
                truncated: true,
            },
            snapshot("same\n"),
        );
        assert!(!report.is_complete());
        assert!(report.before_limited);
        assert!(!report.after_limited);
        assert!(report.summary().contains("partial"));
        assert!(!report.text.contains("are identical"));
        assert!(report.text.contains("may differ"));
    }

    #[test]
    fn literal_escape_sequences_remain_distinct_from_controls() {
        let report = compare(snapshot("\t\u{202e}\n"), snapshot("\\t\\u{202e}\n"));
        assert!(report.text.contains("-\\t\\u{202e}\n"));
        assert!(report.text.contains("+\\\\t\\\\u{202e}\n"));
        assert_eq!((report.added_lines, report.removed_lines), (1, 1));
    }

    #[test]
    fn byte_limit_is_utf8_safe_and_limit_flags_are_independent() {
        let long = format!("{}界tail", "a".repeat(MAX_INPUT_BYTES - 1));
        let report = compare(snapshot(""), snapshot(&long));
        assert!(!report.before_limited);
        assert!(report.after_limited);
        assert!(!report.text.contains('界'));
        assert!(!report.text.contains('�'));
        assert!(report.text.len() <= MAX_RENDER_BYTES);
    }

    #[test]
    fn physical_line_limit_cannot_be_evaded_with_tiny_lines() {
        let long = "x\n".repeat(MAX_INPUT_LINES + 1);
        let report = compare(snapshot(&long), snapshot(&long));
        assert_eq!(report.unchanged_lines, MAX_INPUT_LINES);
        assert!(report.before_limited && report.after_limited);
        assert!(!report.alignment_limited);
        assert!(!report.text.contains("are identical"));
    }

    #[test]
    fn large_middle_uses_explicit_bounded_fallback() {
        let before = format!("start\n{}end\n", "old\n".repeat(1024));
        let after = format!("start\n{}end\n", "new\n".repeat(1024));
        let report = compare(snapshot(&before), snapshot(&after));
        assert!(report.alignment_limited);
        assert_eq!((report.added_lines, report.removed_lines), (1024, 1024));
        assert_eq!(report.unchanged_lines, 2);
        assert!(report.text.contains("not a minimal edit distance"));
        assert!(report.text.len() <= MAX_RENDER_BYTES);
    }

    #[test]
    fn common_context_avoids_unnecessary_quadratic_alignment() {
        let before = format!(
            "{}old\n{}",
            "prefix\n".repeat(1800),
            "suffix\n".repeat(1800)
        );
        let after = before.replacen("old\n", "new\n", 1);
        let report = compare(snapshot(&before), snapshot(&after));
        assert!(!report.alignment_limited);
        assert_eq!((report.added_lines, report.removed_lines), (1, 1));
        assert_eq!(report.unchanged_lines, 3600);
    }

    #[test]
    fn expanded_controls_cannot_exceed_render_budget() {
        let hostile = "\u{1b}".repeat(MAX_INPUT_BYTES);
        let report = compare(snapshot(""), snapshot(&hostile));
        assert!(report.rendering_limited);
        assert_eq!(report.added_lines, 1);
        assert!(report.text.ends_with(RENDER_LIMIT_NOTICE));
        assert!(report.text.len() <= MAX_RENDER_BYTES);
        assert!(!report.text.contains('\u{1b}'));
    }

    #[test]
    fn exact_alignment_reconstructs_both_sides_for_small_adversarial_inputs() {
        // Exercise duplicate and reordered lines, not only an example that
        // mirrors the alignment implementation. Removing added lines must
        // recover before; removing deleted lines must recover after.
        let inputs = ["", "a\n", "b\n", "a\na\n", "a\nb\n", "b\na\n", "a\nb\na\n"];
        for before in inputs {
            for after in inputs {
                let before_lines = bounded_lines(snapshot(before));
                let after_lines = bounded_lines(snapshot(after));
                let (edits, limited) = align(&before_lines.lines, &after_lines.lines);
                assert!(!limited);
                let recovered_before: String = edits
                    .iter()
                    .filter(|(kind, _)| !matches!(kind, Kind::Added))
                    .map(|(_, line)| *line)
                    .collect();
                let recovered_after: String = edits
                    .iter()
                    .filter(|(kind, _)| !matches!(kind, Kind::Removed))
                    .map(|(_, line)| *line)
                    .collect();
                assert_eq!(recovered_before, before);
                assert_eq!(recovered_after, after);
            }
        }
    }
}
