//! notify — fire-and-forget desktop notification for long-running blocks.
//!
//! Shells out to `notify-send` rather than wiring `gio::Notification`. The
//! TermView block-finished callback runs without a window/application
//! handle in scope (would require threading one through `TermView::new`),
//! and notify-send is universally available on Linux desktops (libnotify
//! is a near-mandatory dep of every major DE). The subprocess cost is one
//! fork+exec per long-running command — negligible compared to whatever
//! the command itself just spent doing.
//!
//! Errors are intentionally swallowed: if notify-send is missing or
//! D-Bus is broken, the user shouldn't see a stack trace from a feature
//! that's meant to be unobtrusive.

use std::process::Stdio;
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::OnceLock;
use std::time::Duration;

const NOTIFICATION_QUEUE_CAPACITY: usize = 16;
const NOTIFY_SEND_TIMEOUT: Duration = Duration::from_secs(3);

struct Notification {
    urgency: &'static str,
    timeout_ms: &'static str,
    title: String,
    body: String,
}

static NOTIFICATION_WORKER: OnceLock<Option<SyncSender<Notification>>> = OnceLock::new();

/// Post a desktop notification for a command that just finished. `cmd` is
/// the displayed command (truncated to keep the toast readable);
/// `exit_code` drives the urgency hint (non-zero → critical, since failed
/// long builds are the case users most want to come back to).
///
/// `duration_ms` shows up in the body so the user knows whether they
/// have time to refill their coffee.
pub fn long_block_finished(cmd: &str, exit_code: i32, duration_ms: u64) {
    // Truncate the cmd so the notification title stays one line.
    let title_cmd = notification_title(cmd);

    let status = if exit_code == 0 { "✓" } else { "✗" };
    let title = format!("{status} {title_cmd}");
    let exit_text = match crate::exit_status::signal_name_for_exit(exit_code) {
        Some(sig) => format!("Exit {exit_code} ({sig})"),
        None => format!("Exit {exit_code}"),
    };
    let body = format!("{exit_text} after {}", humanize_duration(duration_ms));

    let urgency = if exit_code == 0 { "normal" } else { "critical" };

    // -t 0 = sticky-until-dismissed by some servers; -t 8000 = 8s. We pick a
    // mid value (5s) so success toasts decay quickly but failures still get
    // a moment of attention.
    let timeout_ms = if exit_code == 0 { "5000" } else { "10000" };

    spawn_notify_send(urgency, timeout_ms, &title, &body);
}

/// Post an application-driven desktop notification (OSC 9 / OSC 777). The
/// shared parser normally bounds and sanitises these fields, and this final
/// sink repeats that contract for direct callers. Callers are expected to
/// rate-limit. A missing title falls back to the app identity so toasts stay
/// attributable.
pub fn app_notification(title: Option<&str>, body: &str) {
    let title = title
        .map(safe_notification_field)
        .filter(|title| !title.is_empty())
        .unwrap_or_else(|| safe_notification_field(crate::identity::get().app_name));
    let body = safe_notification_field(body);
    spawn_notify_send("normal", "5000", &title, &body);
}

/// Queue a toast for one bounded worker. A stuck D-Bus bridge can otherwise
/// leave one process and one reaper thread behind for every notification.
fn spawn_notify_send(urgency: &'static str, timeout_ms: &'static str, title: &str, body: &str) {
    let Some(sender) = notification_worker() else {
        return;
    };
    let notification = Notification {
        urgency,
        timeout_ms,
        title: title.to_owned(),
        body: body.to_owned(),
    };
    match sender.try_send(notification) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => log::warn!("desktop notification queue is full"),
        Err(TrySendError::Disconnected(_)) => {
            log::warn!("desktop notification worker is unavailable")
        }
    }
}

fn notification_worker() -> Option<&'static SyncSender<Notification>> {
    NOTIFICATION_WORKER
        .get_or_init(|| {
            let (sender, receiver) =
                std::sync::mpsc::sync_channel::<Notification>(NOTIFICATION_QUEUE_CAPACITY);
            match std::thread::Builder::new()
                .name("jterm-notification".to_string())
                .spawn(move || {
                    while let Ok(notification) = receiver.recv() {
                        send_notification(notification);
                    }
                }) {
                Ok(_) => Some(sender),
                Err(error) => {
                    log::warn!("failed to start desktop notification worker: {error}");
                    None
                }
            }
        })
        .as_ref()
}

