//! Incremental byte framing extracted from Ember. Semantic dispatch stays in the host.
//! Events are delivered one at a time, before any following input is interpreted.
use std::{borrow::Cow, ops::Range};

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
        params: Vec<u16>,
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
            State::Csi { .. } => PendingKind::Csi,
            State::String { kind, .. } => PendingKind::String(kind),
            State::DiscardApc { .. } => PendingKind::DiscardApc,
            State::DiscardString { kind, .. } => PendingKind::String(kind),
            State::DiscardEscape => PendingKind::Escape,
        }
    }
    fn open_capture(&mut self, state: &State, at: usize) -> bool {
        if !self.frost() || self.capture.is_some() {
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
                prefix.push(0x1b);
                prefix.extend(bytes);
            }
            State::Csi {
                params,
                intermediates,
            } => {
                prefix.extend(b"\x1b[");
                prefix.extend(params);
                prefix.extend(intermediates);
            }
            State::Charset(bank) => prefix.extend([0x1b, *bank]),
            State::String { .. }
            | State::DiscardApc { .. }
            | State::DiscardString { .. }
            | State::DiscardEscape => private = true,
            State::Ground => {}
        }
        self.capture = Some(OpenCapture {
            prefix,
            start: self.feed_offset + at,
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
            State::DiscardString { .. } | State::DiscardEscape => {}
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
            let state = std::mem::replace(&mut self.state, State::Ground);
            if at < input.len() || !matches!(state, State::Ground) {
                started |= self.open_capture(&state, at);
            }
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
                State::DiscardEscape => {
                    let mut ended = false;
                    while let Some(&byte) = input.get(at) {
                        at += 1;
                        if matches!(byte, 0x30..=0x7e | 0x18 | 0x1a) {
                            ended = true;
                            break;
                        }
                    }
                    if !ended {
                        self.state = State::DiscardEscape;
                        abandon!(None);
                    }
                    abandon!(Some(Event::Boundary));
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
                        let scan_len = remaining.len().min(self.limits.pending_bytes + 1);
                        if let Some(end) = remaining[..scan_len].iter().position(|&byte| {
                            matches!(byte, 0x1b | 0x18 | 0x1a)
                                || (kind == StringKind::Osc && byte == 7)
                        }) {
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
                    if byte != 0x1b && data.len() >= self.limits.pending_bytes {
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
                    if let Some((used, restore)) = crate::framing::control_string_abort(
                        remaining,
                        previous_escape,
                        kind == StringKind::Osc,
                    ) {
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
                    let terminator = if previous_escape && remaining.first() == Some(&b'\\') {
                        Some((0, 1, false, true))
                    } else {
                        let mut found = None;
                        for (i, byte) in remaining.iter().enumerate() {
                            if kind == StringKind::Osc && *byte == 7 {
                                found = Some((i, i + 1, true, false));
                                break;
                            }
                            if *byte == 0x1b && remaining.get(i + 1) == Some(&b'\\') {
                                found = Some((i, i + 2, false, false));
                                break;
                            }
                        }
                        found
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
                            self.state = State::Escape(Vec::new());
                            if let Some(capture) = self.capture.as_mut() {
                                capture.escape = true;
                            }
                        }
                        32..=126 => {
                            let start = at - 1;
                            while self.ascii_batching
                                && input.get(at).is_some_and(|b| (32..=126).contains(b))
                            {
                                at += 1;
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
                }
                State::Charset(bank) => {
                    let Some(&designator) = input.get(at) else {
                        self.state = State::Charset(bank);
                        abandon!(None);
                    };
                    at += 1;
                    emit!(Event::Charset { bank, designator });
                }
                State::Escape(mut intermediates) => {
                    let Some(&byte) = input.get(at) else {
                        self.state = State::Escape(intermediates);
                        abandon!(None);
                    };
                    if intermediates.is_empty() {
                        match byte {
                            b'[' => {
                                at += 1;
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
                            let raw_params =
                                params[usize::from(private_prefix.is_some())..].to_vec();
                            let (params, colon_flags) = parse_csi_params(&raw_params);
                            emit!(Event::Csi {
                                raw_params,
                                params,
                                colon_flags,
                                private_prefix,
                                intermediates,
                                final_byte: byte
                            });
                        }
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
                        0..=31 => {}
                        _ => {
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

pub fn parse_csi_params(bytes: &[u8]) -> (Vec<u16>, Vec<bool>) {
    if bytes.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let mut params = Vec::new();
    let mut flags = Vec::new();
    let mut current: u16 = 0;
    let mut colon = false;
    for &byte in bytes {
        match byte {
            b'0'..=b'9' => {
                current = current
                    .saturating_mul(10)
                    .saturating_add((byte - b'0') as u16)
            }
            b';' | b':' => {
                params.push(current);
                flags.push(colon);
                current = 0;
                colon = byte == b':';
            }
            _ => {}
        }
    }
    params.push(current);
    flags.push(colon);
    (params, flags)
}
