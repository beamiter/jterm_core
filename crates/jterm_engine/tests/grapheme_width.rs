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

fn base_row(frame: &CellFrame, row: usize) -> String {
    frame.rows()[row]
        .cells
        .iter()
        .map(|cell| cell.character)
        .collect()
}

fn wide(frame: &CellFrame, row: usize, col: usize, text: &str) {
    assert_eq!(frame.cluster_text(row, col), Some(text));
    assert!(frame.rows()[row].cells[col].flags.wide());
    assert!(frame.rows()[row].cells[col + 1].flags.wide_continuation());
    assert_eq!(frame.cluster_text(row, col + 1), None);
}

#[test]
fn late_emoji_width_grows_with_every_byte_split_and_publishes_the_real_cursor() {
    let input = "a❤️b".as_bytes();
    for split in 0..=input.len() {
        let mut terminal = terminal(2, 4);
        terminal.feed(&input[..split]).unwrap();
        terminal.feed(&input[split..]).unwrap();
        let frame = terminal.frame().unwrap();
        wide(&frame, 0, 1, "❤️");
        assert_eq!(base_row(&frame, 0), "a❤ b");
        assert_eq!(frame.cursor().paint_position(), (0, 3));
        assert!(frame.cursor().pending_wrap);
    }

    let mut legacy = Terminal::new(2, 4).unwrap();
    legacy.feed(input).unwrap();
    let scalar_frame = legacy.frame().unwrap();
    assert_eq!(base_row(&scalar_frame, 0), "a❤b ");
    assert_eq!(scalar_frame.cluster_text(0, 1), None);
    assert!(!scalar_frame.cursor().pending_wrap);

    let mut terminal = terminal(2, 4);
    terminal.feed("ab*".as_bytes()).unwrap();
    let before = terminal.frame().unwrap();
    assert_eq!(before.cursor().paint_position(), (0, 3));
    assert!(!before.cursor().pending_wrap);
    for byte in "\u{fe0f}\u{20e3}".as_bytes() {
        terminal.feed(&[*byte]).unwrap();
    }
    let after = terminal.frame().unwrap();
    wide(&after, 0, 2, "*️⃣");
    assert_eq!(after.cursor().paint_position(), (0, 2));
    assert!(after.cursor().pending_wrap);
    terminal.feed(b"Z").unwrap();
    assert_eq!(terminal.frame().unwrap().cursor().paint_position(), (1, 1));
    assert!(terminal.frame().unwrap().rows()[0].wrapped);
}

#[test]
fn flags_pair_and_repeat_at_resolved_width_and_bare_keycaps_stay_narrow() {
    let mut terminal = terminal(2, 8);
    for byte in "🇺🇸🇨🇦\x1b[bZ".as_bytes() {
        terminal.feed(&[*byte]).unwrap();
    }
    let frame = terminal.frame().unwrap();
    wide(&frame, 0, 0, "🇺🇸");
    wide(&frame, 0, 2, "🇨🇦");
    wide(&frame, 0, 4, "🇨🇦");
    assert_eq!(frame.rows()[0].cells[6].character, 'Z');
    assert_eq!(frame.cursor().paint_position(), (0, 7));

    terminal.feed("\r\n*\u{20e3}Z".as_bytes()).unwrap();
    let fallback = terminal.frame().unwrap();
    assert_eq!(fallback.cluster_text(1, 0), Some("*\u{20e3}"));
    assert!(!fallback.rows()[1].cells[0].flags.wide());
    assert_eq!(fallback.rows()[1].cells[1].character, 'Z');
}

