//! Local VT semantics. Host-dependent commands are returned without mutation.
use crate::{
    parser::{Event, StringKind},
    protocol_state::ProtocolState,
    screen::{Charset, RowMutation, ScreenState},
    Color, CursorShape, StyleFlags, UnderlineStyle,
};
use smallvec::SmallVec;

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

/// Ember-compatible semantics, including its DECSTR policy (not Frost soft reset).
/// Frost parser profiles do not select or authorize this semantic policy.
/// Dispatch exactly one event. Apply returned effects before the next event.
/// Unhandled is transactional: screen, protocol and host are all untouched.
pub fn dispatch_local(
    screen: &mut ScreenState,
    protocol: &mut ProtocolState,
    event: &Event<'_>,
) -> Dispatch {
    let mut effects = Effects::new();
    match event {
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
                    decrqm_private_mode_state(screen, protocol, mode)
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
            effects.push(Effect::GlobalBackground(Color::Default));
        }
        Event::String {
            kind: StringKind::Dcs,
            payload,
            ..
        } if payload.starts_with(b"$q") => {
            effects.push(Effect::Reply(decrqss_reply(screen, &payload[2..])));
        }
        Event::Control(byte) if matches!(byte, 7 | 8 | 9 | 13 | 14 | 15) => match byte {
            7 => effects.push(Effect::Bell),
            8 => {
                screen.pending_wrap = false;
                screen.cursor_col = screen.cursor_col.saturating_sub(1);
                effects.push(Effect::CursorReposition { non_linear: false });
            }
            9 => {
                screen.pending_wrap = false;
                screen.cursor_col = screen.next_tab_stop(screen.cursor_col);
                effects.push(Effect::CursorReposition { non_linear: false });
            }
            13 => {
                screen.pending_wrap = false;
                screen.cursor_col = 0;
                effects.push(Effect::CursorReposition { non_linear: false });
            }
            14 => screen.active_charset = screen.g1_charset,
            15 => screen.active_charset = screen.g0_charset,
            _ => unreachable!(),
        },
        Event::Charset { bank, designator } => {
            let charset = if *designator == b'0' {
                Charset::DecSpecialGraphics
            } else {
                Charset::Ascii
            };
            if *bank == b'(' {
                screen.g0_charset = charset;
                screen.active_charset = charset;
            } else {
                screen.g1_charset = charset;
            }
        }
        Event::Escape {
            intermediates,
            final_byte,
        } if intermediates.is_empty()
            && matches!(final_byte, b'7' | b'8' | b'H' | b'>' | b'=' | b'<') =>
        {
            match final_byte {
                b'7' => screen.save_cursor_state(protocol),
                b'8' => screen.restore_cursor_state(protocol),
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
                | b'd' | b'e' | b'f' | b'g' | b'j' | b'k' | b'`' | b'm' | b's' | b'u' => true,
                b'K' => intermediates.is_empty() && matches!(private_prefix, None | Some(b'?')),
                b'@' | b'P' | b'X' | b'r' => plain,
                b'q' => {
                    private_prefix.is_none() && (intermediates == b" " || intermediates == b"\"")
                }
                b'n' => matches!(
                    (private_prefix, params.first().copied().unwrap_or(0)),
                    (None, 5 | 6) | (Some(b'?'), 6)
                ),
                // Mode commands stay whole in the host, including mixed lists.
                _ => false,
            };
            if !local {
                return Dispatch::Unhandled;
            }
            if matches!(
                cmd,
                b'A' | b'B'
                    | b'C'
                    | b'D'
                    | b'E'
                    | b'F'
                    | b'G'
                    | b'H'
                    | b'f'
                    | b'd'
                    | b'`'
                    | b'I'
                    | b'Z'
                    | b'a'
                    | b'e'
                    | b'j'
                    | b'k'
            ) {
                screen.pending_wrap = false;
            }
            if matches!(
                cmd,
                b'A' | b'B' | b'E' | b'F' | b'H' | b'f' | b'd' | b'e' | b'k'
            ) {
                effects.push(Effect::CursorReposition { non_linear: true });
            } else if matches!(cmd, b'C' | b'D' | b'G' | b'`' | b'I' | b'Z' | b'a' | b'j') {
                effects.push(Effect::CursorReposition { non_linear: false });
            }
            let first = params.first().copied();
            let count = first.unwrap_or(1).max(1) as usize;
            match cmd {
                b'A' | b'k' => screen.move_cursor_up(count),
                b'B' | b'e' => screen.move_cursor_down(count),
                b'C' | b'a' => screen.move_cursor_forward(count),
                b'D' | b'j' => screen.move_cursor_backward(count),
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
                b'H' => screen.set_cursor_position(
                    first.unwrap_or(1) as usize,
                    params.get(1).copied().unwrap_or(1) as usize,
                ),
                b'f' => {
                    if *private_prefix == Some(b'>') && intermediates.is_empty() {
                        if first.unwrap_or(0) == 4 {
                            protocol
                                .set_xterm_format_other_keys(params.get(1).copied().unwrap_or(0));
                        }
                    } else {
                        screen.set_cursor_position(
                            first.unwrap_or(1) as usize,
                            params.get(1).copied().unwrap_or(1) as usize,
                        );
                    }
                }
                b'I' => screen.tab_forward(count),
                b'Z' => screen.tab_backward(count),
                b'g' => match first.unwrap_or(0) {
                    0 => screen.clear_tab_stop(),
                    3 => screen.clear_all_tab_stops(),
                    _ => {}
                },
                b'm' => {
                    if *private_prefix == Some(b'>') && intermediates.is_empty() {
                        if first.unwrap_or(0) == 4 {
                            protocol
                                .set_xterm_modify_other_keys(params.get(1).copied().unwrap_or(0));
                        }
                    } else {
                        handle_sgr(screen, params, colon_flags, &mut effects);
                    }
                }
                b's' => {
                    if plain {
                        screen.save_cursor_state(protocol);
                    }
                }
                b'u' => {
                    if intermediates.is_empty() {
                        match private_prefix {
                            None => screen.restore_cursor_state(protocol),
                            Some(b'?') => effects.push(Effect::Reply(
                                format!("\x1b[?{}u", protocol.keyboard_enhancement_flags())
                                    .into_bytes(),
                            )),
                            Some(b'=') => protocol.set_keyboard_enhancement_flags(
                                first.unwrap_or(0),
                                params.get(1).copied().unwrap_or(1),
                            ),
                            Some(b'>') => {
                                protocol.push_keyboard_enhancement_flags(first.unwrap_or(0))
                            }
                            Some(b'<') => {
                                protocol.pop_keyboard_enhancement_flags(first.unwrap_or(1) as usize)
                            }
                            _ => {}
                        }
                    }
                }
                b'K' => {
                    let span = match first.unwrap_or(0) {
                        0 => {
                            screen.pending_wrap = false;
                            Some((screen.cursor_col, screen.grid.row_len()))
                        }
                        1 => Some((0, screen.cursor_col + 1)),
                        2 => Some((0, screen.grid.row_len())),
                        _ => None,
                    };
                    if let Some((start, end)) = span {
                        effects.extend(
                            screen
                                .erase_span(
                                    screen.cursor_row,
                                    start,
                                    end,
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
                b'r' => {
                    let top = first.unwrap_or(1).max(1) as usize;
                    let bottom = match params.get(1).copied().unwrap_or(screen.grid.rows() as u16) {
                        0 => screen.grid.rows(),
                        v => v as usize,
                    };
                    screen.set_scroll_region(top, bottom);
                }
                b'q' => {
                    if intermediates == b" " {
                        screen.cursor_shape = match first.unwrap_or(0) {
                            3 | 4 => CursorShape::Underline,
                            5 | 6 => CursorShape::Beam,
                            _ => CursorShape::Block,
                        };
                    } else {
                        match first.unwrap_or(0) {
                            0 | 2 => screen.current_flags.set_protected(false),
                            1 => screen.current_flags.set_protected(true),
                            _ => {}
                        }
                    }
                }
                b'n' => {
                    if first == Some(5) {
                        effects.push(Effect::Reply(b"\x1b[0n".to_vec()));
                    } else {
                        let (row, col) = reported_cursor_position(screen);
                        let prefix = if *private_prefix == Some(b'?') {
                            "?"
                        } else {
                            ""
                        };
                        effects.push(Effect::Reply(
                            format!("\x1b[{prefix}{row};{col}R").into_bytes(),
                        ));
                    }
                }
                _ => unreachable!(),
            }
        }
        _ => return Dispatch::Unhandled,
    }
    Dispatch::Handled(effects)
}

fn parse_sgr_extended_color(
    params: &[u16],
    colon_flags: &[bool],
    i: usize,
) -> Option<(Color, usize)> {
    match params.get(i + 1).copied()? {
        5 => {
            let idx = params.get(i + 2).copied()? as u8;
            Some((Color::Indexed(idx), i + 2))
        }
        2 => {
            // 计算从 i 起的冒号连续子参数组长度(组内各参数 colon_flag 为 true)。
            let mut end = i + 1;
            while end < params.len() && colon_flags.get(end).copied().unwrap_or(false) {
                end += 1;
            }
            let colon_form = colon_flags.get(i + 1).copied().unwrap_or(false);
            // 冒号形式且组长 >=6 表示存在颜色空间字段(38,2,cs,r,g,b),r 偏移为 3;
            // 否则(传统分号形式或无 cs 的冒号形式)r 偏移为 2。
            let r_off = if colon_form && (end - i) >= 6 { 3 } else { 2 };
            let r = params.get(i + r_off).copied()?;
            let g = params.get(i + r_off + 1).copied()?;
            let b = params.get(i + r_off + 2).copied()?;
            Some((Color::Rgb(r as u8, g as u8, b as u8), i + r_off + 2))
        }
        _ => None,
    }
}
fn handle_sgr(
    screen: &mut ScreenState,
    params: &[u16],
    colon_flags: &[bool],
    effects: &mut Effects,
) {
    // DECSCA is independent of the visual rendition reset by SGR 0.
    let protected = screen.current_flags.protected();
    if params.is_empty() {
        screen.current_flags = StyleFlags::default();
        screen.current_flags.set_protected(protected);
        screen.current_fg = Color::Default;
        screen.current_bg = Color::Default;
        return;
    }

    let mut i = 0;
    while i < params.len() {
        let param = params[i];
        match param {
            0 => {
                screen.current_flags = StyleFlags::default();
                screen.current_flags.set_protected(protected);
                screen.current_fg = Color::Default;
                screen.current_bg = Color::Default;
            }
            1 => screen.current_flags.set_bold(true),
            2 => screen.current_flags.set_dim(true),
            3 => screen.current_flags.set_italic(true),
            4 => {
                // 仅当下一个参数是冒号子参数(`4:x`)时才作为扩展下划线样式;
                // 分号分隔的 `4;x` 中 x 是独立 SGR(如 4;1 = 下划线+粗体),不能吞。
                let next_is_substyle = i + 1 < params.len()
                    && colon_flags.get(i + 1).copied().unwrap_or(false)
                    && params[i + 1] <= 5;
                if next_is_substyle {
                    let style = params[i + 1];
                    screen.current_flags.set_underline(match style {
                        0 => UnderlineStyle::None,
                        1 => UnderlineStyle::Single,
                        2 => UnderlineStyle::Double,
                        3 => UnderlineStyle::Curly,
                        4 => UnderlineStyle::Dotted,
                        5 => UnderlineStyle::Dashed,
                        _ => UnderlineStyle::Single,
                    });
                    i += 1;
                } else {
                    screen.current_flags.set_underline(UnderlineStyle::Single);
                }
            }
            5 => screen.current_flags.set_blink(true),
            7 => screen.current_flags.set_inverse(true),
            9 => screen.current_flags.set_strikethrough(true),
            21 => screen.current_flags.set_underline(UnderlineStyle::Double),
            22 => {
                screen.current_flags.set_bold(false);
                screen.current_flags.set_dim(false);
            }
            23 => screen.current_flags.set_italic(false),
            24 => screen.current_flags.set_underline(UnderlineStyle::None),
            25 => screen.current_flags.set_blink(false),
            27 => screen.current_flags.set_inverse(false),
            29 => screen.current_flags.set_strikethrough(false),
            39 => screen.current_fg = Color::Default,
            30..=37 => {
                screen.current_fg = match param {
                    30 => Color::Black,
                    31 => Color::Red,
                    32 => Color::Green,
                    33 => Color::Yellow,
                    34 => Color::Blue,
                    35 => Color::Magenta,
                    36 => Color::Cyan,
                    37 => Color::White,
                    _ => Color::Default,
                };
            }
            49 => screen.current_bg = Color::Default,
            40..=47 => {
                screen.current_bg = match param {
                    40 => Color::Black,
                    41 => Color::Red,
                    42 => Color::Green,
                    43 => Color::Yellow,
                    44 => Color::Blue,
                    45 => Color::Magenta,
                    46 => Color::Cyan,
                    47 => Color::White,
                    _ => Color::Default,
                };
                effects.push(Effect::GlobalBackground(screen.current_bg)); // Update global background
            }
            90..=97 => {
                screen.current_fg = match param {
                    90 => Color::BrightBlack,
                    91 => Color::BrightRed,
                    92 => Color::BrightGreen,
                    93 => Color::BrightYellow,
                    94 => Color::BrightBlue,
                    95 => Color::BrightMagenta,
                    96 => Color::BrightCyan,
                    97 => Color::BrightWhite,
                    _ => Color::Default,
                };
            }
            100..=107 => {
                screen.current_bg = match param {
                    100 => Color::BrightBlack,
                    101 => Color::BrightRed,
                    102 => Color::BrightGreen,
                    103 => Color::BrightYellow,
                    104 => Color::BrightBlue,
                    105 => Color::BrightMagenta,
                    106 => Color::BrightCyan,
                    107 => Color::BrightWhite,
                    _ => Color::Default,
                };
                effects.push(Effect::GlobalBackground(screen.current_bg)); // Update global background
            }
            // 扩展前景色:38;5;n / 38;2;r;g;b 及对应冒号形式
            38 => {
                if let Some((color, last)) = parse_sgr_extended_color(params, colon_flags, i) {
                    screen.current_fg = color;
                    i = last;
                }
            }
            // 扩展背景色
            48 => {
                if let Some((color, last)) = parse_sgr_extended_color(params, colon_flags, i) {
                    screen.current_bg = color;
                    effects.push(Effect::GlobalBackground(screen.current_bg));
                    i = last;
                }
            }
            // 58 设置下划线颜色 / 59 复位。当前渲染管线未存储独立下划线颜色,
            // 故仅正确消费其子参数,避免颜色分量泄漏成后续 SGR 码被误解析。
            58 => {
                if let Some((_color, last)) = parse_sgr_extended_color(params, colon_flags, i) {
                    i = last;
                }
            }
            59 => {}
            _ => {}
        }
        i += 1;
    }
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
    screen.current_fg = Color::Default;
    screen.current_bg = Color::Default;
    screen.current_flags = StyleFlags::default();
    screen.g0_charset = Charset::Ascii;
    screen.g1_charset = Charset::Ascii;
    screen.active_charset = Charset::Ascii;
    screen.scroll_region_top = 0;
    screen.scroll_region_bottom = screen.grid.rows().saturating_sub(1);
    screen.saved_state = None;
    screen.origin_mode = false;
    screen.insert_mode = false;
    // DECTCEM and DECAWM on, DECCKM (application cursor keys) off,
    // DECKPAM (application keypad) off. Alternate Scroll returns to the
    // xterm-resource default (enabled).
    protocol.modes_mut().insert(25);
    protocol.modes_mut().insert(7);
    protocol.modes_mut().insert(1007);
    protocol.modes_mut().remove(&1);
    protocol.modes_mut().remove(&66);
    screen.pending_wrap = false;
}

fn decrqss_reply(screen: &ScreenState, pt: &[u8]) -> Vec<u8> {
    let reply = match pt {
        b" q" => {
            let n = match screen.cursor_shape {
                CursorShape::Underline => 4,
                CursorShape::Beam => 6,
                _ => 2,
            };
            format!("\x1bP1$r{n} q\x1b\\")
        }
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
        b"m" => format!("\x1bP1$r{}\x1b\\", decrqss_sgr_payload(screen)),
        b"\"p" => "\x1bP1$r65;1\"p\x1b\\".to_string(),
        _ => "\x1bP0$r\x1b\\".to_string(),
    };
    reply.into_bytes()
}

fn decrqss_sgr_payload(screen: &ScreenState) -> String {
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
    if screen.current_flags.strikethrough() {
        params.push("9".into());
    }
    params.extend(sgr_color_params(screen.current_fg, true));
    params.extend(sgr_color_params(screen.current_bg, false));
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

// Ember mode-query compatibility policy. Unsupported modes stay unrecognized.
fn decrqm_private_mode_state(screen: &ScreenState, protocol: &ProtocolState, mode: u16) -> u8 {
    let set = match mode {
        6 => screen.origin_mode,
        1
        | 7
        | 12
        | 25
        | 47
        | 66
        | 1000..=1007
        | 1015
        | 1034
        | 1036
        | 1047..=1049
        | 2004
        | 2026
        | 2031
        | 5522 => protocol.modes().contains(&mode),
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
