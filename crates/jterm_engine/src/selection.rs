//! Inclusive scalar-cell selection copying for the existing native profiles.
//!
//! Hosts supply their authoritative history/grid rows. This keeps their row
//! decoding, projection admission and link replacement outside the byte rules.
use crate::TerminalCell;
use std::ops::Range;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SoftWrapSpaces {
    Preserve,
    /// Trim only ASCII spaces at the end of intermediate soft-wrapped rows
    /// in a linear selection. Rectangles retain every selected space.
    Trim,
}

/// Consume the selected physical rows in order. Endpoints remain inclusive,
/// even when retained rows have a different width from the current grid.
/// These legacy profiles copy scalar cells and skip wide continuations; rich
/// cluster and synthetic-margin selection contracts stay on their own APIs.
pub struct SelectionCopy {
    first: (usize, usize),
    last: (usize, usize),
    row_end: usize,
    columns: usize,
    rectangular: bool,
    spaces: SoftWrapSpaces,
    text: String,
}

impl SelectionCopy {
    pub fn new(
        anchor: (usize, usize),
        active: (usize, usize),
        columns: usize,
        total_rows: usize,
        rectangular: bool,
        spaces: SoftWrapSpaces,
    ) -> Self {
        let (first, last) = if anchor <= active {
            (anchor, active)
        } else {
            (active, anchor)
        };
        Self {
            first,
            last,
            row_end: last.0.saturating_add(1).min(total_rows),
            columns,
            rectangular,
            spaces,
            text: String::new(),
        }
    }

    pub fn rows(&self) -> Range<usize> {
        self.first.0..self.row_end
    }

    fn columns_at(&self, row: usize) -> (usize, usize) {
        let right_edge = self.columns.saturating_sub(1);
        if self.rectangular {
            (
                self.first.1.min(self.last.1),
                self.first.1.max(self.last.1).min(right_edge),
            )
        } else {
            (
                if row == self.first.0 { self.first.1 } else { 0 },
                if row == self.last.0 {
                    self.last.1.min(right_edge)
                } else {
                    right_edge
                },
            )
        }
    }

    /// Append one selected row from the host, without acquiring or mutating
    /// its storage. Short/empty history rows retain their original clipping.
    pub fn push_row(&mut self, row: usize, cells: &[TerminalCell], wrapped: bool) {
        let (start, end) = self.columns_at(row);
        let text_start = self.text.len();
        self.text.extend(
            cells
                .iter()
                .take(end.saturating_add(1))
                .skip(start)
                .filter(|cell| !cell.flags.wide_continuation())
                .map(|cell| cell.character),
        );
        if self.spaces == SoftWrapSpaces::Trim
            && !self.rectangular
            && wrapped
            && row < self.row_end.saturating_sub(1)
        {
            let retained = self.text[text_start..].trim_end_matches(' ').len();
            self.text.truncate(text_start + retained);
        }
        if row < self.row_end.saturating_sub(1) && (self.rectangular || !wrapped) {
            self.text.push('\n');
        }
    }

    pub fn finish(self) -> String {
        self.text
    }
}

fn is_extended_token_separator(c: char) -> bool {
    matches!(
        c,
        '/' | '\\' | '.' | ':' | '-' | '~' | '?' | '&' | '=' | '#' | '%' | '+' | '@'
    )
}

/// Character admission for native URL/path token selection, including the
/// adapters that gather a logical token across soft-wrapped rows.
fn is_extended_token_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || is_extended_token_separator(c)
}

fn is_token_prefix_wrapper(c: char) -> bool {
    matches!(c, '"' | '\'' | '`' | '(' | '[' | '{' | '<')
}

fn is_token_suffix_wrapper(c: char) -> bool {
    matches!(
        c,
        '"' | '\'' | '`' | ')' | ']' | '}' | '>' | ',' | ';' | '!'
    )
}

