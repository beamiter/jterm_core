//! Toolkit-independent capture of a live or historical terminal viewport.

use std::{ops::Range, sync::Arc};

use jterm_engine::{
    colors::ColorState,
    frame::{CellFrame, Screen},
    history_reflow::{HistoryAnchor, HistoryLayout},
    retained_bytes::{RetainedBytes, RetainedBytesBudget},
    terminal::{HistoryPage, Terminal},
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct HistoricalWindow {
    top: HistoryAnchor,
    rows: usize,
    layout: u64,
    pub history_rows: usize,
    history_tail: Option<u64>,
    live_revision: Option<u64>,
    palette_revision: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ScrollTarget {
    Source(HistoryAnchor),
    FromTail(u64),
}

impl ScrollTarget {
    pub fn row(self, layout: &HistoryLayout) -> usize {
        match self {
            Self::Source(anchor) => layout.row_at(anchor),
            Self::FromTail(distance) => layout.len() - distance.min(layout.len() as u64) as usize,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ViewportRequest {
    pub top: Option<ScrollTarget>,
    pub painted_history: Option<HistoricalWindow>,
}

pub struct ViewportFrame {
    pub frame: CellFrame,
    pub live_margin_gaps: u128,
    pub colors: Option<(u64, Arc<ColorState>)>,
    pub extent: Range<u64>,
    pub history: Option<HistoryViewport>,
    pub evicted_anchor: bool,
}

pub struct HistoryViewport {
    pub layout: Arc<HistoryLayout>,
    pub anchor: HistoryAnchor,
    pub first: usize,
    pub window: HistoricalWindow,
    pub page: Option<HistoryPage>,
}

impl ViewportFrame {
    pub fn reserve_delivery(&self, budget: &Arc<RetainedBytesBudget>) -> Option<RetainedBytes> {
        let history_bytes = self
            .history
            .as_ref()
            .and_then(|history| history.page.as_ref())
            .map_or(0, HistoryPage::retained_delivery_bytes);
        budget.reserve(
            self.frame
                .retained_delivery_bytes()
                .saturating_add(history_bytes),
        )
    }
}

impl ViewportRequest {
    pub fn capture(
        self,
        terminal: &Terminal,
        exited: bool,
    ) -> Result<Option<ViewportFrame>, String> {
        if terminal.publication_held() && !exited {
            return Ok(None);
        }
        let frame = terminal.frame()?;
        let colors = terminal.paint_colors();
        let extent = terminal.history_extent();
        let top = self.top.filter(|_| frame.stamp().screen == Screen::Primary);
        let evicted_anchor =
            matches!(top, Some(ScrollTarget::Source(anchor)) if anchor.row < extent.start);
        let history = top
            .filter(|_| !extent.is_empty())
            .map(|target| {
                let palette_revision = colors.as_ref().map_or(0, |(revision, _)| *revision);
                self.capture_history(terminal, &frame, &extent, target, palette_revision)
            })
            .transpose()?
            .flatten();
        Ok(Some(ViewportFrame {
            frame,
            live_margin_gaps: terminal.live_margin_gaps(),
            colors,
            extent,
            history,
            evicted_anchor,
        }))
    }

    fn capture_history(
        self,
        terminal: &Terminal,
        frame: &CellFrame,
        extent: &Range<u64>,
        target: ScrollTarget,
        palette_revision: u64,
    ) -> Result<Option<HistoryViewport>, String> {
        let layout = terminal.history_layout()?;
        let first = target.row(&layout);
        let Some(top) = layout.anchor_at(first) else {
            return Ok(None);
        };
        // Preserve the original source column across successive widths.
        let anchor = match target {
            ScrollTarget::Source(anchor) if anchor.row >= extent.start => anchor,
            _ => top,
        };
        let available = layout.len() - first;
        let rows = frame.rows().len();
        let window = HistoricalWindow {
            top,
            rows,
            layout: frame.stamp().layout,
            history_rows: available.min(rows),
            history_tail: (available <= rows).then_some(extent.end),
            live_revision: (available < rows).then(|| frame.stamp().revision),
            palette_revision,
        };
        let page = (self.painted_history != Some(window))
            .then(|| terminal.projected_history_page(&layout, first, window.history_rows))
            .transpose()?;
        Ok(Some(HistoryViewport {
            layout,
            anchor,
            first,
            window,
            page,
        }))
    }
}

#[cfg(test)]
#[test]
fn a_palette_change_invalidates_a_pinned_historical_page() {
    let mut terminal = Terminal::new(2, 12).unwrap();
    terminal.set_color_defaults(Default::default());
    terminal.feed(b"one\r\ntwo\r\nthree\r\nfour\r\n").unwrap();
    let request = ViewportRequest {
        top: Some(ScrollTarget::FromTail(3)),
        painted_history: None,
    };
    let first = request
        .capture(&terminal, false)
        .unwrap()
        .unwrap()
        .history
        .unwrap();
    assert!(first.page.is_some());
    let request = ViewportRequest {
        top: Some(ScrollTarget::Source(first.anchor)),
        painted_history: Some(first.window),
    };
    terminal.feed(b"\x1b]11;?\x07").unwrap();
    assert!(request
        .capture(&terminal, false)
        .unwrap()
        .unwrap()
        .history
        .unwrap()
        .page
        .is_none());
    terminal.feed(b"\x1b]11;#102030\x07").unwrap();
    assert!(request
        .capture(&terminal, false)
        .unwrap()
        .unwrap()
        .history
        .unwrap()
        .page
        .is_some());
}
