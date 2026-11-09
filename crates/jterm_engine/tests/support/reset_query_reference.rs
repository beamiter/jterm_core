//! Frozen original Ember algorithms from candidate cdee016c, copied from
//! scratch/reset-query-oracles/src/terminal/reset_query_oracles.rs before extraction.
//! Only the owner wrapper, imports and entry visibility differ. Never delegate
//! this oracle to production semantics or regenerate it from extracted code.
use jterm_engine::{
    protocol_state::ProtocolState,
    screen::{Charset, ScreenState},
    Color, CursorShape, StyleFlags, UnderlineStyle,
};
pub struct Original {
    pub screen: ScreenState,
    pub protocol: ProtocolState,
    pub global_bg: Color,
    pub output_buffer: Vec<u8>,
}
impl Original {
    pub fn frozen_soft_reset(&mut self) {
        self.screen.current_fg = Color::Default;
        self.screen.current_bg = Color::Default;
        self.global_bg = Color::Default;
        self.screen.current_flags = StyleFlags::default();
        self.screen.g0_charset = Charset::Ascii;
        self.screen.g1_charset = Charset::Ascii;
        self.screen.active_charset = Charset::Ascii;
        self.screen.scroll_region_top = 0;
        self.screen.scroll_region_bottom = self.screen.grid.rows().saturating_sub(1);
        self.screen.saved_state = None;
        self.screen.origin_mode = false;
        self.screen.insert_mode = false;
        // DECTCEM and DECAWM on, DECCKM (application cursor keys) off,
        // DECKPAM (application keypad) off. Alternate Scroll returns to the
        // xterm-resource default (enabled).
        self.protocol.modes_mut().insert(25);
        self.protocol.modes_mut().insert(7);
        self.protocol.modes_mut().insert(1007);
        self.protocol.modes_mut().remove(&1);
        self.protocol.modes_mut().remove(&66);
        self.screen.pending_wrap = false;
    }

    pub fn frozen_handle_decrqss(&mut self, pt: &[u8]) {
        let reply = match pt {
            b" q" => {
                let n = match self.screen.cursor_shape {
                    CursorShape::Underline => 4,
                    CursorShape::Beam => 6,
                    _ => 2,
                };
                format!("\x1bP1$r{n} q\x1b\\")
            }
            b"r" => format!(
                "\x1bP1$r{};{}r\x1b\\",
                self.screen.scroll_region_top + 1,
                self.screen.scroll_region_bottom + 1
            ),
            b"\"q" => {
                let n = if self.screen.current_flags.protected() {
                    1
                } else {
                    0
                };
                format!("\x1bP1$r{n}\"q\x1b\\")
            }
            b"m" => format!("\x1bP1$r{}\x1b\\", self.frozen_decrqss_sgr_payload()),
            b"\"p" => "\x1bP1$r65;1\"p\x1b\\".to_string(),
            _ => "\x1bP0$r\x1b\\".to_string(),
        };
        self.output_buffer.extend_from_slice(reply.as_bytes());
    }

    fn frozen_decrqss_sgr_payload(&self) -> String {
        let mut params: Vec<String> = Vec::new();
        if self.screen.current_flags.bold() {
            params.push("1".into());
        }
        if self.screen.current_flags.dim() {
            params.push("2".into());
        }
        if self.screen.current_flags.italic() {
            params.push("3".into());
        }
        match self.screen.current_flags.underline() {
            UnderlineStyle::None => {}
            UnderlineStyle::Single => params.push("4".into()),
            UnderlineStyle::Double => params.push("21".into()),
            UnderlineStyle::Curly => params.push("4:3".into()),
            UnderlineStyle::Dotted => params.push("4:4".into()),
            UnderlineStyle::Dashed => params.push("4:5".into()),
        }
        if self.screen.current_flags.blink() {
            params.push("5".into());
        }
        if self.screen.current_flags.inverse() {
            params.push("7".into());
        }
        if self.screen.current_flags.strikethrough() {
            params.push("9".into());
        }
        params.extend(Self::frozen_sgr_color_params(self.screen.current_fg, true));
        params.extend(Self::frozen_sgr_color_params(self.screen.current_bg, false));
        if params.is_empty() {
            "0m".into()
        } else {
            format!("{}m", params.join(";"))
        }
    }

    fn frozen_sgr_color_params(color: Color, foreground: bool) -> Vec<String> {
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
}
