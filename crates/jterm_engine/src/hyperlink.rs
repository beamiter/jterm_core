use std::{
    collections::{BTreeSet, HashMap},
    sync::Arc,
};

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
    let mut retained = BTreeSet::new();
    for id in ids.filter(|id| !id.is_none()) {
        if resources.binary_search_by_key(&id, |(id, _)| *id).is_ok() {
            retained.insert(id);
        }
    }
    if retained.is_empty() {
        None
    } else if retained.len() == resources.len() {
        Some(Arc::clone(resources))
    } else {
        Some(
            resources
                .iter()
                .filter(|(id, _)| retained.contains(id))
                .cloned()
                .collect(),
        )
    }
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
        let ids: BTreeSet<_> = ids.filter(|id| !id.is_none()).collect();
        let targets: Vec<_> = ids
            .into_iter()
            .filter_map(|id| self.target(id).map(|target| (id, target.clone())))
            .collect();
        (!targets.is_empty()).then(|| targets.into())
    }
}
