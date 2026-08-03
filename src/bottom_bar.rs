//! The family-wide bottom status bar: one content contract, four renderers.
//!
//! Every jterm draws its own bar with its own toolkit, but what the bar says
//! and how it reads must not depend on which terminal happens to be open.
//! This module owns that contract: the app collects a [`Snapshot`] from state
//! it already tracks, [`compose`] turns it into ordered [`Segment`]s, and the
//! renderer draws each segment's text verbatim in the segment's [`Tone`]
//! color. Nothing here draws anything and nothing here probes anything — the
//! git meta, cwd and command metadata all arrive through the app's existing
//! plumbing (OSC 7, OSC 133, [`crate::git_meta`]).
//!
//! Renderer contract, so the four bars also *look* alike:
//!   * container: `theme.tabbar.bg` background, 1px `theme.ui.border` top
//!     border, ~22px tall;
//!   * text: small chrome size (11px), one label per segment, colored via
//!     [`Tone::color`];
//!   * layout: [`Content::left`] packed from the left edge, [`Content::right`]
//!     from the right, segments separated by a wide space;
//!   * the whole bar is gated by a `bottom_bar` boolean in the app's config
//!     file — same key in every jterm — defaulting to [`ENABLED_BY_DEFAULT`].

use std::path::Path;

use crate::git_meta::RepoMeta;
use crate::theme::Theme;

/// The config key every jterm uses for the toggle, so a user who learned
/// `bottom_bar = false` in one config file knows all four.
pub const CONFIG_KEY: &str = "bottom_bar";

/// The bar ships visible: a toggle nobody notices protects a feature nobody
/// finds.
pub const ENABLED_BY_DEFAULT: bool = true;

/// Canonical bar height in logical pixels. Layout code that reserves room for
/// the bar (grid sizing, overlay offsets) should use this rather than a local
/// constant, so the four terminals stay in step.
pub const BAR_HEIGHT: f32 = 22.0;

/// Longest rendered cwd before the head is elided.
const MAX_CWD_CHARS: usize = 48;

/// How a segment should be colored. The mapping to actual colors lives in
/// [`Tone::color`] so all four terminals resolve a tone identically.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    /// Regular chrome text.
    Normal,
    /// De-emphasized: metadata the eye should skip until asked for.
    Muted,
    /// A success — the last command exited 0.
    Positive,
    /// A failure — the last command exited nonzero.
    Negative,
}

impl Tone {
    /// Resolve against the active theme. Positive/negative reuse the ANSI
    /// green/red so the bar agrees with what the shell itself just printed.
    pub fn color(self, theme: &Theme) -> [u8; 3] {
        match self {
            Tone::Normal => theme.ui.text,
            Tone::Muted => theme.ui.text_disabled,
            Tone::Positive => theme.terminal.ansi_colors[2],
            Tone::Negative => theme.terminal.ansi_colors[1],
        }
    }
}

/// What a segment is about. Renderers should not branch on this for styling —
/// [`Segment::tone`] already carries that — but it lets an app attach
/// behavior (a click target, a tooltip) to a specific segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SegmentKind {
    Cwd,
    Git,
    LastCommand,
    Grid,
    Tabs,
}

/// One label on the bar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    pub kind: SegmentKind,
    pub text: String,
    pub tone: Tone,
}

/// Everything the bar can say, gathered by the app from state it already
/// tracks. Every field is optional in spirit: a default snapshot composes to
/// an empty bar rather than to placeholders.
#[derive(Clone, Copy, Debug, Default)]
pub struct Snapshot<'a> {
    /// Working directory of the focused pane (OSC 7, `/proc`, or the shell's
    /// OSC 133 `cwd_url` — whatever the app already believes).
    pub cwd: Option<&'a Path>,
    /// The user's home, for `~` abbreviation. Passed in rather than read from
    /// the environment so composition stays deterministic and testable.
    pub home: Option<&'a Path>,
    /// Git metadata for that cwd, from the app's existing
    /// [`crate::git_meta`] cache. `None` renders nothing: outside a repo the
    /// bar should not say "no repo".
    pub git: Option<&'a RepoMeta>,
    /// A command is currently running in the focused pane. Takes precedence
    /// over the finished-command fields.
    pub running: bool,
    /// Exit code of the last finished command in the focused pane.
    pub last_exit: Option<i32>,
    /// Wall-clock duration of that command, when the shell reported one.
    pub last_duration_ms: Option<u64>,
    /// Grid size of the focused pane; 0 in either axis means unknown.
    pub cols: u16,
    pub rows: u16,
    /// Zero-based active tab and total tab count. The segment only appears
    /// with more than one tab: `[1/1]` is noise.
    pub tab_index: usize,
    pub tab_count: usize,
}

