//! Shared native workspace shell contract for Anvil, Forge, Ember and Frost.
//!
//! Geometry is in logical pixels. These values reconstruct the supplied JPEG
//! from measured regions; they are not an editable source design export.
//! Native frontends own layout, terminal rendering, input and accessibility.
//! The shell must not create a second input owner or change a reading anchor.

pub const REFERENCE_WIDTH: f32 = 1356.0;
pub const REFERENCE_HEIGHT: f32 = 704.0;
pub const TITLEBAR_HEIGHT: f32 = 42.0;
pub const RAIL_WIDTH: f32 = 55.0;
pub const NAVIGATION_WIDTH: f32 = 237.0;
pub const INSPECTOR_WIDTH: f32 = 306.0;
/// Interactive sidebars retain a usable command canvas between their handles.
pub const SIDEBAR_DIVIDER_WIDTH: f32 = 6.0;
pub const MIN_CENTER_WIDTH: f32 = 480.0;
pub const MIN_NAVIGATION_WIDTH: f32 = 120.0;
pub const MAX_NAVIGATION_WIDTH: f32 = 800.0;
pub const MIN_INSPECTOR_WIDTH: f32 = 240.0;
pub const MAX_INSPECTOR_WIDTH: f32 = 1200.0;
pub const CANVAS_HEADER_HEIGHT: f32 = 49.0;
pub const COMPOSER_HEIGHT: f32 = 80.0;
pub const COMPOSER_INSET: f32 = 28.0;
pub const COMPOSER_BOTTOM: f32 = 11.0;
pub const METADATA_GAP: f32 = 6.0;
pub const NAVIGATION_PADDING: f32 = 14.0;
pub const NAVIGATION_ROW_HEIGHT: f32 = 26.0;
pub const NAVIGATION_ROW_GAP: f32 = 3.0;
pub const NAVIGATION_LABEL_TOP: f32 = 58.0;
pub const WORKSPACE_SELECTOR_TOP: f32 = 77.0;
pub const WORKSPACE_SELECTOR_HEIGHT: f32 = 33.0;
pub const NAVIGATION_ITEMS_TOP: f32 = 117.0;
pub const CONTROL_HEIGHT: f32 = 28.0;
pub const SELECTED_RAIL_SIZE: f32 = 34.0;
pub const CARD_RADIUS: f32 = 12.0;
pub const COMPOSER_RADIUS: f32 = 14.0;
pub const CONTROL_RADIUS: f32 = 7.0;
pub const COMPOSER_CONTROL_BOTTOM: f32 = 8.0;
pub const COMPOSER_CONTROL_LEFT: f32 = 12.0;
pub const COMPOSER_CONTROL_RIGHT: f32 = 10.0;
pub const COMPOSER_MODEL_MIN_WIDTH: f32 = 120.0;
pub const MODEL_UNAVAILABLE_LABEL: &str = "Model unavailable";
pub const MODE_CONTROL_HEIGHT: f32 = 28.0;
pub const MODE_CONTROL_TOP: f32 = 7.0;
pub const MODE_CONTROL_MIN_WIDTH: f32 = 52.0;
pub const COMPOSER_PLUS_WIDTH: f32 = 28.0;
pub const COMPOSER_ATTACH_WIDTH: f32 = 62.0;
pub const COMPOSER_IMAGE_WIDTH: f32 = 60.0;
pub const COMPOSER_CODE_WIDTH: f32 = 56.0;
pub const COMPOSER_TERMINAL_WIDTH: f32 = 76.0;
pub const COMPOSER_TOOLS_WIDTH: f32 = 68.0;
pub const COMPOSER_SEND_WIDTH: f32 = 28.0;
pub const COMPOSER_SECONDARY_MIN_WIDTH: f32 = 564.0;
pub const COMPOSER_MODEL_MIN_AVAILABLE: f32 = 392.0;
pub const COMPOSER_TERMINAL_MIN_AVAILABLE: f32 = 304.0;
pub const INSPECTOR_BADGE_HEIGHT: f32 = 18.0;
pub const INSPECTOR_BADGE_FONT_SIZE: f32 = 8.0;
pub const INSPECTOR_BADGE_HORIZONTAL_PADDING: f32 = 5.0;
pub const INSPECTOR_BADGE_VERTICAL_PADDING: f32 = 2.0;
pub const INSPECTOR_BADGE_RADIUS: f32 = 5.0;
pub const NO_PROGRESS_LABEL: &str = "No progress";
pub const HEADER_ACTION_SIZE: f32 = 24.0;
pub const HEADER_ACTION_GAP: f32 = 4.0;
pub const INSPECTOR_TAB_WIDTH: f32 = 50.0;
pub const SEARCH_WIDTH: f32 = 362.0;
pub const SEARCH_HEIGHT: f32 = 28.0;
pub const SEARCH_TOP: f32 = 8.0;
pub const SEARCH_CENTER_OFFSET: f32 = 9.0;
pub const INSPECTOR_INSET: f32 = 14.0;
pub const INSPECTOR_TABS_HEIGHT: f32 = 49.0;
pub const ASSISTANT_CONTENT_INSET: f32 = 68.0;
pub const ASSISTANT_CONTENT_RIGHT: f32 = 54.0;
pub const ASSISTANT_AVATAR_INSET: f32 = 36.0;
pub const USER_BUBBLE_WIDTH: f32 = 337.0;
pub const USER_BUBBLE_RIGHT: f32 = 71.0;
pub const CONVERSATION_TOP_PADDING: f32 = 30.0;
pub const REFERENCE_RECENT_TOP: f32 = 273.0;
pub const REFERENCE_SPACES_TOP: f32 = 477.0;
pub const PROFILE_DIAMETER: f32 = 28.0;
pub const RECENT_ROW_HEIGHT: f32 = 32.0;
pub const SPACES_ROW_HEIGHT: f32 = 30.0;
pub const NAVIGATION_STATUS_HEIGHT: f32 = 49.0;
pub const NAVIGATION_STATUS_TEXT_BOTTOM: f32 = 32.0;
pub const USER_BUBBLE_HEIGHT: f32 = 48.0;
pub const USER_BUBBLE_RADIUS: f32 = 10.0;
pub const TASK_ROW_HEIGHT: f32 = 25.0;
pub const MESSAGE_PART_GAP: f32 = 8.0;
pub const DIFF_HEADER_HEIGHT: f32 = 30.0;
pub const DIFF_LINE_HEIGHT: f32 = 15.0;
pub const DIFF_GUTTER_COLUMN_WIDTH: f32 = 28.0;
pub const DIFF_SIGN_WIDTH: f32 = 16.0;
pub const DIFF_BODY_VERTICAL_PADDING: f32 = 4.0;
pub const METADATA_PILL_HEIGHT: f32 = 23.0;
pub const INSPECTOR_METADATA_ROW_HEIGHT: f32 = 19.0;
pub const INSPECTOR_PROGRESS_TARGET_Y: f32 = 158.0;
pub const INSPECTOR_PROGRESS_EXTRA_TOP: f32 = 9.0;
pub const INSPECTOR_QUEUE_ROW_HEIGHT: f32 = 24.0;
pub const INSPECTOR_QUEUE_GAP: f32 = 2.0;
pub const INSPECTOR_CURRENT_HEIGHT: f32 = 45.0;
pub const INSPECTOR_CURRENT_TARGET_Y: f32 = 298.0;
pub const INSPECTOR_METADATA_TARGET_Y: f32 = 185.0;
pub const INSPECTOR_OUTPUT_HEIGHT: f32 = 140.0;
pub const INSPECTOR_OUTPUT_BOTTOM: f32 = 8.0;
/// Available in the validation environment; original JPEG fonts are unknown.
pub const FONT_SANS: &str = "DejaVu Sans";
pub const FONT_MONOSPACE: &str = "DejaVu Sans Mono";
pub const HEADLINE_WEIGHT: u16 = 700;
pub const BODY_WEIGHT: u16 = 400;

