use jterm_engine::{
    frame::{Error, Limits, Screen},
    terminal::Terminal,
    Color,
};
use std::sync::Arc;

fn terminal(rows: usize, cols: usize) -> Terminal {
    let mut terminal = Terminal::new(rows, cols).unwrap();
    terminal.enable_grapheme_clusters();
    terminal
}

#[test]
fn fragmented_combining_marks_and_emoji_join_their_physical_heads() {
    let mut enabled = terminal(2, 12);
    let mut legacy = Terminal::new(2, 12).unwrap();
    let text = "e\u{301}\u{327}👩🏽\u{200d}💻Z";
    for byte in text.as_bytes() {
        assert!(enabled.feed(&[*byte]).unwrap().is_empty());
        assert!(legacy.feed(&[*byte]).unwrap().is_empty());
    }
    let frame = enabled.frame().unwrap();
    assert_eq!(frame.cluster_text(0, 0), Some("e\u{301}\u{327}"));
    assert_eq!(frame.cluster_text(0, 1), Some("👩🏽\u{200d}💻"));
    assert_eq!(frame.cluster_text(0, 2), None);
    assert_eq!(frame.cluster_text(0, 3), None);
    assert!(frame.rows()[0].cells[1].flags.wide());
    assert!(frame.rows()[0].cells[2].flags.wide_continuation());
    assert_eq!(frame.rows()[0].cells[3].character, 'Z');
    assert_eq!(frame.cursor().col, 4);
    assert!(!frame.cursor().pending_wrap);
    assert_eq!(enabled.unsupported_count(), 0);
    assert!(enabled
        .visible_text()
        .starts_with("e\u{301}\u{327}👩🏽\u{200d}💻 Z"));
    assert!(legacy.frame().unwrap().cluster_text(0, 0).is_none());
    assert!(legacy.unsupported_count() > 0);
}

#[test]
fn wide_emoji_sequences_use_pairs_and_keep_deferred_wrap() {
    let mut terminal = terminal(2, 6);
    terminal.feed("❤️#️⃣🇺🇸".as_bytes()).unwrap();
    let frame = terminal.frame().unwrap();
    assert_eq!(frame.cluster_text(0, 0), Some("❤️"));
    assert_eq!(frame.cluster_text(0, 2), Some("#️⃣"));
    assert_eq!(frame.cluster_text(0, 4), Some("🇺🇸"));
    assert_eq!(frame.cursor().col, 4);
    assert!(frame.cursor().pending_wrap);
    for pair in frame.rows()[0].cells.chunks_exact(2) {
        assert!(pair[0].flags.wide());
        assert!(pair[1].flags.wide_continuation());
    }
    terminal.feed(b"Z").unwrap();
    let frame = terminal.frame().unwrap();
    assert_eq!(frame.rows()[1].cells[0].character, 'Z');
    assert!(frame.rows()[0].wrapped);
}

#[test]
fn text_only_changes_preserve_the_base_row_and_publish_through_sync_delta() {
    let mut terminal = terminal(2, 8);
    terminal.enable_underline_colors();
    terminal.enable_hyperlinks(|_, uri| uri.starts_with("https://"));
    terminal.begin_update(true);
    terminal
        .feed(b"\x1b[?2026h\x1b[4;58;5;2m\x1b]8;;https://example.test/a\x07A")
        .unwrap();
    let before = terminal.frame().unwrap();
    terminal.feed("\u{301}\x1b[?2026l".as_bytes()).unwrap();
    let completed = terminal.take_completed_sync_frame().unwrap();
    assert!(Arc::ptr_eq(
        &before.rows()[0].cells,
        &completed.rows()[0].cells
    ));
    assert_eq!(before.cluster_text(0, 0), None);
    assert_eq!(completed.cluster_text(0, 0), Some("A\u{301}"));
    assert_eq!(completed.underline_color(0, 0), Some(Color::Indexed(2)));
    let link = completed.rows()[0].cells[0].hyperlink_id;
    assert_eq!(
        completed.hyperlink_target(link),
        Some("https://example.test/a")
    );

    let limits = Limits {
        max_rows: 2,
        max_cols: 8,
        max_cells: 16,
    };
    let delta = before.delta_to(&completed).unwrap();
    assert_eq!(delta.rows.len(), 1);
    assert_eq!(before.apply(&delta, limits).unwrap(), completed);
    let mut corrupt = delta;
    let mut mismatched = corrupt.rows[0].1.cells.to_vec();
    mismatched[0].character = 'B';
    corrupt.rows[0].1.cells = mismatched.into();
    assert_eq!(before.apply(&corrupt, limits), Err(Error::Bounds));

    terminal.feed(b"\rA").unwrap();
    let after = terminal.frame().unwrap();
    assert_eq!(after.cluster_text(0, 0), None);
    let delta = completed.delta_to(&after).unwrap();
    assert_eq!(completed.apply(&delta, limits).unwrap(), after);
    assert_eq!(completed.cluster_text(0, 0), Some("A\u{301}"));
}