/// Inclusive physical-cell span for a URL/path-like token. The caller keeps
/// its own row gathering and selection assignment. Wide continuations remain
/// part of the returned span but contribute no token characters.
pub fn extended_token_span(line: &[TerminalCell], start_col: usize) -> Option<(usize, usize)> {
    let cols = line.len();
    if start_col >= cols {
        return None;
    }

    let start_char = line[start_col].character;
    if !is_extended_token_char(start_char) {
        return None;
    }

    let mut left = start_col;
    while left > 0 {
        let prev = left - 1;
        if line[prev].flags.wide_continuation() {
            left = prev;
            continue;
        }
        if !is_extended_token_char(line[prev].character) {
            break;
        }
        left = prev;
    }

    let mut right = start_col;
    loop {
        let next = if line[right].flags.wide() {
            right + 2
        } else {
            right + 1
        };
        if next >= cols {
            break;
        }
        if line[next].flags.wide_continuation() {
            if next + 1 < cols && is_extended_token_char(line[next + 1].character) {
                right = next + 1;
                continue;
            }
            break;
        }
        if !is_extended_token_char(line[next].character) {
            break;
        }
        right = next;
    }

    while left < start_col && is_token_prefix_wrapper(line[left].character) {
        left += 1;
    }

    while right > start_col && is_token_suffix_wrapper(line[right].character) {
        right -= if line[right].flags.wide_continuation() && right > 0 {
            2
        } else {
            1
        };
    }

    if left > right || start_col < left || start_col > right {
        return None;
    }

    let mut has_alnum = false;
    let mut has_separator = false;
    for cell in &line[left..=right] {
        if cell.flags.wide_continuation() {
            continue;
        }
        let ch = cell.character;
        has_alnum |= ch.is_alphanumeric();
        has_separator |= is_extended_token_separator(ch);
    }

    if !has_alnum || !has_separator {
        return None;
    }

    if line[right].flags.wide() && right + 1 < cols {
        right += 1;
    }

    Some((left, right))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn cells(text: &str) -> Vec<TerminalCell> {
        text.chars()
            .map(|character| TerminalCell {
                character,
                ..TerminalCell::default()
            })
            .collect()
    }
    #[test]
    fn soft_spaces_are_profiled_and_hard_breaks_remain_literal() {
        for (spaces, expected) in [
            (SoftWrapSpaces::Preserve, "ab  cd  \nEFGH"),
            (SoftWrapSpaces::Trim, "abcd  \nEFGH"),
        ] {
            let mut copy = SelectionCopy::new((0, 0), (2, 3), 4, 3, false, spaces);
            copy.push_row(0, &cells("ab  "), true);
            copy.push_row(1, &cells("cd  "), false);
            copy.push_row(2, &cells("EFGH"), false);
            assert_eq!(copy.finish(), expected);
        }
    }
    #[test]
    fn reversed_rectangles_keep_spaces_and_separate_wrapped_rows() {
        let mut copy = SelectionCopy::new((2, 2), (0, 1), 4, 3, true, SoftWrapSpaces::Trim);
        copy.push_row(0, &cells("ab  "), true);
        copy.push_row(1, &cells("cd  "), false);
        copy.push_row(2, &cells("EFGH"), false);
        assert_eq!(copy.finish(), "b \nd \nFG");
    }
    #[test]
    fn inclusive_ranges_clip_old_widths_without_selecting_continuation_text() {
        let mut row = cells("a界 z");
        row[1].flags.set_wide(true);
        row[2].flags.set_wide_continuation(true);
        let mut copy = SelectionCopy::new((0, 1), (0, 2), 4, 1, false, SoftWrapSpaces::Preserve);
        copy.push_row(0, &row, true);
        assert_eq!(copy.finish(), "界");
        let mut copy = SelectionCopy::new((0, 2), (0, 2), 4, 1, false, SoftWrapSpaces::Preserve);
        copy.push_row(0, &row, true);
        assert_eq!(copy.finish(), "");
        let mut copy = SelectionCopy::new((0, 0), (9, 3), 4, 2, false, SoftWrapSpaces::Preserve);
        assert_eq!(copy.rows(), 0..2);
        copy.push_row(0, &cells("abcdef"), true);
        copy.push_row(1, &[], false);
        assert_eq!(copy.finish(), "abcd");
        let empty = SelectionCopy::new((0, 0), (9, 3), 4, 0, false, SoftWrapSpaces::Preserve);
        assert!(empty.rows().is_empty());
        assert_eq!(empty.finish(), "");
        assert!(
            SelectionCopy::new((9, 0), (10, 0), 4, 2, false, SoftWrapSpaces::Trim)
                .rows()
                .is_empty()
        );
    }
}

#[cfg(test)]
mod token_tests {
    use super::*;

    fn cells(text: &str) -> Vec<TerminalCell> {
        text.chars()
            .map(|character| TerminalCell {
                character,
                ..TerminalCell::default()
            })
            .collect()
    }

    #[test]
    fn token_span_keeps_native_url_path_and_punctuation_rules() {
        for text in [
            "https://example.test/path",
            "/tmp/file.rs:42:7",
            r"C:\Users\name\file.txt",
            "目录/文件.rs",
        ] {
            let row = cells(text);
            for cursor in 0..row.len() {
                assert_eq!(
                    extended_token_span(&row, cursor),
                    Some((0, row.len() - 1)),
                    "{text:?} at {cursor}"
                );
            }
        }
        let row = cells("(foo/bar),");
        assert_eq!(extended_token_span(&row, 2), Some((1, 7)));
        for text in ["plain_word", "/", "--", "_."] {
            assert_eq!(extended_token_span(&cells(text), 0), None);
        }
        assert_eq!(extended_token_span(&[], 0), None);
    }

    #[test]
    fn token_span_uses_physical_wide_cells_and_rejects_outside_cursor() {
        let mut row = cells("界 /x");
        row[0].flags.set_wide(true);
        row[1].flags.set_wide_continuation(true);
        assert_eq!(extended_token_span(&row, 0), Some((0, 3)));
        assert_eq!(extended_token_span(&row, 1), None);
        assert_eq!(extended_token_span(&row, row.len()), None);
        assert_eq!(extended_token_span(&row, usize::MAX), None);
    }
}
