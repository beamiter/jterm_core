/// Compact reference stored in each terminal cell. Zero means no hyperlink.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
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
