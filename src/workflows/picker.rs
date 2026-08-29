//! The searchable overlay ember, frost and forge put in front of a loaded
//! library.
//!
//! anvil folds workflows into its tiered command palette instead, as does
//! forge's unified command palette; neither of those surfaces uses this
//! workflow-only state machine. What is shared here is a query, a highlight,
//! a filtered view, and the invariant that binds them.
//!
//! # The invariant
//!
//! **The highlight can never point past a drawn row.** Navigation and drawing
//! both go through [`WorkflowPicker::filtered`], so the same cap applies to
//! both; the highlight resets to the first row on every query change, because
//! a query that shrinks the result list would otherwise leave the highlight
//! selecting a workflow the user can no longer see. frost had that reset
//! written out at three separate call sites in `main.rs`; forge's standalone
//! palette instead built as many as 1,024 GTK rows on the main thread. Both
//! now share the same bounded state and drawn-row invariant.

use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;

use super::Workflow;
use super::MAX_WORKFLOW_FIELD_BYTES;

/// A query is one line a person types, not a document. Bounding it bounds the
/// per-entry fuzzy match, which is otherwise linear in query length.
pub const MAX_PICKER_QUERY_BYTES: usize = MAX_WORKFLOW_FIELD_BYTES;

/// What this app's picker searches and how much of it it shows.
///
/// Deliberately has no `Default`. The result cap is the one limit on this
/// surface that legitimately differs — 15 for a workflow-only overlay whose
/// keyboard navigation must match the drawn list, and a caller-chosen larger
/// value for a palette that interleaves workflows with actions and history.
/// Whether the command template itself is searchable differs too: forge alone
/// searches it, so `lsof` finds its kill-port workflow and nothing else in the
/// family finds it that way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PickerPolicy {
    max_results: usize,
    search_command: bool,
}

impl PickerPolicy {
    /// `max_results` is both the drawing cap and the navigation cap — they
    /// must be the same number or the highlight can leave the drawn list.
    ///
    /// `const` so an app can pin its policy in a module-level constant. That
    /// is the shape this type exists to encourage: one named value per app
    /// rather than a literal repeated at each construction site, which is how
    /// two call sites in the same binary end up disagreeing.
    pub const fn new(max_results: usize, search_command: bool) -> Self {
        Self {
            max_results,
            search_command,
        }
    }

    pub const fn max_results(&self) -> usize {
        self.max_results
    }

    /// Whether the command template joins name, description and tags in the
    /// search haystack. Off in ember and frost: a workflow is recalled by what
    /// it is for, and the template is full of flags that match everything.
    pub const fn search_command(&self) -> bool {
        self.search_command
    }
}

/// Query, highlight and filtered view over one loaded library.
pub struct WorkflowPicker {
    entries: Vec<Workflow>,
    policy: PickerPolicy,
    matcher: SkimMatcherV2,
    query: String,
    /// Indices into `entries`, in exactly the order callers draw them.
    ///
    /// Fuzzy matching is proportional to the whole loaded library (and, when
    /// command search is enabled, to every command template). Keeping that
    /// work at the query-change boundary prevents a single frame from doing
    /// it again for drawing, keyboard navigation, mouse resolution and
    /// activation.
    filtered_indices: Vec<usize>,
    selected: usize,
}

impl WorkflowPicker {
    /// Take the library as loaded.
    ///
    /// Deliberately does **not** sort: ember's picker re-sorted by name at
    /// construction, which silently overrode whatever
    /// [`LoadOrder`](super::LoadOrder) the caller chose. Order is the loader's
    /// decision, stated once.
    pub fn new(entries: Vec<Workflow>, policy: PickerPolicy) -> Self {
        let mut picker = Self {
            entries,
            policy,
            matcher: SkimMatcherV2::default(),
            query: String::new(),
            filtered_indices: Vec::new(),
            selected: 0,
        };
        picker.rebuild_filtered();
        picker
    }

    pub fn policy(&self) -> PickerPolicy {
        self.policy
    }

    pub fn entries(&self) -> &[Workflow] {
        &self.entries
    }