/// The composed bar: left group grows from the left edge, right group from
/// the right.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Content {
    pub left: Vec<Segment>,
    pub right: Vec<Segment>,
}

/// Turn a snapshot into the family-wide segment list. Pure and total: no
/// clocks, no filesystem, no environment.
pub fn compose(snapshot: &Snapshot) -> Content {
    let mut left = Vec::new();
    if let Some(cwd) = snapshot.cwd {
        left.push(Segment {
            kind: SegmentKind::Cwd,
            text: display_cwd(cwd, snapshot.home),
            tone: Tone::Normal,
        });
    }
    if let Some(meta) = snapshot.git {
        left.push(Segment {
            kind: SegmentKind::Git,
            text: crate::git_meta::format_strip(meta),
            tone: Tone::Muted,
        });
    }

    let mut right = Vec::new();
    if let Some(segment) = last_command_segment(snapshot) {
        right.push(segment);
    }
    if snapshot.cols > 0 && snapshot.rows > 0 {
        right.push(Segment {
            kind: SegmentKind::Grid,
            text: format!("{}×{}", snapshot.cols, snapshot.rows),
            tone: Tone::Muted,
        });
    }
    if snapshot.tab_count > 1 {
        let shown = (snapshot.tab_index + 1).min(snapshot.tab_count);
        right.push(Segment {
            kind: SegmentKind::Tabs,
            text: format!("[{}/{}]", shown, snapshot.tab_count),
            tone: Tone::Muted,
        });
    }

    Content { left, right }
}

fn last_command_segment(snapshot: &Snapshot) -> Option<Segment> {
    if snapshot.running {
        return Some(Segment {
            kind: SegmentKind::LastCommand,
            text: "…".to_string(),
            tone: Tone::Muted,
        });
    }
    let exit = snapshot.last_exit?;
    let mut text;
    let tone;
    if exit == 0 {
        text = "✓".to_string();
        tone = Tone::Positive;
    } else {
        text = format!("✗ {exit}");
        if let Some(signal) = crate::exit_status::signal_name_for_exit(exit) {
            text.push_str(&format!(" {signal}"));
        }
        tone = Tone::Negative;
    }
    if let Some(ms) = snapshot.last_duration_ms {
        text.push_str(&format!(" · {}", crate::notify::humanize_duration(ms)));
    }
    Some(Segment {
        kind: SegmentKind::LastCommand,
        text,
        tone,
    })
}

