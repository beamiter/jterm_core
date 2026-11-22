use super::*;

#[test]
fn row_effects_are_already_applied_exactly_once() {
    let mut t = Terminal::new(3, 12).unwrap();
    t.feed(b"abcdef\r\x1b[2C\x1b[2P").unwrap();
    assert!(t.visible_text().starts_with("abef"));
    t.feed(b"\x1b[2@XY").unwrap();
    assert!(t.visible_text().starts_with("abXYef"));
    t.feed(b"\x1b[2X").unwrap();
    assert!(t.visible_text().starts_with("abXY  "));
    t.feed(b"\r\x1b[2K").unwrap();
    assert!(t.visible_text().lines().next().unwrap().trim().is_empty());
}

#[test]
fn alternate_saved_slots_repeated_entry_and_resize() {
    let mut t = Terminal::new(4, 12).unwrap();
    t.feed(b"PRIMARY\x1b[31m\x1b[?1049hALT\x1b[?1048hX\x1b[?1048lY\x1b[?1049h")
        .unwrap();
    assert!(t.screen.use_alt_buffer);
    assert!(t.visible_text().starts_with("ALTY"));
    assert_eq!(t.frame().unwrap().stamp().screen, Screen::Alternate);
    t.resize(5, 14).unwrap();
    t.feed(b"\x1b[?7l\x1b[?1049lZ").unwrap();
    assert!(!t.screen.use_alt_buffer);
    assert!(t.visible_text().starts_with("PRIMARYZ"));
    assert!(t.protocol.modes().contains(&7));
    assert_eq!(t.screen.current_fg, crate::Color::Red);
    assert_eq!(t.unsupported_count(), 0);
}

#[test]
fn mode47_retains_alternate_1047_clears_and_keyboard_slots_swap_once() {
    let mut t = Terminal::new(4, 12).unwrap();
    t.feed(b"\x1b[=2u\x1b[?47hA\x1b[=4u\x1b[?47h").unwrap();
    assert_eq!(t.protocol.keyboard_enhancement_flags(), 4);
    t.feed(b"\x1b[?47l").unwrap();
    assert_eq!(t.protocol.keyboard_enhancement_flags(), 2);
    t.feed(b"\x1b[?47h").unwrap();
    assert!(t.visible_text().starts_with('A'));
    t.feed(b"\x1b[?47l\x1b[?1047h").unwrap();
    assert!(t.visible_text().trim().is_empty());
    assert_eq!(t.screen.cursor_col, 0);
}

#[test]
fn erase_and_region_edits_use_shared_screen() {
    let mut t = Terminal::new(4, 8).unwrap();
    t.feed(b"111\r\n222\r\n333\r\n444\x1b[2;1H\x1b[L").unwrap();
    assert!(t.visible_text().lines().nth(1).unwrap().trim().is_empty());
    assert!(t.visible_text().lines().nth(2).unwrap().starts_with("222"));
    t.feed(b"\x1b[M").unwrap();
    assert!(t.visible_text().lines().nth(1).unwrap().starts_with("222"));
    t.feed(b"\x1b[2;3r\x1b[S").unwrap();
    assert!(t.visible_text().lines().next().unwrap().starts_with("111"));
    assert!(t.visible_text().lines().nth(1).unwrap().starts_with("333"));
    t.feed(b"\x1b[2J").unwrap();
    assert!(t.visible_text().trim().is_empty());
    assert_eq!(t.unsupported_count(), 0);
}

#[test]
fn ed2_replaces_protected_wide_cells_with_background_blanks() {
    let mut t = Terminal::new(2, 4).unwrap();
    t.feed("\x1b[41m\x1b[1\"q中XQ".as_bytes()).unwrap();
    assert!(t.screen.pending_wrap);
    t.feed(b"\x1b[?2J").unwrap();
    assert_eq!(t.screen.grid.get(0, 0).character, '中');
    let (row, col) = (t.screen.cursor_row, t.screen.cursor_col);
    t.feed(b"\x1b[2J").unwrap();
    assert_eq!((t.screen.cursor_row, t.screen.cursor_col), (row, col));
    assert!(!t.screen.pending_wrap);
    for row in 0..t.screen.grid.rows() {
        for col in 0..t.screen.grid.cols() {
            let cell = t.screen.grid.get(row, col);
            assert_eq!(cell.character, ' ');
            assert_eq!(cell.background, crate::Color::Red);
            assert_eq!(cell.flags, crate::StyleFlags::default());
        }
    }
}

#[test]
fn cleared_screens_do_not_join_new_text_using_erased_soft_wraps() {
    let mut terminal = Terminal::new(3, 4).unwrap();
    terminal.feed(b"12345").unwrap();
    assert!(terminal.frame().unwrap().rows()[0].wrapped);
    terminal.feed(b"\x1b[2J\x1b[H\r\nnew").unwrap();
    assert!(!terminal.frame().unwrap().rows()[0].wrapped);

    terminal
        .feed(b"\x1b[?1049h12345\x1b[?1049l\x1b[?47h")
        .unwrap();
    assert!(terminal.frame().unwrap().rows()[0].wrapped);
    terminal.feed(b"\x1b[?47l\x1b[?1049h\r\nalt").unwrap();
    assert!(!terminal.frame().unwrap().rows()[0].wrapped);
}

