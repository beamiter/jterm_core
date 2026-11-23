//! Bounded direct Kitty images at the terminal's primary-screen cursor.

use super::{parse_command, Action, Assembled, Assembler, Caps, Command, Error, Format, Step};
use crate::terminal::{InlineRaster, PlacedInlineImage, Terminal};

const RETAINED_BYTES: usize = 2 * 1024 * 1024;
const ENCODED_BYTES: usize = 4 * RETAINED_BYTES.div_ceil(3);
const MAX_RESOURCES: usize = 32;
const CAPS: Caps = Caps {
    max_encoded_bytes: ENCODED_BYTES,
    max_decoded_bytes: RETAINED_BYTES,
    max_dimension: 4096,
    max_control_bytes: super::MAX_CONTROL_BYTES,
    max_pending_bytes: ENCODED_BYTES,
};

pub type PngDecoder = fn(&[u8], u32, u32) -> Option<Vec<u8>>;

pub(crate) struct InlineGraphics {
    assembler: Assembler,
    pending: Option<Transfer>,
    retained: Vec<RetainedImage>,
    decode_png: PngDecoder,
    cell_pixels: (u16, u16),
}

struct RetainedImage {
    client_id: Option<u32>,
    resource: PlacedInlineImage,
    bytes: usize,
}

#[derive(Clone, Copy, Default)]
struct Reply {
    id: Option<u32>,
    number: Option<u32>,
    quiet: u8,
}

impl Reply {
    // The full parser can reject an unsupported control before reaching i=.
    // Echo only numeric fields from the bounded header, never arbitrary bytes.
    fn scan(payload: &[u8]) -> Self {
        let control = payload[1..]
            .split(|&byte| byte == b';')
            .next()
            .unwrap_or_default();
        let mut reply = Self::default();
        if control.len() > CAPS.max_control_bytes {
            return reply;
        }
        if let Ok(control) = std::str::from_utf8(control) {
            for (key, value) in control.split(',').filter_map(|pair| pair.split_once('=')) {
                match key {
                    "i" => reply.id = super::parse_u32(value).ok(),
                    "I" => reply.number = super::parse_u32(value).ok(),
                    "q" => reply.quiet = value.parse::<u8>().unwrap_or_default().min(2),
                    _ => {}
                }
            }
        }
        reply
    }

    fn send(self, outcome: Result<(), Error>) -> Option<String> {
        if self.quiet == 2 || (self.quiet == 1 && outcome.is_ok()) {
            return None;
        }
        let identity = match (self.id, self.number) {
            (Some(id), None) => format!("i={id}"),
            (None, Some(number)) => format!("I={number}"),
            _ => return None,
        };
        let status = match outcome {
            Ok(()) => "OK",
            Err(Error::Invalid(_)) => "EINVAL:invalid image transfer",
            Err(Error::TooLarge) => "ENOSPC:image resource limit",
            Err(Error::NotSupported(_)) => "ENOTSUP:unsupported image operation",
        };
        Some(format!("\x1b_G{identity};{status}\x1b\\"))
    }
}

struct Transfer {
    reply: Reply,
    placement: Option<Placement>,
}

struct Placement {
    columns: Option<u32>,
    rows: Option<u32>,
    moves_cursor: bool,
}

impl Placement {
    fn parse(command: &Command<'_>) -> Result<Self, Error> {
        for key in ["x", "y", "w", "h", "X", "Y", "p"] {
            if command.u32_value(key)?.unwrap_or_default() != 0 {
                return Err(Error::NotSupported("offset, crop or placement ID"));
            }
        }
        if command.i32_value("z")?.unwrap_or_default() != 0 || command.number.is_some() {
            return Err(Error::NotSupported("layer or image number"));
        }
        let moves_cursor = match command.get("C") {
            None | Some("0") => true,
            Some("1") => false,
            _ => return Err(Error::Invalid("cursor policy")),
        };
        Ok(Self {
            columns: command.u32_value("c")?.filter(|&size| size != 0),
            rows: command.u32_value("r")?.filter(|&size| size != 0),
            moves_cursor,
        })
    }

    fn dimension(&self, pixels: (u32, u32), cell: (u16, u16)) -> Result<(u16, u16), Error> {
        let (width, height) = (u64::from(pixels.0), u64::from(pixels.1));
        let (cell_width, cell_height) = (u64::from(cell.0), u64::from(cell.1));
        let (cols, rows) = match self.columns.map(u64::from).zip(self.rows.map(u64::from)) {
            Some(size) => size,
            None => match (self.columns, self.rows) {
                (Some(cols), _) => {
                    let cols = u64::from(cols);
                    (
                        cols,
                        (cols * cell_width * height).div_ceil(width * cell_height),
                    )
                }
                (_, Some(rows)) => {
                    let rows = u64::from(rows);
                    (
                        (rows * cell_height * width).div_ceil(height * cell_width),
                        rows,
                    )
                }
                _ => (width.div_ceil(cell_width), height.div_ceil(cell_height)),
            },
        };
        Ok((
            cols.try_into().map_err(|_| Error::TooLarge)?,
            rows.try_into().map_err(|_| Error::TooLarge)?,
        ))
    }
}

