use super::*;
use crate::history_reflow::HistoryAnchor;

#[test]
fn decst8c_restores_tabs_after_custom_stops_at_every_split() {
    let command = b"\x1b[?005W";
    for cut in 0..=command.len() {
        let mut terminal = Terminal::new(3, 25).unwrap();
        terminal.feed(b"\x1b[3g\x1b[4G\x1bH\x1b[2;25H").unwrap();
        terminal.feed(b"x").unwrap();
        let before = terminal.frame().unwrap();
        assert!(before.cursor().pending_wrap);
        terminal.feed(&command[..cut]).unwrap();
        terminal.feed(&command[cut..]).unwrap();
        assert_eq!(terminal.frame().unwrap(), before);
        terminal.feed(b"\r\t").unwrap();
        assert_eq!(terminal.frame().unwrap().cursor().col, 8);
        terminal.feed(b"\x1b[2I").unwrap();
        assert_eq!(terminal.frame().unwrap().cursor().col, 24);
        terminal.feed(b"\x1b[2Z").unwrap();
        assert_eq!(terminal.frame().unwrap().cursor().col, 8);
        assert_eq!(terminal.unsupported_count(), 0);
    }
}

#[test]
fn saved_cursor_queries_follow_the_retained_slot_and_reset() {
    const QUERY: &[u8] = b"\x1b[?1048$p";
    for save in [b"\x1b7".as_slice(), b"\x1b[s", b"\x1b[?1048h"] {
        for restore in [b"\x1b8".as_slice(), b"\x1b[u", b"\x1b[?1048l"] {
            for chunk in 1..=QUERY.len() {
                let mut terminal = Terminal::new(4, 8).unwrap();
                assert_eq!(terminal.feed(QUERY).unwrap(), b"\x1b[?1048;2$y");
                terminal.feed(b"\x1b[2;3H").unwrap();
                let saved = terminal.frame().unwrap().cursor();
                let stream = [save, QUERY, b"\x1b[4;7H", restore, QUERY].concat();
                let mut replies = Vec::new();
                for bytes in stream.chunks(chunk) {
                    replies.extend(terminal.feed(bytes).unwrap());
                }
                assert_eq!(replies, b"\x1b[?1048;1$y\x1b[?1048;1$y");
                assert_eq!(terminal.frame().unwrap().cursor(), saved);
                assert_eq!(
                    terminal
                        .feed(&[b"\x1bc".as_slice(), QUERY].concat())
                        .unwrap(),
                    b"\x1b[?1048;2$y"
                );
            }
        }
    }
}

#[test]
fn alternate_mode_queries_follow_the_live_screen_across_aliases() {
    for enter in [47, 1047, 1049] {
        for leave in [47, 1047, 1049] {
            for chunk in 1..=8 {
                let mut terminal = Terminal::new(3, 12).unwrap();
                let query = b"\x1b[?47$p\x1b[?1047$p\x1b[?1049$p";
                assert_eq!(
                    terminal.feed(query).unwrap(),
                    b"\x1b[?47;2$y\x1b[?1047;2$y\x1b[?1049;2$y"
                );
                let stream = format!("\x1b[?{enter}h");
                for bytes in stream.as_bytes().chunks(chunk) {
                    terminal.feed(bytes).unwrap();
                }
                assert!(terminal.alternate_screen_active());
                assert_eq!(
                    terminal.feed(query).unwrap(),
                    b"\x1b[?47;1$y\x1b[?1047;1$y\x1b[?1049;1$y"
                );
                let stream = format!("\x1b[?{leave}l");
                for bytes in stream.as_bytes().chunks(chunk) {
                    terminal.feed(bytes).unwrap();
                }
                assert!(!terminal.alternate_screen_active());
                assert_eq!(
                    terminal.feed(query).unwrap(),
                    b"\x1b[?47;2$y\x1b[?1047;2$y\x1b[?1049;2$y"
                );
            }
        }
    }
}

#[test]
fn invalid_scroll_margins_preserve_cursor_wrap_and_existing_region() {
    for sequence in [b"\x1b[4;2r".as_slice(), b"\x1b[3;3r", b"\x1b[6;99r"] {
        for cut in 0..=sequence.len() {
            let mut terminal = Terminal::new(5, 4).unwrap();
            terminal.feed(b"\x1b[2;4r\x1b[3;1HABCD").unwrap();
            let before = terminal.frame().unwrap();
            assert!(before.cursor().pending_wrap);
            terminal.feed(&sequence[..cut]).unwrap();
            terminal.feed(&sequence[cut..]).unwrap();
            assert_eq!(terminal.frame().unwrap(), before);
            assert_eq!(
                (
                    terminal.vt.screen.scroll_region_top,
                    terminal.vt.screen.scroll_region_bottom
                ),
                (1, 3)
            );
        }
    }
    let mut terminal = Terminal::new(5, 4).unwrap();
    terminal.feed(b"\x1b[2;4r\x1b[?6h\x1b[2;99r").unwrap();
    assert_eq!(
        (
            terminal.vt.screen.scroll_region_top,
            terminal.vt.screen.scroll_region_bottom
        ),
        (1, 4)
    );
    assert_eq!(terminal.frame().unwrap().cursor().paint_position(), (1, 0));
    terminal.feed(b"\x1b[r").unwrap();
    assert_eq!(
        (
            terminal.vt.screen.scroll_region_top,
            terminal.vt.screen.scroll_region_bottom
        ),
        (0, 4)
    );
    assert_eq!(terminal.frame().unwrap().cursor().paint_position(), (0, 0));
}

#[test]
fn key_format_queries_report_encoding_and_preserve_tabs() {
    for chunk in 1..=8 {
        let mut terminal = Terminal::new(2, 16).unwrap();
        terminal.feed(b"\x1b[>4;2m").unwrap();
        let tabs = terminal.vt.screen.tab_stops.clone();
        for (format, expected) in [(0, 0), (1, 1), (2, 0), (65535, 0), (1, 1)] {
            let stream = format!("\x1b[>4;{format}f\x1b[?04g\x1b[?3g\x1b[?0g");
            let mut reply = Vec::new();
            for bytes in stream.as_bytes().chunks(chunk) {
                reply.extend(terminal.feed(bytes).unwrap());
            }
            assert_eq!(reply, format!("\x1b[>4;{expected}f").as_bytes());
            assert_eq!(terminal.vt.screen.tab_stops, tabs);
            assert_eq!(
                terminal.encode_key("a", false, false, true).unwrap(),
                if expected == 1 {
                    b"\x1b[97;5u".as_slice()
                } else {
                    b"\x1b[27;5;97~"
                }
            );
        }
        assert!(terminal
            .feed(b"\x1b[?4:0g\x1b[?4;0g\x1b[?4 g")
            .unwrap()
            .is_empty());
        assert_eq!(terminal.vt.screen.tab_stops, tabs);
        terminal.feed(b"\t").unwrap();
        assert_eq!(terminal.frame().unwrap().cursor().col, 8);
        terminal.feed(b"\x1b[g\r\t").unwrap();
        assert_eq!(terminal.frame().unwrap().cursor().col, 15);
        terminal.feed(b"\x1b[4G\x1bH").unwrap();
        assert!(terminal.vt.screen.tab_stops[3]);
        terminal.feed(b"\x1b[3g").unwrap();
        assert!(terminal.vt.screen.tab_stops.iter().all(|stop| !stop));
    }
}

#[test]
fn key_format_negotiation_preserves_pending_wrap_and_changes_key_bytes() {
    let negotiation = b"\x1b[>4;2m\x1b[>4;1f";
    for cut in 0..=negotiation.len() {
        let mut terminal = Terminal::new(2, 4).unwrap();
        terminal.feed(b"ABCD").unwrap();
        let before = terminal.frame().unwrap();
        assert!(before.cursor().pending_wrap);
        terminal.feed(&negotiation[..cut]).unwrap();
        terminal.feed(&negotiation[cut..]).unwrap();
        assert_eq!(terminal.frame().unwrap(), before);
        assert_eq!(
            terminal.encode_key("a", false, false, true).unwrap(),
            b"\x1b[97;5u"
        );
        terminal.feed(b"X").unwrap();
        assert_eq!(terminal.visible_text(), "ABCD\nX   ");
    }
}

#[test]
fn directory_reports_keep_authority_and_ignore_invalid_paths() {
    let mut terminal = Terminal::new(2, 8).unwrap();
    terminal.enable_window_updates();
    assert_eq!(terminal.reported_directory_uri(), None);
    terminal
        .feed(b"\x1b]7;file://ssh-host/projects/shared%20terminal\x1b\\")
        .unwrap();
    assert_eq!(
        terminal.reported_directory_uri(),
        Some("file://ssh-host/projects/shared%20terminal")
    );
    for invalid in [
        &b"\x1b]7;file://ssh-host/projects/%00\x07"[..],
        &b"\x1b]7;file://guest@ssh-host/projects\x07"[..],
        &b"\x1b]7;http://ssh-host/projects\x07"[..],
    ] {
        assert!(terminal.feed(invalid).unwrap().is_empty());
        assert_eq!(
            terminal.reported_directory_uri(),
            Some("file://ssh-host/projects/shared%20terminal")
        );
    }
}

#[test]
fn palette_queries_keep_order_and_only_changed_colors_publish() {
    let mut terminal = Terminal::new(2, 8).unwrap();
    let initial = TerminalDefaultColors {
        background: (22, 33, 44),
        ..TerminalDefaultColors::default()
    };
    terminal.set_color_defaults(initial);
    let (initial_revision, _) = terminal.paint_colors().unwrap();
    let reply = terminal
        .feed(b"\x1b]4;202;#12e371;202;?\x07\x1b[5n\x1b]11;?\x1b\\\x1b]12;rgb:1/2/3\x07")
        .unwrap();
    assert_eq!(
        reply,
        b"\x1b]4;202;rgb:1212/e3e3/7171\x07\x1b[0n\x1b]11;rgb:1616/2121/2c2c\x1b\\"
    );
    let (changed_revision, colors) = terminal.paint_colors().unwrap();
    assert_eq!(changed_revision, initial_revision + 2);
    assert_eq!(colors.palette_color(202), (18, 227, 113));
    assert_eq!(colors.cursor_override(), Some((17, 34, 51)));
    assert_eq!(
        terminal.feed(b"\x1b]12;?\x07").unwrap(),
        b"\x1b]12;rgb:1111/2222/3333\x07"
    );
    let (still, unchanged) = terminal.paint_colors().unwrap();
    assert_eq!(still, changed_revision);
    assert!(Arc::ptr_eq(&colors, &unchanged));

    let next = TerminalDefaultColors {
        background: (1, 2, 3),
        ..initial
    };
    terminal.set_color_defaults(next);
    assert_eq!(
        terminal.paint_colors().unwrap().1.palette_color(202),
        (18, 227, 113)
    );
    terminal.feed(b"\x1b]104;202\x07\x1b]112\x07").unwrap();
    let reset = terminal.paint_colors().unwrap().1;
    assert_eq!(reset.palette_color(202), ColorState::default_256_color(202));
    assert_eq!(reset.cursor_override(), None);
    terminal.feed(b"\x1b]11;#aaaaff\x07\x1bc").unwrap();
    assert_eq!(
        terminal.paint_colors().unwrap().1.effective_background(),
        (1, 2, 3)
    );
}

