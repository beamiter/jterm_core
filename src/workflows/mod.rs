//! Parameterised command templates — the family's Warp-style "workflows".
//!
//! A workflow is a TOML or YAML file: a name, a description, an optional shell
//! hint, an optional tag list, a command template with `{arg}` or `{{arg}}`
//! placeholders, and named arguments with optional defaults and descriptions.
//! Files are discovered under a five-tier search path, loaded with every read
//! bounded, validated, and rendered into text the app *inserts at the prompt
//! for review*. Nothing here executes a command.
//!
//! Every jterm terminal grew its own copy: anvil `src/workflows.rs` +
//! `src/workflow_ops.rs` + `src/dialogs/workflow.rs`, forge `src/workflows.rs`,
//! ember `src/workflows.rs` + `src/workflow_picker.rs`, frost the same pair.
//! The on-disk format is the whole point of the subsystem — the four apps read
//! the same library out of the same directories — so a difference in what one
//! app *accepts* is a difference in what a user's file *means* depending on
//! which terminal opened it. This module is their union.
//!
//! # What each lineage contributed
//!
//! anvil, ember and frost are one lineage and had barely drifted: serde-derive
//! deserialisation for both formats, `Result<(), String>` validation whose
//! message is the diagnostic, a template engine that trims placeholder names
//! and preserves an unterminated `{{` verbatim, and the missing-value guard in
//! [`render`]. ember and frost added `O_NOFOLLOW` to the bounded reader and the
//! `Option`-returning user-config lookup. anvil contributed the single-flight
//! [`RefreshLatch`] its background rescan runs behind.
//!
//! forge is the other lineage and contributed the one thing the first lineage
//! lacked: it is the only copy that ran an attacker-controllable *filename*
//! through [`crate::review_input::safe_inline_display`] before logging it. Its
//! parser contributed nothing — see below.
//!
//! # What the merge closed
//!
//! - **`O_NOFOLLOW`.** anvil opened workflow files with `O_NONBLOCK |
//!   O_CLOEXEC` only, so a symlink planted in `~/.config/anvil/workflows/`
//!   pointing at a world-writable file was followed, parsed, and its command
//!   became a palette entry. The other three refuse it at `open`. The reader
//!   here refuses it.
//! - **Type-wrong fields reject the file.** forge hand-rolled its TOML parser
//!   over `toml::Table` with `as_str().unwrap_or("")` coercions, so
//!   `default = 3000` (an unquoted port — the most natural authoring mistake
//!   there is) silently became the empty string and the file *loaded*. The
//!   user got a blank field and, on Insert, `lsof -ti tcp: | xargs -r kill`
//!   typed at their prompt. Both formats now go through serde derive, so a
//!   type-wrong field rejects the whole file with a message naming it.
//! - **Zero-argument workflows render.** forge inserted the raw template when
//!   `args` was empty, so its own documented `{{ }}` literal-brace escape was
//!   not applied there: `-d '{{"env":"prod"}}'` reached the prompt with the
//!   braces doubled. There is one insertion path here, [`render`], and it does
//!   not care how many arguments a workflow declares.
//! - **Placeholder names are trimmed.** `{{ service }}` — how third-party and
//!   Warp-style mustache libraries are written — bound in three apps and
//!   rendered as the literal `{ service }` in forge.
//! - **An unterminated `{{` survives.** Three copies advance by one byte so
//!   the second brace is re-scanned and `awk '{{print $1}' f` round-trips;
//!   forge advanced by two and turned it into a different, executable awk
//!   program.
//! - **Errors reach the caller.** forge built a good error string in its
//!   bounded reader and then dropped it with `let Ok(x) = .. else { continue }`,
//!   so an oversized, symlinked or non-UTF-8 file vanished from the palette
//!   with no log line at all — which is precisely why forge's other
//!   divergences went unnoticed for so long. [`load_one`] returns the reason
//!   and [`load_all`] logs it with the offending path.
//! - **No CWD-relative search tier.** forge derived its user directory from
//!   `HOME` with `unwrap_or_default()`, so with `HOME` unset it scanned
//!   `./.config/forge/workflows` — clone a repo containing that directory,
//!   start forge in it, and its files became the *highest-precedence*
//!   workflows. [`DirSources`] returns `Option`, and [`search_path`] drops any
//!   non-absolute directory a `DirSources` yields.
//! - **One blank-argument rule for both formats.** forge silently dropped a
//!   YAML argument with a blank name (leaving its `{}` placeholder to render
//!   verbatim into the command) while rejecting the identical file written in
//!   TOML. Validation is format-independent here: a blank argument name
//!   rejects the workflow either way.
//!
//! # The defect that was in all four: an unfilled argument
//!
//! `render()`'s missing-value guard was implemented and unit-tested in anvil,
//! ember and frost — and *dead in the whole family*, because every UI
//! pre-seeded each declared argument with `""` (anvil
//! `dialogs/workflow.rs:210`, ember `workflow_picker.rs:147`, frost
//! `workflow_picker.rs:143`, forge `dialogs.rs:3993`). `kill -9 {pid}` with an
//! untouched Pid field rendered `kill -9 ` and was typed at the prompt.
//!
//! The contract here, stated once and enforced below the UI:
//!
//! > **An empty value is meaningful only if the file says so.** An argument
//! > that declares a default — `default = ""` included — may render empty. An
//! > argument that declares *no* default is not filled by a blank string:
//! > absent and blank are the same state, and [`render`] reports it as a
//! > missing value.
//!
//! Both halves of the fix are load-bearing, and they agree:
//!
//! 1. [`render`] applies the rule to the values map itself, so a caller that
//!    builds its own `HashMap` — every UI in the family does — cannot pre-seed
//!    its way past the guard. This is the half the UI cannot bypass.
//! 2. [`ArgsForm`] carries the distinction in the type system: a slot is
//!    [`ArgsForm::is_set`] or not, seeded from the declared default, so a UI
//!    can tell the user *which* fields are still outstanding
//!    ([`ArgsForm::missing`]) before they press Insert, and
//!    [`ArgsForm::clear`] restores the declared default rather than meaning
//!    "supplied empty".
//!
//! # Three more the copies agreed on
//!
//! Not divergences — every copy behaved the same way, which is why comparing
//! them could not surface these either:
//!
//! - **An unterminated `{{` was preserved only while nothing later in the
//!   template closed a pair.** The close was found by scanning to the end, so
//!   `awk '{{print $1}' f` round-tripped while `awk '{{print $1}' {{log}}`
//!   let the first `{{` claim the second placeholder's `}}`, take the escape
//!   branch, and hand the user `awk '{print $1}' access.log` — the identical
//!   leading bytes meaning two different things, and the second one an
//!   executable awk program its author never wrote. `{{` and `}}` nest here
//!   now, so a pair's close is its own.
//! - **Placeholder names were trimmed; declared argument names were not.**
//!   `{{ service }}` binds like `{{service}}` by design, but nothing held the
//!   other side of that comparison to the same spelling. A quoted
//!   `name = "pid "` loaded clean, validated clean, and matched nothing:
//!   `kill -9 {{ pid }}` rendered the literal `kill -9 { pid }`, the
//!   missing-value guard returned `Ok` because the argument *had* a value,
//!   [`ArgsForm::missing`] reported the form complete, and the number the user
//!   typed was discarded on the way to the prompt. A name that has to bind
//!   must equal its own trim, on both sides.
//! - **Half of every log line was sanitised.** The path went through
//!   [`crate::review_input::safe_inline_display`]; the reason was
//!   interpolated raw, and a TOML parse error quotes the offending source line
//!   back verbatim — so a file whose unterminated string is
//!   `command = "echo <ESC>]0;title<BEL>` wrote that OSC sequence onto a warn
//!   line for whatever tty was tailing the log. Both halves cross the same
//!   call now ([`MAX_LOGGED_REASON_BYTES`]); [`load_one`]'s `Result` stays raw
//!   for a UI with its own escaping.
//!
//! # Bounds
//!
//! Eleven budgets, byte-identical in all four copies and applied to the same
//! fields in the same order — the one part of this surface that had not
//! drifted. They are frozen here because the 16x asymmetry between the
//! argument-default budget ([`MAX_WORKFLOW_COMMAND_BYTES`]) and the
//! argument-name budget ([`MAX_WORKFLOW_FIELD_BYTES`]) survived four copies by
//! luck and would not have survived a fifth.
//!
//! # Policy, not probes
//!
//! Discovery is the one impure seam, and it asks the caller rather than the
//! environment — including about the app's own name.
//! [`SearchPathSpec::for_current_app`] returns `Option` because
//! [`crate::identity::get`] never fails: before `identity::init` it answers
//! with the neutral `"jterm"`, which would silently point discovery at
//! `~/.config/jterm/workflows` and `$JTERM_WORKFLOW_DIR` — nobody's library,
//! nobody's variable, and no error anywhere, because a directory that does not
//! exist is not a failure. That is the `env!("CARGO_MANIFEST_DIR")` trap again
//! by way of a process global, and it is worst in test binaries, which never
//! call `init`. The XDG backend is injected ([`DirSources`]) because anvil and
//! forge ask glib and ember and frost ask the `dirs` crate, and those answers
//! differ at exactly the edges that matter — `glib::user_config_dir()` never
//! fails, `dirs::config_dir()` returns `None` with `HOME` unset. The dev-tree
//! fallback is injected because `env!("CARGO_MANIFEST_DIR")` is resolved at
//! compile time against the crate being compiled: evaluating it *here* would
//! silently point all four apps at `jterm_core/scripts/workflows`, which does
//! not exist, while their bundled-library tests kept passing. [`LoadOrder`]
//! has no `Default` because anvil and frost list in precedence order while
//! ember and forge sort by name, and a silent default is how two apps inherit
//! a behaviour nobody chose for them.
//!
//! `welcome_notebook_path` deliberately did not migrate. anvil and forge both
//! have one; ember and frost each documented not porting it. It is an asset
//! lookup that lives in `workflows.rs` only because it reuses the
//! directory-search shape, and it stays in the two apps that have a notebook.

