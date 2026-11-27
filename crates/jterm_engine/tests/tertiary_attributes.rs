use jterm_engine::terminal::Terminal;

#[test]
fn tertiary_query_survives_every_partition_and_preserves_hosted_paint() {
    for query in [b"\x1b[=c".as_slice(), b"\x1b[=0c", b"\x1b[=000c"] {
        for split in 0..=query.len() {
            let mut terminal = Terminal::new(4, 16).unwrap();
            terminal.feed(b"\x1b[31;44mTEXT\x1b[2;3H").unwrap();
            let before = terminal.frame().unwrap();
            let mut reply = terminal.feed(&query[..split]).unwrap();
            reply.extend(terminal.feed(&query[split..]).unwrap());
            assert_eq!(reply, b"\x1bP!|00000000\x1b\\");
            assert_eq!(terminal.frame().unwrap(), before);
            assert_eq!(terminal.unsupported_count(), 0);
        }
    }
}

#[test]
fn tertiary_query_does_not_expand_the_hosted_device_identity_contract() {
    for query in [
        b"\x1b[c".as_slice(),
        b"\x1bZ",
        b"\x1b[>c",
        b"\x1b[=1c",
        b"\x1b[=0;0c",
        b"\x1b[=0:0c",
        b"\x1b[=0$c",
        b"\x1b[=0 c",
    ] {
        let mut terminal = Terminal::new(4, 16).unwrap();
        assert!(terminal.feed(query).unwrap().is_empty(), "{query:?}");
        assert_eq!(terminal.unsupported_count(), 1, "{query:?}");
    }
}
