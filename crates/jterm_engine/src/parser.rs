//! Incremental byte framing extracted from Ember. Semantic dispatch stays in the host.
//! Events are delivered one at a time, before any following input is interpreted.
use std::{borrow::Cow, ops::Range};

#[inline]
fn printable_ascii_prefix(input: &[u8]) -> usize {
    const HIGH_BITS: u64 = 0x8080_8080_8080_8080;
    const PRINTABLE_START: u64 = 0x2020_2020_2020_2020;
    const FIRST_REJECTED: u64 = 0x7f7f_7f7f_7f7f_7f7f;
    let mut words = input.chunks_exact(8);
    for (index, bytes) in words.by_ref().enumerate() {
        // Set each byte's high bit before subtraction so neighboring bytes
        // never borrow from one another. Retain the original high bits too.
        let word = u64::from_le_bytes(bytes.try_into().unwrap());
        let lanes = word | HIGH_BITS;
        let below = !lanes.wrapping_sub(PRINTABLE_START);
        let above = lanes.wrapping_sub(FIRST_REJECTED);
        let rejected = (word | below | above) & HIGH_BITS;
        if rejected != 0 {
            return index * 8 + rejected.trailing_zeros() as usize / 8;
        }
    }
    let tail = words.remainder();
    let accepted = tail
        .iter()
        .take_while(|&&byte| (0x20..=0x7e).contains(&byte))
        .count();
    input.len() - tail.len() + accepted
}

#[inline]
fn zero_byte_high_bits(word: u64) -> u64 {
    const LOW_BITS: u64 = 0x7f7f_7f7f_7f7f_7f7f;
    // The add cannot carry across bytes. A zero lane is the only one
    // whose high bit is unset in both the sum and the original word.
    !((word & LOW_BITS).wrapping_add(LOW_BITS) | word | LOW_BITS)
}

fn string_boundary_position(input: &[u8], osc: bool) -> Option<usize> {
    let is_boundary = |byte: u8| matches!(byte, 0x1b | 0x18 | 0x1a) || (osc && byte == 7);
    if input.len() < 32 {
        return input.iter().copied().position(is_boundary);
    }
    let mut words = input.chunks_exact(8);
    for (index, bytes) in words.by_ref().enumerate() {
        let word = u64::from_le_bytes(bytes.try_into().unwrap());
        let esc_or_sub = (word & 0xfefe_fefe_fefe_fefe) ^ 0x1a1a_1a1a_1a1a_1a1a;
        let cancel = word ^ 0x1818_1818_1818_1818;
        let mut boundary = zero_byte_high_bits(esc_or_sub) | zero_byte_high_bits(cancel);
        if osc {
            boundary |= zero_byte_high_bits(word ^ 0x0707_0707_0707_0707);
        }
        if boundary != 0 {
            return Some(index * 8 + boundary.trailing_zeros() as usize / 8);
        }
    }
    let tail = words.remainder();
    tail.iter()
        .copied()
        .position(is_boundary)
        .map(|index| input.len() - tail.len() + index)
}

/// Versioned scanner compatibility, not terminal semantic dispatch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ParserProfile {
    #[default]
    EmberV1,
    FrostLegacyV1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PendingKind {
    None,
    Escape,
    Csi,
    Charset,
    String(StringKind),
    DiscardApc,
}

/// One capturable token: a reconstructed prior-feed ESC/CSI prefix followed by
/// a range in the current feed. Control-string bytes are never returned here.
#[derive(Debug, PartialEq, Eq)]
pub struct CaptureToken {
    pub prefix: Vec<u8>,
    pub input_range: Range<usize>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum CaptureEnd {
    Abandoned,
    Complete {
        token: Option<CaptureToken>,
        escape: bool,
    },
}

/// Apply `started` before semantic dispatch and `end` afterwards. An open CSI
/// token can span several control events, but never spans `begin_feed` calls.
#[derive(Debug, PartialEq, Eq)]
pub struct CaptureStep {
    pub started: bool,
    pub end: Option<CaptureEnd>,
}

#[derive(Debug)]
struct OpenCapture {
    prefix: Vec<u8>,
    start: usize,
    private: bool,
    escape: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct ParserLimits {
    /// Carry policy bound for unfinished strings/ESC. CSI has an independent
    /// 256-byte parameter cap in EmberV1, 128 in FrostLegacyV1, and eight-byte
    /// intermediate caps. Completed OSC/DCS are intentionally not limited here.
    /// Frost preserves abandonment at the next feed, not a strict memory cap.
    pub pending_bytes: usize,
    /// Includes the Kitty `G` discriminator, unlike its control-field limit.
    pub kitty_recovery_bytes: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StringKind {
    Osc,
    Dcs,
    Sos,
    Pm,
    Apc,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RejectReason {
    Cancelled,
    TooLarge,
}
#[derive(PartialEq, Eq)]
pub enum Event<'a> {
    /// Frost-only token boundary with no terminal semantic action.
    Boundary,
    Print(char),
    Ascii(&'a [u8]),
    Control(u8),
    Escape {
        intermediates: Vec<u8>,
        final_byte: u8,
    },
    /// ESC ( / ESC ) historically consume exactly one following designator.
    Charset {
        bank: u8,
        designator: u8,
    },
    Csi {
        raw_params: Vec<u8>,
        /// Numeric fields decoded according to the selected parser profile.
        params: Vec<u16>,
        /// Whether the immediately preceding separator was a colon.
        colon_flags: Vec<bool>,
        private_prefix: Option<u8>,
        intermediates: Vec<u8>,
        final_byte: u8,
    },
    String {
        kind: StringKind,
        payload: Cow<'a, [u8]>,
        bel_terminated: bool,
    },
    RejectApc {
        recovery: Vec<u8>,
        reason: RejectReason,
    },
}
// Keep the original event diagnostic format stable; raw syntax has its own
// accessor/field tests and does not alter existing recorded Ember traces.
impl std::fmt::Debug for Event<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Boundary => f.write_str("Boundary"),
            Self::Print(ch) => f.debug_tuple("Print").field(ch).finish(),
            Self::Ascii(bytes) => f.debug_tuple("Ascii").field(bytes).finish(),
            Self::Control(byte) => f.debug_tuple("Control").field(byte).finish(),
            Self::Escape {
                intermediates,
                final_byte,
            } => f
                .debug_struct("Escape")
                .field("intermediates", intermediates)
                .field("final_byte", final_byte)
                .finish(),
            Self::Charset { bank, designator } => f
                .debug_struct("Charset")
                .field("bank", bank)
                .field("designator", designator)
                .finish(),
            Self::Csi {
                params,
                colon_flags,
                private_prefix,
                intermediates,
                final_byte,
                ..
            } => f
                .debug_struct("Csi")
                .field("params", params)
                .field("colon_flags", colon_flags)
                .field("private_prefix", private_prefix)
                .field("intermediates", intermediates)
                .field("final_byte", final_byte)
                .finish(),
            Self::String {
                kind,
                payload,
                bel_terminated,
            } => f
                .debug_struct("String")
                .field("kind", kind)
                .field("payload", payload)
                .field("bel_terminated", bel_terminated)
                .finish(),
            Self::RejectApc { recovery, reason } => f
                .debug_struct("RejectApc")
                .field("recovery", recovery)
                .field("reason", reason)
                .finish(),
        }
    }
}
#[derive(Debug)]
pub struct Step<'a> {
    pub consumed: usize,
    pub event: Option<Event<'a>>,
    pub capture: Option<CaptureStep>,
    /// The bounded scanner abandoned an APC. Hosts must silently discard
    /// in-flight graphics assembly, without reflecting rejected input or
    /// removing already-completed images. False for ordinary boundaries.
    pub abandoned_apc: bool,
}

