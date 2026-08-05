//! Hardening for everything the frontends write *to* a shell PTY.
//!
//! Every jterm grew its own paste encoder, and they disagreed on every axis.
//! Only ember removed an embedded `ESC[201~` from the payload before framing
//! it; anvil, frost and forge pasted the clipboard verbatim, so a hostile
//! clipboard could close the bracketed-paste frame early and have the rest of
//! its bytes arrive as *keystrokes*:
//!
//! ```text
//! clipboard: "docs\x1b[201~\rrm -rf ~\r"
//!   framed:  ESC[200~ docs ESC[201~ CR rm -rf ~ CR ESC[201~
//!                          ^^^^^^^^ shell leaves paste mode here
//! ```
//!
//! The remainder is then a line the shell executes. This module is the shared
//! replacement for those four encoders, and the rule that fixes the injection
//! is unconditional: [`encode_paste`] and [`InputGuard`] **always** remove
//! paste-bracket markers from a payload body, whatever the policy flags say.
//!
//! Family decisions frozen here (do not relitigate per-app):
//!
//! - **This module does not own DECSET 2004.** [`PasteModes`] is a parameter.
//!   ember/frost read the mode from their own VT emulators, anvil/forge
//!   from a raw byte scan plus a `ParserEvent::DecsetMode` cell, and
//!   [`crate::parser`] carries a fourth (currently unused) copy. A fifth owner
//!   living here would make this module a net regression.
//! - **The submit CR goes *outside* the frame.** Readline deliberately does not
//!   execute newlines contained in a bracketed paste, so a submit appended
//!   inside the frame is swallowed. ember already had this right.
//! - **[`UnbracketedMultiline`] stays a per-app knob.** anvil/forge truncate
//!   a multiline paste to its first line when the shell has not advertised
//!   2004; ember confirms and then sends everything; frost sends everything.
//!   That is a product disagreement, not a bug, and unifying it under cover of
//!   a security fix would silently change two apps.
//! - **Control stripping is a flag, but marker removal is not.** Stripping C0
//!   and C1 changes observable behaviour (ANSI-coloured text pasted into a
//!   pager), so it is opt-in per call site. Clipboard paste and command recall
//!   both enable it; lower-level callers can still opt out when controls are
//!   part of an intentional program-input protocol. Marker removal has no such
//!   tradeoff.

use std::borrow::Cow;

/// Start of a bracketed paste (DECSET 2004 framing).
pub const PASTE_START: &[u8] = b"\x1b[200~";
/// End of a bracketed paste.
pub const PASTE_END: &[u8] = b"\x1b[201~";

/// `Ctrl+U` — discard the shell's current line buffer.
const KILL_LINE: u8 = 0x15;

/// The 8-bit CSI forms of the two markers, in *both* spellings that can reach
/// us. [`InputGuard`] sees a raw `9B` from a caller that assembled bytes
/// itself; a clipboard string carrying `U+009B` arrives UTF-8 encoded as
/// `C2 9B`, so the text path would never match the raw form. Strip both — a
/// UTF-8 reading shell does not treat `C2 9B` as CSI, but the byte the marker
/// is made of is not something to be clever about.
const C1_PASTE_START: &[u8] = b"\x9b200~";
const C1_PASTE_END: &[u8] = b"\x9b201~";
const C1_PASTE_START_UTF8: &[u8] = "\u{9b}200~".as_bytes();
const C1_PASTE_END_UTF8: &[u8] = "\u{9b}201~".as_bytes();

/// Terminal modes that change how a payload must be framed. Supplied by the
/// caller; see the module docs on why this is not tracked here.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PasteModes {
    /// The child advertised DECSET 2004 and will strip the framing itself.
    pub bracketed: bool,
}

/// What to do with a multiline payload when the child has *not* advertised
/// bracketed paste, so every embedded newline would execute a line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnbracketedMultiline {
    /// Keep only the first logical line (anvil, forge).
    FirstLineOnly,
    /// Send every line and let the child execute them (ember, frost).
    SendVerbatim,
}

/// Per-call-site policy. Only [`PastePolicy::strip_controls`] and the multiline
/// choice are discretionary; marker removal happens regardless.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PastePolicy {
    pub unbracketed_multiline: UnbracketedMultiline,
    /// Remove C0 (except `\t` and `\n`) and all C1 from the body.
    pub strip_controls: bool,
    /// Append a CR after the frame so the child runs what was pasted.
    pub submit: bool,
}

impl PastePolicy {
    /// Clipboard defaults: de-fang aggressively, never auto-run.
    pub fn clipboard(unbracketed_multiline: UnbracketedMultiline) -> Self {
        Self {
            unbracketed_multiline,
            strip_controls: true,
            submit: false,
        }
    }

