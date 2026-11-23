use super::{Terminal, VerticalMotion};
use crate::grid::WideContinuationProtection;
use std::sync::{Arc, Weak};

/// Identity allocated by the terminal that owns the grid. A client image ID
/// belongs to its protocol adapter and is never used as a cell identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct InlineImageId(u64);

impl InlineImageId {
    pub fn get(self) -> u64 {
        self.0
    }
}

/// Tightly packed, unassociated RGBA8 pixels owned by an immutable image.
#[derive(PartialEq, Eq)]
pub struct InlineRaster {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

impl std::fmt::Debug for InlineRaster {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InlineRaster")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("byte_len", &self.rgba.len())
            .finish()
    }
}

impl InlineRaster {
    pub(crate) fn new(width: u32, height: u32, rgba: Vec<u8>) -> Option<Self> {
        let expected = usize::try_from(width)
            .ok()?
            .checked_mul(usize::try_from(height).ok()?)?
            .checked_mul(4)?;
        (expected != 0 && rgba.len() == expected).then_some(Self {
            width,
            height,
            rgba,
        })
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn rgba(&self) -> &[u8] {
        &self.rgba
    }
}

/// Resource retained by live, archived and published image rows.
#[derive(Debug, PartialEq, Eq)]
pub struct InlineImage {
    id: InlineImageId,
    columns: u16,
    rows: u16,
    raster: Option<InlineRaster>,
}

impl InlineImage {
    pub fn id(&self) -> InlineImageId {
        self.id
    }

    pub fn columns(&self) -> u16 {
        self.columns
    }

    pub fn rows(&self) -> u16 {
        self.rows
    }

    pub fn raster(&self) -> Option<&InlineRaster> {
        self.raster.as_ref()
    }
}

/// One row of an inline image, attached to its first physical column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InlineImageRow {
    pub image: InlineImageId,
    pub slice: u16,
}

/// Each live, archived or immutable frame annotation shares the resource's
/// lifetime. The raster store holds only a weak reference to this lease.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ImageAnnotation {
    image: Arc<InlineImage>,
    slice: u16,
}

impl ImageAnnotation {
    pub(crate) fn resource(&self) -> (&InlineImage, u16) {
        (&self.image, self.slice)
    }

    pub(crate) fn row(&self) -> InlineImageRow {
        InlineImageRow {
            image: self.image.id,
            slice: self.slice,
        }
    }
}

/// A primary-screen reservation checked against the live frame revision.
pub struct PreparedInlineImage {
    epoch: u64,
    revision: u64,
    column: usize,
    row: usize,
    columns: u16,
    rows: u16,
    moves_cursor: bool,
}

/// Stable engine references that let a renderer retire its raster backing.
pub struct PlacedInlineImage {
    image: InlineImageId,
    lease: Weak<InlineImage>,
}

impl PlacedInlineImage {
    pub fn id(&self) -> InlineImageId {
        self.image
    }

    /// Old retained frames keep their image backing charged until release.
    pub fn is_retained(&self) -> bool {
        self.lease.strong_count() != 0
    }
}

impl Terminal {
    /// Validate the rectangle before an adapter acknowledges a display.
    /// Cursor coordinates are physical, zero-based, and supplied in (x, y)
    /// order. Restrict placement to the primary screen and to rows reached
    /// by ordinary line feeds.
    pub fn prepare_inline_image(
        &self,
        cursor: (usize, usize),
        columns: u16,
        rows: u16,
        moves_cursor: bool,
    ) -> Option<PreparedInlineImage> {
        if self.alternate_screen_active()
            || self.screen.pending_wrap
            || cursor != (self.screen.cursor_col, self.screen.cursor_row)
            || columns == 0
            || rows == 0
            || cursor.0.checked_add(usize::from(columns))? > self.screen.grid.cols()
            || usize::from(rows) > self.screen.grid.rows()
            || (cursor.1 > self.screen.scroll_region_bottom
                && cursor.1.checked_add(usize::from(rows))? > self.screen.grid.rows())
        {
            return None;
        }
        self.next_inline_image_id.checked_add(1)?;
        self.revision.checked_add(1)?;
        Some(PreparedInlineImage {
            epoch: self.epoch.get(),
            revision: self.revision,
            column: cursor.0,
            row: cursor.1,
            columns,
            rows,
            moves_cursor,
        })
    }

    /// Reserve image rows without injecting output bytes or touching the
    /// shell's saved cursor register. A newer feed/resize voids the ticket.
    pub fn place_inline_image(
        &mut self,
        prepared: PreparedInlineImage,
    ) -> Option<PlacedInlineImage> {
        self.commit_inline_image(prepared, None)
    }

    pub(crate) fn place_inline_raster(
        &mut self,
        prepared: PreparedInlineImage,
        raster: InlineRaster,
    ) -> Option<PlacedInlineImage> {
        self.commit_inline_image(prepared, Some(raster))
    }