#[test]
fn virgin_margin_captures_only_the_restored_row_at_history_capacity() {
    let mut terminal = terminal(1, 4);
    terminal.feed(&b"X\r\n".repeat(1024)).unwrap();
    assert_eq!(terminal.history_extent(), 0..1024);
    let oldest = terminal.history_page(1023, 1).unwrap();
    terminal.feed(b"abc#").unwrap();
    let provisional = terminal.frame().unwrap();
    assert_eq!(base_row(&provisional, 0), "abc#");
    assert_eq!(terminal.history_extent(), 0..1024);
    terminal.feed("\u{fe0f}".as_bytes()).unwrap();
    terminal.feed("\u{20e3}".as_bytes()).unwrap();
    let frame = terminal.frame().unwrap();
    wide(&frame, 0, 0, "#️⃣");
    assert_eq!(frame.cursor().paint_position(), (0, 2));
    assert!(!frame.cursor().pending_wrap);
    assert_eq!(terminal.history_extent(), 1..1025);
    let page = terminal.history_page(0, 1).unwrap();
    assert!(page.row(0).unwrap().wrapped);
    assert_eq!(page.text, "abc");
    assert_eq!(page.cluster_text(0, 3), None);
    assert_eq!(page.selection_text(0, 3, 0, 4).unwrap(), "");
    assert_eq!(oldest.text, "X");
    assert_eq!(base_row(&provisional, 0), "abc#");
}

#[test]
fn an_already_taken_deferred_wrap_is_not_replayed() {
    let mut terminal = terminal(1, 4);
    terminal.feed("abcd♥".as_bytes()).unwrap();
    assert_eq!(terminal.history_extent(), 0..1);
    terminal.feed("\u{fe0f}".as_bytes()).unwrap();
    wide(&terminal.frame().unwrap(), 0, 0, "♥\u{fe0f}");
    assert_eq!(terminal.history_extent(), 0..1);
    assert_eq!(terminal.history_page(0, 1).unwrap().text, "abcd");
}

#[test]
fn occupied_margin_restores_a_wide_pair_with_style_link_and_clusters() {
    let mut terminal = terminal(1, 4);
    terminal.enable_underline_colors();
    terminal.enable_hyperlinks(|_, uri| uri.starts_with("https://example.test/"));
    terminal
        .feed("\x1b[4;58;5;11m\x1b]8;;https://example.test/original\x07ab界\u{301}".as_bytes())
        .unwrap();
    terminal
        .feed(b"\x1b[1;4H\x1b[58;5;3m\x1b]8;;https://example.test/keycap\x07#")
        .unwrap();
    assert_eq!(base_row(&terminal.frame().unwrap(), 0), "ab #");
    terminal.feed("\u{fe0f}\u{20e3}".as_bytes()).unwrap();
    let archived = terminal.history_page(0, 1).unwrap();
    assert_eq!(archived.text, "ab界\u{301}");
    assert_eq!(archived.cluster_text(0, 2), Some("界\u{301}"));
    assert!(archived.row(0).unwrap().cells[3].flags.wide_continuation());
    for col in [2, 3] {
        assert_eq!(archived.underline_color(0, col), Some(Color::Indexed(11)));
        let link = archived.row(0).unwrap().cells[col].hyperlink_id;
        assert_eq!(
            archived.hyperlink_target(link),
            Some("https://example.test/original")
        );
    }
    let moved = terminal.frame().unwrap();
    wide(&moved, 0, 0, "#️⃣");
    for col in [0, 1] {
        assert_eq!(moved.underline_color(0, col), Some(Color::Indexed(3)));
        let link = moved.rows()[0].cells[col].hyperlink_id;
        assert_eq!(
            moved.hyperlink_target(link),
            Some("https://example.test/keycap")
        );
    }
}

#[test]
fn nowrap_insert_and_partial_scroll_keep_physical_effects_local() {
    let mut nowrap = terminal(1, 4);
    nowrap.feed("\x1b[?7labc#️⃣".as_bytes()).unwrap();
    let frame = nowrap.frame().unwrap();
    wide(&frame, 0, 2, "#️⃣");
    assert_eq!(base_row(&frame, 0), "ab# ");
    assert_eq!(frame.cursor().paint_position(), (0, 2));
    assert!(!frame.cursor().pending_wrap);
    assert_eq!(nowrap.history_extent(), 0..0);

    let mut insert = terminal(1, 8);
    insert.feed(b"abcdefgh\x1b[1;3H\x1b[4h#").unwrap();
    assert_eq!(base_row(&insert.frame().unwrap(), 0), "ab#cdefg");
    insert.feed("\u{fe0f}\u{20e3}".as_bytes()).unwrap();
    let frame = insert.frame().unwrap();
    wide(&frame, 0, 2, "#️⃣");
    assert_eq!(base_row(&frame, 0), "ab# cdef");

    let mut region = terminal(3, 4);
    region.feed(b"TOP\x1b[2;3r\x1b[3;1Habc#").unwrap();
    region.feed("\u{fe0f}\u{20e3}".as_bytes()).unwrap();
    let frame = region.frame().unwrap();
    assert_eq!(base_row(&frame, 0), "TOP ");
    assert_eq!(base_row(&frame, 1), "abc ");
    assert!(frame.rows()[1].wrapped);
    wide(&frame, 2, 0, "#️⃣");
    assert_eq!(region.history_extent(), 0..0);
}

