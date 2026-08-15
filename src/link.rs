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
    if !matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https") {
        return false;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    // An empty authority is `http:///path`, which resolves relative to the
    // opener's idea of a default host rather than to anything the target
    // names. Userinfo would hand the opener a credential.
    !authority.is_empty() && !authority.contains('@')
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
            // Userinfo would hand a credential to the opener.
            "https://user:token@example.com/",
            "https://user@example.com/",
            // Ambiguous or invisible characters.
            "https://exam\u{200b}ple.com/",
            "https://example.com/\u{202e}path",
            "https://example.com/a b",
            "https://example.com/line\nbreak",
            "https://example.com\\evil",
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
            "https://{}",
            "a".repeat(MAX_OPENABLE_URL_BYTES - "https://".len())
        );
        assert_eq!(longest_allowed.len(), MAX_OPENABLE_URL_BYTES);
        assert!(is_openable_url(&longest_allowed));
    }
}
