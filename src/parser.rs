//! OSC/CSI stream parser. Splits a raw PTY byte stream into semantic
//! `ParserEvent`s — passing through display bytes while extracting the OSC 133
//! shell-integration marks, OSC 52 clipboard, alt-screen toggles and APC
//! sequences that drive the block view. OSC 7/title sequences pass through to
//! VTE so its native cwd/title signals stay authoritative.

/// OSC carries titles and clipboard data. One MiB is ample for supported
/// payloads while bounding malformed strings that never send a terminator.
const MAX_OSC_PAYLOAD_BYTES: usize = 1024 * 1024;

/// Per-field cap for app-driven desktop notifications (OSC 9 / OSC 777),
/// matching the frost in-engine limit so the family behaves identically.
pub const MAX_NOTIFICATION_CHARS: usize = 256;
/// Kitty graphics uses APC. Keep a practical encoded-image ceiling while
/// preventing one unterminated sequence from retaining arbitrary PTY output.
const MAX_APC_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;
/// Sixel and terminal-query DCS payloads are passed through only when complete.
/// Oversized unterminated payloads are discarded until their real terminator.
const MAX_DCS_PAYLOAD_BYTES: usize = 1024 * 1024;
const MAX_CLIPBOARD_BASE64_BYTES: usize = 4 * 1024 * 1024;
const MAX_OSC7_URI_BYTES: usize = 16 * 1024;
const MAX_TITLE_BYTES: usize = 4 * 1024;
/// Per-field caps for the OSC 133 metadata jsh attaches to its C/D packets.
/// jsh bounds these on the way out (`MAX_OSC_COMMAND_BYTES` = 16 KiB,
/// `MAX_OSC_CWD_BYTES` = 4 KiB in jsh/src/osc.rs), but any program can write an
/// OSC 133 packet, so the reader bounds them too rather than trusting a
/// cooperative producer.
const MAX_OSC133_COMMAND_BYTES: usize = 16 * 1024;
const MAX_OSC133_CWD_BYTES: usize = 4 * 1024;
const MAX_OSC133_ID_BYTES: usize = 1024;

/// Which color slot an OSC 10/11/12/4 query asked about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorKind {
    /// OSC 10 — default foreground.
    Foreground,
    /// OSC 11 — default background.
    Background,
    /// OSC 12 — cursor color.
    Cursor,
    /// OSC 4;N — palette index N.
    Palette(u8),
}

pub use crate::kitty_keyboard::KittyKeyboardOp;

/// Which terminal-capability handshake an app sent. The active VTE in block view
/// has no real PTY return path, so we synthesize a sensible "not supported"
/// reply ourselves to keep neovim/helix from blocking on a missing response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyboardProtocolQuery {
    /// `CSI ? u` — kitty progressive-enhancement flag query.
    KittyQuery,
    /// `CSI ? 4 m` — XTQMODKEYS modifyOtherKeys query.
    ModifyOtherKeysQuery,
    /// `CSI c` / `CSI 0 c` — primary device attributes (DA1).
    PrimaryDeviceAttributes,
    /// `CSI > c` / `CSI > 0 c` — secondary device attributes (DA2). Different
    /// reply format from DA1 (`CSI > Pp ; Pv ; Pc c` vs `CSI ? ... c`).
    SecondaryDeviceAttributes,
    /// `CSI = c` / `CSI = 0 c` — tertiary device attributes (DA3).
    TertiaryDeviceAttributes,
    /// `CSI > q` — XTVERSION (xterm name/version request).
    XtVersion,
    /// `CSI 5 n` — DSR: report device status (reply `\e[0n` = OK).
    DeviceStatus,
    /// `CSI 6 n` — DSR: report cursor position (reply `\e[<row>;<col>R`).
    CursorPosition,
}

/// Events emitted by the stream parser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParserEvent {
    /// Raw bytes that should be displayed verbatim (ANSI codes stripped of OSC 133/7).
    Bytes(Vec<u8>),
    /// OSC 133 ;A — prompt about to render.
    PromptStart,
    /// OSC 133 ;B — prompt finished, waiting for user input.
    PromptEnd,
    /// OSC 133 ;C — user pressed Enter, command is executing. Carries whatever
    /// [`CommandMeta`] the shell attached; every field is absent for a shell
    /// that emits only the bare FinalTerm mark.
    CommandStart(CommandMeta),
    /// OSC 133 ;D — command finished.
    ///
    /// `exit` is `None` when the shell sent no status or an unparseable one.
    /// That case used to collapse to `Some(0)`, i.e. a command of unknown
    /// outcome was reported as having succeeded.
    CommandEnd {
        exit: Option<i32>,
        meta: CommandMeta,
    },
    /// OSC 7770 — jsh-specific: the remote shell announces its session ID at
    /// startup. The UI stores it on the tab's RemoteConn so subsequent
    /// reconnects pass `--session <id>` and jsh restores cwd/env/aliases.
    RemoteSessionId(String),
    /// OSC 7771 — the bundled local shell integration consumed its private
    /// one-shot token fd and is ready to correlate Agent executions.
    AgentIntegrationReady(String),
    /// CSI ? 47/1047/1049 h — alt screen entered (vim, less, etc.).
    /// Carries the exact DEC private mode so VTE receives matching semantics.
    AltScreenEnter(u32),
    /// CSI ? 47/1047/1049 l — alt screen left.
    AltScreenLeave(u32),
    /// OSC 52 — application set clipboard content.
    ClipboardSet(String),
    /// OSC 52 with `?` — app is asking for current clipboard content.
    /// We reply with an empty payload (`\e]52;c;\e\\`) so probers (tmux/vim)
    /// know we accept SET but don't expose clipboard contents to the shell.
    ClipboardQuery,
    /// APC sequence (ESC _) — Kitty graphics protocol or similar.
    ApcSequence(Vec<u8>),
    /// CSI private-mode set/reset — DEC private mode change. Emitted in addition to
    /// pass-through so block_view can track reporting modes.
    DecsetMode { mode: u32, set: bool },
    /// OSC 10/11/12/4 with a `?` — app is asking the terminal what color it uses.
    /// The caller must write a `\e]<n>;rgb:RRRR/GGGG/BBBB\e\\` reply to the PTY.
    ColorQuery(ColorKind),
    /// App queried a keyboard/capability protocol. Caller should reply on the PTY
    /// with a canned "not supported" / level-0 response so the app falls back
    /// gracefully (otherwise neovim, helix, etc. hang waiting on the reply).
    KeyboardProtocolQuery(KeyboardProtocolQuery),
    /// `OSC 9 ; <body>` (iTerm2/ConEmu) or
    /// `OSC 777 ; notify ; <title> ; <body>`.
    /// (urxvt) — the application requests a desktop notification. Fields are
    /// bounded to [`MAX_NOTIFICATION_CHARS`] with control and visual-formatting
    /// characters replaced; the UI must still rate-limit before showing it.
    Notification { title: Option<String>, body: String },
    /// OSC 10/11/12 with a color value — the app SET a dynamic color (theme
    /// switching tools, vim `background=`). The raw spec is forwarded verbatim
    /// (apps parse `#RRGGBB`, `rgb:RR/GG/BB`, or X11 names with their toolkit)
    /// and the original bytes still pass through to the live view; the caller
    /// tracks the dynamic value so later [`ParserEvent::ColorQuery`] replies
    /// report it instead of the static theme color.
    ColorSet { kind: ColorKind, spec: String },
    /// OSC 110/111/112 — the app reset a dynamic color back to the default.
    /// Bytes also pass through; the caller drops its tracked dynamic value.
    ColorReset(ColorKind),
    /// `CSI 3 J` / `CSI 03 J` — xterm erase-scrollback. Emitted immediately
    /// before the complete sequence is passed through as [`Self::Bytes`], so
    /// callers can invalidate row authority using the pre-feed ring mapping.
    EraseScrollback,
    /// RIS (`ESC c`) — hard terminal reset. Like [`Self::EraseScrollback`],
    /// this is a byte-coalescing barrier emitted before the raw sequence.
    HardReset,
    /// Kitty keyboard protocol flag-stack operation — `CSI > flags u` (push),
    /// `CSI < n u` (pop), `CSI = flags ; mode u` (set). The bytes still pass
    /// through; the caller owns the stacks (see [`crate::kitty_keyboard`])
    /// and must answer the matching `CSI ? u` query from them.
    KittyKeyboard(KittyKeyboardOp),
}

/// Longest accepted OSC 10/11/12 color spec. X11 names and rgb: forms are
/// short; anything longer is malformed and only passes through.
pub const MAX_COLOR_SPEC_CHARS: usize = 128;

#[derive(Default)]
enum State {
    #[default]
    Ground,
    /// Saw ESC, waiting for next byte
    Esc,
    /// Inside CSI (ESC [): collecting parameter/intermediary bytes
    Csi { buf: Vec<u8> },
    /// Inside OSC (ESC ]): collecting bytes until ST (BEL or ESC \)
    Osc { buf: Vec<u8> },
    /// Just saw ESC while in OSC — next byte should be '\' for ST
    OscEsc { payload: Vec<u8> },
    /// OSC exceeded its hard payload limit. Ignore bytes until BEL or ST.
    OscDiscard,
    /// Saw ESC while discarding an oversized OSC; '\' completes ST.
    OscDiscardEsc,
    /// Inside APC (ESC _): collecting bytes for Kitty graphics etc. APC is a
    /// control string and is terminated only by ST, never by BEL.
    Apc { buf: Vec<u8> },
    /// Saw ESC while in APC — next byte should be '\' for ST
    ApcEsc { payload: Vec<u8> },
    /// APC exceeded its hard payload limit. Ignore bytes until ST.
    ApcDiscard,
    /// Saw ESC while discarding an oversized APC; '\' completes ST.
    ApcDiscardEsc,
    /// Inside DCS (ESC P): collect until ST. Unlike `Ignore`, the bytes are
    /// rewrapped as `ESC P ... ESC \` and passed through to the active VTE so
    /// sixel graphics, DECRQSS replies, and tmux passthrough survive block mode.
    Dcs { buf: Vec<u8> },
    /// Saw ESC while in DCS — next byte should be '\' for ST.
    DcsEsc { payload: Vec<u8> },
    /// DCS exceeded its hard payload limit. Ignore bytes until ST.
    DcsDiscard,
    /// Saw ESC while discarding an oversized DCS; '\' completes ST.
    DcsDiscardEsc,
    /// Inside PM (ESC ^) or SOS (ESC X) — consume until ST and discard.
    Ignore,
    /// Saw ESC while in PM/SOS — consume the ST final byte too.
    IgnoreEsc,
}

