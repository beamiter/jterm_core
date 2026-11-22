use jterm_engine::{
    frame::{CellFrame, Limits},
    terminal::Terminal,
    Color,
};

fn terminal(rows: usize, cols: usize) -> Terminal {
    let mut terminal = Terminal::new(rows, cols).unwrap();
    terminal.enable_grapheme_clusters();
    terminal
}

fn row(frame: &CellFrame, row: usize) -> String {
    frame.rows()[row]
        .cells
        .iter()
        .map(|cell| cell.character)
        .collect()
}

fn selector(terminal: &mut Terminal) {
    for byte in "\u{fe0e}".as_bytes() {
        terminal.feed(&[*byte]).unwrap();
    }
}

#[test]
fn a_contracted_wide_overwrite_restores_original_style_and_cluster() {
    let mut terminal = terminal(2, 6);
    terminal.enable_underline_colors();
    terminal.enable_hyperlinks(|_, uri| uri.starts_with("https://example.test/"));
    terminal
        .feed("\x1b[4;58;5;11m\x1b]8;;https://example.test/original\x07ABCD\u{301}EF".as_bytes())
        .unwrap();
    terminal
        .feed("\x1b[1;3H\x1b[58;5;3m\x1b]8;;https://example.test/watch\x07⌚".as_bytes())
        .unwrap();
    let provisional = terminal.frame().unwrap();
    assert_eq!(row(&provisional, 0), "AB⌚ EF");
    assert_eq!(provisional.cluster_text(0, 3), None);
    selector(&mut terminal);
    let frame = terminal.frame().unwrap();
    assert_eq!(frame.cluster_text(0, 2), Some("⌚\u{fe0e}"));
    assert!(!frame.rows()[0].cells[2].flags.wide());
    assert_eq!(frame.cluster_text(0, 3), Some("D\u{301}"));
    assert_eq!(frame.underline_color(0, 2), Some(Color::Indexed(3)));
    assert_eq!(frame.underline_color(0, 3), Some(Color::Indexed(11)));
    let link = frame.rows()[0].cells[3].hyperlink_id;
    assert_eq!(
        frame.hyperlink_target(link),
        Some("https://example.test/original")
    );
    assert_eq!(frame.cursor().paint_position(), (0, 3));
}

#[test]
fn late_contraction_clears_pending_wrap_and_rep_uses_the_accepted_width() {
    let mut terminal = terminal(2, 4);
    terminal.feed("ab⌚".as_bytes()).unwrap();
    assert!(terminal.frame().unwrap().cursor().pending_wrap);
    selector(&mut terminal);
    let narrow = terminal.frame().unwrap();
    assert!(!narrow.cursor().pending_wrap);
    assert_eq!(narrow.cursor().paint_position(), (0, 3));
    terminal.feed(b"\x1b[b").unwrap();
    let repeated = terminal.frame().unwrap();
    assert_eq!(row(&repeated, 0), "ab⌚⌚");
    assert_eq!(repeated.cluster_text(0, 3), Some("⌚\u{fe0e}"));
    assert!(!repeated.rows()[0].cells[3].flags.wide());
    assert!(repeated.cursor().pending_wrap);
}

#[test]
fn insert_and_nowrap_restore_the_columns_the_unfinalized_width_displaced() {
    let mut insert = terminal(1, 8);
    insert
        .feed("abcdefgh\x1b[1;3H\x1b[4h⌚".as_bytes())
        .unwrap();
    assert_eq!(row(&insert.frame().unwrap(), 0), "ab⌚ cdef");
    selector(&mut insert);
    assert_eq!(row(&insert.frame().unwrap(), 0), "ab⌚cdefg");

    let mut nowrap = terminal(1, 4);
    nowrap.feed("\x1b[?7labc⌚".as_bytes()).unwrap();
    assert_eq!(row(&nowrap.frame().unwrap(), 0), "ab⌚ ");
    selector(&mut nowrap);
    let frame = nowrap.frame().unwrap();
    assert_eq!(row(&frame, 0), "abc⌚");
    assert_eq!(frame.cursor().paint_position(), (0, 3));
    assert!(!frame.cursor().pending_wrap);
    assert_eq!(nowrap.history_extent(), 0..0);
}

#[test]
fn synchronized_non_scrolling_wrap_restores_both_rows_and_delta() {
    let mut terminal = terminal(2, 4);
    terminal.feed(b"\x1b[2;1HWXYZ\x1b[1;1H").unwrap();
    terminal.begin_update(true);
    terminal.feed("\x1b[?2026habc⌚".as_bytes()).unwrap();
    let provisional = terminal.frame().unwrap();
    assert!(provisional.rows()[0].wrapped);
    assert_eq!(row(&provisional, 1), "⌚ YZ");
    selector(&mut terminal);
    terminal.feed(b"\x1b[?2026l").unwrap();
    let completed = terminal.take_completed_sync_frame().unwrap();
    assert_eq!(row(&completed, 0), "abc⌚");
    assert_eq!(row(&completed, 1), "WXYZ");
    assert!(!completed.rows()[0].wrapped);
    assert!(completed.cursor().pending_wrap);
    let delta = provisional.delta_to(&completed).unwrap();
    assert_eq!(delta.rows.len(), 2);
    let limits = Limits {
        max_rows: 2,
        max_cols: 4,
        max_cells: 8,
    };
    assert_eq!(provisional.apply(&delta, limits), Ok(completed));
    terminal.feed(b"Z").unwrap();
    let wrapped = terminal.frame().unwrap();
    assert_eq!(row(&wrapped, 1), "ZXYZ");
    assert!(wrapped.rows()[0].wrapped);
    assert_eq!(terminal.history_extent(), 0..0);
}

