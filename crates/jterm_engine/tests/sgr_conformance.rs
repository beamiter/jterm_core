//! Hand-written SGR state and DECRQSS wire expectations (Ember policy only).
use jterm_engine::{
    parser::{Event, Parser, ParserLimits, ParserProfile, PendingKind},
    protocol_state::ProtocolState,
    screen::ScreenState,
    semantics::{dispatch_local, Dispatch, Effect},
    Color, TerminalGrid, UnderlineStyle,
};

#[derive(Debug, Clone, PartialEq, Eq)]
struct Rendition {
    fg: Color,
    bg: Color,
    // bold, dim, italic, blink, inverse, strikethrough, protected
    flags: [bool; 7],
    underline: UnderlineStyle,
}
fn plain() -> Rendition {
    Rendition {
        fg: Color::Default,
        bg: Color::Default,
        flags: [false; 7],
        underline: UnderlineStyle::None,
    }
}
fn snapshot(s: &ScreenState) -> Rendition {
    let f = s.current_flags;
    Rendition {
        fg: s.current_fg,
        bg: s.current_bg,
        flags: [
            f.bold(),
            f.dim(),
            f.italic(),
            f.blink(),
            f.inverse(),
            f.strikethrough(),
            f.protected(),
        ],
        underline: f.underline(),
    }
}
struct Stage<'a> {
    input: &'a [u8],
    state: Rendition,
    reply: &'a [u8],
}
fn check(stages: &[Stage<'_>]) {
    // Chunk boundaries are varied independently in every stage, including CSI/DCS.
    for chunk in 1..=32 {
        let mut s = ScreenState::with_grids(TerminalGrid::new(3, 6), TerminalGrid::new(3, 6));
        s.grid[1][2].character = '界';
        let grid_before = format!("{:?}", (0..3).map(|r| &s.grid[r]).collect::<Vec<_>>());
        let mut p = ProtocolState::default();
        let mut parser = Parser::with_bounded_profile(
            ParserLimits {
                pending_bytes: 128,
                kitty_recovery_bytes: 32,
            },
            ParserProfile::EmberV1,
        )
        .unwrap();
        for stage in stages {
            let mut replies = Vec::new();
            let mut input = stage.input.to_vec();
            input.extend_from_slice(b"\x1bP$qm\x1b\\");
            for bytes in input.chunks(chunk).flat_map(|b| [b, &[][..]]) {
                parser.begin_feed();
                let mut at = 0;
                let mut drained = false;
                for _ in 0..bytes.len() * 4 + 32 {
                    let step = parser.next(&bytes[at..]);
                    assert!(step.consumed <= bytes.len() - at);
                    at += step.consumed;
                    match step.event {
                        Some(Event::Boundary) => (),
                        Some(event) => {
                            let Dispatch::Handled(effects) = dispatch_local(&mut s, &mut p, &event)
                            else {
                                panic!("unhandled {event:?}");
                            };
                            for effect in effects {
                                match effect {
                                    Effect::Reply(reply) => {
                                        // Assert state at the query, not only after the complete stream.
                                        assert_eq!(snapshot(&s), stage.state, "chunk {chunk}");
                                        replies.push(reply);
                                    }
                                    Effect::GlobalBackground(color) => {
                                        assert_eq!(color, s.current_bg)
                                    }
                                    other => panic!("unexpected effect {other:?}"),
                                }
                            }
                        }
                        None => {
                            assert_eq!(at, bytes.len());
                            drained = true;
                            break;
                        }
                    }
                }
                assert!(drained);
            }
            assert_eq!(replies, vec![stage.reply.to_vec()], "chunk {chunk}");
            assert_eq!(snapshot(&s), stage.state);
            assert_eq!(parser.pending_kind(), PendingKind::None);
            assert_eq!(parser.rejected_count(), 0);
            assert_eq!((s.cursor_row, s.cursor_col), (0, 0));
            assert_eq!(
                format!("{:?}", (0..3).map(|r| &s.grid[r]).collect::<Vec<_>>()),
                grid_before
            );
        }
    }
}

#[test]
fn independent_attributes_and_selective_resets() {
    let colored = Rendition {
        fg: Color::Red,
        bg: Color::Blue,
        ..plain()
    };
    check(&[
        Stage {
            input: b"\x1b[1;2;3;4;5;7;9;31;44m",
            state: Rendition {
                flags: [true, true, true, true, true, true, false],
                underline: UnderlineStyle::Single,
                ..colored.clone()
            },
            reply: b"\x1bP1$r1;2;3;4;5;7;9;31;44m\x1b\\",
        },
        Stage {
            input: b"\x1b[22m",
            state: Rendition {
                flags: [false, false, true, true, true, true, false],
                underline: UnderlineStyle::Single,
                ..colored.clone()
            },
            reply: b"\x1bP1$r3;4;5;7;9;31;44m\x1b\\",
        },
        Stage {
            input: b"\x1b[23m",
            state: Rendition {
                flags: [false, false, false, true, true, true, false],
                underline: UnderlineStyle::Single,
                ..colored.clone()
            },
            reply: b"\x1bP1$r4;5;7;9;31;44m\x1b\\",
        },
        Stage {
            input: b"\x1b[24m",
            state: Rendition {
                flags: [false, false, false, true, true, true, false],
                ..colored.clone()
            },
            reply: b"\x1bP1$r5;7;9;31;44m\x1b\\",
        },
        Stage {
            input: b"\x1b[25m",
            state: Rendition {
                flags: [false, false, false, false, true, true, false],
                ..colored.clone()
            },
            reply: b"\x1bP1$r7;9;31;44m\x1b\\",
        },
        Stage {
            input: b"\x1b[27m",
            state: Rendition {
                flags: [false, false, false, false, false, true, false],
                ..colored.clone()
            },
            reply: b"\x1bP1$r9;31;44m\x1b\\",
        },
        Stage {
            input: b"\x1b[29m",
            state: colored,
            reply: b"\x1bP1$r31;44m\x1b\\",
        },
    ]);
}
#[test]
fn independent_named_indexed_and_rgb_color_queries() {
    check(&[
        Stage {
            input: b"\x1b[91;104m",
            state: Rendition {
                fg: Color::BrightRed,
                bg: Color::BrightBlue,
                ..plain()
            },
            reply: b"\x1bP1$r91;104m\x1b\\",
        },
        Stage {
            input: b"\x1b[38;5;201;48;2;10;20;30m",
            state: Rendition {
                fg: Color::Indexed(201),
                bg: Color::Rgb(10, 20, 30),
                ..plain()
            },
            reply: b"\x1bP1$r38;5;201;48;2;10;20;30m\x1b\\",
        },
        Stage {
            input: b"\x1b[38:2::1:2:3;48:5:42m",
            state: Rendition {
                fg: Color::Rgb(1, 2, 3),
                bg: Color::Indexed(42),
                ..plain()
            },
            reply: b"\x1bP1$r38;2;1;2;3;48;5;42m\x1b\\",
        },
        Stage {
            input: b"\x1b[39m",
            state: Rendition {
                bg: Color::Indexed(42),
                ..plain()
            },
            reply: b"\x1bP1$r48;5;42m\x1b\\",
        },
        Stage {
            input: b"\x1b[49m",
            state: plain(),
            reply: b"\x1bP1$r0m\x1b\\",
        },
    ]);
}
#[test]
fn semicolon_is_not_an_underline_subparameter() {
    check(&[
        Stage {
            input: b"\x1b[4;3m",
            state: Rendition {
                flags: [false, false, true, false, false, false, false],
                underline: UnderlineStyle::Single,
                ..plain()
            },
            reply: b"\x1bP1$r3;4m\x1b\\",
        },
        Stage {
            input: b"\x1b[23;4:3m",
            state: Rendition {
                underline: UnderlineStyle::Curly,
                ..plain()
            },
            reply: b"\x1bP1$r4:3m\x1b\\",
        },
        Stage {
            input: b"\x1b[4:2m",
            state: Rendition {
                underline: UnderlineStyle::Double,
                ..plain()
            },
            reply: b"\x1bP1$r21m\x1b\\",
        },
        Stage {
            input: b"\x1b[4:0m",
            state: plain(),
            reply: b"\x1bP1$r0m\x1b\\",
        },
    ]);
}
#[test]
fn explicit_and_omitted_sgr_reset_preserve_dec_character_protection() {
    let protected = Rendition {
        flags: [false, false, false, false, false, false, true],
        ..plain()
    };
    let decorated = Rendition {
        fg: Color::Red,
        bg: Color::Blue,
        flags: [true, true, true, true, true, true, true],
        underline: UnderlineStyle::Curly,
    };
    check(&[
        Stage {
            input: b"\x1b[1\"q\x1b[1;2;3;4:3;5;7;9;31;44m",
            state: decorated.clone(),
            reply: b"\x1bP1$r1;2;3;4:3;5;7;9;31;44m\x1b\\",
        },
        Stage {
            input: b"\x1b[0m",
            state: protected.clone(),
            reply: b"\x1bP1$r0m\x1b\\",
        },
        Stage {
            input: b"\x1b[1;2;3;4:3;5;7;9;31;44m",
            state: decorated,
            reply: b"\x1bP1$r1;2;3;4:3;5;7;9;31;44m\x1b\\",
        },
        Stage {
            input: b"\x1b[m",
            state: protected,
            reply: b"\x1bP1$r0m\x1b\\",
        },
        Stage {
            input: b"\x1b[0\"q",
            state: plain(),
            reply: b"\x1bP1$r0m\x1b\\",
        },
    ]);
}
#[test]
fn cancelled_sgr_cannot_mutate_existing_rendition() {
    for cancel in [0x18, 0x1a] {
        let mut cancelled = b"\x1b[0;32".to_vec();
        cancelled.push(cancel);
        check(&[
            Stage {
                input: b"\x1b[1;31m",
                state: Rendition {
                    fg: Color::Red,
                    flags: [true, false, false, false, false, false, false],
                    ..plain()
                },
                reply: b"\x1bP1$r1;31m\x1b\\",
            },
            Stage {
                input: &cancelled,
                state: Rendition {
                    fg: Color::Red,
                    flags: [true, false, false, false, false, false, false],
                    ..plain()
                },
                reply: b"\x1bP1$r1;31m\x1b\\",
            },
        ]);
    }
}