/// Which mouse-tracking mode the shell asked for. The active VTE in block-view
/// has no real PTY, so VTE never auto-generates mouse reports; the caller drives
/// reporting itself by reading this state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum MouseMode {
    #[default]
    None,
    /// `?9` — only button presses (no release).
    X10,
    /// `?1000` — button press + release.
    Normal,
    /// `?1002` — press/release + motion while a button is held.
    ButtonEvent,
    /// `?1003` — press/release + all motion.
    AnyEvent,
}

/// Wire format for mouse reports. Set by `?1006`, `?1015`, `?1005` (or default
/// xterm encoding if none enabled).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum MouseEncoding {
    /// Legacy `\e[M` + 3 bytes (button + 32, col + 32, row + 32).
    #[default]
    Default,
    /// `?1006` — SGR: `\e[<b;col;row;{M|m}`.
    Sgr,
    /// `?1015` — urxvt: `\e[b;col;row;M`.
    Urxvt,
    /// `?1005` — UTF-8 encoded coordinates.
    Utf8,
}

pub struct Parser {
    state: State,
    passthrough: Vec<u8>,
    config: ParserConfig,
    /// `?2004` — shell asked for paste content to be bracketed with `\e[200~`
    /// / `\e[201~`. The caller wraps its own `Paste` write when this is on.
    bracketed_paste: bool,
    /// Which mouse mode is currently active (highest-priority "h" wins).
    mouse_mode: MouseMode,
    /// Active mouse encoding flags. SGR/Urxvt/Utf8 are toggled independently; a
    /// later "h" replaces the encoding choice.
    mouse_encoding: MouseEncoding,
    /// `?1004` — shell asked for `\e[I` / `\e[O` on focus enter/leave.
    focus_events: bool,
}

#[derive(Clone, Copy)]
pub struct ParserConfig {
    pub mouse_reporting: bool,
    pub focus_reporting: bool,
}

impl Default for ParserConfig {
    fn default() -> Self {
        Self {
            mouse_reporting: true,
            focus_reporting: true,
        }
    }
}

fn alt_screen_mode(params: &[u8]) -> Option<u32> {
    match params {
        b"?47" => Some(47),
        b"?1047" => Some(1047),
        b"?1049" => Some(1049),
        _ => None,
    }
}

/// True only for the ordinary (non-private, no-intermediate) ED parameter
/// whose numeric value is 3. Extra parameter fields are deliberately rejected:
/// the lifecycle side effect must not fire for a malformed/lookalike CSI that
/// VTE may interpret differently.
fn is_erase_scrollback(params: &[u8]) -> bool {
    !params.is_empty()
        && params.iter().all(u8::is_ascii_digit)
        && params.iter().fold(0_u32, |value, digit| {
            value
                .saturating_mul(10)
                .saturating_add(u32::from(*digit - b'0'))
        }) == 3
}

fn is_mouse_reporting_mode(params: &[u8]) -> bool {
    matches!(
        params,
        b"?9"
            | b"?1000"
            | b"?1001"
            | b"?1002"
            | b"?1003"
            | b"?1005"
            | b"?1006"
            | b"?1015"
            | b"?1016"
    )
}

fn is_focus_reporting_mode(params: &[u8]) -> bool {
    matches!(params, b"?1004")
}

/// A small decimal CSI parameter. Non-digits end the number; an absent one is
/// zero, and the value saturates so a hostile width cannot wrap.
fn csi_u8_param(digits: &[u8]) -> u8 {
    digits
        .iter()
        .take_while(|c| c.is_ascii_digit())
        .fold(0_u8, |value, digit| {
            value.saturating_mul(10).saturating_add(digit - b'0')
        })
}

impl Default for Parser {
    fn default() -> Self {
        Self::new()
    }
}

impl Parser {
    pub fn new() -> Self {
        Self::with_config(ParserConfig::default())
    }

    pub fn with_config(config: ParserConfig) -> Self {
        Parser {
            state: State::default(),
            passthrough: Vec::with_capacity(4096),
            config,
            bracketed_paste: false,
            mouse_mode: MouseMode::None,
            mouse_encoding: MouseEncoding::Default,
            focus_events: false,
        }
    }

    /// True while the shell has `?2004` enabled — callers should wrap pasted
    /// content with `\e[200~` / `\e[201~` before writing to the PTY.
    #[allow(dead_code)]
    pub fn bracketed_paste(&self) -> bool {
        self.bracketed_paste
    }

    /// Currently active mouse-tracking mode, or `None` when reporting is off.
    #[allow(dead_code)]
    pub fn mouse_mode(&self) -> MouseMode {
        self.mouse_mode
    }

    /// Wire encoding the next mouse report should use.
    #[allow(dead_code)]
    pub fn mouse_encoding(&self) -> MouseEncoding {
        self.mouse_encoding
    }

    /// True while `?1004` is enabled — callers should emit `\e[I` on focus-in,
    /// `\e[O` on focus-out.
    #[allow(dead_code)]
    pub fn focus_events(&self) -> bool {
        self.focus_events
    }

    /// Apply each `?N` token from a `CSI ? Pm h/l` to the snooped state.
    /// `enable` = true for `h`, false for `l`. Unknown modes are ignored —
    /// they still pass through to the VTE.
    fn update_dec_private_modes(&mut self, params: &[u8], enable: bool) -> Vec<u32> {
        let mut modes = Vec::new();
        for token in params.split(|&c| c == b';') {
            // Each token may itself start with `?` if the shell sent
            // `CSI ?1;?2 h`; tolerate that.
            let token = token.strip_prefix(b"?").unwrap_or(token);
            let n: u32 = match std::str::from_utf8(token).ok().and_then(|s| s.parse().ok()) {
                Some(n) => n,
                None => continue,
            };
            modes.push(n);
            match n {
                2004 => self.bracketed_paste = enable,
                9 => {
                    self.mouse_mode = if enable {
                        MouseMode::X10
                    } else {
                        MouseMode::None
                    }
                }
                1000 => {
                    self.mouse_mode = if enable {
                        MouseMode::Normal
                    } else {
                        MouseMode::None
                    }
                }
                1002 => {
                    self.mouse_mode = if enable {
                        MouseMode::ButtonEvent
                    } else {
                        MouseMode::None
                    }
                }
                1003 => {
                    self.mouse_mode = if enable {
                        MouseMode::AnyEvent
                    } else {
                        MouseMode::None
                    }
                }
                1004 => self.focus_events = enable,
                1005 => {
                    self.mouse_encoding = if enable {
                        MouseEncoding::Utf8
                    } else {
                        MouseEncoding::Default
                    }
                }
                1006 => {
                    self.mouse_encoding = if enable {
                        MouseEncoding::Sgr
                    } else {
                        MouseEncoding::Default
                    }
                }
                1015 => {
                    self.mouse_encoding = if enable {
                        MouseEncoding::Urxvt
                    } else {
                        MouseEncoding::Default
                    }
                }
                _ => {}
            }
        }
        modes
    }

