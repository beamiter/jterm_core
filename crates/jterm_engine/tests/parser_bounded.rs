use jterm_engine::parser::{Event, Parser, ParserLimits, ParserProfile, PendingKind};

fn parser(profile: ParserProfile, limit: usize) -> Parser {
    Parser::with_bounded_profile(
        ParserLimits {
            pending_bytes: limit,
            kitty_recovery_bytes: 64,
        },
        profile,
    )
    .unwrap()
}
fn feed(p: &mut Parser, bytes: &[u8], out: &mut Vec<String>) {
    p.begin_feed();
    let mut at = 0;
    for _ in 0..bytes.len() * 4 + 20 {
        let step = p.next(&bytes[at..]);
        assert!(step.consumed <= bytes.len() - at);
        at += step.consumed;
        assert!(
            p.retained_bytes() <= 128 + 3,
            "carry exceeds bounded payload + framing"
        );
        match step.event {
            Some(Event::String { kind, payload, .. }) => {
                out.push(format!("string:{kind:?}:{}", payload.len()))
            }
            Some(Event::Ascii(bytes)) => {
                out.extend(bytes.iter().map(|b| format!("print:{}", *b as char)))
            }
            Some(Event::Print(ch)) => out.push(format!("print:{ch}")),
            Some(Event::Escape {
                intermediates,
                final_byte,
            }) => out.push(format!("escape:{}:{final_byte}", intermediates.len())),
            Some(Event::Csi { final_byte, .. }) => out.push(format!("csi:{final_byte}")),
            Some(Event::Boundary) => (),
            Some(event) => out.push(format!("other:{event:?}")),
            None => {
                assert_eq!(at, bytes.len());
                return;
            }
        }
    }
    panic!("bounded parser made no progress");
}
fn trace(profile: ParserProfile, bytes: &[u8], cut: usize) -> (Vec<String>, u64) {
    let mut p = parser(profile, 128);
    let mut out = vec![];
    feed(&mut p, &bytes[..cut], &mut out);
    feed(&mut p, &bytes[cut..], &mut out);
    assert_eq!(p.pending_kind(), PendingKind::None);
    (out, p.rejected_count())
}
#[test]
fn string_boundaries_are_identical_at_every_split_in_both_profiles() {
    for profile in [ParserProfile::EmberV1, ParserProfile::FrostLegacyV1] {
        for intro in *b"]P_X^" {
            for len in [127, 128, 129, 1024] {
                let mut input = vec![27, intro];
                input.extend(vec![b'a'; len]);
                input.extend(b"\x1b\\Z");
                let expected = trace(profile, &input, 0);
                assert_eq!(expected.1, u64::from(len > 128));
                if len > 128 || matches!(intro, b'X' | b'^') {
                    assert_eq!(expected.0, ["print:Z"]);
                } else {
                    assert_eq!(expected.0.len(), 2);
                }
                for cut in 0..=input.len() {
                    assert_eq!(
                        trace(profile, &input, cut),
                        expected,
                        "intro={intro} len={len} cut={cut}"
                    );
                }
            }
        }
    }
}
#[test]
fn overflow_discards_embedded_escape_commands_until_real_termination() {
    for profile in [ParserProfile::EmberV1, ParserProfile::FrostLegacyV1] {
        for intro in *b"]P_X^" {
            for term in [b"\x1b\\".as_slice(), b"\x18", b"\x1a"] {
                let mut input = vec![27, intro];
                input.extend(vec![b'a'; 129]);
                input.extend(b"\x1b[2JSECRET\x1b]52;clipboard\x1b[6n");
                input.extend(term);
                input.extend(b"\x1b[31mZ");
                for cut in 0..=input.len() {
                    assert_eq!(
                        trace(profile, &input, cut),
                        (vec!["csi:109".into(), "print:Z".into()], 1)
                    );
                }
            }
        }
    }
}
#[test]
fn osc_bel_and_split_st_do_not_count_terminators_as_payload() {
    for profile in [ParserProfile::EmberV1, ParserProfile::FrostLegacyV1] {
        for len in [128, 129] {
            let mut input = b"\x1b]".to_vec();
            input.extend(vec![b'a'; len]);
            input.extend(b"\x07Q");
            for cut in 0..=input.len() {
                let (out, rejected) = trace(profile, &input, cut);
                assert_eq!(rejected, u64::from(len > 128));
                assert_eq!(out.last().unwrap(), "print:Q");
                assert_eq!(out.len(), if len > 128 { 1 } else { 2 });
            }
        }
        let mut p = parser(profile, 128);
        let mut out = vec![];
        feed(&mut p, b"\x1bP", &mut out);
        for _ in 0..128 {
            feed(&mut p, b"a", &mut out);
        }
        feed(&mut p, b"\x1b", &mut out);
        assert_eq!(p.retained_bytes(), 131);
        feed(&mut p, b"\\Z", &mut out);
        assert_eq!(out, ["string:Dcs:128", "print:Z"]);
        assert_eq!(p.rejected_count(), 0);
    }
}
#[test]
fn escape_intermediate_hard_limit_and_recovery_are_partition_independent() {
    for profile in [ParserProfile::EmberV1, ParserProfile::FrostLegacyV1] {
        for len in [8, 9, 1024] {
            let mut input = vec![27];
            input.extend(vec![b'%'; len]);
            input.extend(b"QZ");
            let expected = trace(profile, &input, 0);
            assert_eq!(expected.1, u64::from(len > 8));
            assert_eq!(expected.0.last().unwrap(), "print:Z");
            for cut in 0..=input.len() {
                assert_eq!(trace(profile, &input, cut), expected);
            }
        }
    }
}
#[test]
fn oversized_carry_stays_constant_and_reset_recovers_without_resetting_counter() {
    for profile in [ParserProfile::EmberV1, ParserProfile::FrostLegacyV1] {
        let mut p = parser(profile, 128);
        let mut out = vec![];
        feed(&mut p, b"\x1bP", &mut out);
        for _ in 0..1000 {
            feed(&mut p, &[b'a'; 128], &mut out);
        }
        assert!(out.is_empty());
        assert_eq!(p.retained_bytes(), 0);
        assert_eq!(p.rejected_count(), 1);
        p.reset();
        feed(&mut p, b"Q", &mut out);
        assert_eq!(out, ["print:Q"]);
        assert_eq!(p.rejected_count(), 1);
    }
}
#[test]
fn bounded_configuration_rejects_unreasonable_limits_and_legacy_stays_opt_in() {
    for n in [0, 16 * 1024 * 1024 + 1, usize::MAX] {
        assert!(Parser::with_bounded_profile(
            ParserLimits {
                pending_bytes: n,
                kitty_recovery_bytes: 0
            },
            ParserProfile::EmberV1
        )
        .is_err());
    }
    let mut p = Parser::new(ParserLimits {
        pending_bytes: 128,
        kitty_recovery_bytes: 64,
    });
    let mut out = vec![];
    let mut input = b"\x1b]".to_vec();
    input.extend(vec![b'a'; 1024]);
    input.push(7);
    feed(&mut p, &input, &mut out);
    assert_eq!(out, ["string:Osc:1024"]);
    assert_eq!(p.rejected_count(), 0);
}

