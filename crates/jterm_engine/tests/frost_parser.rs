#[path = "support/frost_scanner_reference.rs"]
mod reference;
use jterm_engine::parser::*;
use reference::Reference;

fn limits() -> ParserLimits {
    ParserLimits {
        pending_bytes: 128,
        kitty_recovery_bytes: 64,
    }
}
fn feed(parser: &mut Parser, host: &mut Reference, bytes: &[u8]) {
    parser.begin_feed();
    let mut at = 0;
    let mut active = false;
    let mut spins = 0;
    loop {
        spins += 1;
        assert!(spins <= bytes.len() * 8 + 32, "spin");
        parser.set_ascii_batching(
            host.active_charset == reference::Charset::Ascii && !host.modes.contains(&4),
        );
        let step = parser.next(&bytes[at..]);
        at += step.consumed;
        if step.capture.as_ref().is_some_and(|c| c.started) {
            active = true;
        }
        let done = step.event.is_none();
        if let Some(event) = step.event {
            match event {
                Event::Boundary => {}
                Event::Print(ch) => host.put_char(ch, true),
                Event::Ascii(bytes) => host.put_ascii_run(bytes),
                Event::Control(byte) => match byte {
                    8 => {
                        host.pending_wrap = false;
                        host.cursor_col = host.cursor_col.saturating_sub(1)
                    }
                    9 => {
                        host.pending_wrap = false;
                        host.cursor_col = host.next_tab_stop(host.cursor_col)
                    }
                    10..=12 => {
                        host.pending_wrap = false;
                        host.index()
                    }
                    13 => {
                        host.pending_wrap = false;
                        host.cursor_col = 0
                    }
                    14 => host.active_charset = host.g1_charset,
                    15 => host.active_charset = host.g0_charset,
                    _ => {}
                },
                Event::Escape {
                    intermediates,
                    final_byte,
                } => {
                    if intermediates.is_empty() {
                        match final_byte {
                            b'7' => host.save_cursor(),
                            b'8' => host.restore_cursor(),
                            b'E' => {
                                host.pending_wrap = false;
                                host.next_line()
                            }
                            b'D' => {
                                host.pending_wrap = false;
                                host.index()
                            }
                            b'M' => {
                                host.pending_wrap = false;
                                host.reverse_index()
                            }
                            b'H' => {
                                if let Some(stop) = host.tab_stops.get_mut(host.cursor_col) {
                                    *stop = true
                                }
                            }
                            b'c' => host.full_reset(),
                            b'>' => {
                                host.modes.remove(&66);
                            }
                            b'=' => {
                                host.modes.insert(66);
                            }
                            _ => {}
                        }
                    } else {
                        host.handle_nf_escape(&intermediates, final_byte)
                    }
                }
                Event::Csi {
                    raw_params,
                    private_prefix,
                    intermediates,
                    final_byte,
                    ..
                } => {
                    let params = Reference::parse_csi_params(&raw_params);
                    host.handle_escape_sequence(
                        &params,
                        &raw_params,
                        final_byte as char,
                        private_prefix,
                        &intermediates,
                    );
                }
                Event::String {
                    kind,
                    payload,
                    bel_terminated,
                } => match kind {
                    StringKind::Osc => host.handle_osc_payload(
                        &payload,
                        if bel_terminated { b"\x07" } else { b"\x1b\\" },
                    ),
                    StringKind::Apc if payload.first() == Some(&b'G') => {
                        host.handle_kitty_graphics_apc(&payload)
                    }
                    _ => {}
                },
                Event::RejectApc { recovery, reason } => {
                    let reason = match reason {
                        RejectReason::Cancelled => "Kitty graphics APC was cancelled",
                        RejectReason::TooLarge => {
                            "Kitty graphics APC exceeded the parser size limit"
                        }
                    };
                    host.kitty_graphics
                        .reject_graphics_payload(&recovery, reason);
                }
                Event::Charset { .. } => panic!("Frost unexpectedly used Ember charset scanner"),
            }
        }
        if let Some(end) = step.capture.and_then(|c| c.end) {
            if let CaptureEnd::Complete { token, escape } = end {
                if escape {
                    host.note_chat_native_takeover();
                }
                if active {
                    if let Some(token) = token {
                        let mut raw = token.prefix;
                        raw.extend(&bytes[token.input_range]);
                        host.append_idle_background_bytes(&raw);
                    }
                }
            }
            active = false;
        }
        if done {
            assert_eq!(at, bytes.len());
            break;
        }
    }
}
fn compare(bytes: &[u8], cuts: &[usize]) {
    let mut old = Reference::new();
    let mut new = Reference::new();
    let mut parser = Parser::with_profile(limits(), ParserProfile::FrostLegacyV1);
    let mut at = 0;
    for end in cuts.iter().copied().chain(std::iter::once(bytes.len())) {
        old.process_input(&bytes[at..end]);
        feed(&mut parser, &mut new, &bytes[at..end]);
        assert_eq!(
            new.events, old.events,
            "events at{end}, cuts{cuts:?}, bytes{bytes:?}"
        );
        assert_eq!(
            new.raw, old.raw,
            "capture at{end}, cuts{cuts:?}, bytes{bytes:?}"
        );
        assert_eq!(
            new.cursor_col, old.cursor_col,
            "cursor at{end}, cuts{cuts:?}, bytes{bytes:?}"
        );
        assert_eq!(
            new.kitty_graphics.rejected, old.kitty_graphics.rejected,
            "reject at{end}, cuts{cuts:?}"
        );
        assert_eq!(
            new.hook_trace, old.hook_trace,
            "takeover hook positions at{end}, cuts{cuts:?}, bytes{bytes:?}"
        );
        let snap = parser.snapshot();
        assert_eq!(
            snap.pending_escape, old.pending_escape,
            "escape at{end}, cuts{cuts:?}"
        );
        assert_eq!(
            snap.pending_osc, old.pending_osc,
            "OSC at{end}, cuts{cuts:?}"
        );
        assert_eq!(
            snap.pending_string, old.pending_dcs,
            "DCS at{end}, cuts{cuts:?}"
        );
        assert_eq!(
            snap.pending_apc, old.pending_apc,
            "APC at{end}, cuts{cuts:?}"
        );
        assert_eq!(snap.utf8_len, old.utf8_len, "UTF8 at{end}, cuts{cuts:?}");
        assert_eq!(
            &snap.utf8_buf[..snap.utf8_len as usize],
            &old.utf8_buf[..old.utf8_len as usize]
        );
        assert_eq!(snap.discarding_oversized_apc, old.discarding_oversized_apc);
        assert_eq!(
            snap.discarding_apc_prev_escape,
            old.discarding_apc_prev_escape
        );
        at = end;
    }
}
#[test]
fn frozen_original_scanner_matches_at_identical_partitions() {
    let cases: &[&[u8]] = &[
        b"\x1b(0abc\x1b(Bdef",
        b"a\x1b[;5H\x1b[38:2::1\r\n:2:3mZ",
        b"\xe0\x80\x80Z\xe2\x82A\xf4\x90\x80\x80Z\xf0\x9f\x1bc",
        b"\x1b(\x1b[31mX\x1b(%5Y\x1b\n\x1b\xc2\xa0\x1b#\nQ",
        b"a\x1b]0;title\x07b\x1bPignored\x1b\\c\x1bXsecret\x1b\\d",
        b"\x1b_Gi=4,q=2;data\x1b\\Z\x1b_Gi=5;bad\x18A",
        b"\x1b[1\r\n;2\x18B\x1b[12\xffLOST\x1b[0mC",
        b"\x1b]abc\x1b[6nZ\x1bPbad\x1bcEND\x1b\x1b#\x1aQ",
        b"\x1b]133;A\x07$ \x1b]133;B\x07\r\nhello\x1b[2K",
    ];
    for bytes in cases {
        for cut in 0..=bytes.len() {
            compare(bytes, &[cut]);
        }
        compare(bytes, &(0..=bytes.len()).collect::<Vec<_>>());
    }
}
#[test]
fn frozen_original_overflow_matches_at_identical_partitions() {
    for prefix in [
        b"\x1b]0;".as_slice(),
        b"\x1bP",
        b"\x1bX",
        b"\x1b^",
        b"\x1b_Gi=4,q=2;",
    ] {
        let mut bytes = prefix.to_vec();
        bytes.resize(150, b'A');
        bytes.extend(b"\x1b\\Z");
        for cut in 0..=bytes.len() {
            compare(&bytes, &[cut]);
        }
        for suffix in [b"\x1b[31mZ".as_slice(), b"\x18Z", b"\x1b", b"tail\x07"] {
            let mut bytes = prefix.to_vec();
            bytes.resize(129, b'A');
            bytes.extend(suffix);
            compare(&bytes, &[128, 129]);
            compare(&bytes, &[129]);
        }
    }
}

