//! Local VT semantics. Host-dependent commands are returned without mutation.
use crate::{
    grid::{RegionDirection, RegionWrapPolicy, WideCellEditPolicy},
    parser::{Event, StringKind},
    protocol_state::ProtocolState,
    screen::{Charset, RowMutation, ScreenState},
    Color, CursorShape, RawRowId, StyleFlags, UnderlineStyle,
};
use smallvec::SmallVec;
use std::fmt::Write;
use std::ops::{Range, RangeInclusive};

#[derive(Debug, PartialEq, Eq)]
pub enum Effect {
    Row(RowMutation),
    CursorReposition { non_linear: bool },
    Reply(Vec<u8>),
    Bell,
    GlobalBackground(Color),
}
pub type Effects = SmallVec<[Effect; 2]>;
#[derive(Debug, PartialEq, Eq)]
pub enum Dispatch {
    Unhandled,
    Handled(Effects),
}

fn modify_other_keys_status(protocol: &ProtocolState) -> u16 {
    // The encoder treats all nonzero levels above one as level two.
    protocol.xterm_modify_other_keys().min(2)
}

fn format_other_keys_status(protocol: &ProtocolState) -> u8 {
    u8::from(protocol.xterm_format_other_keys() == 1)
}

fn modify_other_keys_reply(protocol: &ProtocolState) -> Vec<u8> {
    format!("\x1b[>4;{}m", modify_other_keys_status(protocol)).into_bytes()
}

fn format_other_keys_reply(protocol: &ProtocolState) -> Vec<u8> {
    format!("\x1b[>4;{}f", format_other_keys_status(protocol)).into_bytes()
}

fn tab_stop_report(screen: &ScreenState) -> Vec<u8> {
    let mut reply = String::from("\x1bP2$u");
    let mut separator = "";
    for (column, set) in screen.tab_stops.iter().enumerate().take(screen.grid.cols()) {
        if *set {
            write!(reply, "{separator}{}", column + 1).expect("write to String");
            separator = "/";
        }
    }
    reply.push_str("\x1b\\");
    reply.into_bytes()
}

fn displayed_extent_reply(screen: &ScreenState) -> Vec<u8> {
    format!(
        "\x1b[{};{};1;1;1\"w",
        screen.grid.rows(),
        screen.grid.cols()
    )
    .into_bytes()
}

/// A malformed restore cannot partially clear the existing tab settings.
pub(crate) fn restore_tab_stops(screen: &mut ScreenState, payload: &[u8]) {
    let Some(data) = payload.strip_prefix(b"2$t") else {
        return;
    };
    let mut stops = vec![false; screen.grid.cols()];
    if !data.is_empty() {
        for field in data.split(|byte| *byte == b'/') {
            if field.is_empty() {
                return;
            }
            let mut column = 0usize;
            for byte in field {
                if !byte.is_ascii_digit() {
                    return;
                }
                let Some(next) = column
                    .checked_mul(10)
                    .and_then(|n| n.checked_add(usize::from(byte - b'0')))
                else {
                    return;
                };
                column = next;
            }
            if column == 0 {
                return;
            }
            if let Some(stop) = stops.get_mut(column - 1) {
                *stop = true;
            }
        }
    }
    screen.tab_stops = stops;
}

pub(crate) struct UnderlinePen {
    pub current: Color,
    pub primary: Option<Color>,
    saved_cursor: Option<Color>,
}

impl Default for UnderlinePen {
    fn default() -> Self {
        Self {
            current: Color::Default,
            primary: None,
            saved_cursor: None,
        }
    }
}

pub(crate) fn save_cursor_state(
    screen: &mut ScreenState,
    protocol: &ProtocolState,
    underline: Option<&mut UnderlinePen>,
) {
    screen.save_cursor_state(protocol);
    if let Some(underline) = underline {
        underline.saved_cursor = Some(underline.current);
    }
}

pub(crate) fn restore_cursor_state(
    screen: &mut ScreenState,
    protocol: &mut ProtocolState,
    underline: Option<&mut UnderlinePen>,
) {
    let had_saved_cursor = screen.saved_state.is_some();
    screen.restore_cursor_state(protocol);
    if let Some(underline) = underline {
        if !had_saved_cursor {
            underline.current = Color::Default;
        } else if let Some(color) = underline.saved_cursor {
            underline.current = color;
        }
    }
}

/// CSI DA1/DA2 wire replies shared by the existing Ember and Frost policies.
///
/// These profiles ignore numeric parameters, require no intermediate bytes,
/// and do not implement other DA namespaces here. This selector neither
/// advertises capabilities for a different product nor writes to the PTY.
pub fn legacy_device_attributes_reply(
    private_prefix: Option<u8>,
    intermediates: &[u8],
) -> Option<&'static [u8]> {
    match (private_prefix, intermediates) {
        (None, b"") => Some(b"\x1b[?65;1;9c"),
        (Some(b'>'), b"") => Some(b"\x1b[>1;7802;0c"),
        _ => None,
    }
}

/// DA3 reports an anonymous terminal unit ID with the standard eight hex digits.
pub(crate) fn tertiary_device_attributes_reply(
    private_prefix: Option<u8>,
    raw_params: &[u8],
    intermediates: &[u8],
) -> Option<&'static [u8]> {
    (private_prefix == Some(b'=')
        && intermediates.is_empty()
        && raw_params.iter().all(|&byte| byte == b'0'))
    .then_some(b"\x1bP!|00000000\x1b\\")
}

/// Unimplemented DEC devices have explicit capability responses.
fn device_capability_status_reply(
    private_prefix: Option<u8>,
    params: &[u16],
    raw_params: &[u8],
    intermediates: &[u8],
) -> Option<&'static [u8]> {
    if private_prefix != Some(b'?')
        || !intermediates.is_empty()
        || raw_params.iter().any(|byte| !byte.is_ascii_digit())
    {
        return None;
    }
    match params {
        [15] => Some(b"\x1b[?13n"),
        [55] => Some(b"\x1b[?53n"),
        [56] => Some(b"\x1b[?57;0n"),
        _ => None,
    }
}

// ECMA-48 CTC character stops use the terminal's shared horizontal tab map.
// Line-specific and vertical tabulation operations are outside this model.
fn control_character_tabs(screen: &mut ScreenState, params: &[u16]) {
    for parameter in if params.is_empty() { &[0] } else { params } {
        match parameter {
            0 => screen.cursor_controls().set_tab_stop(true),
            2 => screen.clear_tab_stop(),
            5 => screen.clear_all_tab_stops(),
            _ => {}
        }
    }
}

fn dispatch_cursor_csi(
    screen: &mut ScreenState,
    protocol: &ProtocolState,
    params: &[u16],
    cmd: u8,
    private_prefix: Option<u8>,
    intermediates: &[u8],
) -> bool {
    if private_prefix.is_some() || !intermediates.is_empty() {
        return false;
    }
    let first = params.first().copied();
    let count = first.unwrap_or(1).max(1) as usize;
    match cmd {
        b'A' | b'k' => screen.move_cursor_up(count),
        b'B' | b'e' => screen.move_cursor_down(count),
        b'C' | b'a' => screen.move_cursor_forward(count),
        b'D' | b'j' => screen.move_cursor_backward_with_wrap(count, protocol),
        b'E' => {
            screen.move_cursor_down(count);
            screen.set_cursor_column(1);
        }
        b'F' => {
            screen.move_cursor_up(count);
            screen.set_cursor_column(1);
        }
        b'G' | b'`' => screen.set_cursor_column(first.unwrap_or(1) as usize),
        b'd' => screen.set_cursor_row(first.unwrap_or(1) as usize),
        b'H' | b'f' => screen.set_cursor_position(
            first.unwrap_or(1) as usize,
            params.get(1).copied().unwrap_or(1) as usize,
        ),
        b'I' => screen.tab_forward(count),
        b'Z' => screen.tab_backward(count),
        b'g' => match first.unwrap_or(0) {
            0 => screen.clear_tab_stop(),
            3 => screen.clear_all_tab_stops(),
            _ => {}
        },
        b'r' => {
            let rows = screen.grid.rows();
            let top = first.unwrap_or(1).max(1) as usize;
            let bottom = match params.get(1).copied() {
                None | Some(0) => rows,
                Some(value) => usize::from(value).min(rows),
            };
            if bottom > top {
                screen.set_scroll_region(top, bottom);
            }
        }
        _ => return false,
    }
    true
}

