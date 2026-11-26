//! FrostLegacyV1 CSI execution. The engine selects and completes each state
//! transaction; the host consumes every effect synchronously before continuation.
use crate::csi_transaction::{CsiTransaction, CsiTransactionEffect};
use crate::display_erase::{prepare_display_erase, DisplayEraseProfile, DisplayEraseStep};
use crate::graphic_write::{GraphicProfile, GraphicStep, RepeatGraphics};
use crate::mode_mutation::{prepare_native_mode_command, NativeModeEffect, NativeModeProfile};
use crate::protocol_state::ProtocolState;
use crate::region_ops::{prepare_region_operation, RegionOpsProfile, RegionStep};
use crate::screen::ScreenState;
use crate::semantics::{dispatch_frost_legacy_csi, Dispatch, Effect};
use crate::RawRowId;

pub enum FrostCsiEffect<'a> {
    ModifierNotice {
        command: char,
        value: u16,
        previous: u16,
    },
    Local {
        effect: Effect,
        command: char,
        prefix: Option<u8>,
    },
    Erase(DisplayEraseStep),
    EraseScreen,
    EraseSavedLines,
    Region(RegionStep),
    Mode(NativeModeEffect),
    Graphic(GraphicStep),
    Window(&'a [u16]),
    Reply(Vec<u8>),
    VersionReply,
}

