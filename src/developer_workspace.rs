//! Dark Block presentation metrics layered on the common four-app shell.
//!
//! The later 1672x941 reference changes the Block command surface. It does not
//! establish a second global toolbar/sidebar/composer contract for GTK apps.
//! All adapters now use the measured 1356x704 overall shell through this export;
//! only the terminal-card display constants below remain Block-specific.

pub use crate::reference_shell::*;

pub const LIVE_TOOLBAR_HEIGHT: f32 = 36.0;
pub const CARD_HORIZONTAL_INSET: f32 = 12.0;
pub const CARD_GAP: f32 = 10.0;
pub const DEFAULT_CONTENT_FONT_DESC: &str = "JetBrainsMono Nerd Font Mono 12px";
/// Deliberately independent from the content font's pixel units.
pub const DEFAULT_UI_FONT_SIZE: &str = "14";
pub const DEFAULT_UI_FONT_SCALE: f64 = 1.0;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_adapters_share_the_reference_outer_shell() {
        let layout = Layout::for_size(1356.0, 704.0);
        assert_eq!(layout.titlebar.height, 42.0);
        assert_eq!(layout.rail.width, 55.0);
        assert_eq!(layout.navigation.unwrap().width, 237.0);
        assert_eq!(layout.inspector.unwrap().width, 306.0);
        assert_eq!(layout.center.x, 292.0);
        assert_eq!(layout.header.height, 49.0);
        assert_eq!(
            layout.composer,
            Rect {
                x: 320.0,
                y: 613.0,
                width: 702.0,
                height: 80.0
            }
        );
        for (width, nav, inspector) in [
            (1672.0, true, true),
            (1356.0, true, true),
            (1000.0, true, false),
            (800.0, true, false),
            (700.0, false, false),
        ] {
            let shared = crate::reference_shell::Layout::for_size(width, 704.0);
            let block_shell = Layout::for_size(width, 704.0);
            assert_eq!(shared, block_shell);
            assert_eq!(block_shell.navigation.is_some(), nav);
            assert_eq!(block_shell.inspector.is_some(), inspector);
        }
    }

    #[test]
    fn block_card_density_does_not_override_global_chrome() {
        assert_eq!(LIVE_TOOLBAR_HEIGHT, 36.0);
        assert_eq!(CARD_HORIZONTAL_INSET, 12.0);
        assert_eq!(CARD_GAP, 10.0);
        assert_eq!(
            DEFAULT_CONTENT_FONT_DESC,
            "JetBrainsMono Nerd Font Mono 12px"
        );
        assert_eq!(MODE_CONTROL_HEIGHT, 28.0);
        assert_eq!(COMPOSER_HEIGHT, 80.0);
    }

    #[test]
    fn shared_footer_slots_fit_the_reference_and_compact_widths() {
        let full_tools = COMPOSER_PLUS_WIDTH
            + COMPOSER_ATTACH_WIDTH
            + COMPOSER_IMAGE_WIDTH
            + COMPOSER_CODE_WIDTH
            + COMPOSER_TERMINAL_WIDTH
            + COMPOSER_TOOLS_WIDTH
            + 5.0 * METADATA_GAP;
        assert_eq!(full_tools, 380.0);
        let full = full_tools
            + COMPOSER_MODEL_MIN_WIDTH
            + COMPOSER_SEND_WIDTH
            + 2.0 * METADATA_GAP
            + COMPOSER_CONTROL_LEFT
            + COMPOSER_CONTROL_RIGHT;
        assert!(full <= COMPOSER_SECONDARY_MIN_WIDTH);
        let compact = COMPOSER_PLUS_WIDTH
            + COMPOSER_TERMINAL_WIDTH
            + COMPOSER_TOOLS_WIDTH
            + COMPOSER_MODEL_MIN_WIDTH
            + COMPOSER_SEND_WIDTH
            + 4.0 * METADATA_GAP
            + COMPOSER_CONTROL_LEFT
            + COMPOSER_CONTROL_RIGHT;
        assert!(compact <= COMPOSER_MODEL_MIN_AVAILABLE);
        let tiny = COMPOSER_PLUS_WIDTH
            + COMPOSER_TOOLS_WIDTH
            + COMPOSER_SEND_WIDTH
            + 2.0 * METADATA_GAP
            + COMPOSER_CONTROL_LEFT
            + COMPOSER_CONTROL_RIGHT;
        assert!(tiny <= Layout::for_size(320.0, 240.0).composer.width);
    }
}
