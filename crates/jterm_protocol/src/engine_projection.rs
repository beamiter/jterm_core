//! Paint-only projection for terminal wire version 3.

use crate::terminal::{
    self as protocol, ChangedRow, CursorShape as WireCursorShape, RgbColor, Update, WireCell,
    WireDelta, WireFrame,
};
use jterm_engine::{
    colors::{ColorState, Rgb, TerminalDefaultColors},
    frame::{CellFrame, Row},
    Color, CursorShape,
};

/// Resolve a host-owned immutable frame into paint-only values. No ANSI reaches JS.
pub fn from_engine(frame: &CellFrame, session: &str, epoch: u32) -> Result<WireFrame, String> {
    validate_grid(frame)?;
    let revision = u32::try_from(frame.stamp().revision).map_err(|_| "revision exhausted")?;
    let layout = u32::try_from(frame.stamp().layout).map_err(|_| "layout exhausted")?;
    Ok(WireFrame {
        version: protocol::VERSION,
        session: session.into(),
        epoch,
        revision,
        layout,
        cols: frame.cols(),
        rows: frame
            .rows()
            .iter()
            .map(|row| wire_row(row, frame.cols(), frame.paint_colors()))
            .collect(),
        cursor_row: frame.cursor().row,
        cursor_col: frame.cursor().col,
        cursor_visible: frame.cursor().visible,
        cursor_shape: cursor_shape(frame.cursor().shape),
        cursor_color: frame
            .paint_colors()
            .map(|colors| colors.effective_cursor().into()),
        pending_wrap: frame.cursor().pending_wrap,
    })
}
pub fn cursor_shape(shape: CursorShape) -> WireCursorShape {
    match shape {
        CursorShape::Block => WireCursorShape::Block,
        CursorShape::Underline => WireCursorShape::Underline,
        CursorShape::Beam => WireCursorShape::Beam,
    }
}

fn wire_row(row: &Row, cols: usize, colors: Option<&ColorState>) -> Vec<WireCell> {
    row.cells
        .iter()
        .enumerate()
        .map(|(column, c)| {
            let mut fg = paint_color(c.foreground, true, colors);
            let mut bg = paint_color(c.background, false, colors);
            if c.flags.inverse() {
                std::mem::swap(&mut fg, &mut bg);
            }
            WireCell {
                text: c.character.to_string(),
                foreground: fg,
                background: bg,
                width: cell_width(c.flags, column, cols),
                style: u8::from(c.flags.bold())
                    | (u8::from(c.flags.italic()) << 1)
                    | (u8::from(c.flags.underline() != jterm_engine::UnderlineStyle::None) << 2)
                    | (u8::from(c.flags.strikethrough()) << 3)
                    | (u8::from(c.flags.dim()) << 4),
                rendition: (u8::from(c.flags.concealed()) * protocol::RENDITION_CONCEALED)
                    | (u8::from(c.flags.overline()) * protocol::RENDITION_OVERLINE),
            }
        })
        .collect()
}

pub fn cell_width(flags: jterm_engine::StyleFlags, column: usize, cols: usize) -> u8 {
    if flags.wide_continuation() {
        0
    } else if flags.wide() && column + 1 < cols {
        2
    } else {
        1
    }
}

/// The compatibility renderer's initial colors, also used for OSC resets.
pub const WIRE_COLOR_DEFAULTS: TerminalDefaultColors = TerminalDefaultColors {
    foreground: (0xd9, 0xe5, 0xef),
    background: (0x11, 0x18, 0x20),
    cursor: (0x9a, 0xc6, 0xff),
    ansi: [
        (0x00, 0x00, 0x00),
        (0xcd, 0x31, 0x31),
        (0x0d, 0xbc, 0x79),
        (0xe5, 0xe5, 0x10),
        (0x24, 0x72, 0xc8),
        (0xbc, 0x3f, 0xbc),
        (0x11, 0xa8, 0xcd),
        (0xe5, 0xe5, 0xe5),
        (0x66, 0x66, 0x66),
        (0xf1, 0x4c, 0x4c),
        (0x23, 0xd1, 0x8b),
        (0xf5, 0xf5, 0x43),
        (0x3b, 0x8e, 0xea),
        (0xd6, 0x70, 0xd6),
        (0x29, 0xb8, 0xdb),
        (0xff, 0xff, 0xff),
    ],
};

const fn wire_palette() -> [[u8; 7]; 256] {
    let mut palette = [[0; 7]; 256];
    let levels = [0, 95, 135, 175, 215, 255];
    let mut index = 0;
    while index < 256 {
        palette[index] = if index < 16 {
            let (red, green, blue) = WIRE_COLOR_DEFAULTS.ansi[index];
            RgbColor(red, green, blue).hex_bytes()
        } else if index >= 232 {
            let gray = (8 + (index - 232) * 10) as u8;
            RgbColor(gray, gray, gray).hex_bytes()
        } else {
            let cube = index - 16;
            RgbColor(levels[cube / 36], levels[(cube / 6) % 6], levels[cube % 6]).hex_bytes()
        };
        index += 1;
    }
    palette
}