    /// Command recall / block re-run. History can originate in a spoofed
    /// terminal protocol stream, so it receives the same control-byte
    /// defanging as clipboard text even though it is not auto-submitted.
    pub fn prompt_insert(unbracketed_multiline: UnbracketedMultiline) -> Self {
        Self {
            unbracketed_multiline,
            strip_controls: true,
            submit: false,
        }
    }
}

/// What was found in a payload. Drives confirmation dialogs, and records what
/// [`encode_paste`] had to do to the body.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PasteRisk {
    /// Logical lines after newline normalization; 1 for a single-line payload.
    pub lines: usize,
    /// Byte length after normalization, *before* any truncation.
    pub bytes: usize,
    /// A paste-bracket marker was embedded in the body and was removed. This
    /// is the injection attempt itself, so a frontend may want to say so.
    pub had_embedded_paste_marker: bool,
    /// C0/C1 bytes were present (removed only when the policy asked).
    pub had_controls: bool,
    /// Unicode spacing/formatting could make reviewed text display differently
    /// from the bytes sent to the child. Clipboard callers may confirm it;
    /// [`encode_prompt_insert`] rejects it outright.
    pub had_visual_spoofing: bool,
    /// The normalized body is larger than the review-only command boundary.
    /// Clipboard transfer can still confirm it; prompt insertion rejects it.
    pub exceeded_review_limit: bool,
    /// The body was reduced to its first line by [`UnbracketedMultiline`].
    pub truncated_to_first_line: bool,
}

/// An encoded payload, ready for the PTY writer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paste {
    /// Exactly what to hand the PTY, framing and submit CR included.
    pub bytes: Vec<u8>,
    /// The text a frontend should mirror into its own editor model. Never
    /// contains framing or the submit CR, and reflects any truncation, so an
    /// editor shadow cannot drift from what the child actually received.
    pub echo_text: String,
    pub risk: PasteRisk,
}