/// Apply Frost's local cursor, line editing, rendition and query controls.
/// Apply the returned effects before the next event. Paint-dependent queries
/// and host lifecycle commands stay unhandled.
pub fn dispatch_frost_legacy_csi(
    screen: &mut ScreenState,
    protocol: &mut ProtocolState,
    raw_params: &[u8],
    params: &[u16],
    cmd: u8,
    private_prefix: Option<u8>,
    intermediates: &[u8],
) -> Dispatch {
    if cmd == b'n' {
        if let Some(reply) =
            device_capability_status_reply(private_prefix, params, raw_params, intermediates)
        {
            return Dispatch::Handled([Effect::Reply(reply.to_vec())].into_iter().collect());
        }
    }
    if cmd == b'c' {
        if let Some(reply) =
            tertiary_device_attributes_reply(private_prefix, raw_params, intermediates)
        {
            return Dispatch::Handled([Effect::Reply(reply.to_vec())].into_iter().collect());
        }
    }
    if private_prefix.is_none() && intermediates == b"#" && matches!(cmd, b'{' | b'}' | b'p' | b'q')
    {
        let mut effects = Effects::new();
        if !raw_params.contains(&b':') {
            if matches!(cmd, b'{' | b'p') {
                protocol.sgr_stack.push(screen, params);
            } else if params.is_empty() {
                if let Some(color) = protocol.sgr_stack.pop(screen) {
                    effects.push(Effect::GlobalBackground(color));
                }
            }
        }
        return Dispatch::Handled(effects);
    }
    if cmd == b'x'
        && private_prefix.is_none()
        && intermediates == b"*"
        && params.len() <= 1
        && !raw_params.contains(&b':')
    {
        select_attribute_extent(protocol, params);
        return Dispatch::Handled(Effects::new());
    }
    if cmd == b'|' && !raw_params.contains(&b':') {
        if let Some(reply) = crate::rectangular_edit::report_rectangular_sgr(
            screen,
            params,
            '|',
            private_prefix,
            intermediates,
        ) {
            return Dispatch::Handled([Effect::Reply(reply)].into_iter().collect());
        }
    }
    if cmd == b'v'
        && private_prefix.is_none()
        && intermediates == b"\""
        && raw_params.is_empty()
        && params.is_empty()
    {
        return Dispatch::Handled(
            [Effect::Reply(displayed_extent_reply(screen))]
                .into_iter()
                .collect(),
        );
    }
    if cmd == b'w'
        && private_prefix.is_none()
        && intermediates == b"$"
        && matches!(params, [1] | [2])
        && raw_params.iter().all(u8::is_ascii_digit)
    {
        let reply = if params == [1] {
            crate::cursor_report::cursor_information_report(screen)
        } else {
            tab_stop_report(screen)
        };
        return Dispatch::Handled([Effect::Reply(reply)].into_iter().collect());
    }
    if cmd == b'W'
        && private_prefix.is_none()
        && intermediates.is_empty()
        && raw_params
            .iter()
            .all(|byte| byte.is_ascii_digit() || *byte == b';')
    {
        control_character_tabs(screen, params);
        return Dispatch::Handled(Effects::new());
    }
    if cmd == b'W'
        && private_prefix == Some(b'?')
        && intermediates.is_empty()
        && params == [5]
        && raw_params.iter().all(u8::is_ascii_digit)
    {
        screen.reset_eight_column_tab_stops();
        return Dispatch::Handled(Effects::new());
    }
    if cmd == b'g' && private_prefix == Some(b'?') && intermediates.is_empty() {
        let effects = if params == [4] && raw_params.iter().all(u8::is_ascii_digit) {
            [Effect::Reply(format_other_keys_reply(protocol))]
                .into_iter()
                .collect()
        } else {
            Effects::new()
        };
        return Dispatch::Handled(effects);
    }
    if cmd == b'f' && private_prefix == Some(b'>') && intermediates.is_empty() {
        set_xterm_format_other_keys(protocol, params);
        return Dispatch::Handled(Effects::new());
    }
    // Frost retains its original policy for the backward movement aliases.
    if !matches!(cmd, b'j' | b'k')
        && dispatch_cursor_csi(screen, protocol, params, cmd, private_prefix, intermediates)
    {
        return Dispatch::Handled(Effects::new());
    }

    if cmd == b'K' && private_prefix == Some(b'?') && intermediates.is_empty() {
        let mode = params.first().copied().unwrap_or(0);
        let effects = erase_line_span(mode, screen.cursor_col, screen.grid.row_len()).map_or_else(
            Effects::new,
            |span| {
                screen
                    .erase_span(screen.cursor_row, span.start, span.end, true)
                    .into_iter()
                    .map(Effect::Row)
                    .collect()
            },
        );
        return Dispatch::Handled(effects);
    }
    // SL/SR belongs to the ordered column dispatcher, never legacy ICH/CUU.
    if intermediates.starts_with(b" ") && matches!(cmd, b'@' | b'A') {
        return Dispatch::Unhandled;
    }
    if matches!(cmd, b'K' | b'@' | b'P' | b'X') {
        let mutation = edit_frost_legacy_line(screen, params, cmd);
        return Dispatch::Handled(mutation.into_iter().map(Effect::Row).collect());
    }

    let first = params.first().copied();
    let mut effects = Effects::new();
    let reply = match (cmd, private_prefix, intermediates, first) {
        (b'c', _, _, _) => {
            let Some(reply) = legacy_device_attributes_reply(private_prefix, intermediates) else {
                return Dispatch::Unhandled;
            };
            Some(reply.to_vec())
        }
        (b'n', None, b"", Some(5)) => Some(b"\x1b[0n".to_vec()),
        (b'n', prefix @ (None | Some(b'?')), b"", Some(6)) => {
            Some(frost_legacy_cursor_position_reply(
                screen.cursor_row,
                screen.cursor_col,
                prefix.is_some(),
            ))
        }
        (b'p', None, b"!", _) => {
            reset_local_attributes(screen, protocol);
            // Frost's native alternateScroll resource default is off.
            protocol.modes_mut().remove(&1007);
            effects.push(Effect::GlobalBackground(Color::Default));
            None
        }
        (b'p', prefix @ (None | Some(b'?')), b"$", _) => {
            let prefix = if prefix.is_some() { "?" } else { "" };
            let mut response = String::new();
            for &mode in params {
                let state = if prefix.is_empty() {
                    decrqm_ansi_mode_state(screen, mode)
                } else {
                    decrqm_private_mode_state(screen, protocol, mode, false)
                };
                write!(response, "\x1b[{prefix}{mode};{state}$y").expect("write to String");
            }
            (!response.is_empty()).then(|| response.into_bytes())
        }
        (b'm', Some(b'?'), b"", Some(4))
            if params == [4] && raw_params.iter().all(u8::is_ascii_digit) =>
        {
            Some(modify_other_keys_reply(protocol))
        }
        (b'm', Some(b'>'), b"", _) => {
            set_xterm_modify_other_keys(protocol, params);
            None
        }
        (b'm', None, b"", _) => {
            if let Some(color) = frost_sgr_with_rendition(
                raw_params,
                &mut screen.current_fg,
                &mut screen.current_bg,
                &mut screen.current_flags,
                screen.extended_rendition,
            ) {
                effects.push(Effect::GlobalBackground(color));
            }
            None
        }
        (b's', None, b"", _) => {
            screen.save_cursor(protocol.modes().contains(&7));
            None
        }
        (b'u', None, b"", _) => {
            // Frost restores the drawing state and keeps the live DECAWM.
            let _ = screen.restore_cursor();
            None
        }
        (b'u', Some(prefix), b"", _) => kitty_keyboard_csi(protocol, params, prefix),
        (b'q', None, b" ", _) => {
            if let Some(shape) = cursor_shape(first.unwrap_or(0)) {
                screen.cursor_shape = shape;
            }
            None
        }
        (b'q', None, b"\"", _) if params.len() <= 1 && !raw_params.contains(&b':') => {
            set_character_protection(screen, first.unwrap_or(0));
            None
        }
        _ => return Dispatch::Unhandled,
    };
    if let Some(reply) = reply {
        effects.push(Effect::Reply(reply));
    }
    Dispatch::Handled(effects)
}

