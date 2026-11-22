use jterm_engine::{
    frame::{Limits, Screen},
    terminal::Terminal,
    Color, UnderlineColorRun, UnderlineStyle,
};
use std::sync::Arc;

fn terminal(rows: usize, cols: usize) -> Terminal {
    let mut terminal = Terminal::new(rows, cols).unwrap();
    terminal.enable_underline_colors();
    terminal
}

fn line(terminal: &Terminal, row: usize) -> Vec<Option<Color>> {
    let frame = terminal.frame().unwrap();
    (0..frame.cols())
        .map(|col| frame.underline_color(row, col))
        .collect()
}

#[test]
fn opt_in_retains_fragmented_rgb_indexed_and_reset_semantics() {
    let mut styled = terminal(2, 8);
    let mut legacy = Terminal::new(2, 8).unwrap();
    let input = b"\x1b[4:3;58:2::251:31:127mA\x1b[58;5;17mB\x1b[59mC\x1b[58:5:42mD\x1b[0mE";
    for value in input {
        styled.feed(&[*value]).unwrap();
        legacy.feed(&[*value]).unwrap();
    }
    let frame = styled.frame().unwrap();
    assert_eq!(
        &frame.rows()[0].cells[..5],
        &legacy.frame().unwrap().rows()[0].cells[..5]
    );
    assert_eq!(
        frame.rows()[0].cells[0].flags.underline(),
        UnderlineStyle::Curly
    );
    assert_eq!(
        frame.rows()[0].cells[4].flags.underline(),
        UnderlineStyle::None
    );
    assert_eq!(
        line(&styled, 0),
        [
            Some(Color::Rgb(251, 31, 127)),
            Some(Color::Indexed(17)),
            None,
            Some(Color::Indexed(42)),
            None,
            None,
            None,
            None
        ]
    );
    assert!(legacy.frame().unwrap().underline_color_runs(0).is_empty());
    assert_eq!(frame.underline_color(9, 0), None);
    assert_eq!(frame.underline_color(0, usize::MAX), None);

    let query = b"\x1b[4;58;5;9m\x1bP$qm\x1b\\";
    assert_eq!(styled.feed(query).unwrap(), b"\x1bP1$r4;58;5;9m\x1b\\");
    assert_eq!(legacy.feed(query).unwrap(), b"\x1bP1$r4m\x1b\\");
    styled.feed(b"\x1b[!pF").unwrap();
    assert_eq!(styled.frame().unwrap().underline_color(0, 5), None);
}

#[test]
fn decoration_only_change_and_removal_are_admitted_frame_deltas() {
    let mut terminal = terminal(2, 8);
    terminal.feed(b"\x1b[4;58;5;1mAA").unwrap();
    let before = terminal.frame().unwrap();
    assert_eq!(
        before.underline_color_runs(0),
        [UnderlineColorRun {
            start: 0,
            end: 2,
            color: Color::Indexed(1)
        }]
    );
    terminal.feed(b"\r\x1b[58;5;2mAA").unwrap();
    let after = terminal.frame().unwrap();
    assert_eq!(before.rows(), after.rows());
    assert!(Arc::ptr_eq(&before.rows()[0].cells, &after.rows()[0].cells));
    assert_ne!(before.underline_color(0, 0), after.underline_color(0, 0));
    let limits = Limits {
        max_rows: 2,
        max_cols: 8,
        max_cells: 16,
    };
    let delta = before.delta_to(&after).unwrap();
    assert_eq!(delta.rows.iter().map(|(r, _)| *r).collect::<Vec<_>>(), [0]);
    assert_eq!(before.apply(&delta, limits).unwrap(), after);

    terminal.feed(b"\r\x1b[59mAA").unwrap();
    let reset = terminal.frame().unwrap();
    let delta = after.delta_to(&reset).unwrap();
    assert_eq!(delta.rows.len(), 1);
    assert_eq!(after.apply(&delta, limits).unwrap(), reset);
    assert_eq!(before.underline_color(0, 0), Some(Color::Indexed(1)));
    assert!(reset.underline_color_runs(0).is_empty());
}

