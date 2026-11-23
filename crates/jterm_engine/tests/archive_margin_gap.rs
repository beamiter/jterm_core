//! Independent physical/storage oracles for opt-in early-wide-wrap provenance.
use jterm_engine::{
    history_reflow::HistoryAnchor,
    screen::{RegionDirection, ScreenState, VerticalMotion, VerticalMutation},
    terminal::Terminal,
    Color, HyperlinkId, RawRowId, ScrollbackLine, TerminalCell,
};

#[test]
fn a_prompt_cursor_range_crosses_into_history_without_copying_the_wrap_gap() {
    let mut terminal = Terminal::new(2, 4).unwrap();
    terminal.enable_grapheme_clusters();
    terminal.feed(b"> ").unwrap();
    let start = terminal.primary_cursor_anchor().unwrap();
    assert_eq!(start, HistoryAnchor { row: 0, column: 2 });
    terminal.feed("a界ZX".as_bytes()).unwrap();
    let end = terminal.primary_cursor_anchor().unwrap();
    assert_eq!(end, HistoryAnchor { row: 1, column: 4 });
    terminal.feed(b" ").unwrap();
    assert_eq!(terminal.history_extent(), 0..1);
    assert_eq!(terminal.primary_text_range(start, end).unwrap(), "a界ZX");

    terminal.feed(b"\x1b[?1049h").unwrap();
    assert!(terminal.primary_cursor_anchor().is_none());
    assert!(terminal.primary_text_range(start, end).is_err());
    terminal.feed(b"\x1b[?1049l").unwrap();
    assert_eq!(terminal.primary_text_range(start, end).unwrap(), "a界ZX");
    terminal.set_history_row_limit(0);
    assert!(terminal.primary_text_range(start, end).is_err());
}

#[test]
fn editor_suffix_proof_distinguishes_written_whitespace_from_erase_padding() {
    let mut terminal = Terminal::new(2, 10).unwrap();
    assert_eq!(terminal.primary_suffix_is_empty(), None);
    terminal.track_printed_spaces();
    terminal.feed(b"ok  \r\x1b[2C").unwrap();
    assert_eq!(terminal.primary_suffix_is_empty(), Some(false));

    terminal.feed(b"\x1b[K").unwrap();
    assert_eq!(terminal.primary_suffix_is_empty(), Some(true));
    let tab_start = terminal.primary_cursor_anchor().unwrap();
    terminal.feed(b"\t").unwrap();
    let tab_end = terminal.primary_cursor_anchor().unwrap();
    assert!(terminal.primary_text_range(tab_start, tab_end).is_err());
    terminal.feed(b"\r\x1b[2C").unwrap();
    assert_eq!(terminal.primary_suffix_is_empty(), Some(false));
    terminal.feed(b"\x1b[?1049h").unwrap();
    assert_eq!(terminal.primary_suffix_is_empty(), None);
    terminal.feed(b"\x1b[?1049l").unwrap();
    assert_eq!(terminal.primary_suffix_is_empty(), Some(false));

    terminal.feed(b"\x1bcx \r\nZ\r\n").unwrap();
    let start = HistoryAnchor {
        row: terminal.history_extent().start,
        column: 0,
    };
    let end = terminal.primary_cursor_anchor().unwrap();
    assert_eq!(terminal.primary_text_range(start, end).unwrap(), "x \nZ\n");
    assert_eq!(terminal.primary_suffix_is_empty(), Some(true));

    terminal.resize(2, 4).unwrap();
    terminal.feed(b"\x1bc").unwrap();
    terminal.feed("ab界".as_bytes()).unwrap();
    assert_eq!(terminal.primary_cursor_anchor().unwrap().column, 4);
    assert_eq!(terminal.primary_suffix_is_empty(), Some(true));
}

fn untracked() -> RawRowId {
    RawRowId::UNTRACKED
}

fn early_wrap(screen: &mut ScreenState) -> VerticalMutation {
    assert!(!screen.take_pending_wrap(true));
    assert!(screen.prepare_character_width(2, true));
    let effects = screen.vertical_early_wide_wrap(2, true, untracked);
    screen.write_char_in_row('界', 2, true);
    effects
}