static WIRE_PALETTE: [&str; 256] = {
    static COLORS: [[u8; 7]; 256] = wire_palette();
    let mut palette = [""; 256];
    let mut index = 0;
    while index < palette.len() {
        palette[index] = match std::str::from_utf8(&COLORS[index]) {
            Ok(color) => color,
            Err(_) => panic!("wire palette must contain ASCII colors"),
        };
        index += 1;
    }
    palette
};

fn palette_index(c: Color) -> u8 {
    use Color::*;
    match c {
        Black => 0,
        Red => 1,
        Green => 2,
        Yellow => 3,
        Blue => 4,
        Magenta => 5,
        Cyan => 6,
        White => 7,
        BrightBlack => 8,
        BrightRed => 9,
        BrightGreen => 10,
        BrightYellow => 11,
        BrightBlue => 12,
        BrightMagenta => 13,
        BrightCyan => 14,
        BrightWhite => 15,
        Indexed(n) => n,
        Rgb(..) | Default => unreachable!("not a palette color"),
    }
}

fn rgb_color((red, green, blue): Rgb) -> String {
    static PAIRS: [&str; 256] = {
        static BYTES: [[u8; 2]; 256] = {
            let mut pairs = [[0; 2]; 256];
            let mut index = 0;
            while index < 256 {
                let hex = RgbColor(index as u8, 0, 0).hex_bytes();
                pairs[index] = [hex[1], hex[2]];
                index += 1;
            }
            pairs
        };
        let mut pairs = [""; 256];
        let mut index = 0;
        while index < 256 {
            pairs[index] = match std::str::from_utf8(&BYTES[index]) {
                Ok(pair) => pair,
                Err(_) => panic!("hex pairs must be ASCII"),
            };
            index += 1;
        }
        pairs
    };
    let mut result = String::with_capacity(7);
    result.push('#');
    result.push_str(PAIRS[usize::from(red)]);
    result.push_str(PAIRS[usize::from(green)]);
    result.push_str(PAIRS[usize::from(blue)]);
    result
}

pub fn color(c: Color, foreground: bool) -> String {
    paint_color(c, foreground, None)
}

/// Resolve the same captured paint as [`paint_color`] without allocating a CSS string.
pub fn paint_rgb(c: Color, foreground: bool, colors: Option<&ColorState>) -> RgbColor {
    match c {
        Color::Rgb(red, green, blue) => RgbColor(red, green, blue),
        Color::Default => {
            let rgb = if foreground {
                colors.map_or(
                    WIRE_COLOR_DEFAULTS.foreground,
                    ColorState::effective_foreground,
                )
            } else {
                colors.map_or(
                    WIRE_COLOR_DEFAULTS.background,
                    ColorState::effective_background,
                )
            };
            rgb.into()
        }
        _ => {
            let index = palette_index(c);
            let rgb = colors.map_or_else(
                || {
                    WIRE_COLOR_DEFAULTS
                        .ansi
                        .get(usize::from(index))
                        .copied()
                        .unwrap_or_else(|| ColorState::default_256_color(index))
                },
                |colors| colors.palette_color(index),
            );
            rgb.into()
        }
    }
}

/// Resolve paint using the palette captured with the frame or history page.
pub fn paint_color(c: Color, foreground: bool, colors: Option<&ColorState>) -> String {
    match c {
        Color::Rgb(red, green, blue) => rgb_color((red, green, blue)),
        Color::Default => {
            let Some(colors) = colors else {
                return if foreground { "#d9e5ef" } else { "#111820" }.into();
            };
            let resolved = if foreground {
                colors.effective_foreground()
            } else {
                colors.effective_background()
            };
            let initial = if foreground {
                WIRE_COLOR_DEFAULTS.foreground
            } else {
                WIRE_COLOR_DEFAULTS.background
            };
            if resolved == initial {
                if foreground { "#d9e5ef" } else { "#111820" }.into()
            } else {
                rgb_color(resolved)
            }
        }
        _ => {
            let index = palette_index(c);
            match colors {
                Some(colors) => {
                    if let Some(rgb) = colors.dynamic_palette[usize::from(index)] {
                        rgb_color(rgb)
                    } else if index >= 16
                        || colors.default_colors.ansi[usize::from(index)]
                            == WIRE_COLOR_DEFAULTS.ansi[usize::from(index)]
                    {
                        WIRE_PALETTE[usize::from(index)].to_owned()
                    } else {
                        rgb_color(colors.default_colors.ansi[usize::from(index)])
                    }
                }
                None => WIRE_PALETTE[index as usize].to_owned(),
            }
        }
    }
}

