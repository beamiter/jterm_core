//! The one opener policy every clickable target must satisfy.
//!
//! A link's text comes from the attached process, so a click is the terminal
//! acting on untrusted data. Only an absolute HTTP(S) URL with an authority
//! and no userinfo qualifies: `file:` would open a local file with its
//! default application, `ssh:` and `git:` would start a network client, and
//! `https://user:token@host` would hand the opener a credential the user
//! never typed. Whitespace, controls, backslashes, and visually ambiguous
//! characters are refused so the target reads as the origin it resolves to.

/// Clickable-target text is terminal-controlled data, not a bulk payload;
/// this ceiling also bounds the allocation a frontend makes per candidate.
pub const MAX_OPENABLE_URL_BYTES: usize = 2 * 1024;

/// Whether `url` may be handed to the system opener.
pub fn is_openable_url(url: &str) -> bool {
    if url.is_empty()
        || url.len() > MAX_OPENABLE_URL_BYTES
        || url
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
        || url.contains('\\')
        || crate::review_input::contains_visual_spoofing(url)
    {
        return false;
    }
    let Some((scheme, rest)) = url.split_once("://") else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return false;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    // An empty authority is `http:///path`, which resolves relative to the
    // opener's idea of a default host rather than to anything the target
    // names. Userinfo would hand the opener a credential.
    if authority.contains('@') {
        return false;
    }
    authority_host(authority).is_some_and(is_valid_url_host)
}

/// Return the host only when the complete authority has an unambiguous
/// bracket and port shape. URL libraries disagree on malformed IPv6 and port
/// spellings, so an opener boundary must not validate a different authority
/// from the one the desktop handler later resolves.
pub(crate) fn authority_host(authority: &str) -> Option<&str> {
    if authority.is_empty() {
        return None;
    }
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, suffix) = rest.split_once(']')?;
        if host.parse::<std::net::Ipv6Addr>().is_err() {
            return None;
        }
        if !suffix.is_empty() && !suffix.strip_prefix(':').is_some_and(is_port) {
            return None;
        }
        return Some(host);
    }
    if authority.contains(['[', ']']) {
        return None;
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') && !host.is_empty() && is_port(port) => {
            Some(host)
        }
        Some(_) => None,
        None => Some(authority),
    }
}

fn is_port(port: &str) -> bool {
    !port.is_empty()
        && port.bytes().all(|byte| byte.is_ascii_digit())
        && port.parse::<u16>().is_ok_and(|port| port != 0)
}

/// Accept syntactically valid IP literals and conservative ASCII DNS names. Requiring
/// punycode for IDNs and refusing encoded/numeric host aliases keeps the host
/// displayed by the terminal identical to the origin interpreted by common
/// URL handlers.
pub(crate) fn is_valid_url_host(host: &str) -> bool {
    if host.parse::<std::net::Ipv4Addr>().is_ok() || host.parse::<std::net::Ipv6Addr>().is_ok() {
        return true;
    }
    if host.is_empty() || host.len() > 253 || !host.is_ascii() {
        return false;
    }

    let mut labels = host.split('.').peekable();
    while let Some(label) = labels.next() {
        if label.is_empty()
            || label.len() > 63
            || !label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            || !label
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
            || !label
                .as_bytes()
                .last()
                .is_some_and(u8::is_ascii_alphanumeric)
        {
            return false;
        }
        if labels.peek().is_none() && is_url_ipv4_number(label) {
            return false;
        }
    }
    true
}

/// Whether a validated host literal/name resolves only to the local machine.
///
/// This intentionally recognizes the complete IPv4 loopback block, not just
/// `127.0.0.1`, and the canonical IPv6 loopback address. Numeric aliases such
/// as `2130706433` never reach this helper because [`is_valid_url_host`]
/// rejects them.
pub(crate) fn is_loopback_url_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

fn is_url_ipv4_number(label: &str) -> bool {
    if let Some(hex) = label
        .strip_prefix("0x")
        .or_else(|| label.strip_prefix("0X"))
    {
        return !hex.is_empty() && hex.bytes().all(|byte| byte.is_ascii_hexdigit());
    }
    label.bytes().all(|byte| byte.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_http_targets_with_an_authority_are_openable() {
        assert!(is_openable_url("https://example.com/path"));
        assert!(is_openable_url("https://example.com/a?b=c#d"));
        assert!(is_openable_url("HTTP://example.com"));
        assert!(is_openable_url("http://example.com"));
        assert!(is_openable_url("https://api-1.example:8443/path"));
        assert!(is_openable_url("http://127.0.0.1:8080/"));
        assert!(is_openable_url("https://[2001:db8::1]/"));
        assert!(is_openable_url("https://xn--bcher-kva.example/"));

        for rejected in [
            // Schemes that would start a client or open a local file.
            "file:///etc/passwd",
            "ssh://host.example/path",
            "git://host.example/repo",
            "mailto:person@example.com",
            "javascript:alert(1)",
            "data:text/html,hello",
            // Not an absolute URL at all.
            "relative/path",
            "https:/example.com",
            // No authority: resolves against the opener's default, not the
            // origin the target appears to name.
            "https:///path",
            "https://:443/path",
            "https://example.com:/path",
            "https://example.com:not-a-port/path",
            "https://example.com:65536/path",
            "https://example.com:0/path",
            "https://example.com:443:444/path",
            "https://[::1/path",
            "https://[::1]suffix/path",
            "https://::1/path",
            // Userinfo would hand a credential to the opener.
            "https://user:token@example.com/",
            "https://user@example.com/",
            // Ambiguous or invisible characters.
            "https://exam\u{200b}ple.com/",
            "https://example.com/\u{202e}path",
            "https://example.com/a b",
            "https://example.com/line\nbreak",
            "https://example.com\\evil",
            // Host spellings whose interpretation varies across URL parsers.
            "https://%65xample.com/",
            "https://example..com/",
            "https://-example.com/",
            "https://example-.com/",
            "https://exa_mple.com/",
            "https://example.com./",
            "https://2130706433/",
            "https://0x7f000001/",
            "https://127.1/",
            "https://999.1.1.1/",
            "https://例子.example/",
        ] {
            assert!(
                !is_openable_url(rejected),
                "{rejected:?} must not be openable"
            );
        }
    }

    #[test]
    fn the_byte_ceiling_fails_closed() {
        let at_limit = format!("https://safe.test/{}", "x".repeat(MAX_OPENABLE_URL_BYTES));
        assert!(at_limit.len() > MAX_OPENABLE_URL_BYTES);
        assert!(!is_openable_url(&at_limit));

        let longest_allowed = format!(
            "https://safe.test/{}",
            "a".repeat(MAX_OPENABLE_URL_BYTES - "https://safe.test/".len())
        );
        assert_eq!(longest_allowed.len(), MAX_OPENABLE_URL_BYTES);
        assert!(is_openable_url(&longest_allowed));
    }
}