#[derive(Debug)]
enum State {
    Ground,
    DiscardString {
        kind: StringKind,
        previous_escape: bool,
    },
    DiscardEscape,
    DiscardCsi,
    Escape(Vec<u8>),
    Charset(u8),
    Csi {
        params: Vec<u8>,
        intermediates: Vec<u8>,
    },
    String {
        kind: StringKind,
        data: Vec<u8>,
    },
    DiscardApc {
        previous_escape: bool,
    },
}

#[derive(Debug)]
pub struct Parser {
    limits: ParserLimits,
    state: State,
    utf8: Vec<u8>,
    replacements: usize,
    profile: ParserProfile,
    utf8_expected: u8,
    feed_start: bool,
    feed_offset: usize,
    string_carried: bool,
    capture: Option<OpenCapture>,
    ascii_batching: bool,
    bounded: bool,
    apc_bytes: Option<usize>,
    rejected: u64,
}

/// Allocating diagnostic view for compatibility tests, never a second parser owner.
#[derive(Debug, Default)]
pub struct ParserSnapshot {
    pub pending_escape: Vec<u8>,
    pub pending_apc: Vec<u8>,
    pub pending_osc: Vec<u8>,
    pub pending_string: Vec<u8>,
    pub pending_apc_scan_from: usize,
    pub pending_osc_scan_from: usize,
    pub pending_string_scan_from: usize,
    pub discarding_oversized_apc: bool,
    pub discarding_apc_prev_escape: bool,
    pub utf8_buf: [u8; 4],
    pub utf8_len: u8,
    pub utf8_expected: u8,
}

