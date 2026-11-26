//! Profiled OSC5522 capability and pending-request owners. Native adapters supply
//! codecs, entropy and a lazy monotonic clock; only the UI performs clipboard IO.
use std::collections::HashSet;

pub const MAX_PENDING_CLIPBOARD_REQUESTS: usize = 8;
pub const MAX_EMBER_MIME_TYPES: usize = 64;
pub const MAX_EMBER_MIME_LEN: usize = 256;
const EPERM: &str = "type=read:status=EPERM";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EmberReadKind {
    MimeData(String),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrostReadKind {
    MimeList,
    MimeData(String),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipboardReadRequest<K> {
    pub kind: K,
}
pub type EmberReadRequest = ClipboardReadRequest<EmberReadKind>;
pub type FrostReadRequest = ClipboardReadRequest<FrostReadKind>;

#[derive(Debug)]
pub struct PendingPasteGrant<D> {
    pub token: String,
    pub offered_mimes: HashSet<String>,
    pub expires_at: D,
}

#[derive(Debug)]
pub struct EmberOsc5522<D> {
    // Public storage is a transitional frozen-oracle inspection boundary.
    // Production admission/issuance/drain must use this owner's methods.
    pub pending_paste_grant: Option<PendingPasteGrant<D>>,
    pub pending_clipboard_requests: Vec<EmberReadRequest>,
}
impl<D> Default for EmberOsc5522<D> {
    fn default() -> Self {
        Self {
            pending_paste_grant: None,
            pending_clipboard_requests: Vec::new(),
        }
    }
}
impl<D: Ord> EmberOsc5522<D> {
    pub fn revoke(&mut self) {
        self.pending_paste_grant = None;
    }
    pub fn take_requests(&mut self) -> Vec<EmberReadRequest> {
        std::mem::take(&mut self.pending_clipboard_requests)
    }
    pub fn admit(
        &mut self,
        metadata: &str,
        payload: Option<&str>,
        enabled: bool,
        now: impl FnOnce() -> D,
        mut decode: impl FnMut(&str) -> Option<String>,
    ) -> Option<&'static str> {
        let (mut message_type, mut password, mut human_name) = (None, None, None);
        for part in metadata.split(':') {
            if let Some(value) = part.strip_prefix("type=") {
                message_type = Some(value);
            } else if let Some(value) = part.strip_prefix("pw=") {
                password = decode(value);
            } else if let Some(value) = part.strip_prefix("name=") {
                human_name = decode(value);
            }
        }
        if message_type != Some("read") {
            return None;
        }
        let Some(request) = payload.and_then(&mut decode) else {
            return Some(EPERM);
        };
        if request == "." {
            return Some(EPERM);
        }
        let mut mimes = request.split_ascii_whitespace();
        let Some(mime) = mimes.next() else {
            return Some(EPERM);
        };
        if mimes.next().is_some() || !valid_ember_mime(mime) {
            return Some(EPERM);
        }
        if !enabled {
            self.revoke();
            return Some(EPERM);
        }
        let Some(grant) = self.pending_paste_grant.as_ref() else {
            return Some(EPERM);
        };
        if now() >= grant.expires_at {
            self.revoke();
            return Some(EPERM);
        }
        if password.as_deref() != Some(grant.token.as_str())
            || human_name.as_deref() != Some("Paste event")
        {
            return Some(EPERM);
        }
        // Consume before MIME admission and queue capacity, exactly as Ember.
        let Some(grant) = self.pending_paste_grant.take() else {
            return Some(EPERM);
        };
        if !grant.offered_mimes.contains(mime) {
            return Some(EPERM);
        }
        if self.pending_clipboard_requests.len() >= MAX_PENDING_CLIPBOARD_REQUESTS {
            return Some("type=read:status=EBUSY");
        }
        self.pending_clipboard_requests.push(ClipboardReadRequest {
            kind: EmberReadKind::MimeData(mime.to_owned()),
        });
        None
    }
    pub fn issue_paste(
        &mut self,
        enabled: bool,
        mimes: &[String],
        issue: impl FnOnce() -> (String, D),
        encode: impl FnMut(&str) -> String,
    ) -> Vec<u8> {
        if !enabled {
            self.revoke();
            return Vec::new();
        }
        let mut seen = HashSet::new();
        let mimes: Vec<_> = mimes
            .iter()
            .filter(|s| valid_ember_mime(s))
            .filter(|s| seen.insert((*s).clone()))
            .take(MAX_EMBER_MIME_TYPES)
            .cloned()
            .collect();
        let (token, expires_at) = issue();
        self.pending_paste_grant = Some(PendingPasteGrant {
            token: token.clone(),
            offered_mimes: mimes.iter().cloned().collect(),
            expires_at,
        });
        paste_wire(&mimes, "pw", &token, encode)
    }
}
fn valid_ember_mime(mime: &str) -> bool {
    !mime.is_empty()
        && mime.len() <= MAX_EMBER_MIME_LEN
        && mime.is_ascii()
        && !mime
            .bytes()
            .any(|b| b.is_ascii_control() || b.is_ascii_whitespace())
}

#[derive(Debug, Default)]
pub struct FrostOsc5522 {
    pub pending_paste_password: Option<String>,
    pub pending_clipboard_requests: Vec<FrostReadRequest>,
}
impl FrostOsc5522 {
    pub fn take_requests(&mut self) -> Vec<FrostReadRequest> {
        std::mem::take(&mut self.pending_clipboard_requests)
    }
    pub fn admit(
        &mut self,
        metadata: &str,
        mut decode: impl FnMut(&str) -> Option<String>,
        mut spoof: impl FnMut(char) -> bool,
    ) -> Option<&'static str> {
        let (mut message_type, mut mime, mut password) = (None, None, None);
        for part in metadata.split(':') {
            if let Some(value) = part.strip_prefix("type=") {
                message_type = Some(value);
            } else if let Some(value) = part.strip_prefix("mime=") {
                mime = decode(value);
            } else if let Some(value) = part.strip_prefix("password=") {
                password = decode(value);
            } else if let Some(value) = part.strip_prefix("pw=") {
                password = decode(value);
            }
        }
        if message_type != Some("read") {
            return None;
        }
        let kind = if let Some(mime) = mime {
            if mime.is_empty()
                || mime.len() > 128
                || mime
                    .chars()
                    .any(|c| !c.is_ascii() || c.is_ascii_control() || c == '\u{fffd}' || spoof(c))
            {
                return Some("type=read:status=ENOSYS");
            }
            if let Some(expected) = &self.pending_paste_password {
                if password.as_deref() != Some(expected.as_str()) {
                    return Some(EPERM);
                }
            }
            self.pending_paste_password = None;
            FrostReadKind::MimeData(mime)
        } else {
            FrostReadKind::MimeList
        };
        // Frost silently drops overload, retaining its legacy policy.
        if self.pending_clipboard_requests.len() < MAX_PENDING_CLIPBOARD_REQUESTS {
            self.pending_clipboard_requests
                .push(ClipboardReadRequest { kind });
        }
        None
    }
    pub fn issue_paste(
        &mut self,
        mimes: &[String],
        token: String,
        encode: impl FnMut(&str) -> String,
    ) -> Vec<u8> {
        self.pending_paste_password = Some(token.clone());
        paste_wire(mimes, "password", &token, encode)
    }
}
fn paste_wire(
    mimes: &[String],
    key: &str,
    token: &str,
    mut encode: impl FnMut(&str) -> String,
) -> Vec<u8> {
    let mut out = b"\x1b]5522;type=read:status=OK:".to_vec();
    out.extend_from_slice(key.as_bytes());
    out.push(b'=');
    out.extend_from_slice(encode(token).as_bytes());
    out.extend_from_slice(b"\x1b\\");
    for mime in mimes {
        out.extend_from_slice(b"\x1b]5522;type=read:status=DATA:mime=");
        out.extend_from_slice(encode(mime).as_bytes());
        out.extend_from_slice(b"\x1b\\");
    }
    out.extend_from_slice(b"\x1b]5522;type=read:status=DONE\x1b\\");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    fn decode(s: &str) -> Option<String> {
        (s != "bad-codec").then(|| s.to_owned())
    }
    fn grant() -> EmberOsc5522<u64> {
        let mut s = EmberOsc5522::default();
        s.issue_paste(
            true,
            &["text/plain".into()],
            || ("token".into(), 10),
            str::to_owned,
        );
        s
    }
    #[test]
    fn ember_capability_is_consumed_before_mime_and_capacity_admission() {
        let metadata = "type=read:pw=token:name=Paste event";
        let mut s = grant();
        assert_eq!(
            s.admit(metadata, Some("image/png"), true, || 1, decode),
            Some(EPERM)
        );
        assert!(s.pending_paste_grant.is_none());
        let mut s = grant();
        for _ in 0..8 {
            s.pending_clipboard_requests.push(ClipboardReadRequest {
                kind: EmberReadKind::MimeData("old".into()),
            });
        }
        assert_eq!(
            s.admit(metadata, Some("text/plain"), true, || 1, decode),
            Some("type=read:status=EBUSY")
        );
        assert!(s.pending_paste_grant.is_none());
        assert_eq!(s.take_requests().len(), 8);
    }
    #[test]
    fn ember_rejects_do_not_eagerly_expire_or_consume_a_grant() {
        let mut s = grant();
        assert_eq!(
            s.admit(
                "type=read",
                Some("."),
                false,
                || panic!("clock must stay lazy"),
                decode
            ),
            Some(EPERM)
        );
        assert!(s.pending_paste_grant.is_some());
        assert_eq!(
            s.admit(
                "type=read:pw=wrong:name=Paste event",
                Some("text/plain"),
                true,
                || 1,
                decode
            ),
            Some(EPERM)
        );
        assert!(s.pending_paste_grant.is_some());
        assert_eq!(
            s.admit("type=read", Some("text/plain"), true, || 10, decode),
            Some(EPERM)
        );
        assert!(s.pending_paste_grant.is_none());
        let mut s = grant();
        assert!(s
            .issue_paste(
                false,
                &[],
                || panic!("no token while disabled"),
                str::to_owned
            )
            .is_empty());
        assert!(s.pending_paste_grant.is_none());
    }
    #[test]
    fn frost_retains_legacy_list_alias_and_silent_overload_policy() {
        let mut s = FrostOsc5522::default();
        s.issue_paste(&[], "token".into(), str::to_owned);
        assert_eq!(s.admit("type=read", decode, |_| false), None);
        assert_eq!(s.pending_paste_password.as_deref(), Some("token"));
        assert_eq!(
            s.admit("type=read:mime=text plain:pw=wrong", decode, |_| false),
            Some(EPERM)
        );
        assert_eq!(
            s.admit(
                "type=read:mime=text plain:password=wrong:pw=token",
                decode,
                |_| false
            ),
            None
        );
        assert!(s.pending_paste_password.is_none());
        for _ in 0..20 {
            assert_eq!(s.admit("type=read", decode, |_| false), None);
        }
        assert_eq!(s.take_requests().len(), 8);
        assert_eq!(s.admit("type=read:mime=bad-codec", decode, |_| false), None);
        assert_eq!(s.take_requests()[0].kind, FrostReadKind::MimeList);
    }
    #[test]
    fn paste_wire_and_mime_profiles_remain_distinct() {
        let mimes = vec![
            "text/plain".into(),
            "text/plain".into(),
            "text plain".into(),
            "image/png".into(),
        ];
        let mut e = EmberOsc5522::<u64>::default();
        let out = e.issue_paste(true, &mimes, || ("token".into(), 10), str::to_owned);
        assert_eq!(out, b"\x1b]5522;type=read:status=OK:pw=token\x1b\\\x1b]5522;type=read:status=DATA:mime=text/plain\x1b\\\x1b]5522;type=read:status=DATA:mime=image/png\x1b\\\x1b]5522;type=read:status=DONE\x1b\\");
        let mut f = FrostOsc5522::default();
        let out = f.issue_paste(&mimes, "token".into(), str::to_owned);
        assert!(out.starts_with(b"\x1b]5522;type=read:status=OK:password=token"));
        assert_eq!(
            String::from_utf8(out)
                .unwrap()
                .matches("status=DATA")
                .count(),
            4
        );
    }
}