#[test]
fn display_erase_resets_wrap_only_on_completely_erased_rows() {
    let mut terminal = Terminal::new(3, 4).unwrap();
    terminal.feed(b"123456789").unwrap();
    assert!(terminal.frame().unwrap().rows()[0].wrapped);
    assert!(terminal.frame().unwrap().rows()[1].wrapped);

    // jsh uses CHA/CUU/ED to redraw its prompt after a previous long line.
    terminal.feed(b"\x1b[1G\x1b[1A\x1b[Jcmd\r\nout").unwrap();
    let frame = terminal.frame().unwrap();
    assert!(frame.rows()[0].wrapped);
    assert!(!frame.rows()[1].wrapped);
    assert_eq!(frame.rows()[1].cells[0].character, 'c');
    assert_eq!(frame.rows()[2].cells[0].character, 'o');

    terminal.feed(b"\x1b[1;2H\x1b[J").unwrap();
    assert!(terminal.frame().unwrap().rows()[0].wrapped);
    terminal.feed(b"\x1b[1;4H\x1b[1J").unwrap();
    assert!(!terminal.frame().unwrap().rows()[0].wrapped);

    terminal
        .feed(b"\x1b[H1\x1b[1\"q2\x1b[0\"q345\x1b[H\x1b[?2J")
        .unwrap();
    let frame = terminal.frame().unwrap();
    assert!(frame.rows()[0].wrapped);
    assert_eq!(frame.rows()[0].cells[1].character, '2');
}

#[test]
fn host_sequences_fragment_identically_and_unknown_modes_stay_visible() {
    let stream =
        b"PRIMARY\x1b[?1049h\x1b[2J\x1b[2;2HZ\x1b[3b\x1bM\x1bE\x1bD\x1b[?25l\x1b[?1049l\x1b[?9999h";
    let mut a = Terminal::new(4, 16).unwrap();
    let mut b = Terminal::new(4, 16).unwrap();
    a.feed(stream).unwrap();
    for byte in stream {
        b.feed(&[*byte]).unwrap();
    }
    assert_eq!(a.visible_text(), b.visible_text());
    assert_eq!(a.screen.cursor_col, b.screen.cursor_col);
    assert!(!a.frame().unwrap().cursor().visible);
    assert_eq!(a.unsupported_count(), 1);
}

#[test]
fn unsupported_diagnostics_are_bounded_and_omit_payload() {
    let mut t = Terminal::new(3, 10).unwrap();
    t.feed(b"\x1b]52;c;PRIVATE_CLIPBOARD\x07").unwrap();
    for mode in 3000..3050 {
        t.feed(format!("\x1b[?{mode}h").as_bytes()).unwrap();
    }
    assert_eq!(t.unsupported_events().len(), 16);
    assert!(t
        .unsupported_events()
        .iter()
        .all(|s| s.len() < 128 && !s.contains("PRIVATE")));
    assert_eq!(t.unsupported_count(), 51);
}

#[test]
fn keyboard_mode_modifiers_and_unicode_are_rust_owned() {
    let mut t = Terminal::new(3, 10).unwrap();
    assert_eq!(
        t.encode_key("ArrowUp", false, false, false).unwrap(),
        b"\x1b[A"
    );
    t.feed(b"\x1b[?1h").unwrap();
    assert_eq!(
        t.encode_key("ArrowUp", false, false, false).unwrap(),
        b"\x1bOA"
    );
    assert_eq!(
        t.encode_key("ArrowUp", true, false, true).unwrap(),
        b"\x1b[1;6A"
    );
    assert_eq!(
        t.encode_key("界", false, true, false).unwrap(),
        "\x1b界".as_bytes()
    );
    assert_eq!(t.encode_key("c", false, false, true).unwrap(), [3]);
    assert_eq!(t.encode_key("[", false, false, true).unwrap(), [27]);
    assert_eq!(t.encode_key("Tab", true, false, false).unwrap(), b"\x1b[Z");
    assert_eq!(t.encode_key("Enter", false, false, false).unwrap(), b"\r");
    assert!(t.encode_key("界", false, false, true).is_err());
    assert!(t.encode_key("\x1b[2J", false, false, false).is_err());
    assert!(t.encode_key("Dead", false, false, false).is_err());
    t.feed(b"\x1b[=1u").unwrap();
    assert!(t.encode_key("a", false, false, false).is_err());
}

#[test]
fn paste_controls_markers_normalization_and_wire_bound() {
    let mut t = Terminal::new(3, 10).unwrap();
    assert_eq!(
        t.encode_paste("a\r\nb\nc\rd\t界").unwrap(),
        "a\rb\rc\rd\t界".as_bytes()
    );
    for text in ["\x1b[201~", "a\0b", "\u{0085}", "\x7f"] {
        assert!(t.encode_paste(text).is_err());
    }
    assert!(t.encode_paste(&"x".repeat(4096)).is_ok());
    assert!(t.encode_paste(&"x".repeat(4097)).is_err());
    t.feed(b"\x1b[?2004h").unwrap();
    assert_eq!(t.encode_paste("a\nb").unwrap(), b"\x1b[200~a\rb\x1b[201~");
    assert_eq!(t.encode_paste(&"x".repeat(4084)).unwrap().len(), 4096);
    assert!(t.encode_paste(&"x".repeat(4085)).is_err());
    t.feed(b"\x1b[?2004l").unwrap();
    assert_eq!(t.encode_paste("x").unwrap(), b"x");
    assert_eq!(t.unsupported_count(), 0);
}