#[test]
fn non_scrolling_wrap_at_bottom_outside_margins_restores_the_same_row() {
    let mut terminal = terminal(3, 4);
    terminal.feed("\x1b[1;2r\x1b[3;1Habc⌚".as_bytes()).unwrap();
    let wide = terminal.frame().unwrap();
    assert_eq!(row(&wide, 2), "⌚ c ");
    assert!(wide.rows()[2].wrapped);
    selector(&mut terminal);
    let narrow = terminal.frame().unwrap();
    assert_eq!(row(&narrow, 2), "abc⌚");
    assert!(!narrow.rows()[2].wrapped);
    assert_eq!(narrow.cursor().paint_position(), (2, 3));
    assert!(narrow.cursor().pending_wrap);
    assert_eq!(terminal.history_extent(), 0..0);
}

#[test]
fn committed_scroll_and_eviction_keep_the_published_wide_span() {
    let mut terminal = terminal(1, 4);
    terminal.feed(&b"X\r\n".repeat(1024)).unwrap();
    terminal.feed("abc⌚".as_bytes()).unwrap();
    assert_eq!(terminal.history_extent(), 1..1025);
    let archived = terminal.history_page(0, 1).unwrap();
    selector(&mut terminal);
    let frame = terminal.frame().unwrap();
    assert_eq!(frame.cluster_text(0, 0), Some("⌚\u{fe0e}"));
    assert!(frame.rows()[0].cells[0].flags.wide());
    assert_eq!(terminal.history_extent(), 1..1025);
    assert_eq!(archived.text, "abc");
    assert_eq!(terminal.history_page(0, 1).unwrap().text, archived.text);
}

#[test]
fn margin_scroll_stays_committed_and_narrow_replay_repairs_the_original_wide_edges() {
    let mut region = terminal(3, 4);
    region
        .feed("TOP\x1b[2;3r\x1b[3;1Habc⌚".as_bytes())
        .unwrap();
    selector(&mut region);
    let scrolled = region.frame().unwrap();
    assert_eq!(row(&scrolled, 0), "TOP ");
    assert_eq!(row(&scrolled, 1), "abc ");
    assert!(scrolled.rows()[1].wrapped);
    assert_eq!(scrolled.cluster_text(2, 0), Some("⌚\u{fe0e}"));
    assert!(scrolled.rows()[2].cells[0].flags.wide());
    assert_eq!(region.history_extent(), 0..0);

    let mut edges = terminal(2, 6);
    edges.feed("A界界B\x1b[1;3H⌚".as_bytes()).unwrap();
    assert_eq!(row(&edges.frame().unwrap(), 0), "A ⌚  B");
    selector(&mut edges);
    let repaired = edges.frame().unwrap();
    assert_eq!(row(&repaired, 0), "A ⌚界 B");
    assert!(repaired.rows()[0].cells[3].flags.wide());
    assert!(repaired.rows()[0].cells[4].flags.wide_continuation());
    assert_eq!(repaired.cluster_text(0, 2), Some("⌚\u{fe0e}"));
}

#[test]
fn contraction_that_cannot_restore_within_budget_keeps_wide_text() {
    let mut terminal = terminal(2, 64);
    let dense = format!("e{}", "\u{301}".repeat(31)).repeat(49);
    terminal.feed(b"\x1b[1;2H").unwrap();
    terminal.feed(dense.as_bytes()).unwrap();
    terminal.feed("\x1b[1;1H⌚".as_bytes()).unwrap();
    let before = terminal.frame().unwrap();
    let unsupported = terminal.unsupported_count();
    selector(&mut terminal);
    let fallback = terminal.frame().unwrap();
    assert_eq!(fallback.rows(), before.rows());
    assert_eq!(fallback.cursor(), before.cursor());
    assert_eq!(fallback.cluster_text(0, 0), Some("⌚\u{fe0e}"));
    assert_eq!(terminal.unsupported_count(), unsupported);
}

#[test]
fn the_default_scalar_profile_keeps_the_original_width() {
    let mut terminal = Terminal::new(2, 4).unwrap();
    terminal.feed("ab⌚\u{fe0e}".as_bytes()).unwrap();
    let frame = terminal.frame().unwrap();
    assert!(frame.rows()[0].cells[2].flags.wide());
    assert!(frame.rows()[0].cells[3].flags.wide_continuation());
    assert_eq!(frame.cluster_text(0, 2), None);
    assert!(frame.cursor().pending_wrap);
}
