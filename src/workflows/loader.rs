//! Reading a workflow library off disk: bounded, fail-closed, and loud.
//!
//! Every guard here exists because the file being read is untrusted input that
//! becomes a command at the user's prompt. The reader refuses symlinks, FIFOs
//! and oversized files; the parsers refuse a type-wrong field rather than
//! coercing it away; and every refusal produces a log line naming the file,
//! because a workflow that silently disappears from the palette is
//! indistinguishable from one that was never installed.

use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};

use super::{
    Workflow, MAX_DIRECTORY_ENTRIES, MAX_LOGGED_PATH_BYTES, MAX_LOGGED_REASON_BYTES, MAX_WORKFLOWS,
    MAX_WORKFLOW_ARGS, MAX_WORKFLOW_COMMAND_BYTES, MAX_WORKFLOW_DESCRIPTION_BYTES,
    MAX_WORKFLOW_DIRECTORIES, MAX_WORKFLOW_FIELD_BYTES, MAX_WORKFLOW_FILES_PER_DIRECTORY,
    MAX_WORKFLOW_FILE_BYTES, MAX_WORKFLOW_NAME_BYTES, MAX_WORKFLOW_TAGS,
};

/// The order [`load_all`] returns a library in.
///
/// Deliberately has no `Default`. anvil and frost list in precedence order so
/// the user's own files head the list; ember and forge sort by name so the
/// palette reads alphabetically. Both are legitimate, and both were expressed
/// as the presence or absence of one `sort_by` line — exactly the shape of
/// silent default that has bitten this family before. Every construction site
/// says which it wants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoadOrder {
    /// Directory precedence, then filename within a directory: the user's own
    /// workflows first, the bundled examples last.
    Precedence,
    /// Alphabetical by workflow name across the whole library.
    ByName,
}

/// Admission gate for a background library rescan.
///
/// Lifted from anvil's `workflow_ops.rs`, the only copy that refreshes off the
/// UI thread. It is toolkit-free on purpose: the threading, the panic
/// containment and the keep-the-old-cache-on-error policy stay in the app,
/// because egui and iced are single-threaded immediate-mode and GTK is not.
/// What is shared is the invariant — **at most one scan in flight, and a
/// completed scan re-arms** — so repeated shortcut presses reuse the current
/// cache instead of spawning a thread per press.
///
/// Not `Copy`, deliberately: a copied latch would let `begin()` succeed twice
/// while the caller believed it was holding one reservation.
#[derive(Debug, Default)]
pub struct RefreshLatch {
    in_flight: bool,
}

impl RefreshLatch {
    /// Reserve the single in-flight slot. `false` means a scan is already
    /// running and the caller should keep using the cache it has.
    pub fn begin(&mut self) -> bool {
        if self.in_flight {
            return false;
        }
        self.in_flight = true;
        true
    }

    /// Release the reservation. Must run on the completion path *and* on the
    /// failed-to-start path, or the latch never re-arms.
    pub fn finish(&mut self) {
        self.in_flight = false;
    }

    pub fn is_in_flight(&self) -> bool {
        self.in_flight
    }
}

/// Whether a path names a workflow file by extension: `.toml`, `.yaml` or
/// `.yml`, case-insensitively.
///
/// Exported because this predicate had already been re-derived twice outside
/// the loader — anvil's `diagnostics.rs` and forge's own test helper — and a
/// second implementation of an on-disk contract inside one app is how the next
/// divergence starts.
pub fn is_workflow_file(path: &Path) -> bool {
    workflow_extension(path).is_some()
}

fn workflow_extension(path: &Path) -> Option<String> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    matches!(extension.as_str(), "toml" | "yaml" | "yml").then_some(extension)
}

/// The workflow files in one directory, bounded and sorted.
///
/// [`MAX_DIRECTORY_ENTRIES`] applies *before* the extension filter and
/// [`MAX_WORKFLOW_FILES_PER_DIRECTORY`] after it, so a directory stuffed with
/// non-workflow files cannot push the real ones out of the budget while a
/// directory stuffed with `.toml` files still cannot make the loader parse
/// more than the cap. The sort makes two runs over the same directory produce
/// the same palette order — muscle memory is a feature.
///
/// Exported for the same reason as [`is_workflow_file`]: anvil's diagnostics
/// report walks these directories with `fs::read_dir` and *no* caps at all,
/// which is the one place in the family that already ignores every bound the
/// loader enforces.
pub fn workflow_files_in(dir: &Path) -> Vec<PathBuf> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            log::warn!(
                "workflows: cannot list {}: {}",
                logged_path(dir),
                logged_reason(&error.to_string())
            );
            return Vec::new();
        }
    };
    let mut paths: Vec<PathBuf> = entries
        .take(MAX_DIRECTORY_ENTRIES)
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| is_workflow_file(path))
        .take(MAX_WORKFLOW_FILES_PER_DIRECTORY)
        .collect();
    paths.sort();
    paths
}