#[test]
fn vim_modify_other_keys_does_not_block_plain_escape_or_function_keys() {
    let mut t = Terminal::new(3, 10).unwrap();
    t.feed(b"\x1b[>4;2m").unwrap();
    assert_eq!(t.encode_key("Escape", false, false, false).unwrap(), [27]);
    assert_eq!(t.encode_key("Enter", false, false, false).unwrap(), [13]);
    assert_eq!(
        t.encode_key("ArrowUp", false, false, false).unwrap(),
        b"\x1b[A"
    );
    assert_eq!(t.encode_key("A", true, false, false).unwrap(), b"A");
    assert_eq!(
        t.encode_key("c", false, false, true).unwrap(),
        b"\x1b[27;5;99~"
    );
    assert_eq!(
        t.encode_key("Escape", false, false, true).unwrap(),
        b"\x1b[27;5;27~"
    );
    t.feed(b"\x1b[>4;1f").unwrap();
    assert_eq!(
        t.encode_key("c", false, false, true).unwrap(),
        b"\x1b[99;5u"
    );
    t.feed(b"\x1b[>4;1m").unwrap();
    assert_eq!(t.encode_key("c", false, false, true).unwrap(), [3]);
    assert_eq!(
        t.encode_key("c", false, true, false).unwrap(),
        b"\x1b[99;3u"
    );
    t.feed(b"\x1b[>4;0m").unwrap();
    assert_eq!(t.encode_key("c", false, true, false).unwrap(), b"\x1bc");
}

#[test]
fn committed_text_preserves_multiscalar_utf8_without_key_or_paste_wrapping() {
    let mut t = Terminal::new(3, 10).unwrap();
    let text = "中文e\u{301}👨\u{200d}👩";
    assert_eq!(t.encode_text(text).unwrap(), text.as_bytes());
    // Bracketed paste and modified-key protocols must not reinterpret IME.
    t.feed(b"\x1b[?2004h\x1b[>4;2m\x1b[>4;1f").unwrap();
    assert_eq!(t.encode_text(text).unwrap(), text.as_bytes());
    assert_eq!(t.encode_text("A").unwrap(), b"A");
    assert!(t.encode_text("").unwrap().is_empty());
    t.feed(b"\x1b[=8u").unwrap();
    assert!(t.encode_text(text).is_err());
    assert!(t.encode_text("").unwrap().is_empty());
    t.feed(b"\x1b[=0u").unwrap();
    assert_eq!(t.encode_text(text).unwrap(), text.as_bytes());
}

#[test]
fn committed_text_rejects_controls_and_enforces_utf8_bytes_not_scalars() {
    let t = Terminal::new(3, 10).unwrap();
    for text in ["a\nb", "\r", "\t", "\0", "\x1b[201~", "\x7f", "\u{0085}"] {
        assert!(t.encode_text(text).is_err());
    }
    assert_eq!(t.encode_text(&"é".repeat(2048)).unwrap().len(), 4096);
    assert!(t.encode_text(&"é".repeat(2049)).is_err());
    assert!(t.encode_text(&"x".repeat(4097)).is_err());
}

#[test]
fn xtgettcap_negative_reply_does_not_claim_or_reflect_capabilities() {
    let mut t = Terminal::new(3, 10).unwrap();
    assert_eq!(
        t.feed(b"\x1bP+q544e;436f\x1b\\").unwrap(),
        b"\x1bP0+r\x1b\\"
    );
    assert_eq!(t.negative_query_replies(), 1);
    assert_eq!(t.unsupported_count(), 1);
    assert_eq!(t.unsupported_events(), &["DCS XTGETTCAP"]);
    assert!(t.feed(b"\x1b[>c").unwrap().is_empty()); // No fabricated DA2 identity.
    let before = t.negative_query_replies();
    assert!(t.feed(b"\x1b]52;c;SECRET\x07").unwrap().is_empty());
    assert_eq!(t.negative_query_replies(), before);
    let mut fragmented = Terminal::new(3, 10).unwrap();
    let mut replies = Vec::new();
    for byte in b"\x1bP+qnot-hex\x1b\\" {
        replies.extend(fragmented.feed(&[*byte]).unwrap());
    }
    assert_eq!(replies, b"\x1bP0+r\x1b\\");
}

#[test]
fn vim_dcs_probe_is_opaque_without_reply_or_cursor_mutation() {
    let mut t = Terminal::new(3, 10).unwrap();
    t.feed(b"AB").unwrap();
    let before = t.visible_text();
    assert!(t.feed(b"\x1bPzz\x1b\\").unwrap().is_empty());
    assert_eq!(t.visible_text(), before);
    assert_eq!(t.screen.cursor_col, 2);
    assert_eq!(t.unsupported_events(), &["DCS Vim compatibility probe"]);
    assert!(t
        .feed(b"\x1bPunknown_PRIVATE_payload\x1b\\")
        .unwrap()
        .is_empty());
    assert!(t
        .unsupported_events()
        .iter()
        .all(|s| !s.contains("PRIVATE") && !s.contains("prefix=")));
    assert_eq!(t.negative_query_replies(), 0);
}