/// A cursor-only paint change never requires resending the cell rows.
pub fn same_cell_colors(before: Option<&ColorState>, after: Option<&ColorState>) -> bool {
    match (before, after) {
        (None, None) => true,
        (Some(before), Some(after)) => {
            std::ptr::eq(before, after)
                || (before.effective_foreground() == after.effective_foreground()
                    && before.effective_background() == after.effective_background()
                    && before.default_colors.ansi == after.default_colors.ansi
                    && before.palette_overrides() == after.palette_overrides())
        }
        _ => false,
    }
}

/// Produce a v3 snapshot or delta from immutable frames for a native producer.
pub fn update(
    previous: Option<&CellFrame>,
    frame: &CellFrame,
    id: &str,
    epoch: u32,
) -> Result<Update, String> {
    validate_grid(frame)?;
    let colors = frame.paint_colors();
    if let Some(delta) = previous
        .filter(|previous| same_cell_colors(previous.paint_colors(), colors))
        .and_then(|previous| previous.delta_to(frame).ok())
    {
        // If the complete grid changed, the snapshot is no larger and retains
        // its independence from previously accepted state.
        if delta.rows.len() < frame.rows().len() {
            return Ok(Update::Delta {
                frame: WireDelta {
                    version: protocol::VERSION,
                    session: id.to_owned(),
                    epoch,
                    base_revision: revision(delta.base.revision)?,
                    revision: revision(delta.next.revision)?,
                    layout: u32::try_from(delta.next.layout).map_err(|_| "layout exhausted")?,
                    cursor_row: delta.cursor.row,
                    cursor_col: delta.cursor.col,
                    cursor_visible: delta.cursor.visible,
                    cursor_shape: cursor_shape(delta.cursor.shape),
                    cursor_color: colors.map(|colors| colors.effective_cursor().into()),
                    pending_wrap: delta.cursor.pending_wrap,
                    rows: delta
                        .rows
                        .into_iter()
                        .map(|(index, row)| ChangedRow {
                            index,
                            cells: wire_row(&row, frame.cols(), colors),
                        })
                        .collect(),
                },
            });
        }
    }
    Ok(Update::Snapshot {
        frame: from_engine(frame, id, epoch)?,
    })
}

pub fn validate_grid(frame: &CellFrame) -> Result<(), String> {
    let rows = frame.rows().len();
    let cols = frame.cols();
    if rows <= protocol::MAX_ROWS
        && cols <= protocol::MAX_COLUMNS
        && rows * cols <= protocol::MAX_CELLS
    {
        Ok(())
    } else {
        Err("terminal grid exceeds wire limit".into())
    }
}

