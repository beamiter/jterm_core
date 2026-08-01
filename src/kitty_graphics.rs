//! Kitty graphics protocol — the structural layer shared by every jterm terminal.
//!
//! The wire format is an APC packet whose payload is
//! `G<comma-separated control data>;<base64 image data>`. Everything a terminal
//! can decide about such a packet *without* decoding an image lives here:
//!
//! * control-data parsing ([`parse_command`]),
//! * chunk assembly across `m=1` continuations ([`Assembler`]),
//! * base64 decoding ([`decode_base64`]),
//! * raw-format (`f=24` / `f=32`) length validation and RGB→RGBA expansion
//!   ([`Assembled::into_rgba8`]),
//! * the PNG signature/IHDR structural sniff ([`png_dimensions`]),
//! * the caps and overflow guards every one of those steps needs ([`Caps`]).
//!
//! What is deliberately **not** here: the protocol responder, the image store,
//! placements and deletion. Those need the decoded width/height and the
//! decoder's error text, and each frontend decodes with a different library
//! (`image` on the egui/iced side, GDK on the GTK side). Hoisting the responder
//! would force every app to reorder its most delicate invariant, so each app
//! keeps its own responder and calls in here for the structural work.
//!
//! # Deliberate divergences from the four pre-hoist implementations
//!
//! * `f=` defaults to **RGBA** (`f=32`), the protocol default and jterm2's
//!   behaviour. jterm1/jterm3/jterm4 defaulted to PNG, so a command that omits
//!   `f=` now means "raw RGBA, `s=`/`v=` required" for them too. Pinned by the
//!   `format_defaults_to_rgba_not_png` test.
//! * Only `t=d` (direct/inline) transmission is supported. `t=f`, `t=t` and
//!   `t=s` parse into [`Transport`] but are rejected with
//!   [`Error::NotSupported`] before any buffering. jterm3 validated nothing
//!   here.
//! * Only `f=100`, `f=32` and `f=24` are accepted. jterm3's non-standard
//!   `png`/`jpeg`/`jpg`/`webp`/`rgb`/`rgba` aliases are gone.
//! * Raw payload length must match `s*v*channels` **exactly** (jterm2's rule),
//!   not "at least" (jterm1/jterm4 accepted trailing slack).
//! * Continuation chunks may carry only `m=` and an optional `q=` (jterm2's
//!   spec-correct rule), but in-flight transfers are keyed per image id with a
//!   separate anonymous slot (jterm1/jterm4's shape), so an upload for `i=1`
//!   survives an unrelated single-shot transfer for `i=2`.

use std::collections::HashMap;

/// Hard ceiling on the control-data section (everything before the first `;`).
///
/// Public because jterm2's APC scanner uses it to build a bounded recovery
/// prefix when a graphics packet is rejected before it is fully buffered.
pub const MAX_CONTROL_BYTES: usize = 16 * 1024;
/// Maximum number of separately keyed chunked uploads retained at once.
/// A byte-only cap is insufficient because `m=1` permits an empty payload.
pub const MAX_PENDING_TRANSFERS: usize = 256;

/// Memory ceilings applied to one terminal's graphics traffic.
///
/// Every field is a hard limit; nothing in this module allocates a buffer whose
/// size was not checked against these first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caps {
    /// Maximum base64 bytes accumulated for a single image, whitespace excluded.
    pub max_encoded_bytes: usize,
    /// Maximum bytes any single decode may produce, and the ceiling on the RGBA
    /// footprint implied by an image's dimensions.
    pub max_decoded_bytes: usize,
    /// Maximum width or height, in pixels, regardless of the byte budget.
    pub max_dimension: u32,
    /// Maximum length of the control-data section of one command.
    pub max_control_bytes: usize,
    /// Maximum base64 bytes buffered across *all* in-flight transfers.
    pub max_pending_bytes: usize,
}

impl Caps {
    /// Budget for terminals that attach images to finished command blocks
    /// (jterm1, jterm4). One block's images share the decoded budget.
    pub const BLOCK: Self = Self {
        max_encoded_bytes: 16 * 1024 * 1024,
        max_decoded_bytes: 16 * 1024 * 1024,
        max_dimension: 16_384,
        max_control_bytes: MAX_CONTROL_BYTES,
        max_pending_bytes: 16 * 1024 * 1024,
    };

    /// Budget for terminals that keep a live screen image store (jterm2,
    /// jterm3), where a single image may legitimately cover a large window.
    pub const SCREEN: Self = Self {
        max_encoded_bytes: 64 * 1024 * 1024,
        max_decoded_bytes: 64 * 1024 * 1024,
        max_dimension: 16_384,
        max_control_bytes: MAX_CONTROL_BYTES,
        max_pending_bytes: 64 * 1024 * 1024,
    };
}

/// Typed failure. Apps map these onto their own wire codes (`EINVAL`,
/// `ENOTSUP`, …) because the reply format is responder policy, not structure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The command is malformed: it cannot be parsed, or it contradicts itself.
    Invalid(&'static str),
    /// A cap in [`Caps`] would be exceeded, or a size computation overflowed.
    TooLarge,
    /// The command is well-formed but asks for something this module does not
    /// implement (a non-direct transport, an unknown format, a control key that
    /// cannot be silently ignored).
    NotSupported(&'static str),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(what) => write!(f, "invalid kitty graphics command: {what}"),
            Self::TooLarge => write!(f, "kitty graphics command exceeds the configured limits"),
            Self::NotSupported(what) => write!(f, "unsupported kitty graphics {what}"),
        }
    }
}

impl std::error::Error for Error {}

/// Image data format from the `f=` control.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// `f=100` — a PNG file. Dimensions live in its IHDR.
    Png,
    /// `f=24` — raw 24-bit RGB pixels. Requires `s=` and `v=`.
    Rgb8,
    /// `f=32` — raw 32-bit RGBA pixels. Requires `s=` and `v=`. The default.
    Rgba8,
}

impl Format {
    /// Parse an `f=` value. Only the three numeric protocol values exist.
    pub fn from_control(value: &str) -> Result<Self, Error> {
        match value {
            "100" => Ok(Self::Png),
            "24" => Ok(Self::Rgb8),
            "32" => Ok(Self::Rgba8),
            _ => Err(Error::NotSupported("format")),
        }
    }

    /// Bytes per pixel in the *transmitted* data. PNG reports 4 because its
    /// decoded RGBA footprint is what the memory budget must cover.
    pub fn source_channels(self) -> usize {
        match self {
            Self::Png | Self::Rgba8 => 4,
            Self::Rgb8 => 3,
        }
    }

    /// Whether the payload is raw pixels whose dimensions must come from
    /// `s=`/`v=` rather than from the encoded data itself.
    pub fn is_raw(self) -> bool {
        matches!(self, Self::Rgb8 | Self::Rgba8)
    }
}

/// The `a=` control, parsed but never executed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// `a=t` — transmit only. The protocol default when `a=` is absent.
    Transmit,
    /// `a=T` — transmit and immediately display at the cursor.
    Display,
    /// `a=q` — support probe.
    Query,
    /// `a=p` — place an already transmitted image.
    Placement,
    /// `a=d` — delete images or placements.
    Delete,
    /// A single-character action this module does not model. Apps decide
    /// whether to answer or ignore it.
    Other(char),
}

impl Action {
    fn from_control(value: &str) -> Result<Self, Error> {
        Ok(
            match single_char(value).ok_or(Error::Invalid("a= must be one character"))? {
                't' => Self::Transmit,
                'T' => Self::Display,
                'q' => Self::Query,
                'p' => Self::Placement,
                'd' => Self::Delete,
                other => Self::Other(other),
            },
        )
    }

    /// Whether this action carries image data that [`Assembler`] should buffer.
    pub fn is_transmit(self) -> bool {
        matches!(self, Self::Transmit | Self::Display)
    }
}

/// The `t=` control. Only [`Transport::Direct`] is supported; the rest are
/// recognised so an app can report *which* unsupported transport was asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    /// `t=d` — the payload is inline base64. The default and the only
    /// transport this module accepts.
    Direct,
    /// `t=f` — a filesystem path.
    File,
    /// `t=t` — a temporary file the terminal should delete.
    TempFile,
    /// `t=s` — a POSIX shared-memory object.
    SharedMemory,
}

