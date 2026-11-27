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
    screen::{ScreenState, VerticalMotion},
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
                        state.screen.ascii_graphics() && !state.screen.insert_mode,
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
        let watch_wheel_profile = h.track_alternate_scroll_changes()
            && (matches!(&event, Event::Csi { private_prefix: Some(b'=' | b'>' | b'<'), final_byte: b'u', intermediates, .. } if intermediates.is_empty())
                || matches!(&event, Event::Csi { private_prefix: None, final_byte: b'p', intermediates, .. } if intermediates == b"!"));
        let terminal = h.native_terminal();
        let previous_wheel_profile = watch_wheel_profile.then(|| {
            crate::mode_mutation::native_alternate_scroll_profile(
                &terminal.screen,
                &terminal.protocol,
            )
        });
        if let Dispatch::Handled(effects) =
            semantics::dispatch_local(&mut terminal.screen, &mut terminal.protocol, &event)
        {
            if previous_wheel_profile.is_some_and(|old| {
                let terminal = h.native_terminal();
                crate::mode_mutation::native_alternate_scroll_profile(
                    &terminal.screen,
                    &terminal.protocol,
                ) != old
            }) {
                h.csi_effect(crate::ember_csi::EmberCsiEffect::Transaction(
                    crate::csi_transaction::CsiTransactionEffect::Mode(
                        crate::mode_mutation::NativeModeEffect::MouseReportingChanged,
                    ),
                ));
            }
            h.native_effect(NativeEffect::Local(effects));
            h.native_effect(NativeEffect::ObserveTakeover);
            return;
        }
        match event {
            Event::Print(ch) => Self::write_ember(h, ch),
            Event::Ascii(bytes) => {
                if h.native_terminal().screen.ascii_graphics() {
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
                        b'6' | b'9' => {
                            let terminal = h.native_terminal();
                            let screen = &mut terminal.screen;
                            let before =
                                (screen.cursor_row, screen.cursor_col, screen.pending_wrap);
                            if let Some(mut columns) = crate::column_edit::prepare_column_index(
                                screen,
                                final_byte == b'6',
                                &terminal.protocol,
                            ) {
                                while let Some(mutation) =
                                    columns.step(&mut h.native_terminal().screen)
                                {
                                    h.csi_effect(crate::ember_csi::EmberCsiEffect::Transaction(
                                        crate::csi_transaction::CsiTransactionEffect::Erase(
                                            crate::display_erase::DisplayEraseStep::Mutations(
                                                smallvec::smallvec![mutation],
                                            ),
                                        ),
                                    ));
                                }
                            }
                            let screen = &h.native_terminal().screen;
                            if before != (screen.cursor_row, screen.cursor_col, screen.pending_wrap)
                            {
                                h.native_effect(NativeEffect::CursorReposition {
                                    non_linear: false,
                                });
                            }
                        }
                        b'Z' => {
                            if let Some(reply) =
                                semantics::legacy_device_attributes_reply(None, b"")
                            {
                                h.native_effect(NativeEffect::Local(smallvec::smallvec![
                                    semantics::Effect::Reply(reply.to_vec()),
                                ]));
                            }
                        }
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
                raw_params,
                private_prefix,
                intermediates,
                final_byte,
                ..
            } => {
                let strict_parameters = (private_prefix == Some(b'?')
                    && matches!(final_byte, b's' | b'r'))
                    || (private_prefix.is_none()
                        && intermediates == b"$"
                        && matches!(final_byte, b'z' | b'{' | b'x' | b'v' | b'r' | b't'))
                    || (private_prefix.is_none()
                        && intermediates == b"'"
                        && matches!(final_byte, b'}' | b'~'))
                    || (private_prefix.is_none()
                        && intermediates == b" "
                        && matches!(final_byte, b'@' | b'A'));
                if !strict_parameters || !raw_params.contains(&b':') {
                    crate::ember_csi::execute(
                        h,
                        &params,
                        final_byte as char,
                        private_prefix,
                        &intermediates,
                    );
                }
            }
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
                if t.screen.ascii_graphics() && !t.screen.insert_mode {
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
                final_byte: b'Z',
            } if intermediates.is_empty() => {
                // Use Frost's primary DA transaction for the older DECID query.
                crate::frost_csi::execute(h, &[], &[], 'c', None, b"");
            }
            Event::Escape {
                intermediates,
                final_byte,
            } if intermediates.is_empty() && matches!(final_byte, b'6' | b'9') => {
                let terminal = h.native_terminal();
                let screen = &mut terminal.screen;
                if let Some(mut columns) = crate::column_edit::prepare_column_index(
                    screen,
                    final_byte == b'6',
                    &terminal.protocol,
                ) {
                    while let Some(mutation) = columns.step(&mut h.native_terminal().screen) {
                        h.csi_effect(crate::frost_csi::FrostCsiEffect::Erase(
                            crate::display_erase::DisplayEraseStep::Mutations(smallvec::smallvec![
                                mutation
                            ]),
                        ));
                    }
                }
            }
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
                StringKind::Dcs if payload.starts_with(b"2$t") => {
                    crate::semantics::restore_tab_stops(h.csi_state().0, &payload);
                }
                StringKind::Dcs if payload.starts_with(b"1$t") => {
                    crate::cursor_report::restore_cursor_information(
                        h.csi_state().0,
                        &payload[3..],
                    );
                }
                StringKind::Dcs if payload.starts_with(b"$q") => {
                    let (screen, protocol) = h.csi_state();
                    let reply = crate::semantics::restorable_decrqss_reply(
                        screen,
                        protocol,
                        &payload[2..],
                        None,
                    );
                    h.csi_effect(crate::frost_csi::FrostCsiEffect::Reply(reply));
                }
                StringKind::Dcs if payload.starts_with(b"+q") => {
                    if let Some(reply) =
                        crate::termcap_query::termcap_query(&payload, h.csi_state().1)
                    {
                        h.csi_effect(crate::frost_csi::FrostCsiEffect::Reply(reply.into_owned()));
                    }
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
        let translated = t.screen.translate_graphic(ch);
        let op = GraphicWrite::new_translated(
            profile,
            ch,
            translated,
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
        mut bytes: &[u8],
        profile: GraphicProfile,
        width: fn(char) -> usize,
    ) {
        if h.native_terminal().screen.single_shift_pending() {
            if let Some((&first, remaining)) = bytes.split_first() {
                let ember = profile == GraphicProfile::Ember;
                Self::write(h, char::from(first), profile, !ember, width);
                if ember {
                    h.native_effect(NativeEffect::ObserveTakeover);
                }
                bytes = remaining;
            }
        }
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
    use crate::screen::Charset;
    use crate::{
        ember_csi::{EmberCsiEffect, EmberCsiHost},
        frost_csi::{FrostCsiEffect, FrostCsiHost},
        RawRowId,
    };

    #[test]
    fn native_rendition_opt_in_preserves_attributes_across_scanner_boundaries() {
        let input = b"\x1b[8;53mAB\x1bP$qm\x1b\\\x1b[28mC\x1b[55mD";
        for profile in [NativeProfile::Ember, NativeProfile::FrostLegacy] {
            let feed = match profile {
                NativeProfile::Ember => VtState::feed_ember::<Host>,
                NativeProfile::FrostLegacy => VtState::feed_frost::<Host>,
            };
            for enabled in [false, true] {
                for split in 0..=input.len() {
                    let mut host = Host::new(profile);
                    if enabled {
                        host.vt.screen.enable_extended_rendition();
                    }
                    feed(&mut host, &input[..split], None);
                    feed(&mut host, &input[split..], None);
                    let row = &host.vt.screen.grid[0];
                    assert_eq!(
                        row[..4].iter().map(|c| c.character).collect::<String>(),
                        "ABCD"
                    );
                    for (index, cell) in row[..4].iter().enumerate() {
                        assert_eq!(cell.flags.concealed(), enabled && index < 2);
                        assert_eq!(cell.flags.overline(), enabled && index < 3);
                    }
                    let reply: &[u8] = match (profile, enabled) {
                        (_, false) => b"\x1bP1$r0m\x1b\\",
                        (NativeProfile::Ember, true) => b"\x1bP1$r8;53m\x1b\\",
                        (NativeProfile::FrostLegacy, true) => b"\x1bP1$r0;8;53m\x1b\\",
                    };
                    assert_eq!(host.replies, reply, "{profile:?} at {split}");
                }
            }
        }
    }

    struct Host {
        vt: VtState,
        next: u64,
        trace: Vec<String>,
        raw: Vec<u8>,
        replies: Vec<u8>,
        erased_rows: Vec<(usize, char)>,
        dirty_rows: Vec<usize>,
        apply_wrap: bool,
        alternate_scroll_tracking: bool,
        wheel_retires: usize,
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
                replies: vec![],
                erased_rows: vec![],
                dirty_rows: vec![],
                apply_wrap: false,
                alternate_scroll_tracking: false,
                wheel_retires: 0,
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
                NativeEffect::Graphic(step) => {
                    self.trace.push("graphic".into());
                    if self.apply_wrap && matches!(step, GraphicStep::Wrap) {
                        self.vt.screen.vertical_motion(
                            VerticalMotion::Wrap,
                            false,
                            RawRowId::allocate_process_unique,
                        );
                    }
                }
                NativeEffect::Ascii(step) => {
                    self.trace.push("ascii".into());
                    if self.apply_wrap && matches!(step, NativeAsciiStep::Wrap) {
                        self.vt.screen.vertical_motion(
                            VerticalMotion::Wrap,
                            false,
                            RawRowId::allocate_process_unique,
                        );
                    }
                }
                NativeEffect::Local(effects) => {
                    assert_eq!(self.vt.profile, VtProfile::Native(NativeProfile::Ember));
                    for effect in effects {
                        if let semantics::Effect::Reply(reply) = effect {
                            self.replies.extend(reply);
                        }
                    }
                }
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
        fn track_alternate_scroll_changes(&self) -> bool {
            self.alternate_scroll_tracking
        }
        fn csi_state(&mut self) -> (&mut ScreenState, &mut ProtocolState, &mut u64) {
            (&mut self.vt.screen, &mut self.vt.protocol, &mut self.next)
        }
        fn csi_effect(&mut self, e: EmberCsiEffect<'_>) {
            if matches!(
                e,
                EmberCsiEffect::Transaction(crate::csi_transaction::CsiTransactionEffect::Mode(
                    crate::mode_mutation::NativeModeEffect::MouseReportingChanged
                ))
            ) {
                self.wheel_retires += 1;
            }
            if let EmberCsiEffect::Transaction(
                crate::csi_transaction::CsiTransactionEffect::Erase(
                    crate::display_erase::DisplayEraseStep::DirtyRows(ref rows),
                ),
            ) = e
            {
                self.dirty_rows.extend(rows.clone());
            }

            if let EmberCsiEffect::Transaction(
                crate::csi_transaction::CsiTransactionEffect::Erase(
                    crate::display_erase::DisplayEraseStep::Mutations(ref mutations),
                ),
            ) = e
            {
                for mutation in mutations {
                    let following = self
                        .vt
                        .screen
                        .grid
                        .get((mutation.row + 1).min(2), 1)
                        .character;
                    self.erased_rows.push((mutation.row, following));
                }
            }
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
        fn csi_effect(&mut self, effect: FrostCsiEffect<'_>) {
            if matches!(
                effect,
                FrostCsiEffect::Mode(crate::mode_mutation::NativeModeEffect::MouseReportingChanged)
            ) {
                self.wheel_retires += 1;
            }
            if let FrostCsiEffect::Erase(crate::display_erase::DisplayEraseStep::DirtyRows(
                ref rows,
            )) = effect
            {
                self.dirty_rows.extend(rows.clone());
            }

            if let FrostCsiEffect::Erase(crate::display_erase::DisplayEraseStep::Mutations(
                ref mutations,
            )) = effect
            {
                for mutation in mutations {
                    let following = self
                        .vt
                        .screen
                        .grid
                        .get((mutation.row + 1).min(2), 1)
                        .character;
                    self.erased_rows.push((mutation.row, following));
                }
            }
            if let FrostCsiEffect::Reply(reply)
            | FrostCsiEffect::Local {
                effect: semantics::Effect::Reply(reply),
                ..
            } = effect
            {
                self.replies.extend(reply);
            }
        }
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
    fn native_rectangle_scanners_preserve_state_and_notify_before_next_row() {
        let packet = b"\x1b[1;2;2;4${";
        for profile in [NativeProfile::Ember, NativeProfile::FrostLegacy] {
            let feed = match profile {
                NativeProfile::Ember => VtState::feed_ember::<Host>,
                NativeProfile::FrostLegacy => VtState::feed_frost::<Host>,
            };
            for split in 0..=packet.len() {
                let mut host = Host::new(profile);
                host.vt.screen.grid.fill_cells(crate::TerminalCell {
                    character: 'R',
                    foreground: crate::Color::Red,
                    ..crate::TerminalCell::default()
                });
                host.vt.screen.cursor_row = 2;
                host.vt.screen.cursor_col = 11;
                host.vt.screen.pending_wrap = true;
                host.vt.screen.grid.row_wrapped[0] = true;
                host.vt.screen.grid.get_mut(0, 2).flags.set_protected(true);
                let ids: Vec<_> = (0..3).map(|r| host.vt.screen.grid.row_id(r)).collect();
                feed(&mut host, &packet[..split], None);
                feed(&mut host, &packet[split..], None);
                assert_eq!(host.vt.screen.grid.get(0, 1).character, ' ');
                assert_eq!(host.vt.screen.grid.get(0, 2).character, 'R');
                assert_eq!(host.vt.screen.grid.get(1, 2).character, ' ');
                assert_eq!(host.vt.screen.grid.get(1, 2).foreground, crate::Color::Red);
                assert!(host
                    .erased_rows
                    .iter()
                    .all(|&(_, following)| following == 'R'));
                assert_eq!(
                    host.erased_rows.iter().map(|&(r, _)| r).collect::<Vec<_>>(),
                    vec![0, 0, 1]
                );
                let before: Vec<_> = (0..3)
                    .flat_map(|r| (0..12).map(move |c| (r, c)))
                    .map(|(r, c)| *host.vt.screen.grid.get(r, c))
                    .collect();
                feed(
                    &mut host,
                    b"\x1b[?1;1;3;12$z\x1b[1:2;1;3;12$z\x1b[1;1;3;12;1$z\x1b[1;1;3;12 $z",
                    None,
                );
                let after: Vec<_> = (0..3)
                    .flat_map(|r| (0..12).map(move |c| (r, c)))
                    .map(|(r, c)| *host.vt.screen.grid.get(r, c))
                    .collect();
                assert_eq!(before, after);
                feed(&mut host, b"\x1b[1;2;1;4$z", None);
                assert_eq!(
                    *host.vt.screen.grid.get(0, 2),
                    crate::TerminalCell::default()
                );
                assert_eq!(
                    (
                        host.vt.screen.cursor_row,
                        host.vt.screen.cursor_col,
                        host.vt.screen.pending_wrap
                    ),
                    (2, 11, true)
                );
                assert!(host.vt.screen.grid.row_wrapped[0]);
                assert_eq!(
                    (0..3)
                        .map(|r| host.vt.screen.grid.row_id(r))
                        .collect::<Vec<_>>(),
                    ids
                );
            }
        }
    }

    #[test]
    fn native_fill_scanners_translate_gl_apply_rendition_and_notify_in_order() {
        let packet = b"\x1b(0\x1b[1;31;44m\x1b[1\"q\x1b[113;1;2;2;4$x";
        for profile in [NativeProfile::Ember, NativeProfile::FrostLegacy] {
            let feed = match profile {
                NativeProfile::Ember => VtState::feed_ember::<Host>,
                NativeProfile::FrostLegacy => VtState::feed_frost::<Host>,
            };
            for split in 0..=packet.len() {
                let mut host = Host::new(profile);
                host.vt.screen.grid.fill_cells(crate::TerminalCell {
                    character: 'R',
                    ..crate::TerminalCell::default()
                });
                host.vt.screen.cursor_row = 2;
                host.vt.screen.cursor_col = 11;
                host.vt.screen.pending_wrap = true;
                feed(&mut host, &packet[..split], None);
                feed(&mut host, &packet[split..], None);
                for row in 0..2 {
                    for col in 1..4 {
                        let cell = host.vt.screen.grid.get(row, col);
                        assert_eq!(cell.character, '─');
                        assert_eq!(
                            (cell.foreground, cell.background),
                            (crate::Color::Red, crate::Color::Blue)
                        );
                        assert!(cell.flags.bold() && cell.flags.protected());
                        assert!(cell.hyperlink_id.is_none());
                    }
                }
                assert_eq!(host.erased_rows, vec![(0, 'R'), (1, 'R')]);
                assert_eq!(
                    (
                        host.vt.screen.cursor_row,
                        host.vt.screen.cursor_col,
                        host.vt.screen.pending_wrap
                    ),
                    (2, 11, true)
                );
                let before: Vec<_> = (0..3)
                    .flat_map(|r| (0..12).map(move |c| (r, c)))
                    .map(|(r, c)| *host.vt.screen.grid.get(r, c))
                    .collect();
                feed(&mut host,b"\x1b[65:1;1;1;3;12$x\x1b[?65;1;1;3;12$x\x1b[65;1;1;3;12;1$x\x1b[65;1;1;3;12 $x",None);
                let after: Vec<_> = (0..3)
                    .flat_map(|r| (0..12).map(move |c| (r, c)))
                    .map(|(r, c)| *host.vt.screen.grid.get(r, c))
                    .collect();
                assert_eq!(before, after);
                feed(&mut host, b"\x1b[2\"q\x1b[65;1;2;1;2$x", None);
                assert_eq!(host.vt.screen.grid.get(0, 1).character, 'A');
                assert!(!host.vt.screen.grid.get(0, 1).flags.protected());
            }
        }
    }

    #[test]
    fn native_copy_scanners_overlap_bottom_up_and_reject_malformed_namespaces() {
        let packet = b"\x1b[1;1;2;4;7;2;2;9$v";
        for profile in [NativeProfile::Ember, NativeProfile::FrostLegacy] {
            let feed = match profile {
                NativeProfile::Ember => VtState::feed_ember::<Host>,
                NativeProfile::FrostLegacy => VtState::feed_frost::<Host>,
            };
            for split in 0..=packet.len() {
                let mut host = Host::new(profile);
                for row in 0..3 {
                    for col in 0..12 {
                        *host.vt.screen.grid.get_mut(row, col) = crate::TerminalCell {
                            character: char::from(b'A' + row as u8),
                            foreground: crate::Color::Red,
                            ..crate::TerminalCell::default()
                        };
                    }
                }
                host.vt.screen.cursor_col = 11;
                host.vt.screen.pending_wrap = true;
                host.vt.screen.grid.row_wrapped[1] = true;
                let ids: Vec<_> = (0..3).map(|r| host.vt.screen.grid.row_id(r)).collect();
                feed(&mut host, &packet[..split], None);
                feed(&mut host, &packet[split..], None);
                for col in 1..5 {
                    assert_eq!(host.vt.screen.grid.get(1, col).character, 'A');
                    assert_eq!(host.vt.screen.grid.get(2, col).character, 'B');
                    assert_eq!(
                        host.vt.screen.grid.get(2, col).foreground,
                        crate::Color::Red
                    );
                }
                assert_eq!(host.vt.screen.grid.get(1, 0).character, 'B');
                assert_eq!(host.vt.screen.grid.get(2, 5).character, 'C');
                assert_eq!(host.erased_rows, vec![(2, 'B'), (1, 'B')]);
                assert_eq!(
                    (
                        host.vt.screen.cursor_row,
                        host.vt.screen.cursor_col,
                        host.vt.screen.pending_wrap
                    ),
                    (0, 11, true)
                );
                assert!(host.vt.screen.grid.row_wrapped[1]);
                assert_eq!(
                    (0..3)
                        .map(|r| host.vt.screen.grid.row_id(r))
                        .collect::<Vec<_>>(),
                    ids
                );
                let before: Vec<_> = (0..3)
                    .flat_map(|r| (0..12).map(move |c| (r, c)))
                    .map(|(r, c)| *host.vt.screen.grid.get(r, c))
                    .collect();
                feed(&mut host,b"\x1b[1:2;1;2;4;1;1;1;1$v\x1b[?1;1;2;4;1;1;1;1$v\x1b[1;1;2;4;1;1;1;1;1$v\x1b[1;1;2;4;1;1;1;1 $v",None);
                let after: Vec<_> = (0..3)
                    .flat_map(|r| (0..12).map(move |c| (r, c)))
                    .map(|(r, c)| *host.vt.screen.grid.get(r, c))
                    .collect();
                assert_eq!(before, after);
                assert_eq!(host.erased_rows.len(), 2);
            }
        }
    }

    #[test]
    fn native_area_sgr_queries_reply_across_all_partitions_without_state_changes() {
        let packet = b"\x1b[1;1;2;4#|";
        for profile in [NativeProfile::Ember, NativeProfile::FrostLegacy] {
            let feed = match profile {
                NativeProfile::Ember => VtState::feed_ember::<Host>,
                NativeProfile::FrostLegacy => VtState::feed_frost::<Host>,
            };
            for split in 0..=packet.len() {
                let mut host = Host::new(profile);
                let mut flags = crate::StyleFlags::default();
                flags.set_bold(true);
                host.vt.screen.grid.fill_cells(crate::TerminalCell {
                    character: 'X',
                    foreground: crate::Color::Red,
                    flags,
                    ..crate::TerminalCell::default()
                });
                host.vt.screen.grid.get_mut(0, 0).flags.set_italic(true);
                host.vt.screen.cursor_row = 2;
                host.vt.screen.cursor_col = 11;
                host.vt.screen.pending_wrap = true;
                host.vt.screen.grid.row_wrapped[0] = true;
                feed(&mut host, &packet[..split], None);
                feed(&mut host, &packet[split..], None);
                assert_eq!(host.replies, b"\x1b[0;1;31m");
                host.replies.clear();
                feed(
                    &mut host,
                    b"\x1b[1:2;1;2;4#|\x1b[?1;1;2;4#|\x1b[1;1;2;4;1#|\x1b[1;1;2;4 #|",
                    None,
                );
                assert!(host.replies.is_empty());
                assert!(host.erased_rows.is_empty());
                assert_eq!(
                    (
                        host.vt.screen.cursor_row,
                        host.vt.screen.cursor_col,
                        host.vt.screen.pending_wrap
                    ),
                    (2, 11, true)
                );
                assert!(host.vt.screen.grid.row_wrapped[0]);
                assert!(host.vt.screen.grid.get(0, 0).flags.italic());
                assert_eq!(host.vt.screen.grid.get(1, 3).character, 'X');
            }
        }
    }

    #[test]
    fn native_attribute_scanners_distinguish_explicit_spaces_and_dirty_only() {
        let packet = b"\x1b[1;1;1;3;1$r\x1b[2*x\x1b[1;2;2;3;4$r\x1b[1;2;1;2;1$t";
        for profile in [NativeProfile::Ember, NativeProfile::FrostLegacy] {
            let feed = match profile {
                NativeProfile::Ember => VtState::feed_ember::<Host>,
                NativeProfile::FrostLegacy => VtState::feed_frost::<Host>,
            };
            for split in 0..=packet.len() {
                let mut host = Host::new(profile);
                feed(&mut host, b"A ", None);
                assert!(host.vt.screen.grid.cell_drawn(0, 1));
                assert!(!host.vt.screen.grid.cell_drawn(0, 2));
                host.vt.screen.cursor_row = 2;
                host.vt.screen.cursor_col = 11;
                host.vt.screen.pending_wrap = true;
                host.vt.screen.grid.row_wrapped[0] = true;
                feed(&mut host, &packet[..split], None);
                feed(&mut host, &packet[split..], None);
                assert!(host.vt.protocol.rectangular_attribute_extent());
                assert_eq!(host.vt.screen.grid.get(0, 0).character, 'A');
                assert!(host.vt.screen.grid.get(0, 0).flags.bold());
                assert!(!host.vt.screen.grid.get(0, 1).flags.bold());
                assert!(!host.vt.screen.grid.get(0, 2).flags.bold());
                for row in 0..2 {
                    for col in 1..3 {
                        assert_eq!(host.vt.screen.grid.get(row, col).character, ' ');
                        assert_eq!(
                            host.vt.screen.grid.get(row, col).flags.underline(),
                            crate::UnderlineStyle::Single
                        );
                        assert!(host.vt.screen.grid.cell_drawn(row, col));
                    }
                }
                assert_eq!(host.dirty_rows, vec![0, 0, 1, 0]);
                assert!(host.erased_rows.is_empty());
                assert_eq!(
                    (
                        host.vt.screen.cursor_row,
                        host.vt.screen.cursor_col,
                        host.vt.screen.pending_wrap
                    ),
                    (2, 11, true)
                );
                assert!(host.vt.screen.grid.row_wrapped[0]);
                let dirty = host.dirty_rows.clone();
                feed(
                    &mut host,
                    b"\x1b[1:2;1;2;3;1$r\x1b[?1;1;2;3;1$r\x1b[1:2*x",
                    None,
                );
                assert_eq!(host.dirty_rows, dirty);
                assert!(host.vt.protocol.rectangular_attribute_extent());
            }
        }
    }

    #[test]
    fn native_screen_line_status_reads_the_live_screen_across_partitions() {
        let query = b"\x1bP$q*|\x1b\\";
        for profile in [NativeProfile::Ember, NativeProfile::FrostLegacy] {
            let feed = match profile {
                NativeProfile::Ember => VtState::feed_ember::<Host>,
                NativeProfile::FrostLegacy => VtState::feed_frost::<Host>,
            };
            for split in 0..=query.len() {
                let mut host = Host::new(profile);
                feed(&mut host, b"AB\x1b[2;3r\x1b[?6h", None);
                let (row, column) = (host.vt.screen.cursor_row, host.vt.screen.cursor_col);
                let dirty = host.dirty_rows.clone();
                feed(&mut host, &query[..split], None);
                feed(&mut host, &query[split..], None);
                assert_eq!(host.replies, b"\x1bP1$r3*|\x1b\\", "{profile:?} {split}");
                assert_eq!(
                    (host.vt.screen.cursor_row, host.vt.screen.cursor_col),
                    (row, column)
                );
                assert_eq!(host.dirty_rows, dirty);
                host.vt
                    .screen
                    .resize_geometry(7, 16, 0, RawRowId::allocate_process_unique);
                host.replies.clear();
                feed(&mut host, query, None);
                assert_eq!(host.replies, b"\x1bP1$r7*|\x1b\\", "{profile:?} {split}");
            }
        }
    }

    #[test]
    fn native_attribute_extent_queries_follow_the_live_owner_across_partitions() {
        let packet =
            b"\x1bP$q*x\x1b\\\x1b[2*x\x1bP$q*x\x1b\\\x1b[!p\x1bP$q*x\x1b\\\x1b[1*x\x1bP$q*x\x1b\\";
        for profile in [NativeProfile::Ember, NativeProfile::FrostLegacy] {
            let feed = match profile {
                NativeProfile::Ember => VtState::feed_ember::<Host>,
                NativeProfile::FrostLegacy => VtState::feed_frost::<Host>,
            };
            for split in 0..=packet.len() {
                let mut host = Host::new(profile);
                feed(&mut host, &packet[..split], None);
                feed(&mut host, &packet[split..], None);
                assert_eq!(
                    host.replies,
                    b"\x1bP1$r0*x\x1b\\\x1bP1$r2*x\x1b\\\x1bP1$r2*x\x1b\\\x1bP1$r1*x\x1b\\",
                    "{profile:?}, split {split}"
                );
                assert!(!host.vt.protocol.rectangular_attribute_extent());
            }
        }
    }

    #[test]
    fn native_tertiary_device_id_is_shared_across_scanners_and_partitions() {
        let packet = b"\x1b[=c\x1b[=0c\x1b[=000c\x1b[=1c\x1b[=0;0c\x1b[=0:0c\x1b[=0$c";
        for profile in [NativeProfile::Ember, NativeProfile::FrostLegacy] {
            let feed = match profile {
                NativeProfile::Ember => VtState::feed_ember::<Host>,
                NativeProfile::FrostLegacy => VtState::feed_frost::<Host>,
            };
            for split in 0..=packet.len() {
                let mut host = Host::new(profile);
                feed(&mut host, &packet[..split], None);
                feed(&mut host, &packet[split..], None);
                assert_eq!(
                    host.replies,
                    b"\x1bP!|00000000\x1b\\\x1bP!|00000000\x1b\\\x1bP!|00000000\x1b\\",
                    "{profile:?} split {split}"
                );
                assert_eq!(
                    (host.vt.screen.cursor_row, host.vt.screen.cursor_col),
                    (0, 0)
                );
            }
        }
    }

    #[test]
    fn native_unsupported_cursor_styles_preserve_shape_at_every_split() {
        let packet = b"\x1b[6 q\x1b[260 q\x1b[8 q\x1b[65535 q\x1bP$q q\x1b\\";
        for profile in [NativeProfile::Ember, NativeProfile::FrostLegacy] {
            let feed = match profile {
                NativeProfile::Ember => VtState::feed_ember::<Host>,
                NativeProfile::FrostLegacy => VtState::feed_frost::<Host>,
            };
            for split in 0..=packet.len() {
                let mut host = Host::new(profile);
                feed(&mut host, &packet[..split], None);
                feed(&mut host, &packet[split..], None);
                assert_eq!(host.replies, b"\x1bP1$r6 q\x1b\\", "{profile:?} {split}");
                assert_eq!(host.vt.screen.cursor_shape, crate::CursorShape::Beam);
                feed(&mut host, b"\x1b[4 q", None);
                assert_eq!(host.vt.screen.cursor_shape, crate::CursorShape::Underline);
            }
        }
    }

    #[test]
    fn native_printer_status_is_read_only_at_every_split() {
        for packet in [b"\x1b[?15n".as_slice(), b"\x1b[?0015n"] {
            for profile in [NativeProfile::Ember, NativeProfile::FrostLegacy] {
                let feed = match profile {
                    NativeProfile::Ember => VtState::feed_ember::<Host>,
                    NativeProfile::FrostLegacy => VtState::feed_frost::<Host>,
                };
                for split in 0..=packet.len() {
                    let mut host = Host::new(profile);
                    feed(&mut host, b"TEXT\x1b[2;3H\x1b[31;44m", None);
                    host.replies.clear();
                    let before: Vec<_> = (0..host.vt.screen.grid.rows())
                        .map(|row| host.vt.screen.grid[row].to_vec())
                        .collect();
                    feed(&mut host, &packet[..split], None);
                    feed(&mut host, &packet[split..], None);
                    assert_eq!(host.replies, b"\x1b[?13n", "{profile:?} {split}");
                    for (row, cells) in before.iter().enumerate() {
                        assert_eq!(&host.vt.screen.grid[row], cells.as_slice());
                    }
                    assert_eq!(
                        (host.vt.screen.cursor_row, host.vt.screen.cursor_col),
                        (1, 2)
                    );
                    assert_eq!(host.vt.screen.current_fg, crate::Color::Red);
                    assert_eq!(host.vt.screen.current_bg, crate::Color::Blue);
                }
            }
        }
    }

    #[test]
    fn native_locator_status_is_independent_of_mouse_modes_at_every_split() {
        let packet = b"\x1b[?1000;1006h\x1b[?55n\x1b[?56n";
        for profile in [NativeProfile::Ember, NativeProfile::FrostLegacy] {
            let feed = match profile {
                NativeProfile::Ember => VtState::feed_ember::<Host>,
                NativeProfile::FrostLegacy => VtState::feed_frost::<Host>,
            };
            for split in 0..=packet.len() {
                let mut host = Host::new(profile);
                feed(&mut host, b"TEXT\x1b[2;3H", None);
                host.replies.clear();
                let before: Vec<_> = (0..host.vt.screen.grid.rows())
                    .map(|row| host.vt.screen.grid[row].to_vec())
                    .collect();
                feed(&mut host, &packet[..split], None);
                feed(&mut host, &packet[split..], None);
                assert_eq!(host.replies, b"\x1b[?53n\x1b[?57;0n", "{profile:?} {split}");
                for (row, cells) in before.iter().enumerate() {
                    assert_eq!(&host.vt.screen.grid[row], cells.as_slice());
                }
                assert_eq!(
                    (host.vt.screen.cursor_row, host.vt.screen.cursor_col),
                    (1, 2)
                );
                assert!(host.vt.protocol.modes().contains(&1000));
                assert!(host.vt.protocol.modes().contains(&1006));
            }
        }
    }

    #[test]
    fn native_forward_cursor_aliases_preserve_grid_and_report_at_every_partition() {
        let packet =
            b"\x1b[3;4H\x1b[2a\x1b[6n\x1b[1e\x1b[6n\x1b[0a\x1b[0e\x1b[6n\x1b[a\x1b[e\x1b[6n";
        for profile in [NativeProfile::Ember, NativeProfile::FrostLegacy] {
            let feed = match profile {
                NativeProfile::Ember => VtState::feed_ember::<Host>,
                NativeProfile::FrostLegacy => VtState::feed_frost::<Host>,
            };
            for split in 0..=packet.len() {
                let mut host = Host::new(profile);
                host.vt.screen = ScreenState::new(6, 12);
                host.vt.screen.grid[0][0].character = '界';
                let before = host.vt.screen.grid.to_vec();
                feed(&mut host, &packet[..split], None);
                feed(&mut host, &packet[split..], None);
                assert_eq!(
                    host.replies, b"\x1b[3;6R\x1b[4;6R\x1b[5;7R\x1b[6;8R",
                    "{profile:?} {split}"
                );
                assert_eq!(host.vt.screen.grid.to_vec(), before);
                assert_eq!(
                    (host.vt.screen.cursor_row, host.vt.screen.cursor_col),
                    (5, 7)
                );
                assert!(!host.vt.screen.pending_wrap);
            }
        }
    }

    #[test]
    fn native_cursor_presentation_scanners_restore_and_report_across_all_partitions() {
        let packet = b"\x1bP1$t2;5;1;O;A;I;1;2;@;B0BB\x1b\\\x1b[1$w";
        let expected = b"\x1bP1$u2;5;1;O;A;I;1;2;@;B0BB\x1b\\";
        for profile in [NativeProfile::Ember, NativeProfile::FrostLegacy] {
            let feed = match profile {
                NativeProfile::Ember => VtState::feed_ember::<Host>,
                NativeProfile::FrostLegacy => VtState::feed_frost::<Host>,
            };
            for split in 0..=packet.len() {
                let mut host = Host::new(profile);
                host.vt.screen.current_fg = crate::Color::Red;
                host.vt.screen.scroll_region_top = 1;
                let ids: Vec<_> = (0..3).map(|r| host.vt.screen.grid.row_id(r)).collect();
                feed(&mut host, &packet[..split], None);
                feed(&mut host, &packet[split..], None);
                assert_eq!(host.replies, expected);
                assert_eq!(
                    (
                        host.vt.screen.cursor_row,
                        host.vt.screen.cursor_col,
                        host.vt.screen.origin_mode,
                        host.vt.screen.pending_wrap
                    ),
                    (1, 4, true, true)
                );
                assert!(
                    host.vt.screen.current_flags.bold()
                        && host.vt.screen.current_flags.blink()
                        && host.vt.screen.current_flags.inverse()
                        && host.vt.screen.current_flags.protected()
                );
                assert_eq!(
                    host.vt.screen.current_flags.underline(),
                    crate::UnderlineStyle::Single
                );
                assert!(host.vt.screen.invoked_g1());
                assert_eq!(host.vt.screen.translate_char('q'), '─');
                assert_eq!(host.vt.screen.current_fg, crate::Color::Red);
                host.replies.clear();
                feed(
                    &mut host,
                    b"\x1bP1$t1;1;1;@;@;@;0;2;@;B0BZ\x1b\\\x1b[1$w",
                    None,
                );
                assert_eq!(host.replies, expected); // Late invalid designation is atomic.
                assert!(host.erased_rows.is_empty() && host.dirty_rows.is_empty());
                assert_eq!(
                    (0..3)
                        .map(|r| host.vt.screen.grid.row_id(r))
                        .collect::<Vec<_>>(),
                    ids
                );
            }
        }
    }

    #[test]
    fn native_scroll_down_alias_reuses_margin_transaction_at_every_split() {
        for profile in [NativeProfile::Ember, NativeProfile::FrostLegacy] {
            let feed = match profile {
                NativeProfile::Ember => VtState::feed_ember::<Host>,
                NativeProfile::FrostLegacy => VtState::feed_frost::<Host>,
            };
            for parameter in ["", "0", "1", "2", "65535"] {
                let query = format!("\x1b[{parameter}^");
                for split in 0..=query.len() {
                    let mut host = Host::new(profile);
                    host.vt.screen = ScreenState::new(4, 8);
                    feed(&mut host, b"\x1b[1;1HAAAA\x1b[2;1HBBBB\x1b[3;1HCCCC\x1b[4;1HDDDD\x1b[2;3r\x1b[3;2H\x1b[1;31;44m", None);
                    feed(&mut host, &query.as_bytes()[..split], None);
                    feed(&mut host, &query.as_bytes()[split..], None);
                    let text: Vec<String> = (0..4)
                        .map(|row| {
                            (0..8)
                                .map(|col| host.vt.screen.grid.get(row, col).character)
                                .collect()
                        })
                        .collect();
                    let single = matches!(parameter, "" | "0" | "1");
                    assert_eq!(
                        text,
                        [
                            "AAAA    ",
                            "        ",
                            if single { "BBBB    " } else { "        " },
                            "DDDD    "
                        ],
                        "{profile:?} {parameter:?} {split}"
                    );
                    assert_eq!(
                        (host.vt.screen.cursor_row, host.vt.screen.cursor_col),
                        (2, 1)
                    );
                    let blank = host.vt.screen.grid.get(1, 0);
                    assert_eq!(
                        (blank.foreground, blank.background),
                        (crate::Color::Default, crate::Color::Blue)
                    );
                    assert_eq!(blank.flags, crate::StyleFlags::default());
                }
            }
        }
    }

    #[test]
    fn native_decid_matches_primary_da_at_every_partition_without_mutating_screen() {
        let packet = b"\x1bZ\x1b[c";
        for profile in [NativeProfile::Ember, NativeProfile::FrostLegacy] {
            let feed = match profile {
                NativeProfile::Ember => VtState::feed_ember::<Host>,
                NativeProfile::FrostLegacy => VtState::feed_frost::<Host>,
            };
            for split in 0..=packet.len() {
                let mut host = Host::new(profile);
                feed(&mut host, b"\x1b[31;44mSTATE\x1b[2;3H", None);
                let before: Vec<_> = (0..host.vt.screen.grid.rows())
                    .map(|row| host.vt.screen.grid[row].to_vec())
                    .collect();
                let cursor = (host.vt.screen.cursor_row, host.vt.screen.cursor_col);
                feed(&mut host, &packet[..split], None);
                feed(&mut host, &packet[split..], None);
                assert_eq!(
                    host.replies, b"\x1b[?65;1;9c\x1b[?65;1;9c",
                    "{profile:?} split {split}"
                );
                for (row, expected) in before.iter().enumerate() {
                    assert_eq!(&host.vt.screen.grid[row], expected.as_slice());
                }
                assert_eq!(
                    (host.vt.screen.cursor_row, host.vt.screen.cursor_col),
                    cursor
                );
            }
            for malformed in [b"\x1b Z".as_slice(), b"\x1b$Z", b"\x1b#Z"] {
                let mut host = Host::new(profile);
                feed(&mut host, malformed, None);
                assert!(host.replies.is_empty(), "{profile:?} {malformed:?}");
            }
        }
    }

    #[test]
    fn native_uk_nrcs_modes_and_banks_follow_negotiation_at_every_split() {
        let packet = b"\x1b(A#\x1b[?42h#\x1b[?42s\x1b[?42l#\x1b[?42r#\x1b)A\x0e#\x0f\x1b(B\x1b*A\x1bN##\x1b+A\x1bO##\x1b[?42$p";
        for profile in [NativeProfile::Ember, NativeProfile::FrostLegacy] {
            let feed = match profile {
                NativeProfile::Ember => VtState::feed_ember::<Host>,
                NativeProfile::FrostLegacy => VtState::feed_frost::<Host>,
            };
            for split in 0..=packet.len() {
                let mut host = Host::new(profile);
                feed(&mut host, &packet[..split], None);
                feed(&mut host, &packet[split..], None);
                let text: String = (0..9)
                    .map(|column| host.vt.screen.grid.get(0, column).character)
                    .collect();
                assert_eq!(text, "#£#£££#£#", "{profile:?} split {split}");
                assert_eq!(host.replies, b"\x1b[?42;1$y", "{profile:?} split {split}");
                assert_eq!(host.vt.screen.cursor_col, 9);
                assert!(!host.vt.screen.single_shift_pending());
            }
        }
    }

    #[test]
    fn native_four_bank_single_shifts_preserve_reports_then_consume_one_gl_byte() {
        let packet = b"\x1b*0\x1b+0\x1bN\x1b[1$wqq\x1bO\x1b[1$wqq\x1bnq\x1boq\x0fq";
        let expected = b"\x1bP1$u1;1;1;@;@;B;0;2;@;BB00\x1b\\\x1bP1$u1;3;1;@;@;D;0;2;@;BB00\x1b\\";
        for profile in [NativeProfile::Ember, NativeProfile::FrostLegacy] {
            let feed = match profile {
                NativeProfile::Ember => VtState::feed_ember::<Host>,
                NativeProfile::FrostLegacy => VtState::feed_frost::<Host>,
            };
            for split in 0..=packet.len() {
                let mut host = Host::new(profile);
                feed(&mut host, &packet[..split], None);
                feed(&mut host, &packet[split..], None);
                assert_eq!(host.replies, expected, "profile={profile:?} split={split}");
                let text: String = (0..7)
                    .map(|c| host.vt.screen.grid.get(0, c).character)
                    .collect();
                assert_eq!(text, "─q─q──q", "profile={profile:?} split={split}");
                assert!(!host.vt.screen.single_shift_pending());
                assert_eq!(host.vt.screen.invoked_charset_bank(), 0);
                assert_eq!(host.vt.screen.cursor_col, 7);
                assert_eq!(
                    host.vt.screen.charset_bank(2),
                    crate::screen::Charset::DecSpecialGraphics
                );
                assert_eq!(
                    host.vt.screen.charset_bank(3),
                    crate::screen::Charset::DecSpecialGraphics
                );
            }
        }
    }

    #[test]
    fn native_all_termcap_key_aliases_match_independent_common_wire_table() {
        let keys = [
            ("ku", "kcuu1", "\x1b[A", "\x1bOA"),
            ("kd", "kcud1", "\x1b[B", "\x1bOB"),
            ("kr", "kcuf1", "\x1b[C", "\x1bOC"),
            ("kl", "kcub1", "\x1b[D", "\x1bOD"),
            ("kh", "khome", "\x1b[H", "\x1bOH"),
            ("@7", "kend", "\x1b[F", "\x1bOF"),
            ("kI", "kich1", "\x1b[2~", "\x1b[2~"),
            ("kD", "kdch1", "\x1b[3~", "\x1b[3~"),
            ("kP", "kpp", "\x1b[5~", "\x1b[5~"),
            ("kN", "knp", "\x1b[6~", "\x1b[6~"),
            ("k1", "kf1", "\x1bOP", "\x1bOP"),
            ("k2", "kf2", "\x1bOQ", "\x1bOQ"),
            ("k3", "kf3", "\x1bOR", "\x1bOR"),
            ("k4", "kf4", "\x1bOS", "\x1bOS"),
            ("k5", "kf5", "\x1b[15~", "\x1b[15~"),
            ("k6", "kf6", "\x1b[17~", "\x1b[17~"),
            ("k7", "kf7", "\x1b[18~", "\x1b[18~"),
            ("k8", "kf8", "\x1b[19~", "\x1b[19~"),
            ("k9", "kf9", "\x1b[20~", "\x1b[20~"),
            ("k;", "kf10", "\x1b[21~", "\x1b[21~"),
            ("F1", "kf11", "\x1b[23~", "\x1b[23~"),
            ("F2", "kf12", "\x1b[24~", "\x1b[24~"),
        ];
        let hex = |bytes: &[u8]| {
            bytes
                .iter()
                .map(|byte| format!("{byte:02X}"))
                .collect::<String>()
        };
        for profile in [NativeProfile::Ember, NativeProfile::FrostLegacy] {
            let feed = match profile {
                NativeProfile::Ember => VtState::feed_ember::<Host>,
                NativeProfile::FrostLegacy => VtState::feed_frost::<Host>,
            };
            for application in [false, true] {
                let mut host = Host::new(profile);
                if application {
                    feed(&mut host, b"\x1b[?1h", None);
                }
                feed(&mut host, b"\x1b[>4;2m\x1b[>4;1f\x1b[?66h", None);
                for (short, long, normal, app) in keys {
                    for name in [short, long] {
                        let name = hex(name.as_bytes());
                        let request = format!("\x1bP+q{name}\x1b\\");
                        let expected = format!(
                            "\x1bP1+r{name}={}\x1b\\",
                            hex(if application { app } else { normal }.as_bytes())
                        );
                        feed(&mut host, request.as_bytes(), None);
                        assert_eq!(
                            host.replies,
                            expected.as_bytes(),
                            "{profile:?} app={application} name={name}"
                        );
                        host.replies.clear();
                    }
                }
                assert_eq!(host.vt.protocol.modes().contains(&1), application);
            }
        }
    }

    #[test]
    fn native_column_index_edges_are_ordered_across_every_split() {
        for profile in [NativeProfile::Ember, NativeProfile::FrostLegacy] {
            let feed = match profile {
                NativeProfile::Ember => VtState::feed_ember::<Host>,
                NativeProfile::FrostLegacy => VtState::feed_frost::<Host>,
            };
            for backward in [false, true] {
                let packet = if backward { b"\x1b6" } else { b"\x1b9" };
                for split in 0..=packet.len() {
                    let mut host = Host::new(profile);
                    for r in 0..3 {
                        for c in 0..12 {
                            host.vt.screen.grid.get_mut(r, c).character =
                                char::from(b'A' + r as u8);
                        }
                    }
                    host.vt.screen.cursor_col = if backward { 0 } else { 11 };
                    host.vt.screen.pending_wrap = true;
                    host.vt.screen.current_fg = crate::Color::Red;
                    host.vt.screen.current_bg = crate::Color::Blue;
                    feed(&mut host, &packet[..split], None);
                    feed(&mut host, &packet[split..], None);
                    assert_eq!(
                        host.erased_rows.iter().map(|x| x.0).collect::<Vec<_>>(),
                        vec![0, 1, 2]
                    );
                    // First callback sees the following row before its mutation.
                    assert_eq!(host.erased_rows[0].1, 'B');
                    assert!(host.vt.screen.pending_wrap);
                    assert_eq!(host.vt.screen.cursor_col, if backward { 0 } else { 11 });
                    for r in 0..3 {
                        let cell = host.vt.screen.grid.get(r, if backward { 0 } else { 11 });
                        assert_eq!(cell.character, ' ');
                        assert_eq!(
                            (cell.foreground, cell.background),
                            (crate::Color::Red, crate::Color::Blue)
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn native_rendition_stack_selection_at_every_split() {
        let packet = b"\x1b[1;31;44m\x1b[#{\x1b[0;32m\x1b[1:2#{\x1b[?#{\x1b[1#}\x1b[#}\x1b[30#{\x1b[33;42m\x1b[#}";
        let alias_packet = packet
            .iter()
            .map(|&b| match b {
                b'{' => b'p',
                b'}' => b'q',
                b => b,
            })
            .collect::<Vec<_>>();
        for packet in [packet.as_slice(), alias_packet.as_slice()] {
            for profile in [NativeProfile::Ember, NativeProfile::FrostLegacy] {
                let feed = match profile {
                    NativeProfile::Ember => VtState::feed_ember::<Host>,
                    NativeProfile::FrostLegacy => VtState::feed_frost::<Host>,
                };
                for split in 0..=packet.len() {
                    let mut host = Host::new(profile);
                    host.vt.screen.cursor_col = 11;
                    host.vt.screen.pending_wrap = true;
                    feed(&mut host, &packet[..split], None);
                    feed(&mut host, &packet[split..], None);
                    let screen = &host.vt.screen;
                    assert_eq!(
                        screen.current_fg,
                        crate::grid::Color::Red,
                        "{profile:?} split={split}"
                    );
                    assert_eq!(screen.current_bg, crate::grid::Color::Green);
                    assert!(screen.current_flags.bold());
                    assert_eq!(screen.cursor_col, 11);
                    assert!(screen.pending_wrap);
                    assert!(host.erased_rows.is_empty() && host.dirty_rows.is_empty());
                }
            }
        }
    }

    #[test]
    fn native_termcap_key_queries_follow_mode_transitions_at_every_split() {
        let packet =
            b"\x1bP+q6B63757531\x1b\\\x1b[?1h\x1bP+q6B75;6B6631\x1b\\\x1b[?1l\x1bP+q6B75\x1b\\";
        let expected=b"\x1bP1+r6B63757531=1B5B41\x1b\\\x1bP1+r6B75=1B4F41;6B6631=1B4F50\x1b\\\x1bP1+r6B75=1B5B41\x1b\\";
        for profile in [NativeProfile::Ember, NativeProfile::FrostLegacy] {
            let feed = match profile {
                NativeProfile::Ember => VtState::feed_ember::<Host>,
                NativeProfile::FrostLegacy => VtState::feed_frost::<Host>,
            };
            for split in 0..=packet.len() {
                let mut host = Host::new(profile);
                host.vt.screen.cursor_col = 11;
                host.vt.screen.pending_wrap = true;
                feed(&mut host, &packet[..split], None);
                feed(&mut host, &packet[split..], None);
                assert_eq!(host.replies, expected, "{profile:?} split={split}");
                assert!(!host.vt.protocol.modes().contains(&1));
                assert_eq!(host.vt.screen.cursor_col, 11);
                assert!(host.vt.screen.pending_wrap);
                assert!(host.erased_rows.is_empty() && host.dirty_rows.is_empty());
            }
        }
    }

    #[test]
    fn native_termcap_color_queries_keep_reply_order_and_the_live_cursor() {
        let packet = concat!(
            "\x1bP+q436f;524742\x1b\\",
            "\x1bP+q544E;436F\x1b\\",
            "\x1bP+q436f\x18",
            "\x1bP+q524742\x1b\\"
        )
        .as_bytes();
        let replies = b"\x1bP1+r436f=323536;524742=38\x1b\\\x1bP0+r\x1b\\\x1bP1+r524742=38\x1b\\";
        for profile in [NativeProfile::Ember, NativeProfile::FrostLegacy] {
            let feed = match profile {
                NativeProfile::Ember => VtState::feed_ember::<Host>,
                NativeProfile::FrostLegacy => VtState::feed_frost::<Host>,
            };
            for split in 0..=packet.len() {
                let mut host = Host::new(profile);
                feed(&mut host, b"\x1b[2;3HX", None);
                host.vt.screen.pending_wrap = true;
                feed(&mut host, &packet[..split], None);
                feed(&mut host, &packet[split..], None);
                assert_eq!(host.replies, replies, "{profile:?} split={split}");
                let screen = &host.vt.screen;
                assert_eq!((screen.cursor_row, screen.cursor_col), (1, 3));
                assert!(screen.pending_wrap);
                assert_eq!(screen.grid.get(1, 2).character, 'X');
            }
        }
    }

    #[test]
    fn native_selective_erase_line_and_display_keep_protected_cells() {
        let scenarios: &[(&[u8], [&str; 3])] = &[
            (b"\x1b[?K", ["ABCDE", "AB D ", "ABCDE"]),
            (b"\x1b[?1K", ["ABCDE", " B DE", "ABCDE"]),
            (b"\x1b[?2K", ["ABCDE", " B D ", "ABCDE"]),
            (b"\x1b[?J", ["ABCDE", "AB D ", " B D "]),
            (b"\x1b[?1J", [" B D ", " B DE", "ABCDE"]),
            (b"\x1b[?2J", [" B D ", " B D ", " B D "]),
        ];
        let line = b"A\x1b[1\"qB\x1b[0\"qC\x1b[1\"qD\x1b[0\"qE";
        for profile in [NativeProfile::Ember, NativeProfile::FrostLegacy] {
            let feed = match profile {
                NativeProfile::Ember => VtState::feed_ember::<Host>,
                NativeProfile::FrostLegacy => VtState::feed_frost::<Host>,
            };
            for &(packet, expected) in scenarios {
                for split in 0..=packet.len() {
                    let mut host = Host::new(profile);
                    for position in [b"\x1b[1;1H", b"\x1b[2;1H", b"\x1b[3;1H"] {
                        feed(&mut host, position, None);
                        feed(&mut host, line, None);
                    }
                    feed(&mut host, b"\x1b[2;3H", None);
                    let ids: Vec<_> = (0..3).map(|row| host.vt.screen.grid.row_id(row)).collect();
                    feed(&mut host, &packet[..split], None);
                    feed(&mut host, &packet[split..], None);
                    let screen = &host.vt.screen;
                    assert_eq!((screen.cursor_row, screen.cursor_col), (1, 2));
                    for (row, (expected, id)) in expected.into_iter().zip(ids).enumerate() {
                        let text: String = (0..5)
                            .map(|col| screen.grid.get(row, col).character)
                            .collect();
                        assert_eq!(text, expected, "{profile:?} {packet:?} split={split}");
                        assert!(screen.grid.get(row, 1).flags.protected());
                        assert!(screen.grid.get(row, 3).flags.protected());
                        assert_eq!(screen.grid.row_id(row), id);
                    }
                }
            }
        }
    }

    #[test]
    fn native_iso_latin1_sizes_and_fills_survive_every_input_partition() {
        let packet = concat!(
            "\x1b-A\x1b.A\x1b/A\x1bn\x1b}\x1bOqñ",
            "\x1b[241;2;1;2;1$x\x1b[1$w",
            "\x1b*0\x1b[241;2;2;2;2$x\x1b[1$w",
            "\x1bP1$t1;3;1;@;@;@;2;2;N;BAAA\x1b\\",
            "\x1b[255;2;3;2;3$x\x1b[1$w",
        )
        .as_bytes();
        let expected = concat!(
            "\x1bP1$u1;3;1;@;@;@;2;2;N;BAAA\x1b\\",
            "\x1bP1$u1;3;1;@;@;@;2;2;J;BA0A\x1b\\",
            "\x1bP1$u1;3;1;@;@;@;2;2;N;BAAA\x1b\\",
        )
        .as_bytes();
        for profile in [NativeProfile::Ember, NativeProfile::FrostLegacy] {
            let feed = match profile {
                NativeProfile::Ember => VtState::feed_ember::<Host>,
                NativeProfile::FrostLegacy => VtState::feed_frost::<Host>,
            };
            for split in 0..=packet.len() {
                let mut host = Host::new(profile);
                feed(&mut host, &packet[..split], None);
                feed(&mut host, &packet[split..], None);
                assert_eq!(host.replies, expected, "profile={profile:?} split={split}");
                let text = |row, cols| {
                    (0..cols)
                        .map(|col| host.vt.screen.grid.get(row, col).character)
                        .collect::<String>()
                };
                assert_eq!(text(0, 2), "qñ", "profile={profile:?} split={split}");
                assert_eq!(text(1, 3), "ñ─ÿ", "profile={profile:?} split={split}");
            }
        }
    }

    #[test]
    fn native_unsaved_cursor_restore_recovers_defaults_through_all_routes() {
        let setup=b"\x1b[2;3r\x1b[?6h\x1b[31;44;1;3;4m\x1b[1\"q\x1b(0\x1b)0\x1b*0\x1b+0\x1bo\x1b|\x1bN\x1bH\x1b[?7l";
        for profile in [NativeProfile::Ember, NativeProfile::FrostLegacy] {
            let feed = match profile {
                NativeProfile::Ember => VtState::feed_ember::<Host>,
                NativeProfile::FrostLegacy => VtState::feed_frost::<Host>,
            };
            for restore in [b"\x1b8".as_slice(), b"\x1b[u", b"\x1b[?1048l"] {
                for split in 0..=restore.len() {
                    let mut host = Host::new(profile);
                    feed(&mut host, setup, None);
                    let cells = host.vt.screen.grid.to_vec();
                    let tabs = host.vt.screen.tab_stops.clone();
                    feed(&mut host, &restore[..split], None);
                    feed(&mut host, &restore[split..], None);
                    feed(&mut host, b"\x1b[1$w", None);
                    assert_eq!(
                        host.replies, b"\x1bP1$u1;1;1;@;@;@;0;2;@;BBBB\x1b\\",
                        "{profile:?} {restore:?} split={split}"
                    );
                    assert_eq!(host.vt.screen.grid.to_vec(), cells);
                    assert_eq!(host.vt.screen.tab_stops, tabs);
                    assert_eq!(
                        (
                            host.vt.screen.scroll_region_top,
                            host.vt.screen.scroll_region_bottom
                        ),
                        (1, 2)
                    );
                    assert!(!host.vt.protocol.modes().contains(&7));
                    assert_eq!(
                        (
                            host.vt.screen.current_fg,
                            host.vt.screen.current_bg,
                            host.vt.screen.current_flags
                        ),
                        (
                            crate::Color::Default,
                            crate::Color::Default,
                            crate::StyleFlags::default()
                        )
                    );
                    feed(&mut host, b"q", None);
                    assert_eq!(host.vt.screen.grid.get(0, 0).character, 'q');
                }
            }
        }
    }

    #[test]
    fn native_column_edit_runs_from_top_to_bottom_through_all_scanner_splits() {
        let packet = b"\x1b[2'}\x1b[1'~";
        let original = ["ABCDEFGHIJKL", "abcdefghijkl", "012345678901"];
        let expected = ["ABCDEFGHIJKL", "ab cdefghij ", "01 23456789 "];
        for profile in [NativeProfile::Ember, NativeProfile::FrostLegacy] {
            let feed = match profile {
                NativeProfile::Ember => VtState::feed_ember::<Host>,
                NativeProfile::FrostLegacy => VtState::feed_frost::<Host>,
            };
            for split in 0..=packet.len() {
                let mut host = Host::new(profile);
                for (row, line) in original.into_iter().enumerate() {
                    for (column, character) in line.chars().enumerate() {
                        host.vt.screen.grid.get_mut(row, column).character = character;
                    }
                }
                feed(&mut host, b"\x1b[2;3r\x1b[2;3H", None);
                host.vt.screen.pending_wrap = true;
                host.vt.screen.grid.row_wrapped.fill(true);
                let ids: Vec<_> = (0..3).map(|row| host.vt.screen.grid.row_id(row)).collect();
                feed(&mut host, &packet[..split], None);
                feed(&mut host, &packet[split..], None);

                let screen = &host.vt.screen;
                assert_eq!((screen.cursor_row, screen.cursor_col), (1, 2));
                assert!(screen.pending_wrap);
                assert_eq!(screen.grid.row_wrapped, [true, false, false]);
                for (row, (line, id)) in expected.into_iter().zip(ids).enumerate() {
                    let text: String = (0..12)
                        .map(|col| screen.grid.get(row, col).character)
                        .collect();
                    assert_eq!(text, line, "{profile:?} split={split} row={row}");
                    assert_eq!(screen.grid.row_id(row), id);
                }
                let effects: Vec<_> = host.erased_rows.iter().map(|(row, _)| *row).collect();
                assert_eq!(effects, [1, 2, 1, 2]);
                feed(&mut host, b"\x1b[0:2'}\x1b[1;2'~\x1b[?1'}\x1b[1 ~", None);
                assert_eq!(host.erased_rows.len(), 4, "{profile:?}");
            }
        }
    }

    #[test]
    fn native_reverse_wrap_uses_actual_soft_wrap_bs_and_cub_across_all_splits() {
        for backward in [b"\x08".as_slice(), b"\x1b[D"] {
            let packet = [
                b"\x1b[?7h\x1b[?45habcdefghijklZ\x1b[2;1H".as_slice(),
                backward,
                b"\x1b[?45$p\x1b[6n",
            ]
            .concat();
            for profile in [NativeProfile::Ember, NativeProfile::FrostLegacy] {
                let feed = match profile {
                    NativeProfile::Ember => VtState::feed_ember::<Host>,
                    NativeProfile::FrostLegacy => VtState::feed_frost::<Host>,
                };
                for split in 0..=packet.len() {
                    let mut host = Host::new(profile);
                    host.apply_wrap = true;
                    feed(&mut host, &packet[..split], None);
                    feed(&mut host, &packet[split..], None);
                    assert_eq!(
                        host.replies, b"\x1b[?45;1$y\x1b[1;12R",
                        "{profile:?} {backward:?} split={split}"
                    );
                    assert_eq!(
                        (
                            host.vt.screen.cursor_row,
                            host.vt.screen.cursor_col,
                            host.vt.screen.pending_wrap
                        ),
                        (0, 11, false)
                    );
                    assert!(host.vt.screen.grid.row_wrapped[0]);
                    assert_eq!(host.vt.screen.grid.get(1, 0).character, 'Z');
                    host.replies.clear();
                    feed(
                        &mut host,
                        b"\x1b[?45s\x1b[!p\x1b[?45$p\x1b[?45r\x1b[?45$p",
                        None,
                    );
                    assert_eq!(host.replies, b"\x1b[?45;2$y\x1b[?45;1$y");
                }
            }
        }
    }

    #[test]
    fn native_horizontal_scroll_is_full_region_and_malformed_forms_never_fall_back_to_ich() {
        for (packet, insert) in [
            (b"\x1b[2 @".as_slice(), false),
            (b"\x1b[2 A".as_slice(), true),
        ] {
            for profile in [NativeProfile::Ember, NativeProfile::FrostLegacy] {
                let feed = match profile {
                    NativeProfile::Ember => VtState::feed_ember::<Host>,
                    NativeProfile::FrostLegacy => VtState::feed_frost::<Host>,
                };
                for split in 0..=packet.len() {
                    let mut host = Host::new(profile);
                    for row in 0..3 {
                        for col in 0..12 {
                            host.vt.screen.grid.get_mut(row, col).character =
                                char::from(b'A' + row as u8);
                        }
                        host.vt.screen.grid.row_wrapped[row] = true;
                    }
                    host.vt.screen.cursor_row = 1;
                    host.vt.screen.cursor_col = 5;
                    host.vt.screen.pending_wrap = true;
                    host.vt.screen.scroll_region_top = 1;
                    host.vt.screen.scroll_region_bottom = 2;
                    host.vt.screen.current_bg = crate::Color::Blue;
                    feed(&mut host, &packet[..split], None);
                    feed(&mut host, &packet[split..], None);
                    for row in 1..3 {
                        for col in 0..12 {
                            let blank = if insert { col < 2 } else { col >= 10 };
                            let cell = host.vt.screen.grid.get(row, col);
                            assert_eq!(
                                cell.character,
                                if blank {
                                    ' '
                                } else {
                                    char::from(b'A' + row as u8)
                                }
                            );
                            if blank {
                                assert_eq!(cell.background, crate::Color::Blue);
                            }
                        }
                    }
                    assert_eq!(host.vt.screen.grid.get(0, 0).character, 'A');
                    assert_eq!(host.vt.screen.grid.row_wrapped, [true, insert, insert]);
                    assert_eq!(
                        (
                            host.vt.screen.cursor_row,
                            host.vt.screen.cursor_col,
                            host.vt.screen.pending_wrap
                        ),
                        (1, 5, true)
                    );
                    assert_eq!(
                        host.erased_rows
                            .iter()
                            .map(|&(row, _)| row)
                            .collect::<Vec<_>>(),
                        [1, 2]
                    );
                    let before = host.vt.screen.grid.to_vec();
                    feed(
                        &mut host,
                        b"\x1b[2:1 @\x1b[?2 @\x1b[2;1 @\x1b[2:1 A\x1b[?2 A\x1b[2;1 A\x1b[2  @\x1b[2 $@\x1b[2  A\x1b[?2  @",
                        None,
                    );
                    assert_eq!(
                        host.vt.screen.grid.to_vec(),
                        before,
                        "{profile:?} malformed"
                    );
                    // Plain ICH retains its one-row, current-column meaning.
                    feed(&mut host, b"\x1b[@", None);
                    assert_eq!(host.vt.screen.grid.get(1, 5).character, ' ');
                    assert_eq!(host.vt.screen.grid[2], before[2]);
                    feed(&mut host, b"\x1b[A", None);
                    assert_eq!(host.vt.screen.cursor_row, 1); // CUU stops at scroll-region top.
                }
            }
        }
    }

    #[test]
    fn frost_alternate_scroll_mode_query_save_restore_and_soft_reset_cross_all_splits() {
        let packet=b"\x1b[?1007$p\x1b[?1007h\x1b[?1007$p\x1b[?1007s\x1b[?1007l\x1b[?1007$p\x1b[?1007r\x1b[?1007$p\x1b[!p\x1b[?1007$p\x1b[?1007r\x1b[?1007$p";
        let expected =
            b"\x1b[?1007;2$y\x1b[?1007;1$y\x1b[?1007;2$y\x1b[?1007;1$y\x1b[?1007;2$y\x1b[?1007;1$y";
        for split in 0..=packet.len() {
            let mut host = Host::new(NativeProfile::FrostLegacy);
            let before = host.vt.screen.grid.to_vec();
            VtState::feed_frost(&mut host, &packet[..split], None);
            VtState::feed_frost(&mut host, &packet[split..], None);
            assert_eq!(host.replies, expected, "split={split}");
            assert!(host.vt.protocol.modes().contains(&1007));
            assert_eq!(host.vt.screen.grid.to_vec(), before);
        }
    }

    #[test]
    fn opted_ember_wheel_negotiation_catches_parameter_aba_and_keyboard_transitions() {
        let stages: &[(&[u8], usize)] = &[
            (b"\x1b[?1h\x1b[?1l", 2),
            (b"\x1b[?1000h", 1),
            (b"\x1b[?1000;1007l", 2), // None→eligible→None inside one CSI.
            (b"\x1b[?1007h\x1b[?1007h", 1),
            (b"\x1b[?1h\x1b[?1s\x1b[?1l\x1b[?1r", 3),
            (b"\x1b[>1u\x1b[>1u\x1b[<u\x1b[<u", 2),
            (b"\x1b[!p\x1b[!p", 1),
            (b"X\x1b[?1007$p\x1b[?u\x1b[>1 u\x1b[0%m", 0),
            (b"\x1b[?1000h\x1b[?1h\x1b[>8u\x1b[<u\x1b[?1000l", 2),
        ];
        for opted in [false, true] {
            for chunk in 1..=20 {
                let mut host = Host::new(NativeProfile::Ember);
                host.alternate_scroll_tracking = opted;
                host.vt.screen.use_alt_buffer = true;
                host.vt.protocol.modes_mut().insert(1007);
                let mut expected = 0;
                for &(packet, retirements) in stages {
                    for part in packet.chunks(chunk) {
                        VtState::feed_ember(&mut host, part, None);
                    }
                    if opted {
                        expected += retirements;
                    }
                    assert_eq!(
                        host.wheel_retires, expected,
                        "opted={opted} chunk={chunk} packet={packet:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn native_displayed_extent_replies_across_every_input_partition() {
        let query = b"\x1b[\"v";
        for profile in [NativeProfile::Ember, NativeProfile::FrostLegacy] {
            let feed = match profile {
                NativeProfile::Ember => VtState::feed_ember::<Host>,
                NativeProfile::FrostLegacy => VtState::feed_frost::<Host>,
            };
            for split in 0..=query.len() {
                let mut host = Host::new(profile);
                host.vt.screen.cursor_col = 11;
                host.vt.screen.pending_wrap = true;
                feed(&mut host, &query[..split], None);
                feed(&mut host, &query[split..], None);
                assert_eq!(host.replies, b"\x1b[3;12;1;1;1\"w");
                assert_eq!(host.vt.screen.cursor_col, 11);
                assert!(host.vt.screen.pending_wrap);
            }
        }
    }

    #[test]
    fn frost_status_requests_use_the_ordered_reply_effect() {
        let query = b"\x1bP$qm\x1b\\";
        for split in 0..=query.len() {
            let mut host = Host::new(NativeProfile::FrostLegacy);
            host.vt.screen.current_fg = crate::Color::Red;
            VtState::feed_frost(&mut host, &query[..split], None);
            VtState::feed_frost(&mut host, &query[split..], None);
            assert_eq!(host.replies, b"\x1bP1$r0;31m\x1b\\");
            assert_eq!(host.vt.screen.current_fg, crate::Color::Red);
            let report = host
                .replies
                .strip_prefix(b"\x1bP1$r")
                .unwrap()
                .strip_suffix(b"\x1b\\")
                .unwrap();
            let replay = [b"\x1b[".as_slice(), report].concat();
            host.replies.clear();
            VtState::feed_frost(&mut host, b"\x1bPignored\x1b\\\x1bP$qm\x18", None);
            assert!(host.replies.is_empty());
            host.vt.screen.current_flags.set_bold(true);
            VtState::feed_frost(&mut host, &replay, None);
            assert_eq!(host.vt.screen.current_fg, crate::Color::Red);
            assert!(!host.vt.screen.current_flags.bold());
        }

        let mut host = Host::new(NativeProfile::FrostLegacy);
        VtState::feed_frost(
            &mut host,
            b"\x1b[>4;2m\x1b[>4;1f\x1bP$q>4m\x1b\\\x1bP$q>4f\x1b\\",
            None,
        );
        assert_eq!(host.replies, b"\x1bP1$r>4;2m\x1b\\\x1bP1$r>4;1f\x1b\\");
    }

    #[test]
    fn native_character_tab_controls_match_hts_and_tbc_across_partitions() {
        let packet=b"\x1b[5W\x1b[1;5H\x1b[W\x1b[H\t\x1b[6n\x1b[2W\x1b[H\t\x1b[6n\x1b[1;7H\x1b[5;0W\x1b[H\t\x1b[6n";
        for profile in [NativeProfile::Ember, NativeProfile::FrostLegacy] {
            let feed = match profile {
                NativeProfile::Ember => VtState::feed_ember::<Host>,
                NativeProfile::FrostLegacy => VtState::feed_frost::<Host>,
            };
            for split in 0..=packet.len() {
                let mut host = Host::new(profile);
                host.vt.screen = ScreenState::new(3, 12);
                feed(&mut host, &packet[..split], None);
                feed(&mut host, &packet[split..], None);
                assert_eq!(
                    host.replies, b"\x1b[1;5R\x1b[1;12R\x1b[1;7R",
                    "{profile:?} split {split}"
                );
                let tabs = host.vt.screen.tab_stops.clone();
                feed(&mut host, b"\x1b[5;0:2W\x1b[>5W\x1b[5 W", None);
                assert_eq!(host.vt.screen.tab_stops, tabs);
                feed(&mut host, b"\x1b[?5W", None);
                assert_eq!(
                    host.vt.screen.tab_stops,
                    crate::screen::default_tab_stops(12, crate::screen::TabStopOrigin::Clear)
                );
            }
        }
    }

    #[test]
    fn native_saved_private_modes_flow_through_both_scanners() {
        let packet = b"\x1b[?1;1002;1006h\x1b[?1;1000;1015s\x1b[?1;1002;1006l\x1b[?1;1003;1005r\x1b[?1$p\x1b[?1002$p\x1b[?1006$p";
        for profile in [NativeProfile::Ember, NativeProfile::FrostLegacy] {
            for split in 0..=packet.len() {
                let mut host = Host::new(profile);
                let feed = match profile {
                    NativeProfile::Ember => VtState::feed_ember,
                    NativeProfile::FrostLegacy => VtState::feed_frost,
                };
                feed(&mut host, &packet[..split], None);
                feed(&mut host, &packet[split..], None);
                assert_eq!(host.replies, b"\x1b[?1;1$y\x1b[?1002;1$y\x1b[?1006;1$y");
                // Colon groups and intermediate-bearing commands cannot save
                // or restore an accidentally flattened mode number.
                feed(&mut host, b"\x1b[?1:2s\x1b[?1l\x1b[?1:2r\x1b[?1$r", None);
                assert!(!host.vt.protocol.modes().contains(&1));
                feed(&mut host, b"\x1b[?1r", None);
                assert!(host.vt.protocol.modes().contains(&1));
            }
        }
    }

    #[test]
    fn native_x10_negotiation_and_queries_flow_through_both_scanners() {
        let packet = b"\x1b[?9h\x1b[?9$p\x1b[?9l\x1b[?9$p";
        for profile in [NativeProfile::Ember, NativeProfile::FrostLegacy] {
            for split in 0..=packet.len() {
                let mut host = Host::new(profile);
                for bytes in [&packet[..split], &packet[split..]] {
                    match profile {
                        NativeProfile::Ember => VtState::feed_ember(&mut host, bytes, None),
                        NativeProfile::FrostLegacy => VtState::feed_frost(&mut host, bytes, None),
                    }
                }
                assert_eq!(host.replies, b"\x1b[?9;1$y\x1b[?9;2$y");
                assert!(!host.vt.protocol.modes().is_mouse_enabled());
            }
        }
    }

    #[test]
    fn native_pixel_mouse_requires_opt_in_and_a_matching_coordinate_type() {
        use crate::mouse::MousePosition;

        let packet = b"\x1b[?1003;1006h\x1b[?1016$p\x1b[?1016h\x1b[?1016$p\x1b[?1006$p";
        let pixel = MousePosition::Pixel { x: 26, y: 51 };
        let cell = MousePosition::Cell { column: 2, row: 1 };
        for profile in [NativeProfile::Ember, NativeProfile::FrostLegacy] {
            let feed = match profile {
                NativeProfile::Ember => VtState::feed_ember::<Host>,
                NativeProfile::FrostLegacy => VtState::feed_frost::<Host>,
            };
            for split in 0..=packet.len() {
                for enabled in [false, true] {
                    let mut host = Host::new(profile);
                    if enabled {
                        host.vt.protocol.modes_mut().enable_pixel_mouse();
                    }
                    feed(&mut host, &packet[..split], None);
                    feed(&mut host, &packet[split..], None);
                    let modes = host.vt.protocol.modes();
                    if enabled {
                        assert_eq!(host.replies, b"\x1b[?1016;2$y\x1b[?1016;1$y\x1b[?1006;2$y");
                        assert!(modes.get_mouse_report_at(0, cell).is_none());
                        assert!(modes.get_mouse_release_report_at(0, cell).is_none());
                        assert_eq!(
                            modes.get_mouse_report_at(32, pixel).as_deref(),
                            Some(b"\x1b[<32;27;52M".as_slice())
                        );
                        assert_eq!(
                            modes.get_mouse_release_report_at(16, pixel).as_deref(),
                            Some(b"\x1b[<16;27;52m".as_slice())
                        );
                    } else {
                        assert_eq!(host.replies, b"\x1b[?1016;0$y\x1b[?1016;0$y\x1b[?1006;1$y");
                        assert!(modes.get_mouse_report_at(0, pixel).is_none());
                        assert!(modes.get_mouse_release_report_at(0, pixel).is_none());
                        assert_eq!(
                            modes.get_mouse_report_at(0, cell).as_deref(),
                            Some(b"\x1b[<0;3;2M".as_slice())
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn native_saved_pixel_mode_notifies_the_host_at_each_coordinate_transition() {
        use crate::mouse::MouseEncoding;

        for profile in [NativeProfile::Ember, NativeProfile::FrostLegacy] {
            let feed = match profile {
                NativeProfile::Ember => VtState::feed_ember::<Host>,
                NativeProfile::FrostLegacy => VtState::feed_frost::<Host>,
            };
            let mut host = Host::new(profile);
            host.vt.protocol.modes_mut().enable_pixel_mouse();
            feed(&mut host, b"\x1b[?1002;1016h\x1b[?1016s", None);
            let before = host.wheel_retires;
            feed(
                &mut host,
                b"\x1b[?1006h\x1b[?1016$p\x1b[?1006r\x1b[?1016$p",
                None,
            );
            assert_eq!(host.replies, b"\x1b[?1016;2$y\x1b[?1016;1$y");
            assert_eq!(
                host.vt
                    .protocol
                    .modes()
                    .mouse_report_profile()
                    .unwrap()
                    .encoding,
                MouseEncoding::SgrPixels
            );
            let expected = if profile == NativeProfile::Ember {
                2
            } else {
                3
            };
            assert_eq!(host.wheel_retires - before, expected);
        }
    }

    #[test]
    fn frost_tab_state_roundtrip_preserves_cursor_and_wrap() {
        let packet = b"\x1bP2$t4/9/17\x1b\\\x1b[2$w";
        for split in 0..=packet.len() {
            let mut host = Host::new(NativeProfile::FrostLegacy);
            host.vt.screen.cursor_col = 11;
            host.vt.screen.pending_wrap = true;
            VtState::feed_frost(&mut host, &packet[..split], None);
            VtState::feed_frost(&mut host, &packet[split..], None);
            assert_eq!(host.replies, b"\x1bP2$u4/9\x1b\\");
            assert_eq!(host.vt.screen.cursor_col, 11);
            assert!(host.vt.screen.pending_wrap);
            let old = host.vt.screen.tab_stops.clone();
            VtState::feed_frost(&mut host, b"\x1bP2$t4/0\x1b\\", None);
            assert_eq!(host.vt.screen.tab_stops, old);
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
    #[test]
    fn native_dec_special_control_pictures_survive_all_feed_partitions() {
        for profile in [NativeProfile::Ember, NativeProfile::FrostLegacy] {
            let feed = match profile {
                NativeProfile::Ember => VtState::feed_ember::<Host>,
                NativeProfile::FrostLegacy => VtState::feed_frost::<Host>,
            };
            for packet in [
                b"\x1b(0bcdehi\x1b(Bbci".as_slice(),
                b"\x1b)0\x0ebcdehi\x0fbci",
                b"\x1b*0\x1bNb\x1bNc\x1bNd\x1bNe\x1bNh\x1bNibci",
                b"\x1b+0\x1bOb\x1bOc\x1bOd\x1bOe\x1bOh\x1bOibci",
            ] {
                for split in 0..=packet.len() {
                    let mut host = Host::new(profile);
                    let ids = host.vt.screen.grid.row_ids().to_vec();
                    feed(&mut host, &packet[..split], None);
                    feed(&mut host, &packet[split..], None);
                    let text: String = host
                        .vt
                        .screen
                        .grid
                        .row_slice(0)
                        .unwrap()
                        .iter()
                        .map(|cell| cell.character)
                        .collect();
                    assert!(
                        text.starts_with("␉␌␍␊␤␋bci"),
                        "{profile:?} split {split}: {text:?}"
                    );
                    assert_eq!(
                        (
                            host.vt.screen.cursor_row,
                            host.vt.screen.cursor_col,
                            host.vt.screen.pending_wrap
                        ),
                        (0, 9, false)
                    );
                    assert_eq!(host.vt.screen.grid.row_ids(), ids);
                    assert!(host.replies.is_empty());
                    assert!(!host
                        .vt
                        .screen
                        .grid
                        .row_wrapped
                        .iter()
                        .any(|wrapped| *wrapped));
                }
            }
        }
    }
}
