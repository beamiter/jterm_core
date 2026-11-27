use jterm_engine::{
    colors::TerminalDefaultColors,
    frame::{CellFrame, Delta, Limits, Stamp},
    terminal::Terminal,
};
const LIMITS: Limits = Limits {
    max_rows: 128,
    max_cols: 400,
    max_cells: 51200,
};

#[test]
fn immutable_colors_survive_theme_updates_deltas_and_history_reads() {
    let mut terminal = Terminal::new(2, 8).unwrap();
    terminal.feed(b"one\r\ntwo\r\n").unwrap();
    let uncolored = terminal.frame().unwrap();
    assert!(uncolored.paint_colors().is_none());
    terminal.set_color_defaults(TerminalDefaultColors::default());
    let first = terminal.frame().unwrap();
    assert!(first.stamp().revision > uncolored.stamp().revision);
    assert_eq!(first.rows(), uncolored.rows());
    let before_bytes = first.retained_delivery_bytes();
    terminal.set_color_defaults(TerminalDefaultColors::default());
    assert_eq!(terminal.frame().unwrap(), first);
    terminal.feed(b"\x1b]10;?\x07").unwrap();
    assert_eq!(terminal.frame().unwrap(), first);
    let page = terminal.history_page(0, 1).unwrap();
    terminal
        .feed(b"\x1b]10;#123456\x07\x1b]4;1;#abcdef\x07")
        .unwrap();
    let next = terminal.frame().unwrap();
    assert_eq!(next.rows(), first.rows());
    assert_eq!(first.paint_colors().unwrap().dynamic_fg, None);
    assert_eq!(page.paint_colors().unwrap().dynamic_fg, None);
    assert_eq!(next.paint_colors().unwrap().dynamic_fg, Some((18, 52, 86)));
    assert_eq!(
        terminal.history_page(0, 1).unwrap().paint_colors(),
        next.paint_colors()
    );
    let delta = first.delta_to(&next).unwrap();
    assert!(delta.rows.is_empty());
    assert_eq!(first.apply(&delta, LIMITS).unwrap(), next);
    let bare = CellFrame::new(
        Stamp {
            revision: next.stamp().revision + 1,
            ..next.stamp()
        },
        next.cols(),
        next.shared_rows(),
        next.cursor(),
        LIMITS,
    )
    .unwrap();
    assert!(bare.retained_delivery_bytes() < before_bytes);
    assert_eq!(
        next.apply(&next.delta_to(&bare).unwrap(), LIMITS).unwrap(),
        bare
    );
    let raw = Delta::new(next.stamp(), bare.stamp(), next.cursor(), Vec::new());
    assert_eq!(
        next.apply(&raw, LIMITS).unwrap().paint_colors(),
        next.paint_colors()
    );
}

#[test]
fn completed_synchronized_frame_keeps_its_color_snapshot() {
    let mut terminal = Terminal::new(2, 8).unwrap();
    terminal.set_color_defaults(TerminalDefaultColors::default());
    terminal.begin_update(true);
    terminal
        .feed(b"\x1b[?2026h\x1b]11;#112233\x07A\x1b[?2026l\x1b]11;#445566\x07B")
        .unwrap();
    let completed = terminal.take_completed_sync_frame().unwrap();
    let live = terminal.frame().unwrap();
    assert_eq!(
        completed.paint_colors().unwrap().dynamic_bg,
        Some((17, 34, 51))
    );
    assert_eq!(live.paint_colors().unwrap().dynamic_bg, Some((68, 85, 102)));
    assert_eq!(completed.rows()[0].cells[1].character, ' ');
    assert_eq!(live.rows()[0].cells[1].character, 'B');
}