#[test]
fn small_limits_cancel_fresh_escape_and_empty_strings_recover_normally() {
    for profile in [ParserProfile::EmberV1, ParserProfile::FrostLegacyV1] {
        for input in [
            b"\x1b]\x07Q".as_slice(),
            b"\x1bP\x1b\\Q",
            b"\x1b_\x18Q",
            b"\x1b^\x1aQ",
        ] {
            for cut in 0..=input.len() {
                let mut p = parser(profile, 1);
                let mut out = vec![];
                feed(&mut p, &input[..cut], &mut out);
                feed(&mut p, &input[cut..], &mut out);
                assert_eq!(out.last().unwrap(), "print:Q");
                assert_eq!(p.rejected_count(), 0);
                assert_eq!(p.pending_kind(), PendingKind::None);
            }
        }
        // Fresh ESC restarts a not-yet-rejected string exactly as before.
        let mut p = parser(profile, 1);
        let mut out = vec![];
        feed(&mut p, b"\x1b]a\x1b", &mut out);
        feed(&mut p, b"[31mQ", &mut out);
        assert_eq!(out, ["csi:109", "print:Q"]);
        assert_eq!(p.rejected_count(), 0);
        // A rejected string does not restart at that same ESC sequence.
        let mut p = parser(profile, 1);
        let mut out = vec![];
        feed(&mut p, b"\x1b]ab\x1b", &mut out);
        feed(&mut p, b"[31mQ\x07Z", &mut out);
        assert_eq!(out, ["print:Z"]);
        assert_eq!(p.rejected_count(), 1);
        let mut p = parser(profile, 1);
        let mut out = vec![];
        feed(&mut p, b"\x1b%%\x18Z", &mut out);
        assert_eq!(out, ["print:Z"]);
        assert_eq!(p.rejected_count(), 1);
    }
}
#[test]
fn many_complete_oversized_strings_each_reject_once_without_retaining_payload() {
    for profile in [ParserProfile::EmberV1, ParserProfile::FrostLegacyV1] {
        let mut p = parser(profile, 128);
        let mut out = vec![];
        let mut input = vec![];
        for _ in 0..100 {
            input.extend(b"\x1b_G");
            input.extend([b'x'; 128]);
            input.extend(b"\x1b\\");
        }
        input.extend(b"Z");
        feed(&mut p, &input, &mut out);
        assert_eq!(out, ["print:Z"]);
        assert_eq!(p.rejected_count(), 100);
        assert_eq!(p.retained_bytes(), 0);
    }
}