#[test]
fn vim_unknown_dcs_and_intermediate_sgr_probe_preserves_style_and_cpr() {
    let mut t = Terminal::new(3, 10).unwrap();
    t.feed(b"\x1b[1;31mAB").unwrap();
    let before = t.visible_text();
    let probe = b"\x1bPzz\x1b\\\x1b[0%m\x1b[6n";
    assert_eq!(t.feed(probe).unwrap(), b"\x1b[1;3R");
    assert_eq!(t.visible_text(), before);
    assert_eq!(t.screen.current_fg, crate::Color::Red);
    assert!(t.screen.current_flags.bold());
    assert_eq!(t.unsupported_count(), 2);
    assert_eq!(t.negative_query_replies(), 0);
    let mut b = Terminal::new(3, 10).unwrap();
    b.feed(b"\x1b[1;31mAB").unwrap();
    let mut replies = Vec::new();
    for byte in probe {
        replies.extend(b.feed(&[*byte]).unwrap());
    }
    assert_eq!(replies, b"\x1b[1;3R");
    assert_eq!(b.screen.current_fg, crate::Color::Red);
    assert!(b.screen.current_flags.bold());
    assert_eq!(b.visible_text(), before);
}

#[test]
fn primary_history_captures_scroll_and_snapshot_is_immutable() {
    let mut t = Terminal::new(2, 8).unwrap();
    t.feed(b"one\r\ntwo\r\nthree\r\n").unwrap();
    assert_eq!(t.history_rows(), 2);
    let page = t.history_page(0, 128).unwrap();
    assert_eq!(page.text, "one\ntwo");
    assert_eq!(page.selection_text(0, 1, 1, 2).unwrap(), "ne\ntw");
    assert_eq!(page.selection_text(0, 0, 1, 0).unwrap(), "one\n");
    assert!(page.selection_text(0, 9, 1, 0).is_err());
    assert!(page.selection_text(1, 0, 0, 0).is_err());
    assert!(page.selection_text(0, 0, 3, 0).is_err());
    t.feed(b"four\r\n").unwrap();
    assert_eq!(page.text, "one\ntwo");
    assert_eq!(t.history_page(1, 1).unwrap().text, "two");
    assert_eq!(t.history_page(t.history_rows(), 1).unwrap().row_count, 0);
}

#[test]
fn alternate_and_partial_margins_never_archive_or_expose_primary_history() {
    let mut t = Terminal::new(3, 8).unwrap();
    t.feed(b"a\r\nb\r\nc\r\n").unwrap();
    assert_eq!(t.history_rows(), 1);
    t.feed(b"\x1b[?1049hALT\r\nX\r\nY\r\nZ\x1b[3J").unwrap();
    assert_eq!(t.history_rows(), 0);
    assert!(t.history_page(0, 128).unwrap().text.is_empty());
    assert_eq!(t.history_page(1, 128).unwrap().offset, 0);
    t.feed(b"\x1b[?1049l").unwrap();
    assert_eq!(t.history_page(0, 128).unwrap().text, "a");
    t.feed(b"\x1b[2;3r\x1b[3;1H\n\n\x1b[S").unwrap();
    assert_eq!(t.history_rows(), 1);
    t.feed(b"\x1b[3J").unwrap();
    assert_eq!(t.history_rows(), 0);
}

#[test]
fn history_eviction_and_page_limits_are_bounded() {
    let mut t = Terminal::new(1, 8).unwrap();
    for n in 0..1030 {
        t.feed(format!("{n}\r\n").as_bytes()).unwrap();
    }
    assert_eq!(t.history_rows(), MAX_HISTORY_ROWS);
    assert_eq!(t.history_page(1023, 1).unwrap().text, "6");
    assert_eq!(t.history_page(0, 1).unwrap().text, "1029");
    assert!(t.history_page(0, 0).is_err());
    assert!(t.history_page(0, 129).is_err());
    assert!(t.history_page(1025, 1).is_err());
}

#[test]
fn history_extent_tracks_physical_rows_across_eviction_resize_and_clear() {
    let mut terminal = Terminal::new(1, 12).unwrap();
    terminal.feed(b"\x1b[31mpinned\x1b[0m\r\n").unwrap();
    assert_eq!(terminal.history_extent(), 0..1);
    let page = terminal.history_page(0, 1).unwrap();
    let row = page.row(0).unwrap();
    assert_eq!(row.cells[0].foreground, crate::Color::Red);
    assert_eq!(row.cells.len(), 12);
    assert!(page.row(1).is_none());

    terminal.resize(1, 8).unwrap();
    assert_eq!(terminal.history_extent(), 0..1);
    for _ in 0..1024 {
        terminal.feed(b"row\r\n").unwrap();
    }
    assert_eq!(terminal.history_extent(), 1..1025);
    assert_eq!(row.cells[0].character, 'p');
    terminal.feed(b"\x1b[?1049hALT\r\n\x1b[3J").unwrap();
    assert_eq!(terminal.history_extent(), 1025..1025);
    terminal.feed(b"\x1b[?1049l").unwrap();
    assert_eq!(terminal.history_extent(), 1..1025);
    terminal.feed(b"\x1b[3Jnext\r\n").unwrap();
    assert_eq!(terminal.history_extent(), 1025..1026);
}