impl Parser {
    pub fn new(limits: ParserLimits) -> Self {
        Self::with_profile(limits, ParserProfile::EmberV1)
    }
    pub fn with_profile(limits: ParserLimits, profile: ParserProfile) -> Self {
        Self {
            limits,
            state: State::Ground,
            utf8: Vec::with_capacity(4),
            replacements: 0,
            profile,
            utf8_expected: 0,
            feed_start: false,
            feed_offset: 0,
            string_carried: false,
            capture: None,
            ascii_batching: true,
            bounded: false,
            apc_bytes: None,
            rejected: 0,
        }
    }
    /// Opt-in hard bounds, independent of legacy feed-partition behavior.
    /// `pending_bytes` bounds string payload (1..=16 MiB). ESC intermediates
    /// have a separate cap of min(pending_bytes, 8). Rejections emit Boundary,
    /// not Kitty recovery replies. Discard ignores embedded ESC controls until
    /// ST, OSC BEL, CAN or SUB.
    /// A pending ST prefix may retain one additional ESC byte. CSI retains its
    /// existing fixed caps. No event queue is allocated by this parser.
    pub fn with_bounded_profile(
        limits: ParserLimits,
        profile: ParserProfile,
    ) -> Result<Self, &'static str> {
        const MAX: usize = 16 * 1024 * 1024;
        if limits.pending_bytes == 0
            || limits.pending_bytes > MAX
            || limits.kitty_recovery_bytes > MAX
        {
            return Err("bounded parser limits must be within 1..=16 MiB (recovery may be zero)");
        }
        let mut parser = Self::with_profile(limits, profile);
        parser.bounded = true;
        Ok(parser)
    }
    pub(crate) fn enable_inline_graphics_packets(&mut self) {
        self.apc_bytes = Some(64 * 1024);
    }

    fn string_limit(&self, kind: StringKind) -> usize {
        if kind == StringKind::Apc {
            return self.apc_bytes.unwrap_or(self.limits.pending_bytes);
        }
        self.limits.pending_bytes
    }

    /// Lifetime count of resource rejections. Reset clears parser state, not this counter.
    pub fn rejected_count(&self) -> u64 {
        self.rejected
    }
    fn frost(&self) -> bool {
        self.profile == ParserProfile::FrostLegacyV1
    }
    /// Call once for each host feed, including empty feeds, then drain `next`
    /// through its first `event == None`. Never call between events in one feed.
    pub fn begin_feed(&mut self) {
        self.feed_start = true;
        self.feed_offset = 0;
        self.string_carried = matches!(self.state, State::String { .. });
        self.capture = None;
    }
    /// Host-controlled fast path. Frost disables batching for insert mode and
    /// non-ASCII charset dispatch, preserving its per-glyph capture boundaries.
    pub fn set_ascii_batching(&mut self, enabled: bool) {
        self.ascii_batching = enabled;
    }
    pub fn pending_kind(&self) -> PendingKind {
        match self.state {
            State::Ground => PendingKind::None,
            State::Escape(_) => PendingKind::Escape,
            State::Charset(_) => PendingKind::Charset,
            State::Csi { .. } | State::DiscardCsi => PendingKind::Csi,
            State::String { kind, .. } => PendingKind::String(kind),
            State::DiscardApc { .. } => PendingKind::DiscardApc,
            State::DiscardString { kind, .. } => PendingKind::String(kind),
            State::DiscardEscape => PendingKind::Escape,
        }
    }
    fn open_capture(
        profile: ParserProfile,
        capture: &mut Option<OpenCapture>,
        feed_offset: usize,
        state: &State,
        input: &[u8],
        at: usize,
    ) -> bool {
        if profile != ParserProfile::FrostLegacyV1 || capture.is_some() {
            return false;
        }
        let mut prefix = Vec::new();
        let mut private = false;
        // Carried strings complete through Frost's resume path, which has no
        // direct-ESC per-token takeover hook. The outer feed hook remains in
        // the host; APC semantic dispatch independently retains its own hook.
        let escape = matches!(
            state,
            State::Escape(_) | State::Charset(_) | State::Csi { .. }
        );
        match state {
            State::Escape(bytes) => {
                let rest = &input[at..];
                if !(rest.is_empty() || bytes.is_empty() && rest == b"[") {
                    prefix.push(0x1b);
                    prefix.extend(bytes);
                }
            }
            State::Csi {
                params,
                intermediates,
            } => {
                // A lone parameter or intermediate byte cannot complete
                // this capture before the current feed ends.
                if !matches!(&input[at..], [] | [0x20..=0x3f]) {
                    prefix = Vec::with_capacity(2 + params.len() + intermediates.len());
                    prefix.extend(b"\x1b[");
                    prefix.extend(params);
                    prefix.extend(intermediates);
                }
            }
            State::Charset(bank) => prefix.extend([0x1b, *bank]),
            State::String { .. }
            | State::DiscardApc { .. }
            | State::DiscardString { .. }
            | State::DiscardEscape
            | State::DiscardCsi => private = true,
            State::Ground => {}
        }
        *capture = Some(OpenCapture {
            prefix,
            start: feed_offset + at,
            private,
            escape,
        });
        true
    }
    fn capture_step(&mut self, at: usize, started: bool, end: Option<bool>) -> Option<CaptureStep> {
        if !self.frost() {
            return None;
        }
        let end = end.and_then(|complete| {
            self.capture.take().map(|capture| {
                if !complete {
                    return CaptureEnd::Abandoned;
                }
                CaptureEnd::Complete {
                    token: (!capture.private).then_some(CaptureToken {
                        prefix: capture.prefix,
                        input_range: capture.start..self.feed_offset + at,
                    }),
                    escape: capture.escape,
                }
            })
        });
        (started || end.is_some()).then_some(CaptureStep { started, end })
    }
    pub fn snapshot(&self) -> ParserSnapshot {
        let mut result = ParserSnapshot::default();
        result.utf8_buf[..self.utf8.len()].copy_from_slice(&self.utf8);
        result.utf8_len = self.utf8.len() as u8;
        result.utf8_expected = if self.utf8.is_empty() {
            0
        } else {
            self.utf8_expected
        };
        match &self.state {
            State::Ground => {}
            State::Escape(intermediates) => {
                result.pending_escape.push(0x1b);
                result.pending_escape.extend(intermediates);
            }
            State::Charset(bank) => result.pending_escape.extend([0x1b, *bank]),
            State::Csi {
                params,
                intermediates,
            } => {
                result.pending_escape.extend(b"\x1b[");
                result.pending_escape.extend(params);
                result.pending_escape.extend(intermediates);
            }
            State::String { kind, data } => {
                let (target, intro) = match kind {
                    StringKind::Osc => (&mut result.pending_osc, b']'),
                    StringKind::Apc => (&mut result.pending_apc, b'_'),
                    StringKind::Dcs => (&mut result.pending_string, b'P'),
                    StringKind::Sos => (&mut result.pending_string, b'X'),
                    StringKind::Pm => (&mut result.pending_string, b'^'),
                };
                target.extend([0x1b, intro]);
                target.extend(data);
            }
            State::DiscardString { .. } | State::DiscardEscape | State::DiscardCsi => {}
            State::DiscardApc { previous_escape } => {
                result.discarding_oversized_apc = true;
                result.discarding_apc_prev_escape = *previous_escape;
            }
        }
        result.pending_apc_scan_from = result.pending_apc.len().saturating_sub(1);
        result.pending_osc_scan_from = result.pending_osc.len().saturating_sub(1);
        result.pending_string_scan_from = result.pending_string.len().saturating_sub(1);
        result
    }

    pub fn reset(&mut self) {
        self.state = State::Ground;
        self.utf8.clear();
        self.replacements = 0;
        self.utf8_expected = 0;
        self.capture = None;
        self.string_carried = false;
    }
    /// Matches Ember's watcher predicate, deliberately excluding UTF-8 and discard.
    pub fn legacy_pending_control(&self) -> bool {
        !matches!(
            self.state,
            State::Ground
                | State::DiscardApc { .. }
                | State::DiscardString { .. }
                | State::DiscardEscape
                | State::DiscardCsi
        )
    }
    /// Bytes retained as scanner carry, excluding returned events/capture spans.
    /// Frost's legacy OSC/DCS policy can retain an over-limit crossing feed.
    pub fn retained_bytes(&self) -> usize {
        self.utf8.len()
            + match &self.state {
                State::Escape(v) => v.len() + 1,
                State::Charset(_) => 2,
                State::Csi {
                    params,
                    intermediates,
                } => params.len() + intermediates.len() + 2,
                State::String { data, .. } => data.len() + 2,
                _ => 0,
            }
    }
    fn recovery(&self, old: &[u8], new: &[u8]) -> Vec<u8> {
        let first = old.first().or_else(|| new.first());
        // Fresh Frost rejections historically passed the full payload to its
        // responder, which scans MAX_CONTROL_BYTES after an optional G. Carried
        // recovery historically counts G inside that bound instead.
        let limit = self.limits.kitty_recovery_bytes
            + usize::from(self.frost() && !self.string_carried && first == Some(&b'G'));
        let mut result = old[..old.len().min(limit)].to_vec();
        if result.len() < limit && !result.contains(&b';') {
            result.extend_from_slice(&new[..new.len().min(limit - result.len())]);
        }
        result
    }
    /// `consumed == 0` is allowed only for a finite pending event. Call again
    /// with the same input after dispatching it. Never retain events across reset.
    /// `event == None` always consumes all supplied input and ends this feed.
    /// An empty feed does not flush incomplete UTF-8 or control sequences.
    pub fn next<'a>(&mut self, input: &'a [u8]) -> Step<'a> {
        let mut at = 0;
        let mut started = false;
        let mut abandoned_apc = false;
        let feed_start = std::mem::take(&mut self.feed_start);
        // Frost only drops an over-limit nF prefix at the following feed.
        if !self.bounded
            && self.frost()
            && feed_start
            && matches!(&self.state, State::Escape(v) if v.len() + 1 > self.limits.pending_bytes)
        {
            self.state = State::Ground;
        }
        macro_rules! step {
            ($event:expr, $end:expr) => {{
                let capture = self.capture_step(at, started, $end);
                self.feed_offset += at;
                return Step {
                    consumed: at,
                    event: $event,
                    capture,
                    abandoned_apc,
                };
            }};
        }
        macro_rules! emit {
            ($e:expr) => {
                step!(Some($e), Some(true))
            };
        }
        macro_rules! emit_open {
            ($e:expr) => {
                step!(Some($e), None)
            };
        }
        macro_rules! abandon {
            ($e:expr) => {
                step!($e, Some(false))
            };
        }
        if self.replacements > 0 {
            self.replacements -= 1;
            emit!(Event::Print('\u{fffd}'));
        }
        loop {
            if at < input.len() || !matches!(self.state, State::Ground) {
                started |= Self::open_capture(
                    self.profile,
                    &mut self.capture,
                    self.feed_offset,
                    &self.state,
                    input,
                    at,
                );
            }
            if at == input.len()
                && (matches!(self.state, State::Ground | State::Csi { .. })
                    || (self.bounded && matches!(self.state, State::String { .. })))
            {
                abandon!(None);
            }
            // Ordinary carry bytes do not change the active state. Append them
            // in place, avoiding a move of the Vec-bearing state for each byte
            // (including fragmented single-byte PTY reads). Special bytes and
            // exact overflow boundaries still enter the original reducer below.
            match &mut self.state {
                State::Ground => {
                    let Some(&byte) = input.get(at) else {
                        abandon!(None);
                    };
                    if !self.utf8.is_empty() {
                        if byte & 0xc0 != 0x80 {
                            self.utf8.clear();
                            emit_open!(Event::Print('\u{fffd}'));
                        }
                        self.utf8.push(byte);
                        at += 1;
                        if self.frost() {
                            if self.utf8.len() == self.utf8_expected as usize {
                                let ch = std::str::from_utf8(&self.utf8)
                                    .ok()
                                    .and_then(|s| s.chars().next())
                                    .unwrap_or('\u{fffd}');
                                self.utf8.clear();
                                emit!(Event::Print(ch));
                            }
                            emit!(Event::Boundary);
                        }
                        match std::str::from_utf8(&self.utf8) {
                            Ok(text) => {
                                let ch = text.chars().next().unwrap();
                                self.utf8.clear();
                                emit!(Event::Print(ch));
                            }
                            Err(error) => {
                                if let Some(invalid) = error.error_len() {
                                    self.replacements = self.utf8.len() - invalid;
                                    self.utf8.clear();
                                    emit!(Event::Print('\u{fffd}'));
                                }
                            }
                        }
                        continue;
                    }
                    at += 1;
                    match byte {
                        0x1b => {
                            if let Some(capture) = self.capture.as_mut() {
                                capture.escape = true;
                            }
                            // A present two-byte introducer has no
                            // intermediate public event. Enter its active state
                            // directly instead of moving an empty Escape state
                            // through another reducer iteration. Split prefixes
                            // retain the ordinary Escape path below.
                            let kind = if self.bounded {
                                match input.get(at) {
                                    Some(b'[') => {
                                        at += 1;
                                        if at < input.len() {
                                            if let Some((consumed, event)) =
                                                complete_short_csi(&input[at..], self.profile)
                                            {
                                                at += consumed;
                                                emit!(event);
                                            }
                                        }
                                        self.state = State::Csi {
                                            params: Vec::new(),
                                            intermediates: Vec::new(),
                                        };
                                        continue;
                                    }
                                    Some(b']') => Some(StringKind::Osc),
                                    Some(b'P') => Some(StringKind::Dcs),
                                    Some(b'X') => Some(StringKind::Sos),
                                    Some(b'^') => Some(StringKind::Pm),
                                    Some(b'_') => Some(StringKind::Apc),
                                    _ => None,
                                }
                            } else {
                                None
                            };
                            if let Some(kind) = kind {
                                at += 1;
                                self.state = State::String {
                                    kind,
                                    data: Vec::new(),
                                };
                                self.string_carried = false;
                                if let Some(capture) = self.capture.as_mut() {
                                    capture.private = true;
                                }
                            } else {
                                self.state = State::Escape(Vec::new());
                            }
                        }
                        32..=126 => {
                            let start = at - 1;
                            if self.ascii_batching {
                                at += printable_ascii_prefix(&input[at..]);
                            }
                            emit!(Event::Ascii(&input[start..at]));
                        }
                        0xc2..=0xf4 => {
                            let expected = if byte <= 0xdf {
                                2
                            } else if byte <= 0xef {
                                3
                            } else {
                                4
                            };
                            self.utf8_expected = expected;
                            if !self.frost() && at - 1 + expected as usize <= input.len() {
                                let bytes = &input[at - 1..at - 1 + expected as usize];
                                if let Some(ch) = complete_utf8_scalar(bytes) {
                                    at += expected as usize - 1;
                                    emit!(Event::Print(ch));
                                }
                            }
                            if self.frost() {
                                if at - 1 + expected as usize <= input.len() {
                                    let bytes = &input[at - 1..at - 1 + expected as usize];
                                    if bytes[1..].iter().all(|b| b & 0xc0 == 0x80) {
                                        at += expected as usize - 1;
                                        let ch = std::str::from_utf8(bytes)
                                            .ok()
                                            .and_then(|s| s.chars().next())
                                            .unwrap_or('\u{fffd}');
                                        emit!(Event::Print(ch));
                                    }
                                    emit!(Event::Print('\u{fffd}'));
                                }
                                self.utf8.push(byte);
                                emit!(Event::Boundary);
                            }
                            self.utf8.push(byte);
                        }
                        0x80..=0xff => emit!(Event::Print('\u{fffd}')),
                        7..=15 => emit!(Event::Control(byte)),
                        _ => {
                            if self.frost() {
                                emit!(Event::Boundary);
                            }
                        }
                    }
                    continue;
                }
                State::Csi {
                    params,
                    intermediates,
                } => {
                    if let Some(&byte) = input.get(at) {
                        let param_end = if self.bounded { 0x3b } else { 0x3f };
                        if (0x30..=param_end).contains(&byte)
                            && (!self.bounded || intermediates.is_empty())
                        {
                            let limit = if self.profile == ParserProfile::FrostLegacyV1 {
                                128
                            } else {
                                256
                            };
                            let length = input[at..]
                                .iter()
                                .take_while(|&&byte| (0x30..=param_end).contains(&byte))
                                .count();
                            let retained = length.min(limit - params.len());
                            params.extend_from_slice(&input[at..at + retained]);
                            at += length;
                            continue;
                        }
                        if (0x20..=0x2f).contains(&byte) {
                            if intermediates.len() < 8 {
                                intermediates.push(byte);
                            }
                            at += 1;
                            continue;
                        }
                    }
                }
                State::String { kind, data }
                    if !self.bounded
                        && input.len() - at == 1
                        && data.last() != Some(&0x1b)
                        && data
                            .len()
                            .saturating_add(input.len() - at)
                            .saturating_add(2)
                            <= self.limits.pending_bytes
                        && input[at..].iter().all(|byte| {
                            !matches!(byte, 0x1b | 0x18 | 0x1a)
                                && (*kind != StringKind::Osc || *byte != 7)
                        }) =>
                {
                    // No framing byte and no admission boundary can occur in
                    // this span. Preserve legacy end-of-feed capture/retention
                    // while keeping the payload buffer in its active state.
                    data.extend_from_slice(&input[at..]);
                    at = input.len();
                    abandon!(None);
                }
                State::String { kind, data }
                    if self.bounded && !data.is_empty() && data.last() != Some(&0x1b) =>
                {
                    let limit = match kind {
                        StringKind::Apc => self.apc_bytes.unwrap_or(self.limits.pending_bytes),
                        _ => self.limits.pending_bytes,
                    };
                    let available = limit.saturating_sub(data.len());
                    let remaining = &input[at..];
                    let scan_len = remaining.len().min(available);
                    let ordinary =
                        string_boundary_position(&remaining[..scan_len], *kind == StringKind::Osc)
                            .unwrap_or(scan_len);
                    if ordinary > 0 {
                        data.extend_from_slice(&remaining[..ordinary]);
                        at += ordinary;
                        continue;
                    }
                }
                _ => {}
            }
            let state = std::mem::replace(&mut self.state, State::Ground);
            match state {
                State::DiscardString {
                    kind,
                    mut previous_escape,
                } => {
                    let mut ended = false;
                    while let Some(&byte) = input.get(at) {
                        at += 1;
                        if (previous_escape && byte == b'\\')
                            || (kind == StringKind::Osc && byte == 7)
                            || matches!(byte, 0x18 | 0x1a)
                        {
                            ended = true;
                            break;
                        }
                        previous_escape = byte == 0x1b;
                    }
                    if !ended {
                        self.state = State::DiscardString {
                            kind,
                            previous_escape,
                        };
                        abandon!(None);
                    }
                    abandon!(Some(Event::Boundary));
                }
                discarded @ (State::DiscardCsi | State::DiscardEscape) => {
                    let first_final = if matches!(discarded, State::DiscardCsi) {
                        0x40
                    } else {
                        0x30
                    };
                    while let Some(&byte) = input.get(at) {
                        at += 1;
                        match byte {
                            0x1b => {
                                self.state = State::Escape(Vec::new());
                                abandon!(Some(Event::Boundary));
                            }
                            0x18 | 0x1a => {
                                abandon!(Some(Event::Boundary));
                            }
                            0x00..=0x1f => {
                                self.state = discarded;
                                emit_open!(Event::Control(byte));
                            }
                            final_byte if (first_final..=0x7e).contains(&final_byte) => {
                                abandon!(Some(Event::Boundary));
                            }
                            _ => {}
                        }
                    }
                    self.state = discarded;
                    abandon!(None);
                }
                State::String { kind, mut data } if self.bounded => {
                    // Borrow a complete fresh payload only after scanning its
                    // first framing-sensitive byte inside the exact payload
                    // bound. Cancellation, fresh ESC, split ST and overflow
                    // still use the scalar state machine below unchanged.
                    // This avoids per-byte state moves/allocations for common
                    // short OSC titles without accepting any unchecked tail.
                    if data.is_empty() {
                        let remaining = &input[at..];
                        let limit = self.string_limit(kind);
                        let scan_len = remaining.len().min(limit + 1);
                        if let Some(end) = string_boundary_position(
                            &remaining[..scan_len],
                            kind == StringKind::Osc,
                        ) {
                            let bel = kind == StringKind::Osc && remaining[end] == 7;
                            let st =
                                remaining[end] == 0x1b && remaining.get(end + 1) == Some(&b'\\');
                            if bel || st {
                                at += end + if bel { 1 } else { 2 };
                                if matches!(kind, StringKind::Sos | StringKind::Pm) {
                                    abandon!(Some(Event::Boundary));
                                }
                                emit!(Event::String {
                                    kind,
                                    payload: Cow::Borrowed(&remaining[..end]),
                                    bel_terminated: bel
                                });
                            }
                        } else if scan_len >= 32 {
                            // Reuse the completed scan for a long partial
                            // payload. Leave the first excess byte to the
                            // scalar path.
                            let ordinary = scan_len.min(limit);
                            data.extend_from_slice(&remaining[..ordinary]);
                            at += ordinary;
                        }
                    }
                    let Some(&byte) = input.get(at) else {
                        self.state = State::String { kind, data };
                        abandon!(None);
                    };
                    let previous_escape = data.last() == Some(&0x1b);
                    if previous_escape && byte == b'\\' {
                        data.pop();
                        at += 1;
                        if matches!(kind, StringKind::Sos | StringKind::Pm) {
                            abandon!(Some(Event::Boundary));
                        }
                        emit!(Event::String {
                            kind,
                            payload: Cow::Owned(data),
                            bel_terminated: false
                        });
                    }
                    if matches!(byte, 0x18 | 0x1a) {
                        abandoned_apc = kind == StringKind::Apc;
                        at += 1;
                        abandon!(Some(Event::Boundary));
                    }
                    if previous_escape {
                        abandoned_apc = kind == StringKind::Apc;
                        // Before overflow, preserve normal fresh-ESC restart.
                        self.state = State::Escape(Vec::new());
                        abandon!(Some(Event::Boundary));
                    }
                    if kind == StringKind::Osc && byte == 7 {
                        at += 1;
                        emit!(Event::String {
                            kind,
                            payload: Cow::Owned(data),
                            bel_terminated: true
                        });
                    }
                    if byte != 0x1b && data.len() >= self.string_limit(kind) {
                        abandoned_apc = kind == StringKind::Apc;
                        at += 1;
                        self.rejected = self.rejected.saturating_add(1);
                        self.state = State::DiscardString {
                            kind,
                            previous_escape: false,
                        };
                        abandon!(Some(Event::Boundary));
                    }
                    data.push(byte);
                    at += 1;
                    self.state = State::String { kind, data };
                }
                State::String { kind, mut data } => {
                    let remaining = &input[at..];
                    let previous_escape = data.last() == Some(&0x1b);
                    let boundary =
                        legacy_string_boundary(remaining, previous_escape, kind == StringKind::Osc);
                    if let LegacyStringBoundary::Abort(used, restore) = boundary {
                        let recovery = self.recovery(&data, &remaining[..used]);
                        at += used;
                        if restore {
                            self.state = State::Escape(Vec::new());
                        }
                        let graphics = if self.frost() {
                            !self.string_carried || data.first() == Some(&b'G')
                        } else {
                            recovery.first() == Some(&b'G')
                        };
                        if kind == StringKind::Apc && graphics {
                            abandon!(Some(Event::RejectApc {
                                recovery,
                                reason: RejectReason::Cancelled
                            }));
                        }
                        if self.frost() {
                            abandon!(Some(Event::Boundary));
                        }
                        continue;
                    }
                    // Frost cancellation has priority over next-feed overflow.
                    if self.frost()
                        && feed_start
                        && kind != StringKind::Apc
                        && data.len() + 2 > self.limits.pending_bytes
                    {
                        abandon!(Some(Event::Boundary));
                    }
                    let terminator = match boundary {
                        LegacyStringBoundary::End(body_end, consumed, bel, split) => {
                            Some((body_end, consumed, bel, split))
                        }
                        _ => None,
                    };
                    if let Some((body_end, consumed, bel, split)) = terminator {
                        let too_large = data.len().saturating_add(consumed).saturating_add(2)
                            > self.limits.pending_bytes;
                        if kind == StringKind::Apc
                            && too_large
                            && (!self.frost() || self.string_carried)
                        {
                            let recovery = self.recovery(&data, &remaining[..consumed]);
                            at += consumed;
                            if recovery.first() == Some(&b'G')
                                && (!self.frost()
                                    || !self.string_carried
                                    || data.first() == Some(&b'G'))
                            {
                                abandon!(Some(Event::RejectApc {
                                    recovery,
                                    reason: RejectReason::TooLarge
                                }));
                            }
                            if self.frost() {
                                abandon!(Some(Event::Boundary));
                            }
                            continue;
                        }
                        let payload = if data.is_empty() && !split {
                            Cow::Borrowed(&remaining[..body_end])
                        } else {
                            if split {
                                data.pop();
                            }
                            data.extend_from_slice(&remaining[..body_end]);
                            Cow::Owned(data)
                        };
                        at += consumed;
                        if matches!(kind, StringKind::Sos | StringKind::Pm) {
                            if self.frost() {
                                emit!(Event::Boundary);
                            }
                            continue;
                        }
                        emit!(Event::String {
                            kind,
                            payload,
                            bel_terminated: bel
                        });
                    }
                    if data.len().saturating_add(remaining.len()).saturating_add(2)
                        > self.limits.pending_bytes
                        && (!self.frost() || kind == StringKind::Apc)
                    {
                        at = input.len();
                        if kind == StringKind::Apc {
                            let recovery = self.recovery(&data, remaining);
                            self.state = State::DiscardApc {
                                previous_escape: remaining
                                    .last()
                                    .copied()
                                    .or_else(|| data.last().copied())
                                    == Some(0x1b),
                            };
                            if recovery.first() == Some(&b'G')
                                && (!self.frost()
                                    || !self.string_carried
                                    || data.first() == Some(&b'G'))
                            {
                                abandon!(Some(Event::RejectApc {
                                    recovery,
                                    reason: RejectReason::TooLarge
                                }));
                            }
                        }
                        abandon!(None);
                    }
                    data.extend_from_slice(remaining);
                    self.state = State::String { kind, data };
                    at = input.len();
                    abandon!(None);
                }
                State::DiscardApc {
                    mut previous_escape,
                } => {
                    let remaining = &input[at..];
                    if let Some((used, restore)) =
                        crate::framing::control_string_abort(remaining, previous_escape, false)
                    {
                        at += used;
                        if restore {
                            self.state = State::Escape(Vec::new());
                        }
                        if self.frost() {
                            abandon!(Some(Event::Boundary));
                        }
                        continue;
                    }
                    let mut ended = false;
                    for &byte in remaining {
                        at += 1;
                        if previous_escape && byte == b'\\' {
                            ended = true;
                            break;
                        }
                        previous_escape = byte == 0x1b;
                    }
                    if ended {
                        if self.frost() {
                            abandon!(Some(Event::Boundary));
                        }
                        continue;
                    }
                    self.state = State::DiscardApc { previous_escape };
                    abandon!(None);
                }
                State::Ground => unreachable!("ground is advanced in place"),
                State::Charset(bank) => {
                    let Some(&designator) = input.get(at) else {
                        self.state = State::Charset(bank);
                        abandon!(None);
                    };
                    at += 1;
                    if self.bounded {
                        match designator {
                            0x1b => {
                                self.state = State::Escape(Vec::new());
                                abandon!(Some(Event::Boundary));
                            }
                            0x18 | 0x1a => abandon!(Some(Event::Boundary)),
                            0x00..=0x1f => {
                                self.state = State::Charset(bank);
                                emit_open!(Event::Control(designator));
                            }
                            0x7f => {
                                self.state = State::Charset(bank);
                                continue;
                            }
                            0x20..=0x2f => {
                                if self.limits.pending_bytes.min(8) < 2 {
                                    self.rejected = self.rejected.saturating_add(1);
                                    self.state = State::DiscardEscape;
                                    abandon!(Some(Event::Boundary));
                                }
                                self.state = State::Escape(vec![bank, designator]);
                                continue;
                            }
                            _ => {}
                        }
                    }
                    emit!(Event::Charset { bank, designator });
                }
                State::Escape(mut intermediates) => {
                    let Some(&byte) = input.get(at) else {
                        self.state = State::Escape(intermediates);
                        abandon!(None);
                    };
                    if self.bounded {
                        match byte {
                            0x00..=0x17 | 0x19 | 0x1c..=0x1f => {
                                at += 1;
                                self.state = State::Escape(intermediates);
                                emit_open!(Event::Control(byte));
                            }
                            0x7f => {
                                at += 1;
                                self.state = State::Escape(intermediates);
                                continue;
                            }
                            _ => {}
                        }
                    }
                    if intermediates.is_empty() {
                        match byte {
                            b'[' => {
                                at += 1;
                                if at < input.len() {
                                    if let Some((consumed, event)) =
                                        complete_short_csi(&input[at..], self.profile)
                                    {
                                        at += consumed;
                                        emit!(event);
                                    }
                                }
                                self.state = State::Csi {
                                    params: Vec::new(),
                                    intermediates: Vec::new(),
                                };
                                continue;
                            }
                            b']' | b'P' | b'X' | b'^' | b'_' => {
                                at += 1;
                                let kind = match byte {
                                    b']' => StringKind::Osc,
                                    b'P' => StringKind::Dcs,
                                    b'X' => StringKind::Sos,
                                    b'^' => StringKind::Pm,
                                    _ => StringKind::Apc,
                                };
                                self.state = State::String {
                                    kind,
                                    data: Vec::new(),
                                };
                                self.string_carried = false;
                                if let Some(capture) = self.capture.as_mut() {
                                    capture.private = true;
                                }
                                continue;
                            }
                            b'(' | b')' if !self.frost() => {
                                at += 1;
                                self.state = State::Charset(byte);
                                continue;
                            }
                            _ => {}
                        }
                        if self.frost() && byte == 0x1b {
                            emit!(Event::Boundary);
                        }
                        if self.frost() && !(0x20..=0x7e).contains(&byte) {
                            at += 1;
                            emit!(Event::Boundary);
                        }
                    }
                    match byte {
                        0x20..=0x2f => {
                            if self.bounded
                                && intermediates.len() >= self.limits.pending_bytes.min(8)
                            {
                                at += 1;
                                self.rejected = self.rejected.saturating_add(1);
                                self.state = State::DiscardEscape;
                                abandon!(Some(Event::Boundary));
                            }
                            intermediates.push(byte);
                            at += 1;
                            if !self.bounded
                                && !self.frost()
                                && intermediates.len() + 1 > self.limits.pending_bytes
                                && at == input.len()
                            {
                                // Historical pending ESC overflow drops the unfinished read.
                                at = input.len();
                                abandon!(None);
                            }
                            self.state = State::Escape(intermediates);
                        }
                        0x30..=0x7e => {
                            at += 1;
                            if intermediates.is_empty() && byte == b'c' {
                                self.utf8.clear();
                                self.replacements = 0;
                            }
                            emit!(Event::Escape {
                                intermediates,
                                final_byte: byte
                            });
                        }
                        0x18 | 0x1a => {
                            if self.frost() {
                                emit!(Event::Boundary);
                            }
                            at += 1;
                        }
                        _ => {
                            if self.frost() {
                                emit!(Event::Boundary);
                            }
                        } // Reprocess replacement ESC/C0/DEL/UTF-8 in ground.
                    }
                }
                State::Csi {
                    mut params,
                    mut intermediates,
                } => {
                    let Some(&byte) = input.get(at) else {
                        self.state = State::Csi {
                            params,
                            intermediates,
                        };
                        abandon!(None);
                    };
                    at += 1;
                    match byte {
                        0x30..=0x3f => {
                            if self.bounded
                                && (!intermediates.is_empty()
                                    || (byte >= 0x3c && !params.is_empty()))
                            {
                                self.state = State::DiscardCsi;
                                abandon!(Some(Event::Boundary));
                            }
                            if params.len() < if self.frost() { 128 } else { 256 } {
                                params.push(byte);
                            }
                        }
                        0x20..=0x2f => {
                            if intermediates.len() < 8 {
                                intermediates.push(byte);
                            }
                        }
                        0x40..=0x7e => {
                            let private_prefix = params
                                .first()
                                .copied()
                                .filter(|b| matches!(b, b'<' | b'=' | b'>' | b'?'));
                            if private_prefix.is_some() {
                                params.remove(0);
                            }
                            let raw_params = params;
                            let (params, colon_flags) =
                                parse_profiled_csi_params(&raw_params, self.profile);
                            emit!(Event::Csi {
                                raw_params,
                                params,
                                colon_flags,
                                private_prefix,
                                intermediates,
                                final_byte: byte
                            });
                        }
                        0x7f if self.bounded => {}
                        8..=15 => {
                            self.state = State::Csi {
                                params,
                                intermediates,
                            };
                            emit_open!(Event::Control(byte));
                        }
                        0x18 | 0x1a => {
                            if self.frost() {
                                abandon!(Some(Event::Boundary));
                            }
                            continue;
                        }
                        0x1b => {
                            at -= 1;
                            if self.frost() {
                                abandon!(Some(Event::Boundary));
                            }
                            continue;
                        }
                        0..=31 => {
                            if byte == 7 && self.bounded {
                                self.state = State::Csi {
                                    params,
                                    intermediates,
                                };
                                emit_open!(Event::Control(byte));
                            }
                        }
                        _ => {
                            if self.bounded {
                                // End the incomplete CSI and let ground handle UTF-8.
                                at -= 1;
                                abandon!(Some(Event::Boundary));
                            }
                            self.state = State::Csi {
                                params,
                                intermediates,
                            };
                            at = input.len();
                            abandon!(None);
                        }
                    }
                    self.state = State::Csi {
                        params,
                        intermediates,
                    };
                }
            }
        }
    }
}

