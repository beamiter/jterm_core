//! Ember-compatible dynamic color ownership, independent of UI and PTY.
//! Replies are returned in operation order for immediate host delivery.
use crate::semantics::{Effect, Effects};

pub type Rgb = (u8, u8, u8);
pub type DynamicColorPalette = [Option<Rgb>; 256];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalDefaultColors {
    pub foreground: Rgb,
    pub background: Rgb,
    pub cursor: Rgb,
    pub ansi: [Rgb; 16],
}
impl Default for TerminalDefaultColors {
    fn default() -> Self {
        let mut ansi = [(0, 0, 0); 16];
        for (idx, slot) in ansi.iter_mut().enumerate() {
            *slot = ColorState::default_256_color(idx as u8);
        }
        Self {
            foreground: (255, 255, 255),
            background: (0, 0, 0),
            cursor: (255, 255, 255),
            ansi,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColorState {
    pub default_colors: TerminalDefaultColors,
    pub dynamic_fg: Option<Rgb>,
    pub dynamic_bg: Option<Rgb>,
    pub dynamic_cursor_color: Option<Rgb>,
    pub dynamic_palette: DynamicColorPalette,
}
impl Default for ColorState {
    fn default() -> Self {
        Self::new(TerminalDefaultColors::default())
    }
}
impl ColorState {
    pub fn new(default_colors: TerminalDefaultColors) -> Self {
        Self {
            default_colors,
            dynamic_fg: None,
            dynamic_bg: None,
            dynamic_cursor_color: None,
            dynamic_palette: [None; 256],
        }
    }
    pub fn defaults(&self) -> TerminalDefaultColors {
        self.default_colors
    }
    pub fn foreground_override(&self) -> Option<Rgb> {
        self.dynamic_fg
    }
    pub fn background_override(&self) -> Option<Rgb> {
        self.dynamic_bg
    }
    pub fn cursor_override(&self) -> Option<Rgb> {
        self.dynamic_cursor_color
    }
    pub fn palette_overrides(&self) -> &DynamicColorPalette {
        &self.dynamic_palette
    }
    pub fn effective_foreground(&self) -> Rgb {
        self.dynamic_fg.unwrap_or(self.default_colors.foreground)
    }
    pub fn effective_background(&self) -> Rgb {
        self.effective_default_background()
    }
    pub fn effective_cursor(&self) -> Rgb {
        self.dynamic_cursor_color
            .unwrap_or(self.default_colors.cursor)
    }
    pub fn palette_color(&self, idx: u8) -> Rgb {
        self.dynamic_palette[idx as usize].unwrap_or_else(|| {
            self.default_colors
                .ansi
                .get(idx as usize)
                .copied()
                .unwrap_or_else(|| Self::default_256_color(idx))
        })
    }
    /// RIS clears overrides but retains the host theme, without notifications.
    pub fn reset_dynamic(&mut self) {
        self.dynamic_fg = None;
        self.dynamic_bg = None;
        self.dynamic_cursor_color = None;
        self.dynamic_palette = [None; 256];
    }
    /// Only a host theme update emits a subscribed color-scheme change.
    pub fn set_theme(&mut self, colors: TerminalDefaultColors, subscribed_2031: bool) -> Effects {
        let before = self.color_scheme_report();
        self.default_colors = colors;
        let mut effects = Effects::new();
        if subscribed_2031 && self.color_scheme_report() != before {
            effects.push(Effect::Reply(
                format!("\x1b[?997;{}n", self.color_scheme_report()).into_bytes(),
            ));
        }
        effects
    }
    /// Apply only Ember color OSC commands. Unknown commands are inert.
    /// The terminator is supplied by the current OSC, never retained as state.
    pub fn apply_ember_osc(&mut self, command: &str, value: &str, terminator: &[u8]) -> Effects {
        let mut effects = Effects::new();
        match command {
            "4" => self.handle_osc_palette(value, terminator, &mut effects),
            "104" => self.reset_osc_palette(value),
            "10" | "11" | "12" => self.handle_osc_color(command, value, terminator, &mut effects),
            "110" | "111" | "112" => self.reset_osc_color(command),
            _ => {}
        }
        effects
    }
    pub fn effective_default_background(&self) -> (u8, u8, u8) {
        self.dynamic_bg.unwrap_or(self.default_colors.background)
    }
    pub fn color_scheme_report(&self) -> u8 {
        let (r, g, b) = self.effective_default_background();
        let luma = 0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32;
        if luma < 128.0 {
            1
        } else {
            2
        }
    }
    pub fn handle_osc_color(
        &mut self,
        command: &str,
        value: &str,
        terminator: &[u8],
        effects: &mut Effects,
    ) {
        if value == "?" {
            // Query: report what is painted — the OSC override if one is set,
            // else the theme default the app pushed in.
            let color = match command {
                "10" => self.dynamic_fg.unwrap_or(self.default_colors.foreground),
                "11" => self.effective_default_background(),
                "12" => self
                    .dynamic_cursor_color
                    .unwrap_or(self.default_colors.cursor),
                _ => return,
            };
            let response = format!(
                "\x1b]{};rgb:{:04x}/{:04x}/{:04x}",
                command,
                (color.0 as u16) * 257,
                (color.1 as u16) * 257,
                (color.2 as u16) * 257,
            );
            let mut reply = response.into_bytes();
            reply.extend_from_slice(terminator);
            effects.push(Effect::Reply(reply));
        } else if let Some(rgb) = Self::parse_color_spec(value) {
            match command {
                "10" => self.dynamic_fg = Some(rgb),
                "11" => self.dynamic_bg = Some(rgb),
                "12" => self.dynamic_cursor_color = Some(rgb),
                _ => {}
            }
        }
    }
    pub fn reset_osc_color(&mut self, command: &str) {
        match command {
            "110" => self.dynamic_fg = None,
            "111" => self.dynamic_bg = None,
            "112" => self.dynamic_cursor_color = None,
            _ => {}
        }
    }
    pub fn handle_osc_palette(&mut self, value: &str, terminator: &[u8], effects: &mut Effects) {
        let mut parts = value.split(';');
        while let Some(idx_s) = parts.next() {
            let Some(color_s) = parts.next() else {
                break;
            };
            let Ok(idx) = idx_s.parse::<u8>() else {
                continue;
            };
            if color_s == "?" {
                let color = self.dynamic_palette[idx as usize].unwrap_or_else(|| {
                    // The 16 ANSI slots are theme colours; the cube and
                    // grey ramp are not themed.
                    self.default_colors
                        .ansi
                        .get(idx as usize)
                        .copied()
                        .unwrap_or_else(|| Self::default_256_color(idx))
                });
                self.append_osc_palette_response(idx, color, terminator, effects);
            } else if let Some(rgb) = Self::parse_color_spec(color_s) {
                self.dynamic_palette[idx as usize] = Some(rgb);
            }
        }
    }
    pub fn reset_osc_palette(&mut self, value: &str) {
        if value.is_empty() {
            self.dynamic_palette = [None; 256];
            return;
        }
        for idx_s in value.split(';').filter(|s| !s.is_empty()) {
            if let Ok(idx) = idx_s.parse::<u8>() {
                self.dynamic_palette[idx as usize] = None;
            }
        }
    }
    fn append_osc_palette_response(
        &mut self,
        idx: u8,
        color: (u8, u8, u8),
        terminator: &[u8],
        effects: &mut Effects,
    ) {
        let response = format!(
            "\x1b]4;{};rgb:{:04x}/{:04x}/{:04x}",
            idx,
            (color.0 as u16) * 257,
            (color.1 as u16) * 257,
            (color.2 as u16) * 257,
        );
        let mut reply = response.into_bytes();
        reply.extend_from_slice(terminator);
        effects.push(Effect::Reply(reply));
    }
    pub fn default_256_color(idx: u8) -> (u8, u8, u8) {
        const ANSI: [(u8, u8, u8); 16] = [
            (0, 0, 0),
            (205, 0, 0),
            (0, 205, 0),
            (205, 205, 0),
            (0, 0, 238),
            (205, 0, 205),
            (0, 205, 205),
            (229, 229, 229),
            (127, 127, 127),
            (255, 0, 0),
            (0, 255, 0),
            (255, 255, 0),
            (92, 92, 255),
            (255, 0, 255),
            (0, 255, 255),
            (255, 255, 255),
        ];
        match idx {
            0..=15 => ANSI[idx as usize],
            16..=231 => {
                let idx = idx - 16;
                let r_idx = idx / 36;
                let g_idx = (idx % 36) / 6;
                let b_idx = idx % 6;
                let scale = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
                (scale(r_idx), scale(g_idx), scale(b_idx))
            }
            232..=255 => {
                let gray = 8 + (idx - 232) * 10;
                (gray, gray, gray)
            }
        }
    }
    pub fn parse_color_spec(spec: &str) -> Option<(u8, u8, u8)> {
        // Parse rgb:RR/GG/BB / rgb:RRRR/GGGG/BBBB / rgb:R/G/B / rgb:RRR/GGG/BBB / #RRGGBB
        // Per XParseColor, each component is 1..=4 hex digits and is left-aligned
        // into a 16-bit field (i.e. component value * (2^16-1) / (2^bits-1)),
        // then truncated to 8 bits. The previous scale=1 fallback for 1/3-digit
        // components produced a u8 cast of e.g. 0xFFF=4095, which wraps to 255
        // for full-on but is wrong for any partial value.
        fn scale_to_u8(value: u16, hex_digits: usize) -> u8 {
            // Range of n hex digits is [0, 16^n - 1].
            let max_n: u32 = (1u32 << (hex_digits * 4)).saturating_sub(1).max(1);
            // Scale value to 0..=255, rounding to nearest.
            (((value as u32) * 255 + max_n / 2) / max_n) as u8
        }
        if let Some(hex) = spec.strip_prefix('#') {
            // OSC payloads are UTF-8, not necessarily ASCII. Reject non-ASCII
            // before byte slicing so a six-byte malformed color cannot panic.
            if hex.len() == 6 && hex.is_ascii() {
                let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
                let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
                let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
                return Some((r, g, b));
            }
        } else if let Some(rgb) = spec.strip_prefix("rgb:") {
            let parts: Vec<&str> = rgb.split('/').collect();
            if parts.len() == 3
                && (1..=4).contains(&parts[0].len())
                && parts[0].len() == parts[1].len()
                && parts[1].len() == parts[2].len()
            {
                let digits = parts[0].len();
                let r = u16::from_str_radix(parts[0], 16).ok()?;
                let g = u16::from_str_radix(parts[1], 16).ok()?;
                let b = u16::from_str_radix(parts[2], 16).ok()?;
                return Some((
                    scale_to_u8(r, digits),
                    scale_to_u8(g, digits),
                    scale_to_u8(b, digits),
                ));
            }
        }
        None
    }
}
