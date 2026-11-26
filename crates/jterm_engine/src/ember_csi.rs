//! Ember's post-local-dispatch CSI fallback. Admission stays explicitly Ember;
//! physical transaction execution is shared with the FrostLegacyV1 consumer.
use crate::csi_transaction::{CsiTransaction, CsiTransactionEffect};
use crate::display_erase::{prepare_display_erase, DisplayEraseProfile};
use crate::graphic_write::{GraphicProfile, RepeatGraphics};
use crate::mode_mutation::{prepare_native_mode_command, NativeModeProfile};
use crate::protocol_state::ProtocolState;
use crate::region_ops::{prepare_region_operation, RegionOpsProfile};
use crate::screen::ScreenState;
use crate::RawRowId;

#[expect(
    clippy::large_enum_variant,
    reason = "the host consumes the CSI transaction synchronously without boxing each effect"
)]
pub enum EmberCsiEffect<'a> {
    Transaction(CsiTransactionEffect),
    Window(&'a [u16]),
    Reply(Vec<u8>),
    VersionReply,
    /// The original inner dispatcher postamble occurs exactly once even for
    /// ignored/empty commands, in addition to the outer feed's observation.
    Completed,
}
pub trait EmberCsiHost {
    fn csi_state(&mut self) -> (&mut ScreenState, &mut ProtocolState, &mut u64);
    fn csi_effect(&mut self, effect: EmberCsiEffect<'_>);
    fn focused(&self) -> bool;
    fn allow_alt_archive(&self) -> bool;
    fn color_scheme_report(&self) -> u8;
    fn graphic_width(ch: char) -> usize;
    fn allocate_row_id(next: &mut u64) -> RawRowId;
}
pub fn execute<H: EmberCsiHost>(
    host: &mut H,
    params: &[u16],
    command: char,
    prefix: Option<u8>,
    intermediates: &[u8],
) {
    execute_inner(host, params, command, prefix, intermediates);
    host.csi_effect(EmberCsiEffect::Completed);
}
fn execute_inner<H: EmberCsiHost>(
    host: &mut H,
    params: &[u16],
    command: char,
    prefix: Option<u8>,
    intermediates: &[u8],
) {
    let transaction = if let Some(op) = prepare_display_erase(
        DisplayEraseProfile::Ember,
        host.csi_state().0,
        params,
        command,
        prefix,
        intermediates,
    ) {
        Some(CsiTransaction::Erase(op))
    } else if let Some(op) =
        prepare_region_operation(RegionOpsProfile::Ember, host.csi_state().0, params, command)
    {
        Some(CsiTransaction::Region(op))
    } else if let Some(op) =
        prepare_native_mode_command(NativeModeProfile::Ember, params, command, prefix)
    {
        Some(CsiTransaction::Mode(op))
    } else {
        RepeatGraphics::prepare(host.csi_state().0, params, command).map(|repeat| {
            CsiTransaction::Repeat {
                repeat,
                graphic: None,
                profile: GraphicProfile::Ember,
            }
        })
    };
    if let Some(mut transaction) = transaction {
        loop {
            let focused = host.focused();
            let allow = host.allow_alt_archive();
            let (screen, protocol, next) = host.csi_state();
            let Some(effect) =
                transaction.step(screen, protocol, focused, allow, H::graphic_width, || {
                    H::allocate_row_id(next)
                })
            else {
                break;
            };
            host.csi_effect(EmberCsiEffect::Transaction(effect));
        }
        return;
    }
    match command {
        't' if prefix.is_none() && intermediates.is_empty() => {
            host.csi_effect(EmberCsiEffect::Window(params))
        }
        // Ember historically admits this report even with intermediates.
        'n' if prefix == Some(b'?') && params.first() == Some(&996) => {
            let response = format!("\x1b[?997;{}n", host.color_scheme_report());
            host.csi_effect(EmberCsiEffect::Reply(response.into_bytes()));
        }
        'q' if prefix == Some(b'>')
            && intermediates.is_empty()
            && params.first().copied().unwrap_or(0) == 0 =>
        {
            host.csi_effect(EmberCsiEffect::VersionReply)
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csi_transaction::CsiTransactionEffect;
    use crate::mode_mutation::NativeModeEffect;
    struct Host {
        screen: ScreenState,
        protocol: ProtocolState,
        next: u64,
        trace: Vec<String>,
        replies: Vec<u8>,
        focused: bool,
    }
    impl Host {
        fn new() -> Self {
            Self {
                screen: ScreenState::new(3, 7),
                protocol: ProtocolState::default(),
                next: 10,
                trace: vec![],
                replies: vec![],
                focused: true,
            }
        }
    }
    impl EmberCsiHost for Host {
        fn csi_state(&mut self) -> (&mut ScreenState, &mut ProtocolState, &mut u64) {
            (&mut self.screen, &mut self.protocol, &mut self.next)
        }
        fn focused(&self) -> bool {
            self.focused
        }
        fn allow_alt_archive(&self) -> bool {
            false
        }
        fn color_scheme_report(&self) -> u8 {
            2
        }
        fn graphic_width(_: char) -> usize {
            1
        }
        fn allocate_row_id(next: &mut u64) -> RawRowId {
            if *next == 0 {
                return RawRowId::UNTRACKED;
            }
            let id = RawRowId::new(*next);
            *next = next.checked_add(1).unwrap_or(0);
            id
        }
        fn csi_effect(&mut self, effect: EmberCsiEffect<'_>) {
            match effect {
                EmberCsiEffect::Transaction(effect) => match effect {
                    CsiTransactionEffect::Region(_) => self.trace.push("region".into()),
                    CsiTransactionEffect::Mode(effect) => {
                        self.trace.push("mode".into());
                        if let NativeModeEffect::Reply(bytes) = effect {
                            self.replies.extend(bytes);
                        }
                    }
                    CsiTransactionEffect::Erase(_) => self.trace.push("erase".into()),
                    CsiTransactionEffect::EraseScreen => self.trace.push("erase-screen".into()),
                    CsiTransactionEffect::EraseSavedLines => self.trace.push("erase-saved".into()),
                    CsiTransactionEffect::Graphic(_) => self.trace.push("graphic".into()),
                },
                EmberCsiEffect::Reply(bytes) => {
                    self.trace.push("reply".into());
                    self.replies.extend(bytes);
                }
                EmberCsiEffect::Window(_) => self.trace.push("window".into()),
                EmberCsiEffect::VersionReply => self.trace.push("version".into()),
                EmberCsiEffect::Completed => self.trace.push("completed".into()),
            }
        }
    }
    #[test]
    fn ember_admission_and_exactly_once_postamble_remain_distinct() {
        let mut h = Host::new();
        execute(&mut h, &[996], 'n', Some(b'?'), b"!");
        assert_eq!(h.replies, b"\x1b[?997;2n");
        assert_eq!(h.trace, ["reply", "completed"]);
        h.trace.clear();
        execute(&mut h, &[0], 'J', Some(b'>'), b"!");
        assert_eq!(h.trace, ["completed"]);
        h.trace.clear();
        execute(&mut h, &[1004], 'h', None, b"");
        assert_eq!(h.trace, ["mode", "completed"]);
        assert!(!h.protocol.modes().contains(&1004));
        h.trace.clear();
        execute(&mut h, &[1004], 'h', Some(b'?'), b"!");
        assert_eq!(h.trace, ["mode", "completed"]);
        assert!(h.replies.ends_with(b"\x1b[I"));
        h.trace.clear();
        execute(&mut h, &[18], 't', Some(b'?'), b"");
        assert_eq!(h.trace, ["completed"]);
        h.trace.clear();
        execute(&mut h, &[18], 't', None, b"");
        assert_eq!(h.trace, ["window", "completed"]);
    }
    #[test]
    fn shared_region_executor_uses_ember_counter_and_preserves_exhaustion() {
        let mut h = Host::new();
        h.next = u64::MAX;
        execute(&mut h, &[2], 'S', Some(b'='), b"!");
        assert_eq!(h.trace, ["region", "region", "completed"]);
        assert_eq!(h.screen.grid.row_ids()[1], RawRowId::new(u64::MAX));
        assert_eq!(h.screen.grid.row_ids()[2], RawRowId::UNTRACKED);
        assert_eq!(h.next, 0);
    }
}
