//! Character attributes ("the pen"): what SGR selects and every printed cell
//! carries, plus the SGR/OSC 8 serialisation `to_ansi` needs.

use super::parser::Params;
use std::fmt::Write as _;

/// A colour exactly as the stream selected it. `Legacy` (SGR 30-37/90-97,
/// 40-47/100-107) is kept apart from `Indexed` (`38;5;n`) because VTE keeps
/// them apart too: only legacy colours are brightened by bold.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default, Debug)]
pub(super) enum Color {
    #[default]
    Default,
    Legacy(u8),
    Indexed(u8),
    Rgb(u8, u8, u8),
}

pub(super) const BOLD: u16 = 1 << 0;
pub(super) const DIM: u16 = 1 << 1;
pub(super) const ITALIC: u16 = 1 << 2;
pub(super) const BLINK: u16 = 1 << 3;
pub(super) const REVERSE: u16 = 1 << 4;
pub(super) const INVISIBLE: u16 = 1 << 5;
pub(super) const STRIKE: u16 = 1 << 6;
pub(super) const OVERLINE: u16 = 1 << 7;
/// Underline style (0 none, 1 single, 2 double, 3 curly, 4 dotted, 5 dashed)
/// lives in three bits above the flags.
const UNDERLINE_SHIFT: u16 = 8;
const UNDERLINE_MASK: u16 = 0b111 << UNDERLINE_SHIFT;

/// SGR state plus the hyperlink (VTE keeps the current OSC 8 target in the
/// same attribute word, so it is saved/restored with DECSC like colours).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default, Debug)]
pub(super) struct Pen {
    pub(super) fg: Color,
    pub(super) bg: Color,
    /// Underline colour (SGR 58/59).
    pub(super) ul: Color,
    pub(super) flags: u16,
    /// 0 = no hyperlink, otherwise a 1-based index into the link table.
    pub(super) link: u32,
}

impl Pen {
    pub(super) fn underline(&self) -> u16 {
        (self.flags & UNDERLINE_MASK) >> UNDERLINE_SHIFT
    }

    fn set_underline(&mut self, style: u16) {
        self.flags = (self.flags & !UNDERLINE_MASK) | ((style & 0b111) << UNDERLINE_SHIFT);
    }

    /// The attributes an erase leaves behind. VTE fills erased cells with
    /// `m_color_defaults` — the current colours without attributes or link —
    /// and only the background of an empty cell is visible, so that is all
    /// this keeps (`CSI 41m CSI K` still paints a red bar).
    pub(super) fn erase(&self) -> Pen {
        Pen {
            bg: self.bg,
            ..Pen::default()
        }
    }

    /// Whether an empty cell with this pen still shows something (a coloured
    /// or reverse-video background, or a line drawn through/under/over it).
    pub(super) fn blank_is_visible(&self) -> bool {
        self.bg != Color::Default
            || self.flags & (REVERSE | STRIKE | OVERLINE) != 0
            || self.underline() != 0
    }

    /// Same colours and attributes, ignoring the hyperlink.
    pub(super) fn same_style(&self, other: &Pen) -> bool {
        self.fg == other.fg
            && self.bg == other.bg
            && self.ul == other.ul
            && self.flags == other.flags
    }

    pub(super) fn is_plain(&self) -> bool {
        self.same_style(&Pen::default())
    }
}

/// Parses the colour that follows SGR 38/48/58 at `idx`, advancing `idx` to
/// the last consumed parameter. Mirrors VTE's `seq_parse_sgr_color`: the
/// colon form (`38:5:n`, `38:2:r:g:b`, `38:2:cs:r:g:b`) stays inside its
/// sub-parameter group; the semicolon form (`38;5;n`, `38;2;r;g;b`) consumes
/// the following parameters.
fn parse_color(params: &Params, idx: &mut usize) -> Option<Color> {
    if params.is_nonfinal(*idx) {
        *idx += 1;
        let group_end = params.next(*idx - 1);
        match params.raw(*idx) {
            2 => {
                let n = group_end - *idx;
                if n < 4 {
                    return None;
                }
                if n > 4 {
                    // A colour-space id; VTE only accepts it omitted.
                    *idx += 1;
                    if params.raw(*idx) != -1 {
                        return None;
                    }
                }
                let r = params.raw(*idx + 1);
                let g = params.raw(*idx + 2);
                let b = params.raw(*idx + 3);
                *idx += 3;
                rgb(r, g, b)
            }
            5 => {
                if group_end - *idx < 2 {
                    return None;
                }
                *idx += 1;
                let v = params.raw(*idx);
                u8::try_from(v).ok().map(Color::Indexed)
            }
            _ => None,
        }
    } else {
        *idx = params.next(*idx);
        match params.raw(*idx) {
            2 => {
                let r = params.raw(params.next(*idx));
                *idx = params.next(*idx);
                let g = params.raw(params.next(*idx));
                *idx = params.next(*idx);
                let b = params.raw(params.next(*idx));
                *idx = params.next(*idx);
                rgb(r, g, b)
            }
            5 => {
                *idx = params.next(*idx);
                u8::try_from(params.raw(*idx)).ok().map(Color::Indexed)
            }
            _ => None,
        }
    }
}

