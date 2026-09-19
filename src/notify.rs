//! notify — fire-and-forget desktop notification for long-running blocks,
//! application requests (OSC 9 / 777) and bells from an unfocused window.
//! The pure policies that decide whether a toast is worth posting
//! ([`long_block_should_notify`], [`bell_should_notify`]) live here too, so
//! every frontend draws the line in the same place.
//!
//! Shells out to `notify-send` rather than wiring `gio::Notification`. The
//! TermView block-finished callback runs without a window/application
//! handle in scope (would require threading one through `TermView::new`),
//! and notify-send is universally available on Linux desktops (libnotify
//! is a near-mandatory dep of every major DE). The subprocess cost is one
//! fork+exec per long-running command — negligible compared to whatever
//! the command itself just spent doing.
//!
//! Execution goes through [`crate::helper`]'s trusted boundary: the binary
//! is resolved from fixed absolute system candidates and run under the
//! family's output caps and deadline, so this module only owns queueing and
//! field sanitisation.
//!
//! Errors are intentionally swallowed: if notify-send is missing or
//! D-Bus is broken, the user shouldn't see a stack trace from a feature
//! that's meant to be unobtrusive.

use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const NOTIFICATION_QUEUE_CAPACITY: usize = 16;

/// Minimum spacing between two bell toasts from one pane. An agent that rings
/// on every finished turn, or a build that beeps per error, must not bury the
/// desktop; the first ring is the one that tells the user to come back.
pub const BELL_NOTIFY_MIN_INTERVAL: Duration = Duration::from_secs(30);

struct Notification {
    urgency: &'static str,
    timeout_ms: &'static str,
    title: String,
    body: String,
}

static NOTIFICATION_WORKER: OnceLock<Option<SyncSender<Notification>>> = OnceLock::new();

/// Post a desktop notification for a command that just finished. `cmd` is
/// the displayed command (truncated to keep the toast readable);
/// `exit_code` drives the urgency hint (a real failure → critical, since failed
/// long builds are the case users most want to come back to).
///
/// `duration_ms` shows up in the body so the user knows whether they
/// have time to refill their coffee.
pub fn long_block_finished(cmd: &str, exit_code: i32, duration_ms: u64) {
    // Truncate the cmd so the notification title stays one line.
    let title_cmd = notification_title(cmd);

    let (status, urgency, timeout_ms) = long_block_outcome(exit_code);
    let title = format!("{status} {title_cmd}");
    let exit_text = match crate::exit_status::signal_name_for_exit(exit_code) {
        Some(sig) => format!("Exit {exit_code} ({sig})"),
        None => format!("Exit {exit_code}"),
    };
    let body = format!("{exit_text} after {}", humanize_duration(duration_ms));

    spawn_notify_send(urgency, timeout_ms, &title, &body);
}

/// Title glyph, urgency and timeout for a [`long_block_finished`] toast.
///
/// Success and the user-caused stops (Ctrl+C, a closed pipe, SIGTERM, Ctrl+Z;
/// see [`crate::exit_status::interrupt_signal`]) are normal-urgency and decay
/// after 5 s; a suspension or interruption is marked `⏸`, not `✗`. Real
/// failures are critical and linger for 10 s so the user comes back to them.
fn long_block_outcome(exit_code: i32) -> (&'static str, &'static str, &'static str) {
    if exit_code == 0 {
        ("✓", "normal", "5000")
    } else if crate::exit_status::interrupt_signal(exit_code).is_some()
        || crate::exit_status::is_job_stop(exit_code)
    {
        ("⏸", "normal", "5000")
    } else {
        ("✗", "critical", "10000")
    }
}

/// Whether a block that just finished after `duration_ms` deserves the
/// [`long_block_finished`] toast.
///
/// Long is not enough. The long blocks this family now runs most are
/// interactive: a three-hour claude or codex session, a vim or htop run. The
/// user ends those by hand, looking at them, and a "✓ claude — Exit 0 after
/// 3h 2m" toast is then pure noise. The toast is for a user who is somewhere
/// else, so it also needs the window to be inactive or the pane to be off
/// screen (`pane_mapped` is false on a background tab). GNOME Console and
/// Warp draw the same line. Whether the command took keystrokes is
/// deliberately not consulted: a long build the user nudged once still wants
/// its toast when they have switched away.
pub fn long_block_should_notify(
    duration_ms: u64,
    threshold_ms: u64,
    window_active: bool,
    pane_mapped: bool,
) -> bool {
    duration_ms >= threshold_ms && (!window_active || !pane_mapped)
}

/// Whether a BEL from a pane should become an [`attention`] toast.
///
/// BEL is how codex (by default) and claude (with its terminal-bell channel)
/// say "your turn". Inside a focused window the tab badge already shows it,
/// so the toast is only for an inactive window — the case where the agent's
/// tab is also the current one and nothing else would be visible. `last` is
/// when this pane last toasted; the caller owns it and sets it to `now`
/// whenever this returns true, so the limit is per pane.
pub fn bell_should_notify(window_active: bool, last: Option<Instant>, now: Instant) -> bool {
    !window_active
        && last.is_none_or(|last| now.saturating_duration_since(last) >= BELL_NOTIFY_MIN_INTERVAL)
}