    /// Whether the library itself is empty — not whether the query matches
    /// anything.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    /// Replace the query; the highlight returns to the first row.
    pub fn set_query(&mut self, query: impl Into<String>) {
        // This is the boundary used by text widgets and programmatic callers,
        // not only by the raw-key helper below. Keeping the filter here means
        // a pasted newline cannot bypass the one-line query contract.
        let mut query: String = query
            .into()
            .chars()
            .filter(|character| !character.is_control())
            .collect();
        if query.len() > MAX_PICKER_QUERY_BYTES {
            // Truncate on a char boundary so the query stays valid UTF-8.
            let mut end = MAX_PICKER_QUERY_BYTES;
            while end > 0 && !query.is_char_boundary(end) {
                end -= 1;
            }
            query.truncate(end);
        }
        // Widget toolkits can report the same value more than once (forge's
        // SearchEntry does this after the normalization write-back). Preserve
        // `set_query`'s existing highlight-reset contract, but do not rescan
        // the whole library when the effective query did not change.
        if query == self.query {
            self.selected = 0;
            return;
        }
        self.query = query;
        self.selected = 0;
        self.rebuild_filtered();
    }

    /// Append typed text, dropping control characters. Returns whether the
    /// query changed, so a key handler can tell "typed into the picker" from
    /// "swallow this key".
    ///
    /// A whole `&str` rather than a `char` because iced delivers key text as a
    /// string; frost had this filter inline in a 16,000-line `main.rs`.
    pub fn push_query_text(&mut self, text: &str) -> bool {
        let previous = self.query.clone();
        let mut query = previous.clone();
        query.push_str(text);
        self.set_query(query);
        self.query != previous
    }

    /// Delete the last character of the query. Returns whether anything was
    /// deleted.
    pub fn backspace(&mut self) -> bool {
        if self.query.is_empty() {
            return false;
        }
        // Mutate directly rather than `take` + `set_query`: the latter makes
        // the temporarily empty stored query compare equal when deleting the
        // final character, incorrectly skipping the cache rebuild and
        // highlight reset.
        self.query.pop();
        self.selected = 0;
        self.rebuild_filtered();
        true
    }

    /// The rows to draw, capped by the policy. An empty query keeps the
    /// library's own order; otherwise entries rank by fuzzy score, with equal
    /// scores keeping library order (the sort is stable).
    pub fn filtered(&self) -> Vec<&Workflow> {
        self.filtered_indices
            .iter()
            .map(|index| &self.entries[*index])
            .collect()
    }

