use crate::{grid::TerminalCell, screen::ScreenState};
use smallvec::{smallvec, SmallVec};
use std::{collections::HashMap, sync::Arc};

/// Compact reference stored in each terminal cell. Zero means no hyperlink.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HyperlinkId(u16);

impl HyperlinkId {
    pub const NONE: Self = Self(0);

    #[inline]
    pub const fn is_none(self) -> bool {
        self.0 == 0
    }

    #[inline]
    pub const fn from_raw(raw: u16) -> Self {
        Self(raw)
    }

    #[inline]
    pub const fn as_raw(self) -> u16 {
        self.0
    }
}

pub const MAX_OSC8_PARAMS_BYTES: usize = 256;
pub const MAX_OSC8_URI_BYTES: usize = 2 * 1024;
const MAX_HYPERLINKS: usize = 4096;

/// Pure host validation for a bounded parameter string and target. The engine
/// owns retention and identifiers. It never opens a target.
pub type HyperlinkPolicy = fn(params: &str, uri: &str) -> bool;

/// Session-local append-only link metadata. IDs are not recycled while cells
/// or immutable frames may still refer to them.
#[derive(Debug, Default)]
pub struct HyperlinkTable {
    targets: Vec<Arc<str>>,
    ids_by_target: HashMap<Arc<str>, HyperlinkId>,
    url_bytes: usize,
    url_byte_limit: Option<usize>,
}

pub(crate) type HyperlinkResources = Arc<[(HyperlinkId, Arc<str>)]>;

pub(crate) fn capture_row_hyperlinks(cells: &[TerminalCell], ids: &mut Vec<HyperlinkId>) {
    ids.clear();
    for cell in cells {
        let id = cell.hyperlink_id;
        if !id.is_none() && ids.last() != Some(&id) {
            ids.push(id);
        }
    }
    ids.sort_unstable();
    ids.dedup();
}

pub(crate) fn resolve_resource(
    resources: &Option<HyperlinkResources>,
    id: HyperlinkId,
) -> Option<&str> {
    let resources = resources.as_deref()?;
    let index = resources.binary_search_by_key(&id, |(id, _)| *id).ok()?;
    Some(&resources[index].1)
}

pub(crate) fn retain_resources(
    resources: &HyperlinkResources,
    ids: impl Iterator<Item = HyperlinkId>,
) -> Option<HyperlinkResources> {
    let maximum = usize::from(resources.last()?.0.as_raw());
    // Track missing targets without a lookup for every cell in a linked run.
    // The common interner range fits inline, with a bounded spill for sparse IDs.
    let mut missing: SmallVec<[u64; MAX_HYPERLINKS.div_ceil(64)]> =
        smallvec![0; maximum.div_ceil(64)];
    for (id, _) in resources.iter() {
        if let Some(index) = usize::from(id.as_raw()).checked_sub(1) {
            missing[index / 64] |= 1 << (index % 64);
        }
    }
    let mut count = 0;
    for id in ids {
        let raw = usize::from(id.as_raw());
        if raw != 0 && raw <= maximum {
            let index = raw - 1;
            let mask = 1 << (index % 64);
            let word = &mut missing[index / 64];
            if *word & mask != 0 {
                *word &= !mask;
                count += 1;
                if count == resources.len() {
                    return Some(Arc::clone(resources));
                }
            }
        }
    }
    let retained = resources.iter().filter(|(id, _)| {
        usize::from(id.as_raw())
            .checked_sub(1)
            .is_some_and(|index| missing[index / 64] & (1 << (index % 64)) == 0)
    });
    if count == 0 {
        return None;
    }
    let mut targets = Vec::with_capacity(count);
    targets.extend(retained.cloned());
    Some(targets.into())
}