    pub fn feed(&mut self, data: &[u8], events: &mut Vec<ParserEvent>) {
        self.passthrough.clear();

        macro_rules! flush {
            () => {
                if !self.passthrough.is_empty() {
                    events.push(ParserEvent::Bytes(std::mem::take(&mut self.passthrough)));
                }
            };
        }

        macro_rules! hard_reset {
            () => {{
                flush!();
                self.bracketed_paste = false;
                self.mouse_mode = MouseMode::None;
                self.mouse_encoding = MouseEncoding::Default;
                self.focus_events = false;
                events.push(ParserEvent::HardReset);
                // RIS itself is an immediate feed boundary. Keeping it in a
                // dedicated Bytes event prevents a later query/DECSET in the
                // same PTY read from being coalesced across the reset hook.
                events.push(ParserEvent::Bytes(b"\x1bc".to_vec()));
                self.state = State::Ground;
            }};
        }

        /// Abort an incomplete control string at its non-ST ESC sequence and
        /// reinterpret the escape introducer plus final byte from scratch.
        /// Keeping this in one local helper avoids subtly different recovery
        /// rules between OSC, APC, DCS, and their oversized discard states.
        macro_rules! reprocess_escape_final {
            ($byte:expr) => {{
                match $byte {
                    0x1b => self.state = State::Esc,
                    b'[' => self.state = State::Csi { buf: Vec::new() },
                    b']' => self.state = State::Osc { buf: Vec::new() },
                    b'_' => self.state = State::Apc { buf: Vec::new() },
                    b'P' => self.state = State::Dcs { buf: Vec::new() },
                    b'^' | b'X' => self.state = State::Ignore,
                    b'c' => hard_reset!(),
                    byte => {
                        self.passthrough.push(0x1b);
                        self.passthrough.push(byte);
                        self.state = State::Ground;
                    }
                }
            }};
        }

        // Ground-state fast-path: bulk-copy runs of bytes until the next ESC.
        // The previous per-byte loop dominated cost on heavy text streams; ESC
        // is the only byte that exits Ground, so memchr lets us hop directly
        // to the next state transition.
        let mut i = 0usize;
        let len = data.len();
        while i < len {
            if matches!(self.state, State::Ground) {
                match memchr::memchr(0x1b, &data[i..]) {
                    Some(off) => {
                        if off > 0 {
                            self.passthrough.extend_from_slice(&data[i..i + off]);
                        }
                        i += off + 1;
                        self.state = State::Esc;
                        continue;
                    }
                    None => {
                        self.passthrough.extend_from_slice(&data[i..]);
                        break;
                    }
                }
            }

            let b = data[i];
            i += 1;
            match &mut self.state {
                State::Ground => unreachable!("handled by fast-path above"),

                State::Esc => match b {
                    b'[' => {
                        // Do NOT emit "ESC[" yet. Buffer the whole CSI in state so a
                        // read boundary falling mid-sequence cannot split it across
                        // two Bytes events — downstream scanners (interactive-mode
                        // detection) rely on seeing each CSI whole.
                        self.state = State::Csi { buf: Vec::new() };
                    }
                    b']' => {
                        self.state = State::Osc { buf: Vec::new() };
                    }
                    b'_' => {
                        self.state = State::Apc { buf: Vec::new() };
                    }
                    b'P' => {
                        self.state = State::Dcs { buf: Vec::new() };
                    }
                    // PM (`ESC ^`) and SOS (`ESC X`) are control strings whose
                    // payload is ignored through ST. Treat both alike here;
                    // their ESC + non-ST recovery is handled below.
                    b'^' | b'X' => {
                        self.state = State::Ignore;
                    }
                    b'c' => {
                        hard_reset!();
                    }
                    _ => {
                        self.passthrough.push(0x1b);
                        self.passthrough.push(b);
                        self.state = State::Ground;
                    }
                },

                State::Csi { buf } => {
                    if (0x40..=0x7e).contains(&b) {
                        // Final byte of CSI sequence
                        let params = std::mem::take(buf);
                        self.state = State::Ground;
                        let alt_mode = alt_screen_mode(&params);
                        if (b == b'h' || b == b'l')
                            && params.first() == Some(&b'?')
                            && alt_mode.is_none()
                        {
                            for mode in self.update_dec_private_modes(&params[1..], b == b'h') {
                                events.push(ParserEvent::DecsetMode {
                                    mode,
                                    set: b == b'h',
                                });
                            }
                        }
                        let erase_scrollback = b == b'J' && is_erase_scrollback(&params);
                        if erase_scrollback {
                            flush!();
                            events.push(ParserEvent::EraseScrollback);
                            // Like RIS, ED3 must reach VTE before any suffix
                            // event from this same feed is dispatched.
                            let mut sequence = Vec::with_capacity(params.len() + 3);
                            sequence.extend_from_slice(b"\x1b[");
                            sequence.extend_from_slice(&params);
                            sequence.push(b);
                            events.push(ParserEvent::Bytes(sequence));
                        }
                        if let (b'h', Some(mode)) = (b, alt_mode) {
                            // Recognized alt-screen enter: drop the sequence bytes
                            // (never passed through) and emit the exact DEC mode.
                            flush!();
                            events.push(ParserEvent::AltScreenEnter(mode));
                        } else if let (b'l', Some(mode)) = (b, alt_mode) {
                            flush!();
                            events.push(ParserEvent::AltScreenLeave(mode));
                        } else if !self.config.mouse_reporting
                            && (b == b'h' || b == b'l')
                            && is_mouse_reporting_mode(&params)
                        {
                            // Drop: keep VTE out of mouse reporting mode.
                        } else if !self.config.focus_reporting
                            && (b == b'h' || b == b'l')
                            && is_focus_reporting_mode(&params)
                        {
                            // Drop: keep VTE out of focus reporting mode.
                        } else if !erase_scrollback {
                            // Detect terminal-capability handshakes whose response
                            // the active VTE would write back through its own PTY
                            // (which is not connected). The caller synthesizes a
                            // canned reply on `ctx.pty` so neovim/helix/etc. don't
                            // hang waiting on it. The byte stream itself is still
                            // passed through so the VTE updates its internal state.
                            //
                            // `CSI ? u`                       — kitty keyboard query
                            // `CSI ? 4 m`                     — XTQMODKEYS query
                            // `CSI c`, `CSI 0 c`              — primary DA (DA1)
                            // `CSI > c`, `CSI > 0 c`          — secondary DA (DA2)
                            // `CSI = c`, `CSI = 0 c`          — tertiary DA (DA3)
                            // `CSI > q`                       — XTVERSION
                            // `CSI 5 n` / `CSI 6 n`           — DSR status / cursor pos
                            match (b, params.as_slice()) {
                                (b'u', b"?") => {
                                    events.push(ParserEvent::KeyboardProtocolQuery(
                                        KeyboardProtocolQuery::KittyQuery,
                                    ));
                                }
                                (b'u', p) if p.first() == Some(&b'>') => {
                                    events.push(ParserEvent::KittyKeyboard(
                                        KittyKeyboardOp::Push(csi_u8_param(&p[1..])),
                                    ));
                                }
                                (b'u', p) if p.first() == Some(&b'<') => {
                                    events.push(ParserEvent::KittyKeyboard(
                                        KittyKeyboardOp::Pop(csi_u8_param(&p[1..])),
                                    ));
                                }
                                (b'u', p) if p.first() == Some(&b'=') => {
                                    let mut fields = p[1..].split(|&c| c == b';');
                                    let flags = csi_u8_param(fields.next().unwrap_or(b""));
                                    let mode = fields
                                        .next()
                                        .map(csi_u8_param)
                                        .filter(|mode| *mode != 0)
                                        .unwrap_or(1);
                                    events.push(ParserEvent::KittyKeyboard(
                                        KittyKeyboardOp::Set { flags, mode },
                                    ));
                                }
                                (b'm', b"?4") => {
                                    events.push(ParserEvent::KeyboardProtocolQuery(
                                        KeyboardProtocolQuery::ModifyOtherKeysQuery,
                                    ));
                                }
                                (b'c', b"") | (b'c', b"0") => {
                                    events.push(ParserEvent::KeyboardProtocolQuery(
                                        KeyboardProtocolQuery::PrimaryDeviceAttributes,
                                    ));
                                }
                                (b'c', b">") | (b'c', b">0") => {
                                    events.push(ParserEvent::KeyboardProtocolQuery(
                                        KeyboardProtocolQuery::SecondaryDeviceAttributes,
                                    ));
                                }
                                (b'c', b"=") | (b'c', b"=0") => {
                                    events.push(ParserEvent::KeyboardProtocolQuery(
                                        KeyboardProtocolQuery::TertiaryDeviceAttributes,
                                    ));
                                }
                                (b'q', b">") | (b'q', b">0") => {
                                    events.push(ParserEvent::KeyboardProtocolQuery(
                                        KeyboardProtocolQuery::XtVersion,
                                    ));
                                }
                                (b'n', b"5") => {
                                    events.push(ParserEvent::KeyboardProtocolQuery(
                                        KeyboardProtocolQuery::DeviceStatus,
                                    ));
                                }
                                (b'n', b"6") => {
                                    events.push(ParserEvent::KeyboardProtocolQuery(
                                        KeyboardProtocolQuery::CursorPosition,
                                    ));
                                }
                                _ => {}
                            }
                            // Pass the complete sequence through as one contiguous run.
                            self.passthrough.push(0x1b);
                            self.passthrough.push(b'[');
                            self.passthrough.extend_from_slice(&params);
                            self.passthrough.push(b);
                        }
                    } else {
                        buf.push(b);
                        // Guard against an unterminated CSI growing without bound
                        // (malformed stream). Dump what we have and recover.
                        if buf.len() > 4096 {
                            let params = std::mem::take(buf);
                            self.state = State::Ground;
                            self.passthrough.push(0x1b);
                            self.passthrough.push(b'[');
                            self.passthrough.extend_from_slice(&params);
                        }
                    }
                }

                State::Osc { buf } => match b {
                    0x07 => {
                        let payload = std::mem::take(buf);
                        self.state = State::Ground;
                        flush!();
                        handle_osc(&payload, events);
                    }
                    0x1b => {
                        let payload = std::mem::take(buf);
                        self.state = State::OscEsc { payload };
                    }
                    _ => {
                        if buf.len() >= MAX_OSC_PAYLOAD_BYTES {
                            log::warn!("Dropping OSC larger than {MAX_OSC_PAYLOAD_BYTES} bytes");
                            // Release the accumulated payload immediately, then
                            // resynchronise only at this string's terminator.
                            self.state = State::OscDiscard;
                        } else {
                            buf.push(b);
                        }
                    }
                },

                State::OscEsc { payload } => {
                    if b == b'\\' {
                        let payload = std::mem::take(payload);
                        self.state = State::Ground;
                        flush!();
                        handle_osc(&payload, events);
                    } else {
                        // ESC followed by a non-ST byte aborts the incomplete
                        // OSC. Do not accept its payload (especially OSC 133),
                        // and reinterpret this ESC + byte as a fresh escape
                        // sequence. The byte is processed inline so RIS and
                        // suffix semantic events keep exact stream order.
                        reprocess_escape_final!(b);
                    }
                }

                State::OscDiscard => match b {
                    0x07 => self.state = State::Ground,
                    0x1b => self.state = State::OscDiscardEsc,
                    _ => {}
                },

                State::OscDiscardEsc => {
                    if b == b'\\' {
                        self.state = State::Ground;
                    } else {
                        reprocess_escape_final!(b);
                    }
                }

                State::Apc { buf } => match b {
                    0x1b => {
                        let payload = std::mem::take(buf);
                        self.state = State::ApcEsc { payload };
                    }
                    _ => {
                        if buf.len() >= MAX_APC_PAYLOAD_BYTES {
                            log::warn!("Dropping APC larger than {MAX_APC_PAYLOAD_BYTES} bytes");
                            self.state = State::ApcDiscard;
                        } else {
                            buf.push(b);
                        }
                    }
                },

                State::ApcEsc { payload } => {
                    if b == b'\\' {
                        let payload = std::mem::take(payload);
                        self.state = State::Ground;
                        flush!();
                        events.push(ParserEvent::ApcSequence(payload));
                    } else {
                        reprocess_escape_final!(b);
                    }
                }

                State::ApcDiscard => {
                    if b == 0x1b {
                        self.state = State::ApcDiscardEsc;
                    }
                }

                State::ApcDiscardEsc => {
                    if b == b'\\' {
                        self.state = State::Ground;
                    } else {
                        reprocess_escape_final!(b);
                    }
                }

                State::Dcs { buf } => match b {
                    0x1b => {
                        let payload = std::mem::take(buf);
                        self.state = State::DcsEsc { payload };
                    }
                    _ => {
                        if buf.len() >= MAX_DCS_PAYLOAD_BYTES {
                            log::warn!("Dropping DCS larger than {MAX_DCS_PAYLOAD_BYTES} bytes");
                            self.state = State::DcsDiscard;
                        } else {
                            buf.push(b);
                        }
                    }
                },

                State::DcsEsc { payload } => {
                    if b == b'\\' {
                        let payload = std::mem::take(payload);
                        self.state = State::Ground;
                        emit_dcs_passthrough(&payload, &mut self.passthrough);
                    } else {
                        reprocess_escape_final!(b);
                    }
                }

                State::DcsDiscard => {
                    if b == 0x1b {
                        self.state = State::DcsDiscardEsc;
                    }
                }

                State::DcsDiscardEsc => {
                    if b == b'\\' {
                        self.state = State::Ground;
                    } else {
                        reprocess_escape_final!(b);
                    }
                }

                State::Ignore => {
                    if b == 0x1b {
                        self.state = State::IgnoreEsc;
                    }
                }

                State::IgnoreEsc => {
                    if b == b'\\' {
                        self.state = State::Ground;
                    } else {
                        reprocess_escape_final!(b);
                    }
                }
            }
        }

        flush!();
    }
}