impl Paste {
    /// Nothing survived normalization; callers should skip the write entirely.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

/// Fold every line-ending form to `\n`.
///
/// A lone CR is an executable Enter in a canonical-mode shell, so leaving it
/// alone would let a payload bypass both the newline check a frontend does for
/// confirmation and the first-line truncation below.
fn normalize_newlines(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// Remove paste-bracket markers, then optionally C0/C1, recording what was
/// found. Markers go first: stripping ESC first would leave a literal `[201~`
/// behind, which is harmless but wrong to echo back into an editor model.
///
/// Removal checks the **tail of the output** after appending each character,
/// rather than scanning the input for markers. Scanning the input is what an
/// obvious implementation does, and it is exploitable: deleting a marker splices
/// what was in front of it onto what followed, and those halves can spell a live
/// marker that a forward scan has already walked past. `ESC[ ESC[ ESC[201~ 201~`
/// is the shortest example — remove the one real match and the leading `ESC[`
/// joins the trailing `201~`. Checking the output tail catches every marker the
/// splice creates, in one pass, because a marker can only ever be completed by
/// the character just appended.
fn defang(text: &str, strip_controls: bool) -> (String, bool, bool) {
    // Pass 1 — markers. Must run before control stripping: removing the ESC
    // first would leave a literal `[201~` behind, which is inert but wrong to
    // echo into an editor model.
    let mut stripped = String::with_capacity(text.len());
    let mut had_marker = false;
    for ch in text.chars() {
        stripped.push(ch);
        // A marker ends in `~`, so only that character can complete one; every
        // other byte skips the check.
        if ch == '~' {
            if let Some(len) = marker_len_at_end_str(&stripped) {
                had_marker = true;
                stripped.truncate(stripped.len() - len);
            }
        }
    }

    // Pass 2 — controls. No marker can reappear here: every marker spelling
    // begins with a control (ESC or 0x9B), so if controls are being stripped
    // none can survive, and if they are not, this pass changes nothing.
    let mut had_controls = false;
    let mut out = String::with_capacity(stripped.len());
    for ch in stripped.chars() {
        let is_control = matches!(ch, '\u{0}'..='\u{8}' | '\u{b}'..='\u{1f}' | '\u{7f}')
            || matches!(ch, '\u{80}'..='\u{9f}');
        if is_control {
            had_controls = true;
            if strip_controls {
                continue;
            }
        }
        out.push(ch);
    }

    (out, had_marker, had_controls)
}

/// Length of a paste-bracket marker ending at the tail of a byte stream. Both
/// C1 spellings are in play here: a caller assembling bytes itself can emit a
/// bare `9B`.
fn marker_len_at_end(data: &[u8]) -> Option<usize> {
    [
        PASTE_START,
        PASTE_END,
        C1_PASTE_START,
        C1_PASTE_END,
        C1_PASTE_START_UTF8,
        C1_PASTE_END_UTF8,
    ]
    .into_iter()
    .find(|marker| data.ends_with(marker))
    .map(<[u8]>::len)
}

/// Same, for text.
///
/// The *raw* C1 spellings are deliberately excluded: in a `str`, `U+009B` is
/// encoded `C2 9B`, so the 5-byte `9B 32 30 31 7E` form matches the tail of the
/// 6-byte encoded one. Truncating by 5 would leave a dangling `C2` and panic on
/// the next `String::truncate`, which is how this was found.
fn marker_len_at_end_str(text: &str) -> Option<usize> {
    [
        PASTE_START,
        PASTE_END,
        C1_PASTE_START_UTF8,
        C1_PASTE_END_UTF8,
    ]
    .into_iter()
    .find(|marker| text.as_bytes().ends_with(marker))
    .map(<[u8]>::len)
}

/// Length of a paste-bracket marker at the head of `data`, in either the 7-bit
/// `ESC [ 2 0 0 ~` or 8-bit `9B 2 0 0 ~` spelling.
fn marker_len(data: &[u8]) -> Option<usize> {
    for marker in [
        PASTE_START,
        PASTE_END,
        C1_PASTE_START,
        C1_PASTE_END,
        C1_PASTE_START_UTF8,
        C1_PASTE_END_UTF8,
    ] {
        if data.starts_with(marker) {
            return Some(marker.len());
        }
    }
    None
}

/// Describe a payload without building it, for a pre-flight confirmation.
pub fn classify_paste(text: &str) -> PasteRisk {
    let normalized = normalize_newlines(text);
    let (body, had_marker, had_controls) = defang(&normalized, false);
    let had_visual_spoofing = crate::review_input::contains_noncontrol_visual_spoofing(&body);
    let exceeded_review_limit = body.len() > crate::review_input::MAX_REVIEW_INPUT_BYTES;
    PasteRisk {
        lines: body.split('\n').count(),
        bytes: body.len(),
        had_embedded_paste_marker: had_marker,
        had_controls,
        had_visual_spoofing,
        exceeded_review_limit,
        truncated_to_first_line: false,
    }
}

/// Whether a payload deserves a confirmation prompt.
///
/// Multiline is the common foot-gun: the receiving program may execute on the
/// first newline whatever the terminal advertised. An embedded marker is a
/// deliberate injection attempt and always worth surfacing, even though
/// [`encode_paste`] has already neutralized it. Unicode visual-spoofing text is
/// likewise never treated as an ordinary one-line paste.
pub fn should_confirm(risk: &PasteRisk, threshold_bytes: usize) -> bool {
    risk.lines > 1
        || risk.bytes > threshold_bytes
        || risk.had_embedded_paste_marker
        || risk.had_visual_spoofing
        || risk.exceeded_review_limit
}

/// Encode a clipboard payload for the PTY.
///
/// Pure: a caller may re-run it with the same arguments and get the same bytes,
/// which is what ember's all-or-nothing backpressure retry depends on.
pub fn encode_paste(text: &str, modes: PasteModes, policy: PastePolicy) -> Paste {
    let normalized = normalize_newlines(text);
    let (body, had_marker, had_controls) = defang(&normalized, policy.strip_controls);
    let had_visual_spoofing = crate::review_input::contains_noncontrol_visual_spoofing(&body);
    let exceeded_review_limit = body.len() > crate::review_input::MAX_REVIEW_INPUT_BYTES;

    let mut risk = PasteRisk {
        lines: body.split('\n').count(),
        bytes: body.len(),
        had_embedded_paste_marker: had_marker,
        had_controls,
        had_visual_spoofing,
        exceeded_review_limit,
        truncated_to_first_line: false,
    };

    let mut body = body;
    if !modes.bracketed && policy.unbracketed_multiline == UnbracketedMultiline::FirstLineOnly {
        if let Some(first) = body.split('\n').next() {
            if first.len() != body.len() {
                body.truncate(first.len());
                risk.truncated_to_first_line = true;
            }
        }
    }

    if policy.submit {
        // The frame's own trailing newline would be submitted by the CR below,
        // so drop it rather than running a blank line.
        if let Some(trimmed) = body.strip_suffix('\n') {
            body.truncate(trimmed.len());
        }
    }

    if body.is_empty() {
        return Paste {
            bytes: Vec::new(),
            echo_text: body,
            risk,
        };
    }

    let mut bytes = Vec::with_capacity(body.len() + PASTE_START.len() + PASTE_END.len() + 1);
    if modes.bracketed {
        bytes.extend_from_slice(PASTE_START);
        bytes.extend_from_slice(body.as_bytes());
        bytes.extend_from_slice(PASTE_END);
    } else {
        bytes.extend_from_slice(body.as_bytes());
    }
    if policy.submit {
        // Outside the frame: readline does not execute newlines contained in a
        // bracketed paste.
        bytes.push(b'\r');
    }

    Paste {
        bytes,
        echo_text: body,
        risk,
    }
}

/// Encode a command this app is putting on the child's prompt — history recall,
/// block re-run, an agent's suggestion.
///
/// `clear_line_first` should be `true` unconditionally. forge learned why:
/// gating the `Ctrl+U` on a "the shell's line buffer is in sync" flag appends
/// the recalled command to whatever the user had already typed, because typed
/// text is not represented by that flag.
pub fn encode_prompt_insert(
    command: &str,
    modes: PasteModes,
    policy: PastePolicy,
    clear_line_first: bool,
) -> Paste {
    let mut paste = encode_paste(command, modes, policy);
    if paste.risk.had_visual_spoofing || paste.risk.exceeded_review_limit {
        paste.bytes.clear();
        paste.echo_text.clear();
        return paste;
    }
    if clear_line_first && !paste.bytes.is_empty() {
        paste.bytes.insert(0, KILL_LINE);
    }
    paste
}

/// The PTY-boundary net for writes that did **not** come from [`encode_paste`].
///
/// anvil and forge funnel every PTY write through one choke point, which is
/// the only thing covering their ad-hoc writers (a history palette that emits
/// raw command bytes, a queued startup command formatted with a trailing CR).
/// This replaces the `sanitize_input_chunk` both repos grew independently, with
/// two behaviour changes: markers are removed from a frame's *body* instead of
/// framed data being waved through untouched, and an explicit trailing CR no
/// longer exempts a payload whose earlier lines would each execute.
///
/// Callers must not split a marker across two writes. Every current caller
/// emits framing as one unit and [`encode_paste`] returns the whole payload in
/// one buffer, so the guard never withholds bytes — delaying a partial marker
/// until a write that may never come would be worse than the injection.
#[derive(Debug, Default)]
pub struct InputGuard {
    /// A frame this app opened is still open, so a terminator at the very end
    /// of a chunk is the legitimate close rather than an injection.
    in_frame: bool,
}

impl InputGuard {
    pub fn new() -> Self {
        Self::default()
    }

    /// True while an outgoing bracketed-paste frame is open.
    pub fn in_frame(&self) -> bool {
        self.in_frame
    }

    /// Rewrite one outgoing chunk. Borrows when nothing needed changing, so the
    /// common single-keystroke write stays allocation-free.
    pub fn filter<'a>(
        &mut self,
        data: &'a [u8],
        modes: PasteModes,
        policy: PastePolicy,
    ) -> Cow<'a, [u8]> {
        if data.is_empty() {
            return Cow::Borrowed(data);
        }

        let opens = data.starts_with(PASTE_START) || data.starts_with(C1_PASTE_START);
        // A terminator flush against the end of the chunk closes a frame this
        // app opened; anywhere else it is a body byte and must go.
        let closes = self.in_frame || opens;
        let trailing_close = closes && ends_with_terminator(data);

        let framed = self.in_frame || opens;
        let mut out: Vec<u8> = Vec::new();
        let mut changed = false;

        let body_end = match terminator_len_at_end(data) {
            Some(len) if trailing_close => data.len() - len,
            _ => data.len(),
        };
        let body_start = if opens {
            marker_len(data).unwrap_or(0)
        } else {
            0
        };

        // Same tail-check rule as `defang`, and for the same reason: removing a
        // marker splices its neighbours together and those halves can spell a
        // live marker. `out` holds the rewritten prefix once anything changes,
        // so the tail being checked is always post-splice.
        let mut i = body_start;
        while i < body_end {
            let byte = data[i];
            i += 1;
            if changed {
                out.push(byte);
                if byte == b'~' {
                    if let Some(len) = marker_len_at_end(&out) {
                        out.truncate(out.len() - len);
                    }
                }
                continue;
            }
            if byte == b'~' {
                if let Some(len) = marker_len_at_end(&data[body_start..i]) {
                    // First marker in this chunk: copy the clean prefix, drop
                    // the marker, and switch to the owned path.
                    out.extend_from_slice(&data[..i - len]);
                    changed = true;
                }
            }
        }

        self.in_frame = if trailing_close { false } else { framed };

        if changed {
            out.extend_from_slice(&data[body_end..]);
            if framed {
                return Cow::Owned(out);
            }

            // Marker removal and multiline protection are independent. If the
            // same unframed write carries both, the hostile marker must not
            // make the rewritten bytes skip first-line truncation or framing.
            return Cow::Owned(match self.apply_multiline_policy(&out, modes, policy) {
                Cow::Borrowed(_) => out,
                Cow::Owned(rewritten) => rewritten,
            });
        }

        if framed {
            // Inside a frame the child strips the framing itself, so newlines
            // are inert and no truncation policy applies.
            return Cow::Borrowed(data);
        }

        self.apply_multiline_policy(data, modes, policy)
    }