impl Transport {
    fn from_control(value: &str) -> Result<Self, Error> {
        match single_char(value).ok_or(Error::Invalid("t= must be one character"))? {
            'd' => Ok(Self::Direct),
            'f' => Ok(Self::File),
            't' => Ok(Self::TempFile),
            's' => Ok(Self::SharedMemory),
            _ => Err(Error::NotSupported("transport")),
        }
    }
}

/// One parsed graphics command. Borrows the caller's payload; nothing is copied
/// except the handful of integers that were parsed out of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command<'a> {
    /// `a=`, defaulting to [`Action::Transmit`].
    pub action: Action,
    /// `f=`, defaulting to [`Format::Rgba8`] — the protocol default.
    pub format: Format,
    /// `t=`, defaulting to [`Transport::Direct`].
    pub transport: Transport,
    /// `i=` — the client's image id. `Some(0)` is the explicit anonymous id and
    /// is preserved as written so responders can echo it back verbatim.
    pub id: Option<u32>,
    /// `I=` — the client's image number.
    pub number: Option<u32>,
    /// `p=` — the placement id.
    pub placement: Option<u32>,
    /// `s=` — declared pixel width, meaningful only for raw formats.
    pub width: Option<u32>,
    /// `v=` — declared pixel height, meaningful only for raw formats.
    pub height: Option<u32>,
    /// `m=1` — more chunks follow.
    pub more: bool,
    /// `q=` — 0 answers everything, 1 suppresses `OK`, 2 suppresses errors too.
    pub quiet: u8,
    /// The base64 section, i.e. everything after the first `;`. Empty when the
    /// command has no `;` at all.
    pub payload_b64: &'a str,
    /// The raw control-data section, re-iterable through [`Command::pairs`].
    control: &'a str,
    more_specified: bool,
    quiet_specified: bool,
    /// Any control other than `m`/`q` was present, so this cannot be a legal
    /// continuation chunk.
    metadata: bool,
}

impl<'a> Command<'a> {
    /// Iterate every `key=value` pair in the control data, in wire order.
    ///
    /// Every pair is guaranteed to contain `=` because [`parse_command`]
    /// rejects the command otherwise; keys this module does not model (`x`,
    /// `y`, `w`, `h`, `c`, `r`, `z`, `C`, `X`, `Y`, `d`, …) are read from here
    /// by whichever app implements placement or deletion.
    pub fn pairs(&self) -> impl Iterator<Item = (&'a str, &'a str)> {
        self.control
            .split(',')
            .filter(|pair| !pair.is_empty())
            .filter_map(|pair| pair.split_once('='))
    }

    /// The last value written for `key`, or `None` when absent. Duplicate keys
    /// resolve the same way the parser resolves them: the last one wins.
    pub fn get(&self, key: &str) -> Option<&'a str> {
        let mut found = None;
        for (name, value) in self.pairs() {
            if name == key {
                found = Some(value);
            }
        }
        found
    }

    /// `key` parsed as an unsigned control value.
    pub fn u32_value(&self, key: &str) -> Result<Option<u32>, Error> {
        self.get(key).map(parse_u32).transpose()
    }

    /// `key` parsed as a signed control value (`z=` is the only signed control
    /// in the protocol).
    pub fn i32_value(&self, key: &str) -> Result<Option<i32>, Error> {
        self.get(key)
            .map(|value| {
                value
                    .parse::<i32>()
                    .map_err(|_| Error::Invalid("signed control value"))
            })
            .transpose()
    }

    /// `s=`/`v=` as a pair, present only when both were given.
    pub fn declared(&self) -> Option<(u32, u32)> {
        self.width.zip(self.height)
    }

    /// Whether this command is a legal continuation chunk: it specified `m=`
    /// and carried no control other than `m` and `q`.
    ///
    /// The protocol sends metadata only on the first chunk, so a command that
    /// carries any other control is a new command even when `m=` is present.
    pub fn is_continuation(&self) -> bool {
        self.more_specified && !self.metadata
    }

    /// Reject anything but `t=d`. [`Assembler::feed`] calls this for transmit
    /// actions; apps that dispatch `a=q` themselves should call it too.
    pub fn require_direct_transport(&self) -> Result<(), Error> {
        if self.transport == Transport::Direct {
            Ok(())
        } else {
            Err(Error::NotSupported("transport"))
        }
    }
}

/// Whether a buffered APC payload belongs to the graphics protocol.
pub fn is_graphics_payload(payload: &[u8]) -> bool {
    payload.first() == Some(&b'G')
}

/// Parse the control data of one graphics command.
///
/// `payload` is the APC body: the bytes between `ESC _` and `ESC \`, starting
/// with `G`.
///
/// Splitting exactly once at `;` matters: base64 padding (`=`) belongs to the
/// data section and must never be read as another control pair.
pub fn parse_command<'a>(payload: &'a [u8], caps: &Caps) -> Result<Command<'a>, Error> {
    let rest = payload
        .strip_prefix(b"G")
        .ok_or(Error::Invalid("payload is not a graphics command"))?;
    let (control, body) = match memchr::memchr(b';', rest) {
        Some(index) => (&rest[..index], &rest[index + 1..]),
        None => (rest, &rest[rest.len()..]),
    };
    if control.len() > caps.max_control_bytes {
        return Err(Error::TooLarge);
    }
    let control =
        std::str::from_utf8(control).map_err(|_| Error::Invalid("control data is not UTF-8"))?;
    let payload_b64 =
        std::str::from_utf8(body).map_err(|_| Error::Invalid("image data is not UTF-8"))?;

    let mut command = Command {
        action: Action::Transmit,
        format: Format::Rgba8,
        transport: Transport::Direct,
        id: None,
        number: None,
        placement: None,
        width: None,
        height: None,
        more: false,
        quiet: 0,
        payload_b64,
        control,
        more_specified: false,
        quiet_specified: false,
        metadata: false,
    };

    for pair in control.split(',') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair
            .split_once('=')
            .ok_or(Error::Invalid("control pair without '='"))?;
        match key {
            "m" => {
                command.more = match value {
                    "0" => false,
                    "1" => true,
                    _ => return Err(Error::Invalid("m= must be 0 or 1")),
                };
                command.more_specified = true;
                continue;
            }
            "q" => {
                let quiet = parse_u32(value)?;
                if quiet > 2 {
                    return Err(Error::Invalid("q= must be 0, 1 or 2"));
                }
                command.quiet = quiet as u8;
                command.quiet_specified = true;
                continue;
            }
            _ => command.metadata = true,
        }
        match key {
            "a" => command.action = Action::from_control(value)?,
            "f" => command.format = Format::from_control(value)?,
            "t" => command.transport = Transport::from_control(value)?,
            "i" => command.id = Some(parse_u32(value)?),
            "I" => command.number = Some(parse_u32(value)?),
            "p" => command.placement = Some(parse_u32(value)?),
            "s" => command.width = Some(parse_u32(value)?),
            "v" => command.height = Some(parse_u32(value)?),
            // Unicode placeholders and the virtual-placement controls change
            // where an image lives on screen; silently ignoring them would
            // draw the image in the wrong place instead of reporting it.
            "U" | "P" | "Q" | "H" | "V" => return Err(Error::NotSupported("placement control")),
            // Ignoring o=z would hand a zlib stream to a PNG decoder.
            "o" => return Err(Error::NotSupported("compression")),
            // Ignoring these would silently drop a client's file/frame data.
            "S" | "O" => return Err(Error::NotSupported("external data control")),
            // Everything else — including the placement and delete controls
            // this module does not model — is tolerated for forward
            // compatibility and read back through Command::pairs.
            _ => {}
        }
    }

    if command.id.is_some() && command.number.is_some() {
        return Err(Error::Invalid("i= and I= are mutually exclusive"));
    }
    Ok(command)
}

/// A caps-checked pixel layout. Every field was computed with checked
/// arithmetic and compared against [`Caps`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RawLayout {
    /// Validated pixel width.
    pub width: u32,
    /// Validated pixel height.
    pub height: u32,
    /// `width * height`.
    pub pixels: usize,
    /// Bytes the transmitted data must contain, exactly.
    pub source_bytes: usize,
    /// Bytes the RGBA form occupies.
    pub rgba_bytes: usize,
    /// Row stride of the RGBA form, for texture uploads.
    pub rgba_stride: usize,
}