fn rgb(r: i32, g: i32, b: i32) -> Option<Color> {
    Some(Color::Rgb(
        u8::try_from(r).ok()?,
        u8::try_from(g).ok()?,
        u8::try_from(b).ok()?,
    ))
}

/// Applies an SGR parameter list the way VTE's `Terminal::SGR` does
/// (including its reading of an omitted parameter as 0, and `4:n` underline
/// styles). The hyperlink survives `SGR 0`, as in VTE.
pub(super) fn apply_sgr(pen: &mut Pen, params: &Params) {
    let reset = |pen: &mut Pen| {
        *pen = Pen {
            link: pen.link,
            ..Pen::default()
        }
    };
    if params.len() == 0 {
        reset(pen);
        return;
    }
    let mut i = 0;
    while i < params.len() {
        match params.raw(i) {
            -1 | 0 => reset(pen),
            1 => pen.flags |= BOLD,
            2 => pen.flags |= DIM,
            3 => pen.flags |= ITALIC,
            4 => {
                let mut style = 1;
                if params.is_nonfinal(i) {
                    match params.get(i + 1, 1) {
                        v @ 0..=5 => style = v as u16,
                        // VTE skips an out-of-range style instead of guessing.
                        _ => style = u16::MAX,
                    }
                }
                if style != u16::MAX {
                    pen.set_underline(style);
                }
            }
            5 | 6 => pen.flags |= BLINK,
            7 => pen.flags |= REVERSE,
            8 => pen.flags |= INVISIBLE,
            9 => pen.flags |= STRIKE,
            21 => pen.set_underline(2),
            22 => pen.flags &= !(BOLD | DIM),
            23 => pen.flags &= !ITALIC,
            24 => pen.set_underline(0),
            25 => pen.flags &= !BLINK,
            27 => pen.flags &= !REVERSE,
            28 => pen.flags &= !INVISIBLE,
            29 => pen.flags &= !STRIKE,
            v @ 30..=37 => pen.fg = Color::Legacy((v - 30) as u8),
            38 => {
                if let Some(c) = parse_color(params, &mut i) {
                    pen.fg = c;
                }
            }
            39 => pen.fg = Color::Default,
            v @ 40..=47 => pen.bg = Color::Legacy((v - 40) as u8),
            48 => {
                if let Some(c) = parse_color(params, &mut i) {
                    pen.bg = c;
                }
            }
            49 => pen.bg = Color::Default,
            53 => pen.flags |= OVERLINE,
            55 => pen.flags &= !OVERLINE,
            58 => {
                if let Some(c) = parse_color(params, &mut i) {
                    pen.ul = c;
                }
            }
            59 => pen.ul = Color::Default,
            v @ 90..=97 => pen.fg = Color::Legacy((v - 90 + 8) as u8),
            v @ 100..=107 => pen.bg = Color::Legacy((v - 100 + 8) as u8),
            _ => {}
        }
        i = params.next(i);
    }
}

/// Accumulates one SGR parameter list (`1;38;5;208`) without allocating
/// per code.
#[derive(Default)]
pub(super) struct SgrCodes {
    buf: String,
}

impl SgrCodes {
    fn clear(&mut self) {
        self.buf.clear();
    }

    fn push(&mut self, code: std::fmt::Arguments<'_>) {
        if !self.buf.is_empty() {
            self.buf.push(';');
        }
        let _ = self.buf.write_fmt(code);
    }

    fn push_str(&mut self, code: &str) {
        if !self.buf.is_empty() {
            self.buf.push(';');
        }
        self.buf.push_str(code);
    }

    fn len(&self) -> usize {
        self.buf.len()
    }