/// Load every workflow file under `dirs`, skipping what does not parse.
///
/// Missing directories are skipped silently; a file that fails to load is
/// logged with its path and the reason, because one broken file must never
/// disable the rest — and must never disappear without a trace either.
/// Duplicate names collapse to the first occurrence, so an earlier directory
/// overrides a later one: that is what lets `~/.config/<app>/workflows` shadow
/// an installed example by name.
pub fn load_all(dirs: &[PathBuf], order: LoadOrder) -> Vec<Workflow> {
    let mut out = Vec::new();
    let mut names = HashSet::new();
    'directories: for dir in dirs.iter().take(MAX_WORKFLOW_DIRECTORIES) {
        if !dir.is_dir() {
            continue;
        }
        for path in workflow_files_in(dir) {
            match load_one(&path) {
                Ok(workflow) => {
                    if names.insert(workflow.name.clone()) {
                        out.push(workflow);
                        if out.len() >= MAX_WORKFLOWS {
                            break 'directories;
                        }
                    }
                }
                Err(error) => {
                    // Both halves, not just the path: the reason quotes the
                    // file back at you. See `logged_reason`.
                    log::warn!(
                        "workflows: skipping {}: {}",
                        logged_path(&path),
                        logged_reason(&error)
                    )
                }
            }
        }
    }
    if matches!(order, LoadOrder::ByName) {
        out.sort_by(|a, b| a.name.cmp(&b.name));
    }
    out
}

/// Load and validate one workflow file.
///
/// Both formats go through serde derive against the same [`Workflow`] type, so
/// TOML and YAML cannot disagree about what a field means, and a field whose
/// type is wrong rejects the file instead of being coerced to the empty
/// string. The returned message names the problem: it is the only thing the
/// user will ever see about a file that vanished from their palette.
///
/// # The message is untrusted text
///
/// A parse error quotes the offending source line verbatim, so the `Err` half
/// carries bytes the file's author chose. It is left raw here on purpose — a
/// UI that renders it has its own escaping — but nothing may put it on a log
/// line or in a widget without crossing
/// [`crate::review_input::safe_inline_display`] first. [`load_all`] does.
pub fn load_one(path: &Path) -> Result<Workflow, String> {
    let text = read_bounded_workflow(path)?;
    let mut workflow: Workflow = match workflow_extension(path).as_deref() {
        Some("toml") => toml::from_str(&text).map_err(|error| format!("parse TOML: {error}"))?,
        Some("yaml") | Some("yml") => {
            serde_yaml_ng::from_str(&text).map_err(|error| format!("parse YAML: {error}"))?
        }
        _ => return Err("unsupported workflow extension".to_string()),
    };
    validate(&workflow)?;
    // Stamped only after validation, so a workflow carrying a source path is
    // one the loader accepted.
    workflow.source_path = Some(path.to_path_buf());
    Ok(workflow)
}

