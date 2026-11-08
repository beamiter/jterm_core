// Original source SHA256: feab8952b2dda2876e7288673c9e30cb7b8f8f8a5ac8065c795e4b1f703fb990
// Frozen original Frost scanner, extracted verbatim from terminal.rs.
// Host fixture below records dispatch/capture; it is not used by production.
#![allow(dead_code)]
use jterm_engine::framing::control_string_abort;
use smallvec::SmallVec;
use std::collections::HashSet;
const MAX_PENDING_ESCAPE: usize = 128;
const OSC_BEL: &[u8] = b"\x07";
const OSC_ST: &[u8] = b"\x1b\\";
mod jterm_core {
    pub mod kitty_graphics {
        pub const MAX_CONTROL_BYTES: usize = 64;
        pub fn is_graphics_payload(payload: &[u8]) -> bool {
            payload.starts_with(b"G")
        }
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Charset {
    #[default]
    Ascii,
    Other,
}
#[derive(Debug, Default)]
pub struct Graphics {
    pub rejected: Vec<(Vec<u8>, String)>,
}
impl Graphics {
    pub fn reject_graphics_payload(&mut self, payload: &[u8], reason: &str) {
        // Mirror the host responder's bounded observation, not scanner policy.
        let rest = payload.strip_prefix(b"G").unwrap_or(payload);
        let control = &rest[..rest
            .len()
            .min(jterm_core::kitty_graphics::MAX_CONTROL_BYTES)];
        self.rejected.push((
            control.split(|b| *b == b';').next().unwrap().to_vec(),
            reason.to_owned(),
        ));
    }
    fn take_responses(&mut self) -> Vec<u8> {
        Vec::new()
    }
}
#[derive(Debug, Default)]
pub struct Reference {
    pub pending_escape: Vec<u8>,
    pub pending_apc: Vec<u8>,
    pub pending_osc: Vec<u8>,
    pub pending_dcs: Vec<u8>,
    pub pending_apc_scan_from: usize,
    pub pending_osc_scan_from: usize,
    pub pending_dcs_scan_from: usize,
    pub discarding_oversized_apc: bool,
    pub discarding_apc_prev_escape: bool,
    pub utf8_buf: [u8; 4],
    pub utf8_len: u8,
    pub utf8_expected: u8,
    pub active_charset: Charset,
    pub g0_charset: Charset,
    pub g1_charset: Charset,
    pub modes: HashSet<u16>,
    pub cursor_col: usize,
    pub pending_wrap: bool,
    pub tab_stops: Vec<bool>,
    pub kitty_graphics: Graphics,
    pub output_buffer: Vec<u8>,
    pub events: Vec<String>,
    pub raw: Vec<Vec<u8>>,
    pub hook_trace: Vec<usize>,
}
impl Reference {
    pub fn new() -> Self {
        Self {
            tab_stops: vec![false; 100],
            ..Self::default()
        }
    }
    pub fn process_input(&mut self, input: &[u8]) {
        self.process_input_inner(input);
    }
    pub fn put_char(&mut self, c: char, _: bool) {
        self.events.push(format!("P{c}"));
        self.cursor_col += 1;
    }
    pub fn put_ascii_run(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.put_char(b as char, true);
        }
    }
    pub fn index(&mut self) {
        self.events.push("LF".into());
    }
    pub fn next_line(&mut self) {
        self.cursor_col = 0;
        self.events.push("NEL".into());
    }
    pub fn reverse_index(&mut self) {
        self.events.push("RI".into());
    }
    pub fn next_tab_stop(&self, col: usize) -> usize {
        col / 8 * 8 + 8
    }
    pub fn save_cursor(&mut self) {
        self.events.push("SAVE".into());
    }
    pub fn restore_cursor(&mut self) {
        self.events.push("RESTORE".into());
    }
    // Record the exact semantic-event position of the scanner/graphics hook.
    // The outer feed hook is deliberately excluded: its repeated calls while
    // recursive resume frames unwind are idempotent and occur after all input.
    pub fn note_chat_native_takeover(&mut self) {
        self.hook_trace.push(self.events.len());
    }
    pub fn idle_background_capture_active(&self) -> bool {
        true
    }
    pub fn append_idle_background_bytes(&mut self, input: &[u8]) {
        if input.starts_with(b"\x1b")
            && matches!(input.get(1), Some(b']' | b'P' | b'X' | b'^' | b'_'))
        {
            return;
        }
        if !input.is_empty() {
            self.raw.push(input.to_vec());
        }
    }
    pub fn handle_osc_payload(&mut self, bytes: &[u8], term: &[u8]) {
        self.events.push(format!("OSC:{bytes:?}:{term:?}"));
    }
    pub fn handle_kitty_graphics_apc(&mut self, bytes: &[u8]) {
        self.events.push(format!("APC:{bytes:?}"));
        // Original production handle_kitty_graphics_apc also calls this hook,
        // independently of the scanner's direct-ESC completion hook.
        self.note_chat_native_takeover();
    }
    pub fn handle_nf_escape(&mut self, inter: &[u8], final_byte: u8) {
        self.events.push(format!("ESC:{inter:?}:{final_byte}"));
        if inter == b"(" || inter == b")" {
            let c = if final_byte == b'0' {
                Charset::Other
            } else {
                Charset::Ascii
            };
            if inter == b"(" {
                self.g0_charset = c;
                self.active_charset = c;
            } else {
                self.g1_charset = c;
            }
        }
    }
    pub fn handle_escape_sequence(
        &mut self,
        params: &[u16],
        raw: &[u8],
        cmd: char,
        private: Option<u8>,
        inter: &[u8],
    ) {
        self.events.push(format!(
            "CSI:{params:?}:{raw:?}:{cmd}:{private:?}:{inter:?}"
        ));
    }
    pub fn full_reset(&mut self) {
        self.events.push("RESET".into());
        self.pending_apc.clear();
        self.pending_osc.clear();
        self.pending_dcs.clear();
        self.pending_apc_scan_from = 0;
        self.pending_osc_scan_from = 0;
        self.pending_dcs_scan_from = 0;
        self.discarding_oversized_apc = false;
        self.discarding_apc_prev_escape = false;
    }
    pub fn parse_csi_params(param_bytes: &[u8]) -> SmallVec<[u16; 8]> {
        let mut params = SmallVec::new();
        let mut current: u16 = 0;
        let mut has_digits = false;

        for &byte in param_bytes {
            match byte {
                b'0'..=b'9' => {
                    current = current
                        .saturating_mul(10)
                        .saturating_add((byte - b'0') as u16);
                    has_digits = true;
                }
                b';' | b':' => {
                    if has_digits {
                        params.push(current);
                    }
                    current = 0;
                    has_digits = false;
                }
                _ => {}
            }
        }

        if has_digits {
            params.push(current);
        }

        params
    }
    fn abort_pending_control_string(&mut self, input: &[u8]) -> bool {
        let (previous_escape, osc) = if self.discarding_oversized_apc {
            (self.discarding_apc_prev_escape, false)
        } else if !self.pending_apc.is_empty() {
            (self.pending_apc.last() == Some(&0x1b), false)
        } else if !self.pending_osc.is_empty() {
            (self.pending_osc.last() == Some(&0x1b), true)
        } else if !self.pending_dcs.is_empty() {
            (self.pending_dcs.last() == Some(&0x1b), false)
        } else {
            return false;
        };
        let Some((resume, restore_escape)) = control_string_abort(input, previous_escape, osc)
        else {
            return false;
        };
        if !self.pending_apc.is_empty() {
            self.reject_buffered_kitty_apc_with_suffix(
                &input[..resume],
                "Kitty graphics APC was cancelled",
            );
        }
        self.pending_apc.clear();
        self.pending_apc_scan_from = 0;
        self.pending_osc.clear();
        self.pending_osc_scan_from = 0;
        self.pending_dcs.clear();
        self.pending_dcs_scan_from = 0;
        self.discarding_oversized_apc = false;
        self.discarding_apc_prev_escape = false;
        if restore_escape {
            self.pending_escape.push(0x1b);
        }
        self.process_input(&input[resume..]);
        true
    }
    fn begin_pending_apc(&mut self, tail: &[u8]) {
        if tail.len() > MAX_PENDING_ESCAPE {
            if let Some(payload) = tail.strip_prefix(b"\x1b_") {
                if jterm_core::kitty_graphics::is_graphics_payload(payload) {
                    self.kitty_graphics.reject_graphics_payload(
                        payload,
                        "Kitty graphics APC exceeded the parser size limit",
                    );
                    let responses = self.kitty_graphics.take_responses();
                    if !responses.is_empty() {
                        self.output_buffer.extend_from_slice(&responses);
                    }
                }
            }
            self.pending_apc.clear();
            self.pending_apc_scan_from = 0;
            self.discarding_oversized_apc = true;
            self.discarding_apc_prev_escape = tail.last() == Some(&0x1b);
            return;
        }
        self.pending_apc.clear();
        self.pending_apc.extend_from_slice(tail);
        self.pending_apc_scan_from = self.pending_apc.len().saturating_sub(1);
    }
    fn reject_buffered_kitty_apc_with_suffix(&mut self, suffix: &[u8], error: &str) {
        let Some(payload) = self.pending_apc.strip_prefix(b"\x1b_") else {
            return;
        };
        if !jterm_core::kitty_graphics::is_graphics_payload(payload) {
            return;
        }

        let limit = jterm_core::kitty_graphics::MAX_CONTROL_BYTES;
        let mut recovery =
            Vec::with_capacity(limit.min(payload.len().saturating_add(suffix.len())));
        recovery.extend_from_slice(&payload[..payload.len().min(limit)]);
        if recovery.len() < limit && !recovery.contains(&b';') {
            recovery.extend_from_slice(&suffix[..suffix.len().min(limit - recovery.len())]);
        }
        self.kitty_graphics
            .reject_graphics_payload(&recovery, error);
        let responses = self.kitty_graphics.take_responses();
        if !responses.is_empty() {
            self.output_buffer.extend_from_slice(&responses);
        }
    }
    fn resume_pending_apc(&mut self, input: &[u8]) -> bool {
        if self.discarding_oversized_apc {
            let mut previous_escape = self.discarding_apc_prev_escape;
            for (index, byte) in input.iter().copied().enumerate() {
                if previous_escape && byte == b'\\' {
                    self.discarding_oversized_apc = false;
                    self.discarding_apc_prev_escape = false;
                    if index + 1 < input.len() {
                        self.process_input(&input[index + 1..]);
                    }
                    return true;
                }
                previous_escape = byte == 0x1b;
            }
            self.discarding_apc_prev_escape = previous_escape;
            return true;
        }

        if self.pending_apc.is_empty() {
            return false;
        }

        // Everything before pending_apc_scan_from was proved not to contain
        // ST in the previous call. A new terminator can therefore only
        // straddle the old/new boundary or live entirely in input. Search the
        // new bytes once before doing the capacity check: bytes after ST are
        // normal terminal input and must not be charged to the APC size limit.
        let scan_from = self
            .pending_apc_scan_from
            .min(self.pending_apc.len().saturating_sub(1));
        let terminator = if scan_from + 1 == self.pending_apc.len()
            && self.pending_apc[scan_from] == 0x1b
            && input.first() == Some(&b'\\')
        {
            Some((scan_from, 1))
        } else {
            input
                .windows(2)
                .position(|window| window == b"\x1b\\")
                .map(|offset| (self.pending_apc.len() + offset, offset + 2))
        };

        if let Some((terminator, consumed)) = terminator {
            let packet_len = self.pending_apc.len().saturating_add(consumed);
            if packet_len > MAX_PENDING_ESCAPE {
                self.reject_buffered_kitty_apc_with_suffix(
                    &input[..consumed],
                    "Kitty graphics APC exceeded the parser size limit",
                );
                self.pending_apc.clear();
                self.pending_apc_scan_from = 0;
            } else {
                self.pending_apc.extend_from_slice(&input[..consumed]);
                let packet = std::mem::take(&mut self.pending_apc);
                self.pending_apc_scan_from = 0;
                if packet.starts_with(b"\x1b_")
                    && jterm_core::kitty_graphics::is_graphics_payload(&packet[2..terminator])
                {
                    self.handle_kitty_graphics_apc(&packet[2..terminator]);
                }
            }
            if consumed < input.len() {
                self.process_input(&input[consumed..]);
            }
            return true;
        }

        if self.pending_apc.len().saturating_add(input.len()) > MAX_PENDING_ESCAPE {
            let previous_escape = input
                .last()
                .copied()
                .or_else(|| self.pending_apc.last().copied())
                == Some(0x1b);
            self.reject_buffered_kitty_apc_with_suffix(
                input,
                "Kitty graphics APC exceeded the parser size limit",
            );
            self.pending_apc.clear();
            self.pending_apc_scan_from = 0;
            self.discarding_oversized_apc = true;
            self.discarding_apc_prev_escape = previous_escape;
            return true;
        }

        self.pending_apc.extend_from_slice(input);
        self.pending_apc_scan_from = self.pending_apc.len().saturating_sub(1);
        true
    }
    fn begin_pending_osc(&mut self, tail: &[u8]) {
        self.pending_osc.clear();
        self.pending_osc.extend_from_slice(tail);
        self.pending_osc_scan_from = self.pending_osc.len().saturating_sub(1);
    }
    fn resume_pending_osc(&mut self, input: &[u8]) -> bool {
        if self.pending_osc.is_empty() {
            return false;
        }

        // Mirror the old `pending_escape` entry check exactly: a buffered
        // prefix past the cap is abandoned wholesale and this read is parsed
        // as ordinary input (payload tail prints, BEL is a plain bell).
        if self.pending_osc.len() > MAX_PENDING_ESCAPE {
            self.pending_osc.clear();
            self.pending_osc_scan_from = 0;
            return false;
        }

        // Everything before pending_osc_scan_from was proved not to contain
        // BEL or ST in the previous call. A terminator can therefore only
        // straddle the old/new boundary (ESC at the stashed tail, `\` here)
        // or live entirely in input. Scan the new bytes once, before any
        // capacity concern: like the merged re-parse, a packet whose
        // terminator shows up is dispatched regardless of its size.
        let scan_from = self
            .pending_osc_scan_from
            .min(self.pending_osc.len().saturating_sub(1));
        let straddled_st = scan_from + 1 == self.pending_osc.len()
            && self.pending_osc[scan_from] == 0x1b
            && input.first() == Some(&b'\\');
        let terminator = if straddled_st {
            Some((scan_from, 1))
        } else {
            let bel = input.iter().position(|&byte| byte == 0x07);
            let st = input.windows(2).position(|window| window == b"\x1b\\");
            match (bel, st) {
                (Some(bel), Some(st)) if bel < st => Some((self.pending_osc.len() + bel, bel + 1)),
                (Some(_), Some(st)) => Some((self.pending_osc.len() + st, st + 2)),
                (Some(bel), None) => Some((self.pending_osc.len() + bel, bel + 1)),
                (None, Some(st)) => Some((self.pending_osc.len() + st, st + 2)),
                (None, None) => None,
            }
        };

        if let Some((payload_end, consumed)) = terminator {
            let capture_idle_background = self.idle_background_capture_active();
            self.pending_osc.extend_from_slice(&input[..consumed]);
            let packet = std::mem::take(&mut self.pending_osc);
            self.pending_osc_scan_from = 0;
            let terminator = if packet.last() == Some(&0x07) {
                OSC_BEL
            } else {
                OSC_ST
            };
            self.handle_osc_payload(&packet[2..payload_end], terminator);
            if capture_idle_background && self.idle_background_capture_active() {
                self.append_idle_background_bytes(&packet);
            }
            if consumed < input.len() {
                self.process_input(&input[consumed..]);
            }
            return true;
        }

        self.pending_osc.extend_from_slice(input);
        self.pending_osc_scan_from = self.pending_osc.len().saturating_sub(1);
        true
    }
    fn begin_pending_dcs(&mut self, tail: &[u8]) {
        self.pending_dcs.clear();
        self.pending_dcs.extend_from_slice(tail);
        self.pending_dcs_scan_from = self.pending_dcs.len().saturating_sub(1);
    }
    fn resume_pending_dcs(&mut self, input: &[u8]) -> bool {
        if self.pending_dcs.is_empty() {
            return false;
        }

        // Same overflow rule as the OSC path (old `pending_escape` entry
        // check): abandon the prefix wholesale, parse this read fresh.
        if self.pending_dcs.len() > MAX_PENDING_ESCAPE {
            self.pending_dcs.clear();
            self.pending_dcs_scan_from = 0;
            return false;
        }

        // See resume_pending_osc: only the stashed tail ESC can straddle the
        // boundary into an ST; every earlier byte was proved clean.
        let scan_from = self
            .pending_dcs_scan_from
            .min(self.pending_dcs.len().saturating_sub(1));
        let consumed = if scan_from + 1 == self.pending_dcs.len()
            && self.pending_dcs[scan_from] == 0x1b
            && input.first() == Some(&b'\\')
        {
            Some(1)
        } else {
            input
                .windows(2)
                .position(|window| window == b"\x1b\\")
                .map(|offset| offset + 2)
        };

        if let Some(consumed) = consumed {
            let capture_idle_background = self.idle_background_capture_active();
            self.pending_dcs.extend_from_slice(&input[..consumed]);
            let packet = std::mem::take(&mut self.pending_dcs);
            self.pending_dcs_scan_from = 0;
            if capture_idle_background && self.idle_background_capture_active() {
                self.append_idle_background_bytes(&packet);
            }
            if consumed < input.len() {
                self.process_input(&input[consumed..]);
            }
            return true;
        }

        self.pending_dcs.extend_from_slice(input);
        self.pending_dcs_scan_from = self.pending_dcs.len().saturating_sub(1);
        true
    }
    fn consume_utf8_lead(&mut self, byte: u8, expected: u8, data: &[u8], i: &mut usize) {
        let need = expected as usize;
        if *i + need > data.len() {
            // Incomplete: stash the lead byte and wait for the next batch.
            self.utf8_buf[0] = byte;
            self.utf8_len = 1;
            self.utf8_expected = expected;
            *i += 1;
            return;
        }

        let all_continuation = (1..need).all(|k| (data[*i + k] & 0xC0) == 0x80);
        if all_continuation {
            match std::str::from_utf8(&data[*i..*i + need]) {
                Ok(s) => {
                    if let Some(ch) = s.chars().next() {
                        self.put_char(ch, true);
                    }
                }
                Err(_) => self.put_char('\u{FFFD}', true),
            }
            *i += need;
        } else {
            // Lead byte directly followed by a non-continuation byte.
            self.put_char('\u{FFFD}', true);
            *i += 1;
        }
    }
    fn process_input_inner(&mut self, input: &[u8]) {
        if self.abort_pending_control_string(input) {
            return;
        }
        // A fragmented kitty APC streams against its own bounded buffer; this
        // must run before the pending_escape merge so the APC never pays the
        // O(n^2) re-scan and its tail is never parsed as ordinary input.
        if self.resume_pending_apc(input) {
            return;
        }
        // Fragmented OSC and DCS/SOS/PM strings stream the same way: at most
        // one string state can be in flight, so whichever buffer is non-empty
        // owns the input until its terminator (or the overflow abandon).
        if self.resume_pending_osc(input) {
            return;
        }
        if self.resume_pending_dcs(input) {
            return;
        }
        // Guard against an unterminated escape sequence. Such a sequence
        // is buffered into `pending_escape` and re-scanned from its start on every
        // read, which is both O(n^2) in CPU and unbounded in memory. Once the
        // buffered prefix exceeds this cap, abandon the partial sequence. The cap
        // is generous enough for legitimate large payloads (e.g. OSC 52 clipboard).
        // Only short CSI/charset prefixes still land in `pending_escape` — the
        // unbounded OSC/DCS/SOS/PM string states stream against their own
        // scan-cursor buffers above.
        if self.pending_escape.len() > MAX_PENDING_ESCAPE {
            self.pending_escape.clear();
        }

        // Fast path: if no pending escape, process input directly without allocation
        let data;
        let data_slice: &[u8] = if self.pending_escape.is_empty() {
            input
        } else {
            // Slow path: merge pending escape with new input
            let mut combined = std::mem::take(&mut self.pending_escape);
            combined.extend_from_slice(input);
            data = combined;
            &data
        };

        let mut i = 0;

        while i < data_slice.len() {
            let token_start = i;
            let capture_idle_background = self.idle_background_capture_active();
            let byte = data_slice[i];

            // A pending multi-byte UTF-8 sequence interrupted by a
            // non-continuation byte is malformed: emit the U+FFFD replacement
            // character per Unicode guidance and reset, then process the
            // current byte normally so the partial sequence is neither
            // silently dropped nor allowed to swallow later bytes.
            if self.utf8_len > 0 && (byte & 0xC0) != 0x80 {
                self.put_char('\u{FFFD}', true);
                self.utf8_len = 0;
            }

            match byte {
                b'\x08' => {
                    // Backspace (0x08) - just move cursor left.
                    // Shell handles actual deletion and sends back updated display.
                    self.pending_wrap = false;
                    if self.cursor_col > 0 {
                        self.cursor_col -= 1;
                    }
                    i += 1;
                }
                b'\x7f' => {
                    // DEL (0x7f) is a fill/padding character; xterm ignores it.
                    i += 1;
                }
                b'\n' | b'\x0b' | b'\x0c' => {
                    // Linefeed - move cursor down or scroll the region. VT and
                    // FF are executed as LF, as in xterm and VTE (and as the
                    // CSI parser below already does for them).
                    self.pending_wrap = false;
                    self.index();
                    i += 1;
                }
                b'\r' => {
                    self.pending_wrap = false;
                    self.cursor_col = 0;
                    i += 1;
                }
                b'\x0e' => {
                    self.active_charset = self.g1_charset;
                    i += 1;
                }
                b'\x0f' => {
                    self.active_charset = self.g0_charset;
                    i += 1;
                }
                b'\x07' => {
                    // Bell - ignore
                    i += 1;
                }
                b'\t' => {
                    // Tab - advance to the next tab stop.
                    self.pending_wrap = false;
                    self.cursor_col = self.next_tab_stop(self.cursor_col);
                    i += 1;
                }
                b'\x1b' => {
                    let esc_start = i;

                    if i + 1 >= data_slice.len() {
                        self.pending_escape
                            .extend_from_slice(&data_slice[esc_start..]);
                        break;
                    }

                    match data_slice[i + 1] {
                        // A repeated ESC abandons the unfinished prefix.
                        0x1b => {
                            i += 1;
                        }
                        b'7' => {
                            // DECSC - Save cursor (position + SGR + charset + origin)
                            self.save_cursor();
                            i += 2;
                        }
                        b'8' => {
                            // DECRC - Restore cursor
                            self.restore_cursor();
                            i += 2;
                        }
                        b'E' => {
                            // NEL - Next Line (linefeed + carriage return)
                            self.pending_wrap = false;
                            self.next_line();
                            i += 2;
                        }
                        b'D' => {
                            // IND - Index (linefeed without carriage return)
                            self.pending_wrap = false;
                            self.index();
                            i += 2;
                        }
                        b'M' => {
                            // RI - Reverse Index
                            self.pending_wrap = false;
                            self.reverse_index();
                            i += 2;
                        }
                        b'H' => {
                            // HTS - set a horizontal tab stop at the current column
                            if let Some(stop) = self.tab_stops.get_mut(self.cursor_col) {
                                *stop = true;
                            }
                            i += 2;
                        }
                        b'c' => {
                            // RIS - Reset to Initial State
                            self.full_reset();
                            i += 2;
                        }
                        b']' => {
                            i += 2;

                            let payload_start = i;
                            if let Some((resume, _)) =
                                control_string_abort(&data_slice[i..], false, true)
                            {
                                i += resume;
                                continue;
                            }

                            let mut terminated = false;
                            while i < data_slice.len() {
                                if data_slice[i] == 0x07 {
                                    i += 1;
                                    terminated = true;
                                    break;
                                } else if i + 1 < data_slice.len()
                                    && data_slice[i] == 0x1b
                                    && data_slice[i + 1] == 0x5c
                                {
                                    i += 2;
                                    terminated = true;
                                    break;
                                } else {
                                    i += 1;
                                }
                            }

                            if !terminated {
                                // Fragmented OSC: stream against its own
                                // bounded buffer instead of re-scanning
                                // pending_escape from byte 0 each read.
                                self.begin_pending_osc(&data_slice[esc_start..]);
                                break;
                            }

                            let (payload_end, terminator) = if data_slice[i - 1] == 0x07 {
                                (i - 1, OSC_BEL)
                            } else {
                                (i - 2, OSC_ST)
                            };
                            if payload_end >= payload_start {
                                self.handle_osc_payload(
                                    &data_slice[payload_start..payload_end],
                                    terminator,
                                );
                            }
                        }
                        b'P' | b'X' | b'^' | b'_' => {
                            // DCS (ESC P), SOS (ESC X), PM (ESC ^) and APC (ESC _)
                            // all end at ST, but only APC carries Kitty graphics.
                            // Sniffing every string for `a=` used to hand an
                            // unrelated DCS to the graphics state.
                            let is_apc = data_slice[i + 1] == b'_';
                            i += 2;

                            if let Some((resume, _)) =
                                control_string_abort(&data_slice[i..], false, false)
                            {
                                if is_apc {
                                    self.kitty_graphics.reject_graphics_payload(
                                        &data_slice[i..i + resume],
                                        "Kitty graphics APC was cancelled",
                                    );
                                    let responses = self.kitty_graphics.take_responses();
                                    self.output_buffer.extend_from_slice(&responses);
                                }
                                i += resume;
                                continue;
                            }
                            let mut terminated = false;
                            let dcs_start = i;
                            while i < data_slice.len() {
                                if i + 1 < data_slice.len()
                                    && data_slice[i] == 0x1b
                                    && data_slice[i + 1] == 0x5c
                                {
                                    let payload = &data_slice[dcs_start..i];

                                    if is_apc
                                        && jterm_core::kitty_graphics::is_graphics_payload(payload)
                                    {
                                        self.handle_kitty_graphics_apc(payload);
                                    }

                                    i += 2;
                                    terminated = true;
                                    break;
                                }
                                i += 1;
                            }

                            if !terminated {
                                if is_apc {
                                    // Kitty transfers stream against their
                                    // own bounded buffer instead of the
                                    // re-scanning pending_escape.
                                    self.begin_pending_apc(&data_slice[esc_start..]);
                                } else {
                                    // DCS/SOS/PM stream the same way; their
                                    // payload stays dropped on completion.
                                    self.begin_pending_dcs(&data_slice[esc_start..]);
                                }
                                break;
                            }
                        }
                        b'>' => {
                            // ESC > - DECKPNM (Keypad Numeric Mode)
                            self.modes.remove(&66);
                            i += 2;
                        }
                        b'<' => {
                            // ESC < - DECKPM (Keypad Application Mode) or other private sequence
                            // Just skip it
                            i += 2;
                        }
                        b'=' => {
                            // ESC = - DECKPAM (Keypad Application Mode)
                            self.modes.insert(66);
                            i += 2;
                        }
                        0x20..=0x2f => {
                            // nF escape: ESC, one or more intermediates
                            // (0x20..=0x2f), then one final byte. The whole
                            // sequence is consumed even when frost does not
                            // implement it; stopping after two bytes printed
                            // the final as text (`ESC % G` left a "G").
                            let mut end = i + 1;
                            while end < data_slice.len() && (0x20..=0x2f).contains(&data_slice[end])
                            {
                                end += 1;
                            }
                            if end >= data_slice.len() {
                                self.pending_escape
                                    .extend_from_slice(&data_slice[esc_start..]);
                                break;
                            }
                            let final_byte = data_slice[end];
                            if (0x30..=0x7e).contains(&final_byte) {
                                self.handle_nf_escape(&data_slice[i + 1..end], final_byte);
                                i = end + 1;
                            } else {
                                // A control or 8-bit byte aborts the sequence;
                                // leave it for the outer loop to execute.
                                i = end;
                            }
                        }
                        b'[' => {
                            i += 2;

                            // Use stack arrays for CSI params (typical CSI sequences are short)
                            let mut param_bytes = [0u8; 128];
                            let mut param_len = 0;
                            let mut intermediates = [0u8; 8];
                            let mut inter_len = 0;
                            let mut final_byte = None;
                            let mut cancelled = false;

                            while i < data_slice.len() {
                                match data_slice[i] {
                                    0x30..=0x3f => {
                                        if param_len < param_bytes.len() {
                                            param_bytes[param_len] = data_slice[i];
                                            param_len += 1;
                                        }
                                    }
                                    0x20..=0x2f => {
                                        if inter_len < intermediates.len() {
                                            intermediates[inter_len] = data_slice[i];
                                            inter_len += 1;
                                        }
                                    }
                                    0x40..=0x7e => {
                                        final_byte = Some(data_slice[i]);
                                        break;
                                    }
                                    // ECMA-48 allows C0 controls inside a control
                                    // sequence. Pagers such as util-linux `more`
                                    // can insert CR/LF while wrapping colored text,
                                    // even between the digits of an SGR parameter.
                                    // Execute those controls immediately and keep
                                    // collecting the surrounding CSI sequence.
                                    b'\x08' => {
                                        self.pending_wrap = false;
                                        self.cursor_col = self.cursor_col.saturating_sub(1);
                                    }
                                    b'\t' => {
                                        self.pending_wrap = false;
                                        self.cursor_col = self.next_tab_stop(self.cursor_col);
                                    }
                                    b'\n' | b'\x0b' | b'\x0c' => {
                                        self.pending_wrap = false;
                                        self.index();
                                    }
                                    b'\r' => {
                                        self.pending_wrap = false;
                                        self.cursor_col = 0;
                                    }
                                    b'\x0e' => self.active_charset = self.g1_charset,
                                    b'\x0f' => self.active_charset = self.g0_charset,
                                    // CAN and SUB cancel the current control
                                    // sequence. Leave a following ESC untouched so
                                    // the outer parser can begin the replacement.
                                    b'\x18' | b'\x1a' => {
                                        cancelled = true;
                                        i += 1;
                                        break;
                                    }
                                    b'\x1b' => {
                                        cancelled = true;
                                        break;
                                    }
                                    // Other C0 controls are either padding or have
                                    // no visible effect in Frost, but they do not
                                    // make the CSI incomplete.
                                    0x00..=0x1f => {}
                                    _ => break,
                                }
                                i += 1;
                            }

                            if cancelled {
                                continue;
                            }

                            let Some(final_byte) = final_byte else {
                                // C0 controls embedded in the partial CSI were
                                // already executed above. Buffer only the CSI
                                // syntax so a later PTY read does not replay a
                                // linefeed or carriage return.
                                self.pending_escape.extend_from_slice(b"\x1b[");
                                self.pending_escape
                                    .extend_from_slice(&param_bytes[..param_len]);
                                self.pending_escape
                                    .extend_from_slice(&intermediates[..inter_len]);
                                break;
                            };

                            let private_prefix = match param_bytes.first().copied() {
                                Some(prefix @ (b'<' | b'=' | b'>' | b'?')) => {
                                    // Shift remaining params left
                                    for j in 0..param_len - 1 {
                                        param_bytes[j] = param_bytes[j + 1];
                                    }
                                    param_len -= 1;
                                    Some(prefix)
                                }
                                _ => None,
                            };
                            let params = Self::parse_csi_params(&param_bytes[..param_len]);
                            let cmd = final_byte as char;

                            self.handle_escape_sequence(
                                &params,
                                &param_bytes[..param_len],
                                cmd,
                                private_prefix,
                                &intermediates[..inter_len],
                            );
                            i += 1;
                        }
                        _ => {
                            // Unknown 2-byte escape (e.g. SS2 `ESC N`, SS3 `ESC O`).
                            // Consume BOTH bytes so the trailing letter isn't printed
                            // as literal text.
                            i += 2;
                        }
                    }
                }
                32..=126 => {
                    // ASCII fast path: scan for run of printable ASCII and process in bulk.
                    // Insert mode (IRM) needs per-cell shifting, so fall back to put_char.
                    if self.utf8_len == 0
                        && self.active_charset == Charset::Ascii
                        && !self.modes.contains(&4)
                    {
                        let run_start = i;
                        i += 1;
                        while i < data_slice.len() {
                            let b = data_slice[i];
                            if !(32..=126).contains(&b) {
                                break;
                            }
                            i += 1;
                        }
                        self.put_ascii_run(&data_slice[run_start..i]);
                    } else {
                        self.put_char(byte as char, true);
                        i += 1;
                    }
                }
                // UTF-8 multi-byte sequences: try to consume all bytes eagerly
                0xC2..=0xDF => {
                    self.consume_utf8_lead(byte, 2, data_slice, &mut i);
                }
                0xE0..=0xEF => {
                    self.consume_utf8_lead(byte, 3, data_slice, &mut i);
                }
                0xF0..=0xF4 => {
                    self.consume_utf8_lead(byte, 4, data_slice, &mut i);
                }
                _ => {
                    // Reaching here, byte is either a continuation byte (the
                    // abandoned-sequence check above guarantees a sequence is
                    // really pending) or an invalid UTF-8 lead byte
                    // (0xC0/0xC1/0xF5..=0xFF), or an unhandled C0 control.
                    if self.utf8_len > 0 && (byte & 0xC0) == 0x80 {
                        self.utf8_buf[self.utf8_len as usize] = byte;
                        self.utf8_len += 1;
                        if self.utf8_len == self.utf8_expected {
                            match std::str::from_utf8(&self.utf8_buf[..self.utf8_len as usize]) {
                                Ok(s) => {
                                    if let Some(ch) = s.chars().next() {
                                        self.put_char(ch, true);
                                    }
                                }
                                // Full length but invalid content (overlong
                                // encoding / surrogate): emit U+FFFD.
                                Err(_) => self.put_char('\u{FFFD}', true),
                            }
                            self.utf8_len = 0;
                        }
                    } else {
                        // Orphan continuation bytes and invalid lead bytes are
                        // malformed UTF-8 and get U+FFFD; unhandled C0
                        // controls stay ignored.
                        if byte >= 0x80 {
                            self.put_char('\u{FFFD}', true);
                        }
                        self.utf8_len = 0;
                    }
                    i += 1;
                }
            }
            if byte == 0x1b {
                self.note_chat_native_takeover();
            }
            if capture_idle_background && self.idle_background_capture_active() && i > token_start {
                self.append_idle_background_bytes(&data_slice[token_start..i]);
            }
        }
    }
}