mod discovery;
mod loader;
mod picker;
mod render;

pub use discovery::{search_path, DirSources, SearchPathSpec, XdgEnvDirs};
pub use loader::{
    is_workflow_file, load_all, load_one, validate, workflow_files_in, LoadOrder, RefreshLatch,
};
pub use picker::{PickerPolicy, WorkflowPicker, MAX_PICKER_QUERY_BYTES};
pub use render::{render, substitute, ArgsForm};

use serde::Deserialize;
use std::path::PathBuf;

/// One workflow file, whole.
pub const MAX_WORKFLOW_FILE_BYTES: u64 = 256 * 1024;
/// Directory entries inspected before the extension filter runs.
pub const MAX_DIRECTORY_ENTRIES: usize = 4_096;
/// Workflow-looking files loaded from one directory.
pub const MAX_WORKFLOW_FILES_PER_DIRECTORY: usize = 512;
/// Workflows in one loaded library, across every directory.
pub const MAX_WORKFLOWS: usize = 1_024;
/// Directories in one search path.
pub const MAX_WORKFLOW_DIRECTORIES: usize = 64;
/// A workflow's own name.
pub const MAX_WORKFLOW_NAME_BYTES: usize = 256;
/// A workflow's description, and each argument's.
pub const MAX_WORKFLOW_DESCRIPTION_BYTES: usize = 4 * 1024;
/// The command template, each argument default, each caller value, and the
/// cumulative rendered output.
pub const MAX_WORKFLOW_COMMAND_BYTES: usize = 64 * 1024;
/// Tags on one workflow.
pub const MAX_WORKFLOW_TAGS: usize = 64;
/// Declared arguments on one workflow, and caller values passed to [`render`].
pub const MAX_WORKFLOW_ARGS: usize = 64;
/// Each tag, the shell hint, each argument name, and each caller value's key.
pub const MAX_WORKFLOW_FIELD_BYTES: usize = 4 * 1024;
/// A file path is untrusted text: an attacker who can create a file in a
/// scanned directory chooses its name. Only forge bounded and sanitised the
/// path it logged; the other three wrote `path.display()` straight into the
/// log, bidi overrides included. Every path in a log line here goes through
/// [`crate::review_input::safe_inline_display`] with this budget.
pub const MAX_LOGGED_PATH_BYTES: usize = 2 * 1024;
/// The *reason* half of the same log line, which is untrusted for a second
/// reason: `toml::from_str` quotes the offending source line back verbatim, so
/// a loader error carries bytes the file's author chose — ESC, BEL, bidi
/// overrides and newlines among them. A sanitised path in front of a raw
/// reason is not a sanitised log line, and forge is the app this matters most
/// for: it discards its reader and parser errors today, so migration is the
/// moment it starts writing file content to a log at all. [`load_one`] keeps
/// its message raw for a UI that escapes it its own way; [`load_all`] is what
/// writes it to a log, and it sanitises with this budget — larger than a
/// path's because a parse error carries the source line it failed on.
pub const MAX_LOGGED_REASON_BYTES: usize = 4 * 1024;

