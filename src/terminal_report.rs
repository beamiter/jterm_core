//! Telling the reports libvte generates on its own apart from the keys it
//! commits for the user.
//!
//! The live VTE in block mode has no PTY. Its `commit` signal is how typed
//! keys reach the child: the hosts forward every commit to the real PTY. That
//! signal carries more than keystrokes. libvte 0.76 emits every reply it owes
//! the application, every focus report and every SGR mouse report through
//! `commit` as well, PTY or not (`send_child` → `emit_commit`, vte.cc
//! ~4634-4650):
//! - answers to DA1/DA2/DA3, XTVERSION, DSR, CPR, DECRQM, DECRQSS and
//!   XTWINOPS queries that reached it;
//! - `CSI I` / `CSI O` under DECSET 1004 whenever the widget gains or loses
//!   focus, which includes every window activation and deactivation, plus
//!   one immediately when the mode is set;
//! - click, drag, motion and wheel reports under DECSET 1000-1003 with 1006.
//!
//! Those bytes must still reach the child, since they are its answers and its
//! mouse input, but they are not the user typing. Handled as typing, the
//! focus-out report from an Alt+Tab clears a block selection, releases the
//! selection hold that parks streaming output, snaps a history view back to
//! the live card and counts as human input. [`classify_terminal_report`]
//! tells the host which commits are reports, so it can write them and skip
//! the rest.
//!
//! libvte formats each report whole and hands it to `send_child` once, so a
//! report is always exactly one commit and only whole commits are matched.
//! A key must never be claimed: every legacy form VTE 0.76 commits for a key
//! press classifies as `None`. There is one unavoidable collision. A cursor
//! position report `CSI row;col R` has the shape of a modified F3 (`CSI 1;2 R`
//! is Shift+F3), so the host counts the CPR queries it left to the VTE and
//! passes `cpr_outstanding`.
//!
//! X10 and UTF-8 mouse encodings never show up: without a PTY, libvte's
//! `feed_child_binary` drops them (vte.cc ~4701-4711). Only SGR mouse reports
//! reach `commit`.

use std::ops::RangeInclusive;

/// A report libvte generated itself, as opposed to bytes for a key press.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TerminalReport {
    /// `CSI I`: the widget gained focus (DECSET 1004).
    FocusIn,
    /// `CSI O`: the widget lost focus (DECSET 1004). Also sent on every
    /// window deactivation, since GTK synthesises a focus leave for it.
    FocusOut,
    /// SGR motion with no button held (DECSET 1003): the pointer moved.
    MouseMotion,
    /// SGR press, release, or motion with a button held (a drag).
    MouseButton,
    /// SGR wheel press (buttons 4-7).
    MouseWheel,
    /// DA1 `CSI ? … c` or DA2 `CSI > … c`.
    DeviceAttributes,
    /// DECRPM `CSI ? mode ; value $ y` (DEC private) or `CSI mode ; value $ y`
    /// (ANSI): the answer to DECRQM.
    ModeReport,
    /// A DCS or OSC string: XTVERSION `DCS >|VTE(7600) ST`, DA3
    /// `DCS !|id ST`, DECRQSS `DCS 1$r… ST`, or an OSC colour reply.
    ControlString,
    /// DSR `CSI 0 n` (ok) or `CSI 3 n` (malfunction), or a DEC-private
    /// `CSI ? … n` status report.
    StatusReport,
    /// XTWINOPS `CSI n ; … t`: window state, position or size in pixels or
    /// cells.
    WindowReport,
    /// CPR `CSI row ; col R` or DECXCPR `CSI ? row ; col ; page R`.
    CursorPosition,
}

impl TerminalReport {
    /// Reports that are never a user action aimed at the application: focus
    /// changes and bare pointer motion. They must not release a selection
    /// hold or end anything the user is in the middle of. Button presses and
    /// wheel reports are the user clicking or scrolling the app and are not
    /// passive.
    pub fn is_passive(self) -> bool {
        matches!(self, Self::FocusIn | Self::FocusOut | Self::MouseMotion)
    }

    /// Answers to queries the application sent. Nothing about them is
    /// input.
    pub fn is_reply(self) -> bool {
        matches!(
            self,
            Self::DeviceAttributes
                | Self::ModeReport
                | Self::ControlString
                | Self::StatusReport
                | Self::WindowReport
                | Self::CursorPosition
        )
    }
}

