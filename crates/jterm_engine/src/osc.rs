//! Shared OSC framing, native admission and effect parameters.
use crate::command_blocks::{ShellMarkerClock, ShellMarkerPosition};
use crate::shell_markers::is_valid_jsh_session_id;

pub const BEL: &[u8] = b"\x07";
pub const ST: &[u8] = b"\x1b\\";
pub const MAX_NOTIFICATION_CHARS: usize = 256;
const PENDING_NOTIFICATION_LIMIT: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OscCode {
    Title(&'static str),
    Color(&'static str),
    Directory,
    Hyperlink,
    Notification,
    RxvtNotification,
    Clipboard,
    ClipboardExtension,
    ShellMarker,
    JshSession,
    Unsupported,
}

/// One complete scanner packet. Keeping the value bytes borrowed lets a
/// profile recognize its command before deciding how to handle invalid UTF-8.
#[derive(Clone, Copy)]
pub struct OscPacket<'a> {
    code: OscCode,
    value: Option<&'a [u8]>,
    payload_len: usize,
    bel: bool,
}

impl<'a> OscPacket<'a> {
    pub fn decode(payload: &'a [u8], bel: bool) -> Self {
        use OscCode::*;
        let (command, value) = match payload.iter().position(|&byte| byte == b';') {
            Some(index) => (&payload[..index], Some(&payload[index + 1..])),
            None => (payload, None),
        };
        let code = match command {
            b"0" => Title("0"),
            b"1" => Title("1"),
            b"2" => Title("2"),
            b"4" => Color("4"),
            b"7" => Directory,
            b"8" => Hyperlink,
            b"9" => Notification,
            b"10" => Color("10"),
            b"11" => Color("11"),
            b"12" => Color("12"),
            b"52" => Clipboard,
            b"104" => Color("104"),
            b"110" => Color("110"),
            b"111" => Color("111"),
            b"112" => Color("112"),
            b"133" => ShellMarker,
            b"777" => RxvtNotification,
            b"5522" => ClipboardExtension,
            b"7770" => JshSession,
            _ => Unsupported,
        };
        Self {
            code,
            value,
            bel,
            payload_len: payload.len(),
        }
    }

    pub fn code(self) -> OscCode {
        self.code
    }

    pub fn value_bytes(self) -> Option<&'a [u8]> {
        self.value
    }

    pub fn text(self) -> Option<&'a str> {
        std::str::from_utf8(self.value.unwrap_or_default()).ok()
    }

    pub fn payload_len(self) -> usize {
        self.payload_len
    }

    pub fn terminator(self) -> &'static [u8] {
        if self.bel {
            BEL
        } else {
            ST
        }
    }

    /// Advance the marker position only after the selected profile recognizes
    /// a marker. The byte range includes the whole original framed packet.
    pub fn marker_position(
        self,
        markers: &mut ShellMarkerClock,
        end: Option<u64>,
    ) -> Option<ShellMarkerPosition> {
        markers.osc(end, self.payload_len, self.bel)
    }

    /// Resolve a native packet after installing its wire terminator. Both
    /// native profiles remember the terminator even for rejected OSC input.
    pub fn native_event(
        self,
        profile: NativeOscProfile,
        reply_terminator: &mut &'static [u8],
        markers: &mut ShellMarkerClock,
        end: Option<u64>,
    ) -> Option<NativeOsc<'a>> {
        *reply_terminator = self.terminator();
        let value = self.text()?;
        Some(match self.code {
            OscCode::Title(command) => NativeOsc::Title(command, value),
            OscCode::Color(command) => NativeOsc::Color(command, value),
            OscCode::Directory => NativeOsc::Directory(value),
            OscCode::Hyperlink => NativeOsc::Hyperlink(value),
            OscCode::Notification => NativeOsc::Notification(Notification::AppTitle(value)),
            OscCode::RxvtNotification => {
                let fields = value.strip_prefix("notify;")?;
                let (title, body) = fields.split_once(';').unwrap_or((fields, ""));
                NativeOsc::Notification(Notification::CustomTitle { title, body })
            }
            OscCode::Clipboard => NativeOsc::Clipboard(value),
            OscCode::ClipboardExtension => match value.split_once(';') {
                Some((metadata, data)) => NativeOsc::ClipboardExtension(metadata, Some(data)),
                None => NativeOsc::ClipboardExtension(value, None),
            },
            OscCode::ShellMarker => {
                let position = match value.split(';').next() {
                    Some("A" | "B" | "C" | "D") => self.marker_position(markers, end),
                    _ => None,
                };
                NativeOsc::ShellMarker(value, position)
            }
            OscCode::JshSession
                if profile == NativeOscProfile::Ember && is_valid_jsh_session_id(value) =>
            {
                NativeOsc::JshSession(value)
            }
            OscCode::Unsupported | OscCode::JshSession => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeOscProfile {
    Ember,
    Frost,
}

#[derive(Debug, PartialEq, Eq)]
pub enum NativeOsc<'a> {
    Title(&'static str, &'a str),
    Color(&'static str, &'a str),
    Directory(&'a str),
    Hyperlink(&'a str),
    Notification(Notification<'a>),
    Clipboard(&'a str),
    ClipboardExtension(&'a str, Option<&'a str>),
    ShellMarker(&'a str, Option<ShellMarkerPosition>),
    JshSession(&'a str),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Notification<'a> {
    AppTitle(&'a str),
    CustomTitle { title: &'a str, body: &'a str },
}

/// Bound content before it leaves the terminal. The caller supplies its
/// visual-spoofing predicate since presentation policy belongs to the host.
pub fn bounded_notification_field(raw: &str, spoof: fn(char) -> bool) -> String {
    raw.chars()
        .map(|ch| {
            if ch.is_control() || spoof(ch) {
                '\u{fffd}'
            } else {
                ch
            }
        })
        .take(MAX_NOTIFICATION_CHARS)
        .collect::<String>()
        .trim()
        .to_owned()
}

impl Notification<'_> {
    /// The engine admits bounded effects into the host's drain queue. A
    /// rewritten field is never delivered to trusted desktop chrome.
    pub fn enqueue(
        self,
        queue: &mut Vec<(String, String)>,
        app_title: impl FnOnce() -> String,
        spoof: fn(char) -> bool,
    ) {
        if queue.len() >= PENDING_NOTIFICATION_LIMIT {
            return;
        }
        let check_title = matches!(self, Self::CustomTitle { .. });
        let (title, body) = match self {
            Self::AppTitle(body) => (app_title(), body),
            Self::CustomTitle { title, body } => {
                let title = bounded_notification_field(title, spoof);
                (if title.is_empty() { app_title() } else { title }, body)
            }
        };
        let body = bounded_notification_field(body, spoof);
        // OSC 9 has no PTY-authored title. Preserve the application's title
        // even if it deliberately contains a replacement character.
        if !body.contains('\u{fffd}') && (!check_title || !title.contains('\u{fffd}')) {
            queue.push((title, body));
        }
    }
}

/// Append one OSC 5522 response using the terminator observed on ingress.
pub fn append_clipboard_status(
    output: &mut Vec<u8>,
    terminator: &[u8],
    metadata: &str,
    data: Option<&str>,
) {
    output.extend_from_slice(b"\x1b]5522;");
    output.extend_from_slice(metadata.as_bytes());
    if let Some(data) = data {
        output.push(b';');
        output.extend_from_slice(data.as_bytes());
    }
    output.extend_from_slice(terminator);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_osc_keeps_exact_command_and_field_admission() {
        let cases: &[(&[u8], Option<NativeOsc<'_>>)] = &[
            (b"0", Some(NativeOsc::Title("0", ""))),
            (b"104", Some(NativeOsc::Color("104", ""))),
            (b"111;ignored", Some(NativeOsc::Color("111", "ignored"))),
            (b"08;;target", None),
            (b"8;\xff", None),
            (
                b"7;file://remote/path",
                Some(NativeOsc::Directory("file://remote/path")),
            ),
            (b"777;notify", None),
            (
                b"777;notify;",
                Some(NativeOsc::Notification(Notification::CustomTitle {
                    title: "",
                    body: "",
                })),
            ),
            (
                b"777;notify;t;a;b",
                Some(NativeOsc::Notification(Notification::CustomTitle {
                    title: "t",
                    body: "a;b",
                })),
            ),
            (b"5522;m", Some(NativeOsc::ClipboardExtension("m", None))),
            (
                b"5522;m;",
                Some(NativeOsc::ClipboardExtension("m", Some(""))),
            ),
            (
                b"5522;;a;b",
                Some(NativeOsc::ClipboardExtension("", Some("a;b"))),
            ),
            (
                b"133;Ax;meta",
                Some(NativeOsc::ShellMarker("Ax;meta", None)),
            ),
            (b"7770;trim me", None),
        ];
        for profile in [NativeOscProfile::Ember, NativeOscProfile::Frost] {
            let mut markers = ShellMarkerClock::default();
            for (index, (payload, expected)) in cases.iter().enumerate() {
                let mut wire = ST;
                let packet = OscPacket::decode(payload, index % 2 == 0);
                let event = packet.native_event(profile, &mut wire, &mut markers, None);
                assert_eq!(&event, expected, "{profile:?} {payload:?}");
                assert_eq!(wire, if index % 2 == 0 { BEL } else { ST });
            }
            let mut wire = BEL;
            let valid = OscPacket::decode(b"7770;explicit-id", false);
            let event = valid.native_event(profile, &mut wire, &mut markers, None);
            assert_eq!(
                event,
                (profile == NativeOscProfile::Ember)
                    .then_some(NativeOsc::JshSession("explicit-id"))
            );
        }
        let no_separator = OscPacket::decode(b"8", false);
        assert_eq!(no_separator.value_bytes(), None);
        assert_eq!(no_separator.text(), Some(""));
        let malformed = OscPacket::decode(b"8;\xff", false);
        assert_eq!(malformed.code(), OscCode::Hyperlink);
        assert_eq!(malformed.text(), None);
    }

    #[test]
    fn marker_positions_use_complete_framed_ranges_and_keep_sequence() {
        let mut clock = ShellMarkerClock::default();
        assert_eq!(clock.begin_feed(5), Some(0));
        assert_eq!(clock.begin_feed(40), Some(5));
        let mut wire = ST;
        let bad = OscPacket::decode(b"133;b", true);
        assert_eq!(
            bad.native_event(NativeOscProfile::Ember, &mut wire, &mut clock, Some(10)),
            Some(NativeOsc::ShellMarker("b", None))
        );
        for (payload, bel, end, sequence) in [
            (b"133;C;id=abc".as_slice(), true, 25, 1),
            (b"133;D;0".as_slice(), false, 39, 2),
        ] {
            let event = OscPacket::decode(payload, bel).native_event(
                NativeOscProfile::Frost,
                &mut wire,
                &mut clock,
                Some(end),
            );
            let Some(NativeOsc::ShellMarker(_, Some(position))) = event else {
                panic!("marker without position")
            };
            assert_eq!(position.sequence(), sequence);
            assert_eq!(
                position.packet_bytes(),
                end - payload.len() as u64 - if bel { 3 } else { 4 }..end
            );
        }
    }

    #[test]
    fn notifications_and_clipboard_status_preserve_bounded_wire_effects() {
        let spoof = |ch| ch == '\u{202e}';
        let mut queue = vec![];
        let app_title = || "App".to_owned();
        Notification::CustomTitle {
            title: "  ",
            body: "  ready  ",
        }
        .enqueue(&mut queue, app_title, spoof);
        Notification::AppTitle("wrong\u{202e}order").enqueue(&mut queue, app_title, spoof);
        Notification::CustomTitle {
            title: "\u{fffd}",
            body: "ignored",
        }
        .enqueue(&mut queue, app_title, spoof);
        assert_eq!(queue, [("App".into(), "ready".into())]);
        let long = "文".repeat(MAX_NOTIFICATION_CHARS + 3);
        for _ in 0..10 {
            Notification::AppTitle(&long).enqueue(&mut queue, app_title, spoof);
        }
        assert_eq!(queue.len(), 8);
        assert_eq!(queue[7].1.chars().count(), MAX_NOTIFICATION_CHARS);

        let mut wire = b"earlier".to_vec();
        append_clipboard_status(&mut wire, BEL, "type=read:status=DATA", Some(""));
        append_clipboard_status(&mut wire, ST, "type=read:status=DONE", None);
        assert_eq!(
            wire,
            b"earlier\x1b]5522;type=read:status=DATA;\x07\x1b]5522;type=read:status=DONE\x1b\\"
        );
    }
}
