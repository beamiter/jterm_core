use jterm_engine::terminal::Terminal;

#[test]
fn locator_capabilities_are_read_only_and_survive_every_partition() {
    for (query, response) in [
        (b"\x1b[?55n".as_slice(), b"\x1b[?53n".as_slice()),
        (b"\x1b[?56n".as_slice(), b"\x1b[?57;0n".as_slice()),
        (b"\x1b[?0055n".as_slice(), b"\x1b[?53n".as_slice()),
    ] {
        for split in 0..=query.len() {
            let mut terminal = Terminal::new(4, 16).unwrap();
            terminal
                .feed(b"\x1b[31;44mTEXT\x1b[2;3H\x1b[?1000;1006h")
                .unwrap();
            let before = terminal.frame().unwrap();
            let mut reply = terminal.feed(&query[..split]).unwrap();
            reply.extend(terminal.feed(&query[split..]).unwrap());
            assert_eq!(reply, response);
            assert_eq!(terminal.frame().unwrap(), before);
            assert_eq!(terminal.unsupported_count(), 0);
        }
    }
}

#[test]
fn locator_queries_keep_their_private_single_parameter_namespace() {
    for query in [
        b"\x1b[55n".as_slice(),
        b"\x1b[56n",
        b"\x1b[>55n",
        b"\x1b[?55$n",
        b"\x1b[?55;0n",
        b"\x1b[?55:0n",
        b"\x1b[?54n",
    ] {
        let mut terminal = Terminal::new(4, 16).unwrap();
        let before = terminal.frame().unwrap();
        assert!(terminal.feed(query).unwrap().is_empty(), "{query:?}");
        assert_eq!(terminal.frame().unwrap(), before);
    }
}

#[test]
fn absent_printer_status_is_read_only_and_partition_independent() {
    for packet in [b"\x1b[?15n".as_slice(), b"\x1b[?0015n"] {
        for split in 0..=packet.len() {
            let mut terminal = Terminal::new(4, 16).unwrap();
            terminal.feed(b"\x1b[31;44mTEXT\x1b[2;3H").unwrap();
            let before = terminal.frame().unwrap();
            let mut reply = terminal.feed(&packet[..split]).unwrap();
            reply.extend(terminal.feed(&packet[split..]).unwrap());
            assert_eq!(reply, b"\x1b[?13n");
            assert_eq!(terminal.frame().unwrap(), before);
            assert_eq!(terminal.unsupported_count(), 0);
        }
    }
}

#[test]
fn printer_status_rejects_other_namespaces_and_parameter_lists() {
    for packet in [
        b"\x1b[15n".as_slice(),
        b"\x1b[>15n",
        b"\x1b[?15$n",
        b"\x1b[?15;0n",
        b"\x1b[?15:0n",
    ] {
        let mut terminal = Terminal::new(4, 16).unwrap();
        let before = terminal.frame().unwrap();
        assert!(terminal.feed(packet).unwrap().is_empty(), "{packet:?}");
        assert_eq!(terminal.frame().unwrap(), before);
    }
}
