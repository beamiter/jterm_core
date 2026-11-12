//! Reproducible, same-process framing baseline; not a terminal-rendering benchmark.
use jterm_engine::parser::{Parser, ParserLimits, ParserProfile, PendingKind};
use std::{hint::black_box, time::Instant};

fn parser(profile: ParserProfile, bounded: bool) -> Parser {
    let limits = ParserLimits {
        pending_bytes: 1024 * 1024,
        kitty_recovery_bytes: 4096,
    };
    if bounded {
        Parser::with_bounded_profile(limits, profile).unwrap()
    } else {
        Parser::with_profile(limits, profile)
    }
}
fn replay(data: &[u8], chunk: usize, profile: ParserProfile, bounded: bool) -> (u64, u64) {
    let mut parser = parser(profile, bounded);
    let mut events = 0u64;
    for feed in data.chunks(chunk) {
        parser.begin_feed();
        let mut at = 0;
        let mut stalls = 0;
        loop {
            let step = parser.next(&feed[at..]);
            assert!(step.consumed <= feed.len() - at, "invalid consumed length");
            if step.consumed == 0 {
                stalls += 1;
                assert!(stalls <= 8, "parser stalled");
            } else {
                stalls = 0;
            }
            at += step.consumed;
            let done = step.event.is_none();
            if done {
                assert_eq!(at, feed.len(), "undrained feed");
            }
            events += u64::from(step.event.is_some());
            black_box(step);
            if done {
                break;
            }
        }
    }
    assert_eq!(
        parser.pending_kind(),
        PendingKind::None,
        "incomplete corpus"
    );
    (events, parser.rejected_count())
}
fn main() {
    // Fixed, public synthetic data; no user sessions or captured shell output.
    let units: [(&str, &[u8]); 4] = [
        (
            "ascii",
            b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ\r\n",
        ),
        (
            "csi_utf8",
            "\x1b[12;34H\x1b[1;31mHello界e\u{301}\x1b[0m\r\n".as_bytes(),
        ),
        ("short_osc", b"\x1b]0;benchmark-title\x07x"),
        ("dcs_query", b"\x1bP$qm\x1b\\\x1b[6nZ"),
    ];
    let smoke = std::env::args().any(|arg| arg == "--smoke");
    let target: usize = if smoke { 64 * 1024 } else { 2 * 1024 * 1024 };
    let samples = if smoke { 1 } else { 7 };
    println!("corpus,profile,mode,chunk,bytes,sample,nanoseconds,events,rejections");
    for (name, unit) in units {
        let data = unit.repeat(target.div_ceil(unit.len()));
        for (profile_name, profile) in [
            ("ember", ParserProfile::EmberV1),
            ("frost", ParserProfile::FrostLegacyV1),
        ] {
            for chunk in [1, 4096] {
                // Untimed warmup for both modes; preallocated corpus outside timer.
                for bounded in [false, true] {
                    black_box(replay(&data, chunk, profile, bounded));
                }
                for sample in 0..samples {
                    // Alternate execution order to reduce thermal/order bias.
                    let order = if sample % 2 == 0 {
                        [false, true]
                    } else {
                        [true, false]
                    };
                    for bounded in order {
                        let start = Instant::now();
                        let (events, rejected) = replay(black_box(&data), chunk, profile, bounded);
                        let elapsed = start.elapsed().as_nanos();
                        assert_eq!(rejected, 0, "valid synthetic corpus was rejected");
                        println!("{name},{profile_name},{},{chunk},{},{sample},{elapsed},{events},{rejected}", if bounded { "bounded" } else { "legacy" }, data.len());
                    }
                }
            }
        }
    }
}
