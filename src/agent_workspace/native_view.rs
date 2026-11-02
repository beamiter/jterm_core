//! Bound transport copies while the driver retains its canonical live view.
use crate::agent_task::CodexAppServerViewSnapshot;
use std::sync::Arc;
const BUDGET: usize = 512 * 1024;
fn clip(text: &mut String, limit: usize) -> bool {
    if text.len() <= limit {
        return false;
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    true
}
pub(super) fn bounded(mut view: CodexAppServerViewSnapshot) -> CodexAppServerViewSnapshot {
    let fits = |view: &CodexAppServerViewSnapshot| {
        serde_json::to_vec(view).is_ok_and(|bytes| bytes.len() <= BUDGET)
    };
    if fits(&view) {
        return view;
    }
    view.dropped_turns += view.turn_history.len();
    view.turn_history = Arc::from([]);
    view.persisted_history = Arc::from([]);
    view.history_cursor = None;
    view.history_loading = false;
    for command in &mut view.commands {
        command.output_truncated |= clip(&mut command.output, 2048);
    }
    for item in &mut view.file_changes {
        for change in &mut item.changes {
            if clip(&mut change.diff, 2048) {
                change.diff_truncated = true;
                change.diff_exact = false;
            }
        }
    }
    view.agent_text_truncated |= clip(&mut view.agent_text, 64 * 1024);
    if !fits(&view) {
        // An incomplete approval presentation never becomes a clickable grant.
        // The canonical driver proposal remains unchanged and may be denied,
        // or inspected in the provider CLI after an explicit handoff.
        view.pending_approvals.clear();
        view.last_error = Some(
            "Approval evidence exceeds the remote view budget; no approval action is exposed."
                .into(),
        );
    }
    if !fits(&view) {
        view.commands.truncate(16);
        view.file_changes.truncate(16);
        view.dropped_updates = view.dropped_updates.saturating_add(1);
        view.last_error=Some("Some native details exceed the remote view budget; provider history retains the full record.".into());
    }
    if !fits(&view) {
        let thread = view.provider_thread_id.clone();
        let phase = view.phase;
        view = CodexAppServerViewSnapshot::default();
        view.provider_thread_id = thread;
        view.phase = phase;
        view.last_error = Some("Native details exceed the view budget. Open provider history after stopping this execution.".into());
    }
    view
}