/// Rewrap a DCS payload as `ESC P ... ESC \` and append to the passthrough buffer
/// so the active VTE — which can interpret sixel, DECRQSS replies, tmux
/// passthrough, etc. — gets the original sequence verbatim.
fn emit_dcs_passthrough(payload: &[u8], passthrough: &mut Vec<u8>) {
    passthrough.reserve(payload.len() + 4);
    passthrough.push(0x1b);
    passthrough.push(b'P');
    passthrough.extend_from_slice(payload);
    passthrough.push(0x1b);
    passthrough.push(b'\\');
}

/// The metadata a shell attaches to its OSC 133 `C` and `D` packets.
///
/// jsh — the family's own shell — carries the command line, the execution id it
/// keys its execution journal on, the cwd and the measured duration, all
/// percent-encoded (see `jsh/src/osc.rs`, whose tests pin the exact bytes).
/// This parser used to split on `;`, read the mark, and throw every parameter
/// away, so the two frontends that share it reconstructed the command by
/// scraping it back off the screen and could never correlate their captured
/// output with a journal record — while [`crate::execution_journal::submit`]
/// existed for exactly that.
///
/// Every field is optional: the same mark is emitted by shells that send no
/// metadata at all, and a field whose encoding is malformed, oversized or not
/// UTF-8 is dropped rather than guessed at, so a hostile producer degrades the
/// metadata instead of the mark.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommandMeta {
    /// The shell's own execution id — the correlation key into its journal.
    pub id: Option<String>,
    /// The command line as the shell parsed it, not as the screen rendered it.
    pub command: Option<String>,
    /// Working directory the command ran in.
    pub cwd: Option<String>,
    /// Wall-clock duration the shell measured, which beats a timer started when
    /// the frontend happened to notice the mark.
    pub duration_ms: Option<u64>,
    /// The shell had a command line but it exceeded the packet budget, so
    /// [`CommandMeta::command`] is absent for a reason worth telling apart from
    /// "this shell sends no metadata".
    pub command_truncated: bool,
}

impl CommandMeta {
    /// Parse the `key=value` fields of an OSC 133 packet.
    ///
    /// Key aliases follow ember's decoder, which is the family's most complete
    /// and has been reading jsh's packets in production; accepting its spellings
    /// means the shared parser cannot understand *less* than the frontend that
    /// already had its own.
    fn from_fields<'a>(fields: impl Iterator<Item = &'a str>) -> Self {
        let mut meta = Self::default();
        for field in fields {
            let Some((key, value)) = field.split_once('=') else {
                continue;
            };
            match key {
                "id" | "jsh_id" | "execution_id" | "command_id" => {
                    meta.id = decode_osc133(value, MAX_OSC133_ID_BYTES).filter(|id| {
                        !id.is_empty()
                            && !id.chars().any(char::is_control)
                            && !crate::review_input::contains_visual_spoofing(id)
                    });
                }
                "cmdline_url" | "command_url" | "command" | "cmdline" => {
                    meta.command = decode_osc133(value, MAX_OSC133_COMMAND_BYTES).filter(|text| {
                        !text.chars().any(char::is_control)
                            && !crate::review_input::contains_visual_spoofing(text)
                    });
                }
                "cwd_url" | "cwd" => {
                    meta.cwd = decode_osc133(value, MAX_OSC133_CWD_BYTES).filter(|text| {
                        !text.chars().any(char::is_control)
                            && !crate::review_input::contains_visual_spoofing(text)
                    });
                }
                "duration_ms" | "duration" => {
                    meta.duration_ms = value.parse().ok();
                }
                "cmd_truncated" | "command_truncated" => {
                    meta.command_truncated = value == "1" || value.eq_ignore_ascii_case("true");
                }
                _ => {}
            }
        }
        meta
    }
}

/// Strict percent-decode of one OSC 133 metadata field.
///
/// Strict on purpose, following ember: a truncated escape, an oversized value
/// or invalid UTF-8 yields `None` rather than a best-effort string, because the
/// decoded value is used as a filesystem path, a journal key, and text put in
/// front of the user. Half-decoded input is worse than none.
fn decode_osc133(value: &str, max_bytes: usize) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded: Vec<u8> = Vec::with_capacity(bytes.len().min(max_bytes));
    let mut i = 0;
    while i < bytes.len() {
        let byte = if bytes[i] == b'%' {
            // Needs two more bytes; `i + 2` must be a valid index.
            if i + 2 >= bytes.len() {
                return None;
            }
            let high = (bytes[i + 1] as char).to_digit(16)? as u8;
            let low = (bytes[i + 2] as char).to_digit(16)? as u8;
            i += 3;
            (high << 4) | low
        } else {
            let byte = bytes[i];
            i += 1;
            byte
        };
        if decoded.len() == max_bytes {
            return None;
        }
        decoded.push(byte);
    }
    String::from_utf8(decoded).ok()
}

fn valid_remote_session_id(id: &str) -> bool {
    crate::execution_journal::is_valid_jsh_session_id(id)
}

fn handle_osc(payload: &[u8], events: &mut Vec<ParserEvent>) {
    let s = match std::str::from_utf8(payload) {
        Ok(s) => s,
        Err(_) => return,
    };

    // OSC 133 ; <mark> [; params...] — shell integration (FTCS).
    if let Some(rest) = s.strip_prefix("133;") {
        let mut fields = rest.split(';');
        match fields.next() {
            Some("A") => events.push(ParserEvent::PromptStart),
            Some("B") => events.push(ParserEvent::PromptEnd),
            Some("C") => {
                events.push(ParserEvent::CommandStart(CommandMeta::from_fields(fields)));
            }
            Some("D") => {
                // The exit status is positional and comes first, but a shell may
                // omit it and send only `key=value` metadata, so a field that
                // carries an `=` is metadata rather than an unparseable status.
                let mut rest = fields.peekable();
                let exit = match rest.peek() {
                    Some(field) if !field.contains('=') => {
                        rest.next().and_then(|field| field.parse::<i32>().ok())
                    }
                    _ => None,
                };
                events.push(ParserEvent::CommandEnd {
                    exit,
                    meta: CommandMeta::from_fields(rest),
                });
            }
            _ => {}
        }
        return;
    }

    // OSC 7770 ; <session-id> — jsh-specific session announce (see jsh osc.rs:107).
    if let Some(rest) = s.strip_prefix("7770;") {
        // Session IDs are protocol identifiers, not display text. Do not trim
        // an invalid payload into a different valid identity: jsh's grammar is
        // already an exact 1-128-byte ASCII token.
        let id = rest;
        if valid_remote_session_id(id) {
            events.push(ParserEvent::RemoteSessionId(id.to_string()));
        }
        return;
    }

    // OSC 7771 ; <32-hex-token> — local Forge shell integration readiness.
    // The token came from a one-shot inherited fd rather than argv/env. Keep
    // this packet out of VTE and let the pane compare it with its private copy.
    if let Some(token) = s.strip_prefix("7771;") {
        if token.len() == 32 && token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            events.push(ParserEvent::AgentIntegrationReady(token.to_string()));
        }
        return;
    }

    // OSC 7 (cwd), OSC 0 / 1 / 2 (title/icon), and everything else: pass through
    // unchanged. VTE consumes them natively and fires
    // current-directory-uri-notify / window-title-changed signals, which the
    // block_view subscribes to instead of re-parsing here.
    if s.starts_with("7;") {
        if payload.len() > MAX_OSC7_URI_BYTES || !safe_osc7_text(s) {
            return;
        }
        let mut bytes = Vec::with_capacity(payload.len() + 4);
        bytes.push(0x1b);
        bytes.push(b']');
        bytes.extend_from_slice(payload);
        bytes.extend_from_slice(b"\x1b\\");
        events.push(ParserEvent::Bytes(bytes));
        return;
    }

    // Title/icon strings become trusted-looking window and tab chrome in the
    // frontends. Drop an unsafe update instead of letting terminal output
    // reorder or invisibly alter that UI label.
    if matches!(s.split_once(';'), Some(("0" | "1" | "2", _)))
        && (payload.len() > MAX_TITLE_BYTES
            || s.chars().any(char::is_control)
            || crate::review_input::contains_visual_spoofing(s))
    {
        return;
    }

    // OSC 9 ; <text> — iTerm2/ConEmu-style desktop notification request.
    if let Some(rest) = s.strip_prefix("9;") {
        let body = bounded_notification_field(rest);
        if !body.is_empty() {
            events.push(ParserEvent::Notification { title: None, body });
        }
        return;
    }

    // OSC 777 ; notify ; <title> ; <body> — urxvt notification extension.
    // Other OSC 777 subcommands fall through to the generic pass-through.
    if let Some(rest) = s.strip_prefix("777;") {
        let mut fields = rest.splitn(3, ';');
        if fields.next() == Some("notify") {
            let title = bounded_notification_field(fields.next().unwrap_or(""));
            let body = bounded_notification_field(fields.next().unwrap_or(""));
            if !(title.is_empty() && body.is_empty()) {
                events.push(ParserEvent::Notification {
                    title: (!title.is_empty()).then_some(title),
                    body,
                });
            }
            return;
        }
    }

    // OSC 10 ; ? / OSC 11 ; ? / OSC 12 ; ?  — color queries (XParseColor reply).
    // The active VTE in block view has no return PTY, so the response we'd
    // expect VTE to emit never reaches the app. Emit a semantic event and let
    // the caller write a reply on the real PTY. A SET (any non-`?` value)
    // additionally emits ColorSet and falls through to the byte pass-through
    // so the live view recolors natively while the caller tracks the value.
    for (prefix, kind) in [
        ("10;", ColorKind::Foreground),
        ("11;", ColorKind::Background),
        ("12;", ColorKind::Cursor),
    ] {
        if let Some(rest) = s.strip_prefix(prefix) {
            if rest.starts_with('?') {
                events.push(ParserEvent::ColorQuery(kind));
                return;
            }
            let spec = rest.trim();
            if !spec.is_empty() && spec.chars().count() <= MAX_COLOR_SPEC_CHARS {
                events.push(ParserEvent::ColorSet {
                    kind,
                    spec: spec.to_string(),
                });
            }
        }
    }

    // OSC 110/111/112 — reset a dynamic color; bytes also pass through.
    for (code, kind) in [
        ("110", ColorKind::Foreground),
        ("111", ColorKind::Background),
        ("112", ColorKind::Cursor),
    ] {
        if s == code
            || s.strip_prefix(code)
                .is_some_and(|rest| rest.starts_with(';'))
        {
            events.push(ParserEvent::ColorReset(kind));
        }
    }

    // OSC 4 ; <idx> ; ? — palette color query.
    if let Some(rest) = s.strip_prefix("4;") {
        let mut it = rest.splitn(2, ';');
        if let (Some(idx_str), Some(value)) = (it.next(), it.next()) {
            if value.starts_with('?') {
                if let Ok(idx) = idx_str.parse::<u8>() {
                    events.push(ParserEvent::ColorQuery(ColorKind::Palette(idx)));
                    return;
                }
            }
        }
    }

    // OSC 52 ; <selection> ; <base64-data | ?> — clipboard set / query
    if let Some(rest) = s.strip_prefix("52;") {
        if let Some(data_start) = rest.find(';') {
            let b64_data = &rest[data_start + 1..];
            if b64_data == "?" {
                events.push(ParserEvent::ClipboardQuery);
            } else if b64_data.len() <= MAX_CLIPBOARD_BASE64_BYTES {
                if let Ok(decoded) = base64_decode(b64_data.as_bytes()) {
                    if let Ok(text) = String::from_utf8(decoded) {
                        events.push(ParserEvent::ClipboardSet(text));
                    }
                }
            } else {
                log::warn!(
                    "Ignoring OSC 52 payload larger than {MAX_CLIPBOARD_BASE64_BYTES} bytes"
                );
            }
        }
        return;
    }

    // All other OSC sequences: reconstruct and pass through.
    let mut bytes = Vec::with_capacity(payload.len() + 4);
    bytes.push(0x1b);
    bytes.push(b']');
    bytes.extend_from_slice(payload);
    bytes.push(0x07);
    events.push(ParserEvent::Bytes(bytes));
}