#[test]
fn one_column_keeps_the_head_and_a_control_ends_the_checkpoint() {
    let mut narrow = terminal(2, 1);
    narrow.feed("❤️".as_bytes()).unwrap();
    let frame = narrow.frame().unwrap();
    assert_eq!(frame.cluster_text(0, 0), Some("❤️"));
    assert_eq!(frame.cursor().paint_position(), (0, 0));
    assert!(frame.cursor().pending_wrap);
    assert_eq!(narrow.history_extent(), 0..0);
    narrow.feed(b"Z").unwrap();
    assert_eq!(narrow.frame().unwrap().rows()[1].cells[0].character, 'Z');

    let mut interrupted = terminal(2, 4);
    interrupted
        .feed("abc#\x1b[1;2H\u{fe0f}".as_bytes())
        .unwrap();
    let frame = interrupted.frame().unwrap();
    assert_eq!(frame.cursor().paint_position(), (0, 1));
    assert_eq!(base_row(&frame, 0), "abc#");
    assert_eq!(frame.cluster_text(0, 3), None);
    assert_eq!(interrupted.history_extent(), 0..0);
}

#[test]
fn exhausted_destination_cannot_apply_half_of_a_wrap() {
    let mut terminal = terminal(2, 64);
    let dense = format!("e{}", "\u{301}".repeat(31)).repeat(49);
    terminal.feed(b"\x1b[2;3H").unwrap();
    terminal.feed(dense.as_bytes()).unwrap();
    terminal.feed("\x1b[1;64H♥".as_bytes()).unwrap();
    let before = terminal.frame().unwrap();
    let unsupported = terminal.unsupported_count();
    terminal.feed("\u{fe0f}".as_bytes()).unwrap();
    let rejected = terminal.frame().unwrap();
    assert_eq!(before.rows(), rejected.rows());
    assert_eq!(before.cursor(), rejected.cursor());
    assert_eq!(rejected.cluster_text(0, 63), None);
    assert_eq!(terminal.history_extent(), 0..0);
    assert_eq!(terminal.unsupported_count(), unsupported + 1);

    // Overwriting an occupied target does reclaim its decorations.
    terminal.feed(b"\x1b[2;1H\x1b[2K").unwrap();
    terminal.feed(dense.as_bytes()).unwrap();
    terminal.feed("\x1b[1;64H❤️".as_bytes()).unwrap();
    wide(&terminal.frame().unwrap(), 1, 0, "❤️");
}

#[test]
fn synchronized_delta_carries_the_restored_margin_and_new_cell_pair() {
    let mut terminal = terminal(2, 4);
    terminal.feed(b"abcX\x1b[1;4H").unwrap();
    terminal.begin_update(true);
    terminal.feed(b"\x1b[?2026h#").unwrap();
    let provisional = terminal.frame().unwrap();
    terminal
        .feed("\u{fe0f}\u{20e3}\x1b[?2026l".as_bytes())
        .unwrap();
    let corrected = terminal.take_completed_sync_frame().unwrap();
    assert_eq!(base_row(&provisional, 0), "abc#");
    assert_eq!(base_row(&corrected, 0), "abcX");
    wide(&corrected, 1, 0, "#️⃣");
    let delta = provisional.delta_to(&corrected).unwrap();
    assert_eq!(delta.rows.len(), 2);
    let limits = Limits {
        max_rows: 2,
        max_cols: 4,
        max_cells: 8,
    };
    assert_eq!(provisional.apply(&delta, limits).unwrap(), corrected);
}