fn set_xterm_modify_other_keys(protocol: &mut ProtocolState, params: &[u16]) {
    if params.first() == Some(&4) {
        protocol.set_xterm_modify_other_keys(params.get(1).copied().unwrap_or(0));
    }
}

fn set_xterm_format_other_keys(protocol: &mut ProtocolState, params: &[u16]) {
    if params.first() == Some(&4) {
        protocol.set_xterm_format_other_keys(params.get(1).copied().unwrap_or(0));
    }
}

fn cursor_shape(param: u16) -> Option<CursorShape> {
    match param {
        0..=2 | 7 => Some(CursorShape::Block),
        3 | 4 => Some(CursorShape::Underline),
        5 | 6 => Some(CursorShape::Beam),
        _ => None,
    }
}

fn set_character_protection(screen: &mut ScreenState, parameter: u16) {
    match parameter {
        0 | 2 => screen.current_flags.set_protected(false),
        1 => screen.current_flags.set_protected(true),
        _ => {}
    }
}

fn select_attribute_extent(protocol: &mut ProtocolState, params: &[u16]) {
    protocol.set_attribute_change_extent(params.first().copied().unwrap_or(0));
}

/// Ember-compatible semantics, including its DECSTR policy (not Frost soft reset).
/// Frost parser profiles do not select or authorize this semantic policy.
/// Dispatch exactly one event. Apply returned effects before the next event.
/// Unhandled is transactional: screen, protocol and host are all untouched.
pub fn dispatch_local(
    screen: &mut ScreenState,
    protocol: &mut ProtocolState,
    event: &Event<'_>,
) -> Dispatch {
    let extended = screen.extended_rendition;
    dispatch_local_with_rendition(screen, protocol, event, None, extended)
}

pub(crate) fn dispatch_hosted(
    screen: &mut ScreenState,
    protocol: &mut ProtocolState,
    event: &Event<'_>,
    underline: Option<&mut UnderlinePen>,
) -> Dispatch {
    dispatch_local_with_rendition(screen, protocol, event, underline, true)
}

