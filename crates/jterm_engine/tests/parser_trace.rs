use jterm_engine::parser::*;

fn limits() -> ParserLimits {
    ParserLimits {
        pending_bytes: 128,
        kitty_recovery_bytes: 64,
    }
}
fn feed(p: &mut Parser, bytes: &[u8], out: &mut Vec<String>) {
    let mut at = 0;
    let mut iterations = 0;
    loop {
        iterations += 1;
        assert!(iterations <= bytes.len() * 5 + 20, "zero-consumption spin");
        let step = p.next(&bytes[at..]);
        assert!(step.consumed <= bytes.len() - at);
        at += step.consumed;
        match step.event {
            Some(Event::Ascii(run)) => out.extend(run.iter().map(|b| format!("P{}", *b as char))),
            Some(Event::Print(ch)) => out.push(format!("P{ch}")),
            Some(Event::String {
                kind,
                payload,
                bel_terminated,
            }) => out.push(format!("S{kind:?}:{payload:?}:{bel_terminated}")),
            Some(Event::RejectApc { recovery, reason }) => {
                // Recovery deliberately retains different data suffixes across reads.
                // Only the bounded control prefix is interpreted by the host responder.
                let control = recovery.split(|b| *b == b';').next().unwrap();
                out.push(format!("RejectApc:{control:?}:{reason:?}"));
            }
            Some(event) => out.push(format!("{event:?}")),
            None => {
                assert_eq!(at, bytes.len());
                break;
            }
        }
    }
}
fn trace(bytes: &[u8], cuts: &[usize]) -> Vec<String> {
    let mut p = Parser::new(limits());
    let mut result = Vec::new();
    let mut at = 0;
    for &end in cuts {
        feed(&mut p, &bytes[at..end], &mut result);
        at = end;
    }
    feed(&mut p, &bytes[at..], &mut result);
    result
}

#[test]
fn ordinary_traces_are_partition_invariant() {
    for bytes in [
        b"a\x1b[1;38:2::1:2:3mZ\x1b[6n".as_slice(),
        b"\x1b]0;title\x07x\x1b]52;c;data\x1b\\y",
        b"\x1b_Gi=4,q=1;data\x1b\\z\x1bP$qm\x1b\\",
        b"\x1b[31\r\n;1mX\x1b[12\x18Y\x1b[12\x1b[0mQ",
        b"\x1b]abc\x1b[0mQ\x1b_Gi=4;data\x18Z",
        b"\x1b_Xopaque\x1b\\\x1b^hidden\x1b\\\x1bXhidden\x1b\\",
        "é界🙂\x1b(0abc\x1b(B\x1b%GZ".as_bytes(),
        b"\xe0\x80\x80A\xf0\x9f\x1bcB",
    ] {
        let expected = trace(bytes, &[]);
        for cut in 0..=bytes.len() {
            assert_eq!(trace(bytes, &[cut]), expected, "cut {cut} bytes {bytes:?}");
        }
        assert_eq!(
            trace(bytes, &(0..=bytes.len()).collect::<Vec<_>>()),
            expected
        );
    }
}

#[test]
fn csi_embedded_controls_execute_once_and_colons_survive() {
    let got = trace(b"\x1b[38:2::1\r\n:2:3m", &[4, 10, 12]);
    assert_eq!(got.len(), 3);
    assert_eq!(got[0], "Control(13)");
    assert_eq!(got[1], "Control(10)");
    assert!(got[2].contains("params: [38, 2, 0, 1, 2, 3]"));
    assert!(got[2].contains("colon_flags: [false, true, true, true, true, true]"));
}

#[test]
fn utf8_matches_standard_library_at_all_splits() {
    for bytes in [
        b"\xe0\x80\x80Z".as_slice(),
        b"\xed\xa0\x80Z",
        b"\xf4\x90\x80\x80Z",
        b"\xc0\xafZ",
        b"\xe2A",
        "界🙂éZ".as_bytes(),
    ] {
        let expected: Vec<_> = String::from_utf8_lossy(bytes)
            .chars()
            .map(|c| format!("P{c}"))
            .collect();
        for split in 0..=bytes.len() {
            assert_eq!(trace(bytes, &[split]), expected);
        }
    }
}

#[test]
fn no_input_preserves_partial_state_and_ris_allows_same_batch_suffix() {
    let mut p = Parser::new(limits());
    let mut out = Vec::new();
    feed(&mut p, b"\x1b[31", &mut out);
    assert!(p.legacy_pending_control());
    let empty = p.next(b"");
    assert_eq!(empty.consumed, 0);
    assert!(empty.event.is_none());
    feed(&mut p, b"m\x1bcOK", &mut out);
    assert!(!p.legacy_pending_control());
    assert!(out.iter().any(|e| e.contains("final_byte: 99")));
    assert_eq!(&out[out.len() - 2..], &["PO", "PK"]);
    feed(&mut p, b"\xe2", &mut out);
    assert!(!p.legacy_pending_control());
    p.reset();
    assert_eq!(p.retained_bytes(), 0);
}