/// Validate a pixel geometry against `caps` without allocating anything.
///
/// For [`Format::Png`] this reports the decoded RGBA budget, which is what a
/// PNG's IHDR must be checked against before a decoder sees the file.
pub fn raw_layout(
    width: u32,
    height: u32,
    format: Format,
    caps: &Caps,
) -> Result<RawLayout, Error> {
    if width == 0 || height == 0 {
        return Err(Error::Invalid("image dimensions must be non-zero"));
    }
    if width > caps.max_dimension || height > caps.max_dimension {
        return Err(Error::TooLarge);
    }
    let pixels = (width as usize)
        .checked_mul(height as usize)
        .ok_or(Error::TooLarge)?;
    let source_bytes = pixels
        .checked_mul(format.source_channels())
        .ok_or(Error::TooLarge)?;
    let rgba_bytes = pixels.checked_mul(4).ok_or(Error::TooLarge)?;
    let rgba_stride = (width as usize).checked_mul(4).ok_or(Error::TooLarge)?;
    if source_bytes > caps.max_decoded_bytes || rgba_bytes > caps.max_decoded_bytes {
        return Err(Error::TooLarge);
    }
    Ok(RawLayout {
        width,
        height,
        pixels,
        source_bytes,
        rgba_bytes,
        rgba_stride,
    })
}

/// Read a PNG's dimensions from its signature and IHDR, and check them against
/// `caps`.
///
/// This is a structural sniff, not a decoder: it exists so a 100-byte payload
/// cannot make an image library allocate a gigapixel canvas. A payload that
/// passes still has to be decoded by the caller.
pub fn png_dimensions(bytes: &[u8], caps: &Caps) -> Result<(u32, u32), Error> {
    const SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    const IHDR_LENGTH: u32 = 13;
    const HEADER_BYTES: usize = 24;

    if bytes.len() < HEADER_BYTES {
        return Err(Error::Invalid("PNG header is truncated"));
    }
    if &bytes[..8] != SIGNATURE {
        return Err(Error::Invalid("payload is not a PNG"));
    }
    if read_be_u32(&bytes[8..12]) != IHDR_LENGTH || &bytes[12..16] != b"IHDR" {
        return Err(Error::Invalid("PNG does not start with an IHDR chunk"));
    }
    let width = read_be_u32(&bytes[16..20]);
    let height = read_be_u32(&bytes[20..24]);
    let layout = raw_layout(width, height, Format::Png, caps)?;
    Ok((layout.width, layout.height))
}

/// Decode standard-alphabet base64.
///
/// Tolerates ASCII whitespace anywhere and missing or repeated `=` padding at
/// the end — the union of what the four pre-hoist decoders accepted. Interior
/// padding, characters outside the alphabet and an impossible length
/// (`len % 4 == 1`) are rejected. The output length is computed and checked
/// against `max_decoded` *before* a single byte is reserved.
pub fn decode_base64(input: &[u8], max_decoded: usize) -> Result<Vec<u8>, Error> {
    let mut significant = 0usize;
    let mut padded = false;
    for &byte in input {
        if byte.is_ascii_whitespace() {
            continue;
        }
        if byte == b'=' {
            padded = true;
            continue;
        }
        if padded {
            return Err(Error::Invalid("base64 padding is not at the end"));
        }
        if base64_index(byte).is_none() {
            return Err(Error::Invalid("base64 alphabet"));
        }
        significant += 1;
    }

    let remainder = significant % 4;
    if remainder == 1 {
        return Err(Error::Invalid("base64 length"));
    }
    let decoded_len = significant / 4 * 3 + remainder.saturating_sub(1);
    if decoded_len > max_decoded {
        return Err(Error::TooLarge);
    }

    let mut out = Vec::new();
    out.try_reserve_exact(decoded_len)
        .map_err(|_| Error::TooLarge)?;
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for &byte in input {
        let Some(value) = base64_index(byte) else {
            continue;
        };
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
            buffer &= (1u32 << bits) - 1;
        }
    }
    debug_assert_eq!(out.len(), decoded_len);
    Ok(out)
}

/// A transfer whose final chunk has arrived, decoded but not yet interpreted as
/// pixels.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Assembled {
    /// The `f=` format of the first chunk.
    pub format: Format,
    /// Dimensions known before decoding: `s=`/`v=` for raw formats, the IHDR
    /// values for [`Format::Png`].
    pub declared: Option<(u32, u32)>,
    /// The decoded payload — raw pixels for `f=24`/`f=32`, the PNG file itself
    /// for `f=100`.
    pub bytes: Vec<u8>,
    /// `a=T` rather than `a=t`: the client wants this shown immediately.
    pub display: bool,
    /// `i=` exactly as written, so a responder can echo it.
    pub id: Option<u32>,
    /// `I=` exactly as written.
    pub number: Option<u32>,
    /// `p=` exactly as written.
    pub placement: Option<u32>,
    /// The most recent `q=` seen across the transfer's chunks.
    pub quiet: u8,
    /// The caps the assembling terminal runs with, carried so the pixel-level
    /// helpers below need no second argument.
    pub caps: Caps,
}

impl Assembled {
    /// Turn a raw-format transfer into RGBA pixels.
    ///
    /// The payload length must match `s * v * channels` exactly; `f=24` is
    /// expanded to RGBA with a fully opaque alpha. Fails for [`Format::Png`],
    /// which needs a real decoder.
    pub fn into_rgba8(self) -> Result<(Vec<u8>, u32, u32), Error> {
        if !self.format.is_raw() {
            return Err(Error::Invalid("into_rgba8 requires f=24 or f=32"));
        }
        let (width, height) = self
            .declared
            .ok_or(Error::Invalid("raw formats require s= and v="))?;
        let layout = raw_layout(width, height, self.format, &self.caps)?;
        if self.bytes.len() != layout.source_bytes {
            return Err(Error::Invalid("raw image length does not match s= and v="));
        }
        if self.format == Format::Rgba8 {
            return Ok((self.bytes, width, height));
        }
        let mut rgba = Vec::new();
        rgba.try_reserve_exact(layout.rgba_bytes)
            .map_err(|_| Error::TooLarge)?;
        for pixel in self.bytes.chunks_exact(3) {
            rgba.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 0xFF]);
        }
        debug_assert_eq!(rgba.len(), layout.rgba_bytes);
        Ok((rgba, width, height))
    }

    /// Re-read the PNG dimensions from the payload. [`Assembler`] already
    /// validated them into [`Assembled::declared`]; this exists so a caller
    /// holding only the bytes need not thread the caps through.
    pub fn png_dimensions(&self) -> Result<(u32, u32), Error> {
        png_dimensions(&self.bytes, &self.caps)
    }
}

