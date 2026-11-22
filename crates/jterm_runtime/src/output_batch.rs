//! Bytes retain their native reader identity through frame slicing and parking.
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

const MAX_ORIGIN_SPANS: usize = 1024;
static NEXT_SOURCE: AtomicU64 = AtomicU64::new(1);

fn allocate_epoch(counter: &AtomicU64) -> Option<u64> {
    let mut current = counter.load(Ordering::Relaxed);
    loop {
        let next = current.checked_add(1)?;
        match counter.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return Some(current),
            Err(observed) => current = observed,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutputOrigin {
    pub source: u64,
    pub sequence: u64,
    pub read_at: Instant,
    pub total: usize,
}

/// One allocation per reader, not per UI owner or presentation frame.
pub struct OutputSource {
    source: Option<u64>,
    next: Option<u64>,
}

impl Default for OutputSource {
    fn default() -> Self {
        Self {
            source: allocate_epoch(&NEXT_SOURCE),
            next: Some(1),
        }
    }
}

impl OutputSource {
    pub fn epoch(&self) -> Option<u64> {
        self.source
    }

    pub fn coalesced(&mut self, bytes: Vec<u8>, first_read_at: Option<Instant>) -> OutputBuffer {
        match first_read_at {
            Some(read_at) => self.batch(bytes, read_at),
            None => OutputBuffer::from_parts(bytes, None),
        }
    }

    pub fn batch(&mut self, bytes: Vec<u8>, read_at: Instant) -> OutputBuffer {
        let origin = if bytes.is_empty() {
            None
        } else {
            self.source.zip(self.next).map(|(source, sequence)| {
                self.next = sequence.checked_add(1);
                OutputOrigin {
                    source,
                    sequence,
                    read_at,
                    total: bytes.len(),
                }
            })
        };
        OutputBuffer::from_parts(bytes, origin)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct OriginSpan {
    len: usize,
    origin: Option<OutputOrigin>,
    offset: usize,
    quarantined: bool,
}

#[derive(Clone, Debug, Default)]
pub struct OutputBuffer {
    bytes: Vec<u8>,
    spans: Vec<OriginSpan>,
}

#[derive(Clone, Copy, Debug)]
pub struct OutputFragment<'a> {
    pub bytes: &'a [u8],
    pub origin: Option<OutputOrigin>,
    pub offset: usize,
    pub quarantined: bool,
}

impl OutputBuffer {
    pub fn from_parts(bytes: Vec<u8>, origin: Option<OutputOrigin>) -> Self {
        let spans = if bytes.is_empty() {
            Vec::new()
        } else {
            vec![OriginSpan {
                len: bytes.len(),
                origin,
                offset: 0,
                quarantined: false,
            }]
        };
        Self { bytes, spans }
    }
    pub fn len(&self) -> usize {
        self.bytes.len()
    }
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
    pub fn extend(&mut self, mut other: Self) {
        self.append(&mut other);
    }
    pub fn append(&mut self, other: &mut Self) {
        self.bytes.append(&mut other.bytes);
        for span in other.spans.drain(..) {
            if let Some(last) = self.spans.last_mut() {
                if last.origin == span.origin
                    && last.quarantined == span.quarantined
                    && (last.origin.is_none()
                        || last.offset.checked_add(last.len) == Some(span.offset))
                {
                    last.len += span.len;
                    continue;
                }
            }
            self.spans.push(span);
            if self.spans.len() > MAX_ORIGIN_SPANS {
                self.spans.clear();
                self.spans.push(OriginSpan {
                    len: self.bytes.len(),
                    origin: None,
                    offset: 0,
                    quarantined: true,
                });
                return;
            }
        }
    }
    pub fn split_off(&mut self, at: usize) -> Self {
        assert!(at <= self.len());
        let bytes = self.bytes.split_off(at);
        let mut left = Vec::new();
        let mut right = Vec::new();
        let mut start = 0;
        for span in self.spans.drain(..) {
            let end = start + span.len;
            if end <= at {
                left.push(span);
            } else if start >= at {
                right.push(span);
            } else {
                let left_len = at - start;
                left.push(OriginSpan {
                    len: left_len,
                    ..span
                });
                right.push(OriginSpan {
                    len: span.len - left_len,
                    offset: span.offset + left_len,
                    ..span
                });
            }
            start = end;
        }
        self.spans = left;
        Self {
            bytes,
            spans: right,
        }
    }
    pub fn quarantine(&mut self) {
        for span in &mut self.spans {
            span.quarantined = true;
        }
    }
    pub fn fragments(&self) -> impl Iterator<Item = OutputFragment<'_>> {
        let mut start = 0;
        self.spans.iter().map(move |span| {
            let bytes = &self.bytes[start..start + span.len];
            start += span.len;
            OutputFragment {
                bytes,
                origin: span.origin,
                offset: span.offset,
                quarantined: span.quarantined,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn slices_and_park_keep_one_original_batch() {
        let mut source = OutputSource::default();
        let at = Instant::now();
        let mut first = source.batch(b"abcdef".to_vec(), at);
        let mut tail = first.split_off(2);
        assert_eq!(first.fragments().next().unwrap().offset, 0);
        assert_eq!(tail.fragments().next().unwrap().offset, 2);
        assert_eq!(tail.fragments().next().unwrap().origin.unwrap().read_at, at);
        first.quarantine();
        tail.quarantine();
        first.append(&mut tail);
        assert_eq!(first.as_bytes(), b"abcdef");
        assert_eq!(first.spans.len(), 1);
        assert!(first.spans[0].quarantined);
        let whole = first.split_off(0);
        assert!(first.is_empty());
        let mut whole = whole;
        assert!(whole.split_off(whole.len()).is_empty());
        assert_eq!(whole.fragments().next().unwrap().origin.unwrap().total, 6);
    }
    #[test]
    fn multiple_reads_remain_distinct_and_empty_does_not_allocate() {
        let mut source = OutputSource::default();
        let at = Instant::now();
        assert!(source.batch(vec![], at).is_empty());
        let mut data = source.batch(vec![1], at);
        data.extend(source.batch(vec![2], at));
        let origins: Vec<_> = data
            .fragments()
            .map(|f| f.origin.unwrap().sequence)
            .collect();
        assert_eq!(origins, [1, 2]);
    }
    #[test]
    fn overflow_loses_only_provenance_and_recovers_at_later_batch() {
        let mut source = OutputSource::default();
        let mut data = OutputBuffer::default();
        for _ in 0..=MAX_ORIGIN_SPANS {
            data.extend(source.batch(vec![42], Instant::now()));
        }
        assert_eq!(data.len(), MAX_ORIGIN_SPANS + 1);
        assert_eq!(data.spans.len(), 1);
        assert!(data.spans[0].origin.is_none());
        data.extend(source.batch(vec![7], Instant::now()));
        assert_eq!(data.spans.len(), 2);
        assert!(data.spans[1].origin.is_some());
    }
    #[test]
    fn every_split_point_preserves_bytes_and_source_spans() {
        let mut source = OutputSource::default();
        let mut data = OutputBuffer::default();
        for bytes in [b"a".as_slice(), b"bcd", b"ef"] {
            data.extend(source.batch(bytes.to_vec(), Instant::now()));
        }
        for at in 0..=data.len() {
            let mut left = data.clone();
            let mut right = left.split_off(at);
            assert_eq!(left.len(), at);
            assert_eq!(right.len(), data.len() - at);
            left.append(&mut right);
            assert_eq!(left.bytes, data.bytes);
            assert_eq!(left.spans, data.spans);
        }
    }
    #[test]
    fn missing_reader_time_never_becomes_a_fresh_delivery_timestamp() {
        let mut source = OutputSource::default();
        let data = source.coalesced(b"retained".to_vec(), None);
        assert_eq!(data.as_bytes(), b"retained");
        assert!(data.fragments().next().unwrap().origin.is_none());
    }
    #[test]
    fn source_epoch_exhaustion_never_reuses_an_identity() {
        let counter = AtomicU64::new(u64::MAX - 1);
        assert_eq!(allocate_epoch(&counter), Some(u64::MAX - 1));
        assert_eq!(allocate_epoch(&counter), None);
        assert_eq!(allocate_epoch(&counter), None);
    }
    #[test]
    fn exhaustion_keeps_bytes_without_reusing_sequence() {
        let mut source = OutputSource {
            source: Some(77),
            next: Some(u64::MAX),
        };
        let last = source.batch(vec![1], Instant::now());
        assert_eq!(last.spans[0].origin.unwrap().sequence, u64::MAX);
        let next = source.batch(vec![2], Instant::now());
        assert_eq!(next.as_bytes(), [2]);
        assert!(next.spans[0].origin.is_none());
    }
}
