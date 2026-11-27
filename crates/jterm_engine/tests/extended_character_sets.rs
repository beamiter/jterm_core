use jterm_engine::terminal::Terminal;

#[test]
fn g2_g3_locking_and_single_shifts_survive_all_feed_partitions() {
    let packet = b"\x1b*0\x1bnq\x1b+B\x1boq\x1bNqq\x1b*B\x1b+0\x0f\x1bOqq";
    for split in 0..=packet.len() {
        let mut terminal = Terminal::new(3, 12).unwrap();
        terminal.feed(&packet[..split]).unwrap();
        terminal.feed(&packet[split..]).unwrap();
        assert!(
            terminal.visible_text().starts_with("─q─q─q"),
            "split {split}"
        );
        assert_eq!(terminal.frame().unwrap().cursor().col, 6);
    }
}

#[test]
fn cursor_report_restores_pending_shift_without_consuming_it_on_unicode_or_controls() {
    let mut terminal = Terminal::new(3, 12).unwrap();
    terminal
        .feed("\x1b*0\x1b+B\x1bo\x1bN\x1b[31mλ".as_bytes())
        .unwrap();
    let before = terminal.frame().unwrap();
    let report = terminal.feed(b"\x1b[1$w").unwrap();
    assert_eq!(report, b"\x1bP1$u1;2;1;@;@;B;3;2;@;BB0B\x1b\\");
    assert_eq!(terminal.frame().unwrap(), before);

    terminal.feed(b"\x1b*B\x1b+0\x1bo\x1bO").unwrap();
    let mut restore = report.clone();
    restore[4] = b't';
    terminal.feed(&restore).unwrap();
    assert_eq!(terminal.feed(b"\x1b[1$w").unwrap(), report);
    terminal.feed(b"qq").unwrap();
    assert!(terminal.visible_text().starts_with("λ─q"));
    assert_eq!(
        terminal.feed(b"\x1b[1$w").unwrap(),
        b"\x1bP1$u1;4;1;@;@;@;3;2;@;BB0B\x1b\\"
    );
}

#[test]
fn saved_single_shift_is_consumed_by_one_graphic_space_after_cursor_restore() {
    let mut terminal = Terminal::new(3, 12).unwrap();
    terminal.feed(b"\x1b+0\x1bO\x1b7q\x1b8 q").unwrap();
    assert!(terminal.visible_text().starts_with(" q"));
    assert_eq!(
        terminal.feed(b"\x1b[1$w").unwrap(),
        b"\x1bP1$u1;3;1;@;@;@;0;2;@;BBB0\x1b\\"
    );
}

#[test]
fn repeat_preserves_the_pending_single_shift_for_the_next_input_graphic() {
    let packet = b"q\x1b*0\x1bN\x1b[5bqq";
    for split in 0..=packet.len() {
        let mut terminal = Terminal::new(2, 16).unwrap();
        terminal.feed(&packet[..split]).unwrap();
        terminal.feed(&packet[split..]).unwrap();
        assert!(terminal.visible_text().starts_with("qqqqqq─q"));
        assert_eq!(terminal.frame().unwrap().cursor().col, 8);
    }
}

#[test]
fn dec_special_control_pictures_survive_all_feed_partitions() {
    for packet in [
        b"\x1b(0bcdehi\x1b(Bbci".as_slice(),
        b"\x1b)0\x0ebcdehi\x0fbci",
        b"\x1b*0\x1bNb\x1bNc\x1bNd\x1bNe\x1bNh\x1bNibci",
        b"\x1b+0\x1bOb\x1bOc\x1bOd\x1bOe\x1bOh\x1bOibci",
    ] {
        for split in 0..=packet.len() {
            let mut terminal = Terminal::new(3, 12).unwrap();
            terminal.feed(&packet[..split]).unwrap();
            terminal.feed(&packet[split..]).unwrap();
            assert!(
                terminal.visible_text().starts_with("␉␌␍␊␤␋bci"),
                "split {split}"
            );
            let frame = terminal.frame().unwrap();
            assert_eq!((frame.cursor().row, frame.cursor().col), (0, 9));
            assert_eq!(terminal.unsupported_count(), 0);
        }
    }
}