#[test]
fn frozen_original_random_multipart_traces_match() {
    let pieces: &[&[u8]] = &[
        b"abc",
        b"\x1b[;3H",
        b"\x1b[1\r\n;2m",
        b"\x1b(0",
        b"\x1b(B",
        b"\xe2\x82A",
        b"\xe0\x80\x80",
        b"\xf4\x90\x80\x80",
        b"\xc2\xa0",
        b"\x1b]0;title\x07",
        b"\x1bPignored\x1b\\",
        b"\x1b_Gi=4;abc\x18",
        b"\x1b_Gi=4;abc\x1b\\",
        b"\x1b\n",
        b"\x1b(\x1b",
        b"\x18",
        b"\x1a",
        b"\r\n",
        b"\x1b[12\xff",
        b"\x1b%G",
        b"\x1b",
        b"\x00\x7f",
    ];
    let mut seed = 0x61738acdf024a1bu64;
    for _ in 0..512 {
        let mut bytes = Vec::new();
        let mut cuts = Vec::new();
        for _ in 0..12 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            bytes.extend(pieces[seed as usize % pieces.len()]);
        }
        let mut at = 0;
        while at < bytes.len() {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            at = (at + 1 + seed as usize % 11).min(bytes.len());
            cuts.push(at);
        }
        compare(&bytes, &cuts);
    }
}