/// What [`Assembler::feed`] made of one payload.
#[derive(Debug, PartialEq, Eq)]
pub enum Step<'a> {
    /// The payload is not a graphics command — it does not start with `G`.
    NotOurs,
    /// A chunk was buffered; the transfer continues.
    NeedMore,
    /// The final chunk arrived and the payload decoded.
    Ready(Assembled),
    /// A parsed command that transmits nothing (`a=q`, `a=p`, `a=d`, an
    /// unmodelled action). The app owns these.
    Other {
        /// The parsed command.
        command: Command<'a>,
        /// An in-flight transfer was aborted to make room for it. jterm2 turns
        /// this into "chunked transfer interrupted by another action" for every
        /// action but `a=d`.
        interrupted: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Slot {
    Anonymous,
    Id(u32),
}

#[derive(Debug)]
struct Pending {
    format: Format,
    declared: Option<(u32, u32)>,
    display: bool,
    id: Option<u32>,
    number: Option<u32>,
    placement: Option<u32>,
    quiet: u8,
    encoded: Vec<u8>,
}

/// Reassembles chunked (`m=1`) transfers.
///
/// In-flight transfers are keyed by `i=`, with one extra slot for anonymous
/// (`i=0` or absent) uploads, so a chunked upload survives an unrelated
/// single-shot transfer. Continuation chunks carry no id, so they are routed to
/// the most recently started chunked transfer.
#[derive(Debug)]
pub struct Assembler {
    caps: Caps,
    in_flight: HashMap<u32, Pending>,
    anonymous: Option<Pending>,
    /// The slot an id-less continuation belongs to.
    current: Option<Slot>,
    /// Base64 bytes held across every stored slot.
    pending_bytes: usize,
}

impl Assembler {
    /// A new assembler with no in-flight state.
    pub fn new(caps: Caps) -> Self {
        Self {
            caps,
            in_flight: HashMap::new(),
            anonymous: None,
            current: None,
            pending_bytes: 0,
        }
    }

    /// The caps this assembler enforces.
    pub fn caps(&self) -> &Caps {
        &self.caps
    }

    /// Whether any transfer is waiting for more chunks.
    pub fn has_pending(&self) -> bool {
        self.anonymous.is_some() || !self.in_flight.is_empty()
    }

    /// Base64 bytes currently buffered across every in-flight transfer.
    pub fn pending_bytes(&self) -> usize {
        self.pending_bytes
    }

    /// Drop every in-flight transfer. Call when a block ends or the terminal
    /// resets so a half-finished upload cannot leak into the next command.
    pub fn reset(&mut self) {
        self.in_flight.clear();
        self.anonymous = None;
        self.current = None;
        self.pending_bytes = 0;
    }

    /// Feed one APC payload — the bytes between `ESC _` and `ESC \`.
    ///
    /// A payload that fails to parse aborts the transfer an id-less
    /// continuation would have landed in, because its `m=` boundary is unknown
    /// and the byte stream can no longer be trusted to be aligned.
    pub fn feed<'a>(&mut self, payload: &'a [u8]) -> Result<Step<'a>, Error> {
        if !is_graphics_payload(payload) {
            return Ok(Step::NotOurs);
        }
        let command = match parse_command(payload, &self.caps) {
            Ok(command) => command,
            Err(error) => {
                self.abort_current();
                return Err(error);
            }
        };

        if !command.action.is_transmit() {
            let interrupted = self.has_pending();
            if interrupted {
                self.reset();
            }
            return Ok(Step::Other {
                command,
                interrupted,
            });
        }

        if command.is_continuation() {
            self.continue_transfer(&command)
        } else {
            self.begin_transfer(&command)
        }
    }

    fn begin_transfer<'a>(&mut self, command: &Command<'_>) -> Result<Step<'a>, Error> {
        let slot = match command.id.filter(|id| *id != 0) {
            Some(id) => Slot::Id(id),
            None => Slot::Anonymous,
        };
        if self.take_slot(slot).is_some() {
            // The protocol sends metadata on the first chunk only, so a second
            // one for the same image is a client that lost track of its own
            // transfer. Drop the half-built upload rather than splice it.
            return Err(Error::Invalid(
                "continuation chunks may carry only m= and an optional q=",
            ));
        }
        if command.more && self.pending_transfer_count() >= MAX_PENDING_TRANSFERS {
            return Err(Error::TooLarge);
        }
        command.require_direct_transport()?;

        let mut entry = Pending {
            format: command.format,
            declared: command.declared(),
            display: command.action == Action::Display,
            id: command.id,
            number: command.number,
            placement: command.placement,
            quiet: command.quiet,
            encoded: Vec::new(),
        };
        self.append(&mut entry, command.payload_b64)?;
        self.settle(slot, entry, command.more)
    }

    fn continue_transfer<'a>(&mut self, command: &Command<'_>) -> Result<Step<'a>, Error> {
        let Some(slot) = self.current.take() else {
            return Err(Error::Invalid(
                "continuation without a transfer in progress",
            ));
        };
        let Some(mut entry) = self.take_slot(slot) else {
            return Err(Error::Invalid(
                "continuation without a transfer in progress",
            ));
        };
        if command.quiet_specified {
            entry.quiet = command.quiet;
        }
        self.append(&mut entry, command.payload_b64)?;
        self.settle(slot, entry, command.more)
    }

    /// Buffer or finish `entry` depending on `more`. Any error here has already
    /// removed the entry from its slot, so a failed chunk aborts the transfer.
    fn settle<'a>(&mut self, slot: Slot, entry: Pending, more: bool) -> Result<Step<'a>, Error> {
        if more {
            self.store_slot(slot, entry);
            self.current = Some(slot);
            return Ok(Step::NeedMore);
        }
        Ok(Step::Ready(self.finish(entry)?))
    }

    fn finish(&self, entry: Pending) -> Result<Assembled, Error> {
        // Reject an impossible raw geometry before decoding, so a tiny payload
        // that claims to be a gigapixel image never reaches an allocation.
        let mut declared = None;
        if entry.format.is_raw() {
            let (width, height) = entry
                .declared
                .ok_or(Error::Invalid("raw formats require s= and v="))?;
            raw_layout(width, height, entry.format, &self.caps)?;
            declared = Some((width, height));
        }
        let bytes = decode_base64(&entry.encoded, self.caps.max_decoded_bytes)?;
        if entry.format == Format::Png {
            declared = Some(png_dimensions(&bytes, &self.caps)?);
        }
        Ok(Assembled {
            format: entry.format,
            declared,
            bytes,
            display: entry.display,
            id: entry.id,
            number: entry.number,
            placement: entry.placement,
            quiet: entry.quiet,
            caps: self.caps,
        })
    }

    /// Append one chunk's base64, stripping ASCII whitespace some emitters add.
    /// The final length is checked against both caps before anything is
    /// reserved: reserving an attacker-controlled chunk first would briefly
    /// bypass the cap and can abort on allocation failure.
    fn append(&self, entry: &mut Pending, body: &str) -> Result<(), Error> {
        let chunk = body.bytes().filter(|byte| !byte.is_ascii_whitespace());
        let chunk_len = chunk.clone().count();
        let encoded_len = entry
            .encoded
            .len()
            .checked_add(chunk_len)
            .ok_or(Error::TooLarge)?;
        if encoded_len > self.caps.max_encoded_bytes {
            return Err(Error::TooLarge);
        }
        if self
            .pending_bytes
            .checked_add(encoded_len)
            .is_none_or(|total| total > self.caps.max_pending_bytes)
        {
            return Err(Error::TooLarge);
        }
        entry
            .encoded
            .try_reserve(chunk_len)
            .map_err(|_| Error::TooLarge)?;
        entry.encoded.extend(chunk);
        Ok(())
    }

    fn abort_current(&mut self) {
        if let Some(slot) = self.current.take() {
            self.take_slot(slot);
        }
    }

    fn pending_transfer_count(&self) -> usize {
        self.in_flight
            .len()
            .saturating_add(usize::from(self.anonymous.is_some()))
    }

    fn take_slot(&mut self, slot: Slot) -> Option<Pending> {
        let entry = match slot {
            Slot::Anonymous => self.anonymous.take(),
            Slot::Id(id) => self.in_flight.remove(&id),
        };
        if let Some(entry) = &entry {
            self.pending_bytes = self.pending_bytes.saturating_sub(entry.encoded.len());
            if self.current == Some(slot) {
                self.current = None;
            }
        }
        entry
    }

    fn store_slot(&mut self, slot: Slot, entry: Pending) {
        self.pending_bytes = self.pending_bytes.saturating_add(entry.encoded.len());
        match slot {
            Slot::Anonymous => self.anonymous = Some(entry),
            Slot::Id(id) => {
                self.in_flight.insert(id, entry);
            }
        }
    }
}

fn single_char(value: &str) -> Option<char> {
    let mut chars = value.chars();
    chars.next().filter(|_| chars.next().is_none())
}

fn parse_u32(value: &str) -> Result<u32, Error> {
    value
        .parse::<u32>()
        .map_err(|_| Error::Invalid("unsigned control value"))
}

fn read_be_u32(bytes: &[u8]) -> u32 {
    let mut value = 0u32;
    for &byte in bytes.iter().take(4) {
        value = (value << 8) | u32::from(byte);
    }
    value
}

