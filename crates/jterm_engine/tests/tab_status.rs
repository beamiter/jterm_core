use jterm_engine::terminal::Terminal;

#[test]
fn tab_status_roundtrip_is_fragmented_atomic_and_preserves_cursor() {
    let packet = b"\x1bP2$t1/4/9/17/9/999\x1b\\\x1b[2$w";
    for cut in 0..=packet.len() {
        let mut terminal = Terminal::new(3, 20).unwrap();
        terminal.feed(b"\x1b[2;20HX").unwrap();
        let before = terminal.frame().unwrap();
        let mut reply = terminal.feed(&packet[..cut]).unwrap();
        reply.extend(terminal.feed(&packet[cut..]).unwrap());
        assert_eq!(reply, b"\x1bP2$u1/4/9/17\x1b\\");
        assert_eq!(terminal.frame().unwrap(), before);
        for invalid in [
            b"1/0/9".as_slice(),
            b"4/",
            b"4/x",
            b"999999999999999999999999999999",
            b"/4",
        ] {
            terminal
                .feed(&[b"\x1bP2$t", invalid, b"\x1b\\"].concat())
                .unwrap();
            assert_eq!(terminal.feed(b"\x1b[2$w").unwrap(), reply);
        }
        terminal.feed(b"\x1b[3g").unwrap();
        let restore = [
            b"\x1bP2$t".as_slice(),
            &reply[5..reply.len() - 2],
            b"\x1b\\",
        ]
        .concat();
        terminal.feed(&restore).unwrap();
        terminal.feed(b"\x1b[H\t").unwrap();
        assert_eq!(terminal.frame().unwrap().cursor().col, 3);
        terminal.feed(b"\x1bP2$t\x1b\\").unwrap();
        assert_eq!(terminal.feed(b"\x1b[2$w").unwrap(), b"\x1bP2$u\x1b\\");
    }
}