#[test]
fn discarded_private_content_never_enters_frost_capture_but_following_text_does() {
    use jterm_engine::parser::CaptureEnd;
    for intro in *b"]P_X^" {
        let mut input = vec![27, intro];
        input.extend([b'x'; 129]);
        input.extend(b"\x1b[31mPRIVATE\x1b\\Z");
        for cut in 0..=input.len() {
            let mut p = parser(ParserProfile::FrostLegacyV1, 128);
            let mut captured = vec![];
            for part in [&input[..cut], &input[cut..]] {
                p.begin_feed();
                let mut at = 0;
                loop {
                    let step = p.next(&part[at..]);
                    at += step.consumed;
                    if let Some(capture) = step.capture {
                        if let Some(CaptureEnd::Complete {
                            token: Some(token), ..
                        }) = capture.end
                        {
                            captured.extend(token.prefix);
                            captured.extend_from_slice(&part[token.input_range]);
                        }
                    }
                    if step.event.is_none() {
                        break;
                    }
                }
            }
            assert_eq!(captured, b"Z", "intro={intro} cut={cut}");
        }
    }
}
#[test]
fn eight_megabyte_complete_or_chunked_strings_do_not_escape_the_hard_bound() {
    for profile in [ParserProfile::EmberV1, ParserProfile::FrostLegacyV1] {
        for intro in *b"]P" {
            let mut input = vec![27, intro];
            input.resize(8 * 1024 * 1024, b'a');
            input.extend(b"\x1b\\Z");
            for chunk in [input.len(), 4096] {
                let mut p = parser(profile, 128);
                let mut out = vec![];
                for part in input.chunks(chunk) {
                    feed(&mut p, part, &mut out);
                }
                assert_eq!(out, ["print:Z"]);
                assert_eq!(p.rejected_count(), 1);
                assert_eq!(p.retained_bytes(), 0);
            }
        }
    }
}

#[test]
fn bel_does_not_terminate_non_osc_discard() {
    for profile in [ParserProfile::EmberV1, ParserProfile::FrostLegacyV1] {
        for intro in *b"P_X^" {
            let mut input = vec![27, intro];
            input.extend([b'x'; 129]);
            input.extend(b"\x07SECRET\x1b[31m\x1b\\Z");
            for cut in 0..=input.len() {
                assert_eq!(trace(profile, &input, cut), (vec!["print:Z".into()], 1));
            }
        }
    }
}