impl InlineGraphics {
    pub(crate) fn new(cell_pixels: (u16, u16), decode_png: PngDecoder) -> Self {
        Self {
            assembler: Assembler::new(CAPS),
            pending: None,
            retained: Vec::new(),
            decode_png,
            cell_pixels: (cell_pixels.0.max(1), cell_pixels.1.max(1)),
        }
    }

    pub(crate) fn cancel(&mut self) {
        self.assembler.reset();
        self.pending = None;
    }

    pub(crate) fn apply(
        &mut self,
        terminal: &mut Terminal,
        cursor: (usize, usize),
        payload: &[u8],
    ) -> Option<String> {
        let command = match parse_command(payload, &CAPS) {
            Ok(command) => command,
            Err(error) => {
                self.cancel();
                return Reply::scan(payload).send(Err(error));
            }
        };
        let reply = Reply {
            id: command.id,
            number: command.number,
            quiet: command.quiet,
        };
        let (chunk, transfer) = if command.is_continuation() {
            let mut transfer = self.pending.take()?;
            if command.quiet_specified {
                transfer.reply.quiet = command.quiet;
            }
            (self.assembler.continue_transfer(&command), transfer)
        } else {
            self.cancel();
            let placement = match command.action {
                Action::Display => match Placement::parse(&command) {
                    Ok(placement) => Some(placement),
                    Err(error) => return reply.send(Err(error)),
                },
                Action::Query if !command.more => None,
                _ => return reply.send(Err(Error::NotSupported("action"))),
            };
            (
                self.assembler.begin_transfer(&command),
                Transfer { reply, placement },
            )
        };
        match chunk {
            Ok(Step::NeedMore) => {
                self.pending = Some(transfer);
                None
            }
            Ok(Step::Ready(assembled)) => transfer.reply.send(self.complete(
                terminal,
                cursor,
                transfer.reply.id.filter(|&id| id != 0),
                transfer.placement,
                assembled,
            )),
            Err(error) => transfer.reply.send(Err(error)),
            Ok(_) => unreachable!("the transfer either buffers or completes"),
        }
    }