// Complete short CSI commands without building parser carry. The state machine
// handles split input, intermediates and parameter lists beyond the lookahead.
#[inline(never)]
fn complete_short_csi(input: &[u8], profile: ParserProfile) -> Option<(usize, Event<'_>)> {
    const MAX_SHORT_PARAMS: usize = 16;
    let (private_prefix, body) = match input {
        [first @ (b'<' | b'=' | b'>' | b'?'), rest @ ..] => (Some(*first), rest),
        _ => (None, input),
    };
    let end = body
        .iter()
        .take(MAX_SHORT_PARAMS + 1)
        .position(|&byte| !(0x30..=0x3b).contains(&byte))?;
    let final_byte = body[end];
    if !(0x40..=0x7e).contains(&final_byte) {
        return None;
    }
    let consumed = end + 1 + usize::from(private_prefix.is_some());
    let raw_params = &body[..end];
    let (params, colon_flags) = parse_profiled_csi_params(raw_params, profile);
    Some((
        consumed,
        Event::Csi {
            raw_params: raw_params.to_vec(),
            params,
            colon_flags,
            private_prefix,
            intermediates: Vec::new(),
            final_byte,
        },
    ))
}

// Keep whole-scalar validation out of the scanner's ASCII hot loop.
#[inline(never)]
fn complete_utf8_scalar(bytes: &[u8]) -> Option<char> {
    std::str::from_utf8(bytes).ok()?.chars().next()
}

