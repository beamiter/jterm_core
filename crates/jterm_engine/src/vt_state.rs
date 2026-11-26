//! Shared authoritative VT storage and scanner entrypoint for hosted and native
//! profiles. Reacquire it after callbacks: Ember RIS may replace the whole owner.
use crate::{
    ascii_write::{NativeAsciiStep, NativeAsciiWrite},
    ember_csi::EmberCsiHost,
    feed::{drive_feed, FeedError, FeedStep, FeedTail},
    frost_controls::{FrostStateCommand, FrostStateEffect},
    frost_csi::FrostCsiHost,
    graphic_write::{GraphicProfile, GraphicStep, GraphicWrite},
    parser::{CaptureEnd, Event, Parser, RejectReason, StringKind},
    protocol_state::ProtocolState,
    screen::{Charset, ScreenState, VerticalMotion},
    semantics::{self, Dispatch, Effects},
    synchronized_output::{SyncProfile, SynchronizedOutput},
    HyperlinkId,
};
use std::borrow::Cow;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeProfile {
    Ember,
    FrostLegacy,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VtProfile {
    Hosted,
    Native(NativeProfile),
}
pub struct VtState {
    profile: VtProfile,
    pub screen: ScreenState,
    pub protocol: ProtocolState,
    pub parser: Parser,
    pub sync_output: SynchronizedOutput,
}
pub enum NativeEffect<'a> {
    AbandonApc,
    Local(Effects),
    Graphic(GraphicStep),
    Ascii(NativeAsciiStep),
    Vertical(VerticalMotion),
    CursorReposition {
        non_linear: bool,
    },
    Reset,
    Alignment,
    FrostState(FrostStateEffect),
    Osc {
        payload: &'a [u8],
        bel_terminated: bool,
        packet_end: Option<u64>,
    },
    KittyApc(&'a [u8]),
    RejectApc {
        recovery: &'a [u8],
        reason: RejectReason,
    },
    ObserveTakeover,
    CaptureBytes(Cow<'a, [u8]>),
}
pub trait NativeTerminalHost {
    fn native_terminal(&mut self) -> &mut VtState;
    fn native_effect(&mut self, effect: NativeEffect<'_>);
    fn idle_capture_active(&self) -> bool {
        false
    }
}
impl VtState {
    /// Adopt the already-profiled grids and parser without copying or creating
    /// a shadow. Allocator identity and parser limits remain explicit inputs.
    pub fn adopt(
        profile: NativeProfile,
        screen: ScreenState,
        protocol: ProtocolState,
        parser: Parser,
    ) -> Self {
        Self {
            profile: VtProfile::Native(profile),
            screen,
            protocol,
            parser,
            sync_output: SynchronizedOutput::new(match profile {
                NativeProfile::Ember => SyncProfile::Ember,
                NativeProfile::FrostLegacy => SyncProfile::FrostLegacy,
            }),
        }
    }
    /// Hosted and native consumers share the same physical parser/screen/mode
    /// storage. Unicode/history/reply policies remain their explicit consumers.
    pub fn adopt_hosted(screen: ScreenState, protocol: ProtocolState, parser: Parser) -> Self {
        Self {
            profile: VtProfile::Hosted,
            screen,
            protocol,
            parser,
            sync_output: SynchronizedOutput::new(SyncProfile::Hosted),
        }
    }
    /// One state-owned scanner entrypoint for all profiles. Reacquire the
    /// live owner between effects so RIS can replace it without a parser move.
    pub fn drive<H, E>(
        host: &mut H,
        input: &[u8],
        mut owner: impl for<'a> FnMut(&'a mut H) -> &'a mut Self,
        dispatch: impl FnMut(&mut H, FeedStep<'_>) -> Result<(), E>,
    ) -> Result<(), FeedError<E>> {
        let tail = match owner(host).profile {
            VtProfile::Hosted => FeedTail::InputConsumed,
            _ => FeedTail::DrainPending,
        };
        drive_feed(
            host,
            input,
            tail,
            |h| {
                let state = owner(h);
                if state.profile == VtProfile::Native(NativeProfile::FrostLegacy) {
                    state.parser.set_ascii_batching(
                        state.screen.active_charset == Charset::Ascii && !state.screen.insert_mode,
                    );
                }
                &mut state.parser
            },
            dispatch,
        )
    }
    pub fn feed_ember<H: NativeTerminalHost + EmberCsiHost>(
        host: &mut H,
        input: &[u8],
        stream_start: Option<u64>,
    ) {
        assert_eq!(
            host.native_terminal().profile,
            VtProfile::Native(NativeProfile::Ember)
        );
        Self::drive(
            host,
            input,
            |h| h.native_terminal(),
            |h, step| {
                if step.abandoned_apc {
                    h.native_effect(NativeEffect::AbandonApc);
                }
                if step.rejected_resource {
                    h.native_terminal().screen.current_hyperlink = HyperlinkId::NONE;
                }
                if let Some(event) = step.event {
                    Self::dispatch_ember(
                        h,
                        event,
                        stream_start.map(|start| start + step.end_offset as u64),
                    );
                }
                Ok::<_, std::convert::Infallible>(())
            },
        )
        .expect("terminal scanner must make progress");
    }
    fn dispatch_ember<H: NativeTerminalHost + EmberCsiHost>(
        h: &mut H,
        event: Event<'_>,
        packet_end: Option<u64>,
    ) {
        let terminal = h.native_terminal();
        if let Dispatch::Handled(effects) =
            semantics::dispatch_local(&mut terminal.screen, &mut terminal.protocol, &event)
        {
            h.native_effect(NativeEffect::Local(effects));
            h.native_effect(NativeEffect::ObserveTakeover);
            return;
        }
        match event {
            Event::Print(ch) => Self::write_ember(h, ch),
            Event::Ascii(bytes) => {
                if h.native_terminal().screen.active_charset == Charset::Ascii {
                    Self::ascii_ember(h, bytes);
                } else {
                    for &byte in bytes {
                        Self::write_ember(h, byte as char);
                        h.native_effect(NativeEffect::ObserveTakeover);
                    }
                }
            }
            Event::Control(10..=12) => {
                h.native_effect(NativeEffect::Vertical(VerticalMotion::LineFeed))
            }
            Event::Control(_) | Event::Charset { .. } | Event::Boundary => {}
            Event::Escape {
                intermediates,
                final_byte,
            } => {
                if intermediates.is_empty() {
                    match final_byte {
                        b'c' => h.native_effect(NativeEffect::Reset),
                        b'M' => {
                            h.native_effect(NativeEffect::Vertical(VerticalMotion::ReverseIndex))
                        }
                        b'D' => h.native_effect(NativeEffect::Vertical(VerticalMotion::LineFeed)),
                        b'E' => {
                            h.native_terminal().screen.begin_next_line();
                            h.native_effect(NativeEffect::CursorReposition { non_linear: false });
                            h.native_effect(NativeEffect::Vertical(VerticalMotion::LineFeed));
                        }
                        _ => {}
                    }
                } else if intermediates == b"#" && final_byte == b'8' {
                    h.native_effect(NativeEffect::Alignment);
                }
            }
            Event::Csi {
                params,
                private_prefix,
                intermediates,
                final_byte,
                ..
            } => crate::ember_csi::execute(
                h,
                &params,
                final_byte as char,
                private_prefix,
                &intermediates,
            ),
            Event::String {
                kind,
                payload,
                bel_terminated,
            } => match kind {
                StringKind::Osc => h.native_effect(NativeEffect::Osc {
                    payload: &payload,
                    bel_terminated,
                    packet_end,
                }),
                StringKind::Apc => h.native_effect(NativeEffect::KittyApc(&payload)),
                StringKind::Dcs | StringKind::Sos | StringKind::Pm => {}
            },
            Event::RejectApc { recovery, reason } => h.native_effect(NativeEffect::RejectApc {
                recovery: &recovery,
                reason,
            }),
        }
        h.native_effect(NativeEffect::ObserveTakeover);
    }
    pub fn feed_frost<H: NativeTerminalHost + FrostCsiHost>(
        host: &mut H,
        input: &[u8],
        stream_start: Option<u64>,
    ) {
        assert_eq!(
            host.native_terminal().profile,
            VtProfile::Native(NativeProfile::FrostLegacy)
        );
        let mut capture_active = false;
        Self::drive(
            host,
            input,
            |h| h.native_terminal(),
            |h, step| {
                if step.abandoned_apc {
                    h.native_effect(NativeEffect::AbandonApc);
                }
                if step.rejected_resource {
                    h.native_terminal().screen.current_hyperlink = HyperlinkId::NONE;
                }
                if step.capture.as_ref().is_some_and(|c| c.started) {
                    capture_active = h.idle_capture_active();
                }
                if let Some(event) = step.event {
                    Self::dispatch_frost(
                        h,
                        event,
                        stream_start.map(|start| start + step.end_offset as u64),
                    );
                }
                if let Some(end) = step.capture.and_then(|c| c.end) {
                    if let CaptureEnd::Complete { token, escape } = end {
                        if escape {
                            h.native_effect(NativeEffect::ObserveTakeover);
                        }
                        if capture_active && h.idle_capture_active() {
                            if let Some(token) = token {
                                let raw = if token.prefix.is_empty() {
                                    Cow::Borrowed(&input[token.input_range])
                                } else {
                                    let mut raw = token.prefix;
                                    raw.extend_from_slice(&input[token.input_range]);
                                    Cow::Owned(raw)
                                };
                                h.native_effect(NativeEffect::CaptureBytes(raw));
                            }
                        }
                    }
                    capture_active = false;
                }
                Ok::<_, std::convert::Infallible>(())
            },
        )
        .expect("terminal scanner must make progress");
    }
    fn dispatch_frost<H: NativeTerminalHost + FrostCsiHost>(
        h: &mut H,
        event: Event<'_>,
        packet_end: Option<u64>,
    ) {
        match event {
            Event::Boundary => {}
            Event::Print(ch) => Self::write_frost(h, ch, true),
            Event::Ascii(bytes) => {
                let t = h.native_terminal();
                if t.screen.active_charset == Charset::Ascii && !t.screen.insert_mode {
                    Self::ascii_frost(h, bytes);
                } else {
                    for &byte in bytes {
                        Self::write_frost(h, byte as char, true);
                    }
                }
            }
            Event::Control(byte) => Self::frost_control(h, FrostStateCommand::control(byte)),
            Event::Escape {
                intermediates,
                final_byte,
            } => Self::frost_control(h, FrostStateCommand::escape(&intermediates, final_byte)),
            Event::Charset { .. } => unreachable!("Frost uses the nF charset scanner"),
            Event::Csi {
                raw_params,
                params,
                private_prefix,
                intermediates,
                final_byte,
                ..
            } => crate::frost_csi::execute(
                h,
                &params,
                &raw_params,
                final_byte as char,
                private_prefix,
                &intermediates,
            ),
            Event::String {
                kind,
                payload,
                bel_terminated,
            } => match kind {
                StringKind::Osc => h.native_effect(NativeEffect::Osc {
                    payload: &payload,
                    bel_terminated,
                    packet_end,
                }),
                StringKind::Apc if crate::kitty_graphics::is_graphics_payload(&payload) => {
                    h.native_effect(NativeEffect::KittyApc(&payload))
                }
                StringKind::Apc | StringKind::Dcs | StringKind::Sos | StringKind::Pm => {}
            },
            Event::RejectApc { recovery, reason } => h.native_effect(NativeEffect::RejectApc {
                recovery: &recovery,
                reason,
            }),
        }
    }
    pub fn frost_control<H: NativeTerminalHost>(h: &mut H, mut command: FrostStateCommand) {
        loop {
            let t = h.native_terminal();
            let Some(effect) = command.step(&mut t.screen, &mut t.protocol) else {
                break;
            };
            h.native_effect(NativeEffect::FrostState(effect));
        }
    }
    pub fn write_ember<H: NativeTerminalHost + EmberCsiHost>(h: &mut H, ch: char) {
        Self::write(
            h,
            ch,
            GraphicProfile::Ember,
            false,
            <H as EmberCsiHost>::graphic_width,
        );
    }
    pub fn write_frost<H: NativeTerminalHost + FrostCsiHost>(h: &mut H, ch: char, track: bool) {
        Self::write(
            h,
            ch,
            GraphicProfile::FrostLegacy,
            track,
            <H as FrostCsiHost>::graphic_width,
        );
    }
    fn write<H: NativeTerminalHost>(
        h: &mut H,
        ch: char,
        profile: GraphicProfile,
        track: bool,
        width: fn(char) -> usize,
    ) {
        let t = h.native_terminal();
        let op = GraphicWrite::new(
            profile,
            &t.screen,
            ch,
            t.protocol.modes().contains(&7),
            track,
            width,
        );
        Self::graphic(h, op);
    }
    pub fn graphic<H: NativeTerminalHost>(h: &mut H, mut op: GraphicWrite) {
        while let Some(effect) = op.step(&mut h.native_terminal().screen) {
            h.native_effect(NativeEffect::Graphic(effect));
        }
    }
    pub fn ascii_ember<H: NativeTerminalHost + EmberCsiHost>(h: &mut H, bytes: &[u8]) {
        Self::ascii(
            h,
            bytes,
            GraphicProfile::Ember,
            <H as EmberCsiHost>::graphic_width,
        );
    }
    pub fn ascii_frost<H: NativeTerminalHost + FrostCsiHost>(h: &mut H, bytes: &[u8]) {
        Self::ascii(
            h,
            bytes,
            GraphicProfile::FrostLegacy,
            <H as FrostCsiHost>::graphic_width,
        );
    }
    fn ascii<H: NativeTerminalHost>(
        h: &mut H,
        bytes: &[u8],
        profile: GraphicProfile,
        width: fn(char) -> usize,
    ) {
        let t = h.native_terminal();
        let wrap = t.protocol.modes().contains(&7);
        let mut op = NativeAsciiWrite::new(profile, &mut t.screen, bytes, wrap);
        loop {
            let t = h.native_terminal();
            let Some(effect) = op.step(&mut t.screen, t.protocol.modes().contains(&7), width)
            else {
                break;
            };
            match effect {
                NativeAsciiStep::Graphic(write) => Self::graphic(h, write),
                effect => h.native_effect(NativeEffect::Ascii(effect)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ember_csi::{EmberCsiEffect, EmberCsiHost},
        frost_csi::{FrostCsiEffect, FrostCsiHost},
        RawRowId,
    };
    struct Host {
        vt: VtState,
        next: u64,
        trace: Vec<String>,
        raw: Vec<u8>,
    }
    impl Host {
        fn new(profile: NativeProfile) -> Self {
            let parser = Parser::with_bounded_profile(
                crate::parser::ParserLimits {
                    pending_bytes: 4096,
                    kitty_recovery_bytes: 1024,
                },
                match profile {
                    NativeProfile::Ember => crate::parser::ParserProfile::EmberV1,
                    NativeProfile::FrostLegacy => crate::parser::ParserProfile::FrostLegacyV1,
                },
            )
            .unwrap();
            Self {
                vt: VtState::adopt(
                    profile,
                    ScreenState::new(3, 12),
                    ProtocolState::default(),
                    parser,
                ),
                next: 1,
                trace: vec![],
                raw: vec![],
            }
        }
    }
    impl NativeTerminalHost for Host {
        fn native_terminal(&mut self) -> &mut VtState {
            &mut self.vt
        }
        fn idle_capture_active(&self) -> bool {
            true
        }
        fn native_effect(&mut self, effect: NativeEffect<'_>) {
            match effect {
                NativeEffect::Reset => {
                    self.trace.push("reset".into());
                    self.vt = Self::new(NativeProfile::Ember).vt;
                }
                NativeEffect::CaptureBytes(bytes) => self.raw.extend_from_slice(&bytes),
                NativeEffect::ObserveTakeover => self.trace.push("observe".into()),
                NativeEffect::Graphic(_) => self.trace.push("graphic".into()),
                NativeEffect::Ascii(_) => self.trace.push("ascii".into()),
                NativeEffect::FrostState(FrostStateEffect::CharsetNotice { .. }) => {
                    self.trace.push("charset".into())
                }
                NativeEffect::Vertical(motion)
                | NativeEffect::FrostState(FrostStateEffect::Vertical(motion)) => {
                    self.vt.screen.vertical_motion(
                        motion,
                        false,
                        RawRowId::allocate_process_unique,
                    );
                }
                _ => {}
            }
        }
    }
    impl EmberCsiHost for Host {
        fn csi_state(&mut self) -> (&mut ScreenState, &mut ProtocolState, &mut u64) {
            (&mut self.vt.screen, &mut self.vt.protocol, &mut self.next)
        }
        fn csi_effect(&mut self, e: EmberCsiEffect<'_>) {
            if matches!(e, EmberCsiEffect::Completed) {
                self.trace.push("csi-end".into());
            }
        }
        fn focused(&self) -> bool {
            false
        }
        fn allow_alt_archive(&self) -> bool {
            false
        }
        fn color_scheme_report(&self) -> u8 {
            1
        }
        fn graphic_width(_: char) -> usize {
            1
        }
        fn allocate_row_id(next: &mut u64) -> RawRowId {
            let id = RawRowId::new(*next);
            *next += 1;
            id
        }
    }
    impl FrostCsiHost for Host {
        fn csi_state(&mut self) -> (&mut ScreenState, &mut ProtocolState) {
            (&mut self.vt.screen, &mut self.vt.protocol)
        }
        fn csi_effect(&mut self, _: FrostCsiEffect<'_>) {}
        fn allow_alt_archive(&self) -> bool {
            false
        }
        fn background_is_dark(&self) -> bool {
            true
        }
        fn graphic_width(_: char) -> usize {
            1
        }
    }
    #[test]
    fn ember_reset_replaces_the_single_owner_midfeed_and_continues_suffix() {
        let mut host = Host::new(NativeProfile::Ember);
        VtState::feed_ember(&mut host, b"old\x1bcXYZ", Some(0));
        assert_eq!(host.vt.screen.grid[0][0].character, 'X');
        assert_eq!(host.vt.screen.grid[0][2].character, 'Z');
        assert_eq!(host.vt.screen.cursor_col, 3);
        assert_eq!(host.trace.iter().filter(|s| *s == "reset").count(), 1);
        assert!(host.vt.parser.snapshot().pending_escape.is_empty());
    }
    #[test]
    fn frost_owner_dispatch_and_capture_preserve_every_input_partition() {
        let bytes = b"a\x1b(0q\x1b(Bz";
        for split in 0..=bytes.len() {
            let mut host = Host::new(NativeProfile::FrostLegacy);
            VtState::feed_frost(&mut host, &bytes[..split], Some(0));
            VtState::feed_frost(&mut host, &bytes[split..], Some(split as u64));
            assert_eq!(host.vt.screen.grid[0][0].character, 'a');
            assert_eq!(host.vt.screen.grid[0][1].character, '─');
            assert_eq!(host.vt.screen.grid[0][2].character, 'z');
            assert_eq!(host.raw, bytes, "split={split}");
            assert_eq!(host.trace.iter().filter(|s| *s == "charset").count(), 2);
        }
    }
    fn state_for(kind: u8) -> VtState {
        let profile = if kind == 2 {
            crate::parser::ParserProfile::FrostLegacyV1
        } else {
            crate::parser::ParserProfile::EmberV1
        };
        let parser = Parser::with_bounded_profile(
            crate::parser::ParserLimits {
                pending_bytes: 32,
                kitty_recovery_bytes: 12,
            },
            profile,
        )
        .unwrap();
        let screen = ScreenState::new(3, 12);
        let protocol = ProtocolState::default();
        match kind {
            0 => VtState::adopt_hosted(screen, protocol, parser),
            1 => VtState::adopt(NativeProfile::Ember, screen, protocol, parser),
            _ => VtState::adopt(NativeProfile::FrostLegacy, screen, protocol, parser),
        }
    }
    fn reset_from_event(state: &mut VtState, event: &Option<Event<'_>>, kind: u8) {
        if matches!(event,Some(Event::Escape {intermediates,final_byte:b'c'}) if intermediates.is_empty())
        {
            *state = state_for(kind);
        }
    }
    #[test]
    fn shared_state_entrypoint_matches_prior_schedulers_for_all_profiles_and_partitions() {
        let bytes = b"abc\x1b[2\n;4H\x1b_Gi=4,m=1;x\x1b\\\x1b_Gi=4;\x18\x1b]8;;\x07\x1bcB\xe0\x80";
        for kind in 0..3 {
            for split in 0..=bytes.len() {
                let mut actual = state_for(kind);
                let mut expected = state_for(kind);
                for chunk in [&bytes[..split], &bytes[split..], b"", b"\x1b[6nZ"] {
                    // Frozen09b parser scheduling: hosted stops at consumed input;
                    // natives drain queued events, Frost updates batching per step.
                    expected.parser.begin_feed();
                    let mut offset = 0;
                    let mut old = vec![];
                    while offset < chunk.len() || kind != 0 {
                        if kind == 2 {
                            expected.parser.set_ascii_batching(
                                expected.screen.active_charset == Charset::Ascii
                                    && !expected.screen.insert_mode,
                            );
                        }
                        let before = expected.parser.rejected_count();
                        let step = expected.parser.next(&chunk[offset..]);
                        offset += step.consumed;
                        old.push((
                            offset,
                            step.abandoned_apc,
                            before != expected.parser.rejected_count(),
                            format!("{:?}", step.event),
                            format!("{:?}", step.capture),
                        ));
                        let done = step.event.is_none();
                        reset_from_event(&mut expected, &step.event, kind);
                        if done {
                            break;
                        }
                    }
                    let mut new = vec![];
                    VtState::drive(
                        &mut actual,
                        chunk,
                        |state| state,
                        |state, step| {
                            new.push((
                                step.end_offset,
                                step.abandoned_apc,
                                step.rejected_resource,
                                format!("{:?}", step.event),
                                format!("{:?}", step.capture),
                            ));
                            reset_from_event(state, &step.event, kind);
                            Ok::<_, std::convert::Infallible>(())
                        },
                    )
                    .unwrap();
                    assert_eq!(new, old, "profile={kind} split={split}");
                    assert_eq!(
                        format!("{:?}", actual.parser.snapshot()),
                        format!("{:?}", expected.parser.snapshot())
                    );
                }
            }
        }
    }
}