    fn commit_inline_image(
        &mut self,
        prepared: PreparedInlineImage,
        raster: Option<InlineRaster>,
    ) -> Option<PlacedInlineImage> {
        if prepared.epoch != self.epoch.get() || prepared.revision != self.revision {
            return None;
        }
        let cursor = (prepared.column, prepared.row);
        self.prepare_inline_image(
            cursor,
            prepared.columns,
            prepared.rows,
            prepared.moves_cursor,
        )?;
        self.finish_grapheme();
        // Parser-driven placements run inside a feed, whose one revision
        // already covers every row write in byte order.
        if raster.is_none() {
            self.begin_revision(self.revision.checked_add(1)?);
        }
        self.next_inline_image_id = self.next_inline_image_id.checked_add(1)?;
        let image = InlineImageId(self.next_inline_image_id);
        let blank = self.screen.create_blank_cell();
        let lease = Arc::new(InlineImage {
            id: image,
            columns: prepared.columns,
            rows: prepared.rows,
            raster,
        });
        for slice in 0..prepared.rows {
            let row = self.screen.cursor_row;
            let column = prepared.column;
            let grid = &mut self.screen.grid;
            grid.overwrite_graphic(
                row,
                column,
                1,
                blank,
                blank,
                WideContinuationProtection::FromLead,
            );
            grid.set_image(
                row,
                column,
                ImageAnnotation {
                    image: Arc::clone(&lease),
                    slice,
                },
            );
            if slice + 1 < prepared.rows {
                self.motion(VerticalMotion::LineFeed);
            }
        }
        if prepared.moves_cursor {
            self.screen.cursor_col =
                (prepared.column + usize::from(prepared.columns)).min(self.screen.grid.cols() - 1);
        } else {
            self.screen.cursor_col = prepared.column;
            self.screen.cursor_row = prepared.row;
        }
        Some(PlacedInlineImage {
            image,
            lease: Arc::downgrade(&lease),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Limits;

    const LIMITS: Limits = Limits {
        max_rows: 5,
        max_cols: 8,
        max_cells: 40,
    };

    #[test]
    fn publish_and_erase_image_rows_with_their_cell_delta() {
        let mut terminal = Terminal::new(5, 8).unwrap();
        terminal.feed(b"\x1b[2;3H").unwrap();
        let before = terminal.frame().unwrap();
        let prepared = terminal.prepare_inline_image((2, 1), 3, 2, true).unwrap();
        let image = terminal.place_inline_image(prepared).unwrap();
        let frame = terminal.frame().unwrap();
        assert_eq!(frame.cursor().paint_position(), (2, 5));
        for (row, slice) in [(1, 0), (2, 1)] {
            assert_eq!(
                frame.inline_image_rows(row).collect::<Vec<_>>(),
                [(
                    2,
                    InlineImageRow {
                        image: image.id(),
                        slice
                    }
                )]
            );
        }
        assert_eq!(
            before.apply(&before.delta_to(&frame).unwrap(), LIMITS),
            Ok(frame.clone())
        );

        terminal.feed(b"outside").unwrap();
        assert!(image.is_retained());
        terminal.feed(b"\x1b[2;3H\x1b[2X\x1b[3;3H ").unwrap();
        let erased = terminal.frame().unwrap();
        assert!(erased.inline_image_rows(1).next().is_none());
        assert!(erased.inline_image_rows(2).next().is_none());
        assert!(image.is_retained(), "a reader still owns the earlier frame");
        assert_eq!(
            frame.apply(&frame.delta_to(&erased).unwrap(), LIMITS),
            Ok(erased)
        );
        drop(frame);
        assert!(!image.is_retained());
    }

    #[test]
    fn image_rows_follow_archive_reflow_and_expire_with_history() {
        let mut terminal = Terminal::new(3, 10).unwrap();
        terminal.feed(b"abcdefghij").unwrap();
        terminal.feed(b"xy").unwrap();
        let image = terminal.prepare_inline_image((2, 1), 2, 1, false).unwrap();
        let image = terminal.place_inline_image(image).unwrap();
        terminal.feed(b"\r\n\r\n\r\n").unwrap();
        assert!(image.is_retained());
        terminal.resize(3, 8).unwrap();
        let layout = terminal.history_layout().unwrap();
        let page = terminal
            .projected_history_page(&layout, 0, layout.len())
            .unwrap();
        let visible = (0..page.row_count)
            .flat_map(|row| {
                page.inline_image_rows(row)
                    .map(move |(column, image)| (row, column, image))
            })
            .collect::<Vec<_>>();
        assert_eq!(
            visible,
            [(
                1,
                4,
                InlineImageRow {
                    image: image.id(),
                    slice: 0
                }
            )]
        );
        drop(page);
        terminal.feed(b"\x1b[3J").unwrap();
        terminal.frame().unwrap();
        assert!(!image.is_retained());
    }

    #[test]
    fn only_current_primary_cursor_can_commit_and_no_move_preserves_decsc() {
        let mut terminal = Terminal::new(4, 8).unwrap();
        terminal.feed(b"\x1b[4;7H\x1b7\x1b[2;2H").unwrap();
        let stale = terminal.prepare_inline_image((1, 1), 1, 2, false).unwrap();
        terminal.feed(b"\x1b[31m").unwrap();
        assert!(terminal.place_inline_image(stale).is_none());
        let reservation = terminal.prepare_inline_image((1, 1), 1, 2, false).unwrap();
        terminal.place_inline_image(reservation).unwrap();
        assert_eq!(terminal.frame().unwrap().cursor().paint_position(), (1, 1));
        terminal.feed(b"\x1b8").unwrap();
        assert_eq!(terminal.frame().unwrap().cursor().paint_position(), (3, 6));

        terminal.feed(b"\x1b[?1049h").unwrap();
        assert!(terminal.prepare_inline_image((0, 0), 1, 1, true).is_none());
    }
}