    /// `base` is 30 (foreground), 40 (background) or 50 (underline colour,
    /// which only has the 58/59 forms).
    fn push_color(&mut self, color: Color, base: u8) {
        match color {
            Color::Default => self.push(format_args!("{}", base + 9)),
            Color::Legacy(n) if n < 8 && base != 50 => self.push(format_args!("{}", base + n)),
            Color::Legacy(n) if n < 16 && base != 50 => {
                self.push(format_args!("{}", base + 60 + n - 8))
            }
            Color::Legacy(n) | Color::Indexed(n) => self.push(format_args!("{};5;{n}", base + 8)),
            Color::Rgb(r, g, b) => self.push(format_args!("{};2;{r};{g};{b}", base + 8)),
        }
    }

    fn push_underline(&mut self, style: u16) {
        match style {
            0 => self.push_str("24"),
            1 => self.push_str("4"),
            n => self.push(format_args!("4:{n}")),
        }
    }

    /// Every code needed to draw `pen` from the default state, after a `0`.
    fn absolute(&mut self, pen: &Pen) {
        self.clear();
        self.push_str("0");
        for (flag, code) in FLAG_CODES {
            if pen.flags & flag != 0 {
                self.push_str(code.0);
            }
        }
        if pen.underline() != 0 {
            self.push_underline(pen.underline());
        }
        if pen.fg != Color::Default {
            self.push_color(pen.fg, 30);
        }
        if pen.bg != Color::Default {
            self.push_color(pen.bg, 40);
        }
        if pen.ul != Color::Default {
            self.push_color(pen.ul, 50);
        }
    }

    /// Only the codes that change `from` into `to`.
    fn relative(&mut self, from: &Pen, to: &Pen) {
        self.clear();
        let intensity = BOLD | DIM;
        if from.flags & intensity != to.flags & intensity {
            // SGR 22 clears bold and dim together, so re-add whichever survives.
            if from.flags & intensity & !to.flags != 0 {
                self.push_str("22");
                if to.flags & BOLD != 0 {
                    self.push_str("1");
                }
                if to.flags & DIM != 0 {
                    self.push_str("2");
                }
            } else {
                if to.flags & BOLD != 0 && from.flags & BOLD == 0 {
                    self.push_str("1");
                }
                if to.flags & DIM != 0 && from.flags & DIM == 0 {
                    self.push_str("2");
                }
            }
        }
        for (flag, (on, off)) in FLAG_CODES {
            if flag & intensity == 0 && from.flags & flag != to.flags & flag {
                self.push_str(if to.flags & flag != 0 { on } else { off });
            }
        }
        if from.underline() != to.underline() {
            self.push_underline(to.underline());
        }
        if from.fg != to.fg {
            self.push_color(to.fg, 30);
        }
        if from.bg != to.bg {
            self.push_color(to.bg, 40);
        }
        if from.ul != to.ul {
            self.push_color(to.ul, 50);
        }
    }
}

/// Flag → (set code, reset code). Bold and dim share reset code 22, which
/// `relative` handles before this table.
const FLAG_CODES: [(u16, (&str, &str)); 8] = [
    (BOLD, ("1", "22")),
    (DIM, ("2", "22")),
    (ITALIC, ("3", "23")),
    (BLINK, ("5", "25")),
    (REVERSE, ("7", "27")),
    (INVISIBLE, ("8", "28")),
    (STRIKE, ("9", "29")),
    (OVERLINE, ("53", "55")),
];

/// Reusable buffers for [`write_sgr_transition`].
#[derive(Default)]
pub(super) struct SgrScratch {
    relative: SgrCodes,
    absolute: SgrCodes,
}

/// Appends the shortest SGR sequence that turns the style of `from` into the
/// style of `to` (either a diff or a `0;…` restart). Hyperlinks are handled
/// separately by the caller.
pub(super) fn write_sgr_transition(
    out: &mut String,
    scratch: &mut SgrScratch,
    from: &Pen,
    to: &Pen,
) {
    if from.same_style(to) {
        return;
    }
    if to.is_plain() {
        out.push_str("\x1b[0m");
        return;
    }
    scratch.relative.relative(from, to);
    scratch.absolute.absolute(to);
    let codes = if scratch.relative.len() <= scratch.absolute.len() {
        &scratch.relative.buf
    } else {
        &scratch.absolute.buf
    };
    out.push_str("\x1b[");
    out.push_str(codes);
    out.push('m');
}
