//! Byte-level escape-sequence parser for [`super::ScreenReplay`].
//!
//! The state machine follows the DEC/ECMA-48 parser that VTE's own parser is
//! built on (the vt100.net diagram): bytes are UTF-8 decoded first, then every
//! code point is classified, so C1 controls arrive as U+0080..U+009F exactly
//! like VTE sees them. It is incremental — a sequence or a UTF-8 character
//! split across two `feed` calls resumes where it stopped — and it never
//! prints the payload of a sequence it does not understand: unknown CSI/ESC
//! finals are dispatched and ignored by the emulator, DCS/SOS/PM/APC strings
//! are swallowed up to their terminator, and only OSC strings are collected
//! (bounded) so OSC 8 hyperlinks can be carried per cell.

/// VTE keeps at most 32 parameters per control sequence; the rest are dropped.
pub(super) const MAX_PARAMS: usize = 32;

/// Parameter values are clamped like VTE's parser (`0xffff`).
const MAX_PARAM_VALUE: i32 = 0xffff;

/// Bound on a collected OSC payload. An OSC 8 hyperlink is at most
/// `8;` + a 250-byte id parameter + a 2083-byte URI (VTE's limits), so this
/// keeps every link VTE would accept while a runaway string cannot grow the
/// buffer without limit. Longer strings are consumed and dropped.
const MAX_OSC_BYTES: usize = 4096;

/// What the parser hands to the emulator. Kept as a trait (monomorphised, not
/// a trait object) so the printable-ASCII fast path costs no dynamic dispatch.
pub(super) trait Perform {
    /// A run of printable ASCII (0x20..=0x7e) in the ground state.
    fn print_ascii(&mut self, run: &[u8]);
    /// Any other printable character (already UTF-8 decoded).
    fn print(&mut self, c: char);
    /// A C0 control (< 0x20) or a C1 control (0x80..=0x9f) that is not a
    /// sequence introducer.
    fn execute(&mut self, control: u32);
    /// A complete CSI sequence. `prefix` is the private marker (`<`, `=`,
    /// `>`, `?`) or 0; `intermediate` is the single intermediate byte, 0 for
    /// none, or [`MULTIPLE_INTERMEDIATES`].
    fn csi(&mut self, params: &Params, prefix: u8, intermediate: u8, fin: u8);
    /// A complete escape sequence (`ESC [intermediate] final`).
    fn esc(&mut self, intermediate: u8, fin: u8);
    /// A complete OSC string (payload without the introducer/terminator).
    fn osc(&mut self, payload: &[u8]);
}

/// Stand-in intermediate when a sequence carried more than one; no sequence
/// the emulator implements has two, so it simply never matches.
pub(super) const MULTIPLE_INTERMEDIATES: u8 = 0xff;

/// CSI parameters in VTE's shape: each slot is a value or `-1` for "omitted",
/// and a `:` separator links a parameter to the next one (a sub-parameter
/// group such as `38:2::10:20:30`).
#[derive(Clone)]
pub(super) struct Params {
    values: [i32; MAX_PARAMS],
    /// Bit `i` set: parameter `i` is followed by `:` (it is non-final in its
    /// group).
    nonfinal: u32,
    len: usize,
    /// The parameter being accumulated, `-1` while it has no digits yet.
    current: i32,
    /// Whether anything (a digit or a separator) was seen, so `CSI m` has zero
    /// parameters while `CSI ;m` has two omitted ones.
    started: bool,
}

impl Params {
    const fn new() -> Self {
        Self {
            values: [-1; MAX_PARAMS],
            nonfinal: 0,
            len: 0,
            current: -1,
            started: false,
        }
    }

    fn clear(&mut self) {
        self.nonfinal = 0;
        self.len = 0;
        self.current = -1;
        self.started = false;
    }

    fn push_digit(&mut self, digit: u8) {
        self.started = true;
        let value = if self.current < 0 { 0 } else { self.current };
        self.current = (value * 10 + i32::from(digit)).min(MAX_PARAM_VALUE);
    }