/// Validate both the wire spelling and the value a URI consumer sees after
/// percent decoding. Checking only the former lets `%0a` or `%E2%80%AE`
/// reappear as a line break or bidi override in cwd-derived terminal chrome.
fn safe_osc7_text(value: &str) -> bool {
    if value.chars().any(char::is_control) || crate::review_input::contains_visual_spoofing(value) {
        return false;
    }

    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index.saturating_add(2) >= bytes.len() {
                return false;
            }
            let Some(high) = (bytes[index + 1] as char).to_digit(16) else {
                return false;
            };
            let Some(low) = (bytes[index + 2] as char).to_digit(16) else {
                return false;
            };
            decoded.push(((high << 4) | low) as u8);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).is_ok_and(|decoded| {
        !decoded.chars().any(char::is_control)
            && !crate::review_input::contains_visual_spoofing(&decoded)
    })
}

/// Bound one OSC 9/777 field: strip control characters (the text reaches
/// notification daemons verbatim), expose visual formatting as replacement
/// glyphs, collapse surrounding whitespace, and cap the length. Applications,
/// not users, author these strings.
fn bounded_notification_field(raw: &str) -> String {
    raw.chars()
        .map(|ch| {
            if ch.is_control() || crate::review_input::is_visual_spoofing_character(ch) {
                '\u{fffd}'
            } else {
                ch
            }
        })
        .take(MAX_NOTIFICATION_CHARS)
        .collect::<String>()
        .trim()
        .to_string()
}

