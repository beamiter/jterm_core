//! Recover the parenthesized destinations that a TUI may wrap before printing.
use std::ops::Range;
use std::sync::LazyLock;

const MAX_BYTES: usize = 4096;

pub struct DelimitedReference {
    pub target: String,
    pub web: bool,
    /// Reserve the visible region without authorizing a guessed target.
    pub blocked: bool,
    pub spans: Vec<(usize, Range<usize>)>,
}

pub fn detect_delimited_references(rows: &[&str]) -> Vec<DelimitedReference> {
    static DESTINATION: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(
            r"\(((?:(?i:https?)://|(?:\.{0,2}|~)/|[\p{L}\p{N}_][\p{L}\p{N}\p{M}_.-]*/)[^()\r\n]*(?:\n[^()\r\n]+){0,7})\)",
        )
        .unwrap()
    });
    static FILE_END: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(
            r"(?i-u:\.(?:canvas\.tsx|tsx|jsx|mdx?|txt|rs|py|jsonc?|toml|ya?ml|css|html|svg|png|jpe?g|webp|ts|js|pdf|csv|sh))(?::[0-9]+){0,2}$",
        )
        .unwrap()
    });

    if !rows.iter().any(|text| text.contains('(')) {
        return Vec::new();
    }
    let mut text = String::new();
    let mut row_starts = Vec::with_capacity(rows.len());
    for row in rows {
        row_starts.push(text.len());
        text.push_str(row);
        text.push('\n');
    }

    let mut references = Vec::new();
    for matched in DESTINATION.captures_iter(&text) {
        let raw = matched.get(1).unwrap();
        let oversized = raw.len() > MAX_BYTES;
        let first_row = row_starts.partition_point(|&start| start <= raw.start()) - 1;
        let is_web = raw.as_str().split_once("://").is_some_and(|(scheme, _)| {
            scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")
        });
        // Row width cannot distinguish a hard newline from a soft wrap.
        // Retain exact text and reserve ambiguous regions without inventing a
        // different filesystem path, host name, or URL path by trimming.
        let spans = raw
            .as_str()
            .split('\n')
            .enumerate()
            .map(|(index, fragment)| {
                let row = first_row + index;
                let start = if index == 0 {
                    raw.start() - row_starts[row]
                } else {
                    0
                };
                (row, start..start + fragment.len())
            })
            .collect::<Vec<_>>();
        let target = if oversized { "" } else { raw.as_str() };
        let blocked = oversized
            || spans.len() > 1
            || if is_web {
                !super::is_openable_url(target)
            } else {
                target.chars().any(char::is_control)
                    || target.contains("://")
                    || target.contains(['\\', '<', '>', '|'])
                    || !FILE_END.is_match(target)
            };
        // Valid single-row URLs/tokens keep their native matchers. Spaced local
        // names and refused delimited regions need our exact span handling.
        if blocked || (!is_web && target.contains(char::is_whitespace)) {
            references.push(DelimitedReference {
                target: target.to_owned(),
                web: is_web,
                blocked,
                spans,
            });
        }
    }
    references
}
