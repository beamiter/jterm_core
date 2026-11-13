//! Handwritten framing expectations. See docs/PARSER_CONFORMANCE.md.
//! No baseline parser run, host trace, or frozen implementation oracle supplies expectations.
use jterm_engine::parser::{Event, Parser, ParserLimits, ParserProfile, PendingKind, StringKind};

#[derive(Debug, PartialEq, Eq)]
enum Token {
    Print(char),
    Control(u8),
    Escape(Vec<u8>, u8),
    Charset(u8, u8),
    Csi(Vec<u8>, Vec<u16>, Vec<bool>, Option<u8>, Vec<u8>, u8),
    String(StringKind, Vec<u8>, bool),
}
fn text(s: &str) -> Vec<Token> {
    s.chars().map(Token::Print).collect()
}
fn csi(
    raw: &[u8],
    params: &[u16],
    prefix: Option<u8>,
    intermediate: &[u8],
    final_byte: u8,
) -> Token {
    Token::Csi(
        raw.to_vec(),
        params.to_vec(),
        vec![false; params.len()],
        prefix,
        intermediate.to_vec(),
        final_byte,
    )
}
fn string(kind: StringKind, payload: &[u8], bel: bool) -> Token {
    Token::String(kind, payload.to_vec(), bel)
}
#[derive(Debug, PartialEq, Eq)]
struct Trace {
    tokens: Vec<Token>,
    rejected: u64,
    abandoned_apc: usize,
}
fn run(profile: ParserProfile, bytes: &[u8], cuts: &[usize], limit: usize) -> Trace {
    let mut parser = Parser::with_bounded_profile(
        ParserLimits {
            pending_bytes: limit,
            kitty_recovery_bytes: 32,
        },
        profile,
    )
    .unwrap();
    let mut trace = Trace {
        tokens: vec![],
        rejected: 0,
        abandoned_apc: 0,
    };
    let mut start = 0;
    for end in cuts.iter().copied().chain(std::iter::once(bytes.len())) {
        parser.begin_feed();
        let input = &bytes[start..end];
        let mut at = 0;
        let mut done = false;
        for _ in 0..input.len() * 4 + 32 {
            let step = parser.next(&input[at..]);
            assert!(step.consumed <= input.len() - at);
            at += step.consumed;
            trace.abandoned_apc += usize::from(step.abandoned_apc);
            assert!(parser.retained_bytes() <= limit + 3);
            if parser.pending_kind() == PendingKind::Csi {
                let cap = match profile {
                    ParserProfile::EmberV1 => 256,
                    ParserProfile::FrostLegacyV1 => 128,
                };
                assert!(parser.retained_bytes() <= cap + 8 + 2);
            }
            match step.event {
                Some(Event::Ascii(run)) => trace
                    .tokens
                    .extend(run.iter().map(|b| Token::Print(char::from(*b)))),
                Some(Event::Print(ch)) => trace.tokens.push(Token::Print(ch)),
                Some(Event::Control(b)) => trace.tokens.push(Token::Control(b)),
                Some(Event::Escape {
                    intermediates,
                    final_byte,
                }) => trace.tokens.push(Token::Escape(intermediates, final_byte)),
                Some(Event::Charset { bank, designator }) => {
                    trace.tokens.push(Token::Charset(bank, designator))
                }
                Some(Event::Csi {
                    raw_params,
                    params,
                    colon_flags,
                    private_prefix,
                    intermediates,
                    final_byte,
                }) => trace.tokens.push(Token::Csi(
                    raw_params,
                    params,
                    colon_flags,
                    private_prefix,
                    intermediates,
                    final_byte,
                )),
                Some(Event::String {
                    kind,
                    payload,
                    bel_terminated,
                }) => trace
                    .tokens
                    .push(Token::String(kind, payload.into_owned(), bel_terminated)),
                Some(Event::Boundary) => (), // Adapter notification, not a protocol semantic token.
                Some(Event::RejectApc { .. }) => {
                    panic!("bounded mode must not offer reflective recovery")
                }
                None => {
                    assert_eq!(at, input.len());
                    done = true;
                    break;
                }
            }
        }
        assert!(done, "parser failed to drain bounded feed");
        start = end;
    }
    assert_eq!(parser.pending_kind(), PendingKind::None);
    trace.rejected = parser.rejected_count();
    trace
}
fn check_profile(profile: ParserProfile, bytes: &[u8], expected: &Trace, limit: usize) {
    for cut in 0..=bytes.len() {
        assert_eq!(
            &run(profile, bytes, &[cut], limit),
            expected,
            "{profile:?} cut={cut} input={bytes:?}"
        );
    }
    let cuts: Vec<_> = (0..=bytes.len()).flat_map(|n| [n, n]).collect();
    assert_eq!(
        &run(profile, bytes, &cuts, limit),
        expected,
        "bytewise and empty feeds: {profile:?}"
    );
}
fn check(bytes: &[u8], tokens: Vec<Token>, limit: usize, rejected: u64, abandoned_apc: usize) {
    let expected = Trace {
        tokens,
        rejected,
        abandoned_apc,
    };
    for profile in [ParserProfile::EmberV1, ParserProfile::FrostLegacyV1] {
        check_profile(profile, bytes, &expected, limit);
    }
}
#[test]
fn printable_utf8_and_c0_controls() {
    let mut expected = text("Aé界🙂");
    expected.extend([
        Token::Control(13),
        Token::Control(10),
        Token::Control(8),
        Token::Control(9),
        Token::Control(7),
    ]);
    expected.extend(text("Z"));
    check("Aé界🙂\r\n\x08\t\x07Z".as_bytes(), expected, 128, 0, 0);
}
#[test]
fn csi_standard_syntax_and_xterm_extensions() {
    check(
        b"\x1b[12;34H\x1b[?25l\x1b[2 q\x1b[>0c",
        vec![
            csi(b"12;34", &[12, 34], None, b"", b'H'),
            csi(b"25", &[25], Some(b'?'), b"", b'l'),
            csi(b"2", &[2], None, b" ", b'q'),
            csi(b"0", &[0], Some(b'>'), b"", b'c'),
        ],
        128,
        0,
        0,
    );
}
#[test]
fn empty_fields_and_colon_syntax_preserve_raw_bytes() {
    check(
        b"\x1b[;5H\x1b[38:2::1:2:3m",
        vec![
            csi(b";5", &[0, 5], None, b"", b'H'),
            Token::Csi(
                b"38:2::1:2:3".to_vec(),
                vec![38, 2, 0, 1, 2, 3],
                vec![false, true, true, true, true, true],
                None,
                vec![],
                b'm',
            ),
        ],
        128,
        0,
        0,
    );
}
#[test]
fn embedded_c0_executes_without_ending_csi() {
    check(
        b"\x1b[12\r\n;34H",
        vec![
            Token::Control(13),
            Token::Control(10),
            csi(b"12;34", &[12, 34], None, b"", b'H'),
        ],
        128,
        0,
        0,
    );
}
#[test]
fn escape_and_charset_framing() {
    for profile in [ParserProfile::EmberV1, ParserProfile::FrostLegacyV1] {
        let mut tokens = vec![Token::Escape(vec![], b'7'), Token::Escape(vec![], b'8')];
        // Explicit public API difference, not a normalization of observed output.
        tokens.extend(match profile {
            ParserProfile::EmberV1 => vec![Token::Charset(b'(', b'0'), Token::Charset(b'(', b'B')],
            ParserProfile::FrostLegacyV1 => vec![
                Token::Escape(vec![b'('], b'0'),
                Token::Escape(vec![b'('], b'B'),
            ],
        });
        check_profile(
            profile,
            b"\x1b7\x1b8\x1b(0\x1b(B",
            &Trace {
                tokens,
                rejected: 0,
                abandoned_apc: 0,
            },
            128,
        );
    }
}
#[test]
fn osc_bel_and_st_are_distinguished() {
    check(
        b"\x1b]0;title\x07\x1b]2;title\x1b\\Z",
        vec![
            string(StringKind::Osc, b"0;title", true),
            string(StringKind::Osc, b"2;title", false),
            Token::Print('Z'),
        ],
        128,
        0,
        0,
    );
}
#[test]
fn dcs_and_apc_payloads_are_not_executed() {
    check(
        b"\x1bP$qm\x1b\\\x1b_Gi=1;AA\x1b\\Z",
        vec![
            string(StringKind::Dcs, b"$qm", false),
            string(StringKind::Apc, b"Gi=1;AA", false),
            Token::Print('Z'),
        ],
        128,
        0,
        0,
    );
}
#[test]
fn vim_probe_keeps_unknown_dcs_and_csi_intermediate() {
    check(
        b"\x1bPzz\x1b\\\x1b[0%m\x1b[6n",
        vec![
            string(StringKind::Dcs, b"zz", false),
            csi(b"0", &[0], None, b"%", b'm'),
            csi(b"6", &[6], None, b"", b'n'),
        ],
        128,
        0,
        0,
    );
}
#[test]
fn can_and_sub_cancel_incomplete_sequences() {
    for cancel in [0x18, 0x1a] {
        for prefix in [
            b"\x1b[12".as_slice(),
            b"\x1b]0;bad",
            b"\x1bP$q",
            b"\x1b_Gi=1;AA",
        ] {
            let mut bytes = prefix.to_vec();
            bytes.extend([cancel, b'Z']);
            check(
                &bytes,
                text("Z"),
                128,
                0,
                usize::from(prefix.starts_with(b"\x1b_")),
            );
        }
    }
}
#[test]
fn fresh_escape_restarts_before_resource_rejection() {
    check(
        b"\x1b]bad\x1b[31mZ",
        vec![csi(b"31", &[31], None, b"", b'm'), Token::Print('Z')],
        128,
        0,
        0,
    );
}
#[test]
fn local_policy_exact_cap_and_cap_plus_one() {
    for (intro, kind) in [
        (b']', StringKind::Osc),
        (b'P', StringKind::Dcs),
        (b'_', StringKind::Apc),
    ] {
        for len in [8, 9] {
            let mut bytes = vec![27, intro];
            bytes.extend(vec![b'a'; len]);
            bytes.extend(b"\x1b\\Z");
            let expected = if len == 8 {
                vec![string(kind, &[b'a'; 8], false), Token::Print('Z')]
            } else {
                text("Z")
            };
            check(
                &bytes,
                expected,
                8,
                u64::from(len > 8),
                usize::from(len > 8 && kind == StringKind::Apc),
            );
        }
    }
}
#[test]
fn local_policy_discard_prevents_embedded_commands() {
    for intro in *b"]P_X^" {
        for terminator in [b"\x1b\\".as_slice(), b"\x18", b"\x1a"] {
            let mut bytes = vec![27, intro];
            bytes.extend(b"123456789\x1b[2J\x1b]52;c;SECRET\x1b[6n");
            bytes.extend(terminator);
            bytes.extend(b"\x1b[31mZ");
            check(
                &bytes,
                vec![csi(b"31", &[31], None, b"", b'm'), Token::Print('Z')],
                8,
                1,
                usize::from(intro == b'_'),
            );
        }
    }
}