    /// Unframed multiline input would submit every completed line. Frame it when
    /// the child can strip the framing, otherwise fall back per policy.
    fn apply_multiline_policy<'a>(
        &mut self,
        data: &'a [u8],
        modes: PasteModes,
        policy: PastePolicy,
    ) -> Cow<'a, [u8]> {
        // A single line the caller explicitly submitted is not multiline input.
        let submitted_tail = data.ends_with(b"\r") || data.ends_with(b"\n");
        let scan_end = if submitted_tail {
            data.len() - 1
        } else {
            data.len()
        };
        let Some(first_break) = data[..scan_end]
            .iter()
            .position(|&byte| byte == b'\r' || byte == b'\n')
        else {
            return Cow::Borrowed(data);
        };

        if modes.bracketed {
            let mut wrapped =
                Vec::with_capacity(PASTE_START.len() + data.len() + PASTE_END.len() + 1);
            wrapped.extend_from_slice(PASTE_START);
            // Keep an explicit submission, but outside the frame.
            let (body, tail): (&[u8], &[u8]) = if submitted_tail {
                (&data[..data.len() - 1], &data[data.len() - 1..])
            } else {
                (data, b"")
            };
            wrapped.extend_from_slice(body);
            wrapped.extend_from_slice(PASTE_END);
            wrapped.extend_from_slice(tail);
            return Cow::Owned(wrapped);
        }

        match policy.unbracketed_multiline {
            UnbracketedMultiline::FirstLineOnly => Cow::Owned(data[..first_break].to_vec()),
            UnbracketedMultiline::SendVerbatim => Cow::Borrowed(data),
        }
    }
}