#[test]
fn dec_special_control_pictures_are_single_column_graphics() {
    use unicode_width::UnicodeWidthChar;
    for (input, expected) in "bcdehi".chars().zip("␉␌␍␊␤␋".chars()) {
        let translated = jterm_engine::screen::Charset::DecSpecialGraphics.translate(input);
        assert_eq!(translated, expected);
        assert_eq!(translated.width(), Some(1));
        let mut terminal = Terminal::new(2, 12).unwrap();
        terminal
            .feed(format!("\x1b(0{input}\x1b[2b").as_bytes())
            .unwrap();
        assert!(terminal
            .visible_text()
            .starts_with(&expected.to_string().repeat(3)));
        let frame = terminal.frame().unwrap();
        assert_eq!((frame.cursor().row, frame.cursor().col), (0, 3));
    }
}

#[test]
fn uk_nrcs_negotiation_and_banks_survive_every_feed_partition() {
    for designation in [
        b"\x1b(A".as_slice(),
        b"\x1b)A\x0e",
        b"\x1b*A\x1bn",
        b"\x1b+A\x1bo",
    ] {
        let mut packet = designation.to_vec();
        packet.extend_from_slice(b"#\x1b[?42h##\x1b[?42l#\x1b[?42h#");
        packet.extend_from_slice("é中".as_bytes());
        for split in 0..=packet.len() {
            let mut terminal = Terminal::new(2, 16).unwrap();
            terminal.feed(&packet[..split]).unwrap();
            terminal.feed(&packet[split..]).unwrap();
            assert!(
                terminal.visible_text().starts_with("#££#£é中"),
                "split {split}, {designation:?}"
            );
            assert_eq!(terminal.frame().unwrap().cursor().col, 8);
            assert_eq!(terminal.feed(b"\x1b[?42$p").unwrap(), b"\x1b[?42;1$y");
            assert_eq!(terminal.unsupported_count(), 0);
        }
    }
}

#[test]
fn uk_nrcs_mode_saves_reports_and_resets_remain_independent_of_designation() {
    let mut terminal = Terminal::new(3, 24).unwrap();
    assert_eq!(terminal.feed(b"\x1b[?42$p").unwrap(), b"\x1b[?42;2$y");
    terminal
        .feed(b"\x1b*A\x1b+A\x1b[?42h\x1b[?42s\x1b[?42l\x1b[?42r\x1bN##\x1bO##")
        .unwrap();
    assert!(terminal.visible_text().starts_with("£#£#"));
    let report = terminal.feed(b"\x1b[1$w").unwrap();
    assert_eq!(report, b"\x1bP1$u1;5;1;@;@;@;0;2;@;BBAA\x1b\\");
    terminal.feed(b"\x1b*B\x1b+B").unwrap();
    let mut restore = report.clone();
    restore[4] = b't';
    terminal.feed(&restore).unwrap();
    assert_eq!(terminal.feed(b"\x1b[1$w").unwrap(), report);
    terminal.feed(b"\x1bN#\x1bN\x1b[35;2;1;2;2$x#").unwrap();
    assert!(terminal.visible_text().starts_with("£#£#£"));
    assert!(terminal.visible_text().contains("££"));
    terminal.feed(b"\x1bc").unwrap();
    assert_eq!(terminal.feed(b"\x1b[?42$p").unwrap(), b"\x1b[?42;2$y");
    terminal.feed(b"\x1b(A#").unwrap();
    assert!(terminal.visible_text().starts_with('#'));
}

#[test]
fn uk_designation_saved_cursor_soft_reset_and_gr_fill_remain_independent() {
    let mut terminal = Terminal::new(3, 16).unwrap();
    terminal
        .feed(b"\x1b[?42h\x1b(A\x1b7\x1b(B#\x1b8#\x1b[!p")
        .unwrap();
    assert!(terminal.visible_text().starts_with('£'));
    assert_eq!(terminal.feed(b"\x1b[?42$p").unwrap(), b"\x1b[?42;1$y");
    terminal
        .feed(b"\x1b)A\x1b~\x1b[163;2;1;2;2$x\x1b[255;3;1;3;2$x")
        .unwrap();
    let text = terminal.visible_text();
    assert_eq!(text.lines().nth(1).unwrap().trim_end(), "££");
}

