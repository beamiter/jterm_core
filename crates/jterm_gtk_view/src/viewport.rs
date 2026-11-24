use std::{ops::Range, sync::Arc};

use jterm_engine::history_reflow::{HistoryAnchor, HistoryLayout};
pub(crate) use jterm_runtime::viewport::{
    HistoricalWindow, ScrollTarget, ViewportFrame, ViewportRequest,
};

#[derive(Default)]
pub(crate) struct HistoryRail {
    pub extent: Range<u64>,
    pub layout: Option<Arc<HistoryLayout>>,
}

impl HistoryRail {
    pub fn len(&self) -> usize {
        self.layout
            .as_ref()
            .map_or((self.extent.end - self.extent.start) as usize, |layout| {
                layout.len()
            })
    }

    pub fn target_at(&self, row: usize) -> Option<ScrollTarget> {
        if let Some(layout) = &self.layout {
            return layout.anchor_at(row).map(ScrollTarget::Source);
        }
        (row < self.len()).then(|| {
            ScrollTarget::Source(HistoryAnchor {
                row: self.extent.start + row as u64,
                column: 0,
            })
        })
    }

    pub fn scroll(&self, current: Option<ScrollTarget>, by: i64) -> Option<ScrollTarget> {
        if by == i64::MAX {
            return None;
        }
        if by == 0 {
            return current;
        }
        if let Some(layout) = &self.layout {
            let row = current.map_or(layout.len(), |target| target.row(layout));
            return self.target_at(
                (row as u64)
                    .saturating_add_signed(by)
                    .min(layout.len() as u64) as usize,
            );
        }
        if let Some(ScrollTarget::Source(anchor)) = current {
            let next = anchor
                .row
                .saturating_add_signed(by)
                .clamp(self.extent.start, self.extent.end);
            return self.target_at((next - self.extent.start) as usize);
        }
        let distance = match current {
            Some(ScrollTarget::FromTail(distance)) => distance,
            _ => 0,
        };
        let distance = if by < 0 {
            distance.saturating_add(by.unsigned_abs())
        } else {
            distance.saturating_sub(by as u64)
        };
        (distance > 0).then_some(ScrollTarget::FromTail(distance))
    }
}
