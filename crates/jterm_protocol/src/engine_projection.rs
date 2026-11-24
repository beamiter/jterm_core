//! Paint-only projection for terminal wire version 3.

use crate::terminal::{
    self as protocol, ChangedRow, CursorShape as WireCursorShape, Update, WireCell, WireDelta,
    WireFrame,
};
use jterm_engine::{
    frame::{CellFrame, Row},
    CursorShape,
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
            .map(|row| wire_row(row, frame.cols()))
            .collect(),
        cursor_row: frame.cursor().row,
        cursor_col: frame.cursor().col,
        cursor_visible: frame.cursor().visible,
        cursor_shape: cursor_shape(frame.cursor().shape),
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

fn wire_row(row: &Row, cols: usize) -> Vec<WireCell> {
    row.cells
        .iter()
        .enumerate()
        .map(|(column, c)| {
            let mut fg = color(c.foreground, true);
            let mut bg = color(c.background, false);
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

pub fn color(c: jterm_engine::Color, foreground: bool) -> String {
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
/// Produce a v3 snapshot or delta from immutable frames for a native producer.
pub fn update(
    previous: Option<&CellFrame>,
    frame: &CellFrame,
    id: &str,
    epoch: u32,
) -> Result<Update, String> {
    validate_grid(frame)?;
    if let Some(delta) = previous.and_then(|previous| previous.delta_to(frame).ok()) {
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
                    pending_wrap: delta.cursor.pending_wrap,
                    rows: delta
                        .rows
                        .into_iter()
                        .map(|(index, row)| ChangedRow {
                            index,
                            cells: wire_row(&row, frame.cols()),
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
