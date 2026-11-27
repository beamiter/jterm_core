use jterm_engine::{terminal::Terminal, Color, StyleFlags};
const SETUP:&[u8]=b"\x1b[2;3r\x1b[?6h\x1b[31;44;1;3;4m\x1b[58;2;1;2;3m\x1b[1\"q\x1b(0\x1b)0\x1b*0\x1b+0\x1bo\x1b|\x1bN\x1bH\x1b[?7l";
#[test]
fn missing_decsc_slot_recovers_default_pen_through_all_hosted_restore_routes() {
    for restore in [b"\x1b8".as_slice(), b"\x1b[u", b"\x1b[?1048l"] {
        for split in 0..=restore.len() {
            let mut terminal = Terminal::new(4, 8).unwrap();
            terminal.enable_underline_colors();
            terminal.feed(SETUP).unwrap();
            let before = terminal.frame().unwrap();
            let tabs = terminal.feed(b"\x1b[2$w").unwrap();
            terminal.feed(&restore[..split]).unwrap();
            terminal.feed(&restore[split..]).unwrap();
            let after = terminal.frame().unwrap();
            assert_eq!(after.rows(), before.rows());
            assert_eq!(
                (
                    after.cursor().row,
                    after.cursor().col,
                    after.cursor().pending_wrap
                ),
                (0, 0, false)
            );
            assert_eq!(
                terminal.feed(b"\x1b[1$w").unwrap(),
                b"\x1bP1$u1;1;1;@;@;@;0;2;@;BBBB\x1b\\"
            );
            assert_eq!(
                terminal.feed(b"\x1bP$qm\x1b\\").unwrap(),
                b"\x1bP1$r0m\x1b\\"
            );
            assert_eq!(
                terminal.feed(b"\x1b[?6$p\x1b[?7$p").unwrap(),
                b"\x1b[?6;2$y\x1b[?7;2$y"
            );
            assert_eq!(terminal.feed(b"\x1b[2$w").unwrap(), tabs);
            assert_eq!(
                terminal.feed(b"\x1bP$qr\x1b\\").unwrap(),
                b"\x1bP1$r2;3r\x1b\\"
            );
            terminal.feed(b"q").unwrap();
            let frame = terminal.frame().unwrap();
            let cell = frame.rows()[0].cells[0];
            assert_eq!(cell.character, 'q');
            assert_eq!(
                (cell.foreground, cell.background, cell.flags),
                (Color::Default, Color::Default, StyleFlags::default())
            );
            assert_eq!(frame.underline_color(0, 0), None);
        }
    }
}