// RFC 3629 section 3 / Unicode scalar boundaries, not a host-renderer oracle.
#[test]
fn unicode_scalar_encoding_boundaries_are_preserved() {
    check(
        b"~\xc2\x80\xdf\xbf\xe0\xa0\x80\xed\x9f\xbf\xee\x80\x80\xef\xbf\xbf\xf0\x90\x80\x80\xf4\x8f\xbf\xbf",
        vec![
            Token::Print('~'), Token::Print('\u{80}'), Token::Print('\u{7ff}'),
            Token::Print('\u{800}'), Token::Print('\u{d7ff}'), Token::Print('\u{e000}'),
            Token::Print('\u{ffff}'), Token::Print('\u{10000}'), Token::Print('\u{10ffff}'),
        ],
        128, 0, 0,
    );
}

#[test]
fn unicode_cluster_components_are_not_normalized_or_dropped() {
    // Combining accent, VS16, ZWJ emoji and regional indicators remain scalars.
    // This does NOT assert grapheme segmentation, cell width or presentation.
    check(
        b"e\xcc\x81\xe2\x9d\xa4\xef\xb8\x8f\xf0\x9f\x91\xa9\xe2\x80\x8d\xf0\x9f\x92\xbb\xf0\x9f\x87\xa8\xf0\x9f\x87\xb3",
        vec![
            Token::Print('e'), Token::Print('\u{301}'), Token::Print('\u{2764}'),
            Token::Print('\u{fe0f}'), Token::Print('\u{1f469}'), Token::Print('\u{200d}'),
            Token::Print('\u{1f4bb}'), Token::Print('\u{1f1e8}'), Token::Print('\u{1f1f3}'),
        ],
        128, 0, 0,
    );
}