/// One parameterised command template.
///
/// Deserialised from TOML or YAML by the same derive, so the two formats
/// cannot disagree about what a field means. There is deliberately no
/// `deny_unknown_fields`: ignoring unknown keys is what lets one on-disk
/// library serve four apps (and the next version of any of them).
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Workflow {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub command: String,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Optional interpreter hint retained for shared workflow libraries.
    /// Workflows remain review-only and are never auto-executed.
    #[serde(default)]
    pub shell: Option<String>,
    #[serde(default)]
    pub args: Vec<WorkflowArg>,
    /// Source file this workflow was loaded from — the palette hands a path
    /// back rather than an index, because the library can be rebuilt between
    /// listing and activation. `None` for a workflow built in memory.
    ///
    /// Stamped by [`load_one`] only after [`validate`] has passed, so a
    /// workflow that carries a path is one that was accepted.
    #[serde(skip)]
    pub source_path: Option<PathBuf>,
}

/// One declared argument of a [`Workflow`].
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct WorkflowArg {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// `None` means the file declared no default, which is *not* the same as
    /// `Some(String::new())`. forge's copy typed this `String` and lost the
    /// distinction, which is why forge alone could not have implemented the
    /// missing-value guard at all. See the module docs: the declaration is
    /// what licenses an empty rendered value.
    #[serde(default)]
    pub default: Option<String>,
}

