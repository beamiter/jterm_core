//! Presentation for the shared native Tools menu. Adapters retain action authority.

pub const WIDTH: f32 = 264.0;
pub const ROW_HEIGHT: f32 = 30.0;
pub const PADDING: f32 = 8.0;
pub const RADIUS: f32 = 12.0;
pub const ICON_SIZE: f32 = 14.0;
pub const SURFACE: [u8; 4] = [22, 33, 51, 255];
pub const BORDER: [u8; 4] = [57, 73, 96, 255];
pub const FOREGROUND: [u8; 4] = [225, 233, 246, 255];
pub const MUTED: [u8; 4] = [145, 162, 187, 255];
pub const SELECTED: [u8; 4] = [40, 61, 111, 255];

/// Inner metrics for the existing, already mode-stable footer controls.
/// These do not change the composer frame, footer baseline or action ownership.
pub const FOOTER_ICON_SIZE: f32 = 12.0;
pub const FOOTER_LABEL_SIZE: f32 = 10.0;
pub const FOOTER_ICON_LABEL_GAP: f32 = 4.0;
pub const FOOTER_HORIZONTAL_PADDING: f32 = 6.0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolCommandId {
    Terminal,
    CommandPalette,
    SearchCanvas,
    Attach,
    Image,
    Code,
}

#[derive(Clone, Copy, Debug)]
pub struct ToolCommand {
    pub id: ToolCommandId,
    pub icon_key: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub placeholder: bool,
    pub separator_before: bool,
}

/// Real entries route to existing actions only when that adapter has them.
/// Unavailable entries retain their geometry and cannot execute shell input.
pub const COMMANDS: &[ToolCommand] = &[
    ToolCommand {
        id: ToolCommandId::Terminal,
        icon_key: "terminal",
        title: "Terminal",
        description: "Focus input",
        placeholder: false,
        separator_before: false,
    },
    ToolCommand {
        id: ToolCommandId::CommandPalette,
        icon_key: "search",
        title: "Command palette",
        description: "Commands",
        placeholder: false,
        separator_before: false,
    },
    ToolCommand {
        id: ToolCommandId::SearchCanvas,
        icon_key: "history",
        title: "Search canvas",
        description: "History",
        placeholder: false,
        separator_before: false,
    },
    ToolCommand {
        id: ToolCommandId::Attach,
        icon_key: "attach",
        title: "Attach file",
        description: "Unavailable",
        placeholder: true,
        separator_before: true,
    },
    ToolCommand {
        id: ToolCommandId::Image,
        icon_key: "image",
        title: "Insert image",
        description: "Unavailable",
        placeholder: true,
        separator_before: false,
    },
    ToolCommand {
        id: ToolCommandId::Code,
        icon_key: "code",
        title: "Insert code",
        description: "Unavailable",
        placeholder: true,
        separator_before: false,
    },
];