fn base64_index(byte: u8) -> Option<u32> {
    match byte {
        b'A'..=b'Z' => Some(u32::from(byte - b'A')),
        b'a'..=b'z' => Some(u32::from(byte - b'a') + 26),
        b'0'..=b'9' => Some(u32::from(byte - b'0') + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    fn encode(input: &[u8]) -> String {
        let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
        for chunk in input.chunks(3) {
            let a = chunk[0];
            let b = chunk.get(1).copied().unwrap_or(0);
            let c = chunk.get(2).copied().unwrap_or(0);
            out.push(TABLE[(a >> 2) as usize] as char);
            out.push(TABLE[(((a & 0x03) << 4) | (b >> 4)) as usize] as char);
            out.push(if chunk.len() > 1 {
                TABLE[(((b & 0x0f) << 2) | (c >> 6)) as usize] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                TABLE[(c & 0x3f) as usize] as char
            } else {
                '='
            });
        }
        out
    }

    fn png_header(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"\x89PNG\r\n\x1a\n");
        bytes.extend_from_slice(&13u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
        bytes
    }

    fn parse(payload: &str) -> Result<Command<'_>, Error> {
        parse_command(payload.as_bytes(), &Caps::SCREEN)
    }

    fn assembler() -> Assembler {
        Assembler::new(Caps::SCREEN)
    }

    fn ready(step: Step<'_>) -> Assembled {
        match step {
            Step::Ready(assembled) => assembled,
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    // ---- control parsing -------------------------------------------------

    #[test]
    fn parses_standard_apc_and_preserves_base64_padding() {
        let command = parse("Gf=24,i=1,s=1,v=1;AQI=").unwrap();
        assert_eq!(command.action, Action::Transmit);
        assert_eq!(command.format, Format::Rgb8);
        assert_eq!(command.id, Some(1));
        assert_eq!(command.declared(), Some((1, 1)));
        // The single split at ';' keeps the '=' padding out of the controls.
        assert_eq!(command.payload_b64, "AQI=");
        assert_eq!(command.get("f"), Some("24"));
    }

    #[test]
    fn a_payload_without_a_semicolon_has_no_data() {
        let command = parse("Ga=d,d=I,i=22").unwrap();
        assert_eq!(command.action, Action::Delete);
        assert_eq!(command.payload_b64, "");
        assert_eq!(command.get("d"), Some("I"));
    }

    #[test]
    fn format_defaults_to_rgba_not_png() {
        // Decision pinned: jterm1/jterm3/jterm4 used to default to PNG here.
        let command = parse("Gi=5,s=1,v=1;AQIDBA==").unwrap();
        assert_eq!(command.format, Format::Rgba8);

        let assembled = ready(assembler().feed(b"Gi=5,s=1,v=1;AQIDBA==").unwrap());
        assert_eq!(assembled.format, Format::Rgba8);
        assert_eq!(assembled.into_rgba8().unwrap(), (vec![1, 2, 3, 4], 1, 1));
    }

    #[test]
    fn defaults_are_transmit_direct_and_loud() {
        let command = parse("G;").unwrap();
        assert_eq!(command.action, Action::Transmit);
        assert_eq!(command.transport, Transport::Direct);
        assert_eq!(command.quiet, 0);
        assert!(!command.more);
        assert!(!command.is_continuation());
    }

    #[test]
    fn control_data_at_the_cap_parses_and_one_byte_past_is_too_large() {
        let caps = Caps {
            max_control_bytes: 16,
            ..Caps::SCREEN
        };
        let at_cap = format!("G{};AA==", "q=0,".repeat(4));
        assert_eq!(at_cap.len() - 1 - "AA==".len() - 1, 16);
        assert!(parse_command(at_cap.as_bytes(), &caps).is_ok());

        let past_cap = format!("G{}q=0;AA==", "q=0,".repeat(4));
        assert_eq!(
            parse_command(past_cap.as_bytes(), &caps),
            Err(Error::TooLarge)
        );
    }

    #[test]
    fn oversized_control_data_is_rejected_before_the_pairs_are_read() {
        // jterm2's regression: 16 KiB of junk with no '=' must not be parsed.
        let payload = format!("Gi=49,{}", "x".repeat(MAX_CONTROL_BYTES));
        assert_eq!(parse(&payload), Err(Error::TooLarge));
    }

    #[test]
    fn a_pair_without_an_equals_sign_is_rejected() {
        assert_eq!(
            parse("Ga=p,i=49,bad"),
            Err(Error::Invalid("control pair without '='"))
        );
    }

    #[test]
    fn empty_pairs_are_skipped() {
        let command = parse("G,,a=T,,i=3,;AA==").unwrap();
        assert_eq!(command.action, Action::Display);
        assert_eq!(command.id, Some(3));
    }

    #[test]
    fn explicitly_unsupported_keys_are_rejected_rather_than_ignored() {
        for control in ["U=1", "P=1", "Q=1", "H=-1", "V=1"] {
            assert_eq!(
                parse(&format!("Ga=p,i=48,{control}")),
                Err(Error::NotSupported("placement control")),
                "{control}"
            );
        }
        assert_eq!(
            parse("Ga=t,i=1,o=z,s=1,v=1;AA=="),
            Err(Error::NotSupported("compression"))
        );
        for control in ["S=64", "O=8"] {
            assert_eq!(
                parse(&format!("Ga=t,i=1,{control};AA==")),
                Err(Error::NotSupported("external data control")),
                "{control}"
            );
        }
    }

    #[test]
    fn unknown_keys_are_tolerated_but_count_as_metadata() {
        let command = parse("Gm=1,zz=7;AA==").unwrap();
        assert_eq!(command.get("zz"), Some("7"));
        assert!(!command.is_continuation());
    }

    #[test]
    fn placement_controls_this_module_ignores_are_readable() {
        let command = parse("Ga=p,i=1,x=4,y=5,w=6,h=7,c=2,r=3,z=-9,C=1,X=2,Y=3").unwrap();
        assert_eq!(command.u32_value("x").unwrap(), Some(4));
        assert_eq!(command.u32_value("c").unwrap(), Some(2));
        assert_eq!(command.i32_value("z").unwrap(), Some(-9));
        assert_eq!(command.u32_value("nope").unwrap(), None);
        assert_eq!(
            command.u32_value("z"),
            Err(Error::Invalid("unsigned control value"))
        );
        assert_eq!(command.pairs().count(), 12);
    }

    #[test]
    fn actions_parse_without_being_executed() {
        for (value, action) in [
            ("t", Action::Transmit),
            ("T", Action::Display),
            ("q", Action::Query),
            ("p", Action::Placement),
            ("d", Action::Delete),
            ("f", Action::Other('f')),
        ] {
            assert_eq!(parse(&format!("Ga={value},i=1")).unwrap().action, action);
        }
        assert_eq!(
            parse("Ga=tt,i=1"),
            Err(Error::Invalid("a= must be one character"))
        );
        assert!(Action::Transmit.is_transmit() && Action::Display.is_transmit());
        assert!(!Action::Query.is_transmit() && !Action::Other('f').is_transmit());
    }

    #[test]
    fn only_the_three_numeric_formats_are_accepted() {
        assert_eq!(Format::from_control("100").unwrap(), Format::Png);
        assert_eq!(Format::from_control("32").unwrap(), Format::Rgba8);
        assert_eq!(Format::from_control("24").unwrap(), Format::Rgb8);
        // jterm3's non-standard aliases do not survive the hoist.
        for alias in ["png", "jpeg", "jpg", "webp", "rgb", "rgba", "0", ""] {
            assert_eq!(
                Format::from_control(alias),
                Err(Error::NotSupported("format")),
                "{alias}"
            );
        }
    }

    #[test]
    fn transports_are_recorded_and_only_direct_is_supported() {
        for (value, transport) in [
            ("d", Transport::Direct),
            ("f", Transport::File),
            ("t", Transport::TempFile),
            ("s", Transport::SharedMemory),
        ] {
            let payload = format!("Gt={value},i=1,s=1,v=1;AA==");
            let command = parse(&payload).unwrap();
            assert_eq!(command.transport, transport);
            assert_eq!(
                command.require_direct_transport().is_ok(),
                transport == Transport::Direct
            );
        }
        assert_eq!(
            parse("Gt=x,i=1;AA=="),
            Err(Error::NotSupported("transport"))
        );
        assert_eq!(
            parse("Gt=dd,i=1;AA=="),
            Err(Error::Invalid("t= must be one character"))
        );
    }

    #[test]
    fn non_direct_transports_never_reach_the_assembler() {
        let mut assembler = assembler();
        assert_eq!(
            assembler.feed(b"Gf=32,t=f,i=1,s=1,v=1;L3RtcC9pbWFnZQ=="),
            Err(Error::NotSupported("transport"))
        );
        assert!(!assembler.has_pending());
    }

    #[test]
    fn id_and_number_are_mutually_exclusive() {
        assert_eq!(
            parse("Gi=1,I=2,s=1,v=1;AA=="),
            Err(Error::Invalid("i= and I= are mutually exclusive"))
        );
    }

    #[test]
    fn quiet_and_more_reject_out_of_range_values() {
        assert_eq!(parse("Gq=2,i=1").unwrap().quiet, 2);
        assert_eq!(
            parse("Gq=3,i=1"),
            Err(Error::Invalid("q= must be 0, 1 or 2"))
        );
        assert!(parse("Gm=1,q=1;AA==").unwrap().more);
        assert_eq!(parse("Gm=2;AA=="), Err(Error::Invalid("m= must be 0 or 1")));
    }

    #[test]
    fn numeric_controls_reject_garbage() {
        assert_eq!(
            parse("Gs=abc,v=1,i=1;AA=="),
            Err(Error::Invalid("unsigned control value"))
        );
        assert_eq!(
            parse("Gi=-1;AA=="),
            Err(Error::Invalid("unsigned control value"))
        );
    }

    #[test]
    fn non_utf8_sections_are_rejected() {
        assert_eq!(
            parse_command(b"Gi=1,a=\xff;AA==", &Caps::SCREEN),
            Err(Error::Invalid("control data is not UTF-8"))
        );
        assert_eq!(
            parse_command(b"Gi=1;\xff", &Caps::SCREEN),
            Err(Error::Invalid("image data is not UTF-8"))
        );
    }

    #[test]
    fn payloads_that_are_not_graphics_commands_are_rejected() {
        assert!(!is_graphics_payload(b""));
        assert!(!is_graphics_payload(b"X"));
        assert!(is_graphics_payload(b"G"));
        assert_eq!(
            parse("a=t;AA=="),
            Err(Error::Invalid("payload is not a graphics command"))
        );
    }

    // ---- chunk assembly --------------------------------------------------

    #[test]
    fn non_graphics_payloads_are_not_ours() {
        let mut assembler = assembler();
        assert!(matches!(assembler.feed(b"").unwrap(), Step::NotOurs));
        assert!(matches!(assembler.feed(b"X").unwrap(), Step::NotOurs));
        assert!(matches!(
            assembler.feed(b"11;rgb:00/00/00").unwrap(),
            Step::NotOurs
        ));
        assert!(!assembler.has_pending());
    }

    #[test]
    fn chunked_transfer_uses_first_chunk_metadata() {
        let mut assembler = assembler();
        let rgba = [1u8, 2, 3, 4, 5, 6, 7, 8];

        assert!(matches!(
            assembler
                .feed(format!("Gf=32,i=9,s=2,v=1,m=1;{}", encode(&rgba[..3])).as_bytes())
                .unwrap(),
            Step::NeedMore
        ));
        assert!(matches!(
            assembler
                .feed(format!("Gm=1,q=1;{}", encode(&rgba[3..6])).as_bytes())
                .unwrap(),
            Step::NeedMore
        ));
        let assembled = ready(
            assembler
                .feed(format!("Gm=0;{}", encode(&rgba[6..])).as_bytes())
                .unwrap(),
        );

        assert_eq!(assembled.id, Some(9));
        assert_eq!(assembled.declared, Some((2, 1)));
        // A continuation's q= replaces the transfer's quiet level.
        assert_eq!(assembled.quiet, 1);
        assert_eq!(assembled.into_rgba8().unwrap(), (rgba.to_vec(), 2, 1));
        assert!(!assembler.has_pending());
        assert_eq!(assembler.pending_bytes(), 0);
    }

    #[test]
    fn chunked_transfer_accepts_an_empty_final_chunk() {
        let mut assembler = assembler();
        let rgba = [20u8, 30, 40, 255];
        assembler
            .feed(format!("Gf=32,i=10,s=1,v=1,m=1;{}", encode(&rgba)).as_bytes())
            .unwrap();
        let assembled = ready(assembler.feed(b"Gm=0;").unwrap());
        assert_eq!(assembled.bytes, rgba);
    }

    #[test]
    fn transmit_and_display_is_reported_separately() {
        let mut assembler = assembler();
        let display = ready(assembler.feed(b"Ga=T,f=32,s=1,v=1,p=7;AQIDBA==").unwrap());
        assert!(display.display);
        assert_eq!(display.placement, Some(7));
        assert_eq!(display.id, None);

        let quiet = ready(assembler.feed(b"Ga=t,f=32,i=0,s=1,v=1;AQIDBA==").unwrap());
        assert!(!quiet.display);
        // i=0 is anonymous but is preserved verbatim for the responder.
        assert_eq!(quiet.id, Some(0));
    }

    #[test]
    fn continuation_metadata_is_rejected_and_aborts_the_transfer() {
        let mut assembler = assembler();
        assembler.feed(b"Gf=32,i=11,s=1,v=1,m=1;AQID").unwrap();
        assert_eq!(
            assembler.feed(b"Gi=11,m=0;BA=="),
            Err(Error::Invalid(
                "continuation chunks may carry only m= and an optional q="
            ))
        );
        assert!(!assembler.has_pending());
        assert_eq!(assembler.pending_bytes(), 0);
    }

    #[test]
    fn a_continuation_without_a_transfer_is_rejected() {
        let mut assembler = assembler();
        assert_eq!(
            assembler.feed(b"Gm=0;BA=="),
            Err(Error::Invalid(
                "continuation without a transfer in progress"
            ))
        );
        assert_eq!(
            assembler.feed(b"Gm=1;BA=="),
            Err(Error::Invalid(
                "continuation without a transfer in progress"
            ))
        );
    }

    #[test]
    fn named_and_anonymous_uploads_coexist() {
        let mut assembler = assembler();
        // A chunked upload for i=1 is in flight …
        assembler.feed(b"Gf=32,i=1,s=1,v=1,m=1;AQID").unwrap();
        // … an unrelated single-shot upload completes without disturbing it …
        let other = ready(assembler.feed(b"Gf=24,i=2,s=1,v=1;CQgH").unwrap());
        assert_eq!(other.id, Some(2));
        assert!(assembler.has_pending());
        // … and an anonymous chunked upload gets its own slot.
        assembler.feed(b"Gf=32,s=1,v=1,m=1;BAMC").unwrap();
        assert_eq!(assembler.in_flight.len(), 1);
        assert!(assembler.anonymous.is_some());

        // The id-less continuation lands in the most recent chunked transfer.
        let anonymous = ready(assembler.feed(b"Gm=0;AQ==").unwrap());
        assert_eq!(anonymous.id, None);
        assert_eq!(anonymous.bytes, [4, 3, 2, 1]);
        assert!(assembler.anonymous.is_none());
        assert_eq!(assembler.in_flight.len(), 1);

        assembler.reset();
        assert!(!assembler.has_pending());
        assert_eq!(assembler.pending_bytes(), 0);
    }

    #[test]
    fn a_second_first_chunk_for_the_same_image_aborts_it() {
        let mut assembler = assembler();
        assembler.feed(b"Gf=32,i=1,s=1,v=1,m=1;AQID").unwrap();
        assert!(assembler.feed(b"Gf=32,i=1,s=1,v=1,m=1;AQID").is_err());
        assert!(!assembler.has_pending());
        // The abort clears the slot, so an honest retry succeeds.
        assembler.feed(b"Gf=32,i=1,s=1,v=1,m=1;AQID").unwrap();
        assert_eq!(ready(assembler.feed(b"Gm=0;BA==").unwrap()).id, Some(1));
    }

    #[test]
    fn a_non_transmit_action_aborts_an_in_flight_transfer() {
        let mut assembler = assembler();
        assembler.feed(b"Gf=32,i=23,s=1,v=1,m=1;AQID").unwrap();
        let step = assembler.feed(b"Ga=d,d=I,i=22").unwrap();
        match step {
            Step::Other {
                command,
                interrupted,
            } => {
                assert_eq!(command.action, Action::Delete);
                assert!(interrupted);
            }
            other => panic!("expected Other, got {other:?}"),
        }
        assert!(!assembler.has_pending());

        // With nothing in flight the same command interrupts nothing.
        match assembler.feed(b"Ga=q,i=31,f=24,s=1,v=1;AAAA").unwrap() {
            Step::Other {
                command,
                interrupted,
            } => {
                assert_eq!(command.action, Action::Query);
                assert!(!interrupted);
            }
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[test]
    fn a_malformed_command_aborts_the_transfer_it_could_have_continued() {
        let mut assembler = assembler();
        assembler.feed(b"Gf=32,i=12,s=1,v=1,m=1;AQID").unwrap();
        assert!(assembler.feed(b"Gm=0,bad;BA==").is_err());
        assert!(!assembler.has_pending());
    }

    #[test]
    fn invalid_base64_in_the_final_chunk_aborts_the_transfer() {
        let mut assembler = assembler();
        assembler.feed(b"Gf=32,i=12,s=1,v=1,m=1;AQID").unwrap();
        assert_eq!(
            assembler.feed(b"Gm=0;%%%="),
            Err(Error::Invalid("base64 alphabet"))
        );
        assert!(!assembler.has_pending());
    }

    #[test]
    fn embedded_whitespace_is_stripped_across_chunks() {
        let mut assembler = assembler();
        assembler.feed(b"Gf=32,i=14,s=1,v=1,m=1;AQ\r\n").unwrap();
        let assembled = ready(assembler.feed(b"Gm=0; ID  BA== \n").unwrap());
        assert_eq!(assembled.bytes, [1, 2, 3, 4]);
    }

    // ---- caps ------------------------------------------------------------

    #[test]
    fn the_two_presets_keep_the_pre_hoist_budgets() {
        assert_eq!(
            Caps::BLOCK,
            Caps {
                max_encoded_bytes: 16 * 1024 * 1024,
                max_decoded_bytes: 16 * 1024 * 1024,
                max_dimension: 16_384,
                max_control_bytes: 16 * 1024,
                max_pending_bytes: 16 * 1024 * 1024,
            }
        );
        assert_eq!(
            Caps::SCREEN,
            Caps {
                max_encoded_bytes: 64 * 1024 * 1024,
                max_decoded_bytes: 64 * 1024 * 1024,
                max_dimension: 16_384,
                max_control_bytes: 16 * 1024,
                max_pending_bytes: 64 * 1024 * 1024,
            }
        );
        assert_eq!(MAX_CONTROL_BYTES, 16 * 1024);
        assert_eq!(assembler().caps(), &Caps::SCREEN);
    }

    #[test]
    fn the_encoded_cap_holds_at_the_boundary_and_one_past_it() {
        let caps = Caps {
            max_encoded_bytes: 8,
            ..Caps::SCREEN
        };
        let mut assembler = Assembler::new(caps);
        // 8 encoded bytes is exactly the cap.
        assembler.feed(b"Gf=32,i=1,s=1,v=1,m=1;AQID").unwrap();
        assert!(matches!(
            assembler.feed(b"Gm=1;BAUG").unwrap(),
            Step::NeedMore
        ));
        // The ninth aborts the transfer.
        assert_eq!(assembler.feed(b"Gm=0;BwgJ"), Err(Error::TooLarge));
        assert!(!assembler.has_pending());
    }

    #[test]
    fn the_decoded_cap_holds_at_the_boundary_and_one_past_it() {
        assert_eq!(decode_base64(b"AQID", 3).unwrap(), [1, 2, 3]);
        assert_eq!(decode_base64(b"AQID", 2), Err(Error::TooLarge));
        // Nothing is allocated first: jterm2's zero-budget regression.
        assert_eq!(decode_base64(b"AAAA", 0), Err(Error::TooLarge));
        assert_eq!(decode_base64(b"", 0).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn the_dimension_cap_holds_at_the_boundary_and_one_past_it() {
        let caps = Caps {
            max_dimension: 16,
            max_decoded_bytes: usize::MAX,
            ..Caps::SCREEN
        };
        assert_eq!(
            raw_layout(16, 16, Format::Rgba8, &caps).unwrap().pixels,
            256
        );
        assert_eq!(
            raw_layout(17, 16, Format::Rgba8, &caps),
            Err(Error::TooLarge)
        );
        assert_eq!(
            raw_layout(16, 17, Format::Rgba8, &caps),
            Err(Error::TooLarge)
        );
    }

    #[test]
    fn the_decoded_byte_budget_bounds_the_geometry() {
        let caps = Caps {
            max_decoded_bytes: 16 * 1024 * 1024,
            ..Caps::BLOCK
        };
        let at_limit = raw_layout(2048, 2048, Format::Rgba8, &caps).unwrap();
        assert_eq!(at_limit.rgba_bytes, caps.max_decoded_bytes);
        assert_eq!(at_limit.rgba_stride, 2048 * 4);
        assert_eq!(
            raw_layout(2049, 2048, Format::Rgba8, &caps),
            Err(Error::TooLarge)
        );
        // f=24 is budgeted on the RGBA form it expands into.
        assert_eq!(
            raw_layout(2049, 2048, Format::Rgb8, &caps),
            Err(Error::TooLarge)
        );
    }

    #[test]
    fn the_pending_cap_bounds_every_transfer_together() {
        let caps = Caps {
            max_pending_bytes: 8,
            ..Caps::SCREEN
        };
        let mut assembler = Assembler::new(caps);
        assembler.feed(b"Gf=32,i=1,s=1,v=1,m=1;AQID").unwrap();
        assert_eq!(assembler.pending_bytes(), 4);
        assembler.feed(b"Gf=32,i=2,s=1,v=1,m=1;AQID").unwrap();
        assert_eq!(assembler.pending_bytes(), 8);
        // A third slot would push the total past the aggregate budget, even
        // though no single transfer is near the per-image cap.
        assert_eq!(
            assembler.feed(b"Gf=32,i=3,s=1,v=1,m=1;AQID"),
            Err(Error::TooLarge)
        );
        assert_eq!(assembler.pending_bytes(), 8);
        assert_eq!(assembler.in_flight.len(), 2);
    }

    #[test]
    fn empty_chunked_uploads_cannot_grow_the_slot_map_without_bound() {
        let mut assembler = assembler();
        for id in 1..=MAX_PENDING_TRANSFERS {
            let command = format!("Gi={id},m=1;");
            assert_eq!(assembler.feed(command.as_bytes()).unwrap(), Step::NeedMore);
        }
        assert_eq!(assembler.pending_bytes(), 0);
        assert_eq!(assembler.in_flight.len(), MAX_PENDING_TRANSFERS);

        let command = format!("Gi={},m=1;", MAX_PENDING_TRANSFERS + 1);
        assert_eq!(assembler.feed(command.as_bytes()), Err(Error::TooLarge));
        assert_eq!(assembler.in_flight.len(), MAX_PENDING_TRANSFERS);
    }

    #[test]
    fn dimension_arithmetic_cannot_overflow() {
        let caps = Caps {
            max_dimension: u32::MAX,
            max_decoded_bytes: usize::MAX,
            ..Caps::SCREEN
        };
        // width * height fits in usize but the RGBA expansion does not.
        assert_eq!(
            raw_layout(u32::MAX, u32::MAX, Format::Rgba8, &caps),
            Err(Error::TooLarge)
        );
        assert_eq!(
            raw_layout(u32::MAX, u32::MAX, Format::Rgb8, &caps),
            Err(Error::TooLarge)
        );
        // The default caps reject the same geometry on the dimension check.
        assert_eq!(
            raw_layout(u32::MAX, 1, Format::Rgba8, &Caps::SCREEN),
            Err(Error::TooLarge)
        );
    }

    // ---- raw formats -----------------------------------------------------

    #[test]
    fn raw_formats_require_an_exact_length_in_both_directions() {
        for encoded in ["AQID", "AQIDBAU="] {
            let assembled = ready(
                assembler()
                    .feed(format!("Gf=32,i=13,s=1,v=1;{encoded}").as_bytes())
                    .unwrap(),
            );
            assert_eq!(
                assembled.into_rgba8(),
                Err(Error::Invalid("raw image length does not match s= and v="))
            );
        }
        let exact = ready(assembler().feed(b"Gf=32,i=13,s=1,v=1;AQIDBA==").unwrap());
        assert_eq!(exact.into_rgba8().unwrap(), (vec![1, 2, 3, 4], 1, 1));
    }

    #[test]
    fn rgb_is_expanded_to_rgba_with_opaque_alpha() {
        let rgb = [10u8, 20, 30, 40, 50, 60];
        let assembled = ready(
            assembler()
                .feed(format!("Gf=24,i=6,s=2,v=1;{}", encode(&rgb)).as_bytes())
                .unwrap(),
        );
        assert_eq!(
            assembled.into_rgba8().unwrap(),
            (vec![10, 20, 30, 255, 40, 50, 60, 255], 2, 1)
        );
    }

    #[test]
    fn zero_and_missing_dimensions_are_rejected_before_decoding() {
        let mut assembler = assembler();
        assert_eq!(
            assembler.feed(b"Gf=32,i=3,s=0,v=1;AQIDBA=="),
            Err(Error::Invalid("image dimensions must be non-zero"))
        );
        assert_eq!(
            assembler.feed(b"Gf=32,i=3,s=1,v=0;AQIDBA=="),
            Err(Error::Invalid("image dimensions must be non-zero"))
        );
        assert_eq!(
            assembler.feed(b"Gf=32,i=3;AQIDBA=="),
            Err(Error::Invalid("raw formats require s= and v="))
        );
        assert_eq!(
            assembler.feed(b"Gf=24,i=3,s=1;AQIDBA=="),
            Err(Error::Invalid("raw formats require s= and v="))
        );
    }

    #[test]
    fn an_oversized_raw_image_is_rejected_before_any_allocation() {
        let payload = format!("Gf=32,i=1,s={0},v={0};AAAA", Caps::SCREEN.max_dimension);
        assert_eq!(assembler().feed(payload.as_bytes()), Err(Error::TooLarge));

        let payload = format!("Gf=24,i=1,s={},v=1;AAAA", Caps::SCREEN.max_dimension + 1);
        assert_eq!(assembler().feed(payload.as_bytes()), Err(Error::TooLarge));
    }

    #[test]
    fn into_rgba8_refuses_to_stand_in_for_a_png_decoder() {
        let assembled = ready(
            assembler()
                .feed(format!("Gf=100,i=7;{}", encode(&png_header(1, 1))).as_bytes())
                .unwrap(),
        );
        assert_eq!(
            assembled.into_rgba8(),
            Err(Error::Invalid("into_rgba8 requires f=24 or f=32"))
        );
    }

    // ---- PNG sniff -------------------------------------------------------

    #[test]
    fn a_valid_ihdr_yields_the_declared_dimensions() {
        assert_eq!(
            png_dimensions(&png_header(640, 480), &Caps::SCREEN).unwrap(),
            (640, 480)
        );
        let assembled = ready(
            assembler()
                .feed(format!("Gf=100,i=7;{}", encode(&png_header(3, 4))).as_bytes())
                .unwrap(),
        );
        assert_eq!(assembled.declared, Some((3, 4)));
        assert_eq!(assembled.png_dimensions().unwrap(), (3, 4));
    }

    #[test]
    fn a_truncated_or_mistyped_png_header_is_rejected() {
        let header = png_header(1, 1);
        assert_eq!(
            png_dimensions(&header[..23], &Caps::SCREEN),
            Err(Error::Invalid("PNG header is truncated"))
        );
        assert_eq!(
            png_dimensions(b"", &Caps::SCREEN),
            Err(Error::Invalid("PNG header is truncated"))
        );

        let mut wrong_signature = header.clone();
        wrong_signature[1] = b'X';
        assert_eq!(
            png_dimensions(&wrong_signature, &Caps::SCREEN),
            Err(Error::Invalid("payload is not a PNG"))
        );
        assert_eq!(
            png_dimensions(b"not a PNG at all, not even close", &Caps::SCREEN),
            Err(Error::Invalid("payload is not a PNG"))
        );

        let mut wrong_chunk = header.clone();
        wrong_chunk[12..16].copy_from_slice(b"IDAT");
        assert_eq!(
            png_dimensions(&wrong_chunk, &Caps::SCREEN),
            Err(Error::Invalid("PNG does not start with an IHDR chunk"))
        );

        let mut wrong_length = header;
        wrong_length[8..12].copy_from_slice(&12u32.to_be_bytes());
        assert_eq!(
            png_dimensions(&wrong_length, &Caps::SCREEN),
            Err(Error::Invalid("PNG does not start with an IHDR chunk"))
        );
    }

    #[test]
    fn an_oversize_ihdr_is_rejected_before_a_decoder_sees_it() {
        let oversize = png_header(Caps::BLOCK.max_dimension + 1, 1);
        assert_eq!(
            png_dimensions(&oversize, &Caps::BLOCK),
            Err(Error::TooLarge)
        );
        let payload = format!("Gf=100,i=1;{}", encode(&oversize));
        assert_eq!(
            Assembler::new(Caps::BLOCK).feed(payload.as_bytes()),
            Err(Error::TooLarge)
        );

        let zero = png_header(0, 8);
        assert_eq!(
            png_dimensions(&zero, &Caps::SCREEN),
            Err(Error::Invalid("image dimensions must be non-zero"))
        );
    }

    #[test]
    fn f100_that_is_not_a_png_is_rejected_by_the_assembler() {
        let payload = format!("Gf=100,i=8;{}", encode(b"not a PNG"));
        assert_eq!(
            assembler().feed(payload.as_bytes()),
            Err(Error::Invalid("PNG header is truncated"))
        );
    }

    // ---- base64 ----------------------------------------------------------

    #[test]
    fn base64_round_trips_every_remainder() {
        let cases: &[(&[u8], &[u8])] = &[
            (b"", b""),
            (b"f", b"Zg=="),
            (b"fo", b"Zm8="),
            (b"foo", b"Zm9v"),
            (b"foob", b"Zm9vYg=="),
            (b"hello world", b"aGVsbG8gd29ybGQ="),
        ];
        for (plain, encoded) in cases {
            assert_eq!(&decode_base64(encoded, usize::MAX).unwrap(), plain);
            assert_eq!(encode(plain).as_bytes(), *encoded);
        }
        assert_eq!(
            decode_base64(b"+/+/", usize::MAX).unwrap(),
            [0xFB, 0xFF, 0xBF]
        );
    }

    #[test]
    fn base64_tolerates_whitespace_and_sloppy_padding() {
        assert_eq!(decode_base64(b" Zm9v\r\n", usize::MAX).unwrap(), b"foo");
        // Missing padding …
        assert_eq!(decode_base64(b"Zg", usize::MAX).unwrap(), b"f");
        assert_eq!(decode_base64(b"Zm8", usize::MAX).unwrap(), b"fo");
        // … and too much of it.
        assert_eq!(decode_base64(b"Zg======", usize::MAX).unwrap(), b"f");
        assert_eq!(decode_base64(b"Zm9v=", usize::MAX).unwrap(), b"foo");
    }

    #[test]
    fn base64_rejects_garbage_interior_padding_and_impossible_lengths() {
        assert_eq!(
            decode_base64(b"!!!!", usize::MAX),
            Err(Error::Invalid("base64 alphabet"))
        );
        assert_eq!(
            decode_base64(b"%%%bad%%%", usize::MAX),
            Err(Error::Invalid("base64 alphabet"))
        );
        assert_eq!(
            decode_base64(b"Zm=9v", usize::MAX),
            Err(Error::Invalid("base64 padding is not at the end"))
        );
        assert_eq!(
            decode_base64(b"Zm9vZ", usize::MAX),
            Err(Error::Invalid("base64 length"))
        );
    }

    #[test]
    fn errors_describe_themselves() {
        assert_eq!(
            Error::Invalid("base64 length").to_string(),
            "invalid kitty graphics command: base64 length"
        );
        assert_eq!(
            Error::NotSupported("transport").to_string(),
            "unsupported kitty graphics transport"
        );
        assert_eq!(
            Error::TooLarge.to_string(),
            "kitty graphics command exceeds the configured limits"
        );
    }
}