    /// Recompute the drawn-order index once, when the query changes.
    fn rebuild_filtered(&mut self) {
        if self.query.is_empty() {
            self.filtered_indices = (0..self.entries.len())
                .take(self.policy.max_results)
                .collect();
            return;
        }
        let mut scored: Vec<(i64, usize)> = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(index, workflow)| {
                self.matcher
                    .fuzzy_match(&self.haystack(workflow), &self.query)
                    .map(|score| (score, index))
            })
            .collect();
        // Stable sorting retains library order for equal fuzzy scores.
        scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        self.filtered_indices = scored
            .into_iter()
            .take(self.policy.max_results)
            .map(|(_, index)| index)
            .collect();
    }

    /// Name, description and tags — plus the command template when the policy
    /// says so.
    fn haystack(&self, workflow: &Workflow) -> String {
        let mut haystack = String::with_capacity(workflow.name.len() + workflow.description.len());
        haystack.push_str(&workflow.name);
        if !workflow.description.is_empty() {
            haystack.push(' ');
            haystack.push_str(&workflow.description);
        }
        for tag in &workflow.tags {
            haystack.push(' ');
            haystack.push_str(tag);
        }
        if self.policy.search_command {
            haystack.push(' ');
            haystack.push_str(&workflow.command);
        }
        haystack
    }

    /// Move the highlight down, wrapping within the drawn rows.
    pub fn select_next(&mut self) {
        let len = self.filtered_indices.len();
        self.selected = if len == 0 {
            0
        } else {
            (self.selected + 1) % len
        };
    }

    /// Move the highlight up, wrapping within the drawn rows.
    pub fn select_prev(&mut self) {
        let len = self.filtered_indices.len();
        self.selected = match len {
            0 => 0,
            _ if self.selected == 0 => len - 1,
            _ => self.selected - 1,
        };
    }

    /// Point the highlight at a drawn row — a mouse click. Out-of-range
    /// indices are ignored rather than clamped, so a click on a row that has
    /// just been filtered away does not silently select a different workflow.
    pub fn select(&mut self, index: usize) -> bool {
        if index < self.filtered_indices.len() {
            self.selected = index;
            return true;
        }
        false
    }

    /// The highlighted workflow, if the filtered list is non-empty.
    pub fn selected_workflow(&self) -> Option<&Workflow> {
        self.filtered_indices
            .get(self.selected)
            .map(|index| &self.entries[*index])
    }

    /// The workflow at a position in the drawn list — mouse-click dispatch.
    pub fn workflow_at_filtered(&self, index: usize) -> Option<&Workflow> {
        self.filtered_indices
            .get(index)
            .map(|index| &self.entries[*index])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflows::testing::workflow;

    const OVERLAY: PickerPolicy = PickerPolicy {
        max_results: 15,
        search_command: false,
    };

    fn described(name: &str, description: &str, tags: &[&str]) -> Workflow {
        let mut workflow = workflow(name, "echo ok", &[]);
        workflow.description = description.to_string();
        workflow.tags = tags.iter().map(|tag| (*tag).to_string()).collect();
        workflow
    }

    fn names(picker: &WorkflowPicker) -> Vec<&str> {
        picker
            .filtered()
            .iter()
            .map(|workflow| workflow.name.as_str())
            .collect()
    }

    #[test]
    fn an_empty_query_keeps_the_library_order_it_was_given() {
        // ember's picker re-sorted by name at construction, which silently
        // overrode the LoadOrder its loader had already applied. Order is
        // decided once, by the caller.
        let picker = WorkflowPicker::new(
            vec![
                described("zeta", "", &[]),
                described("alpha", "", &[]),
                described("mid", "", &[]),
            ],
            OVERLAY,
        );
        assert_eq!(names(&picker), ["zeta", "alpha", "mid"]);
        assert_eq!(
            picker.selected_workflow().map(|w| w.name.as_str()),
            Some("zeta")
        );
    }

    #[test]
    fn a_query_matches_name_description_and_tags() {
        let mut picker = WorkflowPicker::new(
            vec![
                described("deploy", "Ship the service", &["ops"]),
                described("rebase", "Rewrite history", &["git"]),
            ],
            OVERLAY,
        );
        picker.set_query("ship");
        assert_eq!(names(&picker), ["deploy"]);
        picker.set_query("git");
        assert_eq!(names(&picker), ["rebase"]);
    }

    #[test]
    fn whether_the_command_is_searchable_is_the_policy_s_call() {
        // forge alone searched the command template, so `lsof` found its
        // kill-port workflow and found nothing at all in the other three. The
        // difference stays expressible, and stops being accidental.
        let entries = || {
            let mut workflow = workflow("Kill port", "lsof -ti tcp:{port} | xargs -r kill", &[]);
            workflow.description = "Free a listening port".to_string();
            vec![workflow]
        };

        let mut metadata_only = WorkflowPicker::new(entries(), OVERLAY);
        metadata_only.set_query("lsof");
        assert!(metadata_only.filtered().is_empty());

        let mut with_command = WorkflowPicker::new(entries(), PickerPolicy::new(15, true));
        with_command.set_query("lsof");
        assert_eq!(names(&with_command), ["Kill port"]);
    }

    #[test]
    fn results_are_capped_so_navigation_matches_the_drawn_list() {
        let entries = (0..OVERLAY.max_results() + 5)
            .map(|index| described(&format!("wf-{index:02}"), "", &[]))
            .collect();
        let mut picker = WorkflowPicker::new(entries, OVERLAY);
        assert_eq!(picker.filtered().len(), OVERLAY.max_results());

        picker.select_prev();
        assert_eq!(picker.selected(), OVERLAY.max_results() - 1);
        picker.select_next();
        assert_eq!(picker.selected(), 0);
        assert_eq!(
            picker.selected_workflow().map(|w| w.name.as_str()),
            Some("wf-00")
        );
    }

    #[test]
    fn the_highlight_returns_to_the_first_row_on_every_query_change() {
        // frost kept this reset written out at three separate call sites; a
        // query that shrinks the list would otherwise leave the highlight
        // pointing at a workflow the user can no longer see.
        let mut picker = WorkflowPicker::new(
            vec![described("alpha", "", &[]), described("beta", "", &[])],
            OVERLAY,
        );
        picker.select_next();
        assert_eq!(picker.selected(), 1);

        assert!(picker.push_query_text("a"));
        assert_eq!(picker.selected(), 0);
        assert_eq!(picker.query(), "a");

        picker.select_next();
        assert!(picker.backspace());
        assert_eq!(picker.selected(), 0);
        assert!(picker.query().is_empty());
        assert!(!picker.backspace(), "an empty query has nothing to delete");
    }

    #[test]
    fn an_unchanged_normalized_query_resets_without_rebuilding() {
        // forge writes a normalized query back into SearchEntry, which emits
        // its change signal a second time. Keep the public setter's established
        // highlight reset, but reuse the already computed result index.
        let mut picker = WorkflowPicker::new(
            vec![described("alpha", "", &[]), described("beta", "", &[])],
            OVERLAY,
        );
        picker.select_next();
        assert_eq!(picker.selected(), 1);
        let cached = picker.filtered_indices.clone();

        picker.set_query("\n\t");
        assert!(picker.query().is_empty());
        assert_eq!(picker.selected(), 0);
        assert_eq!(picker.filtered_indices, cached);
        assert_eq!(
            picker
                .selected_workflow()
                .map(|workflow| workflow.name.as_str()),
            Some("alpha")
        );
    }

    #[test]
    fn all_read_paths_share_the_cached_filtered_order() {
        let entries = (0..1_024)
            .map(|index| described(&format!("workflow-{index:04}"), "deploy service", &[]))
            .collect();
        let mut picker = WorkflowPicker::new(entries, OVERLAY);
        picker.set_query("deploy");

        let cached = picker.filtered_indices.clone();
        assert_eq!(cached.len(), OVERLAY.max_results());
        assert_eq!(picker.filtered().len(), cached.len());
        assert_eq!(
            picker
                .selected_workflow()
                .map(|workflow| workflow.name.as_str()),
            Some("workflow-0000")
        );
        assert_eq!(
            picker
                .workflow_at_filtered(14)
                .map(|workflow| workflow.name.as_str()),
            Some("workflow-0014")
        );
        picker.select_next();
        picker.select_prev();
        assert_eq!(picker.filtered_indices, cached);
    }

    #[test]
    fn typed_text_is_filtered_to_printable_characters() {
        let mut picker = WorkflowPicker::new(vec![described("alpha", "", &[])], OVERLAY);
        assert!(!picker.push_query_text("\r\n\u{1b}"));
        assert!(picker.query().is_empty());
        assert!(picker.push_query_text("al\tpha"));
        assert_eq!(picker.query(), "alpha");

        picker.set_query("be\nta\u{1b}");
        assert_eq!(
            picker.query(),
            "beta",
            "widget/paste input must cross the same one-line boundary"
        );
    }

    #[test]
    fn a_query_is_bounded_and_stays_valid_utf8() {
        let mut picker = WorkflowPicker::new(vec![described("alpha", "", &[])], OVERLAY);
        picker.set_query("界".repeat(MAX_PICKER_QUERY_BYTES));
        assert!(picker.query().len() <= MAX_PICKER_QUERY_BYTES);
        assert!(picker.query().chars().all(|ch| ch == '界'));

        picker.set_query("x".repeat(MAX_PICKER_QUERY_BYTES));
        let full = picker.query().to_string();
        assert!(
            !picker.push_query_text("more"),
            "a push truncated back to the same bounded query is not a change"
        );
        assert_eq!(picker.query(), full);
    }

    #[test]
    fn selection_wraps_and_click_indices_resolve() {
        let mut picker = WorkflowPicker::new(
            vec![described("one", "", &[]), described("two", "", &[])],
            OVERLAY,
        );
        picker.select_prev();
        assert_eq!(picker.selected(), 1);
        assert_eq!(
            picker.workflow_at_filtered(1).map(|w| w.name.as_str()),
            Some("two")
        );
        picker.select_next();
        assert_eq!(picker.selected(), 0);
        assert!(picker.workflow_at_filtered(2).is_none());

        assert!(picker.select(1));
        assert_eq!(picker.selected(), 1);
        // A click on a row that has just been filtered away must not select a
        // different workflow instead.
        assert!(!picker.select(9));
        assert_eq!(picker.selected(), 1);
    }

    #[test]
    fn an_empty_library_navigates_without_panicking() {
        let mut picker = WorkflowPicker::new(Vec::new(), OVERLAY);
        assert!(picker.is_empty());
        picker.select_next();
        picker.select_prev();
        assert_eq!(picker.selected(), 0);
        assert!(picker.selected_workflow().is_none());
    }
}
