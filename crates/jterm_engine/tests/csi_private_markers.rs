use jterm_engine::terminal::Terminal;

#[test]
fn misplaced_private_marker_does_not_move_the_cursor() {
    // XTerm 412 leaves CUP 2;3 unchanged for each of these malformed CUF packets.
    for marker in b"<=>?" {
        let packet = [b"\x1b[2;3H\x1b[4".as_slice(), &[*marker], b"C\x1b[6n"].concat();
        for split in 0..=packet.len() {
            let mut terminal = Terminal::new(8, 12).unwrap();
            let mut reply = terminal.feed(&packet[..split]).unwrap();
            reply.extend(terminal.feed(&packet[split..]).unwrap());
            assert_eq!(reply, b"\x1b[2;3R", "marker {marker}, split {split}");
        }
    }
}