fn dispatch_local_with_rendition(
    screen: &mut ScreenState,
    protocol: &mut ProtocolState,
    event: &Event<'_>,
    mut underline: Option<&mut UnderlinePen>,
    extended_sgr: bool,
) -> Dispatch {
    let mut effects = Effects::new();
    if let Event::Csi {
        raw_params,
        params,
        private_prefix,
        intermediates,
        final_byte: b'n',
        ..
    } = event
    {
        if let Some(reply) =
            device_capability_status_reply(*private_prefix, params, raw_params, intermediates)
        {
            effects.push(Effect::Reply(reply.to_vec()));
            return Dispatch::Handled(effects);
        }
    }
    match event {
        Event::Csi {
            params,
            colon_flags,
            private_prefix: None,
            intermediates,
            final_byte,
            ..
        } if intermediates == b"#" && matches!(final_byte, b'{' | b'}' | b'p' | b'q') => {
            if !colon_flags.iter().any(|flag| *flag) {
                if matches!(final_byte, b'{' | b'p') {
                    protocol.sgr_stack.push(screen, params);
                } else if params.is_empty() {
                    if let Some(color) = protocol.sgr_stack.pop(screen) {
                        effects.push(Effect::GlobalBackground(color));
                    }
                }
            }
        }
        Event::Csi {
            params,
            colon_flags,
            private_prefix: None,
            intermediates,
            final_byte: b'x',
            ..
        } if intermediates == b"*"
            && params.len() <= 1
            && !colon_flags.iter().any(|flag| *flag) =>
        {
            select_attribute_extent(protocol, params);
        }
        Event::Csi {
            params,
            colon_flags,
            private_prefix: None,
            intermediates,
            final_byte: b'|',
            ..
        } if intermediates == b"#" && !colon_flags.iter().any(|flag| *flag) => {
            let Some(reply) = crate::rectangular_edit::report_rectangular_sgr(
                screen,
                params,
                '|',
                None,
                intermediates,
            ) else {
                return Dispatch::Unhandled;
            };
            effects.push(Effect::Reply(reply));
        }
        Event::Csi {
            raw_params,
            params,
            private_prefix: None,
            intermediates,
            final_byte: b'v',
            ..
        } if intermediates == b"\"" && raw_params.is_empty() && params.is_empty() => {
            effects.push(Effect::Reply(displayed_extent_reply(screen)));
        }
        Event::Csi {
            raw_params,
            params,
            private_prefix: None,
            intermediates,
            final_byte: b'w',
            ..
        } if intermediates == b"$"
            && matches!(params.as_slice(), [1] | [2])
            && raw_params.iter().all(u8::is_ascii_digit) =>
        {
            let reply = if params.as_slice() == [1] {
                crate::cursor_report::cursor_information_report(screen)
            } else {
                tab_stop_report(screen)
            };
            effects.push(Effect::Reply(reply));
        }
        Event::String {
            kind: StringKind::Dcs,
            payload,
            ..
        } if payload.starts_with(b"1$t") => {
            if crate::cursor_report::restore_cursor_information(screen, &payload[3..]) {
                effects.push(Effect::CursorReposition { non_linear: true });
            }
        }
        Event::String {
            kind: StringKind::Dcs,
            payload,
            ..
        } if payload.starts_with(b"2$t") => {
            restore_tab_stops(screen, payload);
        }
        Event::String {
            kind: StringKind::Dcs,
            payload,
            ..
        } if payload.starts_with(b"+q") => {
            if let Some(reply) = crate::termcap_query::termcap_query(payload, protocol) {
                effects.push(Effect::Reply(reply.into_owned()));
            }
        }
        Event::Csi {
            raw_params,
            params,
            private_prefix: None,
            intermediates,
            final_byte: b'W',
            ..
        } if intermediates.is_empty()
            && raw_params
                .iter()
                .all(|byte| byte.is_ascii_digit() || *byte == b';') =>
        {
            control_character_tabs(screen, params);
        }
        Event::Csi {
            raw_params,
            params,
            private_prefix: Some(b'?'),
            intermediates,
            final_byte: b'W',
            ..
        } if intermediates.is_empty()
            && params.as_slice() == [5]
            && raw_params.iter().all(u8::is_ascii_digit) =>
        {
            screen.reset_eight_column_tab_stops();
        }
        Event::Csi {
            params,
            colon_flags,
            private_prefix: Some(b'?'),
            intermediates,
            final_byte: b'g',
            ..
        } if intermediates.is_empty() => {
            if params.as_slice() == [4] && !colon_flags.iter().any(|colon| *colon) {
                effects.push(Effect::Reply(format_other_keys_reply(protocol)));
            }
        }
        Event::Csi {
            params,
            colon_flags,
            private_prefix: Some(b'?'),
            intermediates,
            final_byte: b'm',
            ..
        } if params.as_slice() == [4]
            && !colon_flags.iter().any(|colon| *colon)
            && intermediates.is_empty() =>
        {
            effects.push(Effect::Reply(modify_other_keys_reply(protocol)));
        }
        Event::Csi {
            raw_params,
            private_prefix,
            intermediates,
            final_byte: b'c',
            ..
        } => {
            let reply =
                tertiary_device_attributes_reply(*private_prefix, raw_params, intermediates)
                    .or_else(|| legacy_device_attributes_reply(*private_prefix, intermediates));
            let Some(reply) = reply else {
                return Dispatch::Unhandled;
            };
            effects.push(Effect::Reply(reply.to_vec()));
        }
        Event::Csi {
            params,
            private_prefix,
            intermediates,
            final_byte: b'p',
            ..
        } if intermediates == b"$" && matches!(private_prefix, None | Some(b'?')) => {
            let mode = params.first().copied().unwrap_or(0);
            let response = match private_prefix {
                Some(b'?') => format!(
                    "\x1b[?{};{}$y",
                    mode,
                    decrqm_private_mode_state(screen, protocol, mode, true)
                ),
                None => format!("\x1b[{};{}$y", mode, decrqm_ansi_mode_state(screen, mode)),
                _ => unreachable!(),
            };
            effects.push(Effect::Reply(response.into_bytes()));
        }
        Event::Csi {
            private_prefix: None,
            intermediates,
            final_byte: b'p',
            ..
        } if intermediates == b"!" => {
            soft_reset(screen, protocol);
            if let Some(underline) = underline {
                underline.current = Color::Default;
                underline.saved_cursor = None;
            }
            effects.push(Effect::GlobalBackground(Color::Default));
        }
        Event::String {
            kind: StringKind::Dcs,
            payload,
            ..
        } if payload.starts_with(b"$q") => {
            effects.push(Effect::Reply(decrqss_reply(
                screen,
                protocol,
                &payload[2..],
                underline.as_deref().map(|pen| pen.current),
            )));
        }
        Event::Control(byte) if matches!(byte, 7 | 8 | 9 | 13 | 14 | 15) => match byte {
            7 => effects.push(Effect::Bell),
            8 => {
                let previous_row = screen.cursor_row;
                screen.move_cursor_backward_with_wrap(1, protocol);
                effects.push(Effect::CursorReposition {
                    non_linear: screen.cursor_row != previous_row,
                });
            }
            9 => {
                screen.pending_wrap = false;
                let first = screen.cursor_col;
                screen.cursor_col = screen.next_tab_stop(screen.cursor_col);
                screen
                    .grid
                    .mark_tab_spaces(screen.cursor_row, first..screen.cursor_col);
                effects.push(Effect::CursorReposition { non_linear: false });
            }
            13 => {
                screen.pending_wrap = false;
                screen.cursor_col = 0;
                effects.push(Effect::CursorReposition { non_linear: false });
            }
            14 => screen.invoke_charset(true),
            15 => screen.invoke_charset(false),
            _ => unreachable!(),
        },
        Event::Charset { bank, designator } => {
            let charset = if *designator == b'0' {
                Charset::DecSpecialGraphics
            } else {
                Charset::Ascii
            };
            if *designator == b'A' {
                screen.designate_british(u8::from(*bank != b'('));
            } else {
                screen.designate_charset(*bank != b'(', charset);
            }
        }
        Event::Escape {
            intermediates,
            final_byte,
        } if matches!(intermediates.as_slice(), b"*" | b"+")
            && matches!(final_byte, b'A' | b'B' | b'0') =>
        {
            let bank = if intermediates == b"*" { 2 } else { 3 };
            let charset = if *final_byte == b'0' {
                Charset::DecSpecialGraphics
            } else {
                Charset::Ascii
            };
            if *final_byte == b'A' {
                screen.designate_british(bank);
            } else {
                screen.designate_charset_bank(bank, charset);
            }
        }
        Event::Escape {
            intermediates,
            final_byte: b'A',
        } if matches!(intermediates.as_slice(), b"-" | b"." | b"/") => {
            screen.designate_latin1(1 + intermediates[0] - b'-');
        }
        Event::Escape {
            intermediates,
            final_byte,
        } if intermediates.is_empty()
            && matches!(final_byte, b'n' | b'o' | b'N' | b'O' | b'~' | b'}' | b'|') =>
        {
            match final_byte {
                b'n' => screen.invoke_charset_bank(2),
                b'o' => screen.invoke_charset_bank(3),
                b'N' => screen.single_shift(2),
                b'O' => screen.single_shift(3),
                b'~' | b'}' | b'|' => screen.invoke_gr_charset_bank(1 + b'~' - *final_byte),
                _ => unreachable!(),
            }
        }
        Event::Escape {
            intermediates,
            final_byte,
        } if intermediates.is_empty()
            && matches!(final_byte, b'7' | b'8' | b'H' | b'>' | b'=' | b'<') =>
        {
            match final_byte {
                b'7' => save_cursor_state(screen, protocol, underline),
                b'8' => restore_cursor_state(screen, protocol, underline),
                b'H' => {
                    if let Some(stop) = screen.tab_stops.get_mut(screen.cursor_col) {
                        *stop = true;
                    }
                }
                b'>' => {
                    protocol.modes_mut().remove(&66);
                }
                b'=' => {
                    protocol.modes_mut().insert(66);
                }
                b'<' => {}
                _ => unreachable!(),
            }
        }
        Event::Csi {
            params,
            private_prefix: Some(b'>'),
            intermediates,
            final_byte: b'f',
            ..
        } if intermediates.is_empty() => set_xterm_format_other_keys(protocol, params),
        Event::Csi {
            params,
            colon_flags,
            private_prefix,
            intermediates,
            final_byte,
            ..
        } => {
            let cmd = *final_byte;
            let plain = private_prefix.is_none() && intermediates.is_empty();
            // Classify BEFORE changing wrap/cursor or generating effects.
            let local = match cmd {
                b'A' | b'B' | b'C' | b'D' | b'E' | b'F' | b'G' | b'H' | b'I' | b'Z' | b'a'
                | b'd' | b'e' | b'f' | b'j' | b'k' | b'`' | b'g' | b'@' | b'P' | b'X' | b'r' => {
                    plain
                }
                b'm' => intermediates.is_empty() && matches!(private_prefix, None | Some(b'>')),
                b's' if *private_prefix == Some(b'?')
                    && intermediates.is_empty()
                    && !colon_flags.iter().any(|flag| *flag) =>
                {
                    false
                }
                b's' | b'u' => true,
                b'K' => intermediates.is_empty() && matches!(private_prefix, None | Some(b'?')),
                b'q' => {
                    private_prefix.is_none() && (intermediates == b" " || intermediates == b"\"")
                }
                b'n' => {
                    // Standard and DEC-private DSR have no intermediate bytes.
                    intermediates.is_empty()
                        && matches!(
                            (private_prefix, params.first().copied().unwrap_or(0)),
                            (None, 5 | 6) | (Some(b'?'), 6)
                        )
                }
                // Mode commands stay whole in the host, including mixed lists.
                _ => false,
            };
            if !local {
                return Dispatch::Unhandled;
            }
            if matches!(
                cmd,
                b'A' | b'B' | b'E' | b'F' | b'H' | b'f' | b'd' | b'e' | b'k'
            ) {
                effects.push(Effect::CursorReposition { non_linear: true });
            } else if matches!(cmd, b'C' | b'D' | b'G' | b'`' | b'I' | b'Z' | b'a' | b'j') {
                effects.push(Effect::CursorReposition { non_linear: false });
            }
            let previous_row = screen.cursor_row;
            if dispatch_cursor_csi(
                screen,
                protocol,
                params,
                cmd,
                *private_prefix,
                intermediates,
            ) {
                if screen.cursor_row != previous_row {
                    if let Some(Effect::CursorReposition { non_linear }) = effects.last_mut() {
                        *non_linear = true;
                    }
                }
                return Dispatch::Handled(effects);
            }
            let first = params.first().copied();
            let count = first.unwrap_or(1).max(1) as usize;
            match cmd {
                b'm' => {
                    if *private_prefix == Some(b'>') && intermediates.is_empty() {
                        set_xterm_modify_other_keys(protocol, params);
                    } else {
                        handle_sgr(
                            screen,
                            params,
                            colon_flags,
                            &mut effects,
                            underline.as_deref_mut().map(|pen| &mut pen.current),
                            extended_sgr,
                        );
                    }
                }
                b's' => {
                    if plain {
                        save_cursor_state(screen, protocol, underline);
                    }
                }
                b'u' => {
                    if intermediates.is_empty() {
                        match private_prefix {
                            None => restore_cursor_state(screen, protocol, underline),
                            Some(prefix) => {
                                if let Some(reply) = kitty_keyboard_csi(protocol, params, *prefix) {
                                    effects.push(Effect::Reply(reply));
                                }
                            }
                        }
                    }
                }
                b'K' => {
                    let mode = first.unwrap_or(0);
                    if mode == 0 {
                        screen.pending_wrap = false;
                    }
                    if let Some(span) =
                        erase_line_span(mode, screen.cursor_col, screen.grid.row_len())
                    {
                        effects.extend(
                            screen
                                .erase_span(
                                    screen.cursor_row,
                                    span.start,
                                    span.end,
                                    *private_prefix == Some(b'?'),
                                )
                                .into_iter()
                                .map(Effect::Row),
                        );
                    }
                }
                b'@' => {
                    if let Some(effect) = screen.insert_characters(count) {
                        effects.push(Effect::Row(effect));
                    }
                }
                b'P' => {
                    if let Some(effect) = screen.delete_characters(count) {
                        effects.push(Effect::Row(effect));
                    }
                }
                b'X' => {
                    let end = screen
                        .cursor_col
                        .saturating_add(count)
                        .min(screen.grid.row_len());
                    effects.extend(
                        screen
                            .erase_span(screen.cursor_row, screen.cursor_col, end, false)
                            .into_iter()
                            .map(Effect::Row),
                    );
                }
                b'q' => {
                    if intermediates == b" " {
                        if let Some(shape) = cursor_shape(first.unwrap_or(0)) {
                            screen.cursor_shape = shape;
                        }
                    } else {
                        set_character_protection(screen, first.unwrap_or(0));
                    }
                }
                b'n' => {
                    if first == Some(5) {
                        effects.push(Effect::Reply(b"\x1b[0n".to_vec()));
                    } else {
                        let (row, col) = reported_cursor_position(screen);
                        effects.push(Effect::Reply(cursor_position_wire(
                            row,
                            col,
                            *private_prefix == Some(b'?'),
                        )));
                    }
                }
                _ => unreachable!(),
            }
        }
        _ => return Dispatch::Unhandled,
    }
    Dispatch::Handled(effects)
}