/// Classify one whole `commit` from the live VTE, or `None` when it is (or
/// could be) the bytes of a key press, text or a paste.
///
/// `cpr_outstanding` says whether a cursor position query is waiting for the
/// VTE's answer. Only then is `CSI row;col R` a report; otherwise it is a
/// modified F3.
pub fn classify_terminal_report(commit: &[u8], cpr_outstanding: bool) -> Option<TerminalReport> {
    if let Some(csi) = commit.strip_prefix(b"\x1b[") {
        return classify_csi(csi, cpr_outstanding);
    }
    // DCS replies always end in ST.
    if let Some(body) = commit.strip_prefix(b"\x1bP") {
        let body = body.strip_suffix(b"\x1b\\")?;
        return is_string_body(body).then_some(TerminalReport::ControlString);
    }
    // OSC replies mirror the query's terminator, ST or BEL.
    if let Some(body) = commit.strip_prefix(b"\x1b]") {
        let body = body
            .strip_suffix(b"\x1b\\")
            .or_else(|| body.strip_suffix(b"\x07"))?;
        return is_string_body(body).then_some(TerminalReport::ControlString);
    }
    None
}

/// `csi` is everything after `ESC [`, final byte included.
fn classify_csi(csi: &[u8], cpr_outstanding: bool) -> Option<TerminalReport> {
    let (&final_byte, head) = csi.split_last()?;
    let report = match (head, final_byte) {
        (b"", b'I') => TerminalReport::FocusIn,
        (b"", b'O') => TerminalReport::FocusOut,
        ([b'<', params @ ..], b'M' | b'm') => return sgr_mouse(params),
        ([b'?' | b'>', params @ ..], b'c') if fields_in(params, 1..=usize::MAX) => {
            TerminalReport::DeviceAttributes
        }
        ([params @ .., b'$'], b'y') if fields_in(strip_private(params), 2..=2) => {
            TerminalReport::ModeReport
        }
        (b"0" | b"3", b'n') => TerminalReport::StatusReport,
        ([b'?', params @ ..], b'n') if fields_in(params, 1..=usize::MAX) => {
            TerminalReport::StatusReport
        }
        (params, b't') if fields_in(params, 1..=3) => TerminalReport::WindowReport,
        // No key press commits `CSI ? … R`, so the private DECXCPR reply is
        // always a report. The host's CPR ledger only counts plain `CSI 6 n`
        // queries, so this form must not wait for it (nor use up its credit).
        ([b'?', params @ ..], b'R') if fields_in(params, 2..=3) => TerminalReport::CursorPosition,
        // The one shape a key shares: `CSI 1;2 R` is Shift+F3 in VTE 0.76.
        ([b'0'..=b'9', ..], b'R') if cpr_outstanding && fields_in(head, 2..=2) => {
            TerminalReport::CursorPosition
        }
        _ => return None,
    };
    Some(report)
}

/// DECRPM comes in a DEC-private (`?`) and an ANSI flavour.
fn strip_private(params: &[u8]) -> &[u8] {
    params.strip_prefix(b"?").unwrap_or(params)
}

/// An SGR mouse report's `button ; col ; row`, classified by its button byte.
/// Bit 32 is motion and bit 64 the wheel; motion whose low button bits are 3
/// ("no button") is the pointer merely moving under DECSET 1003.
fn sgr_mouse(params: &[u8]) -> Option<TerminalReport> {
    let mut fields = params.split(|&byte| byte == b';');
    let button = decimal(fields.next()?)?;
    decimal(fields.next()?)?;
    decimal(fields.next()?)?;
    if fields.next().is_some() {
        return None;
    }
    Some(if button & 32 != 0 && button & 3 == 3 && button < 64 {
        TerminalReport::MouseMotion
    } else if button & 64 != 0 {
        TerminalReport::MouseWheel
    } else {
        TerminalReport::MouseButton
    })
}

/// Whether `params` is a list of decimal fields whose count is in `range`.
fn fields_in(params: &[u8], range: RangeInclusive<usize>) -> bool {
    decimal_fields(params).is_some_and(|count| range.contains(&count))
}

/// The number of `;`-separated fields in a CSI parameter string, or `None`
/// when any field is empty or not purely decimal. No report VTE sends has a
/// sub-parameter or a marker in the middle.
fn decimal_fields(params: &[u8]) -> Option<usize> {
    params
        .split(|&byte| byte == b';')
        .try_fold(0, |count, field| decimal(field).map(|_| count + 1))
}