#[test]
fn history_wide_softwrap_and_resize_keep_original_geometry() {
    let mut t = Terminal::new(1, 4).unwrap();
    t.feed("界abXY\r\n".as_bytes()).unwrap();
    let page = t.history_page(0, 128).unwrap();
    assert_eq!(page.text, "界abXY");
    assert_eq!(page.selection_text(0, 0, 0, 2).unwrap(), "界");
    assert_eq!(page.selection_text(0, 1, 0, 2).unwrap(), "");
    t.resize(2, 2).unwrap();
    assert_eq!(t.history_page(0, 128).unwrap().text, "界abXY");
    assert_eq!(t.history_rows(), 2);
    t.feed(b"\x1b[S").unwrap();
    assert_eq!(t.history_rows(), 3);
}

#[test]
fn shared_parser_fragmentation_and_sgr() {
    let mut a = Terminal::new(4, 40).unwrap();
    let mut b = Terminal::new(4, 40).unwrap();
    let bytes = "hello\r\n\x1b[31m世界\x1b[0m!".as_bytes();
    a.feed(bytes).unwrap();
    for byte in bytes {
        b.feed(&[*byte]).unwrap();
    }
    assert_eq!(a.visible_text(), b.visible_text());
    assert_eq!(a.screen.grid[1][0].foreground, crate::Color::Red);
    assert_eq!(a.screen.grid[1][0].character, '世');
}

#[test]
fn utf8_every_split_preserves_wide_frame_and_colors() {
    let bytes = "a界中\x1b[31m文\x1b[0mZ".as_bytes();
    let mut baseline = Terminal::new(3, 5).unwrap();
    baseline.feed(bytes).unwrap();
    for cut in 0..=bytes.len() {
        let mut t = Terminal::new(3, 5).unwrap();
        t.feed(&bytes[..cut]).unwrap();
        t.feed(&bytes[cut..]).unwrap();
        for row in 0..3 {
            assert_eq!(t.screen.grid[row], baseline.screen.grid[row], "cut={cut}");
        }
        assert_eq!(
            t.frame().unwrap().cursor(),
            baseline.frame().unwrap().cursor()
        );
        assert_eq!(t.unsupported_count(), 0);
    }
}

#[test]
fn wide_overwrite_repairs_both_halves_and_wraps() {
    let mut t = Terminal::new(3, 4).unwrap();
    t.feed("界中".as_bytes()).unwrap();
    assert!(t.screen.grid[0][0].flags.wide());
    assert!(t.screen.grid[0][1].flags.wide_continuation());
    t.feed(b"X").unwrap();
    assert_eq!(t.screen.grid[1][0].character, 'X');
    t.feed(b"\x1b[1;2HA").unwrap();
    assert_eq!(t.screen.grid[0][0].character, ' ');
    assert_eq!(t.screen.grid[0][1].character, 'A');
    assert!(!t.screen.grid[0][1].flags.wide_continuation());
    t.feed(b"\x1b[1;3HB").unwrap();
    assert_eq!(t.screen.grid[0][3].character, ' ');
    assert!(!t.screen.grid[0][3].flags.wide_continuation());
    t.feed("\x1b[2;4H界".as_bytes()).unwrap();
    assert_eq!(t.screen.grid[2][0].character, '界');
    assert!(t.screen.grid[2][1].flags.wide_continuation());
    t.feed("\x1b[?7l\x1b[2;4H中".as_bytes()).unwrap();
    assert_eq!(t.screen.grid[1][2].character, '中');
    assert!(t.screen.grid[1][3].flags.wide_continuation());
    t.frame().unwrap();
}

#[test]
fn narrow_resize_repairs_pairs_and_single_column_retains_core_truncated_lead() {
    let mut t = Terminal::new(2, 4).unwrap();
    t.feed("ab界".as_bytes()).unwrap();
    t.resize(2, 3).unwrap();
    assert_eq!(t.screen.grid[0][2].character, ' ');
    assert!(!t.screen.grid[0][2].flags.wide());
    t.resize(2, 1).unwrap();
    t.feed("\x1b[H界".as_bytes()).unwrap();
    assert_eq!(t.screen.grid[1][0].character, '界');
    assert!(t.screen.grid[1][0].flags.wide());
    assert!(!t.screen.grid[1][0].flags.wide_continuation());
    assert_eq!(t.unsupported_count(), 0);
    t.frame().unwrap();
}

#[test]
fn combining_marks_are_explicitly_unsupported_without_cursor_or_base_damage() {
    let mut t = Terminal::new(2, 4).unwrap();
    t.feed("e\u{301}\u{200d}X".as_bytes()).unwrap();
    assert_eq!(t.screen.grid[0][0].character, 'e');
    assert_eq!(t.screen.grid[0][1].character, 'X');
    assert_eq!(t.screen.cursor_col, 2);
    assert_eq!(t.unsupported_count(), 2);
    assert_eq!(
        t.unsupported_events(),
        &["zero-width scalar / grapheme composition unsupported"]
    );
}