#[test]
fn tui_lifecycle_unicode_and_query_survive_every_two_cut_partition() {
    // Hand-authored TUI syntax: alternate screen, hide cursor, CUP, Unicode text,
    // SGR, CPR query, restore cursor and primary screen. Not a captured Vim trace.
    let bytes = b"\x1b[?1049h\x1b[?25l\x1b[2;3H\xe7\x95\x8ce\xcc\x81\x1b[31mX\x1b[0m\x1b[6n\x1b[?25h\x1b[?1049l";
    let expected = Trace {
        tokens: vec![
            csi(b"1049", &[1049], Some(b'?'), b"", b'h'),
            csi(b"25", &[25], Some(b'?'), b"", b'l'),
            csi(b"2;3", &[2, 3], None, b"", b'H'),
            Token::Print('\u{754c}'),
            Token::Print('e'),
            Token::Print('\u{301}'),
            csi(b"31", &[31], None, b"", b'm'),
            Token::Print('X'),
            csi(b"0", &[0], None, b"", b'm'),
            csi(b"6", &[6], None, b"", b'n'),
            csi(b"25", &[25], Some(b'?'), b"", b'h'),
            csi(b"1049", &[1049], Some(b'?'), b"", b'l'),
        ],
        rejected: 0,
        abandoned_apc: 0,
    };
    for profile in [ParserProfile::EmberV1, ParserProfile::FrostLegacyV1] {
        check_profile(profile, bytes, &expected, 128);
        for first in 0..=bytes.len() {
            for second in first..=bytes.len() {
                assert_eq!(
                    run(profile, bytes, &[first, second], 128),
                    expected,
                    "{profile:?} two-cut partition {first}, {second}"
                );
            }
        }
    }
}