/// A single ordered scan replaces the legacy abort scan followed by a
/// separate terminator scan. In particular, a fresh ESC aborts before any
/// later BEL, while a real terminator ends scanning before later cancellation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LegacyStringBoundary {
    Abort(usize, bool),
    End(usize, usize, bool, bool),
    Incomplete,
}

fn legacy_string_boundary(input: &[u8], previous_escape: bool, osc: bool) -> LegacyStringBoundary {
    if previous_escape {
        return match input.first() {
            Some(b'\\') => LegacyStringBoundary::End(0, 1, false, true),
            Some(0x18 | 0x1a) => LegacyStringBoundary::Abort(1, false),
            Some(_) => LegacyStringBoundary::Abort(0, true),
            None => LegacyStringBoundary::Incomplete,
        };
    }
    for (index, byte) in input.iter().copied().enumerate() {
        match byte {
            0x18 | 0x1a => return LegacyStringBoundary::Abort(index + 1, false),
            7 if osc => return LegacyStringBoundary::End(index, index + 1, true, false),
            0x1b => {
                return match input.get(index + 1) {
                    Some(b'\\') => LegacyStringBoundary::End(index, index + 2, false, false),
                    Some(_) => LegacyStringBoundary::Abort(index, false),
                    None => LegacyStringBoundary::Incomplete,
                }
            }
            _ => {}
        }
    }
    LegacyStringBoundary::Incomplete
}

