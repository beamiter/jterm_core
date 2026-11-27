//! Native XTWINOPS replies. A frontend supplies the dimensions it actually uses.

use crate::titles::ProfiledTitleState;
use std::io::Write;

#[derive(Clone, Copy)]
pub enum NativeWindowPixels {
    Cell { width: u32, height: u32 },
    Viewport { width: u32, height: u32 },
}

impl NativeWindowPixels {
    /// A renderer must supply positive dimensions in physical pixels.
    pub fn validate(self) -> Result<(), String> {
        let (width, height) = match self {
            Self::Cell { width, height } | Self::Viewport { width, height } => (width, height),
        };
        if width == 0 || height == 0 {
            return Err("empty pixel measurement".into());
        }
        Ok(())
    }
}

pub struct NativeWindowGeometry {
    pub columns: usize,
    pub rows: usize,
    pub pixels: NativeWindowPixels,
}

impl NativeWindowGeometry {
    fn text_pixels(&self) -> (u32, u32) {
        match self.pixels {
            NativeWindowPixels::Cell { width, height } => (
                height.saturating_mul(self.rows as u32),
                width.saturating_mul(self.columns as u32),
            ),
            NativeWindowPixels::Viewport { width, height } => (height, width),
        }
    }

    pub(crate) fn cell_pixels(&self) -> (u32, u32) {
        match self.pixels {
            NativeWindowPixels::Cell { width, height } => (height, width),
            NativeWindowPixels::Viewport { width, height } => {
                fn rounded_extent(pixels: u32, cells: usize) -> u32 {
                    let cells = u32::try_from(cells.max(1)).unwrap_or(u32::MAX);
                    pixels.saturating_add(cells / 2) / cells
                }
                (
                    rounded_extent(height, self.rows),
                    rounded_extent(width, self.columns),
                )
            }
        }
    }
}

pub(crate) fn write_text_area_chars(rows: usize, columns: usize, replies: &mut Vec<u8>) {
    write!(replies, "\x1b[8;{rows};{columns}t").unwrap();
}

pub(crate) fn write_text_area_pixels(geometry: &NativeWindowGeometry, replies: &mut Vec<u8>) {
    let (height, width) = geometry.text_pixels();
    write!(replies, "\x1b[4;{height};{width}t").unwrap();
}

pub(crate) fn write_cell_pixels(geometry: &NativeWindowGeometry, replies: &mut Vec<u8>) {
    let (height, width) = geometry.cell_pixels();
    write!(replies, "\x1b[6;{height};{width}t").unwrap();
}

/// Apply one already-admitted CSI `t` in place so queries stay in parser order.
/// Without compositor coordinates, state is normal and position is origin.
/// Monitor geometry mirrors the grid. Move, resize and iconify are ignored.
pub fn apply_native_window_op<const LIMIT: usize>(
    params: &[u16],
    geometry: NativeWindowGeometry,
    titles: &mut ProfiledTitleState<LIMIT>,
    report_title: impl FnOnce(&str) -> String,
    replies: &mut Vec<u8>,
) {
    let (rows, columns) = (geometry.rows, geometry.columns);
    match params.first().copied().unwrap_or(0) {
        11 => replies.extend_from_slice(b"\x1b[1t"),
        13 => replies.extend_from_slice(b"\x1b[3;0;0t"),
        14 => write_text_area_pixels(&geometry, replies),
        16 => write_cell_pixels(&geometry, replies),
        18 => write_text_area_chars(rows, columns, replies),
        19 => write!(replies, "\x1b[9;{rows};{columns}t").unwrap(),
        _ => {
            if let Some(reply) = titles.apply_title_window_op(params, report_title) {
                replies.extend(reply);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::titles::TitleState;

    #[test]
    fn native_pixel_profiles_preserve_rounding_and_saturation() {
        for (rows, columns, pixels, expected) in [
            (
                5,
                8,
                NativeWindowPixels::Cell {
                    width: 9,
                    height: 13,
                },
                b"\x1b[4;65;72t\x1b[6;13;9t".as_slice(),
            ),
            (
                5,
                8,
                NativeWindowPixels::Viewport {
                    width: 75,
                    height: 67,
                },
                b"\x1b[4;67;75t\x1b[6;13;9t".as_slice(),
            ),
            (
                2,
                3,
                NativeWindowPixels::Cell {
                    width: u32::MAX,
                    height: u32::MAX,
                },
                b"\x1b[4;4294967295;4294967295t\x1b[6;4294967295;4294967295t".as_slice(),
            ),
            (
                0,
                0,
                NativeWindowPixels::Viewport {
                    width: 75,
                    height: 67,
                },
                b"\x1b[4;67;75t\x1b[6;67;75t".as_slice(),
            ),
        ] {
            let mut replies = vec![];
            for op in [14, 16] {
                apply_native_window_op(
                    &[op],
                    NativeWindowGeometry {
                        rows,
                        columns,
                        pixels,
                    },
                    &mut TitleState::default(),
                    |_| panic!("geometry has no title payload"),
                    &mut replies,
                );
            }
            assert_eq!(replies, expected);
        }
    }
}
