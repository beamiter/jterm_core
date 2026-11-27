use jterm_engine::{terminal::Terminal, Color, StyleFlags};
const SETUP: &[u8] =
    b"ABCDEF\r\nGHIJKL\r\nMNOPQR\r\nSTUVWX\x1b[2;3r\x1b[44m\x1bP1$t2;4;1;@;@;H;0;2;@;BBBB\x1b\\";
#[test]
fn hosted_horizontal_scroll_preserves_cursor_and_uses_bce_at_every_split() {
    for (packet, expected) in [
        (
            b"\x1b[2 @".as_slice(),
            ["ABCDEF", "IJKL  ", "OPQR  ", "STUVWX"],
        ),
        (
            b"\x1b[2 A".as_slice(),
            ["ABCDEF", "  GHIJ", "  MNOP", "STUVWX"],
        ),
    ] {
        for split in 0..=packet.len() {
            let mut terminal = Terminal::new(4, 6).unwrap();
            terminal.feed(SETUP).unwrap();
            let before = terminal.frame().unwrap();
            terminal.feed(&packet[..split]).unwrap();
            terminal.feed(&packet[split..]).unwrap();
            let after = terminal.frame().unwrap();
            assert_eq!(after.cursor(), before.cursor());
            for (row, line) in expected.iter().enumerate() {
                assert_eq!(
                    after.rows()[row]
                        .cells
                        .iter()
                        .map(|cell| cell.character)
                        .collect::<String>(),
                    *line
                );
            }
            for row in 1..3 {
                for cell in after.rows()[row]
                    .cells
                    .iter()
                    .filter(|cell| cell.character == ' ')
                {
                    assert_eq!(
                        (cell.foreground, cell.background, cell.flags),
                        (Color::Default, Color::Blue, StyleFlags::default())
                    );
                }
            }
        }
    }
}
#[test]
fn hosted_horizontal_scroll_guards_do_not_change_plain_ich_or_cuu() {
    let mut terminal = Terminal::new(4, 6).unwrap();
    terminal.feed(SETUP).unwrap();
    let before = terminal.frame().unwrap();
    terminal
        .feed(b"\x1b[2:1 @\x1b[?2 @\x1b[2;1 @\x1b[2:1 A\x1b[?2 A\x1b[2;1 A\x1b[2  @\x1b[2 $@\x1b[2  A\x1b[?2  @")
        .unwrap();
    assert_eq!(terminal.frame().unwrap().rows(), before.rows());
    terminal.feed(b"\x1b[@").unwrap();
    assert_eq!(terminal.frame().unwrap().rows()[1].cells[3].character, ' ');
    assert_eq!(terminal.frame().unwrap().rows()[2], before.rows()[2]);
    terminal.feed(b"\x1b[A").unwrap();
    assert_eq!(terminal.frame().unwrap().cursor().row, 1);
}
