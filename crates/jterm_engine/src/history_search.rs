//! Bounded literal search over the retained primary-screen archive.
use std::{ops::Range, sync::Arc};

use crate::terminal::Terminal;

const SEARCH_ROWS: usize = 64;
const MAX_QUERY_BYTES: usize = 128;

pub fn validate_query(query: &str) -> Result<(), String> {
    if query.is_empty() || query.len() > MAX_QUERY_BYTES || query.chars().any(char::is_control) {
        return Err(
            "Enter a literal history query of 1–128 UTF-8 bytes without control characters.".into(),
        );
    }
    Ok(())
}

#[derive(Debug)]
pub enum HistorySearchProgress {
    Match(HistoryMatch),
    More,
    End,
}

#[derive(Debug)]
pub struct HistoryMatch {
    pub revision: u64,
    pub offset: usize,
    pub total_rows: usize,
    /// Raw rows in this page. A cross-window match can also include a prefix
    /// of the following page without changing which rows this page owns.
    pub row_count: usize,
    pub text: Arc<str>,
    /// UTF-8 byte boundaries in `text`.
    pub range: Range<usize>,
}

struct Window {
    text: Arc<str>,
    owned_bytes: usize,
    before: usize,
    offset: usize,
    total_rows: usize,
    row_count: usize,
}

struct SearchSource {
    extent: Range<u64>,
    revision: u64,
}

fn char_boundary_before(text: &str, mut limit: usize) -> usize {
    while !text.is_char_boundary(limit) {
        limit -= 1;
    }
    limit
}

/// A newest-to-oldest cursor belonging to one terminal session. Output may
/// append and the grid may resize between steps. Losing a source row invalidates
/// the search; matches always refer to the archive that the first step saw.
pub struct HistorySearch {
    query: String,
    max_window_bytes: usize,
    source: Option<SearchSource>,
    offset: usize,
    // A match crossing a window boundary must start in the current window.
    newer_prefix: String,
    window: Option<Window>,
}

impl HistorySearch {
    /// The byte limit includes the searched rows and any cross-window prefix.
    /// Search can shrink a window to one complete row to fit the limit.
    pub fn new(query: String, max_window_bytes: usize) -> Result<Self, String> {
        validate_query(&query)?;
        if max_window_bytes < query.len() {
            return Err("history search window is smaller than the query".into());
        }
        Ok(Self {
            query,
            max_window_bytes,
            source: None,
            offset: 0,
            newer_prefix: String::new(),
            window: None,
        })
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn step(&mut self, terminal: &Terminal) -> Result<HistorySearchProgress, String> {
        let extent = terminal.history_extent();
        let newer_rows = match &self.source {
            None => 0,
            Some(source)
                if extent.start == source.extent.start && extent.end >= source.extent.end =>
            {
                (extent.end - source.extent.end) as usize
            }
            Some(_) => return Err("History changed. Start a new search.".into()),
        };
        if self.window.is_none() {
            let total_rows = (extent.end - extent.start) as usize - newer_rows;
            let end = total_rows.saturating_sub(self.offset);
            let max_bytes = self.max_window_bytes - self.newer_prefix.len();
            let mut limit = end
                .clamp(1, SEARCH_ROWS)
                .next_power_of_two()
                .min(SEARCH_ROWS);
            let (revision, row_count, mut text) = loop {
                let start = end.saturating_sub(limit);
                if let Some((revision, text)) =
                    terminal.history_search_text(start..end, self.offset > 0, max_bytes)
                {
                    break (revision, end - start, text);
                }
                if limit == 1 {
                    return Err("history search snapshot exceeds its byte limit".into());
                }
                limit /= 2;
            };
            self.source.get_or_insert(SearchSource { extent, revision });
            if row_count == 0 {
                return Ok(HistorySearchProgress::End);
            }
            let owned_bytes = text.len();
            text.push_str(&self.newer_prefix);
            let prefix = char_boundary_before(&text, text.len().min(self.query.len() - 1));
            self.newer_prefix.clear();
            self.newer_prefix.push_str(&text[..prefix]);
            self.window = Some(Window {
                before: text.len(),
                text: text.into(),
                owned_bytes,
                offset: self.offset,
                total_rows,
                row_count,
            });
            self.offset += row_count;
        }

        let window = self.window.as_mut().expect("loaded history window");
        // Limit by start so overlapping occurrences can be visited in order.
        let limit = char_boundary_before(
            &window.text,
            (window.before + self.query.len() - 1).min(window.text.len()),
        );
        if let Some((start, _)) = window.text[..limit]
            .rmatch_indices(&self.query)
            .find(|(start, _)| *start < window.before.min(window.owned_bytes))
        {
            window.before = start;
            return Ok(HistorySearchProgress::Match(HistoryMatch {
                revision: self
                    .source
                    .as_ref()
                    .expect("loaded history source")
                    .revision,
                offset: window.offset + newer_rows,
                total_rows: window.total_rows + newer_rows,
                row_count: window.row_count,
                text: Arc::clone(&window.text),
                range: start..start + self.query.len(),
            }));
        }
        let remaining = self.offset < window.total_rows;
        self.window = None;
        Ok(if remaining {
            HistorySearchProgress::More
        } else {
            HistorySearchProgress::End
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_windows_find_a_soft_wrap_match_after_new_output() {
        let mut terminal = Terminal::new(2, 4).unwrap();
        terminal.feed(b"ABCDXY12\x1b[2S").unwrap();
        let mut search = HistorySearch::new("CDXY".into(), 7).unwrap();
        assert!(matches!(
            search.step(&terminal).unwrap(),
            HistorySearchProgress::More
        ));
        terminal.feed(b"\rnew\x1b[S").unwrap();
        let HistorySearchProgress::Match(found) = search.step(&terminal).unwrap() else {
            panic!("a match crossing the one-row window was lost");
        };
        assert_eq!(found.offset, 2);
        assert_eq!(found.total_rows, 3);
        assert_eq!(found.row_count, 1);
        assert_eq!(&*found.text, "ABCDXY1");
        assert_eq!(&found.text[found.range], "CDXY");
        assert!(matches!(
            search.step(&terminal).unwrap(),
            HistorySearchProgress::End
        ));
    }

    #[test]
    fn small_windows_never_drop_a_hard_break_or_truncate_a_row() {
        let mut terminal = Terminal::new(2, 4).unwrap();
        terminal.feed(b"ABCD\r\nXY12\x1b[2S").unwrap();
        let mut search = HistorySearch::new("CDXY".into(), 8).unwrap();
        assert!(matches!(
            search.step(&terminal).unwrap(),
            HistorySearchProgress::More
        ));
        assert!(matches!(
            search.step(&terminal).unwrap(),
            HistorySearchProgress::End
        ));
        let mut search = HistorySearch::new("XY1".into(), 3).unwrap();
        assert_eq!(
            search.step(&terminal).unwrap_err(),
            "history search snapshot exceeds its byte limit"
        );
    }

    #[test]
    fn empty_search_stays_at_its_original_archive_boundary() {
        let mut terminal = Terminal::new(2, 6).unwrap();
        let mut search = HistorySearch::new("needle".into(), 32).unwrap();
        assert!(matches!(
            search.step(&terminal).unwrap(),
            HistorySearchProgress::End
        ));
        terminal.feed(b"needle\x1b[S").unwrap();
        assert_eq!(terminal.history_rows(), 1);
        assert!(matches!(
            search.step(&terminal).unwrap(),
            HistorySearchProgress::End
        ));
    }
}
