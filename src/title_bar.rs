//! Shared window title presentation for Anvil, Forge, Ember and Frost.
//!
//! Renderers use a 32px (or font size + 12px) title row above the tab/action
//! row. The title expands and elides before the trailing window controls;
//! metadata disappears progressively in narrow windows. The full, sanitized
//! context remains in a tooltip. No filesystem or process probes happen here.

use std::path::Path;

pub fn height(font_size: f32) -> f32 {
    32.0_f32.max(font_size + 12.0)
}

#[derive(Default)]
pub struct Snapshot<'a> {
    pub app: &'a str,
    pub title: &'a str,
    pub cwd: Option<&'a Path>,
    pub home: Option<&'a Path>,
    pub mode: &'a str,
    pub tab_index: usize,
    pub tab_count: usize,
    pub pane_count: usize,
    pub private: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Content {
    pub heading: String,
    pub details: String,
    pub compact_details: String,
    pub tooltip: String,
    pub window_title: String,
}

impl Content {
    /// Width of the entire window in logical pixels, identical in each UI.
    pub fn details_for_width(&self, width: f32) -> &str {
        if width < 600.0 {
            ""
        } else if width < 1000.0 {
            &self.compact_details
        } else {
            &self.details
        }
    }
}

/// Bound and de-spoof shell-controlled chrome, including OSC titles and paths.
fn clean(value: &str, limit: usize) -> String {
    let mut chars = value.chars().filter(|c| {
        !c.is_control()
            && !matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
    });
    let mut out: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        out.push('…');
    }
    out.trim().to_string()
}

pub fn compose(snapshot: &Snapshot<'_>) -> Content {
    let app = clean(snapshot.app, 24);
    let title = if snapshot.private {
        "Private".to_string()
    } else {
        clean(snapshot.title, 160)
    };
    let heading = if title.is_empty() || title.eq_ignore_ascii_case(&app) {
        app.clone()
    } else {
        format!("{app} · {title}")
    };
    let mode = match snapshot.mode {
        "block" => "Blocks",
        "chat" => "Chat",
        "unified" => "Unified",
        _ => "Terminal",
    };
    let tabs = if snapshot.tab_count == 0 {
        "No tabs".to_string()
    } else {
        format!(
            "Tab {}/{}",
            snapshot.tab_index.min(snapshot.tab_count - 1) + 1,
            snapshot.tab_count
        )
    };
    let compact_details = format!("{mode} · {tabs}");
    let mut details = Vec::new();
    if !snapshot.private {
        if let Some(cwd) = snapshot.cwd {
            let path = match snapshot.home.filter(|home| !home.as_os_str().is_empty()) {
                Some(home) => match cwd.strip_prefix(home) {
                    Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
                    Ok(rest) => format!("~/{}", rest.display()),
                    Err(_) => cwd.display().to_string(),
                },
                None => cwd.display().to_string(),
            };
            let path = clean(&path, 240);
            if !path.is_empty() {
                details.push(path);
            }
        }
    }
    details.push(compact_details.clone());
    if snapshot.pane_count > 1 {
        details.push(format!("{} panes", snapshot.pane_count));
    }
    let details = details.join(" · ");
    Content {
        tooltip: format!("{heading}\n{details}"),
        window_title: heading.clone(),
        heading,
        details,
        compact_details,
    }
}

/// GTK's Paned can shrink its children, independently of their natural sizes.
/// Keep a useful dock and at least 240px for terminal content when possible.
pub fn sidebar_position(position: i32, window_width: i32) -> i32 {
    position.clamp(120, (window_width - 240).clamp(120, 800))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_context_never_reaches_any_title_or_tooltip() {
        let content = compose(&Snapshot {
            app: "Ember",
            title: "secret",
            cwd: Some(Path::new("/secret/project")),
            private: true,
            mode: "chat",
            tab_count: 2,
            pane_count: 2,
            ..Default::default()
        });
        assert_eq!(content.heading, "Ember · Private");
        assert!(!format!("{content:?}").contains("secret"));
        assert_eq!(content.details_for_width(500.0), "");
        assert_eq!(content.details_for_width(800.0), "Chat · Tab 1/2");
        assert!(content.details_for_width(1200.0).contains("2 panes"));
    }

    #[test]
    fn paths_use_component_boundaries_and_titles_are_bounded() {
        let mut snapshot = Snapshot {
            app: "Forge",
            title: "\x1bhello\n\u{202e}",
            cwd: Some(Path::new("/home/user2/work")),
            home: Some(Path::new("/home/user")),
            tab_index: usize::MAX,
            tab_count: 2,
            ..Default::default()
        };
        let content = compose(&snapshot);
        assert_eq!(content.heading, "Forge · hello");
        assert!(content.details.starts_with("/home/user2/work"));
        assert!(content.details.ends_with("Tab 2/2"));
        snapshot.cwd = Some(Path::new("/home/user/work"));
        assert!(compose(&snapshot).details.starts_with("~/work"));
        let long = "中".repeat(500);
        snapshot.title = &long;
        assert!(compose(&snapshot).heading.chars().count() < 180);
        assert_eq!(sidebar_position(0, 1200), 120);
        assert_eq!(sidebar_position(1200, 800), 560);
        assert_eq!(sidebar_position(1200, 2000), 800);
    }
}