/// Apply Frost's IL/DL row splice and return the range its viewport invalidates.
pub fn frost_legacy_splice_lines(
    screen: &mut ScreenState,
    direction: RegionDirection,
    count: usize,
) -> RangeInclusive<usize> {
    let cursor = screen.cursor_row;
    let bottom = screen.scroll_region_bottom;
    if (screen.scroll_region_top..=bottom).contains(&cursor) {
        let blank = screen.create_blank_cell();
        // Allocate each new row in order while keeping the wraps at their
        // physical indices, as in the original Frost line edits.
        for _ in 0..count.min(bottom - cursor + 1) {
            screen.grid.shift_rows(
                direction,
                cursor..=bottom,
                1,
                blank,
                RegionWrapPolicy::KeepPhysicalIndices,
                RawRowId::allocate_process_unique,
            );
        }
    }
    cursor..=bottom
}

fn erase_line_span(mode: u16, column: usize, width: usize) -> Option<Range<usize>> {
    match mode {
        0 => Some(column..width),
        1 => Some(0..column + 1),
        2 => Some(0..width),
        _ => None,
    }
}

/// Frost's inherited eraser admits orphan wide flags. Retain the shared
/// glyph-span contract while also covering the eraser's physical edge repairs.
pub(crate) fn frost_legacy_erase_damage_span(
    screen: &ScreenState,
    row: usize,
    start: usize,
    end: usize,
) -> Range<usize> {
    let width = screen.grid.cols();
    let start = start.min(width);
    let end = end.min(width);
    let (left, right) = screen.expanded_cell_mutation_span(row, start, end);
    let mut changed = left..right;
    if row < screen.grid.rows() && start < end {
        if start > 0 && screen.grid.get(row, start).flags.wide_continuation() {
            changed.start = changed.start.min(start - 1);
        }
        if end < width && screen.grid.get(row, end - 1).flags.wide() {
            changed.end = changed.end.max(end + 1);
        }
    }
    changed
}

fn edit_frost_legacy_line(
    screen: &mut ScreenState,
    params: &[u16],
    cmd: u8,
) -> Option<RowMutation> {
    let row = screen.cursor_row;
    let col = screen.cursor_col;
    let width = screen.grid.row_len();
    let first = params.first().copied();
    let count = first.unwrap_or(1).max(1) as usize;
    let blank = screen.create_blank_cell();
    let span = match cmd {
        b'K' => erase_line_span(first.unwrap_or(0), col, width)?,
        b'X' => col..col.saturating_add(count).min(width),
        b'@' | b'P' => col..width,
        _ => unreachable!("Frost line editing selector"),
    };
    let mut changed = span.clone();
    if changed.start < changed.end {
        if matches!(cmd, b'K' | b'X') {
            changed = frost_legacy_erase_damage_span(screen, row, span.start, span.end);
        } else if col > 0 {
            // Mirror erase_wide_pair_touching's ordered branches. A live lead
            // wins over an unrelated preceding lead in a malformed row.
            let current = screen.grid.get(row, col).flags;
            let clears_previous = if current.wide_continuation() {
                true
            } else if col + 1 < width && current.wide() {
                false
            } else {
                screen.grid.get(row, col - 1).flags.wide()
            };
            if clears_previous {
                changed.start -= 1;
            }
        }
    }
    match cmd {
        b'K' => {
            screen.grid.erase_cells(row, span, blank);
        }
        b'@' => {
            if !screen.insert_frost_legacy_blanks(count) {
                return None;
            }
        }
        b'P' => {
            screen.grid.delete_cells(
                row,
                col,
                count,
                blank,
                WideCellEditPolicy::ClearBoundaryPairs,
            );
        }
        b'X' => {
            screen
                .grid
                .erase_cells(row, col..col.saturating_add(count), blank);
        }
        _ => unreachable!("Frost line editing selector"),
    }
    Some(RowMutation {
        row,
        row_id: screen.grid.row_id(row),
        changed,
        printed: None,
    })
}

