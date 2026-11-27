use jterm_engine::{terminal::Terminal, Color};
#[test]
fn hosted_column_index_is_fragmentation_independent_and_namespace_exact() {
    for backward in [true, false] {
        let packet = if backward {
            b"ABCD\r\nEFGH\x1b[1;1H\x1b[31;44m\x1b6".as_slice()
        } else {
            b"ABCD\r\nEFGH\x1b[1;4H\x1b[31;44m\x1b9".as_slice()
        };
        for split in 0..=packet.len() {
            let mut t = Terminal::new(2, 4).unwrap();
            t.feed(&packet[..split]).unwrap();
            t.feed(&packet[split..]).unwrap();
            let f = t.frame().unwrap();
            let texts = f
                .rows()
                .iter()
                .map(|r| r.cells.iter().map(|c| c.character).collect::<String>())
                .collect::<Vec<_>>();
            assert_eq!(
                texts,
                if backward {
                    vec![" ABC", " EFG"]
                } else {
                    vec!["BCD ", "FGH "]
                }
            );
            let c = f.rows()[0].cells[if backward { 0 } else { 3 }];
            assert_eq!((c.foreground, c.background), (Color::Red, Color::Blue));
        }
    }
    let mut t = Terminal::new(2, 4).unwrap();
    t.feed(b"ABCD\x1b[1;3H\x1b6").unwrap();
    assert_eq!(t.frame().unwrap().cursor().col, 1);
    t.feed(b"\x1b#6\x1b 9").unwrap();
    assert_eq!(t.frame().unwrap().cursor().col, 1);
}

#[test]
fn backward_index_consumes_pending_wrap_with_reverse_wrap_enabled() {
    let mut t = Terminal::new(2, 4).unwrap();
    t.feed(b"\x1b[?45hABCD\x1b6").unwrap();
    let f = t.frame().unwrap();
    assert_eq!(f.cursor().col, 3);
    assert!(!f.cursor().pending_wrap);
    t.feed(b"\x1b6").unwrap();
    assert_eq!(t.frame().unwrap().cursor().col, 2);
}