pub fn parse_csi_params(bytes: &[u8]) -> (Vec<u16>, Vec<bool>) {
    parse_profiled_csi_params(bytes, ParserProfile::EmberV1)
}

fn parse_profiled_csi_params(bytes: &[u8], profile: ParserProfile) -> (Vec<u16>, Vec<bool>) {
    if bytes.is_empty() {
        return (Vec::new(), Vec::new());
    }
    // Long lists can otherwise grow both output buffers several times. Keep
    // the common short sequences single-pass.
    let mut params = Vec::new();
    let mut flags = Vec::new();
    // Five-field RGB lists can fit in 16 bytes but exceed Vec's initial
    // four-parameter allocation; reserve before the first growth there too.
    if bytes.len() > 8 {
        let capacity = 1 + bytes
            .iter()
            .filter(|&&byte| matches!(byte, b';' | b':'))
            .count();
        params.reserve_exact(capacity);
        flags.reserve_exact(capacity);
    }
    let mut current: u16 = 0;
    let mut colon = false;
    let mut has_digits = false;
    let fill_empty = profile == ParserProfile::EmberV1;
    for &byte in bytes {
        match byte {
            b'0'..=b'9' => {
                current = current
                    .saturating_mul(10)
                    .saturating_add((byte - b'0') as u16);
                has_digits = true;
            }
            b';' | b':' => {
                if has_digits || fill_empty {
                    params.push(current);
                    flags.push(colon);
                }
                current = 0;
                has_digits = false;
                colon = byte == b':';
            }
            _ => {}
        }
    }
    if has_digits || fill_empty {
        params.push(current);
        flags.push(colon);
    }
    (params, flags)
}