fn printed_prefix(rows: usize, style: Color) -> ScreenState {
    let mut screen = ScreenState::new(rows, 4);
    screen.current_fg = style;
    assert_eq!(screen.write_ascii_in_row(b"abc", true).unwrap().0, 3);
    screen.current_fg = Color::Default;
    screen
}

fn wrapped_prefix(rows: usize) -> ScreenState {
    let mut screen = printed_prefix(rows, Color::Default);
    early_wrap(&mut screen);
    screen
}

fn capture_top(screen: &mut ScreenState, bottom: usize) -> ScrollbackLine {
    screen
        .scroll_region_one(RegionDirection::Up, 0, bottom, true, untracked)
        .unwrap()
        .removed
        .remove(0)
        .captured
        .unwrap()
}

#[test]
fn equal_physical_rows_keep_distinct_margin_origins_in_both_compression_lanes() {
    for style in [Color::Default, Color::Red] {
        let mut virgin = printed_prefix(2, style);
        let mut explicit = printed_prefix(2, style);
        explicit.write_char_in_row(' ', 1, true);
        assert!(explicit.take_pending_wrap(true));
        explicit.vertical_motion(VerticalMotion::Wrap, true, untracked);
        explicit.write_char_in_row('界', 2, true);
        early_wrap(&mut virgin);

        assert_eq!(virgin.grid.to_vec(), explicit.grid.to_vec());
        assert_eq!(virgin.grid.row_wrapped, explicit.grid.row_wrapped);
        assert_eq!(virgin.grid[0][3], TerminalCell::default());
        assert_eq!(virgin.grid[1][0].character, '界');
        assert!(virgin.grid[1][1].flags.wide_continuation());
        let proof = capture_top(&mut virgin, 1);
        let literal = capture_top(&mut explicit, 1);
        assert_eq!(proof.decompress(), literal.decompress());
        assert_eq!(proof.reflow_content_len(), literal.reflow_content_len());
        assert_eq!(
            proof.reflow_wide_continuations(),
            literal.reflow_wide_continuations()
        );
        assert!(proof.is_wrapped && literal.is_wrapped);
        assert_eq!(proof.synthetic_margin_gap(), Some(3));
        assert_eq!(literal.synthetic_margin_gap(), None);
        // Ordinary storage callers do not infer proof from identical row bytes.
        assert_eq!(
            ScrollbackLine::compress(&proof.decompress(), true).synthetic_margin_gap(),
            None
        );
    }
}

#[test]
fn positioned_default_protected_and_linked_blanks_are_not_synthetic_gaps() {
    let link = HyperlinkId::from_raw(9);
    for (protected, hyperlink) in [
        (false, HyperlinkId::NONE),
        (true, HyperlinkId::NONE),
        (false, link),
        (true, link),
    ] {
        let mut screen = ScreenState::new(2, 4);
        screen.current_flags.set_protected(protected);
        screen.current_hyperlink = hyperlink;
        screen.write_ascii_in_row(b"abc ", true);
        screen.set_cursor_column(4); // cancel deferred wrap; specifically test early wrap.
        screen.current_flags.set_protected(false);
        screen.current_hyperlink = HyperlinkId::NONE;
        early_wrap(&mut screen);

        assert_eq!(screen.grid[0][3].flags.protected(), protected);
        assert_eq!(screen.grid[0][3].hyperlink_id, hyperlink);
        let old_storage = ScrollbackLine::compress(&screen.grid[0], true).decompress();
        let line = capture_top(&mut screen, 1);
        let cells = line.decompress();
        assert!(line.is_wrapped);
        assert_eq!(line.synthetic_margin_gap(), None);
        assert_eq!(cells[3].character, ' ');
        assert_eq!(cells[3].hyperlink_id, hyperlink);
        // Compression historically omits the live DECSCA protection bit;
        // this separate archive provenance must not alter cell storage.
        assert_eq!(cells, old_storage);
    }
}