/// Sampled / inferred palette, shared across native toolkit adapters.
pub mod color {
    pub const TITLEBAR: [u8; 3] = [15, 23, 38];
    pub const RAIL: [u8; 3] = [13, 21, 34];
    pub const NAVIGATION: [u8; 3] = [22, 33, 51];
    pub const NAVIGATION_SELECTED: [u8; 3] = [40, 61, 111];
    pub const INSPECTOR: [u8; 3] = [15, 24, 37];
    pub const INSPECTOR_CARD: [u8; 3] = [28, 41, 58];
    pub const CANVAS: [u8; 3] = [250, 251, 255];
    pub const CANVAS_COOL: [u8; 3] = [228, 240, 254];
    pub const USER_BUBBLE: [u8; 3] = [217, 228, 250];
    pub const COMPOSER: [u8; 3] = [254, 254, 254];
    pub const TERMINAL: [u8; 3] = [9, 12, 23];
    pub const ACCENT: [u8; 3] = [92, 126, 240];
    pub const ACCENT_PALE: [u8; 3] = [231, 237, 253];
    pub const CHROME_TEXT: [u8; 3] = [221, 229, 244];
    pub const CHROME_MUTED: [u8; 3] = [144, 160, 185];
    pub const CANVAS_TEXT: [u8; 3] = [40, 52, 76];
    // Message roles have different contrast needs. Do not use the headline
    // foreground for paragraphs and make regular text appear artificially bold.
    pub const MESSAGE_BODY: [u8; 3] = [78, 92, 116];
    pub const MESSAGE_USER_TEXT: [u8; 3] = [53, 68, 94];
    pub const MESSAGE_STEP_TEXT: [u8; 3] = [78, 100, 127];
    pub const CANVAS_MUTED: [u8; 3] = [111, 125, 148];
    pub const LIGHT_BORDER: [u8; 3] = [210, 221, 239];
    pub const DARK_BORDER: [u8; 3] = [43, 57, 79];
    pub const CONTROL_SURFACE: [u8; 3] = [247, 250, 255];
    pub const CONTROL_BORDER: [u8; 3] = [212, 224, 240];
    pub const DISABLED_TEXT: [u8; 3] = [113, 130, 154];
    pub const PROFILE_BACKGROUND: [u8; 3] = [53, 73, 109];
    pub const PROFILE_BORDER: [u8; 3] = [98, 118, 154];
}

