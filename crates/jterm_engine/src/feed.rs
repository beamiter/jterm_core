//! Schedule ordered scanner steps without borrowing the terminal across dispatch.
use crate::parser::{CaptureStep, Event, Parser};

/// Preserve the feed boundary selected by the terminal profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeedTail {
    /// Finish this input span and carry any queued zero-byte events forward.
    InputConsumed,
    /// Dispatch all available events, including zero-byte events at the end.
    DrainPending,
}

/// One scanner step at its absolute endpoint within the supplied input span.
/// Resource cleanup precedes dispatch. A capture starts before dispatch and
/// completes after dispatch, since dispatch can close or open its product owner.
pub struct FeedStep<'input> {
    pub end_offset: usize,
    pub event: Option<Event<'input>>,
    pub capture: Option<CaptureStep>,
    pub abandoned_apc: bool,
    pub rejected_resource: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum FeedError<E> {
    NoProgress,
    Dispatch(E),
}

/// Advance exactly one ordinary input feed. Obtain the live parser again for
/// every step so applying RIS can replace it without reviving its previous
/// carry. The accessor can also set the next step's profile batching policy.
pub fn drive_feed<'input, T, E>(
    terminal: &mut T,
    input: &'input [u8],
    tail: FeedTail,
    mut parser: impl FnMut(&mut T) -> &mut Parser,
    mut dispatch: impl FnMut(&mut T, FeedStep<'input>) -> Result<(), E>,
) -> Result<(), FeedError<E>> {
    parser(terminal).begin_feed();
    let mut at = 0;
    let mut remaining_steps = input.len().saturating_mul(2).saturating_add(16);
    while at < input.len() || tail == FeedTail::DrainPending {
        remaining_steps = remaining_steps
            .checked_sub(1)
            .ok_or(FeedError::NoProgress)?;
        let parser = parser(terminal);
        let rejected_before = parser.rejected_count();
        let step = parser.next(&input[at..]);
        let rejected_resource = parser.rejected_count() != rejected_before;
        at += step.consumed;
        let end = step.event.is_none();
        dispatch(
            terminal,
            FeedStep {
                end_offset: at,
                event: step.event,
                capture: step.capture,
                abandoned_apc: step.abandoned_apc,
                rejected_resource,
            },
        )
        .map_err(FeedError::Dispatch)?;
        if end {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{ParserLimits, ParserProfile};
    use std::convert::Infallible;

    const LIMITS: ParserLimits = ParserLimits {
        pending_bytes: 32,
        kitty_recovery_bytes: 12,
    };

    fn replace_on_ris(parser: &mut Parser, event: &Option<Event<'_>>, profile: ParserProfile) {
        if matches!(event, Some(Event::Escape { intermediates, final_byte: b'c' }) if intermediates.is_empty())
        {
            *parser = Parser::with_bounded_profile(LIMITS, profile).unwrap();
        }
    }

    type Trace = Vec<(usize, bool, bool, String, String)>;

    fn legacy_feed(
        parser: &mut Parser,
        input: &[u8],
        profile: ParserProfile,
        tail: FeedTail,
    ) -> Trace {
        parser.begin_feed();
        let mut offset = 0;
        let mut trace = Vec::new();
        while offset < input.len() || tail == FeedTail::DrainPending {
            let before = parser.rejected_count();
            let step = parser.next(&input[offset..]);
            offset += step.consumed;
            trace.push((
                offset,
                step.abandoned_apc,
                before != parser.rejected_count(),
                format!("{:?}", step.event),
                format!("{:?}", step.capture),
            ));
            replace_on_ris(parser, &step.event, profile);
            if step.event.is_none() {
                break;
            }
        }
        trace
    }

    #[test]
    fn shared_feed_preserves_profile_boundaries_across_all_chunk_splits() {
        let input = b"abc\x1b[2\n;4H\x1b_Gi=4,m=1;x\x1b\\\x1b_Gi=4;\x18\x1b]8;;\x07\x1bcB\xe0\x80";
        for profile in [ParserProfile::EmberV1, ParserProfile::FrostLegacyV1] {
            for tail in [FeedTail::InputConsumed, FeedTail::DrainPending] {
                for split in 0..=input.len() {
                    let mut old = Parser::with_bounded_profile(LIMITS, profile).unwrap();
                    let mut new = Parser::with_bounded_profile(LIMITS, profile).unwrap();
                    for chunk in [&input[..split], &input[split..], b"", b"\x1b[6nZ"] {
                        let expected = legacy_feed(&mut old, chunk, profile, tail);
                        let mut actual = Trace::new();
                        drive_feed(
                            &mut new,
                            chunk,
                            tail,
                            |p| p,
                            |p, step| {
                                actual.push((
                                    step.end_offset,
                                    step.abandoned_apc,
                                    step.rejected_resource,
                                    format!("{:?}", step.event),
                                    format!("{:?}", step.capture),
                                ));
                                replace_on_ris(p, &step.event, profile);
                                Ok::<_, Infallible>(())
                            },
                        )
                        .unwrap();
                        assert_eq!(actual, expected, "{profile:?} {tail:?} split {split}");
                        assert_eq!(
                            format!("{:?}", new.snapshot()),
                            format!("{:?}", old.snapshot())
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn dispatch_failure_leaves_the_unvisited_suffix_unparsed() {
        let mut parser = Parser::new(LIMITS);
        let result = drive_feed(
            &mut parser,
            b"abc\x1b[",
            FeedTail::DrainPending,
            |p| p,
            |_, step| {
                assert_eq!(step.end_offset, 3);
                assert!(matches!(step.event, Some(Event::Ascii(b"abc"))));
                Err("dispatch stopped")
            },
        );
        assert_eq!(result, Err(FeedError::Dispatch("dispatch stopped")));
        assert_eq!(parser.pending_kind(), crate::parser::PendingKind::None);
    }
}
