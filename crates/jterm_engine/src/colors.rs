//! Profiled dynamic color ownership, independent of UI and PTY.
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
/// Override storage shared across frontends; defaults and protocol policy stay profiled.
#[derive(Clone, PartialEq, Eq)]
pub struct ProfiledColorState<D> {
    pub default_colors: D,
    pub dynamic_fg: Option<Rgb>,
    pub dynamic_bg: Option<Rgb>,
    pub dynamic_cursor_color: Option<Rgb>,
    pub dynamic_palette: DynamicColorPalette,
}
/// Ember colors retain their original API and debug representation.
pub type ColorState = ProfiledColorState<TerminalDefaultColors>;
/// Frost override storage. The host supplies defaults for each operation.
///
/// Ember parsing is intentionally unavailable on this profile.
/// ```compile_fail
/// use jterm_engine::colors::DynamicColorState;
/// DynamicColorState::parse_color_spec("rgb:8/8/8");
/// ```
/// ```compile_fail
/// use jterm_engine::colors::DynamicColorState;
/// DynamicColorState::default().apply_ember_osc("10", "?", b"\x07");
/// ```
/// The owner is deliberately not Copy; saved-primary snapshots use explicit clone.
/// ```compile_fail
/// use jterm_engine::colors::DynamicColorState;
/// fn requires_copy<T: Copy>() {}
/// requires_copy::<DynamicColorState>();
/// ```
/// ```compile_fail
/// use jterm_engine::colors::DynamicColorState;
/// DynamicColorState::default().effective_foreground();
/// ```
/// ```compile_fail
/// use jterm_engine::colors::{DynamicColorState, TerminalDefaultColors};
/// DynamicColorState::default().set_theme(TerminalDefaultColors::default(), true);
/// ```
/// Ember also remains non-Copy.
/// ```compile_fail
/// use jterm_engine::colors::ColorState;
/// fn requires_copy<T: Copy>() {}
/// requires_copy::<ColorState>();
/// ```
pub type DynamicColorState = ProfiledColorState<()>;
impl<D: core::fmt::Debug> core::fmt::Debug for ProfiledColorState<D> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ColorState")
            .field("default_colors", &self.default_colors)
            .field("dynamic_fg", &self.dynamic_fg)
            .field("dynamic_bg", &self.dynamic_bg)
            .field("dynamic_cursor_color", &self.dynamic_cursor_color)
            .field("dynamic_palette", &self.dynamic_palette)
            .finish()
    }
}
impl<D: Default> Default for ProfiledColorState<D> {
    fn default() -> Self {
        Self::new(D::default())
    }
}
impl<D> ProfiledColorState<D> {
    pub fn new(default_colors: D) -> Self {
        Self {
            default_colors,
            dynamic_fg: None,
            dynamic_bg: None,
            dynamic_cursor_color: None,
            dynamic_palette: [None; 256],
        }
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
    /// Clear overrides only; the host decides when this is appropriate.
    pub fn reset_dynamic(&mut self) {
        self.dynamic_fg = None;
        self.dynamic_bg = None;
        self.dynamic_cursor_color = None;
        self.dynamic_palette = [None; 256];
    }
}
#[derive(Clone, Copy)]
enum OscColorProfile {
    Ember,
    Frost,
}

impl OscColorProfile {
    fn parse(self, spec: &str) -> Option<Rgb> {
        if let Some(hex) = spec.strip_prefix('#') {
            if hex.len() != 6 || !hex.is_ascii() {
                return None;
            }
            return Some((
                u8::from_str_radix(&hex[0..2], 16).ok()?,
                u8::from_str_radix(&hex[2..4], 16).ok()?,
                u8::from_str_radix(&hex[4..6], 16).ok()?,
            ));
        }

        let mut components = spec.strip_prefix("rgb:")?.split('/');
        let (r, g, b) = (components.next()?, components.next()?, components.next()?);
        if components.next().is_some() {
            return None;
        }
        let digits = r.len();
        if matches!(self, Self::Ember)
            && (!(1..=4).contains(&digits) || g.len() != digits || b.len() != digits)
        {
            return None;
        }
        let r = u16::from_str_radix(r, 16).ok()?;
        let g = u16::from_str_radix(g, 16).ok()?;
        let b = u16::from_str_radix(b, 16).ok()?;
        match self {
            Self::Ember => {
                let max = (1u32 << (digits * 4)) - 1;
                let scaled = |component| ((u32::from(component) * 255 + max / 2) / max) as u8;
                Some((scaled(r), scaled(g), scaled(b)))
            }
            Self::Frost => {
                // The original parser keys every channel's scale to the first
                // channel's raw length and accepts mixed component widths.
                let scale = if digits == 4 { 257 } else { 1 };
                Some(((r / scale) as u8, (g / scale) as u8, (b / scale) as u8))
            }
        }
    }
}

fn color_reply(
    command: std::fmt::Arguments<'_>,
    (r, g, b): Rgb,
    terminator: &[u8],
    effects: &mut Effects,
) {
    let (r, g, b) = (u16::from(r) * 257, u16::from(g) * 257, u16::from(b) * 257);
    let mut reply = format!("\x1b]{command};rgb:{r:04x}/{g:04x}/{b:04x}").into_bytes();
    reply.extend_from_slice(terminator);
    effects.push(Effect::Reply(reply));
}

impl<D> ProfiledColorState<D> {
    fn apply_color_osc(
        &mut self,
        command: &str,
        value: &str,
        terminator: &[u8],
        defaults: TerminalDefaultColors,
        profile: OscColorProfile,
    ) -> Effects {
        let mut effects = Effects::new();
        match command {
            "4" => self.osc_palette(value, terminator, defaults, profile, &mut effects),
            "104" => self.reset_osc_palette(value),
            "10" | "11" | "12" => {
                self.osc_color(command, value, terminator, defaults, profile, &mut effects);
            }
            "110" | "111" | "112" => self.reset_osc_color(command),
            _ => {}
        }
        effects
    }