#[test]
fn utf8_encoded_c1_scalars_do_not_reenter_control_parser() {
    // XTerm Control Bytes: C1 obtained by UTF-8 decoding is not reprocessed.
    // These are encoded Unicode scalars, not the single-byte C1 control form.
    check(
        b"\xc2\x9b31mX\xc2\x9d2;title\xc2\x9cZ",
        vec![
            Token::Print('\u{9b}'),
            Token::Print('3'),
            Token::Print('1'),
            Token::Print('m'),
            Token::Print('X'),
            Token::Print('\u{9d}'),
            Token::Print('2'),
            Token::Print(';'),
            Token::Print('t'),
            Token::Print('i'),
            Token::Print('t'),
            Token::Print('l'),
            Token::Print('e'),
            Token::Print('\u{9c}'),
            Token::Print('Z'),
        ],
        128,
        0,
        0,
    );
}

// These bounds/saturation are compatibility API policy, not ECMA-48 mandates.
#[test]
fn local_policy_csi_parameter_storage_caps_are_profile_specific() {
    for (profile, cap) in [
        (ParserProfile::EmberV1, 256),
        (ParserProfile::FrostLegacyV1, 128),
    ] {
        for length in [cap - 1, cap, cap + 1, cap + 17] {
            let mut input = b"\x1b[".to_vec();
            input.extend(vec![b';'; length]);
            input.extend_from_slice(b"mZ\x1b[2J");
            let retained = length.min(cap);
            let expected = Trace {
                tokens: vec![
                    csi(
                        &vec![b';'; retained],
                        &vec![0; retained + 1],
                        None,
                        b"",
                        b'm',
                    ),
                    Token::Print('Z'),
                    csi(b"2", &[2], None, b"", b'J'),
                ],
                rejected: 0,
                abandoned_apc: 0,
            };
            check_profile(profile, &input, &expected, 1024);
        }
    }
}