/// One non-empty decimal field. The value saturates, so a hostile width
/// cannot wrap into a different classification.
fn decimal(field: &[u8]) -> Option<u32> {
    if field.is_empty() || !field.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(field.iter().fold(0_u32, |value, digit| {
        value
            .saturating_mul(10)
            .saturating_add(u32::from(digit - b'0'))
    }))
}

/// A DCS or OSC reply body is one printable string. A control byte inside it
/// means the commit is something else stitched together, not one report.
fn is_string_body(body: &[u8]) -> bool {
    body.iter().all(|&byte| byte >= 0x20 && byte != 0x7f)
}

#[cfg(test)]
mod tests {
    use super::TerminalReport::*;
    use super::*;

    /// Every report shape libvte 0.76 commits, and what it classifies as.
    const REPORTS: &[(&[u8], TerminalReport)] = &[
        // Focus (DECSET 1004).
        (b"\x1b[I", FocusIn),
        (b"\x1b[O", FocusOut),
        // SGR mouse (DECSET 1006): motion with no button, under 1003, with
        // and without modifier bits (shift 4, alt 8, ctrl 16).
        (b"\x1b[<35;10;5M", MouseMotion),
        (b"\x1b[<39;1;1M", MouseMotion),
        (b"\x1b[<43;120;40M", MouseMotion),
        (b"\x1b[<51;7;7M", MouseMotion),
        // Presses, releases and drags with a button held are clicks.
        (b"\x1b[<0;1;1M", MouseButton),
        (b"\x1b[<0;1;1m", MouseButton),
        (b"\x1b[<2;80;24M", MouseButton),
        (b"\x1b[<16;3;4M", MouseButton),
        (b"\x1b[<32;3;4M", MouseButton),
        (b"\x1b[<34;3;4M", MouseButton),
        (b"\x1b[<128;1;1M", MouseButton),
        // Wheel up/down/left/right, and with Ctrl.
        (b"\x1b[<64;1;1M", MouseWheel),
        (b"\x1b[<65;10;20M", MouseWheel),
        (b"\x1b[<66;1;1M", MouseWheel),
        (b"\x1b[<67;1;1M", MouseWheel),
        (b"\x1b[<80;1;1M", MouseWheel),
        // DA1 / DA2 as VTE 0.76 answers them, and xterm-style ones.
        (b"\x1b[?61;1;21;22c", DeviceAttributes),
        (b"\x1b[?1;2c", DeviceAttributes),
        (b"\x1b[>61;7600;1c", DeviceAttributes),
        (b"\x1b[>0;276;0c", DeviceAttributes),
        // DECRPM, DEC private and ANSI.
        (b"\x1b[?2026;4$y", ModeReport),
        (b"\x1b[?1049;2$y", ModeReport),
        (b"\x1b[?2031;0$y", ModeReport),
        (b"\x1b[4;2$y", ModeReport),
        // DCS: XTVERSION, DA3, DECRQSS (valid and invalid request).
        (b"\x1bP>|VTE(7600)\x1b\\", ControlString),
        (b"\x1bP!|7E565445\x1b\\", ControlString),
        (b"\x1bP1$r0m\x1b\\", ControlString),
        (b"\x1bP1$r2 q\x1b\\", ControlString),
        (b"\x1bP0$r\x1b\\", ControlString),
        // OSC colour replies, with either terminator.
        (b"\x1b]11;rgb:1e1e/1e1e/2e2e\x1b\\", ControlString),
        (b"\x1b]10;rgb:ffff/ffff/ffff\x07", ControlString),
        (b"\x1b]4;1;rgb:cdcd/0000/0000\x1b\\", ControlString),
        // DSR and DEC-private DSR.
        (b"\x1b[0n", StatusReport),
        (b"\x1b[3n", StatusReport),
        (b"\x1b[?13n", StatusReport),
        (b"\x1b[?27;0;0;5n", StatusReport),
        // XTWINOPS.
        (b"\x1b[1t", WindowReport),
        (b"\x1b[3;0;0t", WindowReport),
        (b"\x1b[4;800;1200t", WindowReport),
        (b"\x1b[8;40;120t", WindowReport),
    ];