#[cfg(test)]
#[allow(dead_code)]
#[path = "../tests/support/parser_f243_frozen.rs"]
mod frozen_f243;

#[cfg(test)]
#[path = "../tests/support/framing_f243_frozen.rs"]
mod frozen_f243_framing;

#[cfg(test)]
mod hot_path_parity {
    use super::*;

    #[test]
    fn string_boundary_scan_matches_scalar_across_words_and_tails() {
        for osc in [false, true] {
            for len in [0, 1, 7, 8, 31, 32, 33, 47, 64] {
                let mut data = vec![0xe1; len];
                assert_eq!(string_boundary_position(&data, osc), None);
                for index in 0..len {
                    for byte in u8::MIN..=u8::MAX {
                        data[index] = byte;
                        let expected = data
                            .iter()
                            .position(|&b| matches!(b, 0x1b | 0x18 | 0x1a) || (osc && b == 7));
                        assert_eq!(string_boundary_position(&data, osc), expected);
                    }
                    data[index] = 0xe1;
                }
            }
            let mut data = [b'a'; 40];
            data[8] = 7;
            data[14] = 0x1b;
            data[30] = 0x18;
            assert_eq!(
                string_boundary_position(&data, osc),
                Some(if osc { 8 } else { 14 })
            );
        }
    }

    #[test]
    fn printable_prefix_matches_a_byte_scan_at_every_word_and_tail_offset() {
        for offset in 0..8 {
            for length in 0..=27 {
                let mut storage = [b'x'; 40];
                let bytes = &mut storage[offset..offset + length];
                assert_eq!(printable_ascii_prefix(bytes), length);
                for index in 0..length {
                    for byte in 0..=u8::MAX {
                        bytes[index] = byte;
                        let expected = bytes
                            .iter()
                            .take_while(|&&b| (0x20..=0x7e).contains(&b))
                            .count();
                        assert_eq!(printable_ascii_prefix(bytes), expected);
                    }
                    bytes[index] = b'x';
                }
            }
        }
    }

