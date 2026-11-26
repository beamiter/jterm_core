use crate::process_input_budget::{shared_budget, Charge};
use std::{collections::VecDeque, num::NonZeroU16};

pub(super) const INPUT_LIMIT: usize = 16 * 1024;
pub(super) const WRITE_BATCH: usize = 4096;

// Keep the original contiguous payload batch and track opaque logical
// boundaries separately. At most one metadata entry per pending byte; empty
// events do not allocate anything. Only the oldest event can be partial.
pub(super) struct InputQueue {
    bytes: VecDeque<u8>,
    lengths: VecDeque<NonZeroU16>,
    head_written: u16,
    charge: Charge,
}

impl Default for InputQueue {
    fn default() -> Self {
        Self::with_charge(shared_budget().empty_charge())
    }
}

impl InputQueue {
    fn with_charge(charge: Charge) -> Self {
        Self {
            bytes: VecDeque::new(),
            lengths: VecDeque::new(),
            head_written: 0,
            charge,
        }
    }

    pub(super) fn len(&self) -> usize {
        self.bytes.len()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    pub(super) fn clear(&mut self) {
        let pending = self.bytes.len();
        self.bytes.clear();
        self.lengths.clear();
        self.head_written = 0;
        self.charge.release(pending);
    }

    pub(super) fn enqueue(&mut self, bytes: Vec<u8>) -> Result<(), String> {
        self.enqueue_with_limit(bytes, INPUT_LIMIT)
    }

    pub(super) fn enqueue_with_limit(
        &mut self,
        bytes: Vec<u8>,
        limit: usize,
    ) -> Result<(), String> {
        let limit = limit.min(u16::MAX as usize);
        if bytes.len() > limit.saturating_sub(self.len()) {
            return Err("input queue full; event not accepted".into());
        }
        if let Some(length) = NonZeroU16::new(bytes.len() as u16) {
            self.charge
                .try_add(bytes.len(), false)
                .map_err(|_| "process input queue full; event not accepted")?;
            self.bytes.extend(bytes);
            self.lengths.push_back(length);
        }
        Ok(())
    }

    pub(super) fn partial_remaining(&self) -> Option<usize> {
        if self.head_written == 0 {
            return None;
        }
        Some(usize::from(
            self.lengths.front().unwrap().get() - self.head_written,
        ))
    }

    pub(super) fn flush_partial(
        &mut self,
        write: impl FnOnce(&[u8], NonZeroU16, usize) -> Result<usize, String>,
    ) -> Result<(), String> {
        let remaining = self
            .partial_remaining()
            .ok_or("missing partial input head")?;
        let prefix = NonZeroU16::new(self.head_written).unwrap();
        self.flush(|offer| write(&offer[..offer.len().min(remaining)], prefix, remaining))
    }

    pub(super) fn flush(
        &mut self,
        write: impl FnOnce(&[u8]) -> Result<usize, String>,
    ) -> Result<(), String> {
        if self.is_empty() {
            return Ok(());
        }
        let limit = WRITE_BATCH.min(self.len());
        let written = write(&self.bytes.make_contiguous()[..limit])?;
        self.bytes.drain(..written);
        self.charge.release(written);
        let mut remaining = written;
        while remaining > 0 {
            let available = self.lengths.front().unwrap().get() - self.head_written;
            if remaining < usize::from(available) {
                self.head_written += remaining as u16;
                break;
            }
            self.lengths.pop_front();
            self.head_written = 0;
            remaining -= usize::from(available);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process_input_budget::Budget;
    use std::sync::Arc;

    #[test]
    fn accepted_owner_bytes_across_sessions_share_the_cap_and_release_only_on_write() {
        let budget = Arc::new(Budget::new(12, 4));
        let mut first = InputQueue::with_charge(budget.empty_charge());
        let mut second = InputQueue::with_charge(budget.empty_charge());
        first.enqueue(b"abcde".to_vec()).unwrap();
        second.enqueue(b"fgh".to_vec()).unwrap();
        let message = b"ij".to_vec();
        assert!(second.enqueue(message.clone()).is_err());
        assert_eq!(second.bytes, VecDeque::from(*b"fgh"));
        assert!(first.flush(|_| Err("closed".into())).is_err());
        first.flush(|_| Ok(0)).unwrap();
        assert_eq!(budget.usage().reserved_bytes, 8);

        first
            .flush(|offered| {
                assert_eq!(offered, b"abcde");
                Ok(2)
            })
            .unwrap();
        assert_eq!(first.partial_remaining(), Some(3));
        assert_eq!(budget.usage().reserved_bytes, 6);
        second.enqueue(message).unwrap();
        assert_eq!(second.bytes, VecDeque::from(*b"fghij"));
        first.clear();
        assert_eq!(budget.usage().reserved_bytes, 5);
        drop(second);
        assert_eq!(budget.usage().reserved_bytes, 0);
    }

    #[test]
    fn empty_events_have_no_metadata_and_single_byte_events_bound_it() {
        let mut queue = InputQueue::default();
        for _ in 0..20000 {
            queue.enqueue(Vec::new()).unwrap();
        }
        assert!(queue.is_empty());
        assert_eq!(queue.lengths.capacity(), 0);
        for _ in 0..INPUT_LIMIT {
            queue.enqueue(vec![7]).unwrap();
        }
        assert_eq!(queue.len(), INPUT_LIMIT);
        assert_eq!(queue.lengths.len(), INPUT_LIMIT);
        let full = (queue.bytes.clone(), queue.lengths.clone());
        queue.enqueue(Vec::new()).unwrap();
        assert!(queue.enqueue(vec![8]).is_err());
        assert_eq!(queue.bytes, full.0);
        assert_eq!(queue.lengths, full.1);
        queue.flush(|batch| Ok(batch.len())).unwrap();
        assert_eq!(queue.len(), INPUT_LIMIT - WRITE_BATCH);
        assert_eq!(queue.lengths.len(), INPUT_LIMIT - WRITE_BATCH);
        assert_eq!(queue.head_written, 0);
        queue.clear();
        assert!(queue.is_empty());
        assert!(queue.lengths.is_empty());
        assert_eq!(full.0.len(), INPUT_LIMIT);
    }

    #[test]
    fn positive_counts_cross_frame_boundaries_zero_and_error_leave_offsets() {
        let mut queue = InputQueue::default();
        for event in [b"a".as_slice(), b"bcde", b"fgh", b"i"] {
            queue.enqueue(event.to_vec()).unwrap();
        }
        queue
            .flush(|batch| {
                assert_eq!(batch, b"abcdefghi");
                Ok(3)
            })
            .unwrap();
        assert_eq!(queue.head_written, 2);
        let partly_written = (
            queue.bytes.clone(),
            queue.lengths.clone(),
            queue.head_written,
        );
        queue
            .flush(|batch| {
                assert_eq!(batch, b"defghi");
                Ok(0)
            })
            .unwrap();
        assert!(queue.flush(|_| Err("write failed".into())).is_err());
        assert_eq!(queue.bytes, partly_written.0);
        assert_eq!(queue.lengths, partly_written.1);
        assert_eq!(queue.head_written, partly_written.2);
        queue
            .flush(|batch| {
                assert_eq!(batch, b"defghi");
                Ok(3)
            })
            .unwrap();
        assert_eq!(queue.head_written, 1);
        assert_eq!(queue.lengths.front().unwrap().get(), 3);
        queue
            .flush(|batch| {
                assert_eq!(batch, b"ghi");
                Ok(batch.len())
            })
            .unwrap();
        assert!(queue.is_empty());
        assert!(queue.lengths.is_empty());
        assert_eq!(queue.head_written, 0);
        queue.flush(|_| panic!("no write without bytes")).unwrap();
        assert_eq!(partly_written.2, 2);
        assert_eq!(partly_written.0.len(), 6);
    }

    #[test]
    #[should_panic]
    fn invalid_writer_count_over_pending_bytes_still_panics() {
        let mut queue = InputQueue::default();
        queue.enqueue(vec![1]).unwrap();
        let _ = queue.flush(|_| Ok(2));
    }

    #[test]
    fn exact_offers_and_wire_progress_match_old_contiguous_queue() {
        let mut queue = InputQueue::default();
        let mut old = VecDeque::new();
        for n in [3001, 1, 2000, 8192] {
            let data = (0..n).map(|i| (i % 251) as u8).collect::<Vec<_>>();
            old.extend(data.iter().copied());
            queue.enqueue(data).unwrap();
        }
        let mut written = 0;
        for count in [
            Ok(4096),
            Ok(0),
            Err("interrupted"),
            Ok(17),
            Ok(4096),
            Ok(4096),
            Ok(4096),
        ] {
            let limit = WRITE_BATCH.min(old.len());
            let old_offer = old.make_contiguous()[..limit].to_vec();
            let result = queue.flush(|offer| {
                assert_eq!(offer, old_offer);
                count
                    .map(|count| count.min(offer.len()))
                    .map_err(str::to_owned)
            });
            if let Ok(count) = count {
                let n = count.min(limit);
                old.drain(..n);
                written += n;
                assert!(result.is_ok());
            } else {
                assert!(result.is_err());
            }
            assert_eq!(queue.bytes, old);
            let measured = queue
                .lengths
                .iter()
                .map(|n| usize::from(n.get()))
                .sum::<usize>()
                - usize::from(queue.head_written);
            assert_eq!(measured, old.len());
        }
        assert!(queue.is_empty());
        assert_eq!(written, 13194);
        assert_eq!(queue.head_written, 0);
    }
}