fn kitty_keyboard_csi(protocol: &mut ProtocolState, params: &[u16], prefix: u8) -> Option<Vec<u8>> {
    let first = params.first().copied();
    match prefix {
        b'?' => {
            return Some(format!("\x1b[?{}u", protocol.keyboard_enhancement_flags()).into_bytes())
        }
        b'=' => protocol.set_keyboard_enhancement_flags(
            first.unwrap_or(0),
            params.get(1).copied().unwrap_or(1),
        ),
        b'>' => protocol.push_keyboard_enhancement_flags(first.unwrap_or(0)),
        b'<' => protocol.pop_keyboard_enhancement_flags(first.unwrap_or(1) as usize),
        _ => {}
    }
    None
}

fn sgr_group_end(params: &[u16], colon_flags: &[bool], start: usize) -> usize {
    let mut end = start + 1;
    while end < params.len() && colon_flags.get(end).copied().unwrap_or(false) {
        end += 1;
    }
    end
}

/// A colon-owned color cannot read components across the next semicolon.
/// XTerm permits both mode:r:g:b and mode:colorspace:r:g:b; additional
/// subgroup fields are ignored without becoming independent SGR commands.
fn sgr_colon_color(values: &[u16]) -> Option<Color> {
    match values.first()? {
        5 => Some(Color::Indexed(u8::try_from(*values.get(1)?).ok()?)),
        2 => {
            let offset = if values.len() >= 5 { 2 } else { 1 };
            Some(Color::Rgb(
                u8::try_from(*values.get(offset)?).ok()?,
                u8::try_from(*values.get(offset + 1)?).ok()?,
                u8::try_from(*values.get(offset + 2)?).ok()?,
            ))
        }
        _ => None,
    }
}

/// Preserve the historical semicolon component conversion and consumption.
fn parse_sgr_semicolon_color(params: &[u16], i: usize) -> Option<(Color, usize)> {
    match params.get(i + 1).copied()? {
        5 => Some((Color::Indexed(*params.get(i + 2)? as u8), i + 2)),
        2 => Some((
            Color::Rgb(
                *params.get(i + 2)? as u8,
                *params.get(i + 3)? as u8,
                *params.get(i + 4)? as u8,
            ),
            i + 4,
        )),
        _ => None,
    }
}

const ANSI_COLORS: [Color; 8] = [
    Color::Black,
    Color::Red,
    Color::Green,
    Color::Yellow,
    Color::Blue,
    Color::Magenta,
    Color::Cyan,
    Color::White,
];
const ANSI_BRIGHT_COLORS: [Color; 8] = [
    Color::BrightBlack,
    Color::BrightRed,
    Color::BrightGreen,
    Color::BrightYellow,
    Color::BrightBlue,
    Color::BrightMagenta,
    Color::BrightCyan,
    Color::BrightWhite,
];

struct Rendition<'a> {
    foreground: &'a mut Color,
    background: &'a mut Color,
    flags: &'a mut StyleFlags,
    underline_color: Option<&'a mut Color>,
    preserve_protection: bool,
}

impl Rendition<'_> {
    fn reset(&mut self) {
        *self.flags = StyleFlags::default();
        self.flags.set_protected(self.preserve_protection);
        *self.foreground = Color::Default;
        *self.background = Color::Default;
        self.set_underline_color(Color::Default);
    }

    fn set_underline_color(&mut self, color: Color) {
        if let Some(current) = self.underline_color.as_deref_mut() {
            *current = color;
        }
    }

    fn underline(&mut self, style: u16) {
        self.flags.set_underline(match style {
            0 => UnderlineStyle::None,
            2 => UnderlineStyle::Double,
            3 => UnderlineStyle::Curly,
            4 => UnderlineStyle::Dotted,
            5 => UnderlineStyle::Dashed,
            _ => UnderlineStyle::Single,
        });
    }

    /// A new explicit background also updates the host's global background.
    /// Resets affect the pen only. Legacy profiles consume underline colors
    /// while the opted-in profile retains them for subsequent graphic cells.
    fn color(&mut self, param: u16, color: Color) -> Option<Color> {
        match param {
            38 => *self.foreground = color,
            48 => {
                *self.background = color;
                return Some(color);
            }
            58 => self.set_underline_color(color),
            _ => {}
        }
        None
    }

    fn apply(&mut self, param: u16, extended: bool) -> Option<Color> {
        match param {
            0 => self.reset(),
            1 => self.flags.set_bold(true),
            2 => self.flags.set_dim(true),
            3 => self.flags.set_italic(true),
            5 => self.flags.set_blink(true),
            7 => self.flags.set_inverse(true),
            8 | 28 if extended => self.flags.set_concealed(param == 8),
            9 => self.flags.set_strikethrough(true),
            21 => self.underline(2),
            22 => {
                self.flags.set_bold(false);
                self.flags.set_dim(false);
            }
            23 => self.flags.set_italic(false),
            24 => self.underline(0),
            25 => self.flags.set_blink(false),
            27 => self.flags.set_inverse(false),
            29 => self.flags.set_strikethrough(false),
            30..=37 => *self.foreground = ANSI_COLORS[usize::from(param - 30)],
            39 => *self.foreground = Color::Default,
            40..=47 => return self.color(48, ANSI_COLORS[usize::from(param - 40)]),
            49 => *self.background = Color::Default,
            53 | 55 if extended => self.flags.set_overline(param == 53),
            59 => self.set_underline_color(Color::Default),
            90..=97 => *self.foreground = ANSI_BRIGHT_COLORS[usize::from(param - 90)],
            100..=107 => return self.color(48, ANSI_BRIGHT_COLORS[usize::from(param - 100)]),
            _ => {}
        }
        None
    }
}

fn handle_sgr(
    screen: &mut ScreenState,
    params: &[u16],
    colon_flags: &[bool],
    effects: &mut Effects,
    underline_color: Option<&mut Color>,
    extended: bool,
) {
    // DECSCA is independent of Ember's visual rendition reset by SGR 0.
    let preserve_protection = screen.current_flags.protected();
    let mut rendition = Rendition {
        foreground: &mut screen.current_fg,
        background: &mut screen.current_bg,
        flags: &mut screen.current_flags,
        underline_color,
        preserve_protection,
    };
    if params.is_empty() {
        rendition.reset();
        return;
    }

    let mut i = 0;
    while i < params.len() {
        let param = params[i];
        let group_end = sgr_group_end(params, colon_flags, i);
        let mut next = group_end;
        let background = match param {
            4 => {
                let style = params
                    .get(i + 1)
                    .copied()
                    .filter(|style| group_end > i + 1 && *style <= 5)
                    .unwrap_or(1);
                rendition.underline(style);
                None
            }
            38 | 48 | 58 if group_end > i + 1 => sgr_colon_color(&params[i + 1..group_end])
                .and_then(|color| rendition.color(param, color)),
            38 | 48 | 58
                if i + 1 < params.len() && colon_flags.get(i + 2).copied().unwrap_or(false) =>
            {
                // Legacy mixed spelling: the mode following ';' owns a colon
                // group, as in38;2:r:g:b or38;5:index.
                next = sgr_group_end(params, colon_flags, i + 1);
                sgr_colon_color(&params[i + 1..next])
                    .and_then(|color| rendition.color(param, color))
            }
            38 | 48 | 58 => parse_sgr_semicolon_color(params, i).and_then(|(color, last)| {
                next = last + 1;
                rendition.color(param, color)
            }),
            _ => rendition.apply(param, extended),
        };
        if let Some(color) = background {
            effects.push(Effect::GlobalBackground(color));
        }
        i = next;
    }
}