/// Render a cwd the way every jterm's bar shows it: home collapsed to `~`,
/// and when the result is still longer than [`MAX_CWD_CHARS`], the head
/// elided down to the last two components (`…/parent/leaf`). Deliberately
/// not fish-style single-letter abbreviation: a bar is read at a glance, and
/// `…/jterm_core/src` answers "where am I" better than `~/p/j/src`.
pub fn display_cwd(cwd: &Path, home: Option<&Path>) -> String {
    let full = match home.and_then(|home| cwd.strip_prefix(home).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Some(rest) => format!("~/{}", rest.display()),
        None => cwd.display().to_string(),
    };
    if full.chars().count() <= MAX_CWD_CHARS {
        return full;
    }
    let mut tail: Vec<&str> = full.split('/').rev().take(2).collect();
    tail.reverse();
    format!("…/{}", tail.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(branch: &str, dirty: bool) -> RepoMeta {
        RepoMeta {
            branch: branch.to_string(),
            dirty,
            ahead: None,
            behind: None,
        }
    }

    #[test]
    fn an_empty_snapshot_composes_to_an_empty_bar() {
        assert_eq!(compose(&Snapshot::default()), Content::default());
    }

    #[test]
    fn cwd_and_git_sit_left_in_that_order() {
        let repo = meta("master", true);
        let content = compose(&Snapshot {
            cwd: Some(Path::new("/home/u/projects/jterm_core")),
            home: Some(Path::new("/home/u")),
            git: Some(&repo),
            ..Snapshot::default()
        });
        let texts: Vec<(&SegmentKind, &str)> = content
            .left
            .iter()
            .map(|segment| (&segment.kind, segment.text.as_str()))
            .collect();
        assert_eq!(
            texts,
            vec![
                (&SegmentKind::Cwd, "~/projects/jterm_core"),
                (&SegmentKind::Git, "master ●"),
            ]
        );
        assert!(content.right.is_empty());
    }

    #[test]
    fn the_home_directory_itself_reads_as_a_tilde() {
        assert_eq!(
            display_cwd(Path::new("/home/u"), Some(Path::new("/home/u"))),
            "~"
        );
    }

    #[test]
    fn a_cwd_outside_home_stays_absolute() {
        assert_eq!(
            display_cwd(Path::new("/etc/nginx"), Some(Path::new("/home/u"))),
            "/etc/nginx"
        );
    }

    #[test]
    fn a_deep_cwd_elides_to_its_last_two_components() {
        let deep = format!("/home/u/{}/jterm_core/src", "x".repeat(60));
        assert_eq!(
            display_cwd(Path::new(&deep), Some(Path::new("/home/u"))),
            "…/jterm_core/src"
        );
    }

    #[test]
    fn a_cwd_at_the_limit_is_not_elided() {
        let at_limit = format!("/{}", "y".repeat(MAX_CWD_CHARS - 1));
        assert_eq!(display_cwd(Path::new(&at_limit), None), at_limit);
    }

    #[test]
    fn a_running_command_shows_an_ellipsis_and_masks_the_last_exit() {
        let content = compose(&Snapshot {
            running: true,
            last_exit: Some(1),
            last_duration_ms: Some(2_000),
            ..Snapshot::default()
        });
        assert_eq!(content.right.len(), 1);
        assert_eq!(content.right[0].text, "…");
        assert_eq!(content.right[0].tone, Tone::Muted);
    }

    #[test]
    fn success_reads_as_a_check_with_the_duration() {
        let content = compose(&Snapshot {
            last_exit: Some(0),
            last_duration_ms: Some(125_000),
            ..Snapshot::default()
        });
        assert_eq!(content.right[0].text, "✓ · 2m 5s");
        assert_eq!(content.right[0].tone, Tone::Positive);
    }

    #[test]
    fn failure_names_the_signal_the_way_the_block_ui_does() {
        let content = compose(&Snapshot {
            last_exit: Some(130),
            last_duration_ms: Some(500),
            ..Snapshot::default()
        });
        assert_eq!(content.right[0].text, "✗ 130 SIGINT · 0s");
        assert_eq!(content.right[0].tone, Tone::Negative);
    }

    #[test]
    fn a_plain_failure_has_no_signal_suffix() {
        let content = compose(&Snapshot {
            last_exit: Some(1),
            ..Snapshot::default()
        });
        assert_eq!(content.right[0].text, "✗ 1");
    }

    #[test]
    fn grid_and_tabs_sit_right_and_single_tab_says_nothing() {
        let content = compose(&Snapshot {
            cols: 80,
            rows: 24,
            tab_index: 0,
            tab_count: 1,
            ..Snapshot::default()
        });
        let texts: Vec<&str> = content
            .right
            .iter()
            .map(|segment| segment.text.as_str())
            .collect();
        assert_eq!(texts, vec!["80×24"]);

        let content = compose(&Snapshot {
            cols: 80,
            rows: 24,
            tab_index: 2,
            tab_count: 5,
            ..Snapshot::default()
        });
        let texts: Vec<&str> = content
            .right
            .iter()
            .map(|segment| segment.text.as_str())
            .collect();
        assert_eq!(texts, vec!["80×24", "[3/5]"]);
    }

    #[test]
    fn a_stale_tab_index_clamps_instead_of_overcounting() {
        let content = compose(&Snapshot {
            tab_index: 9,
            tab_count: 3,
            ..Snapshot::default()
        });
        assert_eq!(content.right[0].text, "[3/3]");
    }

    #[test]
    fn tones_resolve_against_the_theme_identically_everywhere() {
        let theme = Theme::builtin_dark();
        assert_eq!(Tone::Normal.color(&theme), theme.ui.text);
        assert_eq!(Tone::Muted.color(&theme), theme.ui.text_disabled);
        assert_eq!(Tone::Positive.color(&theme), theme.terminal.ansi_colors[2]);
        assert_eq!(Tone::Negative.color(&theme), theme.terminal.ansi_colors[1]);
    }

    #[test]
    fn the_toggle_contract_is_pinned() {
        assert_eq!(
            (CONFIG_KEY, ENABLED_BY_DEFAULT, BAR_HEIGHT),
            ("bottom_bar", true, 22.0)
        );
    }
}