fn base64_decode(input: &[u8]) -> Result<Vec<u8>, ()> {
    const TABLE: [u8; 256] = {
        let mut t = [0xFFu8; 256];
        let mut i = 0u8;
        loop {
            if i >= 26 {
                break;
            }
            t[(b'A' + i) as usize] = i;
            i += 1;
        }
        i = 0;
        loop {
            if i >= 26 {
                break;
            }
            t[(b'a' + i) as usize] = 26 + i;
            i += 1;
        }
        i = 0;
        loop {
            if i >= 10 {
                break;
            }
            t[(b'0' + i) as usize] = 52 + i;
            i += 1;
        }
        t[b'+' as usize] = 62;
        t[b'/' as usize] = 63;
        t
    };

    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;

    for &b in input {
        if b == b'=' || b == b'\n' || b == b'\r' {
            continue;
        }
        let val = TABLE[b as usize];
        if val == 0xFF {
            return Err(());
        }
        buf = (buf << 6) | val as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect_bytes(events: &[ParserEvent]) -> Vec<u8> {
        let mut out = Vec::new();
        for e in events {
            if let ParserEvent::Bytes(b) = e {
                out.extend_from_slice(b);
            }
        }
        out
    }

    #[test]
    fn ed3_and_ris_are_pre_feed_coalescing_barriers() {
        for (sequence, semantic) in [
            (b"\x1b[3J".as_slice(), ParserEvent::EraseScrollback),
            (b"\x1b[03J".as_slice(), ParserEvent::EraseScrollback),
            (b"\x1bc".as_slice(), ParserEvent::HardReset),
        ] {
            let mut parser = Parser::new();
            let mut events = Vec::new();
            let mut input = b"before".to_vec();
            input.extend_from_slice(sequence);
            input.extend_from_slice(b"after");
            parser.feed(&input, &mut events);
            assert_eq!(events[0], ParserEvent::Bytes(b"before".to_vec()));
            assert_eq!(events[1], semantic);
            assert_eq!(events[2], ParserEvent::Bytes(sequence.to_vec()));
            assert_eq!(events[3], ParserEvent::Bytes(b"after".to_vec()));
            assert_eq!(collect_bytes(&events), input);
        }
    }

    #[test]
    fn reset_boundaries_survive_every_byte_split_and_ignore_lookalikes() {
        for sequence in [b"\x1b[3J".as_slice(), b"\x1b[03J", b"\x1bc"] {
            let mut parser = Parser::new();
            let mut events = Vec::new();
            for byte in sequence {
                parser.feed(std::slice::from_ref(byte), &mut events);
            }
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(
                        event,
                        ParserEvent::EraseScrollback | ParserEvent::HardReset
                    ))
                    .count(),
                1
            );
            assert_eq!(collect_bytes(&events), sequence);
        }

        let mut parser = Parser::new();
        let mut events = Vec::new();
        // Exercise unrelated ED lookalikes and the discarded PM/SOS strings
        // one byte at a time. PM/SOS only end at ST, so BEL stays inert. ESC c
        // is intentionally covered by the abort-and-reprocess tests below.
        for byte in b"\x1b[2J\x1b[?3J\x1b[3;0J\x1b[3:0J\x1b[3 J\x1b[3K\x1b]0;inside [3J\x07\x1b^inside \x07 still inside\x1b\\\x1bXinside \x07 still inside\x1b\\"
        {
            parser.feed(std::slice::from_ref(byte), &mut events);
        }
        assert!(events
            .iter()
            .all(|event| !matches!(event, ParserEvent::EraseScrollback | ParserEvent::HardReset)));
    }

    #[test]
    fn reset_raw_bytes_are_an_immediate_barrier_before_suffix_semantics() {
        for (reset, semantic) in [
            (b"\x1bc".as_slice(), ParserEvent::HardReset),
            (b"\x1b[3J".as_slice(), ParserEvent::EraseScrollback),
        ] {
            let mut parser = Parser::new();
            let mut events = Vec::new();
            let mut input = b"prefix".to_vec();
            input.extend_from_slice(reset);
            input.extend_from_slice(b"\x1b[c\x1b[?2004htext");
            parser.feed(&input, &mut events);

            assert_eq!(events[0], ParserEvent::Bytes(b"prefix".to_vec()));
            assert_eq!(events[1], semantic);
            assert_eq!(events[2], ParserEvent::Bytes(reset.to_vec()));
            assert_eq!(
                events[3],
                ParserEvent::KeyboardProtocolQuery(KeyboardProtocolQuery::PrimaryDeviceAttributes)
            );
            assert_eq!(
                events[4],
                ParserEvent::DecsetMode {
                    mode: 2004,
                    set: true
                }
            );
            assert_eq!(
                collect_bytes(&events),
                [b"prefix".as_slice(), reset, b"\x1b[c\x1b[?2004htext"].concat()
            );
        }
    }

    #[test]
    fn malformed_control_strings_abort_and_reprocess_escape_sequence() {
        for prefix in [
            b"\x1b]133;A".as_slice(),
            b"\x1b_Ga=T;AAAA".as_slice(),
            b"\x1bPqpayload".as_slice(),
            b"\x1b^private-message".as_slice(),
            b"\x1bXsos-payload".as_slice(),
        ] {
            for split in 0..=prefix.len() + 2 {
                let mut parser = Parser::new();
                let mut events = Vec::new();
                let mut input = prefix.to_vec();
                input.extend_from_slice(b"\x1bc\x1b[c");

                let split = split.min(input.len());
                parser.feed(&input[..split], &mut events);
                for byte in &input[split..] {
                    parser.feed(std::slice::from_ref(byte), &mut events);
                }

                assert_eq!(
                    events
                        .iter()
                        .filter(|event| matches!(event, ParserEvent::HardReset))
                        .count(),
                    1,
                    "prefix={prefix:?}, split={split}, events={events:?}"
                );
                assert!(events.iter().all(|event| !matches!(
                    event,
                    ParserEvent::PromptStart | ParserEvent::ApcSequence(_)
                )));
                let reset = events
                    .iter()
                    .position(|event| matches!(event, ParserEvent::HardReset))
                    .unwrap();
                assert_eq!(events[reset + 1], ParserEvent::Bytes(b"\x1bc".to_vec()));
                assert!(matches!(
                    events[reset + 2],
                    ParserEvent::KeyboardProtocolQuery(
                        KeyboardProtocolQuery::PrimaryDeviceAttributes
                    )
                ));
                assert_eq!(collect_bytes(&events), b"\x1bc\x1b[c");
            }
        }
    }

    #[test]
    fn doubled_escape_in_control_string_keeps_second_escape_as_ris_introducer() {
        for prefix in [
            b"\x1b]133;A".as_slice(),
            b"\x1b_Ga=T;AAAA".as_slice(),
            b"\x1bPqpayload".as_slice(),
            b"\x1b^private".as_slice(),
        ] {
            let input = [prefix, b"\x1b\x1bc\x1b[c".as_slice()].concat();
            let mut parser = Parser::new();
            let mut events = Vec::new();
            for byte in &input {
                parser.feed(std::slice::from_ref(byte), &mut events);
            }

            assert!(events.iter().all(|event| !matches!(
                event,
                ParserEvent::PromptStart | ParserEvent::ApcSequence(_)
            )));
            let reset = events
                .iter()
                .position(|event| matches!(event, ParserEvent::HardReset))
                .unwrap();
            assert_eq!(events[reset + 1], ParserEvent::Bytes(b"\x1bc".to_vec()));
            assert!(matches!(
                events[reset + 2],
                ParserEvent::KeyboardProtocolQuery(KeyboardProtocolQuery::PrimaryDeviceAttributes)
            ));
            assert_eq!(collect_bytes(&events), b"\x1bc\x1b[c");
        }
    }

    #[test]
    fn pm_sos_non_st_escape_aborts_before_a_real_ris() {
        for prefix in [b"\x1b^private".as_slice(), b"\x1bXsos".as_slice()] {
            let mut parser = Parser::new();
            let mut events = Vec::new();
            let input = [prefix, b"\x1bq".as_slice(), b"suffix\x1bc".as_slice()].concat();
            for byte in &input {
                parser.feed(std::slice::from_ref(byte), &mut events);
            }

            assert_eq!(events.first(), Some(&ParserEvent::Bytes(b"\x1bq".to_vec())));
            assert_eq!(
                events
                    .iter()
                    .position(|event| matches!(event, ParserEvent::HardReset)),
                Some(events.len() - 2)
            );
            assert_eq!(events.last(), Some(&ParserEvent::Bytes(b"\x1bc".to_vec())));
            assert_eq!(collect_bytes(&events), b"\x1bqsuffix\x1bc");
        }
    }

    #[test]
    fn bel_terminates_only_osc_not_apc_or_dcs() {
        let mut parser = Parser::new();
        let mut events = Vec::new();
        parser.feed(b"\x1b]133;A\x07", &mut events);
        assert!(matches!(&events[..], [ParserEvent::PromptStart]));

        events.clear();
        parser.feed(b"\x1b_Ga=T;A\x07B", &mut events);
        assert!(events.is_empty());
        parser.feed(b"\x1b\\", &mut events);
        assert_eq!(events, [ParserEvent::ApcSequence(b"Ga=T;A\x07B".to_vec())]);

        events.clear();
        parser.feed(b"\x1bPqA\x07B", &mut events);
        assert!(events.is_empty());
        parser.feed(b"\x1b\\", &mut events);
        assert_eq!(collect_bytes(&events), b"\x1bPqA\x07B\x1b\\");
    }

    #[test]
    fn ris_resets_parser_private_mode_snooping() {
        let mut parser = Parser::new();
        let mut events = Vec::new();
        parser.feed(b"\x1b[?2004;1003;1006;1004h", &mut events);
        assert!(parser.bracketed_paste());
        assert_eq!(parser.mouse_mode(), MouseMode::AnyEvent);
        assert_eq!(parser.mouse_encoding(), MouseEncoding::Sgr);
        assert!(parser.focus_events());

        events.clear();
        parser.feed(b"\x1bc", &mut events);
        assert!(!parser.bracketed_paste());
        assert_eq!(parser.mouse_mode(), MouseMode::None);
        assert_eq!(parser.mouse_encoding(), MouseEncoding::Default);
        assert!(!parser.focus_events());
        assert_eq!(events[0], ParserEvent::HardReset);
    }

    #[test]
    fn oversized_osc_is_discarded_and_recovers_from_split_st() {
        let mut parser = Parser::new();
        let mut events = Vec::new();
        parser.feed(b"\x1b]0;", &mut events);

        let chunk = vec![b'x'; 64 * 1024];
        for _ in 0..=(MAX_OSC_PAYLOAD_BYTES / chunk.len()) {
            parser.feed(&chunk, &mut events);
        }

        assert!(events.is_empty());
        assert!(matches!(parser.state, State::OscDiscard));

        parser.feed(b"\x1b", &mut events);
        assert!(matches!(parser.state, State::OscDiscardEsc));
        // A non-ST escape aborts the oversized string and is reinterpreted as
        // a new ESC sequence. `ESC X` starts SOS, so use a harmless escape
        // final here before checking ordinary text becomes visible again.
        parser.feed(b"qvisible", &mut events);

        assert!(matches!(parser.state, State::Ground));
        assert_eq!(collect_bytes(&events), b"\x1bqvisible");
    }

    #[test]
    fn ed3_parameter_past_u32_saturates_away_from_erase_scrollback() {
        let mut parser = Parser::new();
        let mut events = Vec::new();
        // 42949672963 > u32::MAX: the accumulator must saturate, never wrap
        // back onto the value 3 and fire the scrollback side effect for a
        // parameter the active VTE will not read as ED3 either.
        parser.feed(b"\x1b[42949672963J", &mut events);
        assert!(events
            .iter()
            .all(|event| !matches!(event, ParserEvent::EraseScrollback)));
        assert_eq!(collect_bytes(&events), b"\x1b[42949672963J");
    }

    #[test]
    fn osc_aborted_by_esc_bel_drops_the_payload_and_passes_both_bytes_through() {
        let mut parser = Parser::new();
        let mut events = Vec::new();
        // ESC BEL is not ST: the incomplete OSC must be aborted without any
        // handle_osc effect (no OSC 133 mark), and the raw ESC BEL bytes are
        // reinterpreted as an ordinary escape sequence for the terminal.
        parser.feed(b"\x1b]133;A\x1b\x07after", &mut events);
        assert!(events
            .iter()
            .all(|event| !matches!(event, ParserEvent::PromptStart)));
        assert!(matches!(parser.state, State::Ground));
        assert_eq!(collect_bytes(&events), b"\x1b\x07after");
    }

    #[test]
    fn oversized_apc_and_dcs_abort_and_reprocess_a_non_st_escape() {
        let mut parser = Parser::new();
        let mut events = Vec::new();
        parser.feed(b"\x1b_Ga=T;", &mut events);
        let chunk = vec![b'A'; 64 * 1024];
        for _ in 0..=(MAX_APC_PAYLOAD_BYTES / chunk.len()) {
            parser.feed(&chunk, &mut events);
        }
        assert!(matches!(parser.state, State::ApcDiscard));
        parser.feed(b"\x1b", &mut events);
        assert!(matches!(parser.state, State::ApcDiscardEsc));
        // A non-ST escape aborts the oversized string and is reinterpreted as
        // a new ESC sequence; ordinary text becomes visible again.
        parser.feed(b"qvisible", &mut events);
        assert!(matches!(parser.state, State::Ground));
        assert_eq!(collect_bytes(&events), b"\x1bqvisible");
        assert!(events
            .iter()
            .all(|event| !matches!(event, ParserEvent::ApcSequence(_))));

        let mut parser = Parser::new();
        let mut events = Vec::new();
        parser.feed(b"\x1bPq", &mut events);
        let chunk = vec![b'x'; 8 * 1024];
        for _ in 0..=(MAX_DCS_PAYLOAD_BYTES / chunk.len()) {
            parser.feed(&chunk, &mut events);
        }
        assert!(matches!(parser.state, State::DcsDiscard));
        parser.feed(b"\x1b", &mut events);
        assert!(matches!(parser.state, State::DcsDiscardEsc));
        parser.feed(b"qvisible", &mut events);
        assert!(matches!(parser.state, State::Ground));
        assert_eq!(collect_bytes(&events), b"\x1bqvisible");
    }

    #[test]
    fn osc_9_and_777_become_bounded_notification_events() {
        let mut parser = Parser::new();
        let mut events = Vec::new();
        parser.feed(b"\x1b]9;build finished\x07", &mut events);
        assert!(matches!(
            &events[..],
            [ParserEvent::Notification { title: None, body }] if body == "build finished"
        ));

        events.clear();
        parser.feed(b"\x1b]777;notify;CI;tests green\x1b\\", &mut events);
        assert!(matches!(
            &events[..],
            [ParserEvent::Notification { title: Some(title), body }]
                if title == "CI" && body == "tests green"
        ));

        // Control and visual-formatting characters are made visible and fields
        // are capped, so a hostile app cannot inject invisible structure into
        // the notification daemon or trusted-looking desktop chrome.
        events.clear();
        let long = "x".repeat(2 * MAX_NOTIFICATION_CHARS);
        parser.feed(b"\x1b]9;evil\ttab\x1b\\", &mut events);
        parser.feed(
            "\x1b]9;left\u{202e}right\u{00a0}tail\x07".as_bytes(),
            &mut events,
        );
        parser.feed(format!("\x1b]9;{long}\x07").as_bytes(), &mut events);
        let bodies: Vec<&String> = events
            .iter()
            .filter_map(|event| match event {
                ParserEvent::Notification { body, .. } => Some(body),
                _ => None,
            })
            .collect();
        assert_eq!(bodies[0], "evil\u{fffd}tab");
        assert_eq!(bodies[1], "left\u{fffd}right\u{fffd}tail");
        assert_eq!(bodies[2].chars().count(), MAX_NOTIFICATION_CHARS);

        // Empty notifications and non-notify OSC 777 subcommands emit nothing.
        events.clear();
        parser.feed(b"\x1b]9; \x07\x1b]777;other;a;b\x07", &mut events);
        assert!(events
            .iter()
            .all(|event| !matches!(event, ParserEvent::Notification { .. })));
    }

    #[test]
    fn dynamic_color_set_and_reset_emit_events_and_pass_through() {
        let mut parser = Parser::new();
        let mut events = Vec::new();
        parser.feed(b"\x1b]11;#1e1e2e\x07", &mut events);
        assert!(matches!(
            &events[..],
            [
                ParserEvent::ColorSet { kind: ColorKind::Background, spec },
                ParserEvent::Bytes(bytes),
            ] if spec == "#1e1e2e" && bytes.starts_with(b"\x1b]11;#1e1e2e")
        ));

        // Named specs forward verbatim for the app's toolkit to parse.
        events.clear();
        parser.feed(b"\x1b]10;rebeccapurple\x1b\\", &mut events);
        assert!(matches!(
            &events[..],
            [ParserEvent::ColorSet { kind: ColorKind::Foreground, spec }, ParserEvent::Bytes(_)]
                if spec == "rebeccapurple"
        ));

        // Queries stay consumed semantic events, exactly as before.
        events.clear();
        parser.feed(b"\x1b]12;?\x07", &mut events);
        assert!(matches!(
            &events[..],
            [ParserEvent::ColorQuery(ColorKind::Cursor)]
        ));

        // Resets emit the event and still pass through to the live view.
        events.clear();
        parser.feed(b"\x1b]110\x07\x1b]112;\x07", &mut events);
        let resets: Vec<&ColorKind> = events
            .iter()
            .filter_map(|event| match event {
                ParserEvent::ColorReset(kind) => Some(kind),
                _ => None,
            })
            .collect();
        assert_eq!(resets, [&ColorKind::Foreground, &ColorKind::Cursor]);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, ParserEvent::Bytes(_)))
                .count(),
            2
        );

        // Oversized specs only pass through — no semantic event.
        events.clear();
        let long = "x".repeat(MAX_COLOR_SPEC_CHARS + 1);
        parser.feed(format!("\x1b]11;{long}\x07").as_bytes(), &mut events);
        assert!(events
            .iter()
            .all(|event| !matches!(event, ParserEvent::ColorSet { .. })));
        assert!(matches!(&events[..], [ParserEvent::Bytes(_)]));
    }

    #[test]
    fn oversized_apc_is_discarded_and_recovers_at_st() {
        let mut parser = Parser::new();
        let mut events = Vec::new();
        parser.feed(b"\x1b_Ga=T;", &mut events);

        let chunk = vec![b'A'; 64 * 1024];
        for _ in 0..=(MAX_APC_PAYLOAD_BYTES / chunk.len()) {
            parser.feed(&chunk, &mut events);
        }

        assert!(events.is_empty());
        assert!(matches!(parser.state, State::ApcDiscard));

        parser.feed(b"\x07still-hidden", &mut events);
        assert!(matches!(parser.state, State::ApcDiscard));
        parser.feed(b"\x1b\\visible", &mut events);
        assert!(matches!(parser.state, State::Ground));
        assert_eq!(collect_bytes(&events), b"visible");
        assert!(events
            .iter()
            .all(|event| !matches!(event, ParserEvent::ApcSequence(_))));
    }

    #[test]
    fn csi_not_split_across_feeds() {
        let mut p = Parser::new();
        let mut events = Vec::new();
        p.feed(b"\x1b[3", &mut events);
        p.feed(b"1m", &mut events);
        let bytes_events: Vec<&Vec<u8>> = events
            .iter()
            .filter_map(|e| match e {
                ParserEvent::Bytes(b) => Some(b),
                _ => None,
            })
            .collect();
        assert_eq!(bytes_events.len(), 1, "CSI must not be split into pieces");
        assert_eq!(bytes_events[0].as_slice(), b"\x1b[31m");
    }

    #[test]
    fn alt_screen_enter_leave_emitted_and_stripped() {
        let mut p = Parser::new();
        let mut events = Vec::new();
        p.feed(b"\x1b[?1049h\x1b[?1049l", &mut events);
        assert!(matches!(events[0], ParserEvent::AltScreenEnter(1049)));
        assert!(matches!(events[1], ParserEvent::AltScreenLeave(1049)));
        assert!(collect_bytes(&events).is_empty());
    }

    #[test]
    fn legacy_alt_screen_modes_keep_their_exact_mode() {
        let mut p = Parser::new();
        let mut events = Vec::new();
        p.feed(b"\x1b[?47h\x1b[?47l\x1b[?1047h\x1b[?1047l", &mut events);
        assert!(matches!(events[0], ParserEvent::AltScreenEnter(47)));
        assert!(matches!(events[1], ParserEvent::AltScreenLeave(47)));
        assert!(matches!(events[2], ParserEvent::AltScreenEnter(1047)));
        assert!(matches!(events[3], ParserEvent::AltScreenLeave(1047)));
        assert!(collect_bytes(&events).is_empty());
    }

    #[test]
    fn dcs_is_passed_through_not_dropped() {
        // A DCS sixel sequence: ESC P q ... ESC \. The whole thing should
        // appear verbatim in the Bytes stream so the active VTE can render it.
        let mut p = Parser::new();
        let mut events = Vec::new();
        p.feed(b"before\x1bPq#0;2;0;0;0!100~-\x1b\\after", &mut events);
        let bytes = collect_bytes(&events);
        // The plain "before" and "after" survive, and the DCS round-trips.
        assert!(bytes.windows(6).any(|w| w == b"before"));
        assert!(bytes.windows(5).any(|w| w == b"after"));
        assert!(bytes.windows(3).any(|w| w == b"\x1bPq"));
        assert!(bytes.windows(2).any(|w| w == b"\x1b\\"));
    }

    #[test]
    fn oversized_dcs_is_discarded_until_its_actual_terminator() {
        let mut p = Parser::new();
        let mut events = Vec::new();
        p.feed(b"before\x1bPq", &mut events);
        let chunk = vec![b'x'; 8 * 1024];
        for _ in 0..=(MAX_DCS_PAYLOAD_BYTES / chunk.len()) {
            p.feed(&chunk, &mut events);
        }
        assert!(matches!(p.state, State::DcsDiscard));

        p.feed(b"must-not-leak\x1b", &mut events);
        assert!(matches!(p.state, State::DcsDiscardEsc));
        p.feed(b"\\after", &mut events);
        assert_eq!(collect_bytes(&events), b"beforeafter");
    }

    #[test]
    fn pm_st_does_not_leak_backslash() {
        let mut p = Parser::new();
        let mut events = Vec::new();
        p.feed(b"before\x1b^ignored\x1b\\after", &mut events);
        assert_eq!(collect_bytes(&events), b"beforeafter");
    }

    #[test]
    fn osc_color_queries_emit_events() {
        let mut p = Parser::new();
        let mut events = Vec::new();
        p.feed(
            b"\x1b]11;?\x07\x1b]10;?\x07\x1b]12;?\x1b\\\x1b]4;5;?\x07",
            &mut events,
        );
        let kinds: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                ParserEvent::ColorQuery(k) => Some(*k),
                _ => None,
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                ColorKind::Background,
                ColorKind::Foreground,
                ColorKind::Cursor,
                ColorKind::Palette(5),
            ]
        );
    }

    #[test]
    fn keyboard_protocol_queries_emit_events() {
        let mut p = Parser::new();
        let mut events = Vec::new();
        // kitty flag query, modifyOtherKeys query, primary & secondary DA.
        p.feed(b"\x1b[?u\x1b[?4m\x1b[c\x1b[>c", &mut events);
        let qs: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                ParserEvent::KeyboardProtocolQuery(q) => Some(*q),
                _ => None,
            })
            .collect();
        assert_eq!(
            qs,
            vec![
                KeyboardProtocolQuery::KittyQuery,
                KeyboardProtocolQuery::ModifyOtherKeysQuery,
                KeyboardProtocolQuery::PrimaryDeviceAttributes,
                KeyboardProtocolQuery::SecondaryDeviceAttributes,
            ]
        );
    }

    #[test]
    fn kitty_keyboard_stack_operations_emit_events_and_pass_through() {
        let mut p = Parser::new();
        let mut events = Vec::new();
        // codex/kimi push 7 and query; a pop, a bare pop, and the three set modes.
        p.feed(
            b"\x1b[>7u\x1b[?u\x1b[<2u\x1b[<u\x1b[=1;2u\x1b[=5;3u\x1b[=3u\x1b[>u",
            &mut events,
        );
        let ops: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                ParserEvent::KittyKeyboard(op) => Some(*op),
                _ => None,
            })
            .collect();
        assert_eq!(
            ops,
            vec![
                KittyKeyboardOp::Push(7),
                KittyKeyboardOp::Pop(2),
                KittyKeyboardOp::Pop(0),
                KittyKeyboardOp::Set { flags: 1, mode: 2 },
                KittyKeyboardOp::Set { flags: 5, mode: 3 },
                KittyKeyboardOp::Set { flags: 3, mode: 1 },
                KittyKeyboardOp::Push(0),
            ]
        );
        assert!(events.iter().any(|e| matches!(
            e,
            ParserEvent::KeyboardProtocolQuery(KeyboardProtocolQuery::KittyQuery)
        )));
        // Every sequence still reaches the surface verbatim.
        let passed: Vec<u8> = events
            .iter()
            .filter_map(|e| match e {
                ParserEvent::Bytes(b) => Some(b.clone()),
                _ => None,
            })
            .flatten()
            .collect();
        assert!(passed.windows(5).any(|w| w == b"\x1b[>7u"));
        assert!(passed.windows(7).any(|w| w == b"\x1b[=1;2u"));
    }

    #[test]
    fn kitty_keyboard_parameters_saturate_instead_of_wrapping() {
        let mut p = Parser::new();
        let mut events = Vec::new();
        p.feed(b"\x1b[>99999999999999999999u\x1b[<300u", &mut events);
        let ops: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                ParserEvent::KittyKeyboard(op) => Some(*op),
                _ => None,
            })
            .collect();
        assert_eq!(ops, vec![KittyKeyboardOp::Push(255), KittyKeyboardOp::Pop(255)]);
    }

    #[test]
    fn da1_da2_da3_emit_distinct_events() {
        let mut p = Parser::new();
        let mut events = Vec::new();
        p.feed(b"\x1b[0c\x1b[>0c\x1b[=0c", &mut events);
        let qs: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                ParserEvent::KeyboardProtocolQuery(q) => Some(*q),
                _ => None,
            })
            .collect();
        assert_eq!(
            qs,
            vec![
                KeyboardProtocolQuery::PrimaryDeviceAttributes,
                KeyboardProtocolQuery::SecondaryDeviceAttributes,
                KeyboardProtocolQuery::TertiaryDeviceAttributes,
            ]
        );
    }

    #[test]
    fn osc133_command_lifecycle() {
        let mut p = Parser::new();
        let mut events = Vec::new();
        p.feed(b"\x1b]133;A\x07\x1b]133;C\x07\x1b]133;D;0\x07", &mut events);
        let kinds: Vec<_> = events
            .iter()
            .map(|e| match e {
                ParserEvent::PromptStart => "A",
                ParserEvent::CommandStart(_) => "C",
                ParserEvent::CommandEnd { .. } => "D",
                _ => "?",
            })
            .collect();
        assert_eq!(kinds, vec!["A", "C", "D"]);

        // The bare FinalTerm form carries no metadata, and that must stay
        // distinguishable from a shell whose metadata failed to decode.
        assert_eq!(events[1], ParserEvent::CommandStart(CommandMeta::default()));
        assert_eq!(
            events[2],
            ParserEvent::CommandEnd {
                exit: Some(0),
                meta: CommandMeta::default(),
            }
        );
    }

    #[test]
    fn private_agent_integration_ready_marker_is_strict_and_hidden() {
        let token = "0123456789abcdef0123456789abcdef";
        assert_eq!(
            only_event(format!("\x1b]7771;{token}\x07").as_bytes()),
            ParserEvent::AgentIntegrationReady(token.to_string())
        );

        for invalid in [
            "short",
            "0123456789abcdef0123456789abcdeg",
            "0123456789abcdef0123456789abcdef0",
        ] {
            let mut parser = Parser::new();
            let mut events = Vec::new();
            parser.feed(format!("\x1b]7771;{invalid}\x07").as_bytes(), &mut events);
            assert!(events.is_empty(), "accepted invalid token {invalid:?}");
        }
    }

    fn only_event(bytes: &[u8]) -> ParserEvent {
        let mut p = Parser::new();
        let mut events = Vec::new();
        p.feed(bytes, &mut events);
        assert_eq!(events.len(), 1, "{events:?}");
        events.pop().expect("one event")
    }

    /// Twin test: the byte literal is jsh's own, copied from the assertion in
    /// `jsh/src/osc.rs` that pins what `command_output_start_packet` emits. If
    /// either end changes the packet format, this reddens.
    #[test]
    fn osc133_start_decodes_the_packet_jsh_actually_emits() {
        let event = only_event(b"\x1b]133;C;id=jsh-1;cmd_truncated=1;cwd_url=%2Ftmp\x07");
        assert_eq!(
            event,
            ParserEvent::CommandStart(CommandMeta {
                id: Some("jsh-1".to_string()),
                command: None,
                cwd: Some("/tmp".to_string()),
                duration_ms: None,
                // The shell had a command line and dropped it for size. Telling
                // that apart from "sends no metadata" is the whole point.
                command_truncated: true,
            })
        );
    }

    #[test]
    fn osc133_end_keeps_the_positional_exit_and_the_metadata() {
        let event =
            only_event(b"\x1b]133;D;127;id=jsh-2;duration_ms=42;cwd_url=%2Ftmp%2Fa%3Bb\x07");
        assert_eq!(
            event,
            ParserEvent::CommandEnd {
                exit: Some(127),
                meta: CommandMeta {
                    id: Some("jsh-2".to_string()),
                    command: None,
                    // `;` is percent-escaped by jsh precisely so it cannot forge
                    // a field boundary; it must come back as data.
                    cwd: Some("/tmp/a;b".to_string()),
                    duration_ms: Some(42),
                    command_truncated: false,
                },
            }
        );
    }

    #[test]
    fn osc133_start_decodes_a_percent_encoded_command_line() {
        let event = only_event(
            b"\x1b]133;C;id=jsh-3;cmdline_url=printf%20%27a%3Bb%2Bc%27%0A%E9%9B%AA;cwd_url=%2Ftmp\x07",
        );
        let ParserEvent::CommandStart(meta) = event else {
            panic!("expected CommandStart");
        };
        // A multiline command cannot be displayed or replayed as one reviewed
        // prompt line without changing its semantics, so the unsafe command
        // field is dropped while independent metadata still survives.
        assert_eq!(meta.command, None);
        assert_eq!(meta.cwd.as_deref(), Some("/tmp"));
    }

    /// A missing or unparseable status is `None`, not success. It used to
    /// `unwrap_or(0)`, so a command of unknown outcome was reported as having
    /// succeeded -- and an exit-code badge showed a green 0.
    #[test]
    fn osc133_end_without_a_status_is_unknown_rather_than_zero() {
        assert_eq!(
            only_event(b"\x1b]133;D\x07"),
            ParserEvent::CommandEnd {
                exit: None,
                meta: CommandMeta::default()
            }
        );
        assert_eq!(
            only_event(b"\x1b]133;D;not-a-number\x07"),
            ParserEvent::CommandEnd {
                exit: None,
                meta: CommandMeta::default()
            }
        );
        // Metadata with no positional status must not be eaten as the status.
        assert_eq!(
            only_event(b"\x1b]133;D;id=jsh-4\x07"),
            ParserEvent::CommandEnd {
                exit: None,
                meta: CommandMeta {
                    id: Some("jsh-4".to_string()),
                    ..CommandMeta::default()
                }
            }
        );
        // A signal-terminated command reports 128+n, which must survive.
        assert_eq!(
            only_event(b"\x1b]133;D;143\x07"),
            ParserEvent::CommandEnd {
                exit: Some(143),
                meta: CommandMeta::default()
            }
        );
    }

    #[test]
    fn osc133_drops_a_field_it_cannot_decode_but_keeps_the_mark() {
        // Truncated escape, oversized value, and invalid UTF-8 each yield None
        // for that field only: half-decoded text becomes a path and a journal
        // key, so guessing is worse than admitting ignorance.
        let ParserEvent::CommandStart(meta) = only_event(b"\x1b]133;C;id=jsh-5;cwd_url=%2\x07")
        else {
            panic!("expected CommandStart");
        };
        assert_eq!(meta.id.as_deref(), Some("jsh-5"));
        assert_eq!(meta.cwd, None);

        let ParserEvent::CommandStart(meta) = only_event(b"\x1b]133;C;cwd_url=%FF%FE\x07") else {
            panic!("expected CommandStart");
        };
        assert_eq!(meta.cwd, None);

        let oversized = format!(
            "\x1b]133;C;cwd_url={}\x07",
            "x".repeat(MAX_OSC133_CWD_BYTES + 1)
        );
        let ParserEvent::CommandStart(meta) = only_event(oversized.as_bytes()) else {
            panic!("expected CommandStart");
        };
        assert_eq!(meta.cwd, None);
    }

    /// An id becomes a lookup key and is shown to the user, so a control
    /// character in it is refused outright rather than carried around.
    #[test]
    fn osc133_refuses_a_control_bearing_id() {
        let ParserEvent::CommandStart(meta) =
            only_event(b"\x1b]133;C;id=jsh%3A7%3B%1B%07;cwd_url=%2Ftmp\x07")
        else {
            panic!("expected CommandStart");
        };
        assert_eq!(meta.id, None);
        assert_eq!(meta.cwd.as_deref(), Some("/tmp"), "the mark still lands");
    }

    #[test]
    fn osc133_refuses_visual_spoofing_per_field_but_keeps_the_mark() {
        let ParserEvent::CommandStart(meta) = only_event(
            b"\x1b]133;C;id=jsh%E2%80%AE7;cmdline_url=echo%C2%A0hidden;cwd_url=%2Ftmp%E2%80%83dir;duration_ms=9\x07",
        ) else {
            panic!("expected CommandStart");
        };
        assert_eq!(meta.id, None);
        assert_eq!(meta.command, None);
        assert_eq!(meta.cwd, None);
        assert_eq!(meta.duration_ms, Some(9), "independent metadata survives");
    }

    #[test]
    fn osc133_accepts_the_key_aliases_ember_already_read() {
        let ParserEvent::CommandStart(meta) =
            only_event(b"\x1b]133;C;execution_id=x1;command=ls%20-la;cwd=%2Fsrv;duration=7\x07")
        else {
            panic!("expected CommandStart");
        };
        assert_eq!(meta.id.as_deref(), Some("x1"));
        assert_eq!(meta.command.as_deref(), Some("ls -la"));
        assert_eq!(meta.cwd.as_deref(), Some("/srv"));
        assert_eq!(meta.duration_ms, Some(7));
    }

    #[test]
    fn osc7_cwd_passes_through_to_vte() {
        let mut p = Parser::new();
        let mut events = Vec::new();
        p.feed(b"\x1b]7;file://host/home/me/dir\x07", &mut events);
        assert_eq!(
            collect_bytes(&events),
            b"\x1b]7;file://host/home/me/dir\x1b\\"
        );
    }

    #[test]
    fn osc7_and_title_updates_cannot_spoof_frontend_chrome() {
        let mut p = Parser::new();
        let mut events = Vec::new();
        p.feed(
            "\x1b]7;file://host/tmp/left\u{202e}right\x07".as_bytes(),
            &mut events,
        );
        p.feed(b"\x1b]7;file://host/tmp/left%0Aright\x07", &mut events);
        p.feed(
            b"\x1b]7;file://host/tmp/left%E2%80%AEright\x07",
            &mut events,
        );
        p.feed(b"\x1b]7;file://host/tmp/invalid%FFutf8\x07", &mut events);
        p.feed(b"\x1b]7;file://host/tmp/incomplete%2\x07", &mut events);
        p.feed("\x1b]2;build\u{00a0}done\x07".as_bytes(), &mut events);
        assert!(collect_bytes(&events).is_empty());

        events.clear();
        p.feed(b"\x1b]2;safe title\x07", &mut events);
        assert_eq!(collect_bytes(&events), b"\x1b]2;safe title\x07");

        events.clear();
        let oversized_cwd = format!("\x1b]7;{}\x07", "x".repeat(MAX_OSC7_URI_BYTES + 1));
        p.feed(oversized_cwd.as_bytes(), &mut events);
        let oversized_title = format!("\x1b]2;{}\x07", "x".repeat(MAX_TITLE_BYTES + 1));
        p.feed(oversized_title.as_bytes(), &mut events);
        assert!(collect_bytes(&events).is_empty());
    }

    #[test]
    fn mouse_reporting_dropped_when_disabled_but_event_emitted() {
        let mut p = Parser::with_config(ParserConfig {
            mouse_reporting: false,
            focus_reporting: true,
        });
        let mut events = Vec::new();
        p.feed(b"\x1b[?1000h", &mut events);
        assert!(collect_bytes(&events).is_empty());
        assert!(events.iter().any(|e| matches!(
            e,
            ParserEvent::DecsetMode {
                mode: 1000,
                set: true
            }
        )));
    }

    #[test]
    fn osc_7770_emits_remote_session_id() {
        let mut p = Parser::new();
        let mut events = Vec::new();
        p.feed(b"\x1b]7770;home-main\x1b\\", &mut events);
        let id = events.iter().find_map(|e| match e {
            ParserEvent::RemoteSessionId(s) => Some(s.clone()),
            _ => None,
        });
        assert_eq!(id.as_deref(), Some("home-main"));
    }

    #[test]
    fn osc_7770_empty_payload_ignored() {
        let mut p = Parser::new();
        let mut events = Vec::new();
        p.feed(b"\x1b]7770;\x07", &mut events);
        assert!(events
            .iter()
            .all(|e| !matches!(e, ParserEvent::RemoteSessionId(_))));
    }

    #[test]
    fn osc_7770_rejects_non_jsh_identifiers() {
        for payload in [
            b"\x1b]7770;line\nbreak\x07".to_vec(),
            b"\x1b]7770;nul\0byte\x07".to_vec(),
            "\x1b]7770;left\u{202e}right\x07".as_bytes().to_vec(),
            "\x1b]7770;left\u{00a0}right\x07".as_bytes().to_vec(),
            b"\x1b]7770; leading-space\x07".to_vec(),
            b"\x1b]7770;contains.dot\x07".to_vec(),
        ] {
            let mut parser = Parser::new();
            let mut events = Vec::new();
            parser.feed(&payload, &mut events);
            assert!(events
                .iter()
                .all(|event| !matches!(event, ParserEvent::RemoteSessionId(_))));
        }

        let mut payload = b"\x1b]7770;".to_vec();
        payload.extend(std::iter::repeat_n(b'x', 129));
        payload.push(0x07);
        let mut parser = Parser::new();
        let mut events = Vec::new();
        parser.feed(&payload, &mut events);
        assert!(events
            .iter()
            .all(|event| !matches!(event, ParserEvent::RemoteSessionId(_))));
    }
}