#[test]
fn local_policy_csi_prefix_uses_one_storage_byte() {
    for (profile, cap) in [
        (ParserProfile::EmberV1, 256),
        (ParserProfile::FrostLegacyV1, 128),
    ] {
        let mut input = b"\x1b[?".to_vec();
        input.extend(vec![b'0'; cap - 1]);
        // This nonzero sentinel is beyond the cap. It must not change zero to nine.
        input.extend_from_slice(b"9hQ");
        check_profile(
            profile,
            &input,
            &Trace {
                tokens: vec![
                    csi(&vec![b'0'; cap - 1], &[0], Some(b'?'), b"", b'h'),
                    Token::Print('Q'),
                ],
                rejected: 0,
                abandoned_apc: 0,
            },
            1024,
        );
    }
}

#[test]
fn local_policy_csi_intermediate_cap_preserves_next_sequence() {
    for count in [7usize, 8, 9, 25] {
        let mut input = b"\x1b[2".to_vec();
        input.extend(vec![b' '; count.min(8)]);
        input.extend(vec![b'!'; count.saturating_sub(8)]);
        input.extend_from_slice(b"qR\x1b[6n");
        check(
            &input,
            vec![
                csi(b"2", &[2], None, &vec![b' '; count.min(8)], b'q'),
                Token::Print('R'),
                csi(b"6", &[6], None, b"", b'n'),
            ],
            1024,
            0,
            0,
        );
    }
}

#[test]
fn local_policy_csi_numeric_saturation_is_per_field() {
    check(
        b"\x1b[65534;65535;65536;99999999999999999999;1m",
        vec![csi(
            b"65534;65535;65536;99999999999999999999;1",
            &[65534, 65535, 65535, 65535, 1],
            None,
            b"",
            b'm',
        )],
        1024,
        0,
        0,
    );
    check(
        b"\x1b[38:2::65536:1:2m",
        vec![Token::Csi(
            b"38:2::65536:1:2".to_vec(),
            vec![38, 2, 0, 65535, 1, 2],
            vec![false, true, true, true, true, true],
            None,
            vec![],
            b'm',
        )],
        1024,
        0,
        0,
    );
}

#[test]
fn local_policy_csi_at_capacity_cancels_and_restarts() {
    for (profile, cap) in [
        (ParserProfile::EmberV1, 256),
        (ParserProfile::FrostLegacyV1, 128),
    ] {
        for terminator in [b"\x18".as_slice(), b"\x1a", b"\x1b"] {
            let mut input = b"\x1b[".to_vec();
            input.extend(vec![b';'; cap + 17]);
            input.extend_from_slice(terminator);
            // Fresh ESC cancels the full CSI. CAN/SUB leave ground; ESC starts an escape.
            input.extend_from_slice(if terminator == b"\x1b" {
                b"[6nZ"
            } else {
                b"Q\x1b[6nZ"
            });
            let mut expected = Vec::new();
            if terminator != b"\x1b" {
                expected.push(Token::Print('Q'));
            }
            expected.extend([csi(b"6", &[6], None, b"", b'n'), Token::Print('Z')]);
            check_profile(
                profile,
                &input,
                &Trace {
                    tokens: expected,
                    rejected: 0,
                    abandoned_apc: 0,
                },
                1024,
            );
        }
    }
}