/// Read a workflow file with every hostile shape refused before parsing.
///
/// `O_NONBLOCK` so a FIFO planted in the directory cannot hang the scan on
/// `open`, `O_CLOEXEC` so a fork between open and read cannot leak the
/// descriptor, and `O_NOFOLLOW` so a symlink is refused outright rather than
/// resolved. anvil was the copy without `O_NOFOLLOW`: a link at
/// `~/.config/anvil/workflows/deploy.toml` pointing at a world-writable file
/// was followed and its command became a palette entry, while the same planted
/// link was refused by the other three. The size is checked twice — once from
/// the metadata of the *open descriptor* (not the path, which could have been
/// swapped) and once against what was actually read, because the file can grow
/// between the two.
fn read_bounded_workflow(path: &Path) -> Result<String, String> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOFOLLOW);
    }
    let file = options
        .open(path)
        .map_err(|error| format!("read: {error}"))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("inspect: {error}"))?;
    if !metadata.is_file() {
        return Err("source is not a regular file".to_string());
    }
    if metadata.len() > MAX_WORKFLOW_FILE_BYTES {
        return Err(format!(
            "source exceeds the {MAX_WORKFLOW_FILE_BYTES}-byte limit"
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_WORKFLOW_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read: {error}"))?;
    if bytes.len() as u64 > MAX_WORKFLOW_FILE_BYTES {
        return Err(format!(
            "source exceeds the {MAX_WORKFLOW_FILE_BYTES}-byte limit"
        ));
    }
    String::from_utf8(bytes).map_err(|error| format!("source is not UTF-8: {error}"))
}

/// Every rule a workflow must satisfy before it can reach a prompt.
///
/// Returns the reason rather than a bool: forge's copy returned `bool` and
/// logged `workflows: invalid tag` with no filename, in a search path of up to
/// sixty-four directories. The message *is* the diagnostic.
///
/// [`render`](super::render) re-runs this on the workflow it is given, so a
/// workflow assembled in memory rather than loaded from disk crosses the same
/// boundary.
pub fn validate(workflow: &Workflow) -> Result<(), String> {
    validate_display_field("name", &workflow.name, MAX_WORKFLOW_NAME_BYTES, false)?;
    validate_display_field(
        "description",
        &workflow.description,
        MAX_WORKFLOW_DESCRIPTION_BYTES,
        true,
    )?;
    if workflow.command.trim().is_empty() {
        return Err("workflow has empty command".to_string());
    }
    if workflow.command.len() > MAX_WORKFLOW_COMMAND_BYTES {
        return Err(format!(
            "workflow command exceeds {MAX_WORKFLOW_COMMAND_BYTES} bytes"
        ));
    }
    crate::review_input::validate(&workflow.command)
        .map_err(|error| format!("command is unsafe for review-only insertion: {error}"))?;
    if crate::review_input::contains_visual_spoofing(&workflow.command) {
        return Err(
            "workflow command contains an invisible or bidirectional character".to_string(),
        );
    }
    if workflow.tags.len() > MAX_WORKFLOW_TAGS {
        return Err(format!("workflow has more than {MAX_WORKFLOW_TAGS} tags"));
    }
    for tag in &workflow.tags {
        validate_display_field("tag", tag, MAX_WORKFLOW_FIELD_BYTES, false)?;
    }
    if let Some(shell) = &workflow.shell {
        validate_display_field("shell", shell, MAX_WORKFLOW_FIELD_BYTES, false)?;
    }
    if workflow.args.len() > MAX_WORKFLOW_ARGS {
        return Err(format!(
            "workflow has more than {MAX_WORKFLOW_ARGS} arguments"
        ));
    }
    let mut names = HashSet::new();
    for argument in &workflow.args {
        // Format-independent, deliberately: forge silently dropped a YAML
        // argument with a blank name — leaving its placeholder to render
        // verbatim into the command — while rejecting the same file in TOML.
        validate_binding_name("argument name", &argument.name)?;
        // Runs on names that are already trim-equal, so this is the *whole*
        // duplicate rule: without that, `"pid"` and `"pid "` were two accepted
        // arguments addressing one placeholder.
        if !names.insert(argument.name.as_str()) {
            return Err(format!("duplicate workflow argument '{}'", argument.name));
        }
        validate_display_field(
            "argument description",
            &argument.description,
            MAX_WORKFLOW_DESCRIPTION_BYTES,
            true,
        )?;
        if let Some(default) = &argument.default {
            // A default is command text, not a label: it gets the command
            // budget, and an empty one is legal — it is the declaration that
            // says an empty rendered value is meaningful here.
            if default.len() > MAX_WORKFLOW_COMMAND_BYTES {
                return Err(format!(
                    "default for '{}' exceeds {MAX_WORKFLOW_COMMAND_BYTES} bytes",
                    argument.name
                ));
            }
            if contains_unsafe_character(default) {
                return Err(format!(
                    "default for '{}' is unsafe for command insertion",
                    argument.name
                ));
            }
        }
    }
    Ok(())
}

/// One user-visible metadata field: non-empty (unless allowed), bounded, and
/// free of anything that could make it display differently from its bytes.
pub(super) fn validate_display_field(
    label: &str,
    value: &str,
    max_bytes: usize,
    allow_empty: bool,
) -> Result<(), String> {
    if !allow_empty && value.trim().is_empty() {
        return Err(format!("workflow has empty {label}"));
    }
    if value.len() > max_bytes {
        return Err(format!("workflow {label} exceeds {max_bytes} bytes"));
    }
    if contains_unsafe_character(value) {
        return Err(format!(
            "workflow {label} contains a control, invisible, or bidirectional character"
        ));
    }
    Ok(())
}

/// One name that has to *bind*: a declared argument, or a caller value's key.
///
/// A display field plus one rule — the name must equal its own trim.
/// [`render`](super::render) trims every placeholder name, because
/// `{{ service }}` is how mustache-convention shared libraries are written;
/// nothing trimmed the other side of that comparison. A declared
/// `name = "pid "` — a quoted TOML string, one invisible keystroke — loaded
/// clean and validated clean, and then matched nothing: `kill -9 {{ pid }}`
/// rendered as the literal `kill -9 { pid }`, the missing-value guard returned
/// `Ok` because the argument *had* a value, `ArgsForm::missing` reported the
/// form complete, and whatever the user typed into that row was dropped on the
/// way to their prompt. A declared default was dropped the same way.
///
/// Trim equality is the cheapest way to keep both sides of the lookup spelled
/// alike, and it is checked here rather than normalised at load because
/// [`render`](super::render) re-validates workflows built in memory too — a
/// normalisation that only ran in [`load_one`] would leave that half open.
pub(super) fn validate_binding_name(label: &str, value: &str) -> Result<(), String> {
    validate_display_field(label, value, MAX_WORKFLOW_FIELD_BYTES, false)?;
    if value != value.trim() {
        return Err(format!(
            "workflow {label} has leading or trailing whitespace"
        ));
    }
    Ok(())
}

/// Control characters *and* visual spoofing in one predicate, because every
/// site in this module needs both: a control character breaks the review-only
/// promise at the PTY, and a bidi override breaks the promise that what the
/// user reviewed is what they will run.
pub(super) fn contains_unsafe_character(value: &str) -> bool {
    value
        .chars()
        .any(|ch| ch.is_control() || crate::review_input::is_visual_spoofing_character(ch))
}

/// A path is untrusted text — an attacker who can drop a file in a scanned
/// directory picks its name — so it is sanitised and bounded before it reaches
/// a log line.
pub(super) fn logged_path(path: &Path) -> String {
    crate::review_input::safe_inline_display(&path.to_string_lossy(), MAX_LOGGED_PATH_BYTES)
}

/// So is the reason it is printed next to. `toml::from_str` echoes the source
/// line it failed on into its message, so `parse TOML: ...` for a file whose
/// unterminated string is `command = "echo <ESC>]0;title<BEL>` puts that OSC
/// sequence on a log line — where a tty tailing the log executes it. Bounded
/// and sanitised for the same reason as the path, and by the same call.
pub(super) fn logged_reason(reason: &str) -> String {
    crate::review_input::safe_inline_display(reason, MAX_LOGGED_REASON_BYTES)
}

/// A `log::Log` that keeps this module's own warn lines so a test can assert
/// what actually reached the log — the only place the reason half of a skip
/// line is ever rendered.
#[cfg(test)]
mod log_capture {
    use std::sync::{Mutex, OnceLock};

    /// Only this module's lines are kept, so the buffer stays bounded no
    /// matter what else the test binary logs in parallel.
    const PREFIX: &str = "workflows: ";

    static LINES: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    static CAPTURE: Capture = Capture;

    struct Capture;

    impl log::Log for Capture {
        fn enabled(&self, _: &log::Metadata<'_>) -> bool {
            true
        }

        fn log(&self, record: &log::Record<'_>) {
            let line = record.args().to_string();
            if line.starts_with(PREFIX) {
                lines().lock().unwrap().push(line);
            }
        }

        fn flush(&self) {}
    }

    fn lines() -> &'static Mutex<Vec<String>> {
        LINES.get_or_init(|| Mutex::new(Vec::new()))
    }

    /// Install once per process. No other test in this crate installs a
    /// logger, and `log` only accepts one, so a failure here means someone
    /// added a second and this test stopped observing anything — which must
    /// be loud rather than silently green.
    pub(super) fn install() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            log::set_logger(&CAPTURE).expect("no other logger may be installed in this binary");
            log::set_max_level(log::LevelFilter::Warn);
        });
    }

    /// Every captured line containing `needle`. Tests match on a marker unique
    /// to their own fixture, so they do not see each other's lines.
    pub(super) fn matching(needle: &str) -> Vec<String> {
        lines()
            .lock()
            .unwrap()
            .iter()
            .filter(|line| line.contains(needle))
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflows::testing::{workflow, TestDir};

    #[test]
    fn loads_toml_and_preserves_every_declared_field() {
        let dir = TestDir::new("toml-metadata");
        let path = dir.write(
            "deploy.toml",
            r#"name = "部署"
description = "发布服务"
command = "deploy {service}"
tags = ["ops", "中文"]
shell = "fish"

[[args]]
name = "service"
description = "服务名"
default = "api"
"#,
        );
        let loaded = load_one(&path).unwrap();
        assert_eq!(loaded.name, "部署");
        assert_eq!(loaded.tags, ["ops", "中文"]);
        assert_eq!(loaded.shell.as_deref(), Some("fish"));
        assert_eq!(loaded.args[0].default.as_deref(), Some("api"));
        assert_eq!(loaded.source_path.as_deref(), Some(path.as_path()));
    }

    #[test]
    fn loads_yaml_through_the_same_derive_as_toml() {
        let dir = TestDir::new("yaml-metadata");
        let path = dir.write(
            "deploy.yaml",
            "name: Deploy\ndescription: Deploy a service\ncommand: \"deploy {{service}}\"\n\
             tags: [ops, deploy]\nshell: bash\nargs:\n  - name: service\n    \
             description: Service name\n    default: api\n",
        );
        let loaded = load_one(&path).unwrap();
        assert_eq!(loaded.tags, ["ops", "deploy"]);
        assert_eq!(loaded.shell.as_deref(), Some("bash"));
        assert_eq!(loaded.args[0].default.as_deref(), Some("api"));
    }

    #[test]
    fn a_minimal_workflow_needs_only_a_name_and_a_command() {
        let dir = TestDir::new("minimal");
        let path = dir.write("x.toml", "name = \"Echo\"\ncommand = \"echo hi\"\n");
        let loaded = load_one(&path).unwrap();
        assert_eq!(loaded.description, "");
        assert!(loaded.tags.is_empty());
        assert!(loaded.shell.is_none());
        assert!(loaded.args.is_empty());
    }

    #[test]
    fn unknown_keys_are_ignored_so_one_library_can_serve_four_apps() {
        // No `deny_unknown_fields`: a file written for a newer app, or for
        // Warp, must still load everywhere.
        let dir = TestDir::new("unknown-keys");
        let path = dir.write(
            "x.yaml",
            "name: X\ncommand: echo x\nauthor: someone\nsource_url: https://example\n",
        );
        assert_eq!(load_one(&path).unwrap().name, "X");
    }

    #[test]
    fn a_type_wrong_field_rejects_the_whole_file() {
        // forge parsed TOML through `toml::Table` with
        // `as_str().unwrap_or("")`, so `default = 3000` — an unquoted port,
        // the most natural authoring mistake there is — became the empty
        // string and the file LOADED. The user saw a blank Port field and
        // `lsof -ti tcp: | xargs -r kill -TERM` reached their prompt.
        let dir = TestDir::new("type-wrong");
        let unquoted_port = dir.write(
            "kill-port.toml",
            "name = 'Kill port'\ncommand = 'lsof -ti tcp:{port} | xargs -r kill -TERM'\n\
             [[args]]\nname = 'port'\ndefault = 3000\n",
        );
        let error = load_one(&unquoted_port).unwrap_err();
        assert!(error.contains("parse TOML"), "got {error}");
        assert!(error.contains("invalid type"), "got {error}");

        // Same shape, two more spellings forge silently repaired.
        let mixed_tags = dir.write(
            "tags.toml",
            "name = 'T'\ncommand = 'echo t'\ntags = ['net', 1]\n",
        );
        assert!(load_one(&mixed_tags).unwrap_err().contains("parse TOML"));

        let nameless_arg = dir.write(
            "arg.toml",
            "name = 'A'\ncommand = 'echo {x}'\n[[args]]\ndescription = 'no name'\n",
        );
        let error = load_one(&nameless_arg).unwrap_err();
        assert!(error.contains("missing field"), "got {error}");
    }

    #[test]
    fn a_blank_argument_name_is_rejected_in_both_formats() {
        // forge filtered blank argument names out of YAML only, so the same
        // semantic input loaded in one format and was rejected in the other,
        // decided by nothing but the file extension. The YAML file loaded with
        // the argument silently missing, leaving its placeholder to render
        // verbatim into the command.
        let dir = TestDir::new("blank-arg-name");
        let yaml = dir.write(
            "blank.yaml",
            "name: Blank\ncommand: echo {x}\nargs:\n  - name: \"\"\n    description: nothing\n",
        );
        let toml = dir.write(
            "blank.toml",
            "name = 'Blank'\ncommand = 'echo {x}'\n[[args]]\nname = ''\n",
        );
        assert!(load_one(&yaml).unwrap_err().contains("empty argument name"));
        assert!(load_one(&toml).unwrap_err().contains("empty argument name"));
    }

    #[test]
    fn load_one_reports_why_rather_than_vanishing() {
        // forge built these strings and threw every one of them away, so a
        // rejected file disappeared from the palette with no log line at all.
        let dir = TestDir::new("diagnostics");
        let empty_command = dir.write("empty.yaml", "name: X\ncommand: \"\"\n");
        assert!(load_one(&empty_command)
            .unwrap_err()
            .contains("empty command"));

        let control = dir.write("control.yaml", "name: Unsafe\ncommand: \"echo\\tsecret\"\n");
        assert!(load_one(&control)
            .unwrap_err()
            .contains("unsafe for review-only insertion"));

        let broken = dir.write("broken.yaml", "name: [not\n");
        assert!(load_one(&broken).unwrap_err().contains("parse YAML"));

        let wrong_extension = dir.write("note.txt", "name: X\ncommand: echo x\n");
        assert!(load_one(&wrong_extension)
            .unwrap_err()
            .contains("unsupported workflow extension"));

        let missing = dir.path().join("absent.toml");
        assert!(load_one(&missing).unwrap_err().starts_with("read:"));
    }

    #[test]
    fn metadata_rejects_duplicate_arguments_and_visual_spoofing() {
        let mut duplicate = workflow("duplicate", "echo ok", &[("x", None), ("x", Some("ok"))]);
        assert!(validate(&duplicate)
            .unwrap_err()
            .contains("duplicate workflow argument"));

        duplicate.args.truncate(1);
        duplicate.name = "safe\u{202e}txt".into();
        assert!(validate(&duplicate).unwrap_err().contains("bidirectional"));
        duplicate.name = "safe".into();
        duplicate.command = "echo safe\u{200b}hidden".into();
        assert!(validate(&duplicate).unwrap_err().contains("bidirectional"));
        duplicate.command = "echo safe\u{e0020}hidden".into();
        assert!(validate(&duplicate).unwrap_err().contains("bidirectional"));
    }

    #[test]
    fn multiline_and_escape_sequence_commands_are_rejected() {
        let dir = TestDir::new("unsafe-commands");
        let block = dir.write(
            "block.yaml",
            "name: Unsafe\ncommand: |\n  echo one\n  echo two\n",
        );
        assert!(load_one(&block).is_err());
        let spoofed_name = dir.write(
            "name.toml",
            "name = 'safe\u{202e}txt'\ncommand = 'echo safe'\n",
        );
        assert!(load_one(&spoofed_name).is_err());
        let newline = dir.write(
            "nl.toml",
            "name = 'Unsafe'\ncommand = \"echo one\\necho two\"\n",
        );
        assert!(load_one(&newline).is_err());
        let escape = dir.write(
            "esc.toml",
            "name = 'Unsafe'\ncommand = \"echo \\u001b[31mred\"\n",
        );
        assert!(load_one(&escape).is_err());
    }

    #[test]
    fn validation_bounds_every_field_it_accepts() {
        let mut oversized = workflow("bounded", "echo ok", &[]);
        oversized.name = "n".repeat(MAX_WORKFLOW_NAME_BYTES + 1);
        assert!(validate(&oversized).unwrap_err().contains("name exceeds"));

        let mut tags = workflow("tags", "echo ok", &[]);
        tags.tags = (0..MAX_WORKFLOW_TAGS + 1)
            .map(|i| format!("t{i}"))
            .collect();
        assert!(validate(&tags).unwrap_err().contains("more than"));

        let mut args = workflow("args", "echo ok", &[]);
        args.args = (0..MAX_WORKFLOW_ARGS + 1)
            .map(|i| crate::workflows::WorkflowArg {
                name: format!("a{i}"),
                description: String::new(),
                default: None,
            })
            .collect();
        assert!(validate(&args).unwrap_err().contains("more than"));

        let huge_default = workflow(
            "default",
            "echo {x}",
            &[("x", Some(&"d".repeat(MAX_WORKFLOW_COMMAND_BYTES + 1)))],
        );
        assert!(validate(&huge_default).unwrap_err().contains("exceeds"));

        let unsafe_default = workflow("default", "echo {x}", &[("x", Some("a\u{202e}b"))]);
        assert!(validate(&unsafe_default)
            .unwrap_err()
            .contains("unsafe for command insertion"));
    }

    #[test]
    fn an_empty_default_is_legal_because_the_file_declared_it() {
        assert!(validate(&workflow("empty", "echo '{x}'", &[("x", Some(""))])).is_ok());
    }

    #[test]
    fn oversized_files_are_rejected_by_the_reader() {
        let dir = TestDir::new("oversize");
        let path = dir.path().join("oversized.yaml");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_WORKFLOW_FILE_BYTES + 1).unwrap();
        assert!(load_one(&path).unwrap_err().contains("byte limit"));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_workflow_is_refused_at_open() {
        // anvil opened with O_NONBLOCK | O_CLOEXEC only. A link planted in a
        // scanned directory, pointing at a world-writable file or another
        // user's home, was followed and its command became a palette entry —
        // while the identical link was refused by the other three apps. Four
        // apps reading one directory resolved to different libraries, and the
        // one WITHOUT the guard was the one loading attacker-reachable
        // content. The link target here is a perfectly valid workflow, so only
        // O_NOFOLLOW can be what rejects it.
        let dir = TestDir::new("symlink");
        let outside = TestDir::new("symlink-target");
        let target = outside.write("target.yaml", "name: T\ncommand: echo t\n");
        let link = dir.path().join("linked.yaml");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        assert!(load_one(&link).is_err());
        assert!(load_all(&[dir.path().to_path_buf()], LoadOrder::Precedence).is_empty());
        // The target itself still loads: the guard is about how it was
        // reached, not about the file.
        assert!(load_one(&target).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_is_rejected_without_blocking() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let dir = TestDir::new("fifo");
        let path = dir.path().join("blocked.yaml");
        let path_c = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: path_c is a live NUL-terminated pathname for this call.
        assert_eq!(unsafe { libc::mkfifo(path_c.as_ptr(), 0o600) }, 0);
        let started = std::time::Instant::now();
        assert!(load_one(&path).unwrap_err().contains("not a regular file"));
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn load_all_skips_invalid_files_but_returns_the_good_ones() {
        let dir = TestDir::new("skip-invalid");
        dir.write("a.yaml", "name: A\ncommand: echo a\n");
        dir.write("b.yaml", "this: is not a workflow\n");
        dir.write("c.yaml", "name: C\ncommand: echo c\n");
        dir.write("d.txt", "name: D\ncommand: echo d\n");
        dir.write("e.toml", "this is = not valid =");

        let loaded = load_all(&[dir.path().to_path_buf()], LoadOrder::Precedence);
        let names: Vec<&str> = loaded.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(names, ["A", "C"]);
    }

    #[test]
    fn a_missing_directory_is_not_an_error() {
        let missing = PathBuf::from("/nonexistent/jterm/workflows/never");
        assert!(load_all(&[missing], LoadOrder::ByName).is_empty());
    }

    #[test]
    fn earlier_directories_win_when_names_collide_across_formats() {
        let user = TestDir::new("precedence-user");
        let installed = TestDir::new("precedence-installed");
        user.write("override.toml", "name = 'Same'\ncommand = 'echo user'\n");
        installed.write("same.yaml", "name: Same\ncommand: echo installed\n");
        installed.write("other.yml", "name: Other\ncommand: echo other\n");

        let loaded = load_all(
            &[user.path().to_path_buf(), installed.path().to_path_buf()],
            LoadOrder::Precedence,
        );
        assert_eq!(loaded.iter().filter(|wf| wf.name == "Same").count(), 1);
        assert_eq!(
            loaded.iter().find(|wf| wf.name == "Same").unwrap().command,
            "echo user"
        );
        assert!(loaded.iter().any(|wf| wf.name == "Other"));
    }

    #[test]
    fn load_order_is_the_callers_choice_and_has_no_default() {
        // anvil and frost list in precedence order, ember and forge sort by
        // name, and the difference used to be one `sort_by` line present in
        // two copies. Both orders stay reachable, and neither is a default.
        let user = TestDir::new("order-user");
        let installed = TestDir::new("order-installed");
        user.write("z.toml", "name = 'Zulu'\ncommand = 'echo z'\n");
        installed.write("a.toml", "name = 'Alpha'\ncommand = 'echo a'\n");
        let dirs = [user.path().to_path_buf(), installed.path().to_path_buf()];

        let precedence: Vec<String> = load_all(&dirs, LoadOrder::Precedence)
            .into_iter()
            .map(|wf| wf.name)
            .collect();
        assert_eq!(precedence, ["Zulu", "Alpha"]);

        let by_name: Vec<String> = load_all(&dirs, LoadOrder::ByName)
            .into_iter()
            .map(|wf| wf.name)
            .collect();
        assert_eq!(by_name, ["Alpha", "Zulu"]);
    }

    #[test]
    fn one_directory_contributes_at_most_the_per_directory_cap() {
        let dir = TestDir::new("per-directory-cap");
        for index in 0..MAX_WORKFLOW_FILES_PER_DIRECTORY + 8 {
            dir.write(
                &format!("wf-{index:04}.toml"),
                &format!("name = 'wf-{index:04}'\ncommand = 'echo {index}'\n"),
            );
        }
        assert_eq!(
            workflow_files_in(dir.path()).len(),
            MAX_WORKFLOW_FILES_PER_DIRECTORY
        );
        assert_eq!(
            load_all(&[dir.path().to_path_buf()], LoadOrder::Precedence).len(),
            MAX_WORKFLOW_FILES_PER_DIRECTORY
        );
    }

    #[test]
    fn the_file_predicate_is_shared_rather_than_re_derived() {
        // anvil's diagnostics report and forge's test helper each grew their
        // own copy of this three-extension rule.
        assert!(is_workflow_file(Path::new("/w/a.toml")));
        assert!(is_workflow_file(Path::new("/w/a.YAML")));
        assert!(is_workflow_file(Path::new("/w/a.Yml")));
        assert!(!is_workflow_file(Path::new("/w/a.txt")));
        assert!(!is_workflow_file(Path::new("/w/toml")));
    }

    #[test]
    fn listed_files_are_filtered_and_sorted() {
        let dir = TestDir::new("listing");
        dir.write("b.toml", "");
        dir.write("a.yaml", "");
        dir.write("skip.txt", "");
        let listed = workflow_files_in(dir.path());
        assert_eq!(
            listed,
            [dir.path().join("a.yaml"), dir.path().join("b.toml")]
        );
    }

    #[test]
    fn a_logged_path_is_sanitised_and_bounded() {
        // Only forge did this. The other three wrote `path.display()` straight
        // into the log, so a filename carrying a bidi override rewrote the log
        // line around it.
        let rendered = logged_path(Path::new("/w/safe\u{202e}txt.toml"));
        assert!(!rendered.contains('\u{202e}'), "got {rendered}");
        let long = logged_path(&PathBuf::from("x".repeat(MAX_LOGGED_PATH_BYTES * 2)));
        assert!(long.len() <= MAX_LOGGED_PATH_BYTES);
    }

    #[test]
    fn a_logged_reason_is_sanitised_and_bounded_like_a_path() {
        let rendered = logged_reason("parse TOML: \u{1b}]0;PWNED\u{7}\nline two");
        assert!(!rendered.contains('\u{1b}'), "got {rendered:?}");
        assert!(!rendered.contains('\u{7}'), "got {rendered:?}");
        assert!(!rendered.contains('\n'), "got {rendered:?}");
        let long = logged_reason(&"x".repeat(MAX_LOGGED_REASON_BYTES * 2));
        assert!(long.len() <= MAX_LOGGED_REASON_BYTES);
    }

    #[test]
    fn a_skip_line_sanitises_the_reason_as_well_as_the_path() {
        // The path half was already sanitised; the reason half was
        // interpolated raw, and `toml::from_str` quotes the offending source
        // line back verbatim. A file whose unterminated string is
        // `command = "echo <ESC>]0;title<BEL>` therefore put that OSC sequence
        // on a warn line, where any tty tailing the log executes it — one
        // careful half and one hostile half is not a sanitised log line.
        //
        // forge is the app this matters most for: it discards its reader and
        // parser errors today, so migration is the moment it starts writing
        // file content to a log at all.
        log_capture::install();
        let dir = TestDir::new("log-sanitising");
        dir.write(
            "hostile-\u{202e}name.toml",
            "name = 'X'\ncommand = \"echo \u{1b}]0;victim@host\u{7} \u{202e}txet-idib\n",
        );

        assert!(load_all(&[dir.path().to_path_buf()], LoadOrder::Precedence).is_empty());

        let lines = log_capture::matching("hostile-");
        assert_eq!(lines.len(), 1, "expected one skip line, got {lines:?}");
        let line = &lines[0];
        assert!(line.contains("parse TOML"), "got {line:?}");
        for hostile in ['\u{1b}', '\u{7}', '\u{202e}', '\n', '\r'] {
            assert!(
                !line.contains(hostile),
                "{hostile:?} reached the log line: {line:?}"
            );
        }
    }

    #[test]
    fn a_padded_argument_name_is_rejected_rather_than_left_unbindable() {
        // `render` trims every placeholder name so `{{ pid }}` binds like
        // `{{pid}}`; nothing trimmed the declaration. A quoted `name = "pid "`
        // — one invisible keystroke, and TOML always quotes — loaded clean and
        // then matched nothing: `kill -9 {{ pid }}` rendered as the literal
        // `kill -9 { pid }`, the missing-value guard returned Ok because the
        // argument *had* a value, and whatever the user typed was discarded on
        // the way to the prompt.
        let dir = TestDir::new("padded-arg-name");
        let padded = dir.write(
            "kill.toml",
            "name = 'Kill'\ncommand = 'kill -9 {{ pid }}'\n[[args]]\nname = 'pid '\n",
        );
        let error = load_one(&padded).unwrap_err();
        assert!(
            error.contains("argument name has leading or trailing whitespace"),
            "got {error}"
        );

        // A leading space is the same mistake and the same answer, and YAML
        // reaches it the moment the scalar is quoted.
        let leading = dir.write(
            "kill.yaml",
            "name: Kill\ncommand: \"kill -9 {{pid}}\"\nargs:\n  - name: \" pid\"\n",
        );
        assert!(load_one(&leading)
            .unwrap_err()
            .contains("has leading or trailing whitespace"));

        // Two spellings of one placeholder must not both be accepted, or the
        // duplicate rule would not be the whole duplicate rule.
        let both = dir.write(
            "dup.toml",
            "name = 'Dup'\ncommand = 'echo {{pid}}'\n\
             [[args]]\nname = 'pid'\n[[args]]\nname = 'pid '\n",
        );
        assert!(load_one(&both).is_err());

        // Trimmed, it loads and binds — the fix rejects the ambiguity, not the
        // argument.
        let clean = dir.write(
            "ok.toml",
            "name = 'Kill'\ncommand = 'kill -9 {{ pid }}'\n[[args]]\nname = 'pid'\n",
        );
        assert_eq!(load_one(&clean).unwrap().args[0].name, "pid");
    }

    #[test]
    fn the_refresh_latch_admits_one_scan_and_re_arms() {
        let mut latch = RefreshLatch::default();
        assert!(latch.begin());
        assert!(latch.is_in_flight());
        assert!(!latch.begin(), "a second request reuses the in-flight scan");
        latch.finish();
        assert!(!latch.is_in_flight());
        assert!(latch.begin(), "completion must allow a later refresh");
    }
}