#[test]
fn apc_overflow_is_bounded_quiet_and_partition_invariant() {
    let mut bytes = b"\x1b_Gi=42,q=2;".to_vec();
    bytes.resize(180, b'A');
    bytes.extend_from_slice(b"\x1b\\Z");
    let expected = trace(&bytes, &[]);
    for cut in 0..=bytes.len() {
        assert_eq!(trace(&bytes, &[cut]), expected, "cut {cut}");
    }
    let mut p = Parser::new(limits());
    let mut out = Vec::new();
    for piece in bytes.chunks(1) {
        feed(&mut p, piece, &mut out);
        assert!(p.retained_bytes() <= 128);
    }
    assert_eq!(out.iter().filter(|e| e.starts_with("RejectApc")).count(), 1);
    assert_eq!(out.last().unwrap(), "PZ");
    assert_eq!(out, expected);
}

#[test]
fn osc_dcs_overflow_preserves_existing_batch_scoped_policy() {
    for prefix in [b"\x1b]0;".as_slice(), b"\x1bP"] {
        let mut bytes = prefix.to_vec();
        bytes.resize(180, b'A');
        bytes.extend_from_slice(b"\x1b\\Z");
        assert_ne!(trace(&bytes, &[]), trace(&bytes, &[129]));
        let mut p = Parser::new(limits());
        let mut out = Vec::new();
        feed(&mut p, &bytes[..129], &mut out);
        assert_eq!(p.retained_bytes(), 0);
        feed(&mut p, b"\x1bcRESTORED", &mut out);
        assert!(!p.legacy_pending_control());
    }
}

#[test]
fn csi_capacity_and_saturating_numbers_are_unchanged() {
    let (params, flags) = parse_csi_params(b";999999:;5;");
    assert_eq!(params, [0, 65535, 0, 5, 0]);
    assert_eq!(flags, [false, false, true, false, false]);
    let mut bytes = b"\x1b[".to_vec();
    bytes.extend([b'1'; 280]);
    bytes.extend([b' '; 12]);
    bytes.push(b'm');
    let expected = trace(&bytes, &[]);
    assert!(expected[0].contains("params: [65535]"));
    assert!(expected[0].contains("intermediates: [32, 32, 32, 32, 32, 32, 32, 32]"));
    for cut in 0..=bytes.len() {
        assert_eq!(trace(&bytes, &[cut]), expected);
    }
}

#[test]
fn cancellation_resume_offsets_and_terminator_kind_are_exact() {
    let bytes = b"\x1b]0;old\x1b[6nA\x1b]x\x18B\x1b]q\x07\x1b]r\x1b\\C";
    let expected = trace(bytes, &[]);
    assert_eq!(expected, [
        "Csi { params: [6], colon_flags: [false], private_prefix: None, intermediates: [], final_byte: 110 }",
        "PA", "PB", "SOsc:[113]:true", "SOsc:[114]:false", "PC"
    ]);
    for cut in 0..=bytes.len() {
        assert_eq!(trace(bytes, &[cut]), expected);
    }
}

#[test]
fn watcher_ignores_discard_and_utf8_but_observes_every_control_carry() {
    for prefix in [
        b"\x1b".as_slice(),
        b"\x1b(",
        b"\x1b[",
        b"\x1b]",
        b"\x1bP",
        b"\x1b_",
    ] {
        let mut p = Parser::new(limits());
        let mut out = Vec::new();
        feed(&mut p, prefix, &mut out);
        assert!(p.legacy_pending_control(), "{prefix:?}");
        p.reset();
        assert!(!p.legacy_pending_control());
    }
    let mut p = Parser::new(limits());
    let mut out = Vec::new();
    let mut overflow = b"\x1b_Gi=1;".to_vec();
    overflow.resize(129, b'A');
    feed(&mut p, &overflow, &mut out);
    assert!(!p.legacy_pending_control());
    assert_eq!(p.retained_bytes(), 0);
    feed(&mut p, b"more\x1b", &mut out);
    assert!(!p.legacy_pending_control());
    feed(&mut p, b"\\\xe2", &mut out);
    assert!(!p.legacy_pending_control());
    assert_eq!(p.retained_bytes(), 1);
}

#[test]
fn deterministic_random_partition_traces_match() {
    let bytes = b"\x1b[31\r;1mabc\xe2\x82\xac\x1b]0;title\x07\x1b_Gi=9,q=1;stuff\x1b\\\x1bP$qm\x1b\\\x1bcRESET";
    let expected = trace(bytes, &[]);
    for seed in 1..=100_u64 {
        let mut random = seed;
        let mut at = 0;
        let mut cuts = Vec::new();
        while at < bytes.len() {
            random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
            at = (at + 1 + (random as usize % 11)).min(bytes.len());
            cuts.push(at);
        }
        assert_eq!(trace(bytes, &cuts), expected);
    }
}

#[test]
fn complete_osc_and_dcs_are_not_newly_rejected_at_the_pending_limit() {
    for (intro, kind) in [(b']', StringKind::Osc), (b'P', StringKind::Dcs)] {
        let mut bytes = vec![0x1b, intro];
        bytes.extend([b'A'; 200]);
        bytes.extend_from_slice(b"\x1b\\");
        let mut p = Parser::new(limits());
        let step = p.next(&bytes);
        assert_eq!(step.consumed, bytes.len());
        match step.event.unwrap() {
            Event::String {
                kind: actual,
                payload,
                ..
            } => {
                assert_eq!(actual, kind);
                assert_eq!(payload.len(), 200);
            }
            other => panic!("{other:?}"),
        }
    }
}
