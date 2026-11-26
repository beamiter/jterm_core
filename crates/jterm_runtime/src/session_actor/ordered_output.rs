use super::SessionEvent;
use crate::output_batch::OutputBuffer;
use crossbeam_channel::{Receiver, TryRecvError};
use std::time::Instant;

#[derive(Debug, PartialEq, Eq)]
pub enum OutputNotice {
    /// A disconnect can leave the real child status unavailable.
    Exited {
        code: Option<i32>,
        observed_at: Instant,
    },
    Error(String),
}

#[derive(Debug, Default)]
pub struct OutputProgress {
    pub bytes_applied: usize,
    pub has_more: bool,
    /// Output already collected from the reader was held for a later poll.
    pub parked: bool,
    pub notice: Option<OutputNotice>,
}

/// Output receiver and the bytes and notices that precede its next delivery.
/// A session with this receiver has no raw event subscription.
pub struct OrderedOutput {
    events: Receiver<SessionEvent>,
    buffered: OutputBuffer,
    notice: Option<OutputNotice>,
    finished: bool,
}

impl OrderedOutput {
    pub(super) fn new(events: Receiver<SessionEvent>) -> Self {
        Self {
            events,
            buffered: OutputBuffer::default(),
            notice: None,
            finished: false,
        }
    }

    pub(super) fn buffered_bytes(&self) -> usize {
        self.buffered.len()
    }

    pub(super) fn is_finished(&self) -> bool {
        self.finished
    }

    pub(super) fn has_pending(&self) -> bool {
        !self.finished
            && (!self.buffered.is_empty() || self.notice.is_some() || !self.events.is_empty())
    }

    pub(super) fn discard_after_exit(&self) {
        while self.events.try_recv().is_ok() {}
    }

    pub(super) fn advance(
        &mut self,
        budget: usize,
        admit: impl FnOnce() -> bool,
        apply: impl FnOnce(&OutputBuffer, bool),
    ) -> OutputProgress {
        if self.finished {
            return OutputProgress::default();
        }

        // Collect at most one reader chunk beyond the budget, retaining its
        // origin and offset if the parser has to consume it across polls.
        while self.buffered.len() < budget && self.notice.is_none() {
            let notice = match self.events.try_recv() {
                Ok(SessionEvent::Output(batch)) => {
                    self.buffered.extend(batch);
                    continue;
                }
                Ok(SessionEvent::Error(error)) => OutputNotice::Error(error),
                Ok(SessionEvent::Exit(code)) => OutputNotice::Exited {
                    code: Some(code),
                    observed_at: Instant::now(),
                },
                Err(TryRecvError::Disconnected) => OutputNotice::Exited {
                    code: None,
                    observed_at: Instant::now(),
                },
                Err(TryRecvError::Empty) => break,
            };
            self.notice = Some(notice);
        }

        if budget == 0 || (!self.buffered.is_empty() && !admit()) {
            self.buffered.quarantine();
            return OutputProgress {
                has_more: self.has_pending(),
                parked: !self.buffered.is_empty(),
                ..OutputProgress::default()
            };
        }

        let mut batch = std::mem::take(&mut self.buffered);
        if batch.len() > budget {
            self.buffered = batch.split_off(budget);
        }
        let notice = if self.buffered.is_empty() {
            self.notice.take()
        } else {
            None
        };
        let has_more = !matches!(notice, Some(OutputNotice::Exited { .. }))
            && (batch.len() == budget || self.has_pending());
        if !batch.is_empty() {
            apply(&batch, has_more);
        }
        self.finished = matches!(notice, Some(OutputNotice::Exited { .. }));
        OutputProgress {
            bytes_applied: batch.len(),
            has_more,
            parked: false,
            notice,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output_batch::OutputSource;
    use crossbeam_channel::bounded;

    #[test]
    fn parked_tail_retains_exit_and_origin_across_budgeted_polls() {
        let (sender, receiver) = bounded(4);
        let mut output = OrderedOutput::new(receiver);
        let mut source = OutputSource::default();
        let tail = source.batch(b"final".to_vec(), Instant::now());
        let origin = tail.fragments().next().unwrap().origin;
        sender.send(SessionEvent::Output(tail)).unwrap();
        sender.send(SessionEvent::Exit(37)).unwrap();

        let blocked = output.advance(16, || false, |_, _| panic!("gate is closed"));
        assert!(blocked.parked && blocked.has_more);
        assert!(blocked.notice.is_none());
        assert!(!output.is_finished());
        assert_eq!(output.buffered_bytes(), 5);
        let observed_by = Instant::now();

        sender
            .send(SessionEvent::Error("late writer error".into()))
            .unwrap();
        let first = output.advance(
            2,
            || true,
            |batch, backlogged| {
                let part = batch.fragments().next().unwrap();
                assert_eq!(part.bytes, b"fi");
                assert_eq!(part.origin, origin);
                assert_eq!(part.offset, 0);
                assert!(part.quarantined && backlogged);
            },
        );
        assert_eq!(first.bytes_applied, 2);
        assert!(first.has_more && first.notice.is_none());

        let last = output.advance(
            16,
            || true,
            |batch, backlogged| {
                let part = batch.fragments().next().unwrap();
                assert_eq!(part.bytes, b"nal");
                assert_eq!(part.origin, origin);
                assert_eq!(part.offset, 2);
                assert!(part.quarantined && !backlogged);
            },
        );
        assert_eq!(last.bytes_applied, 3);
        assert!(!last.has_more);
        assert!(
            matches!(last.notice, Some(OutputNotice::Exited { code: Some(37), observed_at }) if observed_at <= observed_by)
        );
        assert!(output.is_finished());
        assert!(!output.has_pending());
        let done = output.advance(16, || unreachable!(), |_, _| unreachable!());
        assert!(done.notice.is_none());
    }

    #[test]
    fn zero_budget_preserves_channel_and_disconnect_follows_the_final_bytes() {
        let (sender, receiver) = bounded(1);
        let mut output = OrderedOutput::new(receiver);
        sender
            .send(SessionEvent::Output(OutputBuffer::from_parts(
                b"a".to_vec(),
                None,
            )))
            .unwrap();
        let idle = output.advance(0, || unreachable!(), |_, _| unreachable!());
        assert!(idle.has_more && !idle.parked && idle.notice.is_none());
        assert_eq!(sender.len(), 1);
        drop(sender);

        let drained = output.advance(16, || true, |batch, _| assert_eq!(batch.as_bytes(), b"a"));
        assert_eq!(drained.bytes_applied, 1);
        assert!(matches!(
            drained.notice,
            Some(OutputNotice::Exited { code: None, .. })
        ));
        assert!(output.is_finished());
    }
}
