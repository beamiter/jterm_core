use jterm_engine::{framing::control_string_abort, *};

fn assert_cells(actual: &[TerminalCell], expected: &[TerminalCell]) {
    assert_eq!(actual.len(), expected.len());
    for (a, b) in actual.iter().zip(expected) {
        assert_eq!(a.character, b.character);
        assert_eq!(a.foreground, b.foreground);
        assert_eq!(a.background, b.background);
        assert_eq!(a.flags, b.flags);
        assert_eq!(a.hyperlink_id, b.hyperlink_id);
    }
}

#[test]
fn compression_preserves_styled_unicode_wide_and_linked_cells() {
    let mut lead = TerminalCell {
        character: '界',
        foreground: Color::Red,
        hyperlink_id: HyperlinkId::from_raw(42),
        ..Default::default()
    };
    lead.flags.set_wide(true);
    let mut continuation = TerminalCell::default();
    continuation.flags.set_wide_continuation(true);
    let linked_blank = TerminalCell {
        hyperlink_id: HyperlinkId::from_raw(42),
        ..Default::default()
    };
    let cells = [lead, continuation, linked_blank, TerminalCell::default()];
    let mut line = ScrollbackLine::compress(&cells, true);
    line.set_raw_row_id(RawRowId::new(9));
    assert!(line.is_wrapped);
    assert_eq!(line.raw_row_id().get(), Some(9));
    assert_cells(&line.decompress(), &cells);
    assert_cells(
        &line
            .decode_search_row(4, line.search_encoded_bytes())
            .unwrap(),
        &cells,
    );
    assert!(!linked_blank.is_reflow_trimmable_blank());
    assert_eq!(std::mem::size_of::<TerminalCell>(), 16);
}

#[test]
fn plain_compression_padding_and_search_columns_are_unchanged() {
    let cells = [
        TerminalCell {
            character: 'é',
            ..Default::default()
        },
        TerminalCell::default(),
    ];
    let line = ScrollbackLine::compress(&cells, false);
    assert_cells(&line.decompress(), &cells);
    assert!(line.decode_search_row(1, usize::MAX).is_none());
    let mut wide = TerminalCell {
        character: '界',
        ..Default::default()
    };
    wide.flags.set_wide(true);
    let mut continuation = TerminalCell::default();
    continuation.flags.set_wide_continuation(true);
    assert_eq!(
        searchable_line_text(&[wide, continuation, cells[0], cells[1]]),
        ("界é".to_owned(), vec![0, 2], 3)
    );
}

#[test]
fn resize_and_scroll_preserve_retained_row_identity() {
    let mut grid = TerminalGrid::new(3, 2);
    grid.row_ids_mut()
        .copy_from_slice(&[RawRowId::new(1), RawRowId::new(2), RawRowId::new(3)]);
    grid[1][0].character = 'x';
    grid.row_wrapped[1] = true;
    grid.scroll_up_by(1, TerminalCell::default());
    assert_eq!(
        grid.row_ids(),
        &[RawRowId::new(2), RawRowId::new(3), RawRowId::UNTRACKED]
    );
    assert_eq!(grid[0][0].character, 'x');
    assert!(grid.row_wrapped[0]);
    grid.resize(4, 3, TerminalCell::default());
    assert_eq!(grid.row_id(0), RawRowId::new(2));
    assert_eq!(grid.row_id(3), RawRowId::UNTRACKED);
    assert_eq!(grid[0][0].character, 'x');
    grid.scroll_up_by(4, TerminalCell::default());
    assert!(grid.row_ids().iter().all(|id| !id.is_tracked()));
}

#[test]
fn framing_cancellation_and_terminator_precedence() {
    assert_eq!(
        control_string_abort(b"a\x18", false, true),
        Some((2, false))
    );
    assert_eq!(control_string_abort(b"\x1a", true, false), Some((1, false)));
    assert_eq!(control_string_abort(b"x", true, true), Some((0, true)));
    assert_eq!(control_string_abort(b"", true, true), None);
    assert_eq!(control_string_abort(b"\\\x18", true, false), None);
    assert_eq!(control_string_abort(b"\x1b\\\x18", false, false), None);
    assert_eq!(
        control_string_abort(b"a\x1b[", false, true),
        Some((1, false))
    );
    assert_eq!(control_string_abort(b"a\x1b", false, true), None);
    assert_eq!(control_string_abort(b"\x07\x18", false, true), None);
    assert_eq!(
        control_string_abort(b"\x07\x18", false, false),
        Some((2, false))
    );
}

// Frozen pre-extraction algorithm, retained only as a differential test oracle.
fn original(input: &[u8], previous_escape: bool, osc: bool) -> Option<(usize, bool)> {
    if previous_escape {
        match input.first().copied()? {
            b'\\' => return None,
            0x18 | 0x1a => return Some((1, false)),
            _ => return Some((0, true)),
        }
    }
    for (index, byte) in input.iter().copied().enumerate() {
        match byte {
            0x18 | 0x1a => return Some((index + 1, false)),
            0x07 if osc => return None,
            0x1b => match input.get(index + 1) {
                Some(b'\\') | None => return None,
                Some(_) => return Some((index, false)),
            },
            _ => {}
        }
    }
    None
}

#[test]
fn framing_matches_original_for_control_alphabet_and_every_split() {
    let alphabet = [b'x', b'\\', 0x18, 0x1a, 0x07, 0x1b];
    for len in 0..=5 {
        for mut value in 0..6_usize.pow(len) {
            let mut input = vec![0; len as usize];
            for byte in &mut input {
                *byte = alphabet[value % alphabet.len()];
                value /= alphabet.len();
            }
            for split in 0..=input.len() {
                for fragment in [&input[..split], &input[split..]] {
                    for previous_escape in [false, true] {
                        for osc in [false, true] {
                            assert_eq!(
                                control_string_abort(fragment, previous_escape, osc),
                                original(fragment, previous_escape, osc)
                            );
                        }
                    }
                }
            }
        }
    }
}