    fn osc_color(
        &mut self,
        command: &str,
        value: &str,
        terminator: &[u8],
        defaults: TerminalDefaultColors,
        profile: OscColorProfile,
        effects: &mut Effects,
    ) {
        let (slot, fallback) = match command {
            "10" => (&mut self.dynamic_fg, defaults.foreground),
            "11" => (&mut self.dynamic_bg, defaults.background),
            "12" => (&mut self.dynamic_cursor_color, defaults.cursor),
            _ => return,
        };
        if value == "?" {
            color_reply(
                format_args!("{command}"),
                slot.unwrap_or(fallback),
                terminator,
                effects,
            );
        } else if let Some(rgb) = profile.parse(value) {
            *slot = Some(rgb);
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

    fn osc_palette(
        &mut self,
        value: &str,
        terminator: &[u8],
        defaults: TerminalDefaultColors,
        profile: OscColorProfile,
        effects: &mut Effects,
    ) {
        let mut parts = value.split(';');
        while let (Some(index), Some(spec)) = (parts.next(), parts.next()) {
            let Ok(index) = index.parse::<u8>() else {
                continue;
            };
            if spec == "?" {
                let color = self.palette_color_with_defaults(index, defaults);
                color_reply(format_args!("4;{index}"), color, terminator, effects);
            } else if let Some(rgb) = profile.parse(spec) {
                self.dynamic_palette[usize::from(index)] = Some(rgb);
            }
        }
    }

    pub fn reset_osc_palette(&mut self, value: &str) {
        if value.is_empty() {
            self.dynamic_palette = [None; 256];
            return;
        }
        for index in value.split(';').filter(|s| !s.is_empty()) {
            if let Ok(index) = index.parse::<u8>() {
                self.dynamic_palette[usize::from(index)] = None;
            }
        }
    }

    fn palette_color_with_defaults(&self, index: u8, defaults: TerminalDefaultColors) -> Rgb {
        self.dynamic_palette[usize::from(index)].unwrap_or_else(|| {
            defaults
                .ansi
                .get(usize::from(index))
                .copied()
                .unwrap_or_else(|| ColorState::default_256_color(index))
        })
    }
}

impl ColorState {
    pub fn defaults(&self) -> TerminalDefaultColors {
        self.default_colors
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
        self.palette_color_with_defaults(idx, self.default_colors)
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
    /// The terminator is supplied by the current OSC.
    pub fn apply_ember_osc(&mut self, command: &str, value: &str, terminator: &[u8]) -> Effects {
        self.apply_color_osc(
            command,
            value,
            terminator,
            self.default_colors,
            OscColorProfile::Ember,
        )
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
        self.osc_color(
            command,
            value,
            terminator,
            self.default_colors,
            OscColorProfile::Ember,
            effects,
        );
    }
    pub fn handle_osc_palette(&mut self, value: &str, terminator: &[u8], effects: &mut Effects) {
        self.osc_palette(
            value,
            terminator,
            self.default_colors,
            OscColorProfile::Ember,
            effects,
        );
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
    pub fn parse_color_spec(spec: &str) -> Option<Rgb> {
        OscColorProfile::Ember.parse(spec)
    }
}

impl DynamicColorState {
    /// Apply the Frost legacy OSC color rules with host-supplied theme fallbacks.
    pub fn apply_frost_osc(
        &mut self,
        command: &str,
        value: &str,
        terminator: &[u8],
        defaults: TerminalDefaultColors,
    ) -> Effects {
        self.apply_color_osc(command, value, terminator, defaults, OscColorProfile::Frost)
    }

    pub fn parse_frost_color_spec(spec: &str) -> Option<Rgb> {
        OscColorProfile::Frost.parse(spec)
    }
}
