use unicode_segmentation::GraphemeCursor;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub(crate) const MAX_CLUSTER_BYTES: usize = 64;
fn regional_indicator(ch: char) -> bool {
    ('\u{1f1e6}'..='\u{1f1ff}').contains(&ch)
}

/// Only keep a reversible narrow write for bases that can widen in this profile.
pub(crate) fn may_grow(first: char) -> bool {
    if first.is_ascii() {
        return matches!(first, '#' | '*' | '0'..='9');
    }
    if regional_indicator(first) {
        return true;
    }
    selector_width(first, '\u{fe0f}') == 2
}

pub(crate) fn may_contract(first: char) -> bool {
    first.width() == Some(2) && selector_width(first, '\u{fe0e}') == 1
}

fn selector_width(first: char, selector: char) -> usize {
    let mut bytes = [0; 7];
    let len = first.encode_utf8(&mut bytes).len();
    let end = len + selector.encode_utf8(&mut bytes[len..]).len();
    std::str::from_utf8(&bytes[..end])
        .expect("UTF-8 scalar and selector")
        .width()
}

pub(crate) fn has_narrow_text_presentation(cluster: &str) -> bool {
    cluster.chars().nth(1) == Some('\u{fe0e}') && cluster.width() == 1
}

pub(crate) fn has_wide_emoji_presentation(cluster: &str) -> bool {
    let mut chars = cluster.chars();
    let (Some(first), Some(next)) = (chars.next(), chars.next()) else {
        return false;
    };
    (next == '\u{fe0f}' || (regional_indicator(first) && regional_indicator(next)))
        && cluster.width() == 2
}

/// The base is one retained grapheme (or one scalar). Test its boundary
/// with the incoming scalar using the full left context for emoji and Indic
/// rules. A genuine continuation may exceed the caller's retained size cap.
pub(crate) fn extension<'a>(
    base: &str,
    next: char,
    buf: &'a mut [u8; MAX_CLUSTER_BYTES + 4],
) -> Option<&'a str> {
    buf[..base.len()].copy_from_slice(base.as_bytes());
    let len = base.len() + next.encode_utf8(&mut buf[base.len()..]).len();
    let candidate = std::str::from_utf8(&buf[..len]).expect("UTF-8 cluster and scalar");
    let boundary = GraphemeCursor::new(base.len(), len, true)
        .is_boundary(candidate, 0)
        .expect("complete UTF-8 candidate");
    (!boundary).then_some(candidate)
}