#[test]
fn unicode_023_halfwidth_marks_keep_four_cells_at_every_utf8_split() {
    let input = "ﾊﾟｸﾞ".as_bytes();
    for cut in 0..=input.len() {
        let mut t = Terminal::new(2, 8).unwrap();
        t.feed(&input[..cut]).unwrap();
        t.feed(&input[cut..]).unwrap();
        for (col, character) in ['ﾊ', 'ﾟ', 'ｸ', 'ﾞ'].into_iter().enumerate() {
            let cell = &t.screen.grid[0][col];
            assert_eq!(cell.character, character, "cut={cut}, col={col}");
            assert!(!cell.flags.wide() && !cell.flags.wide_continuation());
        }
        let frame = t.frame().unwrap();
        assert_eq!((frame.cursor().row, frame.cursor().col), (0, 4));
        assert!(!frame.cursor().pending_wrap);
        assert_eq!(t.unsupported_count(), 0);
    }
}

#[test]
fn unicode_023_halfwidth_marks_wrap_and_backspace_after_bytewise_feed() {
    let mut t = Terminal::new(3, 4).unwrap();
    for byte in "abcﾟ".as_bytes() {
        t.feed(&[*byte]).unwrap();
    }
    let frame = t.frame().unwrap();
    assert_eq!((frame.cursor().row, frame.cursor().col), (0, 3));
    assert!(frame.cursor().pending_wrap);
    assert_eq!(frame.rows()[0].cells[3].character, 'ﾟ');
    for byte in "ﾞX".as_bytes() {
        t.feed(&[*byte]).unwrap();
    }
    assert_eq!(t.screen.grid[1][0].character, 'ﾞ');
    assert_eq!(t.screen.grid[1][1].character, 'X');
    t.feed(b"\x08Y").unwrap();
    let frame = t.frame().unwrap();
    assert_eq!(frame.rows()[1].cells[1].character, 'Y');
    assert_eq!((frame.cursor().row, frame.cursor().col), (1, 2));
    assert!(!frame.cursor().pending_wrap);
    assert_eq!(t.unsupported_count(), 0);
}

#[test]
fn unicode_023_zero_width_rejection_is_preserved_between_halfwidth_marks() {
    let input = "ﾟ\u{301}\u{200d}ﾞ".as_bytes();
    for cut in 0..=input.len() {
        let mut t = Terminal::new(2, 4).unwrap();
        t.feed(&input[..cut]).unwrap();
        t.feed(&input[cut..]).unwrap();
        assert_eq!(t.screen.grid[0][0].character, 'ﾟ', "cut={cut}");
        assert_eq!(t.screen.grid[0][1].character, 'ﾞ', "cut={cut}");
        assert_eq!(t.screen.cursor_col, 2);
        assert_eq!(t.unsupported_count(), 2);
        assert_eq!(
            t.unsupported_events(),
            &["zero-width scalar / grapheme composition unsupported"]
        );
        t.frame().unwrap();
    }
}

#[test]
fn immutable_capture_shares_idle_and_unchanged_rows() {
    let mut terminal = Terminal::new(4, 6).unwrap();
    let initial = terminal.frame().unwrap();
    assert!(Arc::ptr_eq(
        &initial.shared_rows(),
        &terminal.frame().unwrap().shared_rows()
    ));

    terminal.feed(b"\x1b[3;1H").unwrap();
    let moved = terminal.frame().unwrap();
    assert_ne!(moved.stamp().revision, initial.stamp().revision);
    assert_eq!(moved.cursor().row, 2);
    assert!(Arc::ptr_eq(&moved.shared_rows(), &initial.shared_rows()));

    terminal.feed(b"X").unwrap();
    let edited = terminal.frame().unwrap();
    let delta = moved.delta_to(&edited).unwrap();
    assert_eq!(delta.rows.len(), 1);
    assert_eq!(delta.rows[0].0, 2);
    for (index, (old, new)) in moved.rows().iter().zip(edited.rows()).enumerate() {
        assert_eq!(Arc::ptr_eq(&old.cells, &new.cells), index != 2);
    }
    assert_eq!(moved.rows()[2].cells[0].character, ' ');
    assert_eq!(edited.rows()[2].cells[0].character, 'X');

    terminal.feed(b"\x1b[?1049h").unwrap();
    let alternate = terminal.frame().unwrap();
    assert_eq!(alternate.stamp().screen, Screen::Alternate);
    assert!(edited.delta_to(&alternate).is_err());

    terminal.resize(5, 8).unwrap();
    let resized = terminal.frame().unwrap();
    assert_eq!((resized.rows().len(), resized.cols()), (5, 8));
    assert!(alternate.delta_to(&resized).is_err());
}

