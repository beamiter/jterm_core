use jterm_engine::{screen::ScreenState, Color, StyleFlags, UnderlineStyle};
pub struct Original {
    pub screen: ScreenState,
    pub global_bg: Color,
}
impl Original {
    pub fn parse_sgr_extended_color(
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
    pub fn handle_sgr(&mut self, params: &[u16], colon_flags: &[bool]) {
        // DECSCA is independent of the visual rendition reset by SGR 0.
        let protected = self.screen.current_flags.protected();
        if params.is_empty() {
            self.screen.current_flags = StyleFlags::default();
            self.screen.current_flags.set_protected(protected);
            self.screen.current_fg = Color::Default;
            self.screen.current_bg = Color::Default;
            return;
        }

        let mut i = 0;
        while i < params.len() {
            let param = params[i];
            match param {
                0 => {
                    self.screen.current_flags = StyleFlags::default();
                    self.screen.current_flags.set_protected(protected);
                    self.screen.current_fg = Color::Default;
                    self.screen.current_bg = Color::Default;
                }
                1 => self.screen.current_flags.set_bold(true),
                2 => self.screen.current_flags.set_dim(true),
                3 => self.screen.current_flags.set_italic(true),
                4 => {
                    // 仅当下一个参数是冒号子参数(`4:x`)时才作为扩展下划线样式;
                    // 分号分隔的 `4;x` 中 x 是独立 SGR(如 4;1 = 下划线+粗体),不能吞。
                    let next_is_substyle = i + 1 < params.len()
                        && colon_flags.get(i + 1).copied().unwrap_or(false)
                        && params[i + 1] <= 5;
                    if next_is_substyle {
                        let style = params[i + 1];
                        self.screen.current_flags.set_underline(match style {
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
                        self.screen
                            .current_flags
                            .set_underline(UnderlineStyle::Single);
                    }
                }
                5 => self.screen.current_flags.set_blink(true),
                7 => self.screen.current_flags.set_inverse(true),
                9 => self.screen.current_flags.set_strikethrough(true),
                21 => self
                    .screen
                    .current_flags
                    .set_underline(UnderlineStyle::Double),
                22 => {
                    self.screen.current_flags.set_bold(false);
                    self.screen.current_flags.set_dim(false);
                }
                23 => self.screen.current_flags.set_italic(false),
                24 => self
                    .screen
                    .current_flags
                    .set_underline(UnderlineStyle::None),
                25 => self.screen.current_flags.set_blink(false),
                27 => self.screen.current_flags.set_inverse(false),
                29 => self.screen.current_flags.set_strikethrough(false),
                39 => self.screen.current_fg = Color::Default,
                30..=37 => {
                    self.screen.current_fg = match param {
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
                49 => self.screen.current_bg = Color::Default,
                40..=47 => {
                    self.screen.current_bg = match param {
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
                    self.global_bg = self.screen.current_bg; // Update global background
                }
                90..=97 => {
                    self.screen.current_fg = match param {
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
                    self.screen.current_bg = match param {
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
                    self.global_bg = self.screen.current_bg; // Update global background
                }
                // 扩展前景色:38;5;n / 38;2;r;g;b 及对应冒号形式
                38 => {
                    if let Some((color, last)) =
                        Self::parse_sgr_extended_color(params, colon_flags, i)
                    {
                        self.screen.current_fg = color;
                        i = last;
                    }
                }
                // 扩展背景色
                48 => {
                    if let Some((color, last)) =
                        Self::parse_sgr_extended_color(params, colon_flags, i)
                    {
                        self.screen.current_bg = color;
                        self.global_bg = self.screen.current_bg;
                        i = last;
                    }
                }
                // 58 设置下划线颜色 / 59 复位。当前渲染管线未存储独立下划线颜色,
                // 故仅正确消费其子参数,避免颜色分量泄漏成后续 SGR 码被误解析。
                58 => {
                    if let Some((_color, last)) =
                        Self::parse_sgr_extended_color(params, colon_flags, i)
                    {
                        i = last;
                    }
                }
                59 => {}
                _ => {}
            }
            i += 1;
        }
    }
}