#[test]
fn window_updates_share_bounded_title_state_and_keep_bell_out_of_osc() {
    let mut terminal = Terminal::new(4, 10).unwrap();
    terminal.enable_window_updates();
    terminal
        .feed("\x1b]2;title\u{202e}\x07\x1b[22;2t".as_bytes())
        .unwrap();
    let update = terminal.take_window_updates();
    assert_eq!(update.title.as_deref(), Some("title"));
    assert!(!update.bell);
    assert_eq!(terminal.window_title(), Some("title"));
    assert_eq!(
        terminal
            .feed(b"\x1b]2;other\x1b\\\x1b[23;2t\x1b[21t\x07\x07")
            .unwrap(),
        b"\x1b]ltitle\x1b\\"
    );
    let update = terminal.take_window_updates();
    assert_eq!(update.title.as_deref(), Some("title"));
    assert!(update.bell);
    let update = terminal.take_window_updates();
    assert!(update.title.is_none());
    assert!(!update.bell);
}

#[test]
fn csi_bell_and_utf8_recovery_survive_every_feed_split() {
    let bytes = "XY\x1b[2;\x073H!\x1b[9;界\x1b[6n".as_bytes();
    for cut in 0..=bytes.len() {
        let mut terminal = Terminal::new(4, 10).unwrap();
        terminal.enable_window_updates();
        let mut replies = terminal.feed(&bytes[..cut]).unwrap();
        let bell = terminal.take_window_updates().bell;
        replies.extend(terminal.feed(&bytes[cut..]).unwrap());
        assert_ne!(bell, terminal.take_window_updates().bell, "cut {cut}");
        assert_eq!(replies, b"\x1b[2;6R", "cut {cut}");
        assert_eq!(
            terminal.visible_text().lines().nth(1).unwrap().trim_end(),
            "  !界"
        );
        assert_eq!(terminal.unsupported_count(), 0, "cut {cut}");
    }
}

#[test]
fn discarded_escape_releases_a_new_query_at_every_feed_split() {
    let bytes = b"AB\x1b%%%%%%%%%\x07\r\x1b[6nZ";
    for cut in 0..=bytes.len() {
        let mut terminal = Terminal::new(2, 8).unwrap();
        terminal.enable_window_updates();
        let mut replies = terminal.feed(&bytes[..cut]).unwrap();
        let bell = terminal.take_window_updates().bell;
        replies.extend(terminal.feed(&bytes[cut..]).unwrap());
        assert_ne!(bell, terminal.take_window_updates().bell, "cut {cut}");
        assert_eq!(replies, b"\x1b[1;1R", "cut {cut}");
        assert_eq!(
            terminal.visible_text().lines().next().unwrap().trim_end(),
            "ZB"
        );
    }
}

#[test]
fn focus_and_pointer_encoding_follows_negotiated_modes() {
    let mut terminal = Terminal::new(4, 10).unwrap();
    assert_eq!(terminal.encode_focus(true), None);
    assert_eq!(terminal.encode_mouse(MouseInput::Press(0), 2, 1, 0), None);
    terminal.feed(b"\x1b[?1004;1002;1006h").unwrap();
    assert_eq!(terminal.encode_focus(true), Some(b"\x1b[I".as_slice()));
    assert_eq!(terminal.encode_focus(false), Some(b"\x1b[O".as_slice()));
    assert_eq!(
        terminal
            .encode_mouse(MouseInput::Press(0), 2, 1, 16)
            .unwrap(),
        b"\x1b[<16;3;2M"
    );
    assert_eq!(
        terminal.encode_mouse(MouseInput::Motion(None), 3, 1, 0),
        None
    );
    assert_eq!(
        terminal
            .encode_mouse(MouseInput::Motion(Some(0)), 3, 1, 0)
            .unwrap(),
        b"\x1b[<32;4;2M"
    );
    assert_eq!(
        terminal
            .encode_mouse(MouseInput::Release(0), 3, 1, 0)
            .unwrap(),
        b"\x1b[<0;4;2m"
    );
    assert_eq!(
        terminal.encode_mouse(MouseInput::WheelUp, 2, 1, 0).unwrap(),
        b"\x1b[<64;3;2M"
    );
    assert_eq!(terminal.encode_mouse(MouseInput::Press(0), 10, 1, 0), None);
    terminal
        .feed(b"\x1b[?1004;1002;1006l\x1b[?1007;1049;1h")
        .unwrap();
    assert_eq!(terminal.encode_focus(false), None);
    assert_eq!(
        terminal.encode_mouse(MouseInput::WheelUp, 2, 1, 0).unwrap(),
        b"\x1bOA"
    );
    terminal.feed(b"\x1b[?1049l").unwrap();
    assert_eq!(terminal.encode_mouse(MouseInput::WheelUp, 2, 1, 0), None);
}

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
    assert!(t.vt.screen.use_alt_buffer);
    assert!(t.visible_text().starts_with("ALTY"));
    assert_eq!(t.frame().unwrap().stamp().screen, Screen::Alternate);
    t.resize(5, 14).unwrap();
    t.feed(b"\x1b[?7l\x1b[?1049lZ").unwrap();
    assert!(!t.vt.screen.use_alt_buffer);
    assert!(t.visible_text().starts_with("PRIMARYZ"));
    assert!(t.vt.protocol.modes().contains(&7));
    assert_eq!(t.vt.screen.current_fg, crate::Color::Red);
    assert_eq!(t.unsupported_count(), 0);
}