#[derive(Default)]
struct FrostSgrGroup {
    values: [u16; 6],
    len: u8,
}

impl FrostSgrGroup {
    fn push(&mut self, value: u16) {
        // Frost reads at most six fields, including the optional color space.
        if let Some(slot) = self.values.get_mut(usize::from(self.len)) {
            *slot = value;
            self.len += 1;
        }
    }

    fn params(&self) -> &[u16] {
        &self.values[..usize::from(self.len)]
    }
}

/// Parse SGR parameters into groups. Top-level parameters are separated by ';';
/// within a group, ':' introduces sub-parameters (ISO 8613-6 / curly underline,
/// e.g. `4:3` or `38:2:r:g:b`). Empty fields parse as 0 so positions are preserved.
fn frost_sgr_groups(param_bytes: &[u8]) -> SmallVec<[FrostSgrGroup; 8]> {
    let mut groups = SmallVec::new();
    let mut group = FrostSgrGroup::default();
    let mut current: u16 = 0;

    for &byte in param_bytes {
        match byte {
            b'0'..=b'9' => {
                current = current
                    .saturating_mul(10)
                    .saturating_add((byte - b'0') as u16);
            }
            b':' => {
                group.push(current);
                current = 0;
            }
            b';' => {
                group.push(current);
                groups.push(std::mem::take(&mut group));
                current = 0;
            }
            _ => {}
        }
    }
    group.push(current);
    groups.push(group);
    groups
}

/// Resolve an extended color (SGR 38/48/58) from either the colon sub-parameter
/// form (within a single group, e.g. `38:2:r:g:b` or `38:2:cs:r:g:b`) or the
/// legacy semicolon form (`38;2;r;g;b`), advancing `gi` past consumed groups.
fn frost_sgr_color(groups: &[FrostSgrGroup], gi: &mut usize) -> Option<Color> {
    let g = groups[*gi].params();
    if g.len() >= 2 {
        // Colon sub-parameter form: everything lives in this group.
        match g[1] {
            5 => g.get(2).map(|&n| Color::Indexed(n as u8)),
            2 => {
                // 38:2:r:g:b (len 5) or 38:2:colorspace:r:g:b (len >= 6)
                if g.len() >= 6 {
                    Some(Color::Rgb(g[3] as u8, g[4] as u8, g[5] as u8))
                } else if g.len() >= 5 {
                    Some(Color::Rgb(g[2] as u8, g[3] as u8, g[4] as u8))
                } else {
                    None
                }
            }
            _ => None,
        }
    } else {
        // Legacy semicolon form: the kind and components are separate groups.
        let first = |idx: usize| groups.get(idx).map(|group| group.values[0]);
        match first(*gi + 1) {
            Some(5) => {
                let n = first(*gi + 2);
                *gi += 2;
                n.map(|n| Color::Indexed(n as u8))
            }
            Some(2) => {
                let (r, gg, b) = (first(*gi + 2), first(*gi + 3), first(*gi + 4));
                *gi += 4;
                match (r, gg, b) {
                    (Some(r), Some(gg), Some(b)) => Some(Color::Rgb(r as u8, gg as u8, b as u8)),
                    _ => None,
                }
            }
            _ => None,
        }
    }
}

/// Apply the Frost legacy SGR argument and reset rules to the shared pen.
/// The last explicit background is returned for the host's global background.
/// Frost treats colon groups as one parameter and consumes the requested
/// semicolon components of recognized extended colors, even if incomplete.
pub fn frost_legacy_sgr(
    raw_params: &[u8],
    foreground: &mut Color,
    background: &mut Color,
    flags: &mut StyleFlags,
) -> Option<Color> {
    frost_sgr_with_rendition(raw_params, foreground, background, flags, false)
}

fn frost_sgr_with_rendition(
    raw_params: &[u8],
    foreground: &mut Color,
    background: &mut Color,
    flags: &mut StyleFlags,
    extended: bool,
) -> Option<Color> {
    let mut rendition = Rendition {
        foreground,
        background,
        flags,
        underline_color: None,
        preserve_protection: false,
    };
    if !raw_params.iter().any(|byte| matches!(byte, b';' | b':')) {
        let param = raw_params.iter().fold(0u16, |value, byte| {
            if byte.is_ascii_digit() {
                value
                    .saturating_mul(10)
                    .saturating_add(u16::from(byte - b'0'))
            } else {
                value
            }
        });
        if param == 4 {
            rendition.underline(1);
            return None;
        }
        return rendition.apply(param, extended);
    }
    let groups = frost_sgr_groups(raw_params);
    let mut global_background = None;
    let mut i = 0;
    while let Some(group) = groups.get(i) {
        let param = group.values[0];
        let changed = match param {
            4 => {
                rendition.underline(group.params().get(1).copied().unwrap_or(1));
                None
            }
            38 | 48 | 58 => {
                frost_sgr_color(&groups, &mut i).and_then(|color| rendition.color(param, color))
            }
            _ => rendition.apply(param, extended),
        };
        if changed.is_some() {
            global_background = changed;
        }
        i += 1;
    }
    global_background
}

fn cursor_position_wire(row: usize, col: usize, private: bool) -> Vec<u8> {
    let prefix = if private { "?" } else { "" };
    format!("\x1b[{prefix}{row};{col}R").into_bytes()
}

/// Format a cursor reply under FrostLegacyV1. Frost reports the absolute,
/// occupied cell even with DECOM or deferred wrap. Preserve the historical
/// u16 narrowing for public CPR but not private DECXCPR. Coordinates are
/// zero based and come from Frost's bounded screen. The caller keeps DSR
/// admission, ordering and other host queries.
pub fn frost_legacy_cursor_position_reply(
    cursor_row: usize,
    cursor_col: usize,
    private: bool,
) -> Vec<u8> {
    let (row, col) = if private {
        (cursor_row.saturating_add(1), cursor_col.saturating_add(1))
    } else {
        (
            (cursor_row as u16).wrapping_add(1) as usize,
            (cursor_col as u16).wrapping_add(1) as usize,
        )
    };
    cursor_position_wire(row, col, private)
}

/// Cursor position reported on the wire, including the Last Column Flag.
pub fn reported_cursor_position(screen: &ScreenState) -> (usize, usize) {
    let row = if screen.origin_mode {
        screen
            .cursor_row
            .saturating_sub(screen.scroll_region_top)
            .saturating_add(1)
    } else {
        screen.cursor_row.saturating_add(1)
    };
    let col = if screen.pending_wrap {
        screen.grid.row_len().saturating_add(1)
    } else {
        screen.cursor_col.saturating_add(1)
    };
    (row.max(1), col.max(1))
}

/// DECSTR (`CSI ! p`): soft reset of the modes and drawing state DECSTR
/// is defined to touch, leaving the screen and cursor position alone.
fn soft_reset(screen: &mut ScreenState, protocol: &mut ProtocolState) {
    reset_local_attributes(screen, protocol);
    // Ember resets application cursor keys and keypad and restores Alternate
    // Scroll to its resource default.
    protocol.modes_mut().insert(1007);
    protocol.modes_mut().remove(&1);
    protocol.modes_mut().remove(&66);
}

