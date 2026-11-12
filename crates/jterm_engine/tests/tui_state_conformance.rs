//! Hand-authored state transitions + replies; no frozen implementation oracle.
use jterm_engine::{
    parser::{Event, Parser, ParserLimits, ParserProfile, PendingKind},
    protocol_state::ProtocolState,
    screen::ScreenState,
    semantics::{dispatch_local, Dispatch, Effect},
    TerminalGrid,
};

#[derive(Debug, PartialEq, Eq)]
struct State {
    row: usize,
    col: usize,
    top: usize,
    bottom: usize,
    protected: bool,
}
fn state(s: &ScreenState) -> State {
    State {
        row: s.cursor_row,
        col: s.cursor_col,
        top: s.scroll_region_top,
        bottom: s.scroll_region_bottom,
        protected: s.current_flags.protected(),
    }
}
#[derive(Debug, PartialEq, Eq)]
struct ResultTrace {
    // Snapshots are at each reply, not just at final stream state.
    replies: Vec<(Vec<u8>, State)>,
    final_state: State,
}
fn run(bytes: &[u8], cuts: &[usize]) -> ResultTrace {
    let mut screen = ScreenState::with_grids(TerminalGrid::new(6, 12), TerminalGrid::new(6, 12));
    screen.grid[3][7].character = '界';
    let untouched_rows: Vec<_> = (0..6)
        .map(|row| format!("{:?}", &screen.grid[row]))
        .collect();
    let mut protocol = ProtocolState::default();
    let mut parser = Parser::with_bounded_profile(
        ParserLimits {
            pending_bytes: 128,
            kitty_recovery_bytes: 32,
        },
        ParserProfile::EmberV1,
    )
    .unwrap();
    let mut replies = vec![];
    let mut start = 0;
    for end in cuts.iter().copied().chain(std::iter::once(bytes.len())) {
        parser.begin_feed();
        let input = &bytes[start..end];
        let mut at = 0;
        let mut drained = false;
        for _ in 0..input.len() * 4 + 32 {
            let step = parser.next(&input[at..]);
            assert!(step.consumed <= input.len() - at);
            at += step.consumed;
            match step.event {
                Some(Event::Boundary) => (),
                Some(event) => {
                    let Dispatch::Handled(effects) =
                        dispatch_local(&mut screen, &mut protocol, &event)
                    else {
                        panic!("fixture includes unhandled event: {event:?}");
                    };
                    for effect in effects {
                        match effect {
                            Effect::Reply(bytes) => replies.push((bytes, state(&screen))),
                            Effect::CursorReposition { .. } => (),
                            unexpected => panic!("unexpected effect {unexpected:?}"),
                        }
                    }
                }
                None => {
                    assert_eq!(at, input.len());
                    drained = true;
                    break;
                }
            }
        }
        assert!(drained);
        start = end;
    }
    assert_eq!(parser.pending_kind(), PendingKind::None);
    assert_eq!(parser.rejected_count(), 0);
    for (row, before) in untouched_rows.iter().enumerate() {
        assert_eq!(
            &format!("{:?}", &screen.grid[row]),
            before,
            "control-only fixture changed cell data"
        );
    }
    ResultTrace {
        replies,
        final_state: state(&screen),
    }
}
fn check(bytes: &[u8], expected: ResultTrace) {
    for cut in 0..=bytes.len() {
        assert_eq!(run(bytes, &[cut]), expected, "cut {cut}");
    }
    let cuts: Vec<_> = (0..=bytes.len()).flat_map(|n| [n, n]).collect();
    assert_eq!(run(bytes, &cuts), expected);
}
fn position(row: usize, col: usize) -> State {
    State {
        row,
        col,
        top: 0,
        bottom: 5,
        protected: false,
    }
}
#[test]
fn tui_cursor_motion_and_status_reply_use_mutated_coordinates() {
    check(
        b"\x1b[3;4H\x1b[A\x1b[2C\x1b[6n\x1b[5n\x1b[H\x1b[6n",
        ResultTrace {
            replies: vec![
                (b"\x1b[2;6R".to_vec(), position(1, 5)),
                (b"\x1b[0n".to_vec(), position(1, 5)),
                (b"\x1b[1;1R".to_vec(), position(0, 0)),
            ],
            final_state: position(0, 0),
        },
    );
}
#[test]
fn margins_and_character_protection_queries_follow_state_changes() {
    check(b"\x1b[2;5r\x1bP$qr\x1b\\\x1b[1\"q\x1bP$q\"q\x1b\\\x1b[0\"q\x1bP$q\"q\x1b\\\x1b[r\x1bP$qr\x1b\\", ResultTrace {
        replies: vec![
            (b"\x1bP1$r2;5r\x1b\\".to_vec(), State { top: 1, bottom: 4, ..position(0, 0) }),
            (b"\x1bP1$r1\"q\x1b\\".to_vec(), State { top: 1, bottom: 4, protected: true, ..position(0, 0) }),
            (b"\x1bP1$r0\"q\x1b\\".to_vec(), State { top: 1, bottom: 4, ..position(0, 0) }),
            (b"\x1bP1$r1;6r\x1b\\".to_vec(), position(0, 0)),
        ], final_state: position(0, 0),
    });
}
#[test]
fn cancelled_status_string_never_replies_or_changes_selected_state() {
    for cancel in [0x18, 0x1a] {
        let mut bytes = b"\x1b[1\"q\x1bP$q\"q".to_vec();
        bytes.push(cancel);
        bytes.extend_from_slice(b"\x1bP$q\"q\x1b\\");
        let selected = State {
            protected: true,
            ..position(0, 0)
        };
        check(
            &bytes,
            ResultTrace {
                replies: vec![(
                    b"\x1bP1$r1\"q\x1b\\".to_vec(),
                    State {
                        protected: true,
                        ..position(0, 0)
                    },
                )],
                final_state: selected,
            },
        );
    }
}
#[test]
fn unknown_status_selector_reports_failure_without_echoing_payload() {
    check(
        b"\x1b[4;7H\x1bP$qunknown-selector\x1b\\\x1b[6n",
        ResultTrace {
            replies: vec![
                (b"\x1bP0$r\x1b\\".to_vec(), position(3, 6)),
                (b"\x1b[4;7R".to_vec(), position(3, 6)),
            ],
            final_state: position(3, 6),
        },
    );
}