#[test]
fn profiles_keep_distinct_raw_csi_bounds_and_pending_kinds() {
    for (profile, limit) in [
        (ParserProfile::EmberV1, 256),
        (ParserProfile::FrostLegacyV1, 128),
    ] {
        let mut p = Parser::with_profile(limits(), profile);
        let mut bytes = b"\x1b[?".to_vec();
        bytes.extend([b'1'; 300]);
        bytes.push(b'm');
        p.begin_feed();
        let step = p.next(&bytes);
        match step.event.unwrap() {
            Event::Csi {
                raw_params,
                private_prefix,
                ..
            } => {
                assert_eq!(raw_params.len(), limit - 1);
                assert_eq!(private_prefix, Some(b'?'));
                assert!(raw_params.iter().all(|b| *b == b'1'));
            }
            _ => panic!("expected CSI"),
        }
        assert_eq!(p.pending_kind(), PendingKind::None);
    }
    let mut p = Parser::with_profile(limits(), ParserProfile::FrostLegacyV1);
    for (bytes, kind) in [
        (b"\x1b".as_slice(), PendingKind::Escape),
        (b"[1", PendingKind::Csi),
    ] {
        p.begin_feed();
        assert!(p.next(bytes).event.is_none());
        assert_eq!(p.pending_kind(), kind);
    }
    p.reset();
    p.begin_feed();
    assert!(p.next(b"\x1b]title").event.is_none());
    assert_eq!(p.pending_kind(), PendingKind::String(StringKind::Osc));
}

#[test]
fn frost_carry_overflow_and_recovery_keep_feed_boundaries() {
    for prefix in [b"\x1b]".as_slice(), b"\x1bP", b"\x1bX", b"\x1b^"] {
        for suffix in [b"[31mQ".as_slice(), b"\x18Q", b"\\Q", b""] {
            let mut bytes = prefix.to_vec();
            bytes.resize(129, b'A');
            bytes.push(0x1b);
            let cut = bytes.len();
            bytes.extend(suffix);
            compare(&bytes, &[cut]);
        }
    }
    let mut nf = b"\x1b".to_vec();
    nf.extend([b'%'; 130]);
    nf.extend(b"Qtail");
    compare(&nf, &[130]);
    compare(&nf, &[131]);
    for payload in [b"G".as_slice(), b"X", b""] {
        let mut apc = b"\x1b_".to_vec();
        apc.extend(payload);
        apc.extend([b'1'; 80]);
        apc.extend(b"\x18Z");
        for cut in 0..=apc.len() {
            compare(&apc, &[cut]);
        }
    }
}

#[test]
fn frost_unfinished_apc_retention_is_bounded() {
    let mut parser = Parser::with_profile(limits(), ParserProfile::FrostLegacyV1);
    let mut host = Reference::new();
    let mut bytes = b"\x1b_Gi=7,q=2;".to_vec();
    bytes.extend([b'A'; 1024]);
    bytes.extend(b"\x1b\\Z");
    for part in bytes.chunks(1) {
        feed(&mut parser, &mut host, part);
        assert!(parser.retained_bytes() <= limits().pending_bytes);
    }
    assert_eq!(host.kitty_graphics.rejected.len(), 1);
    assert_eq!(host.events, ["PZ"]);
    assert_eq!(host.raw, [b"Z".to_vec()]);
    assert_eq!(parser.pending_kind(), PendingKind::None);
}

#[test]
fn frozen_original_arbitrary_bytes_match_with_identical_cuts() {
    let mut seed = 0x1a983b41c770a139u64;
    for _ in 0..256 {
        let mut bytes = Vec::new();
        for _ in 0..64 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            bytes.push(seed as u8);
        }
        compare(&bytes, &[]);
        compare(&bytes, &[1, 7, 23, 31, 50, 63]);
        compare(&bytes, &(0..=bytes.len()).collect::<Vec<_>>());
    }
}

#[test]
fn carried_strings_preserve_original_takeover_hook_timing_and_apc_own_hook() {
    for (bytes, direct, carried) in [
        (b"\x1b]2;title\x07Z".as_slice(), vec![1], vec![]),
        (b"\x1bPignored\x1b\\Z".as_slice(), vec![0], vec![]),
        (b"\x1b_Gi=7;data\x1b\\Z".as_slice(), vec![1, 1], vec![1]),
    ] {
        // Expected positions are independently produced by the unchanged,
        // frozen original scanner and its production graphics-hook call site.
        let mut original = Reference::new();
        original.process_input(bytes);
        assert_eq!(original.hook_trace, direct);
        let mut original = Reference::new();
        original.process_input(&bytes[..2]);
        original.process_input(&bytes[2..]);
        assert_eq!(original.hook_trace, carried);
        compare(bytes, &[]);
        compare(bytes, &[2]);
    }
}
