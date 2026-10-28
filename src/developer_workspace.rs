//! Native workspace metrics for the October Block reference (Anvil/Forge).
//!
//! These are measured logical-pixel estimates from the 1672x941 raster, not
//! original editable design values. Both presentations share the outer shell.
//! Other adapters continue using `reference_shell` unchanged. Reuse its message
//! components, colors and presentation migration contract; override geometry
//! here instead of silently changing every native adapter.

pub use crate::reference_shell::*;

pub const REFERENCE_WIDTH: f32 = 1672.0;
pub const REFERENCE_HEIGHT: f32 = 941.0;
pub const TITLEBAR_HEIGHT: f32 = 55.0;
pub const RAIL_WIDTH: f32 = 53.0;
/// Includes the navigation's trailing one-pixel boundary.
pub const NAVIGATION_WIDTH: f32 = 191.0;
pub const INSPECTOR_WIDTH: f32 = 298.0;
pub const CANVAS_HEADER_HEIGHT: f32 = 42.0;
pub const LIVE_TOOLBAR_HEIGHT: f32 = 36.0;
pub const COMPOSER_HEIGHT: f32 = 68.0;
pub const COMPOSER_INSET: f32 = 12.0;
pub const COMPOSER_BOTTOM: f32 = 8.0;
pub const INSPECTOR_INSET: f32 = 7.0;
pub const INSPECTOR_TABS_HEIGHT: f32 = 42.0;
pub const MODE_CONTROL_HEIGHT: f32 = 34.0;
pub const MODE_CONTROL_TOP: f32 = 11.0;
pub const MODE_CONTROL_MIN_WIDTH: f32 = 72.0;
pub const SEARCH_WIDTH: f32 = 484.0;
pub const SEARCH_HEIGHT: f32 = 36.0;
pub const SEARCH_TOP: f32 = 10.0;
pub const SEARCH_CENTER_OFFSET: f32 = 0.0;
pub const CARD_HORIZONTAL_INSET: f32 = 12.0;
pub const CARD_GAP: f32 = 10.0;
pub const DEFAULT_CONTENT_FONT_DESC: &str = "JetBrainsMono Nerd Font Mono 12px";
/// Deliberately independent from the content font's pixel units.
pub const DEFAULT_UI_FONT_SIZE: &str = "14";
pub const DEFAULT_UI_FONT_SCALE: f64 = 1.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Layout {
    pub titlebar: Rect,
    pub rail: Rect,
    pub navigation: Option<Rect>,
    pub center: Rect,
    pub header: Rect,
    pub inspector: Option<Rect>,
    pub composer: Rect,
}

impl Layout {
    pub fn for_size(width: f32, height: f32) -> Self {
        let width = if width.is_finite() {
            width.max(0.0)
        } else {
            REFERENCE_WIDTH
        };
        let height = if height.is_finite() {
            height.max(0.0)
        } else {
            REFERENCE_HEIGHT
        };
        let title_h = TITLEBAR_HEIGHT.min(height);
        let body_h = (height - title_h).max(0.0);
        let rail_w = RAIL_WIDTH.min(width);
        let navigation_w = if width >= RAIL_WIDTH + NAVIGATION_WIDTH + 480.0 {
            NAVIGATION_WIDTH
        } else {
            0.0
        };
        let inspector_w = if width >= RAIL_WIDTH + NAVIGATION_WIDTH + INSPECTOR_WIDTH + 480.0 {
            INSPECTOR_WIDTH
        } else {
            0.0
        };
        let center_x = rail_w + navigation_w;
        let center_w = (width - center_x - inspector_w).max(0.0);
        let header_h = CANVAS_HEADER_HEIGHT.min(body_h);
        let composer_h = COMPOSER_HEIGHT.min((body_h - header_h - COMPOSER_BOTTOM).max(0.0));
        let inset = COMPOSER_INSET.min(center_w / 8.0);
        Self {
            titlebar: Rect {
                x: 0.0,
                y: 0.0,
                width,
                height: title_h,
            },
            rail: Rect {
                x: 0.0,
                y: title_h,
                width: rail_w,
                height: body_h,
            },
            navigation: (navigation_w > 0.0).then_some(Rect {
                x: rail_w,
                y: title_h,
                width: navigation_w,
                height: body_h,
            }),
            center: Rect {
                x: center_x,
                y: title_h,
                width: center_w,
                height: body_h,
            },
            header: Rect {
                x: center_x,
                y: title_h,
                width: center_w,
                height: header_h,
            },
            inspector: (inspector_w > 0.0).then_some(Rect {
                x: width - inspector_w,
                y: title_h,
                width: inspector_w,
                height: body_h,
            }),
            composer: Rect {
                x: center_x + inset,
                y: (height - COMPOSER_BOTTOM - composer_h).max(title_h + header_h),
                width: (center_w - 2.0 * inset).max(0.0),
                height: composer_h,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measured_workspace_boundaries_at_reference_size() {
        let l = Layout::for_size(1672.0, 941.0);
        assert_eq!(
            l.center,
            Rect {
                x: 244.0,
                y: 55.0,
                width: 1130.0,
                height: 886.0
            }
        );
        assert_eq!(l.header.height, 42.0);
        assert_eq!(l.inspector.unwrap().x, 1374.0);
        assert_eq!(
            l.composer,
            Rect {
                x: 256.0,
                y: 865.0,
                width: 1106.0,
                height: 68.0
            }
        );
        // Raster border estimates differ by about one pixel, not a scale factor.
        assert_eq!(l.inspector.unwrap().x + INSPECTOR_INSET, 1381.0);
    }

    #[test]
    fn inspector_then_navigation_collapse_without_covering_the_input() {
        for (w, nav, inspector) in [
            (1672.0, true, true),
            (1356.0, true, true),
            (1000.0, true, false),
            (700.0, false, false),
        ] {
            let l = Layout::for_size(w, 704.0);
            assert_eq!(l.navigation.is_some(), nav);
            assert_eq!(l.inspector.is_some(), inspector);
            assert!(l.center.width >= 480.0);
            assert!(l.composer.x >= l.center.x);
            assert!(l.composer.x + l.composer.width <= l.center.x + l.center.width);
            assert!(l.composer.y + l.composer.height <= 704.0);
        }
        for (w, h) in [(0.0, 0.0), (320.0, 240.0), (f32::NAN, f32::INFINITY)] {
            let l = Layout::for_size(w, h);
            for v in [
                l.center.x,
                l.center.width,
                l.center.height,
                l.composer.x,
                l.composer.y,
                l.composer.height,
            ] {
                assert!(v.is_finite() && v >= 0.0);
            }
        }
    }

    #[test]
    fn earlier_four_adapter_contract_is_unchanged() {
        let l = crate::reference_shell::Layout::for_size(1356.0, 704.0);
        assert_eq!(l.center.x, 292.0);
        assert_eq!(l.titlebar.height, 42.0);
        assert_eq!(l.composer.height, 80.0);
        assert_eq!(crate::reference_shell::type_size::HEADLINE, 20.0);
        assert_eq!(crate::reference_shell::type_size::BODY, 12.0);
        assert_eq!(crate::reference_shell::type_size::CODE, 10.0);
    }
}