#[test]
fn unsupported_hosted_designations_leave_g0_g1_graphics_at_every_split() {
    for (setup, invoke, unknown, ascii) in [
        (
            b"\x1b(0".as_slice(),
            b"".as_slice(),
            b"\x1b(?".as_slice(),
            b"\x1b(B".as_slice(),
        ),
        (
            b"\x1b)0".as_slice(),
            b"\x0e".as_slice(),
            b"\x1b)?".as_slice(),
            b"\x1b)B".as_slice(),
        ),
    ] {
        let packet = [setup, invoke, b"q", unknown, b"q", ascii, b"q"].concat();
        for split in 0..=packet.len() {
            let mut terminal = Terminal::new(2, 12).unwrap();
            terminal.feed(&packet[..split]).unwrap();
            terminal.feed(&packet[split..]).unwrap();
            assert!(terminal.visible_text().starts_with("──q"), "split {split}");
            assert_eq!(terminal.frame().unwrap().cursor().col, 3);
            assert_eq!(terminal.unsupported_count(), 1);
        }
    }
}

#[test]
fn unsupported_hosted_designation_preserves_saved_and_inactive_bank_state() {
    let mut terminal = Terminal::new(3, 12).unwrap();
    terminal.feed(b"\x1b(0\x1b)0\x1b7").unwrap();
    let before = terminal.feed(b"\x1b[1$w").unwrap();
    let frame = terminal.frame().unwrap();
    terminal.feed(b"\x1b(?\x1b)?").unwrap();
    assert_eq!(terminal.feed(b"\x1b[1$w").unwrap(), before);
    assert_eq!(terminal.frame().unwrap(), frame);
    terminal.feed(b"\x0eqq\x1b(B\x1b)B\x1b8qq").unwrap();
    assert!(terminal.visible_text().starts_with("──"));
    assert_eq!(terminal.unsupported_count(), 2);
}

#[test]
fn unsupported_hosted_designation_preserves_uk_nrcs_bank() {
    let packet = b"\x1b[?42h\x1b(A#\x1b(?#\x1b(B#";
    for split in 0..=packet.len() {
        let mut terminal = Terminal::new(2, 12).unwrap();
        terminal.feed(&packet[..split]).unwrap();
        terminal.feed(&packet[split..]).unwrap();
        assert!(terminal.visible_text().starts_with("££#"), "split {split}");
        assert_eq!(terminal.unsupported_count(), 1);
    }
}

#[test]
fn interrupted_scs_restarts_escape_or_cancels_without_printing_control_tail() {
    for (packet, expected, cursor) in [
        (b"\x1b(0\x1b(\x1b[31mq".as_slice(), "─", (0, 1)),
        (b"\x1b(B\x1b(\x18q", "q", (0, 1)),
        (b"\x1b(\n0q", "─", (1, 1)),
        (b"\x1b(\x7f0q", "─", (0, 1)),
    ] {
        for split in 0..=packet.len() {
            let mut terminal = Terminal::new(3, 12).unwrap();
            terminal.feed(&packet[..split]).unwrap();
            terminal.feed(&packet[split..]).unwrap();
            let frame = terminal.frame().unwrap();
            let text: String = frame.rows()[cursor.0]
                .cells
                .iter()
                .take(cursor.1)
                .map(|cell| cell.character)
                .collect();
            assert_eq!(text, expected, "packet {packet:?} split {split}");
            assert_eq!((frame.cursor().row, frame.cursor().col), cursor);
            assert_eq!(terminal.unsupported_count(), 0);
        }
    }
}

#[test]
fn multibyte_scs_designations_are_rejected_atomically_at_every_partition() {
    for (setup, designation, restore) in [
        (
            b"\x1b(0".as_slice(),
            b"\x1b(%?".as_slice(),
            b"\x1b(B".as_slice(),
        ),
        (
            b"\x1b)0\x0e".as_slice(),
            b"\x1b)%?".as_slice(),
            b"\x1b)B".as_slice(),
        ),
    ] {
        let packet = [setup, b"q", designation, b"q", restore, b"q"].concat();
        for split in 0..=packet.len() {
            let mut terminal = Terminal::new(3, 12).unwrap();
            terminal.feed(&packet[..split]).unwrap();
            terminal.feed(&packet[split..]).unwrap();
            assert!(terminal.visible_text().starts_with("──q"), "split {split}");
            assert_eq!(terminal.frame().unwrap().cursor().col, 3);
            assert_eq!(terminal.unsupported_count(), 1);
        }
    }
}