    /// Every legacy form VTE 0.76 can commit for a key press (keymap.cc plus
    /// the Backspace/Delete/text paths in vte.cc), in normal and application
    /// cursor/keypad modes, with and without xterm modifier parameters. None
    /// is a report, whatever the CPR ledger says, except the F3 collision
    /// tested on its own.
    const KEYS: &[&[u8]] = &[
        // Arrows, normal and application mode, and modified.
        b"\x1b[A",
        b"\x1b[B",
        b"\x1b[C",
        b"\x1b[D",
        b"\x1bOA",
        b"\x1bOB",
        b"\x1bOC",
        b"\x1bOD",
        b"\x1b[1;2A",
        b"\x1b[1;3B",
        b"\x1b[1;5C",
        b"\x1b[1;8D",
        // Home / End / KP_Begin.
        b"\x1b[H",
        b"\x1b[F",
        b"\x1bOH",
        b"\x1bOF",
        b"\x1b[E",
        b"\x1bOE",
        b"\x1b[1;5H",
        b"\x1b[1;2F",
        // Insert / Delete / Page Up / Page Down.
        b"\x1b[2~",
        b"\x1b[3~",
        b"\x1b[5~",
        b"\x1b[6~",
        b"\x1b[3;5~",
        b"\x1b[5;2~",
        b"\x1b[6;3~",
        // F1-F4: SS3 alone, CSI with modifiers (F3's `R` is tested apart).
        b"\x1bOP",
        b"\x1bOQ",
        b"\x1bOR",
        b"\x1bOS",
        b"\x1b[1;2P",
        b"\x1b[1;5Q",
        b"\x1b[1;2S",
        b"\x1b[1;3S",
        // F5-F12 and the F13+ range.
        b"\x1b[15~",
        b"\x1b[17~",
        b"\x1b[18~",
        b"\x1b[19~",
        b"\x1b[20~",
        b"\x1b[21~",
        b"\x1b[23~",
        b"\x1b[24~",
        b"\x1b[15;2~",
        b"\x1b[24;5~",
        b"\x1b[25~",
        b"\x1b[34~",
        b"\x1b[42~",
        b"\x1b[56~",
        // Shift+Tab (back-tab), and the application keypad.
        b"\x1b[Z",
        b"\x1bOI",
        b"\x1bOM",
        b"\x1bOj",
        b"\x1bOk",
        b"\x1bOl",
        b"\x1bOm",
        b"\x1bOo",
        b"\x1bO ",
        // Enter, Tab, Backspace, Space, Esc and their Alt forms, whole or as
        // the split first half.
        b"\r",
        b"\n",
        b"\r\n",
        b"\t",
        b"\x7f",
        b"\x08",
        b" ",
        b"\0",
        b"\x1b",
        b"\x1b\x1b",
        b"\x1b\r",
        b"\x1b\t",
        b"\x1b ",
        b"\x1b\0",
        b"\x1b/",
        b"\x1b?",
        b"\x1b\x7f",
        // Ctrl letters, text, and Alt+letter joined, including the letters
        // that open a control string or a CSI.
        b"\x03",
        b"\x04",
        b"a",
        b"I",
        b"O",
        b"hello",
        "é".as_bytes(),
        "中".as_bytes(),
        b"\x1bb",
        b"\x1bP",
        b"\x1b]",
        b"\x1b[",
        b"\x1bO",
        b"[",
        b"[I",
        // A bracketed paste arrives as one commit too.
        b"\x1b[200~hello\x1b[201~",
        b"\x1b[200~\x1b[I\x1b[201~",
        // kitty CSI u forms (the hosts write these; VTE never commits them).
        b"\x1b[27u",
        b"\x1b[98;3u",
        b"\x1b[13;2u",
    ];

    #[test]
    fn every_report_libvte_commits_is_classified() {
        for &(commit, expected) in REPORTS {
            for cpr_outstanding in [false, true] {
                assert_eq!(
                    classify_terminal_report(commit, cpr_outstanding),
                    Some(expected),
                    "{:?}",
                    String::from_utf8_lossy(commit)
                );
            }
        }
    }

    #[test]
    fn no_key_press_is_ever_a_report() {
        for &commit in KEYS {
            for cpr_outstanding in [false, true] {
                assert_eq!(
                    classify_terminal_report(commit, cpr_outstanding),
                    None,
                    "{:?} cpr_outstanding={cpr_outstanding}",
                    String::from_utf8_lossy(commit)
                );
            }
        }
    }