fn send_notification(notification: Notification) {
    let identity = crate::identity::get();
    let app_name_arg = format!("--app-name={}", identity.app_name);
    let icon_arg = format!("--icon={}", identity.app_id);
    let Ok(mut command) = crate::host::helper_command("notify-send") else {
        return;
    };
    command
        .args([
            app_name_arg.as_str(),
            icon_arg.as_str(),
            "--urgency",
            notification.urgency,
            "--expire-time",
            notification.timeout_ms,
            "--",
            notification.title.as_str(),
            notification.body.as_str(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Err(error) = crate::host::command_status_with_timeout(command, NOTIFY_SEND_TIMEOUT) {
        log::warn!("desktop notification subprocess failed: {error}");
    }
}

fn notification_title(cmd: &str) -> String {
    const MAX_CHARS: usize = 60;

    let first_line = cmd.split(['\r', '\n']).next().unwrap_or(cmd);
    let mut chars = first_line.chars();
    let mut title = String::new();
    for ch in chars.by_ref().take(MAX_CHARS) {
        title.push(visible_notification_character(ch));
    }
    if chars.next().is_some() {
        title.push('…');
    }
    title
}

fn safe_notification_field(raw: &str) -> String {
    raw.chars()
        .map(visible_notification_character)
        .take(crate::parser::MAX_NOTIFICATION_CHARS)
        .collect::<String>()
        .trim()
        .to_owned()
}

fn visible_notification_character(ch: char) -> char {
    if ch.is_control() || crate::review_input::is_visual_spoofing_character(ch) {
        '\u{fffd}'
    } else {
        ch
    }
}

/// Render a millisecond count as a short human string. Used in the
/// notification body so "exit 0 after 12m 4s" reads naturally instead of
/// "exit 0 after 724000ms".
fn humanize_duration(ms: u64) -> String {
    let secs = ms / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        let m = secs / 60;
        let s = secs % 60;
        if s == 0 {
            format!("{m}m")
        } else {
            format!("{m}m {s}s")
        }
    } else {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        if m == 0 {
            format!("{h}h")
        } else {
            format!("{h}h {m}m")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn humanize_seconds_only() {
        assert_eq!(humanize_duration(0), "0s");
        assert_eq!(humanize_duration(7_500), "7s");
        assert_eq!(humanize_duration(59_999), "59s");
    }

    #[test]
    fn humanize_minutes_round() {
        assert_eq!(humanize_duration(60_000), "1m");
        assert_eq!(humanize_duration(120_000), "2m");
    }

    #[test]
    fn humanize_minutes_and_seconds() {
        assert_eq!(humanize_duration(125_000), "2m 5s");
        assert_eq!(humanize_duration(3_599_000), "59m 59s");
    }

    #[test]
    fn humanize_hours() {
        assert_eq!(humanize_duration(3_600_000), "1h");
        assert_eq!(humanize_duration(3_660_000), "1h 1m");
        assert_eq!(humanize_duration(7_200_000), "2h");
    }

    #[test]
    fn notification_title_truncates_cjk_and_emoji_on_char_boundaries() {
        for cmd in [
            format!("a{}", "界".repeat(60)),
            format!("a{}", "🙂".repeat(60)),
        ] {
            let title = notification_title(&cmd);
            assert!(title.ends_with('…'));
            assert_eq!(title.chars().count(), 61);
            assert_eq!(
                title.chars().take(60).collect::<String>(),
                cmd.chars().take(60).collect::<String>()
            );
        }
    }

    #[test]
    fn notification_sink_bounds_and_exposes_untrusted_formatting() {
        assert_eq!(
            notification_title("echo\tleft\u{202e}right\u{00a0}tail\nignored"),
            "echo\u{fffd}left\u{fffd}right\u{fffd}tail"
        );
        let long = "x".repeat(crate::parser::MAX_NOTIFICATION_CHARS + 1);
        assert_eq!(
            safe_notification_field(&long).chars().count(),
            crate::parser::MAX_NOTIFICATION_CHARS
        );
    }
}