    fn finish_param(&mut self, colon: bool) {
        self.started = true;
        if self.len < MAX_PARAMS {
            self.values[self.len] = self.current;
            if colon {
                self.nonfinal |= 1 << self.len;
            }
            self.len += 1;
        }
        self.current = -1;
    }

    /// Closes the trailing parameter at the final byte.
    fn seal(&mut self) {
        if self.started {
            self.finish_param(false);
        }
    }

    /// Number of parameters, counting omitted ones (VTE's `seq.size()`).
    pub(super) fn len(&self) -> usize {
        self.len
    }

    /// Raw value: `-1` when omitted or out of range (VTE's `seq.param(i)`).
    pub(super) fn raw(&self, idx: usize) -> i32 {
        if idx < self.len {
            self.values[idx]
        } else {
            -1
        }
    }

    /// Value or `default` when omitted (VTE's `seq.param(i, default)`).
    pub(super) fn get(&self, idx: usize, default: i32) -> i32 {
        match self.raw(idx) {
            -1 => default,
            value => value,
        }
    }

    /// Whether parameter `idx` is followed by a `:` sub-parameter.
    pub(super) fn is_nonfinal(&self, idx: usize) -> bool {
        idx < self.len && self.nonfinal & (1 << idx) != 0
    }

    /// Index of the first parameter after the group `idx` belongs to (VTE's
    /// `seq.next(i)`).
    pub(super) fn next(&self, mut idx: usize) -> usize {
        while self.is_nonfinal(idx) {
            idx += 1;
        }
        idx + 1
    }

    /// VTE's `collect1(idx, default, min, max)`: omitted → `default`, then
    /// clamped. Explicit zeroes are NOT treated as omitted, which is why
    /// `CSI 0;0r` is ignored while `CSI ;r` resets the region. Like VTE this
    /// is `max(min(v, max), min)` rather than a `clamp`, because callers pass
    /// ranges where `max < min` (REP at the pending-wrap column), and then the
    /// minimum wins.
    pub(super) fn collect1(&self, idx: usize, default: i32, min: i32, max: i32) -> i32 {
        self.get(idx, default).min(max).max(min)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Ground,
    Escape,
    EscapeIntermediate,
    CsiEntry,
    CsiParam,
    CsiIntermediate,
    CsiIgnore,
    Osc,
    /// DCS, SOS, PM and APC: consumed up to ST and discarded.
    IgnoredString,
    /// An ESC inside a string: `\` completes ST, anything else aborts the
    /// string and starts a new escape sequence.
    StringEscape {
        osc: bool,
    },
}

/// Incremental UTF-8 decoder with "maximal subpart" error handling: every
/// ill-formed subsequence becomes one U+FFFD, and a byte that interrupts a
/// multi-byte sequence is decoded again on its own.
#[derive(Clone, Copy, Default)]
struct Utf8 {
    code: u32,
    /// Continuation bytes still expected.
    need: u8,
    /// Allowed range for the next continuation byte (narrower than
    /// 0x80..=0xbf right after E0/ED/F0/F4, which is what rejects overlongs
    /// and surrogates).
    lower: u8,
    upper: u8,
}

enum Utf8Step {
    Pending,
    Char(u32),
    /// Emit U+FFFD; `retry` means the current byte must be decoded again.
    Invalid {
        retry: bool,
    },
}

impl Utf8 {
    fn idle(&self) -> bool {
        self.need == 0
    }