impl HyperlinkTable {
    pub(crate) fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }

    pub(crate) fn with_url_byte_limit(url_byte_limit: usize) -> Self {
        Self {
            url_byte_limit: Some(url_byte_limit),
            ..Self::default()
        }
    }

    pub fn intern(
        &mut self,
        params: &str,
        uri: &str,
        policy: HyperlinkPolicy,
    ) -> Option<HyperlinkId> {
        if params.len() > MAX_OSC8_PARAMS_BYTES
            || uri.is_empty()
            || uri.len() > MAX_OSC8_URI_BYTES
            || params.chars().any(char::is_control)
            || uri.chars().any(char::is_control)
            || params.contains('\u{fffd}')
            || uri.contains('\u{fffd}')
            || !policy(params, uri)
        {
            return None;
        }
        if let Some(id) = self.ids_by_target.get(uri) {
            return Some(*id);
        }
        if self.targets.len() >= MAX_HYPERLINKS
            || self
                .url_byte_limit
                .is_some_and(|limit| self.url_bytes + uri.len() > limit)
        {
            return None;
        }

        let target: Arc<str> = Arc::from(uri);
        let id = HyperlinkId::from_raw((self.targets.len() + 1) as u16);
        self.targets.push(Arc::clone(&target));
        self.ids_by_target.insert(target, id);
        self.url_bytes += uri.len();
        Some(id)
    }

    /// The fields following OSC 8. Empty, malformed or rejected sequences
    /// clear the active link, so subsequent text cannot inherit it by accident.
    pub fn apply_osc8(&mut self, fields: &str, policy: HyperlinkPolicy) -> HyperlinkId {
        fields
            .split_once(';')
            .and_then(|(params, uri)| self.intern(params, uri, policy))
            .unwrap_or_default()
    }

    fn target(&self, id: HyperlinkId) -> Option<&Arc<str>> {
        let index = usize::from(id.as_raw()).checked_sub(1)?;
        self.targets.get(index)
    }

    pub fn resolve(&self, id: HyperlinkId) -> Option<&str> {
        self.target(id).map(AsRef::as_ref)
    }

    pub(crate) fn snapshot(
        &self,
        ids: impl Iterator<Item = HyperlinkId>,
    ) -> Option<HyperlinkResources> {
        if self.is_empty() {
            return None;
        }
        let mut ids = ids.filter_map(|id| {
            usize::from(id.as_raw())
                .checked_sub(1)
                .filter(|&index| index < self.targets.len())
        });
        // Skip scratch initialization when no visible ID resolves. IDs are
        // bounded by this interner, so a fixed bitmap replaces duplicate
        // staging/sorting while retaining ascending resource order.
        let first = ids.next()?;
        let mut seen = [0u64; MAX_HYPERLINKS.div_ceil(64)];
        seen[first / 64] |= 1 << (first % 64);
        let mut last_word = first / 64;
        for index in ids {
            seen[index / 64] |= 1 << (index % 64);
            last_word = last_word.max(index / 64);
        }
        let count = seen[..=last_word]
            .iter()
            .map(|word| word.count_ones() as usize)
            .sum();
        let mut targets = Vec::with_capacity(count);
        for (word_index, &word) in seen[..=last_word].iter().enumerate() {
            let mut bits = word;
            while bits != 0 {
                let index = word_index * 64 + bits.trailing_zeros() as usize;
                targets.push((
                    HyperlinkId::from_raw((index + 1) as u16),
                    self.targets[index].clone(),
                ));
                bits &= bits - 1;
            }
        }
        Some(targets.into())
    }
}

pub const MAX_FROST_OSC8_ID_BYTES: usize = 256;

/// FrostLegacy identifies a target by the URL and first matching optional id.
#[derive(Debug, PartialEq, Eq, Hash)]
struct Osc8Hyperlink {
    uri: String,
    id: Option<String>,
}

/// Bounded session-local link retention for FrostLegacy. URI opening and the
/// host's text-sanitization policy are supplied by the caller.
#[derive(Default)]
pub struct FrostHyperlinkTable {
    targets: Vec<Arc<Osc8Hyperlink>>,
    keys: HashMap<Arc<Osc8Hyperlink>, u16>,
}