/// A borrow of the one live state, not a mirrored terminal. Callbacks may finish
/// product bookkeeping but must not feed another event or change geometry.
pub trait FrostCsiHost {
    fn csi_state(&mut self) -> (&mut ScreenState, &mut ProtocolState);
    fn csi_effect(&mut self, effect: FrostCsiEffect<'_>);
    fn allow_alt_archive(&self) -> bool;
    fn background_is_dark(&self) -> bool;
    fn graphic_width(ch: char) -> usize;
}

pub fn execute<H: FrostCsiHost>(
    host: &mut H,
    params: &[u16],
    raw_params: &[u8],
    command: char,
    prefix: Option<u8>,
    intermediates: &[u8],
) {
    let first = params.first().copied();
    if prefix == Some(b'>')
        && intermediates.is_empty()
        && first == Some(4)
        && matches!(command, 'f' | 'm')
    {
        let previous = if command == 'f' {
            host.csi_state().1.xterm_format_other_keys()
        } else {
            host.csi_state().1.xterm_modify_other_keys()
        };
        host.csi_effect(FrostCsiEffect::ModifierNotice {
            command,
            value: params.get(1).copied().unwrap_or(0),
            previous,
        });
    }
    if command.is_ascii() {
        let (screen, protocol) = host.csi_state();
        if let Dispatch::Handled(effects) = dispatch_frost_legacy_csi(
            screen,
            protocol,
            raw_params,
            params,
            command as u8,
            prefix,
            intermediates,
        ) {
            for effect in effects {
                host.csi_effect(FrostCsiEffect::Local {
                    effect,
                    command,
                    prefix,
                });
            }
            return;
        }
    }
    let operation = prepare_display_erase(
        DisplayEraseProfile::FrostLegacy,
        host.csi_state().0,
        params,
        command,
        prefix,
        intermediates,
    );
    if let Some(operation) = operation {
        execute_transaction(host, CsiTransaction::Erase(operation));
        return;
    }
    if let Some(operation) = prepare_region_operation(
        RegionOpsProfile::FrostLegacy,
        host.csi_state().0,
        params,
        command,
    ) {
        execute_transaction(host, CsiTransaction::Region(operation));
        return;
    }
    if let Some(operation) =
        prepare_native_mode_command(NativeModeProfile::FrostLegacy, params, command, prefix)
    {
        execute_transaction(host, CsiTransaction::Mode(operation));
        return;
    }
    if let Some(repeat) = RepeatGraphics::prepare(host.csi_state().0, params, command) {
        execute_transaction(
            host,
            CsiTransaction::Repeat {
                repeat,
                graphic: None,
                profile: GraphicProfile::FrostLegacy,
            },
        );
        return;
    }
    match command {
        't' if prefix.is_none() && intermediates.is_empty() => {
            host.csi_effect(FrostCsiEffect::Window(params))
        }
        'n' if prefix == Some(b'?') && intermediates.is_empty() && first == Some(996) => {
            let scheme = if host.background_is_dark() { 1 } else { 2 };
            host.csi_effect(FrostCsiEffect::Reply(
                format!("\x1b[?997;{scheme}n").into_bytes(),
            ));
        }
        'q' if prefix == Some(b'>') && intermediates.is_empty() && first.unwrap_or(0) == 0 => {
            host.csi_effect(FrostCsiEffect::VersionReply)
        }
        _ => {}
    }
}
fn execute_transaction<H: FrostCsiHost>(host: &mut H, mut transaction: CsiTransaction<'_>) {
    loop {
        let allow = host.allow_alt_archive();
        let (screen, protocol) = host.csi_state();
        let Some(effect) = transaction.step(
            screen,
            protocol,
            false,
            allow,
            H::graphic_width,
            RawRowId::allocate_process_unique,
        ) else {
            break;
        };
        host.csi_effect(match effect {
            CsiTransactionEffect::Erase(step) => FrostCsiEffect::Erase(step),
            CsiTransactionEffect::EraseScreen => FrostCsiEffect::EraseScreen,
            CsiTransactionEffect::EraseSavedLines => FrostCsiEffect::EraseSavedLines,
            CsiTransactionEffect::Region(step) => FrostCsiEffect::Region(step),
            CsiTransactionEffect::Mode(step) => FrostCsiEffect::Mode(step),
            CsiTransactionEffect::Graphic(step) => FrostCsiEffect::Graphic(step),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screen::VerticalMotion;
    struct Host {
        screen: ScreenState,
        protocol: ProtocolState,
        trace: Vec<String>,
        replies: Vec<u8>,
        dark: bool,
    }
    impl Host {
        fn new() -> Self {
            Self {
                screen: ScreenState::new(3, 7),
                protocol: ProtocolState::default(),
                trace: vec![],
                replies: vec![],
                dark: true,
            }
        }
    }
    impl FrostCsiHost for Host {
        fn csi_state(&mut self) -> (&mut ScreenState, &mut ProtocolState) {
            (&mut self.screen, &mut self.protocol)
        }
        fn allow_alt_archive(&self) -> bool {
            false
        }
        fn background_is_dark(&self) -> bool {
            self.dark
        }
        fn graphic_width(_: char) -> usize {
            1
        }
        fn csi_effect(&mut self, effect: FrostCsiEffect<'_>) {
            match effect {
                FrostCsiEffect::ModifierNotice {
                    command,
                    value,
                    previous,
                } => self
                    .trace
                    .push(format!("modifier:{command}:{value}:{previous}")),
                FrostCsiEffect::Local { effect, .. } => match effect {
                    Effect::Reply(bytes) => {
                        self.trace.push("local-reply".into());
                        self.replies.extend(bytes);
                    }
                    Effect::Row(_) => self.trace.push("local-row".into()),
                    _ => self.trace.push("local-other".into()),
                },
                FrostCsiEffect::Erase(step) => self.trace.push(
                    match step {
                        DisplayEraseStep::ScreenErased => "screen-erased",
                        _ => "erase",
                    }
                    .into(),
                ),
                FrostCsiEffect::EraseScreen => self.trace.push("erase-screen".into()),
                FrostCsiEffect::EraseSavedLines => self.trace.push("erase-saved".into()),
                FrostCsiEffect::Region(_) => self.trace.push("region".into()),
                FrostCsiEffect::Mode(_) => self.trace.push("mode".into()),
                FrostCsiEffect::Graphic(step) => match step {
                    GraphicStep::Wrap => {
                        self.trace.push("wrap".into());
                        self.screen.vertical_motion(
                            VerticalMotion::Wrap,
                            false,
                            RawRowId::allocate_process_unique,
                        );
                    }
                    GraphicStep::Written { .. } => self.trace.push("graphic".into()),
                    _ => self.trace.push("graphic-other".into()),
                },
                FrostCsiEffect::Window(_) => self.trace.push("window".into()),
                FrostCsiEffect::Reply(bytes) => {
                    self.trace.push("reply".into());
                    self.replies.extend(bytes);
                }
                FrostCsiEffect::VersionReply => self.trace.push("version".into()),
            }
        }
    }
    #[test]
    fn actual_row_mutations_and_rep_steps_are_executed_before_each_callback() {
        let mut h = Host::new();
        h.screen.cursor_row = 0;
        h.screen.cursor_col = 0;
        execute(&mut h, &[0], b"", 'J', Some(b'>'), b"!");
        assert_eq!(h.trace, ["screen-erased", "erase"]);
        h.trace.clear();
        h.screen.last_printed_char = Some('q');
        h.screen.cursor_col = 6;
        h.protocol.modes_mut().insert(7);
        execute(&mut h, &[3], b"", 'b', Some(b'?'), b"!");
        assert_eq!(h.trace, ["graphic", "wrap", "graphic", "graphic"]);
        assert_eq!(h.screen.grid[0][6].character, 'q');
        assert_eq!(h.screen.grid[1][0].character, 'q');
        assert_eq!(h.screen.grid[1][1].character, 'q');
        h.trace.clear();
        execute(&mut h, &[2], b"", 'L', Some(b'='), b"!");
        assert_eq!(h.trace, ["region"]);
    }
    #[test]
    fn query_admission_and_modifier_notice_keep_legacy_namespace_and_wire_order() {
        let mut h = Host::new();
        execute(&mut h, &[4, 2], b"", 'm', Some(b'>'), b"");
        assert_eq!(h.trace[0], "modifier:m:2:0");
        assert_eq!(h.protocol.xterm_modify_other_keys(), 2);
        h.trace.clear();
        execute(&mut h, &[996], b"", 'n', Some(b'?'), b"!");
        assert!(h.trace.is_empty());
        execute(&mut h, &[996], b"", 'n', Some(b'?'), b"");
        assert_eq!(h.replies, b"\x1b[?997;1n");
        execute(&mut h, &[18], b"", 't', Some(b'?'), b"");
        assert_eq!(h.trace, ["reply"]);
        execute(&mut h, &[18], b"", 't', None, b"");
        execute(&mut h, &[0], b"", 'q', Some(b'>'), b"");
        assert_eq!(h.trace, ["reply", "window", "version"]);
    }
}