    fn push(&mut self, byte: u8) -> Utf8Step {
        if self.need == 0 {
            let (need, lower, upper, code) = match byte {
                0x00..=0x7f => return Utf8Step::Char(u32::from(byte)),
                0xc2..=0xdf => (1, 0x80, 0xbf, u32::from(byte & 0x1f)),
                0xe0 => (2, 0xa0, 0xbf, u32::from(byte & 0x0f)),
                0xe1..=0xec | 0xee..=0xef => (2, 0x80, 0xbf, u32::from(byte & 0x0f)),
                0xed => (2, 0x80, 0x9f, u32::from(byte & 0x0f)),
                0xf0 => (3, 0x90, 0xbf, u32::from(byte & 0x07)),
                0xf1..=0xf3 => (3, 0x80, 0xbf, u32::from(byte & 0x07)),
                0xf4 => (3, 0x80, 0x8f, u32::from(byte & 0x07)),
                _ => return Utf8Step::Invalid { retry: false },
            };
            *self = Self {
                code,
                need,
                lower,
                upper,
            };
            return Utf8Step::Pending;
        }
        if byte < self.lower || byte > self.upper {
            *self = Self::default();
            return Utf8Step::Invalid { retry: true };
        }
        self.code = (self.code << 6) | u32::from(byte & 0x3f);
        self.need -= 1;
        self.lower = 0x80;
        self.upper = 0xbf;
        if self.need == 0 {
            let code = self.code;
            *self = Self::default();
            Utf8Step::Char(code)
        } else {
            Utf8Step::Pending
        }
    }
}

pub(super) struct Parser {
    state: State,
    utf8: Utf8,
    params: Params,
    prefix: u8,
    intermediate: u8,
    osc: Vec<u8>,
    osc_overflow: bool,
}

impl Parser {
    pub(super) fn new() -> Self {
        Self {
            state: State::Ground,
            utf8: Utf8::default(),
            params: Params::new(),
            prefix: 0,
            intermediate: 0,
            osc: Vec::new(),
            osc_overflow: false,
        }
    }

    pub(super) fn advance<P: Perform>(&mut self, performer: &mut P, bytes: &[u8]) {
        let mut i = 0;
        while i < bytes.len() {
            if self.state == State::Ground && self.utf8.idle() {
                // The hot path: plain text is by far the bulk of any capture,
                // so hand whole printable-ASCII runs over at once.
                let start = i;
                while i < bytes.len() && (0x20..0x7f).contains(&bytes[i]) {
                    i += 1;
                }
                if i > start {
                    performer.print_ascii(&bytes[start..i]);
                    continue;
                }
            }
            let byte = bytes[i];
            i += 1;
            match self.utf8.push(byte) {
                Utf8Step::Pending => {}
                Utf8Step::Char(code) => self.code_point(performer, code),
                Utf8Step::Invalid { retry } => {
                    if retry {
                        i -= 1;
                    }
                    self.code_point(performer, 0xfffd);
                }
            }
        }
    }

    fn clear_sequence(&mut self) {
        self.params.clear();
        self.prefix = 0;
        self.intermediate = 0;
    }

    fn collect_intermediate(&mut self, byte: u8) {
        self.intermediate = if self.intermediate == 0 {
            byte
        } else {
            MULTIPLE_INTERMEDIATES
        };
    }

    fn start_string(&mut self, osc: bool) {
        if osc {
            self.osc.clear();
            self.osc_overflow = false;
            self.state = State::Osc;
        } else {
            self.state = State::IgnoredString;
        }
    }

    fn finish_string<P: Perform>(&mut self, performer: &mut P, osc: bool) {
        if osc && !self.osc_overflow {
            performer.osc(&self.osc);
        }
        self.osc.clear();
        self.state = State::Ground;
    }