#[test]
fn resize_validation_and_kernel_failure_are_atomic() {
    let mut t = Terminal::new(3, 8).unwrap();
    t.feed(b"abcdefgh").unwrap();
    let before = t.frame().unwrap();
    for (rows, cols) in [(0, 8), (3, 0), (129, 8), (3, 257), (usize::MAX, 1)] {
        assert!(t
            .resize_with(rows, cols, || panic!("invalid request reached PTY"))
            .is_err());
    }
    assert!(t
        .resize_with(1, 1, || Err("injected ioctl failure".into()))
        .is_err());
    let after = t.frame().unwrap();
    assert_eq!(after.stamp(), before.stamp());
    assert_eq!(after.cursor(), before.cursor());
    assert_eq!(after.cols(), before.cols());
    assert_eq!(after.rows(), before.rows());
    t.layout = u64::MAX;
    assert!(t
        .resize_with(1, 1, || panic!("overflow reached PTY"))
        .is_err());
    t.layout = 1;
    t.revision = u64::MAX;
    assert!(t
        .resize_with(1, 1, || panic!("overflow reached PTY"))
        .is_err());
}

#[test]
fn cursor_shape_visibility_and_pending_wrap_across_resize_and_alternate() {
    use crate::CursorShape;
    let mut t = Terminal::new(3, 4).unwrap();
    for (param, shape) in [
        (0, CursorShape::Block),
        (1, CursorShape::Block),
        (2, CursorShape::Block),
        (3, CursorShape::Underline),
        (4, CursorShape::Underline),
        (5, CursorShape::Beam),
        (6, CursorShape::Beam),
    ] {
        t.feed(format!("\x1b[{param} q").as_bytes()).unwrap();
        assert_eq!(t.frame().unwrap().cursor().shape, shape);
    }
    t.feed(b"\x1b[?25labcd").unwrap();
    assert!(t.frame().unwrap().cursor().pending_wrap);
    assert!(!t.frame().unwrap().cursor().visible);
    t.resize(1, 1).unwrap();
    let frame = t.frame().unwrap();
    assert_eq!((frame.cursor().row, frame.cursor().col), (0, 0));
    assert!(!frame.cursor().pending_wrap);
    assert_eq!(frame.cursor().shape, CursorShape::Beam);
    assert!(!frame.cursor().visible);
    t.feed(b"\x1b[?1049h\x1b[4 q\x1b[?25h").unwrap();
    assert_eq!(t.frame().unwrap().cursor().shape, CursorShape::Underline);
    t.resize(128, 256).unwrap();
    t.feed(b"\x1b[999;999H").unwrap();
    assert_eq!(
        (
            t.frame().unwrap().cursor().row,
            t.frame().unwrap().cursor().col
        ),
        (127, 255)
    );
    t.feed(b"\x1b[?1049l").unwrap();
    assert_eq!(t.frame().unwrap().cursor().shape, CursorShape::Beam);
    // DECTCEM is global, not part of the saved primary cursor state.
    assert!(t.frame().unwrap().cursor().visible);
    assert_eq!(
        (
            t.frame().unwrap().cursor().row,
            t.frame().unwrap().cursor().col
        ),
        (0, 0)
    );
    assert_eq!(t.unsupported_count(), 0);
}

#[test]
fn resize_and_unknown_are_explicit() {
    let mut t = Terminal::new(4, 20).unwrap();
    t.feed(b"ok\x1b[?9999h").unwrap();
    assert_eq!(t.unsupported_count(), 1);
    assert!(t.resize(0, 10).is_err());
    t.resize(5, 30).unwrap();
    let frame = t.frame().unwrap();
    assert_eq!(frame.cols(), 30);
    assert_eq!(frame.rows().len(), 5);
}

#[test]
fn wrap_scroll_and_bounds() {
    let mut t = Terminal::new(2, 3).unwrap();
    t.feed(b"abcdefg").unwrap();
    assert_eq!(t.screen.grid[0][0].character, 'd');
    assert_eq!(t.screen.grid[1][0].character, 'g');
    assert!(t.feed(&vec![b'a'; MAX_FEED_BYTES + 1]).is_err());
    assert!(Terminal::new(200, 30).is_err());
}

#[test]
fn one_complete_frame_per_poll_budget_reserves_live_revision() {
    let mut t = Terminal::new(2, 20).unwrap();
    t.begin_update(true);
    t.feed(b"\x1b[?2026hA\x1b[?2026l\x1b[?2026hB\x1b[?2026l\x1b[?2026hC")
        .unwrap();
    assert!(t.publication_held());
    let completed = t.take_completed_sync_frame().unwrap();
    assert_eq!(completed.rows()[0].cells[0].character, 'A');
    assert_eq!(completed.rows()[0].cells[1].character, ' ');
    assert!(t.revision > completed.stamp().revision);
    t.feed(b"\x1b[?2026l\x1b[?2026hD").unwrap();
    assert!(t.completed_sync_frame.is_none());
    t.sync_started = Some(std::time::Instant::now() - std::time::Duration::from_millis(251));
    assert!(!t.publication_held());
    assert!(t.frame().unwrap().stamp().revision > completed.stamp().revision);
}
#[test]
fn fragmented_hold_repeated_begin_deadline_and_release() {
    let mut t = Terminal::new(2, 20).unwrap();
    let start = Instant::now();
    t.feed_at(b"\x1b[?202", start).unwrap();
    assert!(!t.publication_held_at(start));
    t.feed_at(b"6hpartial", start).unwrap();
    assert!(t.publication_held_at(start + Duration::from_millis(249)));
    let expired = start + Duration::from_millis(250);
    t.feed_at(b"\x1b[?2026h", expired).unwrap();
    assert!(!t.publication_held_at(expired));
    assert_eq!(
        t.feed_at(b"\x1b[?2026$p", expired).unwrap(),
        b"\x1b[?2026;2$y"
    );
    t.feed_at(b"\x1b[?2026l\x1b[?2026h", expired).unwrap();
    assert!(t.publication_held_at(expired));
    assert_eq!(
        t.feed_at(b"\x1b[?2026$p", expired).unwrap(),
        b"\x1b[?2026;1$y"
    );
    t.feed_at(b"\x1b[?2026l", expired).unwrap();
    assert!(!t.publication_held_at(expired));
    assert_eq!(
        t.feed_at(b"\x1b[?2026$p", expired).unwrap(),
        b"\x1b[?2026;2$y"
    );
}