    #[test]
    fn a_cursor_position_report_needs_an_outstanding_query() {
        // Shift+F3 and Ctrl+F3 are keys when nothing is pending...
        for f3 in [&b"\x1b[1;2R"[..], b"\x1b[1;5R", b"\x1b[1;3R", b"\x1b[1;8R"] {
            assert_eq!(classify_terminal_report(f3, false), None);
        }
        assert_eq!(classify_terminal_report(b"\x1b[12;40R", false), None);
        // ...and answers while the VTE owes one.
        for cpr in [
            &b"\x1b[1;1R"[..],
            b"\x1b[1;2R",
            b"\x1b[12;40R",
            b"\x1b[300;1R",
        ] {
            assert_eq!(classify_terminal_report(cpr, true), Some(CursorPosition));
        }
        // DECXCPR (`CSI ? r;c;p R`) never collides with a key, and the
        // host's ledger never counts its `CSI ? 6 n` query, so it is a
        // report whether or not a plain CPR is pending.
        for xcpr in [&b"\x1b[?12;40;1R"[..], b"\x1b[?3;7R"] {
            for cpr_outstanding in [false, true] {
                assert_eq!(
                    classify_terminal_report(xcpr, cpr_outstanding),
                    Some(CursorPosition)
                );
            }
        }
        // A bare `SS3 R` (F3) is never a CPR; nor is a one-field CSI R.
        assert_eq!(classify_terminal_report(b"\x1bOR", true), None);
        assert_eq!(classify_terminal_report(b"\x1b[5R", true), None);
        assert_eq!(classify_terminal_report(b"\x1b[1;2;3R", true), None);
    }

    #[test]
    fn only_whole_well_formed_reports_match() {
        for malformed in [
            // Truncated, extended or glued together.
            &b"\x1b[<0;1M"[..],
            b"\x1b[<0;1;1",
            b"\x1b[<0;1;1;1M",
            b"\x1b[<0;1;1Mx",
            b"\x1b[<;1;1M",
            b"\x1b[<a;1;1M",
            b"\x1b[<0:1;1;1M",
            b"\x1b[O\x1b[I",
            b"x\x1b[O",
            b"\x1b[Ox",
            b"\x1b[1I",
            b"\x1b[?I",
            // Empty or non-decimal parameters.
            b"\x1b[?c",
            b"\x1b[>c",
            b"\x1b[c",
            b"\x1b[?;1c",
            b"\x1b[?1;;2c",
            b"\x1b[?2026$y",
            b"\x1b[?2026;4;1$y",
            b"\x1b[$y",
            b"\x1b[?x$y",
            b"\x1b[1n",
            b"\x1b[5n",
            b"\x1b[6n",
            b"\x1b[00n",
            b"\x1b[?n",
            b"\x1b[t",
            b"\x1b[8;40;120;1t",
            b"\x1b[8;a;120t",
            // Unterminated or stitched control strings.
            b"\x1bP>|VTE(7600)",
            b"\x1bP>|VTE\x1b[I\x1b\\",
            b"\x1b]11;rgb:0/0/0",
            b"\x1b]11;rgb\n:0/0/0\x07",
            b"\x1b]11;\x1b]12;x\x07",
            // Nothing at all.
            b"",
            b"\x1b[",
        ] {
            for cpr_outstanding in [false, true] {
                assert_eq!(
                    classify_terminal_report(malformed, cpr_outstanding),
                    None,
                    "{:?}",
                    String::from_utf8_lossy(malformed)
                );
            }
        }
    }

    #[test]
    fn huge_mouse_coordinates_saturate_instead_of_wrapping() {
        // 4294967331 would wrap to 35 (passive motion) in u32 arithmetic;
        // saturated to u32::MAX its wheel bit is set, so it stays an action.
        assert_eq!(
            classify_terminal_report(b"\x1b[<4294967331;1;1M", false),
            Some(MouseWheel)
        );
        assert_eq!(
            classify_terminal_report(b"\x1b[<35;99999999999;1M", false),
            Some(MouseMotion)
        );
    }

    #[test]
    fn passive_reports_are_focus_and_bare_motion() {
        let all = [
            FocusIn,
            FocusOut,
            MouseMotion,
            MouseButton,
            MouseWheel,
            DeviceAttributes,
            ModeReport,
            ControlString,
            StatusReport,
            WindowReport,
            CursorPosition,
        ];
        for report in all {
            let passive = matches!(report, FocusIn | FocusOut | MouseMotion);
            let reply = !matches!(
                report,
                FocusIn | FocusOut | MouseMotion | MouseButton | MouseWheel
            );
            assert_eq!(report.is_passive(), passive, "{report:?}");
            assert_eq!(report.is_reply(), reply, "{report:?}");
        }
        // Clicks and wheel turns are the user acting on the app: a selection
        // hold must still flush for them.
        for acting in [MouseButton, MouseWheel] {
            assert!(!acting.is_passive() && !acting.is_reply());
        }
    }
}