#[test]
fn mode47_retains_alternate_1047_clears_and_keyboard_slots_swap_once() {
    let mut t = Terminal::new(4, 12).unwrap();
    t.feed(b"\x1b[=2u\x1b[?47hA\x1b[=4u\x1b[?47h").unwrap();
    assert_eq!(t.vt.protocol.keyboard_enhancement_flags(), 4);
    t.feed(b"\x1b[?47l").unwrap();
    assert_eq!(t.vt.protocol.keyboard_enhancement_flags(), 2);
    t.feed(b"\x1b[?47h").unwrap();
    assert!(t.visible_text().starts_with('A'));
    t.feed(b"\x1b[?47l\x1b[?1047h").unwrap();
    assert!(t.visible_text().trim().is_empty());
    assert_eq!(t.vt.screen.cursor_col, 0);
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
    assert!(t.vt.screen.pending_wrap);
    t.feed(b"\x1b[?2J").unwrap();
    assert_eq!(t.vt.screen.grid.get(0, 0).character, '中');
    let (row, col) = (t.vt.screen.cursor_row, t.vt.screen.cursor_col);
    t.feed(b"\x1b[2J").unwrap();
    assert_eq!((t.vt.screen.cursor_row, t.vt.screen.cursor_col), (row, col));
    assert!(!t.vt.screen.pending_wrap);
    for row in 0..t.vt.screen.grid.rows() {
        for col in 0..t.vt.screen.grid.cols() {
            let cell = t.vt.screen.grid.get(row, col);
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
    assert_eq!(a.vt.screen.cursor_col, b.vt.screen.cursor_col);
    assert!(!a.frame().unwrap().cursor().visible);
    assert_eq!(a.unsupported_count(), 1);
}

#[test]
fn ascii_row_batches_match_scalar_prints_at_wraps_and_grapheme_boundaries() {
    let make = || {
        let mut terminal = Terminal::new(3, 9).unwrap();
        terminal.enable_grapheme_clusters();
        terminal.enable_underline_colors();
        terminal.enable_hyperlinks(|_, target| target.starts_with("https://"));
        terminal
    };
    let mut batched = make();
    let mut scalar = make();
    for text in [
        "界界abcdefghi\r\n\x1b[4;58;2;1;2;3mstyled ascii1\u{fe0f}\u{20e3}\x1b[2b",
        "\x1b]8;;https://example.com/ascii\x07ABCDE wide row\x1b]8;;\x07",
        "\x1b[?7l\rreplace-at-the-right-margin\x1b[?7h",
        "\x1b[1;1H\x1b[4hINSERT\x1b[4l\x1b(0jklmnopq\x1b(B",
    ] {
        batched.feed(text.as_bytes()).unwrap();
        for byte in text.bytes() {
            scalar.feed(&[byte]).unwrap();
        }
        let fast = batched.frame().unwrap();
        let slow = scalar.frame().unwrap();
        assert_eq!(fast.rows(), slow.rows());
        assert_eq!(fast.cursor(), slow.cursor());
        for (row, cells) in fast.rows().iter().enumerate() {
            assert_eq!(
                fast.underline_color_runs(row),
                slow.underline_color_runs(row)
            );
            for (column, cell) in cells.cells.iter().enumerate() {
                assert_eq!(
                    fast.cluster_text(row, column),
                    slow.cluster_text(row, column)
                );
                assert_eq!(
                    fast.hyperlink_target(cell.hyperlink_id),
                    slow.hyperlink_target(cell.hyperlink_id)
                );
            }
        }
        assert_eq!(
            batched.history_page(0, 128).unwrap().text,
            scalar.history_page(0, 128).unwrap().text
        );
    }
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
fn paste_normalization_matches_literal_crlf_rules_for_mixed_utf8() {
    let mut terminal = Terminal::new(2, 8).unwrap();
    let pieces = ["", "界", "\r", "\n", "\t"];
    for bracketed in [false, true] {
        terminal
            .feed(if bracketed {
                b"\x1b[?2004h"
            } else {
                b"\x1b[?2004l"
            })
            .unwrap();
        for first in pieces {
            for second in pieces {
                for third in pieces {
                    let input = format!("{first}{second}{third}");
                    let normalized = input
                        .replace("\r\n", "\n")
                        .replace('\r', "\n")
                        .replace('\n', "\r");
                    let expected = if bracketed {
                        format!("\x1b[200~{normalized}\x1b[201~").into_bytes()
                    } else {
                        normalized.into_bytes()
                    };
                    assert_eq!(terminal.encode_paste(&input).unwrap(), expected);
                }
            }
        }
    }
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
    assert_eq!(t.encode_native_paste("first\r\nsecond").unwrap(), b"first");
    assert!(t.encode_native_paste(&"x".repeat(4097)).is_err());
    t.feed(b"\x1b[?2004h").unwrap();
    assert_eq!(t.encode_paste("a\nb").unwrap(), b"\x1b[200~a\rb\x1b[201~");
    assert_eq!(t.encode_paste(&"x".repeat(4084)).unwrap().len(), 4096);
    assert!(t.encode_paste(&"x".repeat(4085)).is_err());
    let maximum = "x".repeat(MAX_STREAMED_PASTE_BYTES - 12);
    assert_eq!(
        t.encode_native_paste(&maximum).unwrap().len(),
        MAX_STREAMED_PASTE_BYTES
    );
    assert!(t.encode_native_paste(&(maximum + "x")).is_err());
    assert!(t.encode_native_paste("paste\x1b[201~").is_err());
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
fn native_keyboard_reports_only_disambiguation_and_keeps_ime_text_whole() {
    let mut terminal = Terminal::new(4, 80).unwrap();
    terminal.enable_keyboard_disambiguation();
    assert_eq!(terminal.feed(b"\x1b[>31u\x1b[?u").unwrap(), b"\x1b[?1u");
    assert_eq!(
        terminal.encode_key("Enter", true, false, false).unwrap(),
        b"\x1b[13;2u"
    );
    assert_eq!(
        terminal.encode_key("Enter", false, false, false).unwrap(),
        b"\r"
    );
    assert_eq!(
        terminal.encode_key("Escape", false, false, false).unwrap(),
        b"\x1b[27u"
    );
    assert_eq!(
        terminal
            .encode_key_with_base("!", Some('1'), true, true, false)
            .unwrap(),
        b"\x1b[49;4u"
    );
    assert_eq!(
        terminal.encode_text("你e\u{301}").unwrap(),
        "你e\u{301}".as_bytes()
    );

    assert_eq!(terminal.feed(b"\x1b[?1049h\x1b[?u").unwrap(), b"\x1b[?0u");
    terminal.feed(b"\x1b[=15;2u").unwrap();
    assert_eq!(terminal.feed(b"\x1b[?1049l\x1b[?u").unwrap(), b"\x1b[?1u");
    assert_eq!(terminal.feed(b"\x1b[?1049h\x1b[?u").unwrap(), b"\x1b[?0u");
    assert_eq!(
        terminal.feed(b"\x1b[?1049l\x1b[<u\x1b[?u").unwrap(),
        b"\x1b[?0u"
    );
}

#[test]
fn completed_output_annotations_follow_the_live_pen_and_link_bytes() {
    let mut terminal = Terminal::new(4, 20).unwrap();
    terminal.enable_underline_colors();
    terminal.enable_hyperlinks(|_, target| target == "https://example.test/card");
    terminal.observe_command_blocks();
    let wire = "\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C;id=one;command=first\x07\
                \x1b[1;38;2;31;171;193;4:3;58;2;2;4;6m\x1b]8;;https://example.test/card\x1b\\中\
                \x1b]8;;\x1b\\\x1b[0m plain\r\n\x1b]133;D;0;id=one\x07";
    for chunk in wire.as_bytes().chunks(3) {
        assert!(terminal.feed(chunk).unwrap().is_empty());
    }
    let block = terminal.completed_command_blocks().unwrap().back().unwrap();
    let text = block.output.as_ref().unwrap();
    assert_eq!(text, "中 plain\n");
    let annotations = block.output_annotations.as_ref().unwrap();
    assert!(annotations.valid_for(text));
    let [span] = annotations.spans.as_slice() else {
        panic!("expected one style run");
    };
    assert_eq!(&text[span.bytes.clone()], "中");
    assert_eq!(span.style.foreground, Color::Rgb(31, 171, 193));
    assert_eq!(span.style.underline_color, Color::Rgb(2, 4, 6));
    assert_eq!(span.style.flags.underline(), crate::UnderlineStyle::Curly);
    assert!(span.style.flags.bold());
    assert_eq!(&text[annotations.links[0].bytes.clone()], "中");
    assert_eq!(annotations.links[0].target, "https://example.test/card");
}

#[test]
fn hard_reset_clears_both_screens_and_live_protocol_state() {
    let mut terminal = Terminal::new(3, 20).unwrap();
    terminal.enable_keyboard_disambiguation();
    terminal.observe_command_blocks();
    terminal
        .feed(b"\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C;id=one;command=first\x07one\r\ntwo\r\nthree\r\n\x1b]133;D;0;id=one\x07")
        .unwrap();
    let primary_end = terminal.history_extent().end;
    assert_eq!(primary_end, 1);
    assert_eq!(terminal.completed_command_blocks().unwrap().len(), 1);
    terminal
        .feed(b"\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C;id=two;command=second\x07\x1b[>1u\x1b[?1049h\x1b[>1u\x1b[?25l\x1b[?2026h\x1b[31mALT")
        .unwrap();
    let alternate = terminal.frame().unwrap();
    assert_eq!(alternate.stamp().screen, Screen::Alternate);
    assert!(terminal.publication_held());

    assert_eq!(
        terminal
            .feed(b"\x1bc\x1b[?u\x1b[6n\x1b]133;D;0;id=two\x07")
            .unwrap(),
        b"\x1b[?0u\x1b[1;1R"
    );
    let reset = terminal.frame().unwrap();
    assert_eq!(reset.stamp().screen, Screen::Primary);
    assert!(reset.cursor().visible && !terminal.publication_held());
    assert!(reset.rows().iter().all(|row| row
        .cells
        .iter()
        .all(|cell| *cell == crate::TerminalCell::default())));
    assert_eq!(terminal.history_extent(), primary_end..primary_end);
    assert_eq!(terminal.history_retained_bytes(), 0);
    assert_eq!(terminal.completed_command_blocks().unwrap().len(), 1);
    assert!(!terminal.command_prompt_ready());
    assert_eq!(terminal.encode_key("i", false, false, true).unwrap(), b"\t");
    assert_eq!(terminal.feed(b"\x1b[?1049h\x1b[?u").unwrap(), b"\x1b[?0u");
    assert_eq!(terminal.feed(b"\x1b[>31u\x1b[?u").unwrap(), b"\x1b[?1u");
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
fn xtgettcap_colors_report_live_capabilities_across_feed_splits() {
    let query = b"\x1bP+q436f;636F6C6F7273;524742\x1b\\";
    let reply = b"\x1bP1+r436f=323536;636F6C6F7273=323536;524742=38\x1b\\";
    for cut in 0..=query.len() {
        let mut terminal = Terminal::new(2, 4).unwrap();
        terminal.feed(b"\x1b[31mABCD").unwrap();
        let before = terminal.frame().unwrap();
        assert!(before.cursor().pending_wrap);
        let mut bytes = terminal.feed(&query[..cut]).unwrap();
        bytes.extend(terminal.feed(&query[cut..]).unwrap());
        assert_eq!(bytes, reply, "cut {cut}");
        assert_eq!(terminal.frame().unwrap(), before);
        assert_eq!(terminal.negative_query_replies(), 0);
        assert_eq!(terminal.unsupported_count(), 0);
        assert_eq!(
            terminal.feed(b"\x1bP+q524742;544e;436f\x1b\\").unwrap(),
            b"\x1bP1+r524742=38\x1b\\"
        );
        assert_eq!(
            terminal.feed(b"\x1bP+q544e\x1b\\").unwrap(),
            b"\x1bP0+r\x1b\\"
        );
        assert_eq!(terminal.negative_query_replies(), 1);
        assert_eq!(terminal.unsupported_events(), &["DCS XTGETTCAP"]);
    }
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
    assert_eq!(t.vt.screen.cursor_col, 2);
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
    assert_eq!(t.vt.screen.current_fg, crate::Color::Red);
    assert!(t.vt.screen.current_flags.bold());
    assert_eq!(t.unsupported_count(), 2);
    assert_eq!(t.negative_query_replies(), 0);
    let mut b = Terminal::new(3, 10).unwrap();
    b.feed(b"\x1b[1;31mAB").unwrap();
    let mut replies = Vec::new();
    for byte in probe {
        replies.extend(b.feed(&[*byte]).unwrap());
    }
    assert_eq!(replies, b"\x1b[1;3R");
    assert_eq!(b.vt.screen.current_fg, crate::Color::Red);
    assert!(b.vt.screen.current_flags.bold());
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
fn inline_transcript_scrolls_into_history_above_a_fixed_composer() {
    let mut terminal = Terminal::new(5, 12).unwrap();
    terminal
        .feed(b"\x1b[4;1Hcomposer\x1b[5;1Hstatus\x1b[?2026h\x1b[1;3r\x1b[1;1H")
        .unwrap();
    terminal
        .feed(b"one\r\ntwo\r\nthree\r\nfour\r\nfive\x1b[r\x1b[?2026l")
        .unwrap();

    assert_eq!(terminal.history_page(0, 128).unwrap().text, "one\ntwo");
    let screen = terminal.visible_text();
    assert_eq!(
        screen.lines().map(str::trim_end).collect::<Vec<_>>(),
        ["three", "four", "five", "composer", "status"]
    );

    terminal.feed(b"\x1b[1;3r\x1b[S").unwrap();
    assert_eq!(terminal.history_rows(), 2);
}

#[test]
fn early_wide_wrap_keeps_copy_provenance_through_scrolling() {
    let mut terminal = Terminal::new(3, 6).unwrap();
    terminal.feed("abcde界Z".as_bytes()).unwrap();
    assert_eq!(terminal.live_margin_gaps(), 1);
    assert_eq!(terminal.frame().unwrap().rows()[0].cells[5].character, ' ');

    terminal.feed(b"\r\nx\r\ny\r\n").unwrap();
    assert_eq!(terminal.live_margin_gaps(), 0);
    let page = terminal.history_page(0, 2).unwrap();
    assert_eq!(page.synthetic_margin_gap(0), Some(5));
    assert_eq!(page.text, "abcde界Z");

    terminal.feed(b"\x1b[2J\x1b[H").unwrap();
    terminal.feed("abcde 界Z".as_bytes()).unwrap();
    assert_eq!(terminal.live_margin_gaps(), 0);
}

#[test]
fn projected_history_keeps_clusters_styles_links_and_raw_anchors_across_widths() {
    let mut terminal = Terminal::new(2, 6).unwrap();
    terminal.enable_grapheme_clusters();
    terminal.enable_underline_colors();
    terminal.enable_hyperlinks(|_, target| target == "https://example.test/history");
    terminal
        .feed("\x1b]8;;https://example.test/history\x07a\x1b[4;58;2;9;8;7me\u{301}\x1b[0mcde界Z\x1b]8;;\x07\r\nnext\r\n".as_bytes())
        .unwrap();
    let original = terminal.history_layout().unwrap();
    assert_eq!(original.len(), 2);
    let wide = original.anchor_at(1).unwrap();
    assert_eq!(wide.column, 0);
    assert_eq!(terminal.history_page(0, 2).unwrap().text, "ae\u{301}cde界Z");

    terminal.resize(2, 4).unwrap();
    let narrow = terminal.history_layout().unwrap();
    assert_eq!(narrow.row_at(wide), 1);
    let page = terminal
        .projected_history_viewport_page(&narrow, 0, 2)
        .unwrap();
    assert_eq!(page.text, "ae\u{301}cde界Z");
    assert_eq!(page.cluster_text(0, 1), Some("e\u{301}"));
    assert_eq!(page.underline_color(0, 1), Some(Color::Rgb(9, 8, 7)));
    let cells = &page.row(1).unwrap().cells;
    assert!(cells[1].flags.wide());
    assert!(cells[2].flags.wide_continuation());
    assert_eq!(
        page.hyperlink_target(cells[1].hyperlink_id),
        Some("https://example.test/history")
    );
    let overlap = terminal
        .projected_history_viewport_page(&narrow, 1, 1)
        .unwrap();
    assert!(Arc::ptr_eq(cells, &overlap.row(0).unwrap().cells));
    assert_eq!(overlap.text, "e界Z");
    assert_eq!(
        overlap.hyperlink_target(cells[1].hyperlink_id),
        page.hyperlink_target(cells[1].hyperlink_id)
    );
    assert!(terminal.projected_history_page(&original, 0, 2).is_err());

    terminal.resize(2, 8).unwrap();
    let wider = terminal.history_layout().unwrap();
    assert_eq!(wider.len(), 1);
    assert_eq!(wider.row_at(wide), 0);
    let widened = terminal
        .projected_history_viewport_page(&wider, 0, 1)
        .unwrap();
    assert!(terminal
        .projected_history_viewport_page(&narrow, 1, 1)
        .is_err());
    assert_eq!(widened.text, page.text);
    assert_eq!(widened.row(0).unwrap().cells.len(), 8);
    terminal.feed(b"\x1b[?1049h").unwrap();
    assert!(terminal.history_layout().unwrap().is_empty());
    assert!(terminal.projected_history_page(&wider, 0, 1).is_err());
    assert_eq!(page.text, "ae\u{301}cde界Z");
}

#[test]
fn projected_history_preserves_exact_width_wide_pairs_blank_lines_and_wraps() {
    let mut terminal = Terminal::new(2, 6).unwrap();
    terminal
        .feed("a界b界\r\n\r\nabcdefghi\r\nlast\r\n".as_bytes())
        .unwrap();

    for (columns, widths) in [(6, &[6, 0, 6, 3][..]), (4, &[4, 2, 0, 4, 4, 1][..])] {
        terminal.resize(2, columns).unwrap();
        let layout = terminal.history_layout().unwrap();
        let page = terminal
            .projected_history_page(&layout, 0, widths.len())
            .unwrap();

        assert_eq!(layout.len(), widths.len());
        assert_eq!(page.text, "a界b界\n\nabcdefghi");
        for (row, &cells) in widths.iter().enumerate() {
            assert_eq!(page.row(row).unwrap().cells.len(), cells);
        }
        let anchor = layout.anchor_at(0).unwrap();
        assert_eq!((anchor.row, anchor.column), (0, 0));
        assert!(page.row(0).unwrap().cells[2].flags.wide_continuation());
        let last_wide_row = usize::from(columns == 4);
        let last_wide = page.row(last_wide_row).unwrap();
        assert!(last_wide.cells.last().unwrap().flags.wide_continuation());
    }
}

#[test]
fn plain_projected_history_copies_clipped_fragments_and_link_ids() {
    let mut terminal = Terminal::new(2, 6).unwrap();
    terminal.enable_hyperlinks(|_, target| target == "https://example.test/plain");
    terminal
        .feed(b"\x1b]8;;https://example.test/plain\x07abcdefghij\x1b]8;;\x07\r\nnext\r\n")
        .unwrap();
    for (width, expected_rows) in [(4, 3), (8, 2)] {
        terminal.resize(2, width).unwrap();
        let layout = terminal.history_layout().unwrap();
        let page = terminal.projected_history_page(&layout, 0, 8).unwrap();
        assert_eq!(page.text, "abcdefghij");
        assert_eq!(page.row_count, expected_rows);
        for row in 0..page.row_count {
            for cell in page.row(row).unwrap().cells.iter() {
                assert_eq!(
                    page.hyperlink_target(cell.hyperlink_id),
                    Some("https://example.test/plain")
                );
            }
        }
    }
}

#[test]
fn exceptionally_narrow_history_projection_retains_access_to_raw_rows() {
    let mut terminal = Terminal::new(1, 256).unwrap();
    let mut line = "x".repeat(256);
    line.push_str("\r\n");
    for _ in 0..299 {
        terminal.feed(line.as_bytes()).unwrap();
    }
    terminal
        .feed("\x1b[31mé\x1b[0msecond\r\n".as_bytes())
        .unwrap();
    terminal.resize(1, 1).unwrap();
    let history = terminal.history_layout().unwrap();
    assert!(history.projected.is_none());
    assert_eq!(history.len(), 300);
    assert_eq!(history.anchor_at(299).unwrap().row, 299);
    assert_eq!(
        terminal
            .projected_history_page(&history, 0, 1)
            .unwrap()
            .text
            .len(),
        256
    );
    let previous = terminal
        .projected_history_viewport_page(&history, 298, 8)
        .unwrap();
    let tail = terminal
        .projected_history_viewport_page(&history, 299, 8)
        .unwrap();
    assert!(Arc::ptr_eq(
        &previous.row(1).unwrap().cells,
        &tail.row(0).unwrap().cells
    ));
    assert_eq!(tail.text, "ésecond");
    assert_eq!((tail.row_count, tail.total_rows, tail.offset), (1, 300, 0));
    assert_eq!(tail.row(0).unwrap().cells[0].foreground, Color::Red);
    assert_eq!(tail.revision, terminal.history_page(0, 1).unwrap().revision);
    let empty = terminal.projected_history_page(&history, 300, 8).unwrap();
    assert_eq!(empty.text, "");
    assert_eq!(
        (empty.row_count, empty.total_rows, empty.offset),
        (0, 300, 0)
    );
    assert!(terminal.projected_history_page(&history, 301, 8).is_err());
    assert!(terminal.projected_history_page(&history, 0, 0).is_err());
    terminal.set_history_row_limit(299);
    assert!(terminal.projected_history_page(&history, 299, 8).is_err());
}

#[test]
fn alternate_and_interior_margins_never_archive_or_expose_primary_history() {
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
fn private_ed3_clears_primary_history_without_erasing_visible_cells() {
    let sequence = b"\x1b[?3J";
    for split in 0..=sequence.len() {
        let mut terminal = Terminal::new(2, 4).unwrap();
        terminal.feed(b"old\r\nnext\r\n").unwrap();
        let retained = terminal.history_retained_bytes();
        assert!(retained > 0);
        terminal.feed(b"\x1b[?1049hALT").unwrap();
        terminal.feed(&sequence[..split]).unwrap();
        terminal.feed(&sequence[split..]).unwrap();
        assert_eq!(terminal.history_retained_bytes(), retained);
        terminal.feed(b"\x1b[?1049l").unwrap();
        assert_eq!(terminal.history_page(0, 128).unwrap().text, "old");

        terminal.feed("\x1b[1\"q中XQ".as_bytes()).unwrap();
        let history_end = terminal.history_extent().end;
        let before = terminal.frame().unwrap();
        assert!(before.cursor().pending_wrap);
        assert!(before.rows()[1].cells[0].flags.protected());
        terminal.feed(&sequence[..split]).unwrap();
        terminal.feed(&sequence[split..]).unwrap();
        assert_eq!(terminal.history_rows(), 0);
        assert_eq!(terminal.history_retained_bytes(), 0);
        assert_eq!(terminal.history_extent(), history_end..history_end);
        assert!(terminal.history_page(0, 128).unwrap().text.is_empty());
        let after = terminal.frame().unwrap();
        assert_eq!(after.rows(), before.rows());
        assert_eq!(after.cursor(), before.cursor());
        assert!(terminal.vt.screen.current_flags.protected());
        assert_eq!(terminal.unsupported_count(), 0);
    }
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
fn history_byte_limit_charges_decorations_and_preserves_absolute_row_ids() {
    let mut terminal = Terminal::new(1, MAX_COLS).unwrap();
    terminal.enable_underline_colors();
    terminal.enable_grapheme_clusters();
    let text = "e\u{301}".repeat(MAX_COLS - 4);
    for index in 0..MAX_HISTORY_ROWS {
        terminal
            .feed(format!("\x1b[4;58;2;7;8;9m{index:04}{text}\x1b[0m\r\n").as_bytes())
            .unwrap();
    }
    let extent = terminal.history_extent();
    assert_eq!(extent.end, MAX_HISTORY_ROWS as u64);
    assert!(extent.start > 0 && extent.start < extent.end);
    assert_eq!(
        terminal.history_retained_bytes(),
        terminal
            .history
            .iter()
            .map(ScrollbackLine::retained_bytes)
            .sum()
    );
    assert!(terminal.history_retained_bytes() <= crate::retained_history::RETAINED_HISTORY_BYTES);
    let last = terminal.history_page(0, 1).unwrap();
    assert_eq!(last.cluster_text(0, 4), Some("e\u{301}"));
    assert!(last.text.starts_with("1023"));

    let charged = terminal.history_retained_bytes();
    terminal.feed(b"\x1b[?1049h\x1b[3J").unwrap();
    assert_eq!(terminal.history_retained_bytes(), charged);
    assert_eq!(terminal.history_extent(), extent.end..extent.end);
    terminal.feed(b"\x1b[?1049l\x1b[3J").unwrap();
    assert_eq!(terminal.history_retained_bytes(), 0);
    assert_eq!(terminal.history_extent(), extent.end..extent.end);
    assert!(last.text.starts_with("1023"));
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
    terminal.set_history_row_limit(0);
    terminal.feed(b"disabled\r\n").unwrap();
    assert_eq!(terminal.history_extent(), 1026..1026);
    assert_eq!(terminal.history_retained_bytes(), 0);
    terminal.set_history_row_limit(2);
    terminal.feed(b"a\r\nb\r\nc\r\n").unwrap();
    assert_eq!(terminal.history_extent(), 1027..1029);
    assert_eq!(terminal.history_page(0, 2).unwrap().text, "b\nc");
}

#[test]
fn process_history_pressure_expires_the_original_anchor_without_reusing_its_row() {
    use crate::retained_history::HistoryBudget;

    let mut live = Terminal::new(1, 16).unwrap();
    let mut peer = Terminal::new(1, 16).unwrap();
    let mut calibration = Terminal::new(1, 16).unwrap();
    calibration.feed(b"same\r\n").unwrap();
    let one_row = calibration.history_retained_bytes();
    let budget = Arc::new(HistoryBudget::new(one_row * 2));
    live.history = RetainedHistory::with_process_budget(one_row * 4, Arc::clone(&budget));
    peer.history = live.history.empty_sibling();
    live.track_transcript_rows();
    peer.feed(b"peer\r\n").unwrap();
    live.feed(b"same\r\n").unwrap();
    let old_point = crate::history_reflow::HistoryAnchor { row: 0, column: 0 };
    let old = live.transcript_source_at(old_point).unwrap();
    let pinned_page = live.history_page(0, 1).unwrap();
    assert_eq!(budget.usage().retained_bytes, 2 * one_row);
    assert_eq!(live.locate_transcript_source(old), Some(old_point));

    live.feed(b"next\r\n").unwrap();
    assert_eq!(live.history_extent(), 1..2);
    assert_eq!(live.locate_transcript_source(old), None);
    assert_eq!(live.history_page(0, 1).unwrap().text, "next");
    assert_eq!(peer.history_page(0, 1).unwrap().text, "peer");
    assert_eq!(pinned_page.text, "same");
    assert_eq!(budget.usage().retained_bytes, 2 * one_row);
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
    assert_eq!(a.vt.screen.grid[1][0].foreground, crate::Color::Red);
    assert_eq!(a.vt.screen.grid[1][0].character, '世');
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
            assert_eq!(
                t.vt.screen.grid[row], baseline.vt.screen.grid[row],
                "cut={cut}"
            );
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
    assert!(t.vt.screen.grid[0][0].flags.wide());
    assert!(t.vt.screen.grid[0][1].flags.wide_continuation());
    t.feed(b"X").unwrap();
    assert_eq!(t.vt.screen.grid[1][0].character, 'X');
    t.feed(b"\x1b[1;2HA").unwrap();
    assert_eq!(t.vt.screen.grid[0][0].character, ' ');
    assert_eq!(t.vt.screen.grid[0][1].character, 'A');
    assert!(!t.vt.screen.grid[0][1].flags.wide_continuation());
    t.feed(b"\x1b[1;3HB").unwrap();
    assert_eq!(t.vt.screen.grid[0][3].character, ' ');
    assert!(!t.vt.screen.grid[0][3].flags.wide_continuation());
    t.feed("\x1b[2;4H界".as_bytes()).unwrap();
    assert_eq!(t.vt.screen.grid[2][0].character, '界');
    assert!(t.vt.screen.grid[2][1].flags.wide_continuation());
    t.feed("\x1b[?7l\x1b[2;4H中".as_bytes()).unwrap();
    assert_eq!(t.vt.screen.grid[1][2].character, '中');
    assert!(t.vt.screen.grid[1][3].flags.wide_continuation());
    t.frame().unwrap();
}

#[test]
fn narrow_resize_repairs_pairs_and_single_column_retains_core_truncated_lead() {
    let mut t = Terminal::new(2, 4).unwrap();
    t.feed("ab界".as_bytes()).unwrap();
    t.resize(2, 3).unwrap();
    assert_eq!(t.vt.screen.grid[0][2].character, ' ');
    assert!(!t.vt.screen.grid[0][2].flags.wide());
    t.resize(2, 1).unwrap();
    t.feed("\x1b[H界".as_bytes()).unwrap();
    assert_eq!(t.vt.screen.grid[1][0].character, '界');
    assert!(t.vt.screen.grid[1][0].flags.wide());
    assert!(!t.vt.screen.grid[1][0].flags.wide_continuation());
    assert_eq!(t.unsupported_count(), 0);
    t.frame().unwrap();
}

#[test]
fn combining_marks_are_explicitly_unsupported_without_cursor_or_base_damage() {
    let mut t = Terminal::new(2, 4).unwrap();
    t.feed("e\u{301}\u{200d}X".as_bytes()).unwrap();
    assert_eq!(t.vt.screen.grid[0][0].character, 'e');
    assert_eq!(t.vt.screen.grid[0][1].character, 'X');
    assert_eq!(t.vt.screen.cursor_col, 2);
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
            let cell = &t.vt.screen.grid[0][col];
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
    assert_eq!(t.vt.screen.grid[1][0].character, 'ﾞ');
    assert_eq!(t.vt.screen.grid[1][1].character, 'X');
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
        assert_eq!(t.vt.screen.grid[0][0].character, 'ﾟ', "cut={cut}");
        assert_eq!(t.vt.screen.grid[0][1].character, 'ﾞ', "cut={cut}");
        assert_eq!(t.vt.screen.cursor_col, 2);
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
fn frame_reuses_moved_rows_across_scroll_batches_and_recaptures_edited_rows() {
    let mut terminal = Terminal::new(6, 12).unwrap();
    terminal.enable_underline_colors();
    terminal.enable_grapheme_clusters();
    terminal.enable_hyperlinks(|_, _| true);
    terminal
        .feed(
            concat!(
                "A\r\n\x1b]8;;https://example.test/b\x07\x1b[4;58;5;33mBe\u{301}",
                "\x1b[0m\x1b]8;;\x07\r\nC\r\nD\r\nE\r\nF",
            )
            .as_bytes(),
        )
        .unwrap();
    let before = terminal.frame().unwrap();
    terminal.feed(b"\x1b[4;2HX\x1b[S").unwrap();
    terminal.feed(b"\x1b[2;5r\x1b[T\x1b[5;2HQ").unwrap();
    let after = terminal.frame().unwrap();

    assert_eq!(
        after
            .rows()
            .iter()
            .map(|row| row.cells[0].character)
            .collect::<String>(),
        "B CDE "
    );
    assert!(Arc::ptr_eq(&before.rows()[1].cells, &after.rows()[0].cells));
    assert!(Arc::ptr_eq(&before.rows()[2].cells, &after.rows()[2].cells));
    assert_eq!(after.rows()[3].cells[1].character, 'X');
    assert_eq!(after.rows()[4].cells[1].character, 'Q');
    assert_eq!(before.rows()[3].cells[1].character, ' ');
    assert_eq!(before.rows()[4].cells[1].character, ' ');
    assert_eq!(after.cluster_text(0, 1), Some("e\u{301}"));
    assert_eq!(after.underline_color(0, 0), Some(Color::Indexed(33)));
    assert!(Arc::ptr_eq(
        before.underline_row(1).unwrap(),
        after.underline_row(0).unwrap()
    ));
    assert!(Arc::ptr_eq(
        &before.shared_hyperlinks().unwrap(),
        &after.shared_hyperlinks().unwrap()
    ));
    let link = after.rows()[0].cells[0].hyperlink_id;
    assert_eq!(after.hyperlink_target(link), Some("https://example.test/b"));

    terminal.feed(b"\x1b[r\x1b[S").unwrap();
    let later = terminal.frame().unwrap();
    assert!(Arc::ptr_eq(&after.rows()[3].cells, &later.rows()[2].cells));
    assert_eq!(later.hyperlink_target(link), None);
    assert_eq!(after.hyperlink_target(link), Some("https://example.test/b"));
    let limits = Limits {
        max_rows: 6,
        max_cols: 12,
        max_cells: 72,
    };
    assert_eq!(
        after
            .apply(&after.delta_to(&later).unwrap(), limits)
            .unwrap(),
        later
    );
}

#[test]
fn synchronized_capture_becomes_the_source_for_subsequent_scrolling() {
    let mut terminal = Terminal::new(4, 8).unwrap();
    terminal.feed(b"A\r\nB\r\nC\r\nD").unwrap();
    let before = terminal.frame().unwrap();
    terminal.begin_update(true);
    terminal
        .feed(b"\x1b[?2026h\x1b[S\x1b[?2026l\x1b[S")
        .unwrap();
    let completed = terminal.take_completed_sync_frame().unwrap();
    let after = terminal.frame().unwrap();
    assert_eq!(completed.rows()[0].cells[0].character, 'B');
    assert_eq!(after.rows()[0].cells[0].character, 'C');
    assert!(Arc::ptr_eq(
        &before.rows()[1].cells,
        &completed.rows()[0].cells
    ));
    assert!(Arc::ptr_eq(
        &completed.rows()[1].cells,
        &after.rows()[0].cells
    ));
}

#[test]
fn counted_scrolling_matches_single_steps_and_preserves_transcript_identity() {
    for (alternate, inset, height) in [(false, false, 5), (true, false, 5), (false, true, 3)] {
        let seeded = || {
            let mut terminal = Terminal::new(5, 8).unwrap();
            terminal.track_transcript_rows();
            if alternate {
                terminal.feed(b"\x1b[?1049h").unwrap();
            }
            terminal
                .feed("1234567界Z\r\nC\r\nD\r\nE".as_bytes())
                .unwrap();
            if inset {
                terminal.feed(b"\x1b[2;4r").unwrap();
            }
            terminal.frame().unwrap();
            terminal
        };
        for command in ['S', 'T'] {
            for count in [2, 9] {
                let mut batched = seeded();
                let mut stepped = seeded();
                batched
                    .feed(format!("\x1b[{count}{command}").as_bytes())
                    .unwrap();
                for _ in 0..count.min(height) {
                    stepped.feed(format!("\x1b[{command}").as_bytes()).unwrap();
                }
                assert_eq!(
                    batched.vt.screen.grid.row_ids(),
                    stepped.vt.screen.grid.row_ids()
                );
                assert_eq!(
                    batched.frame().unwrap().rows(),
                    stepped.frame().unwrap().rows()
                );
                let batch = batched.history_page(0, 8).unwrap();
                let steps = stepped.history_page(0, 8).unwrap();
                assert_eq!(batch.row_count, steps.row_count);
                assert_eq!(batch.text, steps.text);
                for row in 0..batch.row_count {
                    assert_eq!(
                        batch.synthetic_margin_gap(row),
                        steps.synthetic_margin_gap(row)
                    );
                }
            }
        }
    }
}

#[test]
fn modify_other_keys_query_tracks_the_encoder_without_painting() {
    for device_answers in [false, true] {
        for chunk in 1..=8 {
            let mut terminal = Terminal::new(2, 12).unwrap();
            if device_answers {
                terminal.enable_device_answers();
            }
            terminal.feed(b"\x1b[31;44;1mX").unwrap();
            let before = terminal.frame().unwrap();
            let rendition = terminal.vt.screen.current_flags;
            for (level, effective) in [(0, 0), (1, 1), (2, 2), (65535, 2), (0, 0)] {
                let stream = format!("\x1b[>4;{level}m\x1b[?04m");
                let mut reply = Vec::new();
                for bytes in stream.as_bytes().chunks(chunk) {
                    reply.extend(terminal.feed(bytes).unwrap());
                }
                assert_eq!(reply, format!("\x1b[>4;{effective}m").as_bytes());
                assert_eq!(terminal.frame().unwrap(), before);
                assert_eq!(terminal.vt.screen.current_flags, rendition);
                let encoded = terminal.encode_key("a", false, false, true).unwrap();
                assert_eq!(
                    encoded,
                    if effective == 2 {
                        b"\x1b[27;5;97~".as_slice()
                    } else {
                        b"\x01"
                    }
                );
            }
        }
    }
}

#[test]
fn hosted_character_size_queries_follow_the_current_grid() {
    for enable in [
        Terminal::enable_window_updates,
        Terminal::enable_device_answers,
    ] {
        let mut terminal = Terminal::new(8, 21).unwrap();
        assert!(terminal.feed(b"\x1b[18t").unwrap().is_empty());
        enable(&mut terminal);
        let before = terminal.frame().unwrap().stamp();
        let query = b"\x1b[18t";
        for cut in 0..query.len() {
            assert!(terminal.feed(&query[..cut]).unwrap().is_empty());
            assert_eq!(terminal.feed(&query[cut..]).unwrap(), b"\x1b[8;8;21t");
        }
        assert_eq!(
            terminal.feed(b"\x1b[018t\x1b[6n").unwrap(),
            b"\x1b[8;8;21t\x1b[1;1R",
        );
        assert!(terminal
            .feed(b"\x1b[?18t\x1b[18:t\x1b[18;t\x1b[18$t")
            .unwrap()
            .is_empty());
        assert_eq!(terminal.frame().unwrap().stamp(), before);

        terminal.resize(11, 95).unwrap();
        terminal.feed(b"\x1b[?1049h").unwrap();
        assert_eq!(terminal.feed(query).unwrap(), b"\x1b[8;11;95t");
    }
}

#[test]
fn hosted_pixel_queries_use_current_frontend_measurements() {
    let mut terminal = Terminal::new(24, 80).unwrap();
    terminal.enable_device_answers();
    let query = b"\x1b[14t\x1b[16t";
    assert!(terminal.feed(query).unwrap().is_empty());

    let before = terminal.frame().unwrap().stamp();
    let viewport = NativeWindowPixels::Viewport {
        width: 1920,
        height: 1152,
    };
    terminal.set_window_pixels(24, 80, viewport).unwrap();
    let expected = b"\x1b[4;1152;1920t\x1b[6;48;24t";
    assert_eq!(terminal.feed(query).unwrap(), expected);
    assert_eq!(terminal.frame().unwrap().stamp(), before);
    assert!(terminal
        .resize_with_window_pixels(12, 84, Some(viewport), || Err("pty error".into()))
        .is_err());
    assert_eq!(terminal.feed(query).unwrap(), expected);
    let empty = NativeWindowPixels::Viewport {
        width: 0,
        height: 576,
    };
    assert!(terminal
        .resize_with_window_pixels(12, 84, Some(empty), || panic!(
            "invalid pixels reached the PTY"
        ))
        .is_err());
    assert_eq!(terminal.feed(query).unwrap(), expected);

    let scale_one = NativeWindowPixels::Viewport {
        width: 960,
        height: 576,
    };
    terminal
        .resize_with_window_pixels(24, 80, Some(scale_one), || Ok(()))
        .unwrap();
    assert_eq!(
        terminal.feed(query).unwrap(),
        b"\x1b[4;576;960t\x1b[6;24;12t"
    );
    assert_eq!(terminal.frame().unwrap().stamp(), before);

    terminal.resize(12, 84).unwrap();
    assert!(terminal.feed(query).unwrap().is_empty());
    assert!(terminal.set_window_pixels(24, 80, viewport).is_err());
    let resized = NativeWindowPixels::Viewport {
        width: 1008,
        height: 288,
    };
    terminal
        .resize_with_window_pixels(12, 84, Some(resized), || Ok(()))
        .unwrap();
    terminal.feed(b"\x1b[?1049h").unwrap();
    assert_eq!(
        terminal.feed(query).unwrap(),
        b"\x1b[4;288;1008t\x1b[6;24;12t",
    );
}

#[test]
fn nonpaint_output_keeps_the_frame_revision_and_the_next_delta_base() {
    let mut terminal = Terminal::new(3, 20).unwrap();
    terminal.enable_device_answers();
    terminal.enable_window_updates();
    terminal.set_color_defaults(TerminalDefaultColors::default());
    let first = terminal.frame().unwrap();

    // Fragmented queries advance parsing and produce replies, but do not paint.
    assert!(terminal.feed(b"\x1b[").unwrap().is_empty());
    assert_eq!(terminal.frame().unwrap().stamp(), first.stamp());
    assert_eq!(terminal.feed(b"6n\x1b[c").unwrap(), b"\x1b[1;1R\x1b[?1;2c");
    assert_eq!(terminal.frame().unwrap().stamp(), first.stamp());

    terminal
        .feed(b"\x1b[?1004h\x1b]2;new title\x07\x07")
        .unwrap();
    assert_eq!(terminal.encode_focus(true), Some(b"\x1b[I".as_slice()));
    let effects = terminal.take_window_updates();
    assert!(effects.bell);
    assert_eq!(effects.title.as_deref(), Some("new title"));
    assert_eq!(terminal.frame().unwrap().stamp(), first.stamp());

    terminal.feed(b"X").unwrap();
    let edited = terminal.frame().unwrap();
    let delta = first.delta_to(&edited).unwrap();
    assert_eq!(delta.rows.len(), 1);
    assert_eq!(delta.rows[0].1.cells[0].character, 'X');

    // Theme and history consumers need a new observation even when the
    // visible cell rows still use named colors or are all blank.
    terminal.feed(b"\x1b]10;#102030\x07").unwrap();
    let recolored = terminal.frame().unwrap();
    assert!(recolored.stamp().revision > edited.stamp().revision);
    assert!(Arc::ptr_eq(&edited.shared_rows(), &recolored.shared_rows()));
    terminal.feed(b"\x1b[2J\x1b[3;1H").unwrap();
    let blank = terminal.frame().unwrap();
    terminal.feed(b"\n").unwrap();
    let scrolled = terminal.frame().unwrap();
    assert_eq!(blank.rows(), scrolled.rows());
    assert_eq!(blank.cursor(), scrolled.cursor());
    assert!(scrolled.stamp().revision > blank.stamp().revision);
    assert_eq!(terminal.history_rows(), 1);
}

#[test]
fn visible_hyperlink_resources_follow_cells_across_row_edits_and_screens() {
    let mut terminal = Terminal::new(3, 20).unwrap();
    terminal.enable_hyperlinks(|_, target| target.starts_with("https://"));
    terminal.frame().unwrap();
    terminal
        .feed(b"\x1b]8;;https://example.com/one\x07A\x1b]8;;\x07\r\n\x1b]8;;https://example.com/two\x07B\x1b]8;;\x07")
        .unwrap();
    let initial = terminal.frame().unwrap();
    let one = initial.rows()[0].cells[0].hyperlink_id;
    let two = initial.rows()[1].cells[0].hyperlink_id;
    assert_eq!(
        initial.hyperlink_target(one),
        Some("https://example.com/one")
    );
    assert_eq!(
        initial.hyperlink_target(two),
        Some("https://example.com/two")
    );

    terminal.feed(b"\x1b[1;3Htext").unwrap();
    let edited = terminal.frame().unwrap();
    assert_eq!(edited.rows()[0].cells[2].character, 't');
    assert!(Arc::ptr_eq(
        &initial.shared_hyperlinks().unwrap(),
        &edited.shared_hyperlinks().unwrap()
    ));

    terminal
        .feed(b"\x1b[1;2H\x1b]8;;https://example.com/three\x07C\x1b]8;;\x07")
        .unwrap();
    let added = terminal.frame().unwrap();
    let three = added.rows()[0].cells[1].hyperlink_id;
    assert_eq!(
        added.hyperlink_target(three),
        Some("https://example.com/three")
    );
    assert_eq!(edited.hyperlink_target(three), None);

    terminal.feed(b"\x1b[H\x1b[2K").unwrap();
    let erased = terminal.frame().unwrap();
    assert_eq!(erased.hyperlink_target(one), None);
    assert_eq!(erased.hyperlink_target(three), None);
    assert_eq!(
        erased.hyperlink_target(two),
        Some("https://example.com/two")
    );
    terminal.feed(b"\x1b[?1049h").unwrap();
    assert_eq!(terminal.frame().unwrap().hyperlink_target(two), None);

    terminal.feed(b"\x1b[?1049l\x1b[S").unwrap();
    let scrolled = terminal.frame().unwrap();
    assert_eq!(scrolled.rows()[0].cells[0].hyperlink_id, two);
    assert_eq!(
        scrolled.hyperlink_target(two),
        Some("https://example.com/two")
    );
    assert_eq!(
        initial.hyperlink_target(one),
        Some("https://example.com/one")
    );
}

#[test]
fn repeated_resize_preserves_grapheme_composition_and_frame() {
    for (base, suffix, cluster) in [("e", "\u{301}", "e\u{301}"), ("❤", "\u{fe0f}", "❤\u{fe0f}")]
    {
        for fail in [false, true] {
            let mut terminal = Terminal::new(4, 8).unwrap();
            terminal.enable_grapheme_clusters();
            terminal.feed(base.as_bytes()).unwrap();
            let before = terminal.frame().unwrap();
            let mut calls = 0;
            let result = terminal.resize_with(4, 8, || {
                calls += 1;
                if fail {
                    Err("resize refused".into())
                } else {
                    Ok(())
                }
            });
            assert_eq!(calls, 1);
            assert_eq!(result.is_err(), fail);
            assert_eq!(terminal.frame().unwrap(), before);
            terminal.feed(suffix.as_bytes()).unwrap();
            let frame = terminal.frame().unwrap();
            assert_eq!(frame.cluster_text(0, 0), Some(cluster));
            assert_eq!(terminal.unsupported_count(), 0);
        }
    }
    let mut terminal = Terminal::new(4, 8).unwrap();
    terminal.enable_grapheme_clusters();
    terminal.feed(b"e").unwrap();
    terminal.resize(4, 7).unwrap();
    terminal.feed("\u{301}".as_bytes()).unwrap();
    assert_eq!(terminal.frame().unwrap().cluster_text(0, 0), None);
    assert_eq!(terminal.unsupported_count(), 1);
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
fn primary_reflow_keeps_a_live_shell_line_and_insertion_point_on_split() {
    let mut terminal = Terminal::new(6, 46).unwrap();
    terminal.enable_primary_reflow();
    let line = "new-tab-dir=[/some/path/with a space]";
    terminal.feed(format!("{line}\r\n$ ").as_bytes()).unwrap();
    let start = HistoryAnchor { row: 0, column: 0 };
    let before = terminal
        .primary_text_range(start, terminal.primary_cursor_anchor().unwrap())
        .unwrap();

    terminal.resize(6, 25).unwrap();
    assert!(terminal.frame().unwrap().rows()[0].wrapped);
    assert_eq!(terminal.primary_cursor_anchor().unwrap().row, 2);
    assert_eq!(
        terminal
            .primary_text_range(start, terminal.primary_cursor_anchor().unwrap())
            .unwrap(),
        before
    );
    terminal.resize(6, 46).unwrap();
    assert!(!terminal.frame().unwrap().rows()[0].wrapped);
    assert_eq!(terminal.primary_cursor_anchor().unwrap().row, 1);
    terminal.feed(b"next").unwrap();
    assert_eq!(
        terminal
            .primary_text_range(start, terminal.primary_cursor_anchor().unwrap())
            .unwrap(),
        format!("{line}\n$ next")
    );
}

#[test]
fn primary_reflow_preserves_wide_pairs_clusters_and_decorations_through_one_column() {
    let mut terminal = Terminal::new(20, 16).unwrap();
    terminal.enable_primary_reflow();
    terminal.enable_grapheme_clusters();
    terminal.enable_underline_colors();
    terminal
        .feed("abc\x1b[4;5;58;2;20;30;40m界e\u{301}\x1b[0m ❤\u{fe0f} end".as_bytes())
        .unwrap();
    let start = HistoryAnchor { row: 0, column: 0 };
    let expected = "abc界e\u{301} ❤\u{fe0f} end";

    terminal.resize(20, 4).unwrap();
    let frame = terminal.frame().unwrap();
    assert_eq!(terminal.live_margin_gaps() & 1, 1);
    assert!(frame.rows()[0].wrapped);
    assert!(frame.rows()[1].cells[0].flags.wide());
    assert!(frame.rows()[1].cells[1].flags.wide_continuation());
    assert!(frame.rows()[1].cells[0].flags.blink());
    assert_eq!(frame.underline_color(1, 0), Some(Color::Rgb(20, 30, 40)));
    assert_eq!(frame.cluster_text(1, 2), Some("e\u{301}"));

    for width in [1, 16, 5, 16] {
        terminal.resize(20, width).unwrap();
        assert_eq!(
            terminal
                .primary_text_range(start, terminal.primary_cursor_anchor().unwrap())
                .unwrap(),
            expected
        );
        for row in terminal.frame().unwrap().rows() {
            for (column, _) in row
                .cells
                .iter()
                .enumerate()
                .filter(|(_, cell)| cell.flags.wide())
            {
                assert!(row
                    .cells
                    .get(column + 1)
                    .is_some_and(|next| next.flags.wide_continuation()));
            }
        }
    }
    let frame = terminal.frame().unwrap();
    assert!(frame.rows()[0].cells[3].flags.wide());
    assert_eq!(frame.underline_color(0, 3), Some(Color::Rgb(20, 30, 40)));
    assert_eq!(frame.cluster_text(0, 5), Some("e\u{301}"));
}

#[test]
fn primary_reflow_archives_overflow_in_order_without_moving_existing_history() {
    let mut terminal = Terminal::new(3, 12).unwrap();
    terminal.enable_primary_reflow();
    terminal.track_transcript_rows();
    let archived_origin = terminal.transcript_cursor().unwrap();
    terminal
        .feed(b"retained\r\nabcdefghi\r\nsecond\r\n$ ")
        .unwrap();
    let prior = terminal.locate_transcript_source(archived_origin).unwrap();
    let before = terminal.history_extent();
    assert!(before.contains(&prior.row));
    let expected = "retained\nabcdefghi\nsecond\n$ ";

    for width in [4, 12, 3, 12] {
        terminal.resize(3, width).unwrap();
        assert_eq!(
            terminal.locate_transcript_source(archived_origin),
            Some(prior)
        );
        assert_eq!(
            terminal
                .primary_text_range(prior, terminal.primary_cursor_anchor().unwrap())
                .unwrap(),
            expected
        );
    }
    assert!(terminal.history_extent().end > before.end);
}

#[test]
fn primary_reflow_remaps_saved_cursor_and_preserves_the_hidden_primary_screen() {
    let mut terminal = Terminal::new(8, 20).unwrap();
    terminal.enable_primary_reflow();
    terminal.feed(b"abcdefghij\x1b[1;7H\x1b7\r\n$ ").unwrap();
    terminal.resize(8, 4).unwrap();
    terminal.feed(b"\x1b8!\r\n\x1b[6;1H$ ").unwrap();
    let start = HistoryAnchor { row: 0, column: 0 };
    let expected = terminal
        .primary_text_range(start, terminal.primary_cursor_anchor().unwrap())
        .unwrap();
    assert!(expected.starts_with("abcdef!hij\n"));

    terminal.resize(8, 20).unwrap();
    terminal.feed(b"\x1b[?1049hNORMALGRID").unwrap();
    terminal.resize(8, 5).unwrap();
    let alt = terminal.frame().unwrap();
    assert_eq!(
        alt.rows()[0]
            .cells
            .iter()
            .map(|cell| cell.character)
            .collect::<String>(),
        "NORMA"
    );
    assert!(!alt.rows()[0].wrapped);
    terminal.feed(b"\x1b[?1049l").unwrap();
    assert_eq!(
        terminal
            .primary_text_range(start, terminal.primary_cursor_anchor().unwrap())
            .unwrap(),
        expected
    );
}

#[test]
fn primary_reflow_preserves_virtual_cursor_space_and_deferred_wrap() {
    let mut terminal = Terminal::new(4, 10).unwrap();
    terminal.enable_primary_reflow();
    terminal.feed(b"1234\x1b[7G").unwrap();
    terminal.resize(4, 3).unwrap();
    assert_eq!(
        terminal.primary_cursor_anchor().unwrap(),
        HistoryAnchor { row: 1, column: 3 }
    );
    terminal.feed(b"!").unwrap();
    assert_eq!(
        terminal.primary_cursor_anchor().unwrap(),
        HistoryAnchor { row: 2, column: 1 }
    );
    terminal.resize(4, 10).unwrap();
    assert_eq!(
        terminal.primary_cursor_anchor().unwrap(),
        HistoryAnchor { row: 0, column: 7 }
    );
    assert_eq!(
        terminal
            .primary_text_range(
                HistoryAnchor { row: 0, column: 0 },
                terminal.primary_cursor_anchor().unwrap()
            )
            .unwrap(),
        "1234  !"
    );
}

#[test]
fn retained_text_keeps_blank_base_clusters_below_the_cursor() {
    let mut terminal = Terminal::new(4, 8).unwrap();
    terminal.enable_grapheme_clusters();
    terminal.feed("\x1b[3;1H \u{301}\x1b[H".as_bytes()).unwrap();
    assert_eq!(
        terminal.frame().unwrap().cluster_text(2, 0),
        Some(" \u{301}")
    );
    let mut output = Vec::new();
    terminal.write_retained_text(&mut output).unwrap();
    assert_eq!(String::from_utf8(output).unwrap(), "\n\n \u{301}");
}

#[test]
fn retained_text_joins_plain_archive_padding_to_encoded_and_live_rows() {
    let mut terminal = Terminal::new(2, 6).unwrap();
    terminal
        .feed("ab    \x1b[31mc\x1b[0m\r\né\r\nlast".as_bytes())
        .unwrap();
    assert!(matches!(
        terminal.history[0].content(),
        crate::grid::ScrollbackContent::Plain(_, _)
    ));
    assert!(terminal.history[0].is_wrapped);
    assert!(matches!(
        terminal.history[1].content(),
        crate::grid::ScrollbackContent::Encoded(_)
    ));
    let mut output = Vec::new();
    terminal.write_retained_text(&mut output).unwrap();
    assert_eq!(String::from_utf8(output).unwrap(), "ab    c\né\nlast");
}

#[test]
fn history_search_text_matches_selection_at_every_archive_boundary() {
    for track_spaces in [false, true] {
        let mut terminal = Terminal::new(3, 6).unwrap();
        terminal.enable_grapheme_clusters();
        terminal.enable_hyperlinks(|_, _| true);
        if track_spaces {
            terminal.track_printed_spaces();
        }
        terminal
            .feed(
                concat!(
                    "ab    \x1b[31mC\x1b[0m\r\n\r\n",
                    "abcde界Z\r\n",
                    " \u{301}\r\n",
                    "\x1b]8;;https://example.test\x07e\u{301}\x1b]8;;\x07  \r\n",
                    "blank!  \r\nend\x1b[3S",
                )
                .as_bytes(),
            )
            .unwrap();
        terminal.resize(3, 12).unwrap();
        terminal.feed(b"new width\x1b[3S").unwrap();
        let total = terminal.history_rows();
        for end in 1..=total {
            for start in 0..end {
                let following_row = end < total;
                let sentinel = usize::from(following_row);
                let count = end - start;
                let page = terminal
                    .history_page(total - end - sentinel, count + sentinel)
                    .unwrap();
                let expected = if following_row {
                    page.selection_text(0, 0, count, 0).unwrap()
                } else {
                    page.text
                };
                let bytes = expected.len();
                assert_eq!(
                    terminal.history_search_text(start..end, following_row, bytes),
                    Some((page.revision, expected)),
                    "printed spaces {track_spaces}, range {start}..{end}"
                );
                if bytes > 0 {
                    assert!(terminal
                        .history_search_text(start..end, following_row, bytes - 1)
                        .is_none());
                }
            }
        }
    }
}

#[test]
fn archive_text_keeps_clusters_on_blanks_omitted_by_both_codecs() {
    for (styled, start) in [(false, "a"), (true, "\x1b[31ma\x1b[0m")] {
        let mut terminal = Terminal::new(2, 8).unwrap();
        terminal.enable_grapheme_clusters();
        terminal
            .feed(format!("{start}\x1b[6G \u{301}\r\nnext\r\n").as_bytes())
            .unwrap();
        let archived = &terminal.history[0];
        assert_eq!(
            matches!(
                archived.content(),
                crate::grid::ScrollbackContent::Encoded(_)
            ),
            styled
        );
        let mut visited = 0;
        archived.visit_search_cells(|_, _| visited += 1);
        assert_eq!(visited, 1);
        assert_eq!(terminal.history_page(0, 1).unwrap().text, "a     \u{301}");
        let (_, search) = terminal.history_search_text(0..1, false, 16).unwrap();
        assert_eq!(search, "a     \u{301}");
        let mut export = Vec::new();
        terminal.write_retained_text(&mut export).unwrap();
        assert_eq!(String::from_utf8(export).unwrap(), "a     \u{301}\nnext\n");
    }
}

#[test]
fn streamed_archive_runs_keep_combining_marks_and_written_spaces() {
    let mut terminal = Terminal::new(2, 8).unwrap();
    terminal.enable_grapheme_clusters();
    terminal.track_printed_spaces();
    terminal
        .feed("\x1b[31meee\u{301}e  \x1b[m\r\nnext\r\nlive".as_bytes())
        .unwrap();
    let archived = "eee\u{301}e  ";
    assert_eq!(terminal.history_page(0, 1).unwrap().text, archived);
    let (_, search) = terminal
        .history_search_text(0..1, false, archived.len())
        .unwrap();
    assert_eq!(search, archived);
    let mut output = Vec::new();
    terminal.write_retained_text(&mut output).unwrap();
    assert_eq!(output, format!("{archived}\nnext\nlive").as_bytes());
}

#[test]
fn compact_history_exports_trailing_written_spaces() {
    let mut terminal = Terminal::new(2, 8).unwrap();
    terminal.track_printed_spaces();
    terminal.feed("é a  \r\nnext\r\nlive".as_bytes()).unwrap();
    assert_eq!(terminal.history_rows(), 1);
    assert!(matches!(
        terminal.history[0].content(),
        crate::grid::ScrollbackContent::Plain("é a  ", 3)
    ));
    assert_eq!(terminal.history_page(0, 1).unwrap().text, "é a  ");
    let (_, searched) = terminal.history_search_text(0..1, false, 8).unwrap();
    assert_eq!(searched, "é a  ");
    let mut output = Vec::new();
    terminal.write_retained_text(&mut output).unwrap();
    assert_eq!(output, "é a  \nnext\nlive".as_bytes());
}

#[test]
fn retained_text_copies_archived_and_live_rows_with_exact_logical_boundaries() {
    let mut terminal = Terminal::new(3, 12).unwrap();
    terminal.track_printed_spaces();
    terminal.enable_grapheme_clusters();
    let archive = (0..140)
        .map(|row| format!("archive-{row:03}\r\n"))
        .collect::<String>();
    terminal.feed(archive.as_bytes()).unwrap();
    terminal
        .feed("12345678901界e\u{301}  \r\nlive".as_bytes())
        .unwrap();
    assert!(terminal.history_rows() > 128);

    let copied = |terminal: &Terminal| {
        let mut output = Vec::new();
        terminal.write_retained_text(&mut output).unwrap();
        String::from_utf8(output).unwrap()
    };
    let primary = archive.replace("\r\n", "\n") + "12345678901界e\u{301}  \nlive";
    assert_eq!(copied(&terminal), primary);
    let mut bounded = [0; 13];
    assert_eq!(
        terminal
            .write_retained_text(&mut &mut bounded[..])
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::WriteZero
    );
    assert_eq!(&bounded, b"archive-000\na");

    terminal.feed(b"\x1b[?1049hfullscreen").unwrap();
    assert_eq!(copied(&terminal), "fullscreen");
    terminal.feed(b"\x1b[?1049l").unwrap();
    assert_eq!(copied(&terminal), primary);
}

#[test]
fn native_height_shrink_retains_primary_rows_and_keeps_the_live_prompt() {
    let mut terminal = Terminal::new(5, 12).unwrap();
    terminal.enable_history_preserving_resize();
    terminal.enable_underline_colors();
    terminal.enable_grapheme_clusters();
    terminal
        .feed("\x1b[4;58;2;20;30;40me\u{301}\x1b[0m\r\ntwo\r\nthree\r\nfour\r\nprompt".as_bytes())
        .unwrap();
    terminal.resize(3, 9).unwrap();
    assert_eq!(terminal.history_extent(), 0..2);
    let page = terminal.history_page(0, 2).unwrap();
    assert_eq!(page.text, "e\u{301}\ntwo");
    assert_eq!(page.row(0).unwrap().cells.len(), 12);
    assert_eq!(
        page.underline_color(0, 0),
        Some(crate::Color::Rgb(20, 30, 40))
    );
    let frame = terminal.frame().unwrap();
    assert_eq!(frame.cursor().row, 2);
    assert_eq!(frame.rows()[2].cells[0].character, 'p');

    terminal.resize(5, 9).unwrap();
    terminal.resize(3, 9).unwrap();
    assert_eq!(terminal.history_extent(), 0..2);
    terminal.feed(b"\x1b[?1049h\x1b[3;1HALT").unwrap();
    terminal.resize(2, 9).unwrap();
    assert_eq!(terminal.history_extent(), 2..2);
    terminal.feed(b"\x1b[?1049l").unwrap();
    assert_eq!(terminal.history_extent(), 0..2);
}

#[test]
fn height_shrink_keeps_the_saved_prompt_origin_on_its_live_row() {
    let mut terminal = Terminal::new(6, 12).unwrap();
    terminal.enable_history_preserving_resize();
    terminal
        .feed(b"one\r\ntwo\r\noutput\r\n\x1b7prompt\r\ndraft\r\nstatus")
        .unwrap();
    terminal.resize(4, 9).unwrap();
    assert_eq!(terminal.history_extent(), 0..2);

    terminal.feed(b"\x1b8\x1b[Jrepaint").unwrap();
    let frame = terminal.frame().unwrap();
    assert_eq!(frame.rows()[0].cells[0].character, 'o');
    assert_eq!(frame.rows()[1].cells[0].character, 'r');
    assert_eq!(frame.rows()[2].cells[0].character, ' ');
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
    assert_eq!(t.vt.screen.grid[0][0].character, 'd');
    assert_eq!(t.vt.screen.grid[1][0].character, 'g');
    assert!(t.feed(&vec![b'a'; MAX_FEED_BYTES + 1]).is_err());
    assert!(Terminal::new(200, 30).is_err());
}

#[test]
fn reply_budget_preserves_the_admitted_prefix_and_parses_the_rest_of_the_span() {
    let mut terminal = Terminal::new(2, 20).unwrap();
    const QUERY: &[u8] = b"\x1bP+q\x1b\\";
    const REPLY: &[u8] = b"\x1bP0+r\x1b\\";
    const STATUS: &[u8] = b"\x1b[0n";
    let admitted = (MAX_FEED_BYTES - STATUS.len()) / REPLY.len();
    let mut input = b"\x1b[5n".to_vec();
    input.extend(QUERY.repeat(admitted + 2));
    // This short reply would still fit, but it cannot jump over a refusal.
    input.extend_from_slice(b"\x1b[5nexit-tail\x1b[");
    let result = terminal.feed_bounded_at(&input, Instant::now()).unwrap();

    assert_eq!(&result.bytes[..STATUS.len()], STATUS);
    assert_eq!(result.bytes[STATUS.len()..], REPLY.repeat(admitted));
    assert_eq!(result.refused_bytes, REPLY.len() * 2 + STATUS.len());
    assert!(terminal.visible_text().starts_with("exit-tail"));
    assert_eq!(terminal.feed(b"5n").unwrap(), STATUS);

    let mut legacy = Terminal::new(2, 20).unwrap();
    assert_eq!(legacy.feed(&input).unwrap_err(), "reply limit");
    assert!(legacy.visible_text().starts_with("exit-tail"));
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
    t.vt.sync_output.started =
        Some(std::time::Instant::now() - std::time::Duration::from_millis(251));
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
            assert_eq!(terminal.vt.parser.rejected_count(), 1);
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
                assert_eq!(terminal.vt.parser.rejected_count(), u64::from(oversize));
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
                assert_eq!(terminal.vt.parser.rejected_count(), 0);
            } else {
                assert!(replies.is_empty());
                assert_eq!(terminal.vt.parser.rejected_count(), 1);
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
        assert_eq!(terminal.vt.parser.rejected_count(), round);
        assert!(terminal.feed(b"\x07OK").unwrap().is_empty());
    }
    assert!(terminal.visible_text().starts_with("SAFEOKOK"));
    assert_eq!(
        terminal.feed(b"\x1bP+q544e\x1b\\").unwrap(),
        b"\x1bP0+r\x1b\\"
    );
}

#[test]
fn bounded_history_text_counts_committed_bytes_and_defers_soft_wrap_padding() {
    let row = |text: &str, wrapped| HistoryCopyRow {
        row: Row {
            cells: text
                .chars()
                .map(|character| crate::TerminalCell {
                    character,
                    ..Default::default()
                })
                .collect(),
            wrapped,
        },
        underline_colors: None,
        annotations: None,
        hyperlink_ids: None,
        synthetic_gap: None,
    };
    let copy = |rows: &[HistoryCopyRow], limit| {
        extract_selection_bounded::<true, true>(
            rows,
            (0, 0),
            (rows.len(), 0),
            false,
            None,
            Some(limit),
        )
        .unwrap()
    };
    let mut padded = [row("🐍   ", true), row("   ", false)];
    assert_eq!(copy(&padded, 4).as_deref(), Some("🐍"));
    assert_eq!(copy(&padded, 3), None);
    assert_eq!(copy(&padded[..1], 4), None);

    let mut terminal = Terminal::new(2, 4).unwrap();
    terminal.track_printed_spaces();
    terminal.feed(b" ").unwrap();
    Arc::make_mut(&mut padded[0].row.cells)[2] = terminal.frame().unwrap().rows()[0].cells[0];
    assert_eq!(copy(&padded, 4), None);
    assert_eq!(copy(&padded, 6).as_deref(), Some("🐍  "));

    let separated = [row("🐍", false), row("", false)];
    assert_eq!(copy(&separated, 4), None);
    assert_eq!(copy(&separated, 5).as_deref(), Some("🐍\n"));
}

#[test]
fn immutable_history_copy_boundaries_use_utf16_and_never_select_padding() {
    use crate::history_reflow::HistoryAnchor;
    let mut terminal = Terminal::new(2, 6).unwrap();
    terminal.enable_grapheme_clusters();
    terminal
        .feed("🐍e\u{301}\r\n中\r\n\r\n\r\n".as_bytes())
        .unwrap();
    let page = terminal.history_page(0, 8).unwrap();
    assert_eq!(page.text, "🐍e\u{301}\n中\n");
    let offsets = page.copy_offsets_utf16().unwrap();
    assert_eq!(
        offsets,
        [
            vec![0, 2, 2, 4, 4, 4, 4],
            vec![5, 6, 6, 6, 6, 6, 6],
            vec![7; 7]
        ]
    );
    assert!(terminal.retains_history_anchor(HistoryAnchor { row: 0, column: 2 }));
    terminal.feed(b"\x1b[3J").unwrap();
    assert!(!terminal.retains_history_anchor(HistoryAnchor { row: 0, column: 2 }));
    assert_eq!(page.copy_offsets_utf16().unwrap(), offsets);
}

#[test]
fn offset_only_history_copy_matches_materialized_utf16_boundaries() {
    let mut terminal = Terminal::new(2, 6).unwrap();
    terminal.enable_grapheme_clusters();
    terminal.track_printed_spaces();
    terminal
        .feed(
            "abcde界Z\r\n🧪e\u{301}  \r\n\x1b[44m\x1b[2K\x1b[0m\r\n\r\nlast\r\nmore\r\n".as_bytes(),
        )
        .unwrap();
    let raw = terminal.history_page(0, 8).unwrap();
    assert_eq!(raw.synthetic_margin_gap(0), Some(5));

    terminal.resize(2, 4).unwrap();
    let layout = terminal.history_layout().unwrap();
    let projected = terminal
        .projected_history_page(&layout, 0, layout.len())
        .unwrap();
    for page in [raw, projected] {
        let mut offsets = Vec::new();
        let text = extract_selection::<false, false>(
            &page.rows,
            (0, 0),
            (page.row_count, 0),
            false,
            Some(&mut offsets),
        )
        .unwrap();
        assert!(text.is_empty());
        assert_eq!(offsets, page.copy_offsets_utf16().unwrap());
    }
}

#[test]
fn hosted_and_native_hold_the_identical_vt_owner_type() {
    let terminal = Terminal::new(3, 12).unwrap();
    let _: &crate::native_terminal::NativeTerminal = &terminal.vt;
    assert_eq!(
        std::any::type_name_of_val(&terminal.vt),
        std::any::type_name::<crate::vt_state::VtState>()
    );
}

#[test]
fn fragmented_private_cursor_sequences_preserve_the_last_column_and_origin() {
    for sequence in [b"\x1b[?2;2H".as_slice(), b"\x1b[2$A", b"\x1b[>2C"] {
        for cut in 0..=sequence.len() {
            let mut terminal = Terminal::new(5, 4).unwrap();
            terminal.feed(b"\x1b[2;4r\x1b[?6h\x1b[2;1HABCD").unwrap();
            let before = terminal.frame().unwrap();
            terminal.feed(&sequence[..cut]).unwrap();
            terminal.feed(&sequence[cut..]).unwrap();
            assert_eq!(terminal.frame().unwrap(), before);
            assert!(before.cursor().pending_wrap);
            terminal.feed(b"X").unwrap();
            let after = terminal.frame().unwrap();
            assert_eq!(after.rows()[2].cells[3].character, 'D');
            assert_eq!(after.rows()[3].cells[0].character, 'X');
        }
    }
}

#[test]
fn malformed_sgr_preserves_the_next_printed_cells_rendition() {
    for sequence in [
        b"\x1b[?31m".as_slice(),
        b"\x1b[31$m",
        b"\x1b[31%m",
        b"\x1b[=41m",
    ] {
        for cut in 0..=sequence.len() {
            let mut terminal = Terminal::new(2, 8).unwrap();
            terminal.feed(b"\x1b[32;44;1mA").unwrap();
            terminal.feed(&sequence[..cut]).unwrap();
            terminal.feed(&sequence[cut..]).unwrap();
            terminal.feed(b"B\x1b[31;41mC").unwrap();
            let frame = terminal.frame().unwrap();
            let cells = &frame.rows()[0].cells;
            assert_eq!(
                (cells[1].foreground, cells[1].background, cells[1].flags),
                (cells[0].foreground, cells[0].background, cells[0].flags)
            );
            assert_eq!(
                (cells[2].foreground, cells[2].background),
                (Color::Red, Color::Red)
            );
        }
    }
}
