//! Hit testing on the exact logical text of a read-only terminal document.
use super::{detect_delimited_references, plain_url_at, DelimitedReference};
use std::ops::Range;

/// Find an unambiguous HTTP(S) target at a byte in a document.
pub fn document_url_at(text: &str, byte: usize) -> Option<Range<usize>> {
    let line = Line::at(text, byte)?;
    if line.delimited(text).is_some() {
        return None;
    }
    let range = plain_url_at(line.text, line.offset)?;
    Some(line.start + range.start..line.start + range.end)
}

/// Find the exact path candidate under a click. The caller applies its path
/// policy and asks the user to confirm the local read.
pub fn document_local_reference_at(text: &str, byte: usize) -> Option<String> {
    let line = Line::at(text, byte)?;
    if let Some(reference) = line.delimited(text) {
        return (!reference.blocked && !reference.web).then_some(reference.target);
    }
    if plain_url_at(line.text, line.offset).is_some() {
        return None;
    }
    let delimiter = |ch: char| ch.is_whitespace() || "<>\"'`()[]{}".contains(ch);
    let (before, after) = line.text.split_at(line.offset);
    if after.starts_with(delimiter) {
        return None;
    }
    let start = before
        .char_indices()
        .rfind(|&(_, ch)| delimiter(ch))
        .map_or(0, |(i, ch)| i + ch.len_utf8());
    let end = after
        .find(delimiter)
        .map_or(line.text.len(), |i| line.offset + i);
    Some(line.text[start..end].to_owned())
}

struct Line<'a> {
    text: &'a str,
    start: usize,
    offset: usize,
}

impl<'a> Line<'a> {
    fn at(document: &'a str, byte: usize) -> Option<Self> {
        if byte == document.len() || !document.is_char_boundary(byte) {
            return None;
        }
        let start = document[..byte].rfind('\n').map_or(0, |i| i + 1);
        let end = document[byte..]
            .find('\n')
            .map_or(document.len(), |i| byte + i);
        (byte < end).then_some(Self {
            text: &document[start..end],
            start,
            offset: byte - start,
        })
    }

    fn delimited(&self, document: &str) -> Option<DelimitedReference> {
        // The delimited parser accepts at most eight logical lines.
        let start = document[..self.start]
            .rmatch_indices('\n')
            .nth(7)
            .map_or(0, |(i, _)| i + 1);
        let line_end = self.start + self.text.len();
        let end = document[line_end..]
            .match_indices('\n')
            .nth(7)
            .map_or(document.len(), |(i, _)| line_end + i);
        let row = document[start..self.start].matches('\n').count();
        let lines: Vec<_> = document[start..end].split('\n').collect();
        detect_delimited_references(&lines)
            .into_iter()
            .find(|reference| {
                reference
                    .spans
                    .iter()
                    .any(|(line, span)| *line == row && span.contains(&self.offset))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn document_reference_keeps_literal_names_and_web_targets_separate() {
        let doc =
            "guide （not a reference） (src/中 文.rs:12) ./file.md\nhttps://example.test/guide?";
        let spaced = doc.find('中').unwrap();
        assert_eq!(
            document_local_reference_at(doc, spaced).as_deref(),
            Some("src/中 文.rs:12")
        );
        let literal = doc.find("file.md").unwrap();
        assert_eq!(
            document_local_reference_at(doc, literal).as_deref(),
            Some("./file.md")
        );
        let web = doc.find("example.test").unwrap();
        assert_none_local_and_exact_web(doc, web);
        let untrusted = "($HOME/src/file.md)";
        assert_eq!(
            document_local_reference_at(untrusted, 12).as_deref(),
            Some("$HOME/src/file.md")
        );
    }

    fn assert_none_local_and_exact_web(doc: &str, offset: usize) {
        assert!(document_local_reference_at(doc, offset).is_none());
        assert_eq!(
            &doc[document_url_at(doc, offset).unwrap()],
            "https://example.test/guide"
        );
    }

    #[test]
    fn ambiguous_hard_lines_do_not_fall_back_to_fragments() {
        for (doc, tokens) in [
            (
                "(https://trusted.example/\nother/path.md)",
                ["trusted", "path.md"],
            ),
            ("(/tmp/first-\nsecond.md)", ["first", "second"]),
            ("(https://user@example.test/path)", ["https", "path"]),
        ] {
            for token in tokens {
                let byte = doc.find(token).unwrap();
                assert!(document_url_at(doc, byte).is_none(), "{doc:?}");
                assert!(document_local_reference_at(doc, byte).is_none(), "{doc:?}");
            }
        }
    }
}