#[test]
fn prior_gap_is_revoked_by_margin_writes_or_a_new_ordinary_wrap() {
    for edit in 0..4 {
        let mut screen = ScreenState::new(2, 4);
        screen.write_ascii_in_row(b"abc", true);
        early_wrap(&mut screen);
        screen.set_cursor_position(1, 4);
        match edit {
            0 => {
                screen.write_ascii_in_row(b" ", true);
            }
            1 => {
                screen.erase_span(0, 3, 4, false);
            }
            2 => {
                screen.vertical_motion(VerticalMotion::Wrap, true, untracked);
            }
            3 => {
                let _ = screen.grid.cells_mut();
            }
            _ => unreachable!(),
        }
        let archived = capture_top(&mut screen, 1);
        assert_eq!(archived.synthetic_margin_gap(), None, "edit {edit}");
        assert_eq!(archived.decompress()[3].character, ' ');
        assert!(archived.is_wrapped);
    }
}

#[test]
fn partial_capture_and_edits_changing_the_successor_fail_closed() {
    let mut screen = wrapped_prefix(3);
    let mut partial = wrapped_prefix(3);
    let output = capture_top(&mut partial, 1);
    assert!(output.is_wrapped);
    assert_eq!(output.synthetic_margin_gap(), None);

    let mut insert = wrapped_prefix(3);
    insert
        .edit_lines(RegionDirection::Down, 1, 2, 1, untracked)
        .unwrap();
    assert!(insert.grid.row_wrapped[0]); // retain legacy wrap metadata.
    assert_eq!(capture_top(&mut insert, 2).synthetic_margin_gap(), None);
    assert_eq!(capture_top(&mut screen, 2).synthetic_margin_gap(), Some(3));
}

#[test]
fn early_motion_outside_full_region_or_without_real_departure_makes_no_proof() {
    for row in [0, 1, 2] {
        let mut screen = ScreenState::new(3, 4);
        screen.set_scroll_region(1, 2);
        screen.cursor_row = row;
        screen.write_ascii_in_row(b"abc", true);
        let effects = early_wrap(&mut screen);
        if let Some(region) = effects.region {
            assert_eq!(
                region.removed[0]
                    .captured
                    .as_ref()
                    .unwrap()
                    .synthetic_margin_gap(),
                None
            );
        }
        if row == 2 {
            assert_eq!(screen.cursor_row, 2); // physical bottom, outside region.
        }
        screen.set_scroll_region(1, 3);
        for _ in 0..3 {
            assert_eq!(capture_top(&mut screen, 2).synthetic_margin_gap(), None);
        }
    }
}

#[test]
fn real_same_coordinate_scroll_and_resize_transfer_use_the_old_margin_only() {
    let mut screen = ScreenState::new(1, 4);
    screen.write_ascii_in_row(b"abc", true);
    let effects = early_wrap(&mut screen);
    assert_eq!(screen.cursor_row, 0);
    let region = effects.region.unwrap();
    assert_eq!(
        region.removed[0]
            .captured
            .as_ref()
            .unwrap()
            .synthetic_margin_gap(),
        Some(3)
    );

    let mut screen = wrapped_prefix(3);
    let mut transfer = wrapped_prefix(3);
    let resized = transfer.resize_geometry(2, 6, 1, untracked).unwrap();
    assert_eq!(resized.transferred[0].columns(), 4);
    assert_eq!(resized.transferred[0].synthetic_margin_gap(), Some(3));
    assert_eq!(transfer.grid[0][0].character, '界');

    let mut height_only = wrapped_prefix(3);
    height_only.resize_geometry(4, 4, 0, untracked).unwrap();
    assert_eq!(
        capture_top(&mut height_only, 3).synthetic_margin_gap(),
        Some(3)
    );

    let mut partial = wrapped_prefix(3);
    partial.set_scroll_region(1, 2);
    partial.set_cursor_position(2, 1);
    let resized = partial.resize_geometry(2, 6, 1, untracked).unwrap();
    assert_eq!(resized.transferred[0].columns(), 4);
    assert_eq!(resized.transferred[0].synthetic_margin_gap(), None);

    screen.resize_geometry(3, 6, 0, untracked).unwrap();
    assert_eq!(capture_top(&mut screen, 2).synthetic_margin_gap(), None);
}