    #[test]
    fn single_scan_matches_independent_legacy_abort_then_terminator_scans() {
        fn reference(input: &[u8], previous_escape: bool, osc: bool) -> LegacyStringBoundary {
            if let Some((used, restore)) =
                frozen_f243_framing::control_string_abort(input, previous_escape, osc)
            {
                return LegacyStringBoundary::Abort(used, restore);
            }
            if previous_escape && input.first() == Some(&b'\\') {
                return LegacyStringBoundary::End(0, 1, false, true);
            }
            for (i, byte) in input.iter().enumerate() {
                if osc && *byte == 7 {
                    return LegacyStringBoundary::End(i, i + 1, true, false);
                }
                if *byte == 0x1b && input.get(i + 1) == Some(&b'\\') {
                    return LegacyStringBoundary::End(i, i + 2, false, false);
                }
            }
            LegacyStringBoundary::Incomplete
        }
        let alphabet = [0, 7, 0x1b, 0x18, 0x1a, b'\\', b'x', 255];
        for len in 0..=5 {
            for mut code in 0..alphabet.len().pow(len) {
                let mut input = vec![0; len as usize];
                for byte in &mut input {
                    *byte = alphabet[code % alphabet.len()];
                    code /= alphabet.len();
                }
                for previous_escape in [false, true] {
                    for osc in [false, true] {
                        assert_eq!(
                            legacy_string_boundary(&input, previous_escape, osc),
                            reference(&input, previous_escape, osc),
                            "{input:?} previous_escape={previous_escape} osc={osc}"
                        );
                    }
                }
            }
        }
    }

    fn trace(new: &mut Parser, old: &mut frozen_f243::Parser, input: &[u8]) {
        new.begin_feed();
        old.begin_feed();
        let mut at = 0;
        let mut steps = 0;
        loop {
            let a = new.next(&input[at..]);
            let e = old.next(&input[at..]);
            assert_eq!(
                (
                    a.consumed,
                    a.abandoned_apc,
                    format!("{:?}", a.event),
                    format!("{:?}", a.capture)
                ),
                (
                    e.consumed,
                    e.abandoned_apc,
                    format!("{:?}", e.event),
                    format!("{:?}", e.capture)
                ),
                "offset={at} input={input:?}"
            );
            if let (
                Some(Event::Csi {
                    raw_params: actual, ..
                }),
                Some(frozen_f243::Event::Csi {
                    raw_params: expected,
                    ..
                }),
            ) = (&a.event, &e.event)
            {
                assert_eq!(actual, expected, "CSI wire params at offset={at}");
            }
            // Preserve zero-copy complete packet admission as well as the
            // visible event bytes; ordinary carried spans must stay owned.
            assert_eq!(
                matches!(
                    &a.event,
                    Some(Event::String {
                        payload: Cow::Borrowed(_),
                        ..
                    })
                ),
                matches!(
                    &e.event,
                    Some(frozen_f243::Event::String {
                        payload: Cow::Borrowed(_),
                        ..
                    })
                ),
                "packet ownership offset={at} input={input:?}"
            );
            at += a.consumed;
            let done = a.event.is_none();
            assert_eq!(
                format!("{:?}", new.snapshot()),
                format!("{:?}", old.snapshot())
            );
            assert_eq!(new.rejected_count(), old.rejected_count());
            steps += 1;
            assert!(steps <= input.len() * 5 + 32);
            if done {
                break;
            }
        }
        assert_eq!(at, input.len());
    }
    #[test]
    fn optimized_steps_match_independent_f243_parser_at_every_split() {
        let mut inputs = vec![
            b"printable ABC xyz\r\n\t\x07\x0e\x0f".to_vec(),
            b"\x1b[;:12:;65536mX\x1b[2\n;4H".to_vec(),
            b"\x1b[?25h\x1b[>4;1f\x1b[?u".to_vec(),
            b"\x1b[m\x1b[K\x1b[A\x1b[3D\x1b[?1048$py".to_vec(),
            b"\x1b[38;2;12;93;255m\x1b[48:2::90:5:111m".to_vec(),
            b"\x1b]0;title\x07x\x1bP$qm\x1b\\Z".to_vec(),
            b"\x1b_Gi=4,m=1;x\x1b\\\x1b_Gi=4;\x18A".to_vec(),
            b"\x1b(0q\x1b(Bz\x1b( % 5\x1bcX".to_vec(),
            b"\xe0\x80A\xf0\x9f\x99\x82\xff\xc2Z".to_vec(),
            b"\x1b]8;;secret\x1b[6n\x07A".to_vec(),
        ];
        for len in [16, 17] {
            inputs.push([b"\x1b[?".as_slice(), &b"0".repeat(len), b"mX"].concat());
        }
        // Every folded introducer, including prefix-only EOF, terminal and
        // cancelled payloads. The existing matrix also splits after ESC and
        // after the introducer, tests empty feeds, and compares capture/ownership.
        for &introducer in b"]PX^_" {
            for suffix in [b"".as_slice(), b"x\x1b\\Z", b"x\x18Z"] {
                inputs.push([&[0x1b, introducer], suffix].concat());
            }
        }
        let all_control_bytes: Vec<_> = (0..=255).collect();
        let mut bounded_control_bytes = all_control_bytes.clone();
        // Complete ESC before the next C0 in the legacy parity vector.
        // Embedded C0 framing has its own bounded conformance tests.
        bounded_control_bytes.insert(usize::from(b'\x1b') + 1, b'7');
        inputs.push(all_control_bytes.clone());
        inputs.push([b"\x1b]0;".as_slice(), &[b'x'; 64], b"\x1b\\ok"].concat());
        for frost in [false, true] {
            for bounded in [false, true] {
                for batching in [false, true] {
                    for inline in [false, true] {
                        for limit in [1, 8, 32] {
                            for input in &inputs {
                                let input = if bounded && input == &all_control_bytes {
                                    &bounded_control_bytes
                                } else {
                                    input
                                };
                                // Bounded G0/G1 SCS now consumes intermediate
                                // designators atomically; its grammar and bounds
                                // are checked independently in parser_conformance.
                                if bounded && !frost && input == b"\x1b(0q\x1b(Bz\x1b( % 5\x1bcX" {
                                    continue;
                                }
                                for split in 0..=input.len() {
                                    let np = if frost {
                                        ParserProfile::FrostLegacyV1
                                    } else {
                                        ParserProfile::EmberV1
                                    };
                                    let op = if frost {
                                        frozen_f243::ParserProfile::FrostLegacyV1
                                    } else {
                                        frozen_f243::ParserProfile::EmberV1
                                    };
                                    let nl = ParserLimits {
                                        pending_bytes: limit,
                                        kitty_recovery_bytes: 12,
                                    };
                                    let ol = frozen_f243::ParserLimits {
                                        pending_bytes: limit,
                                        kitty_recovery_bytes: 12,
                                    };
                                    let mut n = if bounded {
                                        Parser::with_bounded_profile(nl, np).unwrap()
                                    } else {
                                        Parser::with_profile(nl, np)
                                    };
                                    let mut o = if bounded {
                                        frozen_f243::Parser::with_bounded_profile(ol, op).unwrap()
                                    } else {
                                        frozen_f243::Parser::with_profile(ol, op)
                                    };
                                    n.set_ascii_batching(batching);
                                    o.set_ascii_batching(batching);
                                    if inline {
                                        n.enable_inline_graphics_packets();
                                        o.enable_inline_graphics_packets();
                                    }
                                    for chunk in
                                        [&input[..split], &input[split..], b"", b" A\xe0\x80Z", b""]
                                    {
                                        trace(&mut n, &mut o, chunk);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