    fn code_point<P: Perform>(&mut self, performer: &mut P, code: u32) {
        // Transitions that apply in every state.
        match code {
            0x18 | 0x1a => {
                // CAN and SUB abort any sequence; VTE's SUB also prints U+FFFD.
                self.osc.clear();
                self.state = State::Ground;
                if code == 0x1a {
                    performer.print('\u{fffd}');
                }
                return;
            }
            0x1b => {
                self.state = match self.state {
                    State::Osc => State::StringEscape { osc: true },
                    State::IgnoredString => State::StringEscape { osc: false },
                    _ => {
                        self.clear_sequence();
                        State::Escape
                    }
                };
                return;
            }
            0x9c if matches!(self.state, State::Osc | State::IgnoredString) => {
                let osc = self.state == State::Osc;
                self.finish_string(performer, osc);
                return;
            }
            0x80..=0x9f => {
                // C1 controls: an 8-bit introducer starts its sequence; any
                // other C1 aborts what was in progress and executes.
                self.osc.clear();
                self.clear_sequence();
                self.state = State::Ground;
                match code {
                    0x90 | 0x98 | 0x9e | 0x9f => self.start_string(false),
                    0x9b => self.state = State::CsiEntry,
                    0x9d => self.start_string(true),
                    0x9c => {}
                    _ => performer.execute(code),
                }
                return;
            }
            _ => {}
        }

        match self.state {
            State::Ground => match code {
                0x00..=0x1f => performer.execute(code),
                0x7f => {}
                _ => performer.print(char::from_u32(code).unwrap_or('\u{fffd}')),
            },
            State::Escape => match code {
                0x00..=0x1f => performer.execute(code),
                0x20..=0x2f => {
                    self.collect_intermediate(code as u8);
                    self.state = State::EscapeIntermediate;
                }
                0x5b => {
                    self.clear_sequence();
                    self.state = State::CsiEntry;
                }
                0x5d => self.start_string(true),
                0x50 | 0x58 | 0x5e | 0x5f => self.start_string(false),
                0x30..=0x7e => {
                    self.state = State::Ground;
                    performer.esc(0, code as u8);
                }
                0x7f => {}
                _ => self.state = State::Ground,
            },
            State::EscapeIntermediate => match code {
                0x00..=0x1f => performer.execute(code),
                0x20..=0x2f => self.collect_intermediate(code as u8),
                0x30..=0x7e => {
                    self.state = State::Ground;
                    performer.esc(self.intermediate, code as u8);
                }
                0x7f => {}
                _ => self.state = State::Ground,
            },
            State::CsiEntry | State::CsiParam => match code {
                0x00..=0x1f => performer.execute(code),
                0x30..=0x39 => {
                    self.params.push_digit((code - 0x30) as u8);
                    self.state = State::CsiParam;
                }
                0x3a => {
                    self.params.finish_param(true);
                    self.state = State::CsiParam;
                }
                0x3b => {
                    self.params.finish_param(false);
                    self.state = State::CsiParam;
                }
                0x3c..=0x3f => {
                    if self.state == State::CsiEntry {
                        self.prefix = code as u8;
                        self.state = State::CsiParam;
                    } else {
                        self.state = State::CsiIgnore;
                    }
                }
                0x20..=0x2f => {
                    self.params.seal();
                    self.collect_intermediate(code as u8);
                    self.state = State::CsiIntermediate;
                }
                0x40..=0x7e => {
                    self.params.seal();
                    self.state = State::Ground;
                    performer.csi(&self.params, self.prefix, self.intermediate, code as u8);
                }
                0x7f => {}
                _ => self.state = State::CsiIgnore,
            },
            State::CsiIntermediate => match code {
                0x00..=0x1f => performer.execute(code),
                0x20..=0x2f => self.collect_intermediate(code as u8),
                0x40..=0x7e => {
                    self.state = State::Ground;
                    performer.csi(&self.params, self.prefix, self.intermediate, code as u8);
                }
                0x7f => {}
                _ => self.state = State::CsiIgnore,
            },
            State::CsiIgnore => match code {
                0x00..=0x1f => performer.execute(code),
                0x40..=0x7e => self.state = State::Ground,
                _ => {}
            },
            State::Osc => match code {
                0x07 => self.finish_string(performer, true),
                0x00..=0x1f => {}
                _ => {
                    if self.osc.len() + 4 > MAX_OSC_BYTES {
                        self.osc_overflow = true;
                    } else if let Some(c) = char::from_u32(code) {
                        let mut buf = [0; 4];
                        self.osc
                            .extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                    }
                }
            },
            State::IgnoredString => {}
            State::StringEscape { osc } => {
                if code == 0x5c {
                    self.finish_string(performer, osc);
                } else {
                    // Not ST: the string is abandoned and this ESC starts a new
                    // sequence with the current code point.
                    self.osc.clear();
                    self.clear_sequence();
                    self.state = State::Escape;
                    self.code_point(performer, code);
                }
            }
        }
    }
}