/// Post a normal-urgency toast that asks the user to come back to a pane: a
/// bell from a program running in an inactive window. `title` names the pane
/// (its tab label) and is cut to one line like a command title; `body` says
/// what happened. Both are untrusted, since a tab label is often program
/// output, and pass the same sanitising as [`app_notification`]. Callers
/// rate-limit through [`bell_should_notify`].
pub fn attention(title: &str, body: &str) {
    let (title, body) = attention_fields(title, body);
    spawn_notify_send("normal", "5000", &title, &body);
}

fn attention_fields(title: &str, body: &str) -> (String, String) {
    let title = notification_title(title.trim());
    let title = if title.is_empty() {
        safe_notification_field(crate::identity::get().app_name)
    } else {
        title
    };
    (title, safe_notification_field(body))
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
    if let Err(error) = crate::helper::notify_send_with(
        &[
            app_name_arg.as_str(),
            icon_arg.as_str(),
            "--urgency",
            notification.urgency,
            "--expire-time",
            notification.timeout_ms,
        ],
        &notification.title,
        &notification.body,
    ) {
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
/// "exit 0 after 724000ms", and by the bottom bar so the same duration
/// reads identically in both places.
pub fn humanize_duration(ms: u64) -> String {
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
    fn long_block_toast_treats_interrupts_and_stops_as_neutral() {
        assert_eq!(long_block_outcome(0), ("✓", "normal", "5000"));
        for neutral in [130, 141, 143, 147, 148, 149, 150] {
            assert_eq!(
                long_block_outcome(neutral),
                ("⏸", "normal", "5000"),
                "code {neutral}"
            );
        }
        for failure in [1, 2, 127, 137, 139] {
            assert_eq!(
                long_block_outcome(failure),
                ("✗", "critical", "10000"),
                "code {failure}"
            );
        }
    }

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
    fn long_block_toast_is_for_a_user_who_is_elsewhere() {
        const HOURS_3: u64 = 3 * 3_600_000;
        const THRESHOLD: u64 = 10_000;
        // (duration, window active, pane mapped) → toast?
        let cases = [
            // Ending a long claude session in the focused window: no toast.
            (HOURS_3, true, true, false),
            // The window is in the background: toast.
            (HOURS_3, false, true, true),
            // The pane is a background tab of the focused window: toast.
            (HOURS_3, true, false, true),
            (HOURS_3, false, false, true),
            // Exactly the threshold counts as long.
            (THRESHOLD, false, true, true),
            // Short blocks never toast, wherever the user is.
            (THRESHOLD - 1, false, false, false),
            (5_000, false, true, false),
        ];
        for (duration, active, mapped, expected) in cases {
            assert_eq!(
                long_block_should_notify(duration, THRESHOLD, active, mapped),
                expected,
                "duration={duration} active={active} mapped={mapped}"
            );
        }
    }

    #[test]
    fn bell_toasts_only_from_an_inactive_window_and_at_most_every_30s() {
        let t0 = Instant::now();
        let at = |secs: u64| t0 + Duration::from_secs(secs);
        // A focused window shows the bell in its tab strip; no toast.
        assert!(!bell_should_notify(true, None, t0));
        assert!(!bell_should_notify(true, Some(t0), at(3600)));
        // Unfocused, first ring: toast.
        assert!(bell_should_notify(false, None, t0));
        // The same pane rings again 5 s later: suppressed.
        assert!(!bell_should_notify(false, Some(t0), at(5)));
        assert!(!bell_should_notify(false, Some(t0), at(29)));
        // After the interval it may toast again.
        assert!(bell_should_notify(false, Some(t0), at(30)));
        assert!(bell_should_notify(false, Some(t0), at(31)));
        // A `last` in the future (a caller mixing clocks) never toasts early.
        assert!(!bell_should_notify(false, Some(at(10)), t0));
    }

    #[test]
    fn attention_fields_are_bounded_and_attributed() {
        let (title, body) = attention_fields("codex\u{202e} ~/src", "Bell from\tcodex");
        assert_eq!(title, "codex\u{fffd} ~/src");
        assert_eq!(body, "Bell from\u{fffd}codex");
        // A long tab label is cut to one short line like a command title.
        let (title, _) = attention_fields(&format!("{}\nsecond line", "x".repeat(80)), "b");
        assert_eq!(title, format!("{}…", "x".repeat(60)));
        // An empty label still names the app.
        let (title, _) = attention_fields("  \n", "b");
        assert_eq!(title, crate::identity::get().app_name);
        let long = "y".repeat(crate::parser::MAX_NOTIFICATION_CHARS + 10);
        let (_, body) = attention_fields("t", &long);
        assert_eq!(body.chars().count(), crate::parser::MAX_NOTIFICATION_CHARS);
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
