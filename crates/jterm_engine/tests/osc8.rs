use jterm_engine::{
    frame::{CellFrame, Error, Limits, Screen},
    hyperlink::MAX_OSC8_URI_BYTES,
    terminal::Terminal,
    HyperlinkId,
};
use std::num::NonZeroU64;

const FIRST: &str = "https://example.test/first";
const SECOND: &str = "https://example.test/second";

fn policy(params: &str, uri: &str) -> bool {
    params != "reject" && uri.starts_with("https://example.test/")
}

fn open(params: &str, uri: &str) -> String {
    format!("\x1b]8;{params};{uri}\x1b\\")
}

fn terminal() -> Terminal {
    let mut terminal = Terminal::new(2, 32).unwrap();
    terminal.enable_hyperlinks(policy);
    terminal
}

fn id(frame: &CellFrame, row: usize, col: usize) -> HyperlinkId {
    frame.rows()[row].cells[col].hyperlink_id
}

#[test]
fn link_parser_is_opt_in_fragmented_and_produces_no_reply() {
    let mut legacy = Terminal::new(2, 32).unwrap();
    let mut styled = terminal();
    let target = "https://example.test/one;two";
    let input = format!("{}A\x1b]8;;\x07B", open("id=one", target));
    for &byte in input.as_bytes() {
        assert!(legacy.feed(&[byte]).unwrap().is_empty());
        assert!(styled.feed(&[byte]).unwrap().is_empty());
    }
    let before = legacy.frame().unwrap();
    let after = styled.frame().unwrap();
    assert_eq!(legacy.visible_text(), styled.visible_text());
    assert!(id(&before, 0, 0).is_none());
    assert_eq!(before.hyperlink_target(HyperlinkId::from_raw(1)), None);
    assert_eq!(after.hyperlink_target(id(&after, 0, 0)), Some(target));
    assert!(id(&after, 0, 1).is_none());
    assert!(after.hyperlink_target(HyperlinkId::NONE).is_none());
    assert_eq!(legacy.unsupported_count(), 2);
    assert_eq!(styled.unsupported_count(), 0);
}

#[test]
fn rejected_openings_and_opaque_parser_rejections_clear_the_active_link() {
    let mut terminal = terminal();
    let mut oversized = b"\x1b]8;;".to_vec();
    oversized.extend([b'x'; 4097]);
    oversized.push(7);

    for invalid in [
        open("reject", SECOND).into_bytes(),
        open("", "file:///tmp/unopened").into_bytes(),
        b"\x1b]8;no-separator\x07".to_vec(),
        b"\x1b]8;;https://example.test/\xff\x07".to_vec(),
        b"\x1b]8\x07".to_vec(),
        oversized,
    ] {
        terminal.feed(open("id=first", FIRST).as_bytes()).unwrap();
        terminal.feed(b"A").unwrap();
        terminal.feed(&invalid).unwrap();
        terminal.feed(b"B").unwrap();
    }
    let frame = terminal.frame().unwrap();
    for pair in frame.rows()[0].cells[..12].chunks_exact(2) {
        assert_eq!([pair[0].character, pair[1].character], ['A', 'B']);
        assert_eq!(frame.hyperlink_target(pair[0].hyperlink_id), Some(FIRST));
        assert!(pair[1].hyperlink_id.is_none());
    }
}

#[test]
fn changed_link_ids_publish_their_targets_through_a_synchronized_delta() {
    let mut terminal = terminal();
    terminal.feed(open("", FIRST).as_bytes()).unwrap();
    terminal.feed(b"A\x1b]8;;\x07").unwrap();
    let before = terminal.frame().unwrap();
    let first = id(&before, 0, 0);

    terminal.begin_update(true);
    let input = format!(
        "\x1b[?2026h\r{}A\x1b]8;;\x07\x1b[?2026l\r{}A",
        open("", SECOND),
        open("", FIRST),
    );
    terminal.feed(input.as_bytes()).unwrap();
    let after = terminal.take_completed_sync_frame().unwrap();
    let live = terminal.frame().unwrap();
    let second = id(&after, 0, 0);
    assert_ne!(first, second);
    assert_eq!(before.hyperlink_target(first), Some(FIRST));
    assert_eq!(before.hyperlink_target(second), None);
    assert_eq!(after.hyperlink_target(first), None);
    assert_eq!(after.hyperlink_target(second), Some(SECOND));
    assert_eq!(live.hyperlink_target(second), None);
    assert_eq!(live.hyperlink_target(first), Some(FIRST));

    let limits = Limits {
        max_rows: 2,
        max_cols: 32,
        max_cells: 64,
    };
    let transition = before.delta_to(&after).unwrap();
    assert_eq!(transition.rows.len(), 1);
    assert_eq!(before.apply(&transition, limits).unwrap(), after);
    assert_eq!(
        after
            .apply(&after.delta_to(&live).unwrap(), limits)
            .unwrap(),
        live
    );

    let other_epoch = Terminal::with_epoch(2, 32, NonZeroU64::new(9).unwrap()).unwrap();
    assert!(matches!(
        before.delta_to(&other_epoch.frame().unwrap()),
        Err(Error::Layout)
    ));
}