fn reset_local_attributes(screen: &mut ScreenState, protocol: &mut ProtocolState) {
    screen.current_fg = Color::Default;
    screen.current_bg = Color::Default;
    screen.current_flags = StyleFlags::default();
    screen.reset_charset_banks();
    screen.scroll_region_top = 0;
    screen.scroll_region_bottom = screen.grid.rows().saturating_sub(1);
    screen.saved_state = None;
    screen.origin_mode = false;
    screen.insert_mode = false;
    protocol.modes_mut().insert(25);
    protocol.modes_mut().insert(7);
    protocol.modes_mut().remove(&45);
    screen.pending_wrap = false;
}

fn decrqss_reply(
    screen: &ScreenState,
    protocol: &ProtocolState,
    pt: &[u8],
    underline_color: Option<Color>,
) -> Vec<u8> {
    let reply = match pt {
        [b'>', zeros @ .., b'4', final_byte @ (b'm' | b'f')]
            if zeros.iter().all(|byte| *byte == b'0') =>
        {
            let status = if *final_byte == b'm' {
                modify_other_keys_status(protocol)
            } else {
                u16::from(format_other_keys_status(protocol))
            };
            format!("\x1bP1$r>4;{status}{}\x1b\\", char::from(*final_byte))
        }
        b" q" => {
            let n = match screen.cursor_shape {
                CursorShape::Underline => 4,
                CursorShape::Beam => 6,
                _ => 2,
            };
            format!("\x1bP1$r{n} q\x1b\\")
        }
        b"*x" => format!("\x1bP1$r{}*x\x1b\\", protocol.attribute_change_extent()),
        b"*|" => format!("\x1bP1$r{}*|\x1b\\", screen.grid.rows()),
        b"r" => format!(
            "\x1bP1$r{};{}r\x1b\\",
            screen.scroll_region_top + 1,
            screen.scroll_region_bottom + 1
        ),
        b"\"q" => {
            let n = if screen.current_flags.protected() {
                1
            } else {
                0
            };
            format!("\x1bP1$r{n}\"q\x1b\\")
        }
        b"m" => format!(
            "\x1bP1$r{}\x1b\\",
            decrqss_sgr_payload(screen, underline_color)
        ),
        b"\"p" => "\x1bP1$r65;1\"p\x1b\\".to_string(),
        _ => "\x1bP0$r\x1b\\".to_string(),
    };
    reply.into_bytes()
}

/// Hosted and newly supported profiles report a replayable SGR state.
pub(crate) fn restorable_decrqss_reply(
    screen: &ScreenState,
    protocol: &ProtocolState,
    pt: &[u8],
    underline_color: Option<Color>,
) -> Vec<u8> {
    if pt != b"m" {
        return decrqss_reply(screen, protocol, pt, underline_color);
    }
    let sgr = decrqss_sgr_payload(screen, underline_color);
    if sgr == "0m" {
        return b"\x1bP1$r0m\x1b\\".to_vec();
    }
    format!("\x1bP1$r0;{sgr}\x1b\\").into_bytes()
}

fn decrqss_sgr_payload(screen: &ScreenState, underline_color: Option<Color>) -> String {
    let mut params: Vec<String> = Vec::new();
    if screen.current_flags.bold() {
        params.push("1".into());
    }
    if screen.current_flags.dim() {
        params.push("2".into());
    }
    if screen.current_flags.italic() {
        params.push("3".into());
    }
    match screen.current_flags.underline() {
        UnderlineStyle::None => {}
        UnderlineStyle::Single => params.push("4".into()),
        UnderlineStyle::Double => params.push("21".into()),
        UnderlineStyle::Curly => params.push("4:3".into()),
        UnderlineStyle::Dotted => params.push("4:4".into()),
        UnderlineStyle::Dashed => params.push("4:5".into()),
    }
    if screen.current_flags.blink() {
        params.push("5".into());
    }
    if screen.current_flags.inverse() {
        params.push("7".into());
    }
    if screen.current_flags.concealed() {
        params.push("8".into());
    }
    if screen.current_flags.strikethrough() {
        params.push("9".into());
    }
    params.extend(sgr_color_params(screen.current_fg, true));
    params.extend(sgr_color_params(screen.current_bg, false));
    if screen.current_flags.overline() {
        params.push("53".into());
    }
    match underline_color {
        Some(Color::Indexed(index)) => params.extend(["58".into(), "5".into(), index.to_string()]),
        Some(Color::Rgb(red, green, blue)) => params.extend([
            "58".into(),
            "2".into(),
            red.to_string(),
            green.to_string(),
            blue.to_string(),
        ]),
        _ => {}
    }
    if params.is_empty() {
        "0m".into()
    } else {
        format!("{}m", params.join(";"))
    }
}

fn sgr_color_params(color: Color, foreground: bool) -> Vec<String> {
    let named = |n: u8| {
        vec![if foreground {
            n.to_string()
        } else {
            (n + 10).to_string()
        }]
    };
    match color {
        Color::Default => Vec::new(),
        Color::Black => named(30),
        Color::Red => named(31),
        Color::Green => named(32),
        Color::Yellow => named(33),
        Color::Blue => named(34),
        Color::Magenta => named(35),
        Color::Cyan => named(36),
        Color::White => named(37),
        Color::BrightBlack => named(90),
        Color::BrightRed => named(91),
        Color::BrightGreen => named(92),
        Color::BrightYellow => named(93),
        Color::BrightBlue => named(94),
        Color::BrightMagenta => named(95),
        Color::BrightCyan => named(96),
        Color::BrightWhite => named(97),
        Color::Indexed(index) => vec![
            if foreground { "38" } else { "48" }.into(),
            "5".into(),
            index.to_string(),
        ],
        Color::Rgb(red, green, blue) => vec![
            if foreground { "38" } else { "48" }.into(),
            "2".into(),
            red.to_string(),
            green.to_string(),
            blue.to_string(),
        ],
    }
}

fn decrqm_private_mode_state(
    screen: &ScreenState,
    protocol: &ProtocolState,
    mode: u16,
    ember_extensions: bool,
) -> u8 {
    let set = match mode {
        6 => screen.origin_mode,
        1
        | 7
        | 9
        | 25
        | 42
        | 45
        | 47
        | 66
        | 1000..=1007
        | 1015
        | 1047..=1049
        | 2004
        | 2026
        | 2031
        | 5522 => protocol.modes().contains(&mode),
        12 | 1034 | 1036 if ember_extensions => protocol.modes().contains(&mode),
        _ => return 0,
    };
    if set {
        1
    } else {
        2
    }
}

fn decrqm_ansi_mode_state(screen: &ScreenState, mode: u16) -> u8 {
    match mode {
        4 if screen.insert_mode => 1,
        4 => 2,
        20 => 4,
        _ => 0,
    }
}

#[cfg(test)]
mod unsaved_cursor_tests {
    use super::*;
    #[test]
    fn missing_cursor_slot_resets_optional_underline_pen_without_consuming_saved_slots() {
        let mut screen = ScreenState::new(3, 6);
        let mut protocol = ProtocolState::default();
        let mut underline = UnderlinePen {
            current: Color::Rgb(1, 2, 3),
            primary: Some(Color::Blue),
            saved_cursor: Some(Color::Red),
        };
        restore_cursor_state(&mut screen, &mut protocol, Some(&mut underline));
        assert_eq!(underline.current, Color::Default);
        assert_eq!(underline.primary, Some(Color::Blue));
        assert_eq!(underline.saved_cursor, Some(Color::Red));
        underline.current = Color::Green;
        save_cursor_state(&mut screen, &protocol, Some(&mut underline));
        underline.current = Color::Blue;
        restore_cursor_state(&mut screen, &mut protocol, Some(&mut underline));
        assert_eq!(underline.current, Color::Green);
        assert!(screen.saved_state.is_some());
    }
}