/// Estimated JPEG typography; toolkit adapters must respect font / DPI scaling.
pub mod type_size {
    pub const HEADLINE: f32 = 20.0;
    pub const BODY: f32 = 12.0;
    pub const PAGE_TITLE: f32 = 12.0;
    pub const ROLE: f32 = 12.0;
    pub const CONTROL: f32 = 10.0;
    pub const METADATA: f32 = 9.0;
    pub const SECTION: f32 = 8.0;
    pub const CODE: f32 = 10.0;
}

/// Public terminal presentations. Shell, Vim, Agent and Files are capabilities.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Presentation {
    Block,
    Chat,
}

impl Presentation {
    /// Legacy settings remain readable without exposing old backend choices.
    pub fn parse_legacy(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "chat" => Some(Self::Chat),
            "block" | "normal" | "vte" | "unified" => Some(Self::Block),
            _ => None,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Block => "Block",
            Self::Chat => "Chat",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// Collapse the inspector, then navigation, while retaining center usability.
/// A collapsed inspector remains reachable through its toolbar control.
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
    /// Fit preferred sidebar content widths without changing those preferences.
    /// A zero width hides that panel explicitly. The right panel collapses
    /// before the navigation is constrained, and a temporarily narrower window
    /// can reduce navigation to its minimum while retaining the center budget.
    /// Each visible sidebar reserves a real, six-pixel native divider.
    /// `for_size` remains the original noninteractive reference geometry.
    pub fn for_size_with_sidebars(
        width: f32,
        height: f32,
        navigation_width: f32,
        inspector_width: f32,
    ) -> Self {
        fn preferred(value: f32, fallback: f32, minimum: f32, maximum: f32) -> f32 {
            if !value.is_finite() {
                fallback
            } else if value <= 0.0 {
                0.0
            } else {
                value.clamp(minimum, maximum)
            }
        }
        let navigation = preferred(
            navigation_width,
            NAVIGATION_WIDTH,
            MIN_NAVIGATION_WIDTH,
            MAX_NAVIGATION_WIDTH,
        );
        let inspector = preferred(
            inspector_width,
            INSPECTOR_WIDTH,
            MIN_INSPECTOR_WIDTH,
            MAX_INSPECTOR_WIDTH,
        );
        let mut layout = Self::for_size(width, height);
        let available = (layout.titlebar.width - layout.rail.width - MIN_CENTER_WIDTH).max(0.0);
        let navigation_budget = navigation
            + if navigation > 0.0 {
                SIDEBAR_DIVIDER_WIDTH
            } else {
                0.0
            };
        let inspector_width = if inspector > 0.0
            && available >= navigation_budget + inspector + SIDEBAR_DIVIDER_WIDTH
        {
            inspector
        } else {
            0.0
        };
        let inspector_budget = inspector_width
            + if inspector_width > 0.0 {
                SIDEBAR_DIVIDER_WIDTH
            } else {
                0.0
            };
        let navigation_available = available - inspector_budget - SIDEBAR_DIVIDER_WIDTH;
        let navigation_width = if navigation > 0.0 && navigation_available >= MIN_NAVIGATION_WIDTH {
            navigation.min(navigation_available)
        } else {
            0.0
        };
        layout.navigation = (navigation_width > 0.0).then_some(Rect {
            x: layout.rail.width,
            y: layout.center.y,
            width: navigation_width,
            height: layout.center.height,
        });
        layout.inspector = (inspector_width > 0.0).then_some(Rect {
            x: layout.titlebar.width - inspector_width,
            y: layout.center.y,
            width: inspector_width,
            height: layout.center.height,
        });
        layout.center.x = layout.rail.width
            + navigation_width
            + if navigation_width > 0.0 {
                SIDEBAR_DIVIDER_WIDTH
            } else {
                0.0
            };
        layout.center.width = (layout.titlebar.width - layout.center.x - inspector_budget).max(0.0);
        layout.header.x = layout.center.x;
        layout.header.width = layout.center.width;
        let inset = COMPOSER_INSET.min(layout.center.width / 8.0);
        layout.composer.x = layout.center.x + inset;
        layout.composer.width = (layout.center.width - 2.0 * inset).max(0.0);
        layout
    }

    /// Remove only the right inspector's allocation. Its native owner retains
    /// the panel and selected page; every central rectangle gets the space.
    pub fn without_inspector(mut self) -> Self {
        if self.inspector.take().is_some() {
            self.center.width = (self.titlebar.width - self.center.x).max(0.0);
            self.header.x = self.center.x;
            self.header.width = self.center.width;
            let inset = COMPOSER_INSET.min(self.center.width / 8.0);
            self.composer.x = self.center.x + inset;
            self.composer.width = (self.center.width - 2.0 * inset).max(0.0);
        }
        self
    }

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
    fn interactive_sidebars_keep_preferences_and_a_usable_center() {
        let left = 600.0;
        let right = 800.0;
        let wide = Layout::for_size_with_sidebars(2400.0, 800.0, left, right);
        assert_eq!(wide.navigation.unwrap().width, left);
        assert_eq!(wide.inspector.unwrap().width, right);
        assert_eq!(wide.center.x, RAIL_WIDTH + left + SIDEBAR_DIVIDER_WIDTH);
        assert_eq!(
            wide.center.width,
            2400.0 - RAIL_WIDTH - left - right - 2.0 * SIDEBAR_DIVIDER_WIDTH
        );
        let narrow = Layout::for_size_with_sidebars(700.0, 800.0, left, right);
        assert!(narrow.inspector.is_none());
        assert!(narrow.navigation.unwrap().width < left);
        assert_eq!(narrow.center.width, MIN_CENTER_WIDTH);
        assert_eq!(
            Layout::for_size_with_sidebars(2400.0, 800.0, left, right),
            wide,
            "a temporary clamp must not become a new preferred width"
        );
        let right_only = Layout::for_size_with_sidebars(1000.0, 800.0, 0.0, 306.0);
        assert!(right_only.navigation.is_none());
        assert_eq!(right_only.inspector.unwrap().width, 306.0);
        assert_eq!(right_only.center.x, RAIL_WIDTH);
        assert_eq!(right_only.center.width, 1000.0 - RAIL_WIDTH - 306.0 - 6.0);
    }

    #[test]
    fn interactive_sidebars_bound_invalid_preferences_and_extreme_windows() {
        for width in [0.0, 64.0, 320.0, 640.0, 800.0, 1000.0, 1356.0, 3000.0] {
            for (left, right) in [
                (0.0, 0.0),
                (120.0, 240.0),
                (237.0, 306.0),
                (9000.0, 9000.0),
                (f32::NAN, f32::INFINITY),
                (-1.0, -1.0),
            ] {
                let layout = Layout::for_size_with_sidebars(width, 704.0, left, right);
                for rect in [
                    Some(layout.rail),
                    layout.navigation,
                    Some(layout.center),
                    layout.inspector,
                    Some(layout.composer),
                ]
                .into_iter()
                .flatten()
                {
                    assert!(rect.x.is_finite() && rect.width.is_finite());
                    assert!(rect.x >= 0.0 && rect.width >= 0.0);
                    assert!(rect.x + rect.width <= width + 0.01);
                }
                if width >= RAIL_WIDTH + MIN_CENTER_WIDTH {
                    assert!(layout.center.width >= MIN_CENTER_WIDTH);
                }
            }
        }
        let huge = Layout::for_size_with_sidebars(3000.0, 800.0, 9000.0, 9000.0);
        assert_eq!(huge.navigation.unwrap().width, MAX_NAVIGATION_WIDTH);
        assert_eq!(huge.inspector.unwrap().width, MAX_INSPECTOR_WIDTH);
        let invalid = Layout::for_size_with_sidebars(f32::NAN, f32::INFINITY, f32::NAN, f32::NAN);
        assert_eq!(invalid.titlebar.width, REFERENCE_WIDTH);
        assert_eq!(invalid.navigation.unwrap().width, NAVIGATION_WIDTH);
        assert_eq!(invalid.inspector.unwrap().width, INSPECTOR_WIDTH);
    }

    #[test]
    fn hidden_inspector_releases_its_width_without_moving_other_chrome() {
        for width in [1356.0, 1078.0, 800.0, 700.0, 0.0] {
            let shown = Layout::for_size(width, 704.0);
            let hidden = shown.without_inspector();
            assert!(hidden.inspector.is_none());
            assert_eq!(hidden.titlebar, shown.titlebar);
            assert_eq!(hidden.rail, shown.rail);
            assert_eq!(hidden.navigation, shown.navigation);
            assert_eq!(hidden.header.width, hidden.center.width);
            assert_eq!(hidden.center.x + hidden.center.width, width);
            assert_eq!(hidden.composer.y, shown.composer.y);
            assert_eq!(hidden.composer.height, shown.composer.height);
            assert!(hidden.composer.x >= hidden.center.x);
            assert!(hidden.composer.x + hidden.composer.width <= width);
            if shown.inspector.is_some() {
                assert_eq!(hidden.center.width - shown.center.width, INSPECTOR_WIDTH);
            } else {
                assert_eq!(hidden, shown);
            }
        }
    }

    #[test]
    fn reference_geometry_and_fixed_gaps_match_the_measured_contract() {
        let layout = Layout::for_size(1356.0, 704.0);
        assert_eq!(
            layout.center,
            Rect {
                x: 292.0,
                y: 42.0,
                width: 758.0,
                height: 662.0
            }
        );
        assert_eq!(
            layout.composer,
            Rect {
                x: 320.0,
                y: 613.0,
                width: 702.0,
                height: 80.0
            }
        );
        assert_eq!(layout.inspector.unwrap().x, 1050.0);
    }

    #[test]
    fn compact_windows_collapse_panels_before_overlapping_the_input() {
        for (width, nav, inspector) in [
            (1356.0, true, true),
            (1000.0, true, false),
            (700.0, false, false),
        ] {
            let layout = Layout::for_size(width, 704.0);
            assert_eq!(layout.navigation.is_some(), nav);
            assert_eq!(layout.inspector.is_some(), inspector);
            assert!(layout.center.width >= 480.0);
            assert!(layout.composer.x >= layout.center.x);
            assert!(
                layout.composer.x + layout.composer.width <= layout.center.x + layout.center.width
            );
        }
    }

    #[test]
    fn legacy_backend_values_preserve_a_supported_presentation() {
        for value in ["normal", "vte", "unified", "block", " Block "] {
            assert_eq!(Presentation::parse_legacy(value), Some(Presentation::Block));
        }
        assert_eq!(Presentation::parse_legacy("chat"), Some(Presentation::Chat));
        assert_eq!(Presentation::parse_legacy("unsupported"), None);
    }
}
