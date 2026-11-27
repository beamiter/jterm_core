//! FrostLegacyV1 CSI execution. The engine selects and completes each state
//! transaction; the host consumes every effect synchronously before continuation.
use crate::column_edit::prepare_column_edit;
use crate::csi_transaction::{CsiTransaction, CsiTransactionEffect};
use crate::display_erase::{prepare_display_erase, DisplayEraseProfile, DisplayEraseStep};
use crate::graphic_write::{GraphicProfile, GraphicStep, RepeatGraphics};
use crate::mode_mutation::{
    apply_native_mode_parameter, native_alternate_scroll_profile, restore_dec_private_modes,
    save_dec_private_modes, ModeNamespace, SavedDecModeProfile,
};
use crate::mode_mutation::{prepare_native_mode_command, NativeModeEffect, NativeModeProfile};
use crate::protocol_state::ProtocolState;
use crate::rectangular_edit::{prepare_rectangular_attributes, prepare_rectangular_edit};
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
    /// Exact content effects for legacy ED0/ED1, separate from metadata-only
    /// DirtyRows. Default preserves the original whole-operation callbacks.
    fn track_legacy_erase_mutations(&self) -> bool {
        false
    }

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
    if prefix.is_none() && intermediates == b"$" && matches!(command, 'r' | 't') {
        let (screen, protocol) = host.csi_state();
        if !raw_params.contains(&b':') {
            if let Some(mut rectangle) = prepare_rectangular_attributes(
                screen,
                params,
                command,
                prefix,
                intermediates,
                protocol.rectangular_attribute_extent,
            ) {
                while let Some(mutations) = rectangle.step(host.csi_state().0) {
                    if let Some(change) = mutations.first() {
                        host.csi_effect(FrostCsiEffect::Erase(DisplayEraseStep::DirtyRows(
                            change.row..=change.row,
                        )));
                    }
                }
            }
        }
        return;
    }
    if prefix.is_none() && intermediates == b"$" && matches!(command, 'z' | '{' | 'x' | 'v') {
        if !raw_params.contains(&b':') {
            if let Some(mut rectangle) =
                prepare_rectangular_edit(host.csi_state().0, params, command, prefix, intermediates)
            {
                while let Some(mutations) = rectangle.step(host.csi_state().0) {
                    if !mutations.is_empty() {
                        host.csi_effect(FrostCsiEffect::Erase(DisplayEraseStep::Mutations(
                            mutations,
                        )));
                    }
                }
            }
        }
        return;
    }
    if (intermediates == b"'" && matches!(command, '}' | '~'))
        || (intermediates.starts_with(b" ") && matches!(command, '@' | 'A'))
    {
        if prefix.is_none() && !raw_params.contains(&b':') {
            if let Some(mut columns) =
                prepare_column_edit(host.csi_state().0, params, command, prefix, intermediates)
            {
                while let Some(mutation) = columns.step(host.csi_state().0) {
                    host.csi_effect(FrostCsiEffect::Erase(DisplayEraseStep::Mutations(
                        smallvec::smallvec![mutation],
                    )));
                }
            }
        }
        return;
    }
    if prefix == Some(b'?') && intermediates.is_empty() && matches!(command, 's' | 'r') {
        if raw_params.contains(&b':') {
            return;
        }
        if command == 's' {
            let (screen, protocol) = host.csi_state();
            save_dec_private_modes(screen, protocol, SavedDecModeProfile::FrostLegacy, params);
        } else {
            let operations = restore_dec_private_modes(
                host.csi_state().1,
                SavedDecModeProfile::FrostLegacy,
                params,
            );
            for (mode, enabled) in operations {
                let focused = false;
                let (screen, protocol) = host.csi_state();
                let effect = apply_native_mode_parameter(
                    NativeModeProfile::FrostLegacy,
                    screen,
                    protocol,
                    ModeNamespace::DecPrivate,
                    mode,
                    enabled,
                    focused,
                );
                host.csi_effect(FrostCsiEffect::Mode(effect));
            }
        }
        return;
    }
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
        let watch_wheel_profile = (command == 'u'
            && matches!(prefix, Some(b'=' | b'>' | b'<'))
            && intermediates.is_empty())
            || (command == 'p' && prefix.is_none() && intermediates == b"!");
        let (screen, protocol) = host.csi_state();
        let previous_wheel_profile =
            watch_wheel_profile.then(|| native_alternate_scroll_profile(screen, protocol));
        if let Dispatch::Handled(effects) = dispatch_frost_legacy_csi(
            screen,
            protocol,
            raw_params,
            params,
            command as u8,
            prefix,
            intermediates,
        ) {
            if previous_wheel_profile.is_some_and(|old| {
                let (screen, protocol) = host.csi_state();
                native_alternate_scroll_profile(screen, protocol) != old
            }) {
                host.csi_effect(FrostCsiEffect::Mode(
                    NativeModeEffect::MouseReportingChanged,
                ));
            }
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
    if let Some(mut operation) = operation {
        if host.track_legacy_erase_mutations() {
            operation.track_legacy_erase_mutations();
        }
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
        wheel_retires: usize,
    }
    impl Host {
        fn new() -> Self {
            Self {
                screen: ScreenState::new(3, 7),
                protocol: ProtocolState::default(),
                trace: vec![],
                replies: vec![],
                dark: true,
                wheel_retires: 0,
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
                FrostCsiEffect::Mode(effect) => {
                    self.trace.push("mode".into());
                    if matches!(effect, NativeModeEffect::MouseReportingChanged) {
                        self.wheel_retires += 1;
                    }
                }
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
    #[test]
    fn eligible_native_wheel_profiles_retire_round_trips_without_noop_churn() {
        let mut primary = Host::new();
        primary.protocol.modes_mut().insert(1007);
        execute(&mut primary, &[1], b"1", 'h', Some(b'?'), b"");
        execute(&mut primary, &[1], b"1", 'u', Some(b'>'), b"");
        assert_eq!(primary.wheel_retires, 0);
        let mut host = Host::new();
        host.screen.use_alt_buffer = true;
        host.protocol.modes_mut().insert(1007);
        for (enabled, expected) in [(true, 1), (true, 1), (false, 2), (false, 2)] {
            execute(
                &mut host,
                &[1],
                b"1",
                if enabled { 'h' } else { 'l' },
                Some(b'?'),
                b"",
            );
            assert_eq!(host.wheel_retires, expected);
        }
        execute(&mut host, &[1], b"1", 'u', Some(b'>'), b"");
        assert_eq!(host.wheel_retires, 3);
        execute(&mut host, &[1], b"1", 'u', Some(b'>'), b"");
        assert_eq!(host.wheel_retires, 3);
        execute(&mut host, &[], b"", 'u', Some(b'<'), b"");
        assert_eq!(host.wheel_retires, 3);
        execute(&mut host, &[], b"", 'u', Some(b'<'), b"");
        assert_eq!(host.wheel_retires, 4);
        execute(&mut host, &[], b"", 'u', Some(b'?'), b"");
        assert_eq!(host.wheel_retires, 4);
        execute(&mut host, &[1000], b"1000", 'h', Some(b'?'), b"");
        assert_eq!(host.wheel_retires, 5);
        execute(&mut host, &[1], b"1", 'h', Some(b'?'), b"");
        execute(&mut host, &[8], b"8", 'u', Some(b'>'), b"");
        execute(&mut host, &[], b"", 'u', Some(b'<'), b"");
        assert_eq!(host.wheel_retires, 5);
        execute(&mut host, &[1000], b"1000", 'l', Some(b'?'), b"");
        assert_eq!(host.wheel_retires, 6);
        execute(&mut host, &[], b"", 'p', None, b"!");
        assert_eq!(host.wheel_retires, 7);
        execute(&mut host, &[], b"", 'p', None, b"!");
        assert_eq!(host.wheel_retires, 7);
        assert!(!host.protocol.modes().contains(&1007));
    }
}

#[cfg(test)]
mod precise_content_tests {
    use super::*;
    use crate::{Color, StyleFlags, TerminalCell};
    use std::ops::Range;
    struct Host {
        screen: ScreenState,
        protocol: ProtocolState,
        tracked: bool,
        mutations: Vec<(usize, Range<usize>, char)>,
        dirty_rows: usize,
    }
    impl Host {
        fn new(tracked: bool) -> Self {
            let mut screen = ScreenState::new(3, 8);
            for row in 0..3 {
                for col in 0..8 {
                    screen.grid.get_mut(row, col).character = char::from(b'A' + row as u8);
                }
                screen.grid.row_wrapped[row] = true;
            }
            screen.cursor_row = 1;
            screen.cursor_col = 3;
            screen.pending_wrap = true;
            screen.current_bg = Color::Blue;
            Self {
                screen,
                protocol: ProtocolState::default(),
                tracked,
                mutations: Vec::new(),
                dirty_rows: 0,
            }
        }
        fn record(&mut self, mutation: crate::screen::RowMutation) {
            assert!(mutation.printed.is_none());
            assert_eq!(mutation.row_id, self.screen.grid.row_id(mutation.row));
            assert!(self.screen.pending_wrap);
            let next = if mutation.row + 1 < 3 {
                self.screen.grid.get(mutation.row + 1, 0).character
            } else {
                '-'
            };
            self.mutations.push((mutation.row, mutation.changed, next));
        }
    }
    impl FrostCsiHost for Host {
        fn csi_state(&mut self) -> (&mut ScreenState, &mut ProtocolState) {
            (&mut self.screen, &mut self.protocol)
        }
        fn track_legacy_erase_mutations(&self) -> bool {
            self.tracked
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
        fn csi_effect(&mut self, effect: FrostCsiEffect<'_>) {
            match effect {
                FrostCsiEffect::Erase(DisplayEraseStep::Mutations(mutations)) => {
                    for mutation in mutations {
                        self.record(mutation);
                    }
                }
                FrostCsiEffect::Erase(DisplayEraseStep::DirtyRows(_)) => self.dirty_rows += 1,
                FrostCsiEffect::Local {
                    effect: Effect::Row(mutation),
                    ..
                } => self.record(mutation),
                _ => {}
            }
        }
    }
    #[test]
    fn opted_legacy_ed_reports_exact_rows_before_mutating_the_following_row() {
        for mode in [0, 1] {
            let mut host = Host::new(true);
            let ids: Vec<_> = (0..3).map(|r| host.screen.grid.row_id(r)).collect();
            if mode == 0 {
                host.screen.grid.get_mut(1, 2).flags.set_wide(true);
                host.screen
                    .grid
                    .get_mut(1, 3)
                    .flags
                    .set_wide_continuation(true);
            } else {
                host.screen.grid.get_mut(1, 3).flags.set_wide(true);
                host.screen
                    .grid
                    .get_mut(1, 4)
                    .flags
                    .set_wide_continuation(true);
            }
            execute(
                &mut host,
                &[mode],
                if mode == 0 { b"0" } else { b"1" },
                'J',
                None,
                b"",
            );
            let expected = if mode == 0 {
                vec![(1, 2..8, 'C'), (2, 0..8, '-')]
            } else {
                vec![(0, 0..8, 'B'), (1, 0..5, 'C')]
            };
            assert_eq!(host.mutations, expected);
            assert_eq!(host.dirty_rows, 0);
            assert_eq!(
                (
                    host.screen.cursor_row,
                    host.screen.cursor_col,
                    host.screen.pending_wrap
                ),
                (1, 3, true)
            );
            assert_eq!(host.screen.grid.row_wrapped, [true, true, true]);
            assert_eq!(
                (0..3)
                    .map(|r| host.screen.grid.row_id(r))
                    .collect::<Vec<_>>(),
                ids
            );
            let blank = host.screen.grid.get(1, 3);
            assert_eq!(
                (
                    blank.character,
                    blank.foreground,
                    blank.background,
                    blank.flags
                ),
                (' ', Color::Default, Color::Blue, StyleFlags::default())
            );
        }
    }
    #[test]
    fn legacy_default_callbacks_and_metadata_only_dirty_rows_stay_distinct() {
        let mut legacy = Host::new(false);
        execute(&mut legacy, &[0], b"0", 'J', None, b"");
        assert!(legacy.mutations.is_empty());
        assert_eq!(legacy.dirty_rows, 1);
        assert_eq!(legacy.screen.grid.get(2, 0).character, ' ');
        assert!(legacy.screen.pending_wrap);
        let mut metadata = Host::new(true);
        execute(
            &mut metadata,
            &[1, 1, 1, 4, 1],
            b"1;1;1;4;1",
            'r',
            None,
            b"$",
        );
        assert!(metadata.mutations.is_empty());
        assert_eq!(metadata.dirty_rows, 1);
        assert_eq!(metadata.screen.grid.get(0, 0).character, 'A');
    }
    #[test]
    fn line_erase_and_edit_callbacks_include_only_requested_content_and_wide_repairs() {
        for (command, param, expected) in [
            ('K', 0, 3..8),
            ('K', 1, 0..4),
            ('K', 2, 0..8),
            ('X', 2, 3..5),
            ('@', 2, 3..8),
            ('P', 2, 3..8),
        ] {
            let mut host = Host::new(false);
            execute(
                &mut host,
                &[param],
                param.to_string().as_bytes(),
                command,
                None,
                b"",
            );
            assert_eq!(host.mutations, [(1, expected, 'C')]);
        }
        for command in ['K', 'X', '@', 'P'] {
            let mut host = Host::new(false);
            host.screen.grid.get_mut(1, 2).flags.set_wide(true);
            host.screen
                .grid
                .get_mut(1, 3)
                .flags
                .set_wide_continuation(true);
            execute(&mut host, &[1], b"1", command, None, b"");
            let span = &host.mutations[0].1;
            assert!(span.start <= 2 && span.end >= 4, "{command} {span:?}");
            assert!(host.screen.pending_wrap);
        }
        let mut host = Host::new(false);
        host.screen.grid.get_mut(1, 4).flags.set_wide(true);
        host.screen
            .grid
            .get_mut(1, 5)
            .flags
            .set_wide_continuation(true);
        execute(&mut host, &[2], b"2", 'X', None, b"");
        assert_eq!(host.mutations, [(1, 3..6, 'C')]);
        assert_eq!(
            *host.screen.grid.get(1, 5),
            TerminalCell {
                background: Color::Blue,
                ..TerminalCell::default()
            }
        );
    }
    #[test]
    fn legacy_orphan_edges_are_covered_and_touching_pair_branch_priority_is_preserved() {
        for command in ['K', 'X'] {
            let mut host = Host::new(false);
            // A continuation without a lead is still repaired by erase_cells.
            host.screen
                .grid
                .get_mut(1, 3)
                .flags
                .set_wide_continuation(true);
            execute(&mut host, &[0], b"0", command, None, b"");
            assert_eq!(host.mutations[0].1.start, 2);
            assert_eq!(host.screen.grid.get(1, 2).character, ' ');
        }
        for mode in [0, 1] {
            let mut host = Host::new(true);
            if mode == 0 {
                host.screen
                    .grid
                    .get_mut(1, 3)
                    .flags
                    .set_wide_continuation(true);
            } else {
                host.screen.grid.get_mut(1, 3).flags.set_wide(true);
            }
            execute(
                &mut host,
                &[mode],
                if mode == 0 { b"0" } else { b"1" },
                'J',
                None,
                b"",
            );
            let span = &host.mutations.iter().find(|entry| entry.0 == 1).unwrap().1;
            if mode == 0 {
                assert_eq!(span.start, 2);
                assert_eq!(host.screen.grid.get(1, 2).character, ' ');
            } else {
                assert_eq!(span.end, 5);
                assert_eq!(host.screen.grid.get(1, 4).character, ' ');
            }
        }
        for command in ['@', 'P'] {
            let mut host = Host::new(false);
            host.screen.grid.get_mut(1, 2).flags.set_wide(true);
            host.screen.grid.get_mut(1, 3).flags.set_wide(true);
            execute(&mut host, &[1], b"1", command, None, b"");
            // Current lead handling precedes the previous-lead fallback.
            assert_eq!(host.mutations[0].1.start, 3);
            assert_eq!(host.screen.grid.get(1, 2).character, 'B');
            assert!(host.screen.grid.get(1, 2).flags.wide());
        }
    }
}