#[cfg(test)]
pub(crate) mod testing {
    //! Helpers shared by this module's three test suites.

    use super::{Workflow, WorkflowArg};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

    /// A scratch directory removed when the test ends, however it ends.
    pub(crate) struct TestDir(PathBuf);

    impl TestDir {
        pub(crate) fn new(label: &str) -> Self {
            let id = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "jterm-core-workflows-{label}-{}-{id}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        pub(crate) fn path(&self) -> &Path {
            &self.0
        }

        pub(crate) fn write(&self, name: &str, contents: &str) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, contents).unwrap();
            path
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A workflow built in memory: `args` is `(name, default)` pairs where
    /// `None` means the file declared no default.
    pub(crate) fn workflow(name: &str, command: &str, args: &[(&str, Option<&str>)]) -> Workflow {
        Workflow {
            name: name.to_string(),
            description: String::new(),
            command: command.to_string(),
            tags: Vec::new(),
            shell: None,
            args: args
                .iter()
                .map(|(name, default)| WorkflowArg {
                    name: (*name).to_string(),
                    description: String::new(),
                    default: default.map(str::to_string),
                })
                .collect(),
            source_path: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::TestDir;
    use super::*;

    /// The whole seam an app shim wires together, end to end: discovery with
    /// an injected backend, a pinned load order, the picker, and the argument
    /// form. It exists to catch an export or signature mistake that no
    /// single-module test would — the four apps are the only consumers, and
    /// they are not built by this crate's gate.
    #[test]
    fn the_public_seam_composes_the_way_a_shim_will_use_it() {
        // The library where the search path expects it: <config>/<app>/workflows.
        let config = TestDir::new("seam");
        let library = config.path().join("app").join("workflows");
        std::fs::create_dir_all(&library).unwrap();
        std::fs::write(
            library.join("deploy.yaml"),
            "name: Deploy\ndescription: Ship a service\n\
             command: \"deploy {{ service }} --env={{env}}\"\ntags: [ops]\n\
             args:\n  - name: service\n    default: api\n  - name: env\n",
        )
        .unwrap();
        std::fs::write(
            library.join("status.toml"),
            "name = 'Status'\ncommand = 'git status --porcelain'\n",
        )
        .unwrap();

        struct Dirs(std::path::PathBuf);
        impl DirSources for Dirs {
            fn user_config_dir(&self) -> Option<std::path::PathBuf> {
                Some(self.0.clone())
            }
            fn user_data_dir(&self) -> Option<std::path::PathBuf> {
                None
            }
            fn system_data_dirs(&self) -> Vec<std::path::PathBuf> {
                Vec::new()
            }
        }

        let spec = SearchPathSpec::new("app", "JTERM_CORE_SEAM_WORKFLOW_DIR_UNSET", None);
        let dirs = search_path(&spec, &Dirs(config.path().to_path_buf()));
        assert_eq!(dirs, std::slice::from_ref(&library));

        let loaded = load_all(&dirs, LoadOrder::ByName);
        assert_eq!(
            loaded.iter().map(|w| w.name.as_str()).collect::<Vec<_>>(),
            ["Deploy", "Status"]
        );

        let mut picker = WorkflowPicker::new(loaded, PickerPolicy::new(15, false));
        picker.push_query_text("ship");
        let picked = picker.selected_workflow().cloned().unwrap();
        assert_eq!(picked.name, "Deploy");

        let mut form = ArgsForm::new(picked);
        assert_eq!(form.missing(), ["env"]);
        assert!(form.render().unwrap_err().contains("missing values: env"));
        form.set(1, "staging");
        assert_eq!(form.render().unwrap(), "deploy api --env=staging");

        // A zero-argument workflow renders through exactly the same path.
        let status = ArgsForm::new(load_one(&library.join("status.toml")).unwrap());
        assert_eq!(status.render().unwrap(), "git status --porcelain");
    }
}