impl FrostHyperlinkTable {
    pub fn apply_osc8(
        &mut self,
        screen: &mut ScreenState,
        fields: &str,
        openable: impl FnOnce(&str) -> bool,
        contains_spoofing: impl Fn(&str) -> bool,
    ) {
        screen.current_hyperlink = fields
            .split_once(';')
            .and_then(|(params, uri)| self.intern(params, uri, openable, contains_spoofing))
            .unwrap_or_default();
    }

    fn intern(
        &mut self,
        params: &str,
        uri: &str,
        openable: impl FnOnce(&str) -> bool,
        contains_spoofing: impl Fn(&str) -> bool,
    ) -> Option<HyperlinkId> {
        if uri.is_empty()
            || uri.len() > MAX_OSC8_URI_BYTES
            || !openable(uri)
            || uri.contains('\u{fffd}')
            || contains_spoofing(uri)
        {
            return None;
        }
        let id = params
            .split(':')
            .find_map(|parameter| parameter.strip_prefix("id="));
        if id.is_some_and(|id| {
            id.len() > MAX_FROST_OSC8_ID_BYTES
                || id.contains('\u{fffd}')
                || id.chars().any(char::is_control)
                || contains_spoofing(id)
        }) {
            return None;
        }

        let candidate = Osc8Hyperlink {
            uri: uri.to_owned(),
            id: id.map(str::to_owned),
        };
        if let Some(&key) = self.keys.get(&candidate) {
            return Some(HyperlinkId::from_raw(key));
        }
        if self.targets.len() >= MAX_HYPERLINKS {
            return None;
        }
        let key = self
            .targets
            .len()
            .checked_add(1)
            .and_then(|key| u16::try_from(key).ok())?;
        let target = Arc::new(candidate);
        self.targets.push(Arc::clone(&target));
        self.keys.insert(target, key);
        Some(HyperlinkId::from_raw(key))
    }

    pub fn resolve(&self, id: HyperlinkId) -> Option<&str> {
        let index = usize::from(id.as_raw()).checked_sub(1)?;
        self.targets.get(index).map(|target| target.uri.as_str())
    }

    pub fn clear(&mut self) {
        self.targets.clear();
        self.keys.clear();
    }

    pub fn retained_count(&self) -> usize {
        self.targets.len()
    }

    #[doc(hidden)] // Full-state compatibility snapshots.
    pub fn snapshot_targets(&self) -> impl std::fmt::Debug + '_ {
        &self.targets
    }

    #[doc(hidden)] // Full-state compatibility snapshots.
    pub fn snapshot_keys(&self) -> impl std::fmt::Debug + '_ {
        let mut links = self.keys.iter().collect::<Vec<_>>();
        links.sort_by_key(|(_, key)| **key);
        links
    }
}

#[cfg(test)]
mod frost_tests {
    use super::*;

    #[test]
    fn frost_osc8_retains_composite_identity_and_closes_on_rejected_fields() {
        let mut screen = ScreenState::new(2, 8);
        let mut links = FrostHyperlinkTable::default();
        let uri = "https://example.test/a";
        let cases = [
            (format!("other=1:id=first:id=unused;{uri}"), 1),
            (format!("id=first:other=2;{uri}"), 1),
            (format!("id=second;{uri}"), 2),
            (format!(";{uri}"), 3),
            (format!("id=;{uri}"), 4),
            (format!("id=\u{200b};{uri}"), 0),
            (format!("id=missing-delimiter{uri}"), 0),
            (
                format!("id=first;{uri}/{}", "x".repeat(MAX_OSC8_URI_BYTES)),
                0,
            ),
            (
                format!("id={};{uri}", "x".repeat(MAX_FROST_OSC8_ID_BYTES + 1)),
                0,
            ),
            (";file:///example".to_owned(), 0),
            (";".to_owned(), 0),
        ];
        for (fields, expected) in cases {
            links.apply_osc8(
                &mut screen,
                &fields,
                |uri| uri.starts_with("https://"),
                |value| value.contains('\u{200b}'),
            );
            assert_eq!(screen.current_hyperlink.as_raw(), expected, "{fields}");
            assert_eq!(
                links.resolve(screen.current_hyperlink),
                (expected != 0).then_some(uri)
            );
        }
        assert_eq!(links.retained_count(), 4);
    }