pub(crate) fn revision(value: u64) -> Result<u32, String> {
    u32::try_from(value).map_err(|_| "revision exhausted".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v3_uses_captured_palette_and_deltas_cursor_only_changes() {
        let mut terminal = jterm_engine::terminal::Terminal::new(2, 4).unwrap();
        terminal.set_color_defaults(WIRE_COLOR_DEFAULTS);
        terminal.feed(b"\x1b[31mA\x1b[0mB").unwrap();
        let base = terminal.frame().unwrap();
        terminal
            .feed(b"\x1b]4;1;#abcdef\x07\x1b]10;#112233;#445566;#778899\x07")
            .unwrap();
        let colored = terminal.frame().unwrap();
        assert_eq!(base.rows(), colored.rows());
        let Update::Snapshot { frame } = update(Some(&base), &colored, "test", 1).unwrap() else {
            panic!("palette changes must reproject unchanged raw rows");
        };
        assert_eq!(frame.rows[0][0].foreground, "#abcdef");
        assert_eq!(frame.rows[0][1].foreground, "#112233");
        assert_eq!(frame.rows[1][0].background, "#445566");
        assert_eq!(frame.cursor_color, Some((0x77, 0x88, 0x99).into()));
        assert_eq!(
            from_engine(&base, "test", 1).unwrap().rows[0][0].foreground,
            "#cd3131"
        );

        terminal.feed(b"\x1b]112\x07").unwrap();
        let reset = terminal.frame().unwrap();
        let Update::Delta { frame } = update(Some(&colored), &reset, "test", 1).unwrap() else {
            panic!("cursor-only changes should avoid row projection");
        };
        assert!(frame.rows.is_empty());
        assert_eq!(frame.cursor_color, Some(WIRE_COLOR_DEFAULTS.cursor.into()));
    }

    fn original_color(c: jterm_engine::Color, foreground: bool) -> String {
        use jterm_engine::Color::*;
        let index = match c {
            Black => 0,
            Red => 1,
            Green => 2,
            Yellow => 3,
            Blue => 4,
            Magenta => 5,
            Cyan => 6,
            White => 7,
            BrightBlack => 8,
            BrightRed => 9,
            BrightGreen => 10,
            BrightYellow => 11,
            BrightBlue => 12,
            BrightMagenta => 13,
            BrightCyan => 14,
            BrightWhite => 15,
            Indexed(n) => n,
            Rgb(r, g, b) => return format!("#{r:02x}{g:02x}{b:02x}"),
            Default => return if foreground { "#d9e5ef" } else { "#111820" }.into(),
        };
        let palette = [
            "#000000", "#cd3131", "#0dbc79", "#e5e510", "#2472c8", "#bc3fbc", "#11a8cd", "#e5e5e5",
            "#666666", "#f14c4c", "#23d18b", "#f5f543", "#3b8eea", "#d670d6", "#29b8db", "#ffffff",
        ];
        if index < 16 {
            return palette[index as usize].into();
        }
        if index >= 232 {
            let n = 8 + (index - 232) * 10;
            return format!("#{n:02x}{n:02x}{n:02x}");
        }
        let n = index - 16;
        let levels = [0u8, 95, 135, 175, 215, 255];
        format!(
            "#{:02x}{:02x}{:02x}",
            levels[(n / 36) as usize],
            levels[((n / 6) % 6) as usize],
            levels[(n % 6) as usize]
        )
    }

    #[test]
    fn wire_colors_match_the_original_palette_and_rgb_format() {
        use jterm_engine::Color::*;
        let named = [
            Black,
            Red,
            Green,
            Yellow,
            Blue,
            Magenta,
            Cyan,
            White,
            BrightBlack,
            BrightRed,
            BrightGreen,
            BrightYellow,
            BrightBlue,
            BrightMagenta,
            BrightCyan,
            BrightWhite,
            Default,
        ];
        for foreground in [false, true] {
            for entry in named.into_iter().chain((0..=255).map(Indexed)) {
                let expected = original_color(entry, foreground);
                assert_eq!(color(entry, foreground), expected);
                let bytes = paint_rgb(entry, foreground, None).hex_bytes();
                assert_eq!(std::str::from_utf8(&bytes).unwrap(), expected);
            }
            for red in 0..=255 {
                for green in [0, 1, 15, 16, 127, 128, 254, 255] {
                    for blue in [0, 1, 15, 16, 127, 128, 254, 255] {
                        let entry = Rgb(red, green, blue);
                        assert_eq!(color(entry, foreground), original_color(entry, foreground));
                    }
                }
            }
        }
    }
    #[test]
    fn captured_palette_fast_paths_preserve_authored_and_dynamic_colors() {
        let mut state = ColorState::new(WIRE_COLOR_DEFAULTS);
        for authored in [false, true] {
            if authored {
                state.default_colors.foreground = (1, 2, 3);
                state.default_colors.background = (4, 5, 6);
                for (index, entry) in state.default_colors.ansi.iter_mut().enumerate() {
                    *entry = (index as u8, 33, 77);
                }
            }
            for overridden in [false, true] {
                if overridden {
                    state.dynamic_fg = Some(WIRE_COLOR_DEFAULTS.foreground);
                    state.dynamic_bg = Some((7, 8, 9));
                    for index in 0..=255u8 {
                        // Include explicit overrides equal to the standard value.
                        state.dynamic_palette[usize::from(index)] = Some(if index % 2 == 0 {
                            ColorState::new(WIRE_COLOR_DEFAULTS).palette_color(index)
                        } else {
                            (index, 88, 99)
                        });
                    }
                }
                for foreground in [false, true] {
                    let expected = if foreground {
                        state.effective_foreground()
                    } else {
                        state.effective_background()
                    };
                    assert_eq!(
                        paint_color(Color::Default, foreground, Some(&state)),
                        rgb_color(expected)
                    );
                    assert_eq!(
                        paint_rgb(Color::Default, foreground, Some(&state)),
                        expected.into()
                    );
                    for index in 0..=255u8 {
                        assert_eq!(
                            paint_color(Color::Indexed(index), foreground, Some(&state)),
                            rgb_color(state.palette_color(index))
                        );
                        assert_eq!(
                            paint_rgb(Color::Indexed(index), foreground, Some(&state)),
                            state.palette_color(index).into()
                        );
                    }
                    assert_eq!(
                        paint_color(Color::Rgb(12, 34, 56), foreground, Some(&state)),
                        "#0c2238"
                    );
                    assert_eq!(
                        paint_rgb(Color::Rgb(12, 34, 56), foreground, Some(&state)),
                        RgbColor(12, 34, 56)
                    );
                }
            }
            state.dynamic_fg = None;
            state.dynamic_bg = None;
            state.dynamic_palette = [None; 256];
        }
    }
}