#[test]
fn wide_shifts_erase_insert_and_resize_keep_colors_at_physical_cells() {
    let mut terminal = terminal(4, 8);
    let first = Some(Color::Indexed(1));
    let second = Some(Color::Indexed(2));
    terminal.feed("\x1b[4;58;5;1mA界B".as_bytes()).unwrap();
    assert_eq!(
        &line(&terminal, 0)[..5],
        &[first, first, first, first, None]
    );
    terminal.feed(b"\x1b[1;2H\x1b[@").unwrap();
    assert_eq!(
        &line(&terminal, 0)[..6],
        &[first, None, first, first, first, None]
    );
    terminal.feed(b"\x1b[1;4H\x1b[P").unwrap();
    assert_eq!(
        &line(&terminal, 0)[..6],
        &[first, None, None, first, None, None]
    );
    let frame = terminal.frame().unwrap();
    assert_eq!(frame.rows()[0].cells[2].character, ' ');
    assert_eq!(frame.rows()[0].cells[3].character, 'B');
    terminal
        .feed(b"\x1b[X\x1b[4h\x1b[H\x1b[58;5;2mX\x1b[4l")
        .unwrap();
    assert_eq!(
        &line(&terminal, 0)[..6],
        &[second, first, None, None, None, None]
    );
    terminal.feed("\x1b[1;4H界".as_bytes()).unwrap();
    assert_eq!(&line(&terminal, 0)[3..5], &[second, second]);
    terminal.resize(4, 4).unwrap();
    assert_eq!(line(&terminal, 0), [second, first, None, None]);
    terminal.resize(4, 8).unwrap();
    assert_eq!(
        line(&terminal, 0),
        [second, first, None, None, None, None, None, None]
    );
}

#[test]
fn alternate_and_saved_cursor_restore_their_color_states() {
    let mut terminal = terminal(3, 8);
    let primary = Some(Color::Indexed(1));
    let altered = Some(Color::Indexed(2));
    terminal
        .feed(b"\x1b[4;58;5;1mA\x1b7\x1b[58;5;2mB\x1b8C")
        .unwrap();
    assert_eq!(&line(&terminal, 0)[..3], &[primary, primary, None]);
    terminal
        .feed(b"\x1b[?1049h\x1b[58;5;2mX\x1b[?1049h")
        .unwrap();
    let alternate = terminal.frame().unwrap();
    assert_eq!(alternate.stamp().screen, Screen::Alternate);
    assert_eq!(alternate.underline_color(0, 0), altered);
    terminal.feed(b"\x1b[?1049lD").unwrap();
    assert_eq!(&line(&terminal, 0)[..4], &[primary, primary, primary, None]);

    terminal.feed(b"\x1b[?47h").unwrap();
    assert_eq!(terminal.frame().unwrap().underline_color(0, 0), altered);
    terminal.feed(b"\x1b[?47lE").unwrap();
    assert_eq!(
        &line(&terminal, 0)[..5],
        &[primary, primary, primary, None, None]
    );

    terminal.feed(b"\x1b[?1047h").unwrap();
    assert!(terminal.frame().unwrap().underline_color_runs(0).is_empty());
    terminal
        .feed(b"\x1b[?1047l\x1b[1;5H\x1b[58;5;1m\x1b[?1048h\x1b[58;5;2mF\x1b[?1048lG")
        .unwrap();
    assert_eq!(terminal.frame().unwrap().underline_color(0, 4), primary);
}

#[test]
fn scrolling_and_selective_erasure_preserve_exact_archive_color_coordinates() {
    let mut terminal = terminal(3, 6);
    let first = Some(Color::Indexed(1));
    let second = Some(Color::Indexed(2));
    let third = Some(Color::Indexed(3));
    terminal
        .feed(b"\x1b[4;58;5;1mA\r\n\x1b[58;5;2mB\r\n\x1b[58;5;3mC\r\n")
        .unwrap();
    assert_eq!(line(&terminal, 0)[0], second);
    assert_eq!(line(&terminal, 1)[0], third);
    assert_eq!(line(&terminal, 2)[0], None);
    let archive = terminal.history_page(0, 1).unwrap();
    assert_eq!(archive.underline_color(0, 0), first);
    assert_eq!(archive.row(0).unwrap().cells[0].character, 'A');
    assert_eq!(archive.text, "A");

    terminal.feed(b"\x1b[H\x1bM").unwrap();
    assert_eq!(
        [
            line(&terminal, 0)[0],
            line(&terminal, 1)[0],
            line(&terminal, 2)[0]
        ],
        [None, second, third]
    );
    terminal.feed(b"\x1b[2;1H\x1b[L").unwrap();
    assert_eq!(
        [line(&terminal, 1)[0], line(&terminal, 2)[0]],
        [None, second]
    );
    terminal.feed(b"\x1b[M").unwrap();
    assert_eq!(
        [line(&terminal, 1)[0], line(&terminal, 2)[0]],
        [second, None]
    );

    terminal
        .feed(b"\x1b[2;1H\x1b[1\"qY\x1b[0\"qZ\x1b[?2K")
        .unwrap();
    assert_eq!(&line(&terminal, 1)[..3], &[third, None, None]);
    terminal.feed(b"\x1b[2J\x1b[3J").unwrap();
    assert!(terminal.frame().unwrap().underline_color_runs(1).is_empty());
    assert_eq!(terminal.history_rows(), 0);
    assert_eq!(archive.underline_color(0, 0), first);
}