    #[test]
    fn frost_osc8_reuses_existing_keys_at_the_cap_and_reset_frees_the_table() {
        let mut screen = ScreenState::new(2, 8);
        let mut links = FrostHyperlinkTable::default();
        for id in 0..MAX_HYPERLINKS {
            let fields = format!("id={id};https://example.test/current");
            links.apply_osc8(&mut screen, &fields, |_| true, |_| false);
            assert_eq!(usize::from(screen.current_hyperlink.as_raw()), id + 1);
        }
        links.apply_osc8(
            &mut screen,
            ";https://example.test/overflow",
            |_| true,
            |_| false,
        );
        assert_eq!(screen.current_hyperlink, HyperlinkId::NONE);
        let retained = "id=0;https://example.test/current";
        links.apply_osc8(&mut screen, retained, |_| true, |_| false);
        assert_eq!(screen.current_hyperlink.as_raw(), 1);
        links.clear();
        assert_eq!(links.retained_count(), 0);
        assert_eq!(links.resolve(screen.current_hyperlink), None);
    }
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;

    #[test]
    fn filter_retains_only_references_to_sparse_resources() {
        let resources: HyperlinkResources = [1, 64, 65, 4096, u16::MAX]
            .into_iter()
            .map(|raw| (HyperlinkId::from_raw(raw), Arc::from(format!("link-{raw}"))))
            .collect();
        let present = [u16::MAX, 65, 0, 65534, 65, 2, 64].map(HyperlinkId::from_raw);
        let retained = retain_resources(&resources, present.into_iter()).unwrap();
        assert_eq!(
            retained
                .iter()
                .map(|(id, _)| id.as_raw())
                .collect::<Vec<_>>(),
            [64, 65, u16::MAX]
        );
        assert!(retain_resources(
            &resources,
            [0, 2, 65534].map(HyperlinkId::from_raw).into_iter()
        )
        .is_none());
        let all = retain_resources(
            &resources,
            resources
                .iter()
                .map(|(id, _)| *id)
                .chain(std::iter::once_with(|| {
                    panic!("stop reading cells once every stored link is referenced")
                })),
        )
        .unwrap();
        assert!(Arc::ptr_eq(&all, &resources));
    }

    #[test]
    fn bounded_snapshots_order_unique_installed_ids_across_bitmap_words() {
        let mut table = HyperlinkTable::default();
        assert!(table
            .snapshot(std::iter::from_fn(|| panic!(
                "empty table must not inspect IDs"
            )))
            .is_none());
        for index in 1..=MAX_HYPERLINKS {
            assert_eq!(
                table
                    .intern("", &format!("https://example.test/{index}"), |_, _| true)
                    .unwrap()
                    .as_raw(),
                index as u16
            );
        }
        assert!(table
            .intern("", "https://example.test/overflow", |_, _| true)
            .is_none());
        let ids = [4096, 65, 0, 64, 65, 65535, 1, 4096, 64].map(HyperlinkId::from_raw);
        let snapshot = table.snapshot(ids.into_iter()).unwrap();
        assert_eq!(
            snapshot
                .iter()
                .map(|(id, _)| id.as_raw())
                .collect::<Vec<_>>(),
            [1, 64, 65, 4096]
        );
        for (id, target) in snapshot.iter() {
            assert_eq!(
                target.as_ref(),
                format!("https://example.test/{}", id.as_raw())
            );
            assert!(Arc::ptr_eq(target, table.target(*id).unwrap()));
        }
        assert!(table
            .snapshot([HyperlinkId::NONE, HyperlinkId::from_raw(65535)].into_iter())
            .is_none());
    }
}
