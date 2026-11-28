use unicode_segmentation::GraphemeCursor;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub(crate) const MAX_CLUSTER_BYTES: usize = 64;

const CACHE_SLOTS: usize = 64;

#[derive(Clone, Copy)]
struct PresentationEntry {
    glyph: char,
    narrow: bool,
    can_change: bool,
}

/// Cache pure scalar decisions across input spans. The full grapheme string
/// and its layout state stay with the terminal.
pub(crate) struct ScalarCache {
    presentations: [Option<PresentationEntry>; CACHE_SLOTS],
    breaks: [Option<[char; 2]>; CACHE_SLOTS],
}

impl Default for ScalarCache {
    fn default() -> Self {
        Self {
            presentations: [None; CACHE_SLOTS],
            breaks: [None; CACHE_SLOTS],
        }
    }
}

impl ScalarCache {
    pub(crate) fn presentation_can_change(&mut self, glyph: char, width: usize) -> bool {
        let narrow = width == 1;
        if glyph.is_ascii() {
            return narrow && may_grow(glyph);
        }
        let scalar = glyph as usize;
        let slot = (scalar ^ (scalar >> 6)) % CACHE_SLOTS;
        let entry = &mut self.presentations[slot];
        if let Some(entry) = entry.filter(|entry| entry.glyph == glyph && entry.narrow == narrow) {
            return entry.can_change;
        }
        let can_change = if narrow {
            may_grow(glyph)
        } else {
            may_contract(glyph)
        };
        *entry = Some(PresentationEntry {
            glyph,
            narrow,
            can_change,
        });
        can_change
    }

    /// Only single-scalar left contexts use this cache. Retained clusters
    /// still need their full left context for emoji and Indic sequences.
    pub(crate) fn scalar_extension<'a>(
        &mut self,
        base: char,
        next: char,
        buf: &'a mut [u8; MAX_CLUSTER_BYTES + 4],
    ) -> Option<&'a str> {
        let pair = [base, next];
        let left = base as usize;
        let right = next as usize;
        let slot = (left.wrapping_mul(17) ^ (left >> 6) ^ right ^ (right >> 6)) % CACHE_SLOTS;
        let cached = &mut self.breaks[slot];
        if *cached == Some(pair) {
            return None;
        }
        let mut scalar = [0; 4];
        let result = extension(base.encode_utf8(&mut scalar), next, buf);
        if result.is_none() {
            *cached = Some(pair);
        }
        result
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_cache_matches_unicode_rules_across_hits_and_evictions() {
        let glyphs = [
            'A', '4', 'é', '中', '文', '✈', '⌚', '👩', '🇨', '🇳', 'क', 'ष', '\u{200d}', '\u{301}',
            '\u{94d}', '\u{fe0e}', '\u{fe0f}', '\u{600}',
        ];
        let mut cache = ScalarCache::default();
        for width in [1, 2, 1] {
            for glyph in glyphs {
                let expected = if width == 1 {
                    may_grow(glyph)
                } else {
                    may_contract(glyph)
                };
                for _ in 0..2 {
                    assert_eq!(cache.presentation_can_change(glyph, width), expected);
                }
            }
        }
        let mut reference = [0; MAX_CLUSTER_BYTES + 4];
        let mut cached = [0; MAX_CLUSTER_BYTES + 4];
        for base in glyphs {
            let mut scalar = [0; 4];
            let left = base.encode_utf8(&mut scalar);
            for next in glyphs {
                let expected = extension(left, next, &mut reference);
                for _ in 0..2 {
                    assert_eq!(cache.scalar_extension(base, next, &mut cached), expected);
                }
            }
        }
    }
}