#[test]
fn owner_epoch_is_stable_across_feeds_and_cannot_form_a_cross_session_delta() {
    let epoch = NonZeroU64::new(7).unwrap();
    let mut t = Terminal::with_epoch(2, 4, epoch).unwrap();
    let before = t.frame().unwrap();
    t.feed(b"hi").unwrap();
    let after = t.frame().unwrap();
    assert_eq!(before.stamp().epoch, epoch.get());
    assert_eq!(after.stamp().epoch, epoch.get());
    assert!(before.delta_to(&after).is_ok());
    let unrelated = Terminal::new(2, 4).unwrap().frame().unwrap();
    assert!(unrelated.delta_to(&after).is_err());
}

#[test]
fn oversized_strings_are_partition_independent_and_cannot_execute_nested_controls() {
    for kind in *b"]P_X^" {
        for chunk in [1, 7, 4095, 4096, 4097, 65536] {
            let mut terminal = Terminal::new(2, 16).unwrap();
            terminal.feed(b"SAFE").unwrap();
            let mut input = vec![0x1b, kind];
            input.extend(vec![b'x'; 8192]);
            input.extend(b"\x1b[2J\x1b[6n\x1bP+q544e");
            input.extend(vec![b'z'; 8192]);
            input.extend(b"\x1b\\OK");
            for part in input.chunks(chunk) {
                assert!(terminal.feed(part).unwrap().is_empty());
            }
            assert_eq!(terminal.parser.rejected_count(), 1);
            assert_eq!(terminal.negative_query_replies(), 0);
            assert!(terminal.visible_text().starts_with("SAFEOK"));
            assert_eq!(
                terminal.feed(b"\x1bP+q544e\x1b\\").unwrap(),
                b"\x1bP0+r\x1b\\"
            );
        }
    }
}

#[test]
fn canceled_strings_recover_without_replaying_payload_or_replying() {
    for kind in *b"]P_X^" {
        for cancel in [0x18, 0x1a] {
            for oversize in [false, true] {
                let mut terminal = Terminal::new(2, 16).unwrap();
                let mut input = vec![0x1b, kind];
                input.extend(b"+q544e");
                input.extend(vec![b'x'; if oversize { 8192 } else { 20 }]);
                input.push(cancel);
                input.extend(b"RECOVERED");
                for part in input.chunks(3) {
                    assert!(terminal.feed(part).unwrap().is_empty());
                }
                assert!(terminal.visible_text().starts_with("RECOVERED"));
                assert_eq!(terminal.parser.rejected_count(), u64::from(oversize));
            }
        }
    }
}

#[test]
fn dcs_exact_payload_limit_accepts_but_one_more_rejects_across_feeds() {
    for payload_len in [4095, 4096, 4097] {
        for chunk in [1, 13, 4096, 65536] {
            let mut terminal = Terminal::new(2, 16).unwrap();
            let mut input = b"\x1bP+q".to_vec();
            input.extend(vec![b'a'; payload_len - 2]);
            input.extend(b"\x1b\\");
            let mut replies = Vec::new();
            for part in input.chunks(chunk) {
                replies.extend(terminal.feed(part).unwrap());
            }
            if payload_len <= 4096 {
                assert_eq!(replies, b"\x1bP0+r\x1b\\");
                assert_eq!(terminal.parser.rejected_count(), 0);
            } else {
                assert!(replies.is_empty());
                assert_eq!(terminal.parser.rejected_count(), 1);
            }
        }
    }
}

#[test]
fn repeated_osc_oversize_recovers_on_bel_and_accepts_fresh_query() {
    let mut terminal = Terminal::new(2, 16).unwrap();
    terminal.feed(b"SAFE").unwrap();
    for round in 1..=2 {
        terminal.feed(b"\x1b]0;").unwrap();
        for _ in 0..1024 {
            assert!(terminal.feed(&[b'x'; 1024]).unwrap().is_empty());
        }
        assert_eq!(terminal.parser.rejected_count(), round);
        assert!(terminal.feed(b"\x07OK").unwrap().is_empty());
    }
    assert!(terminal.visible_text().starts_with("SAFEOKOK"));
    assert_eq!(
        terminal.feed(b"\x1bP+q544e\x1b\\").unwrap(),
        b"\x1bP0+r\x1b\\"
    );
}