#[test]
fn controls_end_continuation_and_rep_repeats_the_complete_graphic() {
    let mut terminal = terminal(3, 8);
    terminal.feed("a\u{301}\x1b[2b".as_bytes()).unwrap();
    let first = terminal.frame().unwrap();
    for col in 0..3 {
        assert_eq!(first.cluster_text(0, col), Some("a\u{301}"));
    }
    terminal.feed("\x1b[D\u{301}".as_bytes()).unwrap();
    let second = terminal.frame().unwrap();
    assert_eq!(second.cluster_text(0, 2), Some("a\u{301}"));
    assert_eq!(second.cursor().col, 2);
    terminal.feed(b"b").unwrap();
    terminal.resize(3, 9).unwrap();
    terminal.feed("\u{301}".as_bytes()).unwrap();
    assert_eq!(terminal.frame().unwrap().cluster_text(0, 2), None);
    assert_eq!(terminal.unsupported_count(), 2);
}

#[test]
fn horizontal_edits_and_resize_move_or_erase_the_matching_cluster() {
    let mut terminal = terminal(4, 8);
    terminal
        .feed("A\u{301}界\u{301}B\u{300}".as_bytes())
        .unwrap();
    terminal.feed(b"\x1b[1;2H\x1b[@").unwrap();
    let shifted = terminal.frame().unwrap();
    assert_eq!(shifted.cluster_text(0, 0), Some("A\u{301}"));
    assert_eq!(shifted.cluster_text(0, 2), Some("界\u{301}"));
    assert_eq!(shifted.cluster_text(0, 3), None);
    assert_eq!(shifted.cluster_text(0, 4), Some("B\u{300}"));
    terminal.feed(b"\x1b[1;4H\x1b[P").unwrap();
    let repaired = terminal.frame().unwrap();
    assert_eq!(repaired.rows()[0].cells[2].character, ' ');
    assert_eq!(repaired.cluster_text(0, 2), None);
    assert_eq!(repaired.cluster_text(0, 3), Some("B\u{300}"));
    terminal
        .feed("\x1b[4h\x1b[H界\u{301}\x1b[4l".as_bytes())
        .unwrap();
    let inserted = terminal.frame().unwrap();
    assert_eq!(inserted.cluster_text(0, 0), Some("界\u{301}"));
    assert_eq!(inserted.cluster_text(0, 2), Some("A\u{301}"));
    assert_eq!(inserted.cluster_text(0, 5), Some("B\u{300}"));
    terminal.resize(4, 1).unwrap();
    assert_eq!(terminal.frame().unwrap().cluster_text(0, 0), None);
    terminal.resize(4, 8).unwrap();
    assert_eq!(terminal.frame().unwrap().cluster_text(0, 2), None);
    assert_eq!(inserted.cluster_text(0, 0), Some("界\u{301}"));
}

#[test]
fn history_keeps_soft_wrap_physical_selection_and_archived_clusters() {
    let mut terminal = terminal(2, 4);
    terminal
        .feed("A\u{301}BC界\u{301}D\r\nE\r\nF".as_bytes())
        .unwrap();
    let before = terminal.history_page(0, 2).unwrap();
    assert_eq!(before.text, "A\u{301}BC界\u{301}D");
    assert_eq!(before.cluster_text(0, 0), Some("A\u{301}"));
    assert_eq!(before.cluster_text(1, 0), Some("界\u{301}"));
    assert_eq!(before.cluster_text(1, 1), None);
    assert_eq!(before.selection_text(0, 1, 1, 2).unwrap(), "BC界\u{301}");
    assert_eq!(terminal.history_extent(), 0..2);
    terminal.feed("\x1b[?1049hA\u{308}".as_bytes()).unwrap();
    let alternate = terminal.frame().unwrap();
    assert_eq!(alternate.stamp().screen, Screen::Alternate);
    assert_eq!(alternate.cluster_text(0, 0), Some("A\u{308}"));
    assert_eq!(terminal.history_extent(), 2..2);
    terminal.resize(2, 6).unwrap();
    terminal.feed(b"\x1b[?1049l").unwrap();
    assert_eq!(terminal.history_page(0, 2).unwrap().text, before.text);
    terminal.feed(b"\x1b[3J").unwrap();
    assert!(terminal.history_page(0, 2).unwrap().text.is_empty());
    assert_eq!(before.cluster_text(0, 0), Some("A\u{301}"));
}

#[test]
fn individual_and_row_text_budgets_keep_the_accepted_prefix() {
    let mut terminal = terminal(2, 64);
    let oversized = format!("e{}", "\u{301}".repeat(40));
    let bounded = format!("e{}", "\u{301}".repeat(31));
    terminal.feed(oversized.as_bytes()).unwrap();
    assert_eq!(
        terminal.frame().unwrap().cluster_text(0, 0),
        Some(bounded.as_str())
    );
    assert_eq!(terminal.unsupported_count(), 9);
    terminal.feed(b"\r\x1b[2K").unwrap();
    terminal.feed(bounded.repeat(52).as_bytes()).unwrap();
    let frame = terminal.frame().unwrap();
    let remainder = format!("e{}", "\u{301}".repeat(23));
    assert_eq!(frame.cluster_text(0, 47), Some(bounded.as_str()));
    assert_eq!(frame.cluster_text(0, 48), Some(remainder.as_str()));
    assert_eq!(frame.cluster_text(0, 49), None);
    assert_eq!(frame.cursor().col, 52);
}
