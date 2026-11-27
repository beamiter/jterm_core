//! FrostLegacyV1 C0/ESC admission and state execution. Finish every returned
//! host effect before stepping again or feeding the next parser event.
use crate::protocol_state::ProtocolState;
use crate::screen::{Charset, ScreenState, VerticalMotion};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrostStateEffect {
    Vertical(VerticalMotion),
    HardReset,
    AlignmentTest,
    /// The native diagnostic historically precedes designation mutation.
    CharsetNotice {
        bank: u8,
        designator: u8,
        charset: Charset,
    },
}

#[derive(Clone, Copy)]
enum Operation {
    Control(u8),
    Escape(u8),
    Designate { bank: u8, designator: u8 },
    DesignateLatin1(u8),
    CommitDesignation { bank: u8, charset: Charset },
    CommitBritish(u8),
    AlignmentTest,
    Done,
}

pub struct FrostStateCommand {
    operation: Operation,
}
impl FrostStateCommand {
    pub fn control(byte: u8) -> Self {
        Self {
            operation: Operation::Control(byte),
        }
    }
    pub fn escape(intermediates: &[u8], final_byte: u8) -> Self {
        let operation = match (intermediates, final_byte) {
            (b"", byte) => Operation::Escape(byte),
            (b"#", b'8') => Operation::AlignmentTest,
            (b"(" | b")", designator) => Operation::Designate {
                bank: intermediates[0],
                designator,
            },
            (b"*" | b"+", designator @ (b'A' | b'B' | b'0')) => Operation::Designate {
                bank: intermediates[0],
                designator,
            },
            (b"-" | b"." | b"/", b'A') => Operation::DesignateLatin1(1 + intermediates[0] - b'-'),
            // Unsupported and multi-byte designators stay consumed.
            _ => Operation::Done,
        };
        Self { operation }
    }
    pub fn step(
        &mut self,
        screen: &mut ScreenState,
        protocol: &mut ProtocolState,
    ) -> Option<FrostStateEffect> {
        match std::mem::replace(&mut self.operation, Operation::Done) {
            Operation::Control(byte) => match byte {
                8 => {
                    screen.move_cursor_backward_with_wrap(1, protocol);
                    None
                }
                9 => {
                    screen.cursor_controls().tab_forward(1);
                    None
                }
                10..=12 => Some(FrostStateEffect::Vertical(VerticalMotion::LineFeed)),
                13 => {
                    screen.cursor_controls().set_column(1);
                    None
                }
                14 => {
                    screen.invoke_charset(true);
                    None
                }
                15 => {
                    screen.invoke_charset(false);
                    None
                }
                // In particular, BEL does not produce Ember's bell effect.
                _ => None,
            },
            Operation::Escape(byte) => match byte {
                b'7' => {
                    screen.save_cursor(protocol.modes().contains(&7));
                    None
                }
                b'8' => {
                    let _ = screen.restore_cursor();
                    None
                }
                b'E' => {
                    screen.begin_next_line();
                    Some(FrostStateEffect::Vertical(VerticalMotion::LineFeed))
                }
                b'D' => Some(FrostStateEffect::Vertical(VerticalMotion::LineFeed)),
                b'M' => Some(FrostStateEffect::Vertical(VerticalMotion::ReverseIndex)),
                b'H' => {
                    screen.cursor_controls().set_tab_stop(true);
                    None
                }
                b'c' => Some(FrostStateEffect::HardReset),
                b'n' => {
                    screen.invoke_charset_bank(2);
                    None
                }
                b'o' => {
                    screen.invoke_charset_bank(3);
                    None
                }
                b'~' | b'}' | b'|' => {
                    screen.invoke_gr_charset_bank(1 + b'~' - byte);
                    None
                }
                b'N' => {
                    screen.single_shift(2);
                    None
                }
                b'O' => {
                    screen.single_shift(3);
                    None
                }
                b'>' => {
                    protocol.modes_mut().remove(&66);
                    None
                }
                b'=' => {
                    protocol.modes_mut().insert(66);
                    None
                }
                // ESC < is ignored by Frost, unlike Ember's local dispatcher.
                _ => None,
            },
            Operation::AlignmentTest => Some(FrostStateEffect::AlignmentTest),
            Operation::Designate { bank, designator } => {
                if designator == b'A' {
                    let index = match bank {
                        b'(' => 0,
                        b')' => 1,
                        b'*' => 2,
                        _ => 3,
                    };
                    if index >= 2 {
                        screen.designate_british(index);
                        return None;
                    }
                    self.operation = Operation::CommitBritish(index);
                    return Some(FrostStateEffect::CharsetNotice {
                        bank,
                        designator,
                        charset: Charset::Ascii,
                    });
                }
                let charset = if designator == b'0' {
                    Charset::DecSpecialGraphics
                } else {
                    Charset::Ascii
                };
                if matches!(bank, b'*' | b'+') {
                    screen.designate_charset_bank(if bank == b'*' { 2 } else { 3 }, charset);
                    return None;
                }
                self.operation = Operation::CommitDesignation { bank, charset };
                Some(FrostStateEffect::CharsetNotice {
                    bank,
                    designator,
                    charset,
                })
            }
            Operation::CommitBritish(index) => {
                screen.designate_british(index);
                None
            }
            Operation::CommitDesignation { bank, charset } => {
                screen.designate_charset(bank != b'(', charset);
                None
            }
            Operation::DesignateLatin1(bank) => {
                screen.designate_latin1(bank);
                None
            }
            Operation::Done => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn snapshot(s: &ScreenState, p: &ProtocolState) -> String {
        format!(
            "{:?}",
            (
                (
                    s.grid.to_vec(),
                    &s.grid.row_wrapped,
                    s.cursor_row,
                    s.cursor_col,
                    s.pending_wrap,
                    &s.tab_stops,
                    &s.saved_state
                ),
                (
                    s.g0_charset,
                    s.g1_charset,
                    s.active_charset,
                    s.current_fg,
                    s.current_bg,
                    s.current_flags,
                    s.origin_mode,
                    s.insert_mode
                ),
                p
            )
        )
    }
    fn seed(bits: usize) -> (ScreenState, ProtocolState) {
        let mut s = ScreenState::new(3, 12);
        let mut p = ProtocolState::default();
        s.cursor_row = 2;
        s.cursor_col = 9;
        s.pending_wrap = bits & 1 != 0;
        s.g0_charset = Charset::DecSpecialGraphics;
        s.g1_charset = Charset::Ascii;
        s.active_charset = Charset::DecSpecialGraphics;
        if bits & 2 != 0 {
            p.modes_mut().insert(7);
        }
        if bits & 4 != 0 {
            p.modes_mut().insert(66);
        }
        if bits & 8 != 0 {
            s.cursor_row = 1;
            s.cursor_col = 3;
            s.save_cursor(true);
            s.cursor_row = 2;
            s.cursor_col = 9;
        }
        (s, p)
    }
    // Native control/escape bodies frozen independently of the command.
    fn frozen(
        s: &mut ScreenState,
        p: &mut ProtocolState,
        input: Option<(&[u8], u8)>,
        byte: u8,
    ) -> Vec<(FrostStateEffect, String)> {
        let mut out = vec![];
        if let Some((intermediates, final_byte)) = input {
            if intermediates.is_empty() {
                match final_byte {
                    b'7' => s.save_cursor(p.modes().contains(&7)),
                    b'8' => {
                        let _ = s.restore_cursor();
                    }
                    b'E' => {
                        s.begin_next_line();
                        out.push((
                            FrostStateEffect::Vertical(VerticalMotion::LineFeed),
                            snapshot(s, p),
                        ));
                    }
                    b'D' => out.push((
                        FrostStateEffect::Vertical(VerticalMotion::LineFeed),
                        snapshot(s, p),
                    )),
                    b'M' => out.push((
                        FrostStateEffect::Vertical(VerticalMotion::ReverseIndex),
                        snapshot(s, p),
                    )),
                    b'H' => s.cursor_controls().set_tab_stop(true),
                    b'c' => out.push((FrostStateEffect::HardReset, snapshot(s, p))),
                    b'>' => p.modes_mut().remove(&66),
                    b'=' => p.modes_mut().insert(66),
                    _ => {}
                }
            } else {
                match (intermediates, final_byte) {
                    (b"#", b'8') => out.push((FrostStateEffect::AlignmentTest, snapshot(s, p))),
                    (b"(" | b")", designator) => {
                        let charset = match designator {
                            b'0' => Charset::DecSpecialGraphics,
                            _ => Charset::Ascii,
                        };
                        out.push((
                            FrostStateEffect::CharsetNotice {
                                bank: intermediates[0],
                                designator,
                                charset,
                            },
                            snapshot(s, p),
                        ));
                        if intermediates == b"(" {
                            s.g0_charset = charset;
                            s.active_charset = s.g0_charset;
                        } else {
                            s.g1_charset = charset;
                        }
                    }
                    _ => {}
                }
            }
        } else {
            match byte {
                8 => s.cursor_controls().move_backward(1),
                9 => s.cursor_controls().tab_forward(1),
                10..=12 => out.push((
                    FrostStateEffect::Vertical(VerticalMotion::LineFeed),
                    snapshot(s, p),
                )),
                13 => s.cursor_controls().set_column(1),
                14 => s.active_charset = s.g1_charset,
                15 => s.active_charset = s.g0_charset,
                _ => {}
            }
        }
        out
    }
    fn actual(
        s: &mut ScreenState,
        p: &mut ProtocolState,
        mut command: FrostStateCommand,
    ) -> Vec<(FrostStateEffect, String)> {
        let mut out = vec![];
        while let Some(effect) = command.step(s, p) {
            out.push((effect, snapshot(s, p)));
        }
        out
    }
    #[test]
    fn all_control_and_escape_bytes_match_frozen_frost_state_and_hook_order() {
        let mut cases = 0;
        let mut new_shift_cases = 0;
        for bits in 0..16 {
            for byte in 0..=255 {
                let (mut a, mut ap) = seed(bits);
                let (mut e, mut ep) = seed(bits);
                assert_eq!(
                    actual(&mut a, &mut ap, FrostStateCommand::control(byte)),
                    frozen(&mut e, &mut ep, None, byte)
                );
                assert_eq!(snapshot(&a, &ap), snapshot(&e, &ep));
                cases += 1;
                for intermediates in [
                    &b""[..],
                    b"#",
                    b"(",
                    b")",
                    b"*",
                    b"+",
                    b"( %",
                    b"( ",
                    b"%",
                    b"##",
                ] {
                    // LS2/LS3 now invoke supported G2/G3 instead of the frozen
                    // legacy no-op. Actual scanners cover these transitions.
                    if intermediates.is_empty() && matches!(byte, b'n' | b'o') {
                        let (mut a, mut ap) = seed(bits);
                        let (mut expected, ep) = seed(bits);
                        assert!(actual(
                            &mut a,
                            &mut ap,
                            FrostStateCommand::escape(intermediates, byte)
                        )
                        .is_empty());
                        expected.active_charset = Charset::Ascii;
                        assert_eq!(snapshot(&a, &ap), snapshot(&expected, &ep));
                        assert_eq!(a.invoked_charset_bank(), if byte == b'n' { 2 } else { 3 });
                        new_shift_cases += 1;
                        continue;
                    }
                    let (mut a, mut ap) = seed(bits);
                    let (mut e, mut ep) = seed(bits);
                    assert_eq!(
                        actual(
                            &mut a,
                            &mut ap,
                            FrostStateCommand::escape(intermediates, byte)
                        ),
                        frozen(&mut e, &mut ep, Some((intermediates, byte)), 0)
                    );
                    assert_eq!(snapshot(&a, &ap), snapshot(&e, &ep));
                    cases += 1;
                }
            }
        }
        assert_eq!(cases, 45024);
        assert_eq!(new_shift_cases, 32);
    }
    #[test]
    fn designation_notice_precedes_commit_and_unknown_sequences_leave_pending_wrap() {
        let (mut s, mut p) = seed(1);
        let mut op = FrostStateCommand::escape(b"(", b'B');
        assert!(matches!(
            op.step(&mut s, &mut p),
            Some(FrostStateEffect::CharsetNotice { .. })
        ));
        assert_eq!(s.active_charset, Charset::DecSpecialGraphics);
        assert!(op.step(&mut s, &mut p).is_none());
        assert_eq!(s.active_charset, Charset::Ascii);
        assert!(s.pending_wrap);
        for mut op in [
            FrostStateCommand::control(7),
            FrostStateCommand::escape(b"", b'<'),
            FrostStateCommand::escape(b"( %", b'0'),
        ] {
            let before = snapshot(&s, &p);
            assert!(op.step(&mut s, &mut p).is_none());
            assert_eq!(before, snapshot(&s, &p));
        }
    }
}
