use jterm_engine::{terminal::Terminal, CursorShape};

#[test]
fn unsupported_cursor_styles_preserve_the_current_shape_across_feed_partitions() {
    let bytes = b"\x1b[6 q\x1b[260 q\x1b[8 q\x1b[65535 q\x1bP$q q\x1b\\";
    for split in 0..=bytes.len() {
        let mut terminal = Terminal::new(3, 12).unwrap();
        let mut reply = terminal.feed(&bytes[..split]).unwrap();
        reply.extend(terminal.feed(&bytes[split..]).unwrap());
        assert_eq!(reply, b"\x1bP1$r6 q\x1b\\", "split {split}");
        assert_eq!(terminal.frame().unwrap().cursor().shape, CursorShape::Beam);
        assert_eq!(
            terminal.feed(b"\x1b[4 q\x1bP$q q\x1b\\").unwrap(),
            b"\x1bP1$r4 q\x1b\\"
        );
    }
}

#[test]
fn invalid_style_values_do_not_change_a_painted_cursor_or_grid() {
    for (parameter, shape) in [
        (2, CursorShape::Block),
        (4, CursorShape::Underline),
        (6, CursorShape::Beam),
    ] {
        let mut terminal = Terminal::new(3, 12).unwrap();
        terminal
            .feed(format!("TEXT\x1b[2;3H\x1b[{parameter} q").as_bytes())
            .unwrap();
        let before = terminal.frame().unwrap();
        for invalid in [8, 9, 255, 256, 260, 261, 65535] {
            terminal
                .feed(format!("\x1b[{invalid} q").as_bytes())
                .unwrap();
            let after = terminal.frame().unwrap();
            assert_eq!(after.cursor(), before.cursor(), "style {invalid}");
            assert_eq!(after.rows(), before.rows());
            assert_eq!(after.cursor().shape, shape);
        }
    }
}

#[test]
fn csi_parameters_after_intermediates_do_not_apply_cursor_style() {
    for malformed in [
        b"\x1b[ 4q".as_slice(),
        b"\x1b[1 4q",
        b"\x1b[ ;4q",
        b"\x1b[ :4q",
    ] {
        let packet = [b"\x1b[6 q".as_slice(), malformed, b"\x1bP$q q\x1b\\"].concat();
        for split in 0..=packet.len() {
            let mut terminal = Terminal::new(3, 12).unwrap();
            let mut reply = terminal.feed(&packet[..split]).unwrap();
            reply.extend(terminal.feed(&packet[split..]).unwrap());
            assert_eq!(
                reply, b"\x1bP1$r6 q\x1b\\",
                "split {split}, malformed {malformed:?}"
            );
            terminal.feed(b"\x1b[4 q").unwrap();
            assert_eq!(
                terminal.feed(b"\x1bP$q q\x1b\\").unwrap(),
                b"\x1bP1$r4 q\x1b\\"
            );
        }
    }
}