    fn complete(
        &mut self,
        terminal: &mut Terminal,
        cursor: (usize, usize),
        client_id: Option<u32>,
        placement: Option<Placement>,
        assembled: Assembled,
    ) -> Result<(), Error> {
        let (width, height) = assembled.declared.ok_or(Error::Invalid("dimensions"))?;
        let bytes = width as usize * height as usize * 4;
        let reservation = if let Some(placement) = placement {
            self.retained.retain(|image| image.resource.is_retained());
            let retained_bytes = self.retained.iter().map(|image| image.bytes).sum::<usize>();
            if self.retained.len() == MAX_RESOURCES || bytes > RETAINED_BYTES - retained_bytes {
                return Err(Error::TooLarge);
            }
            if client_id.is_some()
                && self
                    .retained
                    .iter()
                    .any(|image| image.client_id == client_id)
            {
                return Err(Error::NotSupported("replacement of a retained image"));
            }
            let (columns, rows) = placement.dimension((width, height), self.cell_pixels)?;
            Some(
                terminal
                    .prepare_inline_image(cursor, columns, rows, placement.moves_cursor)
                    .ok_or(Error::NotSupported("viewport placement"))?,
            )
        } else {
            None
        };
        let rgba = match assembled.format {
            Format::Png => (self.decode_png)(&assembled.bytes, width, height)
                .ok_or(Error::Invalid("PNG data"))?,
            _ => assembled.into_rgba8()?.0,
        };
        let raster = InlineRaster::new(width, height, rgba).ok_or(Error::Invalid("RGBA size"))?;
        if let Some(reservation) = reservation {
            let resource = terminal
                .place_inline_raster(reservation, raster)
                .ok_or(Error::NotSupported("viewport placement"))?;
            self.retained.push(RetainedImage {
                client_id,
                resource,
                bytes,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Limits;

    fn terminal() -> Terminal {
        let mut terminal = Terminal::new(5, 80).unwrap();
        terminal.enable_inline_graphics((12, 24), |_, _, _| None);
        terminal
    }

    #[test]
    fn split_upload_and_queries_follow_the_same_output_order() {
        let mut terminal = terminal();
        let base = terminal.frame().unwrap();
        assert!(terminal
            .feed(b"\x1b_Ga=T,i=41,s=1,v=2,c=2,r=2,m=1;/wAA\x1b\\")
            .unwrap()
            .is_empty());
        let reply = terminal
            .feed(b"A\x1b[3;2H\x1b_Gm=0;/wD/AP8=\x1b\\\x1b[6n")
            .unwrap();
        assert_eq!(reply, b"\x1b_Gi=41;OK\x1b\\\x1b[4;4R");
        let frame = terminal.frame().unwrap();
        assert_eq!(frame.rows()[0].cells[0].character, 'A');
        let (column, image, slice) = frame.inline_images(2).next().unwrap();
        assert_eq!((column, slice, image.columns(), image.rows()), (1, 0, 2, 2));
        let raster = image.raster().unwrap();
        assert_eq!((raster.width(), raster.height()), (1, 2));
        assert_eq!(raster.rgba(), &[255, 0, 0, 255, 0, 255, 0, 255]);
        let (_, lower, slice) = frame.inline_images(3).next().unwrap();
        assert_eq!((lower.id(), slice), (image.id(), 1));
        let limits = Limits {
            max_rows: 5,
            max_cols: 80,
            max_cells: 400,
        };
        assert_eq!(
            base.apply(&base.delta_to(&frame).unwrap(), limits),
            Ok(frame.clone())
        );
        terminal.feed(b"\x1b[2J").unwrap();
        let erased = terminal.frame().unwrap();
        assert!(erased.inline_images(2).next().is_none());
        assert_eq!(
            frame.apply(&frame.delta_to(&erased).unwrap(), limits),
            Ok(erased)
        );
    }

    #[test]
    fn cancellation_and_alternate_screen_do_not_commit_pending_graphics() {
        let mut terminal = terminal();
        for cancel in [
            b"\x1b_Gm=0;\x18".as_slice(),
            b"\x1bc",
            b"\x1b[?1049h\x1b[?1049l",
        ] {
            terminal
                .feed(b"\x1b_Ga=T,i=2,s=1,v=1,m=1;AAAA\x1b\\")
                .unwrap();
            terminal.feed(cancel).unwrap();
            assert!(terminal.feed(b"\x1b_Gm=0;/w==\x1b\\").unwrap().is_empty());
            assert!(terminal.frame().unwrap().inline_images(0).next().is_none());
        }
        let query = b"\x1b_Ga=q,i=31,s=1,v=1,f=24;AAAA\x1b\\";
        assert_eq!(terminal.feed(query).unwrap(), b"\x1b_Gi=31;OK\x1b\\");
        let display = b"\x1b_Ga=T,i=2,s=1,v=1,f=24;AAAA\x1b\\";
        terminal.feed(b"\x1b[?1049h").unwrap();
        assert!(terminal
            .feed(display)
            .unwrap()
            .starts_with(b"\x1b_Gi=2;ENOTSUP:"));
        terminal.feed(b"\x1b[?1049l").unwrap();
        assert_eq!(terminal.feed(display).unwrap(), b"\x1b_Gi=2;OK\x1b\\");
    }

    #[test]
    fn admission_counts_resources_still_owned_by_published_frames() {
        let mut terminal = terminal();
        for column in 1..=MAX_RESOURCES {
            let output =
                format!("\x1b[1;{column}H\x1b_Ga=T,i={column},s=1,v=1,f=24,C=1;AAAA\x1b\\");
            assert_eq!(
                terminal.feed(output.as_bytes()).unwrap(),
                format!("\x1b_Gi={column};OK\x1b\\").as_bytes()
            );
        }
        let held = terminal.frame().unwrap();
        assert_eq!(held.inline_images(0).count(), MAX_RESOURCES);
        terminal.feed(b"\x1b[2J").unwrap();
        assert!(terminal.frame().unwrap().inline_images(0).next().is_none());
        let another = b"\x1b_Ga=T,i=80,s=1,v=1,f=24;AAAA\x1b\\";
        assert!(terminal
            .feed(another)
            .unwrap()
            .starts_with(b"\x1b_Gi=80;ENOSPC:"));
        drop(held);
        assert_eq!(terminal.feed(another).unwrap(), b"\x1b_Gi=80;OK\x1b\\");
    }

    #[test]
    fn large_direct_packet_can_be_partitioned_without_loosening_osc_limits() {
        let mut terminal = terminal();
        let packet = format!(
            "\x1b_Ga=T,i=1,s=48,v=32,c=2,r=2;{}\x1b\\",
            "////".repeat(2048)
        );
        assert!(terminal
            .feed(&packet.as_bytes()[..4200])
            .unwrap()
            .is_empty());
        assert_eq!(
            terminal.feed(&packet.as_bytes()[4200..]).unwrap(),
            b"\x1b_Gi=1;OK\x1b\\"
        );
        assert_eq!(
            terminal
                .frame()
                .unwrap()
                .inline_images(0)
                .next()
                .unwrap()
                .1
                .raster()
                .unwrap()
                .rgba()
                .len(),
            6144
        );
        terminal.enable_window_updates();
        terminal.feed(b"\x1b]0;before\x07").unwrap();
        let osc = format!("\x1b]0;{}\x07OK", "x".repeat(4100));
        terminal.feed(osc.as_bytes()).unwrap();
        assert_eq!(terminal.window_title(), Some("before"));
    }
}