fn ends_with_terminator(data: &[u8]) -> bool {
    terminator_len_at_end(data).is_some()
}

fn terminator_len_at_end(data: &[u8]) -> Option<usize> {
    [PASTE_END, C1_PASTE_END, C1_PASTE_END_UTF8]
        .into_iter()
        .find(|marker| data.ends_with(marker))
        .map(<[u8]>::len)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clipboard(bracketed: bool) -> (PasteModes, PastePolicy) {
        (
            PasteModes { bracketed },
            PastePolicy::clipboard(UnbracketedMultiline::SendVerbatim),
        )
    }

    /// The bug this module exists for. A clipboard that closes the frame early
    /// must not leave an executable line behind.
    #[test]
    fn strips_embedded_terminator_from_the_body() {
        let (modes, policy) = clipboard(true);
        let paste = encode_paste("docs\x1b[201~\rrm -rf ~\r", modes, policy);

        let interior = &paste.bytes[PASTE_START.len()..paste.bytes.len() - PASTE_END.len()];
        assert!(
            !contains(interior, PASTE_END),
            "frame body still carries a terminator: {:?}",
            String::from_utf8_lossy(&paste.bytes)
        );
        assert!(paste.risk.had_embedded_paste_marker);
        assert!(paste.bytes.starts_with(PASTE_START));
        assert!(paste.bytes.ends_with(PASTE_END));
    }

    #[test]
    fn strips_embedded_start_marker_and_the_c1_spellings() {
        let (modes, policy) = clipboard(true);
        let paste = encode_paste("a\x1b[200~b\u{9b}201~c", modes, policy);
        assert_eq!(paste.echo_text, "abc");
        assert!(paste.risk.had_embedded_paste_marker);
    }

    /// Removing a marker splices its neighbours together, and those halves can
    /// spell a live marker that a forward scan has already walked past. This is
    /// the shortest payload that exploits it: `ESC[ ESC[ ESC[201~ 201~ 201~`
    /// leaves a working `ESC[201~` inside the frame under a scan-the-input
    /// implementation, and it reaches the PTY through command recall, which does
    /// not strip control bytes.
    #[test]
    fn a_spliced_terminator_does_not_reassemble() {
        let modes = PasteModes { bracketed: true };
        let policy = PastePolicy::prompt_insert(UnbracketedMultiline::SendVerbatim);
        let paste = encode_paste(
            "echo ok\x1b[\x1b[\x1b[201~201~201~\rrm -rf ~",
            modes,
            policy,
        );

        let interior = &paste.bytes[PASTE_START.len()..paste.bytes.len() - PASTE_END.len()];
        assert!(
            !contains(interior, PASTE_END),
            "reassembled terminator: {:?}",
            String::from_utf8_lossy(interior)
        );
        assert_eq!(paste.echo_text, "echo ok\nrm -rf ~");
        assert!(paste.risk.had_embedded_paste_marker);
    }

    /// Same splice, at the byte-level boundary net.
    #[test]
    fn guard_does_not_let_a_spliced_terminator_reassemble() {
        let (modes, policy) = guard_policy();
        let mut guard = InputGuard::new();
        let out = guard.filter(b"a\x1b[\x1b[\x1b[201~201~201~b", modes, policy);
        assert!(!contains(&out, PASTE_END), "{:?}", out);
        assert_eq!(&*out, b"ab");
    }

    #[test]
    fn marker_removal_ignores_the_control_policy() {
        let modes = PasteModes { bracketed: true };
        let policy = PastePolicy {
            unbracketed_multiline: UnbracketedMultiline::SendVerbatim,
            strip_controls: false,
            submit: false,
        };
        let paste = encode_paste("a\x1b[201~b", modes, policy);
        assert_eq!(paste.echo_text, "ab");
    }

    #[test]
    fn normalizes_every_line_ending_form() {
        let (modes, policy) = clipboard(true);
        let paste = encode_paste("a\r\nb\rc\nd", modes, policy);
        assert_eq!(paste.echo_text, "a\nb\nc\nd");
        assert_eq!(paste.risk.lines, 4);
    }

    #[test]
    fn submit_cr_sits_outside_the_frame() {
        let modes = PasteModes { bracketed: true };
        let policy = PastePolicy {
            unbracketed_multiline: UnbracketedMultiline::SendVerbatim,
            strip_controls: true,
            submit: true,
        };
        let paste = encode_paste("echo hi\n", modes, policy);
        assert_eq!(
            paste.bytes,
            b"\x1b[200~echo hi\x1b[201~\r".to_vec(),
            "{:?}",
            String::from_utf8_lossy(&paste.bytes)
        );
        assert_eq!(paste.echo_text, "echo hi");
    }

    #[test]
    fn truncates_to_first_line_only_when_asked_and_unbracketed() {
        let modes = PasteModes { bracketed: false };
        let policy = PastePolicy::clipboard(UnbracketedMultiline::FirstLineOnly);
        let paste = encode_paste("one\ntwo\nthree", modes, policy);
        assert_eq!(paste.echo_text, "one");
        assert!(paste.risk.truncated_to_first_line);
        // The risk report describes the payload, not the truncation.
        assert_eq!(paste.risk.lines, 3);

        let verbatim = encode_paste(
            "one\ntwo",
            modes,
            PastePolicy::clipboard(UnbracketedMultiline::SendVerbatim),
        );
        assert_eq!(verbatim.echo_text, "one\ntwo");
        assert!(!verbatim.risk.truncated_to_first_line);

        // Bracketed shells frame it instead, so neither app truncates.
        let bracketed = encode_paste(
            "one\ntwo",
            PasteModes { bracketed: true },
            PastePolicy::clipboard(UnbracketedMultiline::FirstLineOnly),
        );
        assert_eq!(bracketed.echo_text, "one\ntwo");
    }

    /// anvil's second bypass: an unnormalized clipboard ending in a bare CR
    /// used to hit the boundary's explicit-submission fast path and defeat the
    /// truncation the module doc promised.
    #[test]
    fn a_trailing_cr_does_not_exempt_multiline_input() {
        let modes = PasteModes { bracketed: false };
        let policy = PastePolicy::clipboard(UnbracketedMultiline::FirstLineOnly);
        let paste = encode_paste("one\rrm -rf ~\r", modes, policy);
        assert_eq!(paste.echo_text, "one");
        assert!(paste.risk.truncated_to_first_line);
    }

    #[test]
    fn clipboard_and_prompt_insert_strip_controls() {
        let modes = PasteModes { bracketed: true };
        let stripped = encode_paste(
            "a\x1b[31mb\u{7f}c\td",
            modes,
            PastePolicy::clipboard(UnbracketedMultiline::SendVerbatim),
        );
        assert_eq!(stripped.echo_text, "a[31mbc\td", "tab must survive");
        assert!(stripped.risk.had_controls);

        let prompt = encode_paste(
            "a\x1b[31mb",
            modes,
            PastePolicy::prompt_insert(UnbracketedMultiline::SendVerbatim),
        );
        assert_eq!(prompt.echo_text, "a[31mb");
        assert!(prompt.risk.had_controls);
    }

    #[test]
    fn empty_after_normalization_encodes_nothing() {
        let (modes, policy) = clipboard(true);
        let paste = encode_paste("\x1b[201~", modes, policy);
        assert!(paste.is_empty());
        assert!(paste.bytes.is_empty());
    }

    #[test]
    fn prompt_insert_clears_the_line_first() {
        let paste = encode_prompt_insert(
            "git status",
            PasteModes { bracketed: true },
            PastePolicy::prompt_insert(UnbracketedMultiline::FirstLineOnly),
            true,
        );
        assert_eq!(paste.bytes[0], KILL_LINE);
        assert_eq!(&paste.bytes[1..], b"\x1b[200~git status\x1b[201~");
        assert_eq!(paste.echo_text, "git status");

        let without = encode_prompt_insert(
            "git status",
            PasteModes { bracketed: true },
            PastePolicy::prompt_insert(UnbracketedMultiline::FirstLineOnly),
            false,
        );
        assert_eq!(without.bytes[0], 0x1b);
    }

    #[test]
    fn prompt_insert_of_nothing_does_not_emit_a_bare_kill_line() {
        let paste = encode_prompt_insert(
            "",
            PasteModes { bracketed: true },
            PastePolicy::prompt_insert(UnbracketedMultiline::FirstLineOnly),
            true,
        );
        assert!(paste.bytes.is_empty());
    }

    #[test]
    fn prompt_insert_rejects_visual_spoofing_while_clipboard_surfaces_it() {
        let text = "echo\u{00a0}looks-separated";
        let modes = PasteModes { bracketed: true };
        let clipboard = encode_paste(
            text,
            modes,
            PastePolicy::clipboard(UnbracketedMultiline::FirstLineOnly),
        );
        assert!(clipboard.risk.had_visual_spoofing);
        assert!(should_confirm(&clipboard.risk, usize::MAX));
        assert_eq!(clipboard.echo_text, text);

        let prompt = encode_prompt_insert(
            text,
            modes,
            PastePolicy::prompt_insert(UnbracketedMultiline::FirstLineOnly),
            true,
        );
        assert!(prompt.is_empty());
        assert!(prompt.risk.had_visual_spoofing);

        let oversized = "x".repeat(crate::review_input::MAX_REVIEW_INPUT_BYTES + 1);
        let prompt = encode_prompt_insert(
            &oversized,
            modes,
            PastePolicy::prompt_insert(UnbracketedMultiline::FirstLineOnly),
            true,
        );
        assert!(prompt.is_empty());
        assert!(prompt.risk.exceeded_review_limit);
    }

    #[test]
    fn classify_matches_the_confirmation_policy() {
        let single = classify_paste("echo hi");
        assert_eq!(single.lines, 1);
        assert!(!should_confirm(&single, 4096));

        let multi = classify_paste("echo hi\n");
        assert_eq!(multi.lines, 2, "a trailing newline is a second line");
        assert!(
            !multi.had_visual_spoofing,
            "structural newlines are covered by multiline risk, not Unicode spoofing"
        );
        assert!(should_confirm(&multi, 4096));

        let framed = encode_prompt_insert(
            "printf one\nprintf two",
            PasteModes { bracketed: true },
            PastePolicy::prompt_insert(UnbracketedMultiline::FirstLineOnly),
            true,
        );
        assert_eq!(framed.echo_text, "printf one\nprintf two");
        assert_eq!(
            framed.bytes,
            b"\x15\x1b[200~printf one\nprintf two\x1b[201~"
        );

        let first_line = encode_prompt_insert(
            "printf one\nprintf two",
            PasteModes { bracketed: false },
            PastePolicy::prompt_insert(UnbracketedMultiline::FirstLineOnly),
            true,
        );
        assert_eq!(first_line.echo_text, "printf one");
        assert_eq!(first_line.bytes, b"\x15printf one");

        let verbatim = encode_prompt_insert(
            "printf one\nprintf two",
            PasteModes { bracketed: false },
            PastePolicy::prompt_insert(UnbracketedMultiline::SendVerbatim),
            true,
        );
        assert_eq!(verbatim.echo_text, "printf one\nprintf two");
        assert_eq!(verbatim.bytes, b"\x15printf one\nprintf two");

        let big = classify_paste(&"x".repeat(5000));
        assert!(should_confirm(&big, 4096));
        assert!(!should_confirm(&classify_paste(&"x".repeat(4000)), 4096));

        // A hostile clipboard is surfaced even though encode_paste defused it.
        let hostile = classify_paste("ok\x1b[201~rm -rf ~");
        assert_eq!(hostile.lines, 1);
        assert!(should_confirm(&hostile, 4096));
    }

    #[test]
    fn classify_does_not_strip_controls_from_its_byte_count() {
        let risk = classify_paste("a\x1b[31mb");
        assert!(risk.had_controls);
        assert_eq!(risk.bytes, "a\x1b[31mb".len());
    }

    // ── InputGuard ───────────────────────────────────────────────────────────

    fn guard_policy() -> (PasteModes, PastePolicy) {
        (
            PasteModes { bracketed: true },
            PastePolicy::clipboard(UnbracketedMultiline::FirstLineOnly),
        )
    }

    #[test]
    fn guard_passes_an_ordinary_keystroke_by_reference() {
        let (modes, policy) = guard_policy();
        let mut guard = InputGuard::new();
        assert!(matches!(
            guard.filter(b"a", modes, policy),
            Cow::Borrowed(b"a")
        ));
    }

    /// anvil wrote its frame as three separate `write_bytes` calls, so the
    /// body arrived while a frame was already open and the old boundary waved
    /// it through untouched.
    #[test]
    fn guard_strips_a_terminator_from_a_split_frames_body() {
        let (modes, policy) = guard_policy();
        let mut guard = InputGuard::new();

        assert_eq!(&*guard.filter(PASTE_START, modes, policy), PASTE_START);
        assert!(guard.in_frame());

        let body = guard.filter(b"docs\x1b[201~\rrm -rf ~\r", modes, policy);
        assert!(!contains(&body, PASTE_END), "{:?}", body);
        assert_eq!(&*body, b"docs\rrm -rf ~\r");
        assert!(guard.in_frame(), "the caller has not closed the frame yet");

        assert_eq!(&*guard.filter(PASTE_END, modes, policy), PASTE_END);
        assert!(!guard.in_frame());
    }

    #[test]
    fn guard_keeps_the_close_of_a_whole_frame_and_strips_the_interior() {
        let (modes, policy) = guard_policy();
        let mut guard = InputGuard::new();
        let out = guard.filter(b"\x1b[200~a\x1b[201~b\x1b[201~", modes, policy);
        assert_eq!(&*out, b"\x1b[200~ab\x1b[201~");
        assert!(!guard.in_frame());
    }

    /// A terminator arriving with no frame open is pure injection.
    #[test]
    fn guard_drops_a_stray_terminator() {
        let (modes, policy) = guard_policy();
        let mut guard = InputGuard::new();
        assert_eq!(&*guard.filter(b"a\x1b[201~b", modes, policy), b"ab");
        assert!(!guard.in_frame());
    }

    #[test]
    fn guard_marker_removal_cannot_bypass_multiline_protection() {
        let (modes, policy) = guard_policy();
        let mut guard = InputGuard::new();
        assert_eq!(
            &*guard.filter(b"one\x1b[201~\ntwo\r", modes, policy),
            b"\x1b[200~one\ntwo\x1b[201~\r"
        );

        let modes = PasteModes { bracketed: false };
        let mut guard = InputGuard::new();
        assert_eq!(
            &*guard.filter(b"one\x1b[201~\ntwo\r", modes, policy),
            b"one"
        );
    }

    #[test]
    fn guard_frames_unbracketed_multiline_input_when_the_child_can_strip_it() {
        let (modes, policy) = guard_policy();
        let mut guard = InputGuard::new();
        let out = guard.filter(b"one\ntwo\r", modes, policy);
        assert_eq!(&*out, b"\x1b[200~one\ntwo\x1b[201~\r");
    }

    #[test]
    fn guard_falls_back_to_the_first_line_without_bracketed_paste() {
        let modes = PasteModes { bracketed: false };
        let policy = PastePolicy::clipboard(UnbracketedMultiline::FirstLineOnly);
        let mut guard = InputGuard::new();
        assert_eq!(&*guard.filter(b"one\ntwo\r", modes, policy), b"one");

        let mut verbatim_guard = InputGuard::new();
        let verbatim = PastePolicy::clipboard(UnbracketedMultiline::SendVerbatim);
        assert_eq!(
            &*verbatim_guard.filter(b"one\ntwo\r", modes, verbatim),
            b"one\ntwo\r"
        );
    }

    #[test]
    fn guard_leaves_a_single_explicit_submission_alone() {
        let modes = PasteModes { bracketed: false };
        let policy = PastePolicy::clipboard(UnbracketedMultiline::FirstLineOnly);
        let mut guard = InputGuard::new();
        assert_eq!(
            &*guard.filter(b"git status\r", modes, policy),
            b"git status\r"
        );
    }

    /// The encoder's own output must survive the boundary byte-for-byte, or the
    /// two layers would fight over the same payload.
    #[test]
    fn guard_is_idempotent_over_encoder_output() {
        let (modes, policy) = guard_policy();
        let paste = encode_paste("one\ntwo", modes, policy);
        let mut guard = InputGuard::new();
        assert_eq!(
            &*guard.filter(&paste.bytes, modes, policy),
            &paste.bytes[..]
        );
        assert!(!guard.in_frame());
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }
}