#[test]
fn primary_history_owns_referenced_targets_after_alternate_and_reset() {
    let mut terminal = terminal();
    terminal
        .feed(format!("{}界\x1b]8;;\x07\r\nB\r\nC", open("", FIRST)).as_bytes())
        .unwrap();
    let page = terminal.history_page(0, 1).unwrap();
    let row = page.row(0).unwrap();
    let first = row.cells[0].hyperlink_id;
    assert_ne!(first, HyperlinkId::NONE);
    assert_eq!(row.cells[1].hyperlink_id, first);
    assert_eq!(page.hyperlink_target(first), Some(FIRST));
    assert_eq!(terminal.frame().unwrap().hyperlink_target(first), None);

    terminal
        .feed(format!("\x1b[?1049h{}A", open("", SECOND)).as_bytes())
        .unwrap();
    let alternate = terminal.frame().unwrap();
    let second = id(&alternate, 0, 0);
    assert_eq!(alternate.stamp().screen, Screen::Alternate);
    assert_eq!(alternate.hyperlink_target(second), Some(SECOND));
    assert_eq!(
        terminal.history_page(0, 1).unwrap().hyperlink_target(first),
        None
    );
    terminal.feed(b"\x1b[?1049l\x1b[3J").unwrap();
    assert_eq!(
        terminal.history_page(0, 1).unwrap().hyperlink_target(first),
        None
    );
    assert_eq!(page.hyperlink_target(first), Some(FIRST));
    assert_eq!(page.hyperlink_target(second), None);
}

#[test]
fn overlapping_history_pages_resolve_only_their_own_links() {
    let mut terminal = terminal();
    let third = "https://example.test/third";
    let input = format!(
        "{}first\x1b]8;;\x07\r\n{}second\x1b]8;;\x07\r\nplain\r\n{}third\x1b]8;;\x07\r\nlive\r\nhere",
        open("", FIRST),
        open("", SECOND),
        open("", third),
    );
    terminal.feed(input.as_bytes()).unwrap();
    let layout = terminal.history_layout().unwrap();
    assert_eq!(layout.len(), 4);
    let first = terminal
        .projected_history_viewport_page(&layout, 0, 2)
        .unwrap();
    let first_id = first.row(0).unwrap().cells[0].hyperlink_id;
    let second_id = first.row(1).unwrap().cells[0].hyperlink_id;
    assert_eq!(first.hyperlink_target(first_id), Some(FIRST));
    assert_eq!(first.hyperlink_target(second_id), Some(SECOND));

    let next = terminal
        .projected_history_viewport_page(&layout, 1, 2)
        .unwrap();
    assert_eq!(next.text, "second\nplain");
    assert_eq!(next.hyperlink_target(first_id), None);
    assert_eq!(next.hyperlink_target(second_id), Some(SECOND));

    let last = terminal
        .projected_history_viewport_page(&layout, 2, 2)
        .unwrap();
    let third_id = last.row(1).unwrap().cells[0].hyperlink_id;
    assert_eq!(last.text, "plain\nthird");
    assert_eq!(last.hyperlink_target(second_id), None);
    assert_eq!(last.hyperlink_target(third_id), Some(third));
    assert_eq!(first.hyperlink_target(first_id), Some(FIRST));
    assert_eq!(first.hyperlink_target(third_id), None);
}

#[test]
fn standalone_retains_at_most_one_mebibyte_of_unique_targets() {
    let mut terminal = terminal();
    let target = |index| {
        let mut target = format!("https://example.test/{index}/");
        target.extend(std::iter::repeat_n('x', MAX_OSC8_URI_BYTES - target.len()));
        target
    };
    let entries = 1024 * 1024 / MAX_OSC8_URI_BYTES;
    for index in 0..entries {
        terminal.feed(open("", &target(index)).as_bytes()).unwrap();
    }
    let reopened = target(0);
    terminal.feed(open("again", &reopened).as_bytes()).unwrap();
    terminal.feed(b"A").unwrap();
    terminal
        .feed(open("", &target(entries)).as_bytes())
        .unwrap();
    terminal.feed(b"B").unwrap();
    let frame = terminal.frame().unwrap();
    let existing = id(&frame, 0, 0);
    assert_eq!(existing, HyperlinkId::from_raw(1));
    assert_eq!(frame.hyperlink_target(existing), Some(reopened.as_str()));
    assert!(id(&frame, 0, 1).is_none());
}
