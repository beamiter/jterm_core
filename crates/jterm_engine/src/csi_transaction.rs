//! Shared native CSI transaction progress. Only command progress lives here;
//! ScreenState and ProtocolState remain the one authoritative mutable state.
use crate::display_erase::{DisplayErase, DisplayEraseStep};
use crate::graphic_write::{GraphicProfile, GraphicStep, GraphicWrite, RepeatGraphics};
use crate::mode_mutation::{NativeModeCommand, NativeModeEffect};
use crate::protocol_state::ProtocolState;
use crate::region_ops::{RegionOperation, RegionStep};
use crate::screen::ScreenState;
use crate::RawRowId;

pub enum CsiTransactionEffect {
    Erase(DisplayEraseStep),
    EraseScreen,
    EraseSavedLines,
    Region(RegionStep),
    Mode(NativeModeEffect),
    Graphic(GraphicStep),
}

pub enum CsiTransaction<'a> {
    Erase(DisplayErase),
    Region(RegionOperation),
    Mode(NativeModeCommand<'a>),
    Repeat {
        repeat: RepeatGraphics,
        graphic: Option<GraphicWrite>,
        profile: GraphicProfile,
    },
}
impl CsiTransaction<'_> {
    /// Finish the returned host effect before another step. Focus, archive
    /// admission and allocator policy are sampled at each historical boundary.
    pub fn step(
        &mut self,
        screen: &mut ScreenState,
        protocol: &mut ProtocolState,
        focused: bool,
        allow_alt_archive: bool,
        mut width: impl FnMut(char) -> usize,
        mut allocate_id: impl FnMut() -> RawRowId,
    ) -> Option<CsiTransactionEffect> {
        match self {
            Self::Erase(operation) => match operation {
                DisplayErase::Rows(rows) => rows.step(screen).map(CsiTransactionEffect::Erase),
                DisplayErase::Screen => {
                    *operation = DisplayErase::Ignored;
                    Some(CsiTransactionEffect::EraseScreen)
                }
                DisplayErase::SavedLines => {
                    *operation = DisplayErase::Ignored;
                    Some(CsiTransactionEffect::EraseSavedLines)
                }
                DisplayErase::Ignored => None,
            },
            Self::Region(operation) => operation
                .step(screen, allow_alt_archive, &mut allocate_id)
                .map(CsiTransactionEffect::Region),
            Self::Mode(operation) => operation
                .step(screen, protocol, focused)
                .map(CsiTransactionEffect::Mode),
            Self::Repeat {
                repeat,
                graphic,
                profile,
            } => loop {
                if let Some(write) = graphic {
                    if let Some(effect) = write.step(screen) {
                        return Some(CsiTransactionEffect::Graphic(effect));
                    }
                    *graphic = None;
                }
                *graphic = Some(repeat.next(
                    *profile,
                    screen,
                    protocol.modes().contains(&7),
                    &mut width,
                )?);
            },
        }
    }
}
