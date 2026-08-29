//! Turning a template plus values into text the user will see at their prompt.
//!
//! The engine is intentionally tiny — named substitution and literal brace
//! escapes, no conditionals, no loops — and every byte it emits is counted, so
//! a template that references one argument a thousand times cannot amplify a
//! 64 KiB value into gigabytes.

use std::collections::{HashMap, HashSet};

use super::loader::{contains_unsafe_character, validate, validate_binding_name};
use super::{Workflow, WorkflowArg, MAX_WORKFLOW_ARGS, MAX_WORKFLOW_COMMAND_BYTES};

/// Substitute `{name}` and `{{name}}` placeholders from `bindings`.
///
/// Unknown single-brace placeholders stay visible so the user sees their typo;
/// a `{{...}}` with no matching binding is the literal-brace escape and emits
/// one brace, mirroring `format!`. A `{{` with no `}}` that closes *it* —
/// counting nesting, so a later pair's close does not count — is preserved
/// exactly as authored instead, which is what keeps `awk '{{print $1}' f`
/// meaning the same thing wherever it appears. Placeholder names are trimmed,
/// so `{{ service }}` — how mustache-convention shared libraries are written —
/// binds exactly like `{{service}}`, and a declared argument name is held to
/// the same spelling ([`validate`](super::validate)) so both sides of that
/// lookup agree.
///
/// # This is not the safety boundary
///
/// It validates nothing about `bindings` and nothing about its output; only
/// the size caps apply. Every path that inserts text at a prompt must go
/// through [`render`], which re-validates the workflow, the values and the
/// rendered result. forge's argument dialog called the equivalent of this
/// function directly, which is how its zero-argument path came to skip
/// validation entirely.
pub fn substitute(template: &str, bindings: &[(String, String)]) -> Result<String, String> {
    render_template(template, bindings, &HashSet::new()).map(|(rendered, _)| rendered)
}

/// Walk the template once, emitting into a budget.
///
/// `missing_bindings` names the declared arguments that have no value at all.
/// They are collected rather than substituted, so the caller can report every
/// unfilled placeholder in one message instead of one per attempt — and only
/// the ones the template actually references, because an argument a file
/// declares but never uses is the file's problem, not the user's.
fn render_template(
    template: &str,
    bindings: &[(String, String)],
    missing_bindings: &HashSet<String>,
) -> Result<(String, Vec<String>), String> {
    if template.len() > MAX_WORKFLOW_COMMAND_BYTES {
        return Err(format!(
            "workflow command exceeds {MAX_WORKFLOW_COMMAND_BYTES} bytes"
        ));
    }
    let mut out = String::with_capacity(template.len().min(MAX_WORKFLOW_COMMAND_BYTES));
    let bytes = template.as_bytes();
    // Looking for a close separately at every `{{` makes a command made of
    // unmatched opening braces quadratic. The command budget is deliberately
    // large enough for real scripts (64 KiB), so that is still billions of
    // byte comparisons on the UI insertion path. Compute every opener's
    // answer together in one reverse pass instead. The precomputation keeps
    // the old scanner's exact overlapping-brace semantics; see its exhaustive
    // equivalence test below.
    let double_brace_closes = matching_double_brace_closes(bytes);
    // The single-brace branch has the same trap in a smaller shape: calling
    // `position('}')` for every unmatched `{` repeatedly scans the suffix.
    // One reverse pass gives every byte offset its next `}` in O(1) lookup
    // time while preserving the original "first close wins" rule exactly.
    let next_closing_brace = next_closing_braces(bytes);
    let mut missing = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'{' {
            if i + 1 < bytes.len() && bytes[i + 1] == b'{' {
                if let Some(end) = double_brace_closes[i] {
                    let name = template[i + 2..end].trim();
                    if let Some((_, value)) = bindings.iter().find(|(key, _)| key == name) {
                        push_rendered(&mut out, value)?;
                        i = end + 2;
                        continue;
                    }
                    if missing_bindings.contains(name) {
                        if !missing.iter().any(|entry| entry == name) {
                            missing.push(name.to_owned());
                        }
                        i = end + 2;
                        continue;
                    }
                    // No binding means `{{...}}` is a literal-brace escape.
                    push_rendered(&mut out, "{")?;
                    i += 2;
                    continue;
                }
                // Preserve an unterminated pair exactly as authored: advance by
                // ONE so the second brace is re-scanned and `awk '{{print $1}'`
                // round-trips. forge advanced by two here and silently turned
                // that into a different, executable awk program.
                push_rendered(&mut out, "{")?;
                i += 1;
                continue;
            }

            if let Some(end) = next_closing_brace[i + 1] {
                let name = template[i + 1..end].trim();
                if let Some((_, value)) = bindings.iter().find(|(key, _)| key == name) {
                    push_rendered(&mut out, value)?;
                } else if missing_bindings.contains(name) {
                    if !missing.iter().any(|entry| entry == name) {
                        missing.push(name.to_owned());
                    }
                } else {
                    push_rendered(&mut out, &template[i..=end])?;
                }
                i = end + 1;
                continue;
            }
        } else if bytes[i] == b'}' && i + 1 < bytes.len() && bytes[i + 1] == b'}' {
            push_rendered(&mut out, "}")?;
            i += 2;
            continue;
        }

        // Everything else advances by whole Unicode scalar value, never by raw
        // byte: `i` must stay on a UTF-8 boundary for the slicing above.
        let character = template[i..]
            .chars()
            .next()
            .expect("i always points to a UTF-8 boundary");
        let mut encoded = [0_u8; 4];
        push_rendered(&mut out, character.encode_utf8(&mut encoded))?;
        i += character.len_utf8();
    }

    Ok((out, missing))
}

fn push_rendered(output: &mut String, addition: &str) -> Result<(), String> {
    if output.len().saturating_add(addition.len()) > MAX_WORKFLOW_COMMAND_BYTES {
        return Err(format!(
            "rendered command exceeds {MAX_WORKFLOW_COMMAND_BYTES} bytes"
        ));
    }
    output.push_str(addition);
    Ok(())
}

#[derive(Clone, Copy)]
enum DoubleBraceToken {
    Open(usize),
    Close(usize),
}

/// The matching `}}` for every `{{` byte offset, computed in linear time.
///
/// `{{` and `}}` nest, so this counts depth instead of taking the first `}}`
/// it happens to see. Taking the first one made the *whole* choice at the call
/// site — escape and consume both braces, or preserve and consume one —
/// depend on bytes arbitrarily far to the right, which reinstated the exact
/// forge defect this module says it closed:
///
/// ```text
/// awk '{{print $1}' access.log          -> preserved (no `}}` anywhere)
/// awk '{{print $1}' {{log}} | sort -u   -> `awk '{print $1}' …`
/// ```
///
/// The second template's `}}` belongs to `{{log}}`, but the first `{{` claimed
/// it, fell into the escape branch, and handed the user a different — and
/// executable — awk program. Counting depth gives that `{{` its own answer:
/// unmatched in both templates, so both preserve it, while a genuinely nested
/// escape (`-d '{{"a":{{"b":1}}}}'` -> `{"a":{"b":1}}`) still finds the pair
/// that matches it. For ordinary placeholder names, the depth walk therefore
/// only changes what happens to literal shell/JSON brace syntax.
///
/// There is a subtle reason this is a reverse dynamic program rather than a
/// conventional global brace stack. After an unmatched `{{`, rendering emits
/// the first brace and re-scans the second, so `{{{{}}` asks about openers at
/// overlapping offsets. A global tokenizer would only record offsets 0 and 2
/// and could change which unusual-but-valid binding name is seen at offset 1.
///
/// `first_close[p]` answers a virtual opener whose body scan starts at `p`.
/// The next token either closes it immediately, or opens a nested pair. In the
/// nested case both the nested close and the answer after it are already known
/// because their offsets are to the right. Every array cell is therefore
/// visited a constant number of times.
fn matching_double_brace_closes(bytes: &[u8]) -> Vec<Option<usize>> {
    let len = bytes.len();
    let mut next_token = vec![None; len + 1];
    for offset in (0..len).rev() {
        next_token[offset] =
            if offset + 1 < len && bytes[offset] == b'{' && bytes[offset + 1] == b'{' {
                Some(DoubleBraceToken::Open(offset))
            } else if offset + 1 < len && bytes[offset] == b'}' && bytes[offset + 1] == b'}' {
                Some(DoubleBraceToken::Close(offset))
            } else {
                next_token[offset + 1]
            };
    }

    let mut first_close = vec![None; len + 1];
    for offset in (0..len).rev() {
        first_close[offset] = match next_token[offset] {
            None => None,
            Some(DoubleBraceToken::Close(close)) => Some(close),
            Some(DoubleBraceToken::Open(open)) => {
                first_close[open + 2].and_then(|nested_close| first_close[nested_close + 2])
            }
        };
    }

    let mut matching = vec![None; len];
    for offset in 0..len.saturating_sub(1) {
        if bytes[offset] == b'{' && bytes[offset + 1] == b'{' {
            matching[offset] = first_close[offset + 2];
        }
    }
    matching
}

/// For every starting offset, the first `}` at or after it.
fn next_closing_braces(bytes: &[u8]) -> Vec<Option<usize>> {
    let mut next = vec![None; bytes.len() + 1];
    for offset in (0..bytes.len()).rev() {
        next[offset] = if bytes[offset] == b'}' {
            Some(offset)
        } else {
            next[offset + 1]
        };
    }
    next
}

/// Render a workflow with caller values and declared defaults.
///
/// This is the family's single insertion path — a zero-argument workflow comes
/// through here too, which is what makes the documented `{{ }}` escape work
/// everywhere. It re-validates the workflow, bounds the values, refuses any
/// value carrying a control or spoofing character, and puts the finished text
/// back across [`crate::review_input::validate`].
///
/// # The unfilled-argument rule
///
/// A declared argument whose file gives **no default** and whose value is
/// absent or blank is *not supplied*: it is aggregated into
/// `missing values: <names>` rather than substituted as the empty string. An
/// argument that declares a default — `default = ""` included — may render
/// empty, because the declaration is what says an empty value is meaningful
/// there.
///
/// The rule lives here, and not only in [`ArgsForm`], because every UI in this
/// family used to pre-seed each declared argument with `""` before calling
/// this function, which made the guard unreachable from all four apps while
/// three of them unit-tested it. `kill -9 {pid}` with an untouched field
/// rendered `kill -9 ` and was typed at the prompt. A caller cannot seed its
/// way past a rule that inspects the values themselves.
pub fn render(workflow: &Workflow, values: &HashMap<String, String>) -> Result<String, String> {
    validate(workflow)?;
    if values.len() > MAX_WORKFLOW_ARGS {
        return Err(format!(
            "workflow received more than {MAX_WORKFLOW_ARGS} values"
        ));
    }
    for (name, value) in values {
        // A key is one side of a lookup whose other side is trimmed, so it is
        // held to the same spelling as a declared argument: a padded key can
        // never bind, and silently binding nothing is how a typed value
        // disappears between the dialog and the prompt.
        validate_binding_name("value name", name)?;
        if value.len() > MAX_WORKFLOW_COMMAND_BYTES {
            return Err(format!(
                "value for '{name}' exceeds {MAX_WORKFLOW_COMMAND_BYTES} bytes"
            ));
        }
        if contains_unsafe_character(value) {
            return Err(format!(
                "value for '{name}' is unsafe for review-only insertion"
            ));
        }
    }

    // Declared, undefaulted, and blank or absent: not supplied. Claimed first
    // so the binding list below cannot bind the blank string that a pre-seeded
    // form supplies.
    let mut missing_bindings: HashSet<String> = HashSet::new();
    for argument in &workflow.args {
        if argument.default.is_none()
            && values
                .get(&argument.name)
                .is_none_or(|value| value.trim().is_empty())
        {
            missing_bindings.insert(argument.name.clone());
        }
    }
    // Caller values bind, including names the workflow never declared: a
    // shared library's template may reference a placeholder the app fills from
    // its own context.
    let mut bindings: Vec<(String, String)> = values
        .iter()
        .filter(|(name, _)| !missing_bindings.contains(name.as_str()))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    // A declared default fills any argument the caller left out entirely.
    for argument in &workflow.args {
        if !values.contains_key(&argument.name) {
            if let Some(default) = &argument.default {
                bindings.push((argument.name.clone(), default.clone()));
            }
        }
    }

    let (out, missing) = render_template(&workflow.command, &bindings, &missing_bindings)?;
    if !missing.is_empty() {
        return Err(format!("missing values: {}", missing.join(", ")));
    }
    crate::review_input::validate(&out)
        .map_err(|error| format!("command is unsafe for review-only insertion: {error}"))?;
    Ok(out)
}

/// One slot of an [`ArgsForm`].
///
/// The enum is the point. forge typed its argument default as `String` and
/// could not represent "no default"; every app's dialog then flattened
/// "untouched" into "supplied empty" the moment it seeded its widgets. Keeping
/// the two apart is what lets a form say *which* fields are still outstanding
/// before the user presses Insert.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Slot {
    /// Nothing has been supplied for this argument. For an argument with a
    /// declared default this state is unreachable after construction; for one
    /// without, it is where the form starts and where [`ArgsForm::clear`]
    /// returns it.
    Unset,
    Supplied(String),
}

/// The parameter-fill model behind every app's workflow dialog.
///
/// All four UIs built this by hand, over a `HashMap` (anvil, forge) or a
/// `Vec<String>` aligned with `workflow.args` (ember, frost), and all four
/// seeded every declared argument with `""`. The form keeps the index-aligned
/// shape the immediate-mode UIs need while carrying the distinction the
/// `Vec<String>` lost.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArgsForm {
    workflow: Workflow,
    slots: Vec<Slot>,
}

impl ArgsForm {
    /// A form for `workflow`, each slot seeded from its declared default.
    ///
    /// An argument with no declared default starts unset — visibly empty in
    /// the UI, and *not supplied* as far as [`render`] is concerned.
    pub fn new(workflow: Workflow) -> Self {
        let slots = workflow
            .args
            .iter()
            .map(|argument| match &argument.default {
                Some(default) => Slot::Supplied(default.clone()),
                None => Slot::Unset,
            })
            .collect();
        Self { workflow, slots }
    }

    pub fn workflow(&self) -> &Workflow {
        &self.workflow
    }

    /// The declared arguments, in file order — one row each.
    pub fn args(&self) -> &[WorkflowArg] {
        &self.workflow.args
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// What the widget for row `index` should show. Empty for an untouched
    /// undefaulted argument, which is also what it *means*.
    pub fn value(&self, index: usize) -> &str {
        match self.slots.get(index) {
            Some(Slot::Supplied(value)) => value,
            _ => "",
        }
    }

    /// Whether row `index` has a value at all. A UI can hint the outstanding
    /// rows with this without waiting for a failed render.
    pub fn is_set(&self, index: usize) -> bool {
        matches!(self.slots.get(index), Some(Slot::Supplied(_)))
    }

    /// Record what the user typed. An out-of-range index is ignored, so a
    /// stale widget callback after the form was replaced cannot panic.
    ///
    /// An empty string is recorded as supplied, not as unset: whether it is
    /// *usable* is [`render`]'s rule, applied once, in one place. For an
    /// argument that declares a default, emptying the field is a deliberate
    /// empty value and renders as one — it does not fall back to the default.
    pub fn set(&mut self, index: usize, value: impl Into<String>) {
        if let Some(slot) = self.slots.get_mut(index) {
            *slot = Slot::Supplied(value.into());
        }
    }

    /// Return row `index` to its declared default, or to unset when the file
    /// declared none. This is the "revert" affordance, and it is the only way
    /// back to unset — which is why it is not the same as `set(index, "")`.
    pub fn clear(&mut self, index: usize) {
        let default = self
            .workflow
            .args
            .get(index)
            .and_then(|argument| argument.default.clone());
        if let Some(slot) = self.slots.get_mut(index) {
            *slot = match default {
                Some(default) => Slot::Supplied(default),
                None => Slot::Unset,
            };
        }
    }

    /// The declared arguments this form would not supply a value for: no
    /// declared default, and nothing (or only whitespace) typed.
    ///
    /// [`ArgsForm::render`] fails on these only when the template actually
    /// references them, so this is the superset a UI should highlight, not a
    /// prediction of the exact error text.
    pub fn missing(&self) -> Vec<&str> {
        self.workflow
            .args
            .iter()
            .enumerate()
            .filter(|(index, argument)| {
                argument.default.is_none() && self.value(*index).trim().is_empty()
            })
            .map(|(_, argument)| argument.name.as_str())
            .collect()
    }

    /// The values map this form will render with. Unset slots are absent, which
    /// is exactly how [`render`] learns they were never filled.
    pub fn values(&self) -> HashMap<String, String> {
        self.workflow
            .args
            .iter()
            .zip(self.slots.iter())
            .filter_map(|(argument, slot)| match slot {
                Slot::Supplied(value) => Some((argument.name.clone(), value.clone())),
                Slot::Unset => None,
            })
            .collect()
    }

    /// Render the workflow with the current values.
    pub fn render(&self) -> Result<String, String> {
        render(&self.workflow, &self.values())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflows::testing::workflow;

    fn values(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect()
    }

    /// The previous per-opener scan, kept only as a small-input oracle for the
    /// linear precomputation. In particular, it observes opening pairs at
    /// overlapping offsets after an unmatched pair; a conventional global
    /// stack does not.
    fn legacy_find_close(bytes: &[u8], from: usize) -> Option<usize> {
        let mut depth = 1_usize;
        let mut offset = from;
        while offset + 1 < bytes.len() {
            if bytes[offset] == b'{' && bytes[offset + 1] == b'{' {
                depth += 1;
                offset += 2;
                continue;
            }
            if bytes[offset] == b'}' && bytes[offset + 1] == b'}' {
                depth -= 1;
                if depth == 0 {
                    return Some(offset);
                }
                offset += 2;
                continue;
            }
            offset += 1;
        }
        None
    }

    #[test]
    fn precomputed_double_brace_closes_match_the_old_scanner_exactly() {
        // Exhaust every short mixture, not only balanced expressions. The
        // hostile inputs are the unmatched and overlapping ones, and this is
        // small enough to keep an independent quadratic oracle harmless.
        const ALPHABET: [u8; 3] = *b"{}x";
        for len in 0..=9_u32 {
            for mut encoded in 0..3_usize.pow(len) {
                let mut bytes = Vec::with_capacity(len as usize);
                for _ in 0..len {
                    bytes.push(ALPHABET[encoded % ALPHABET.len()]);
                    encoded /= ALPHABET.len();
                }
                let closes = matching_double_brace_closes(&bytes);
                for offset in 0..bytes.len().saturating_sub(1) {
                    if bytes[offset] == b'{' && bytes[offset + 1] == b'{' {
                        assert_eq!(
                            closes[offset],
                            legacy_find_close(&bytes, offset + 2),
                            "template {:?}, opener {offset}",
                            String::from_utf8_lossy(&bytes)
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn an_input_budget_of_unmatched_openers_stays_a_literal_command() {
        // This is the former quadratic case: every opener scanned the entire
        // remaining 64 KiB looking for a close. It is also a semantic check —
        // an unmatched pair must survive byte-for-byte, however many there
        // are.
        let template = "{".repeat(MAX_WORKFLOW_COMMAND_BYTES);
        assert_eq!(substitute(&template, &[]).unwrap(), template);
    }

    #[test]
    fn an_input_budget_of_unmatched_single_placeholders_is_linear_and_literal() {
        // `position('}')` from every `{` made this second adversarial shape
        // quadratic even after double-brace matching was precomputed.
        let template = "{x".repeat(MAX_WORKFLOW_COMMAND_BYTES / 2);
        assert_eq!(template.len(), MAX_WORKFLOW_COMMAND_BYTES);
        assert_eq!(substitute(&template, &[]).unwrap(), template);
    }

    #[test]
    fn a_single_placeholder_takes_the_supplied_value() {
        let workflow = workflow("t", "git rebase -i {{target}}", &[("target", None)]);
        assert_eq!(
            render(&workflow, &values(&[("target", "origin/main")])).unwrap(),
            "git rebase -i origin/main"
        );
    }

    #[test]
    fn a_declared_default_fills_an_argument_the_caller_left_out() {
        let workflow = workflow(
            "t",
            "echo {{greeting}} {{name}}",
            &[("greeting", Some("hi")), ("name", Some("world"))],
        );
        assert_eq!(render(&workflow, &HashMap::new()).unwrap(), "echo hi world");
    }

    #[test]
    fn the_same_argument_can_appear_more_than_once() {
        let workflow = workflow("t", "cp {{f}} {{f}}.bak", &[("f", None)]);
        assert_eq!(
            render(&workflow, &values(&[("f", "config.toml")])).unwrap(),
            "cp config.toml config.toml.bak"
        );
    }

    #[test]
    fn a_zero_argument_workflow_still_goes_through_the_template_engine() {
        // forge inserted the raw template whenever `args` was empty, at both
        // of its activation sites, so its own documented literal-brace escape
        // was not applied there: this command reached the prompt with the
        // braces still doubled, and the shell read it as a brace expansion
        // producing a malformed body. One file on disk, two different commands
        // typed at the user's prompt.
        let workflow = workflow(
            "deploy",
            r#"curl -X POST -d '{{"env":"prod"}}' https://api/deploy"#,
            &[],
        );
        assert_eq!(
            render(&workflow, &HashMap::new()).unwrap(),
            r#"curl -X POST -d '{"env":"prod"}' https://api/deploy"#
        );
    }

    #[test]
    fn placeholder_names_are_trimmed() {
        // Third-party and Warp-style libraries write mustache placeholders
        // with inner spaces. forge did not trim, so no binding matched
        // " bastion ", the `{{` fell through to the literal-brace path, and
        // the command reached the prompt as `ssh { bastion }` — placeholder
        // names and all.
        let workflow = workflow(
            "tunnel",
            "ssh {{ bastion }} -L { local_port }:localhost:5432",
            &[("bastion", None), ("local_port", None)],
        );
        assert_eq!(
            render(
                &workflow,
                &values(&[("bastion", "jump.example"), ("local_port", "6543")])
            )
            .unwrap(),
            "ssh jump.example -L 6543:localhost:5432"
        );
    }

    #[test]
    fn an_unterminated_double_brace_survives_verbatim() {
        // Three copies advance by one byte so the second brace is re-scanned;
        // forge advanced by two and turned `awk '{{print $1}' f` into
        // `awk '{print $1}' f` — a different, and actually executable, awk
        // program. Keeping it unchanged keeps the failure mode predictable.
        let unterminated = workflow("t", "echo {{not_closed", &[]);
        assert_eq!(
            render(&unterminated, &HashMap::new()).unwrap(),
            "echo {{not_closed"
        );

        let awk = workflow("awk", "awk '{{print $1}' file", &[]);
        assert_eq!(
            render(&awk, &HashMap::new()).unwrap(),
            "awk '{{print $1}' file"
        );
    }

    #[test]
    fn an_unterminated_double_brace_survives_a_placeholder_later_in_the_template() {
        // The rule above held only while nothing else in the template closed a
        // pair. The close was found by scanning to the END of the template, so
        // `{{print` claimed the `}}` belonging to `{{log}}`, took the escape
        // branch, and consumed a brace it did not own — reinstating forge's
        // defect through the copy that claims to have fixed it, in the shape a
        // parameterised awk/jq/sed command actually has. The identical leading
        // bytes `awk '{{print $1}` must mean the identical thing whatever
        // follows them.
        let mixed = workflow(
            "awk",
            "awk '{{print $1}' {{log}} | sort -u",
            &[("log", Some("access.log"))],
        );
        assert_eq!(
            render(&mixed, &HashMap::new()).unwrap(),
            "awk '{{print $1}' access.log | sort -u"
        );

        // A trailing escape was enough to flip it too.
        let commented = workflow("awk", "awk '{{print $1}' file # {{x}}", &[]);
        assert_eq!(
            render(&commented, &HashMap::new()).unwrap(),
            "awk '{{print $1}' file # {x}"
        );

        // And the missing-value guard must still see the placeholder that IS
        // one, rather than the first brace swallowing it.
        let unfilled = workflow("awk", "awk '{{print $1}' {{log}}", &[("log", None)]);
        assert!(render(&unfilled, &HashMap::new())
            .unwrap_err()
            .contains("missing values: log"));
    }

    #[test]
    fn nested_literal_braces_still_escape_as_a_pair() {
        // The other half of the same decision: `{{` and `}}` nest, so a
        // matching close that is genuinely this pair's must still be found.
        // Bounding the lookahead at the next `{` would have made both awk
        // cases right and quietly broken every nested JSON body.
        let nested = workflow(
            "deploy",
            r#"curl -X POST -d '{{"a":{{"b":1}}}}' https://api"#,
            &[],
        );
        assert_eq!(
            render(&nested, &HashMap::new()).unwrap(),
            r#"curl -X POST -d '{"a":{"b":1}}' https://api"#
        );
    }

    #[test]
    fn unicode_survives_both_placeholder_styles_and_literal_braces() {
        let workflow = workflow(
            "发布",
            "发布 {服务} 到 {{环境}}，保留 {{a,b}} 🚀",
            &[("服务", None), ("环境", None)],
        );
        assert_eq!(
            render(&workflow, &values(&[("服务", "接口"), ("环境", "生产")])).unwrap(),
            "发布 接口 到 生产，保留 {a,b} 🚀"
        );
    }

    #[test]
    fn an_unknown_single_brace_placeholder_stays_visible() {
        assert_eq!(
            substitute(
                "hi {name}, your role is {role}",
                &[("name".into(), "Bea".into())]
            )
            .unwrap(),
            "hi Bea, your role is {role}"
        );
        assert_eq!(
            substitute(
                "deploy {env} {target}",
                &[
                    ("env".into(), "prod".into()),
                    ("target".into(), "api".into()),
                ]
            )
            .unwrap(),
            "deploy prod api"
        );
        assert_eq!(
            substitute("shell brace expansion: {{a,b,c}}", &[]).unwrap(),
            "shell brace expansion: {a,b,c}"
        );
        let unchanged = "git status --porcelain";
        assert_eq!(substitute(unchanged, &[]).unwrap(), unchanged);
        assert_eq!(
            substitute("🚀 deploy {env} 完了", &[("env".into(), "prod".into())]).unwrap(),
            "🚀 deploy prod 完了"
        );
    }

    #[test]
    fn an_unfilled_argument_with_no_default_is_a_missing_value() {
        // THE defect that was in all four apps. The guard existed and was
        // unit-tested in three of them, and every UI in the family defeated it
        // by pre-seeding each declared argument with "" before calling render.
        // `kill -9 {pid}` with an untouched Pid field rendered `kill -9 ` —
        // non-blank, so review_input accepted it — and was typed at the
        // prompt. Both spellings of "nothing was filled in" now report it.
        let workflow = workflow("t", "kill -9 {{pid}}", &[("pid", None)]);

        let absent = render(&workflow, &HashMap::new()).unwrap_err();
        assert!(absent.contains("missing values: pid"), "got {absent}");

        let pre_seeded = render(&workflow, &values(&[("pid", "")])).unwrap_err();
        assert!(
            pre_seeded.contains("missing values: pid"),
            "a pre-seeded empty value must not bypass the guard, got {pre_seeded}"
        );

        let whitespace = render(&workflow, &values(&[("pid", "   ")])).unwrap_err();
        assert!(
            whitespace.contains("missing values: pid"),
            "got {whitespace}"
        );

        assert_eq!(
            render(&workflow, &values(&[("pid", "421")])).unwrap(),
            "kill -9 421"
        );
    }

    #[test]
    fn an_empty_value_renders_when_the_file_declared_a_default() {
        // The declaration is what says an empty value is meaningful here, so
        // an author who wants one writes `default = ""` — and a user who
        // clears a defaulted field gets the empty value they asked for rather
        // than the default sneaking back.
        let declared = workflow("t", "grep -r '{{pattern}}' .", &[("pattern", Some("TODO"))]);
        assert_eq!(
            render(&declared, &values(&[("pattern", "")])).unwrap(),
            "grep -r '' ."
        );

        let empty_default = workflow("t", "ls {{flags}} .", &[("flags", Some(""))]);
        assert_eq!(render(&empty_default, &HashMap::new()).unwrap(), "ls  .");
    }

    #[test]
    fn missing_values_are_aggregated_and_only_for_referenced_placeholders() {
        let referenced = workflow(
            "t",
            "ssh {{host}} -p {{port}}",
            &[("host", None), ("port", None)],
        );
        let error = render(&referenced, &HashMap::new()).unwrap_err();
        assert!(error.contains("host"), "got {error}");
        assert!(error.contains("port"), "got {error}");

        // A declared argument the template never uses is the file's problem,
        // not something to block the user with.
        let unreferenced = workflow("t", "echo static", &[("unused", None)]);
        assert_eq!(
            render(&unreferenced, &HashMap::new()).unwrap(),
            "echo static"
        );
    }

    #[test]
    fn a_value_the_workflow_never_declared_still_binds() {
        // A shared library's template may reference a placeholder the app
        // fills from its own context.
        let workflow = workflow("t", "echo {{cwd}}", &[]);
        assert_eq!(
            render(&workflow, &values(&[("cwd", "/tmp")])).unwrap(),
            "echo /tmp"
        );
    }

    #[test]
    fn values_cross_the_review_only_boundary() {
        let workflow = workflow("unsafe", "echo {value}", &[("value", None)]);
        assert!(render(&workflow, &values(&[("value", "ok\nrm -rf /")]))
            .unwrap_err()
            .contains("unsafe for review-only insertion"));
        assert!(render(&workflow, &values(&[("value", "safe\u{202e}txt")]))
            .unwrap_err()
            .contains("unsafe for review-only insertion"));

        let too_many: HashMap<String, String> = (0..MAX_WORKFLOW_ARGS + 1)
            .map(|index| (format!("arg{index}"), "x".to_string()))
            .collect();
        assert!(render(&workflow, &too_many)
            .unwrap_err()
            .contains("more than"));

        let unsafe_key = values(&[("na\u{202e}me", "x")]);
        assert!(render(&workflow, &unsafe_key)
            .unwrap_err()
            .contains("bidirectional"));
    }

    #[test]
    fn a_padded_value_key_is_rejected_rather_than_binding_nothing() {
        // Placeholder names are trimmed, so a padded key is a key that can
        // never match anything. Binding nothing is exactly how a value the
        // user typed disappears between the dialog and the prompt, so the two
        // sides of the lookup are held to the same spelling.
        let workflow = workflow("t", "kill -9 {{ pid }}", &[]);
        let error = render(&workflow, &values(&[("pid ", "4242")])).unwrap_err();
        assert!(
            error.contains("value name has leading or trailing whitespace"),
            "got {error}"
        );
        assert_eq!(
            render(&workflow, &values(&[("pid", "4242")])).unwrap(),
            "kill -9 4242"
        );
    }

    #[test]
    fn a_padded_declared_name_never_reaches_the_prompt_as_a_placeholder() {
        // The same rule seen from the form, which is where it was observable:
        // `missing()` reported the row outstanding, `render()` returned Ok
        // anyway, and a typed value changed nothing — the guard, the hint and
        // the value all failed together, silently.
        let padded = workflow("t", "kill -9 {{ pid }}", &[("pid ", None)]);
        let mut form = ArgsForm::new(padded);
        assert!(form
            .render()
            .unwrap_err()
            .contains("has leading or trailing whitespace"));
        form.set(0, "4242");
        assert!(form
            .render()
            .unwrap_err()
            .contains("has leading or trailing whitespace"));

        // Including the declared-default path, which dropped the default
        // rather than the typed value.
        let defaulted = workflow("t", "kill -9 {{pid}}", &[("pid ", Some("1234"))]);
        assert!(ArgsForm::new(defaulted)
            .render()
            .unwrap_err()
            .contains("has leading or trailing whitespace"));
    }

    #[test]
    fn render_revalidates_the_workflow_it_is_given() {
        // A workflow assembled in memory, or reached by a path that skipped
        // the loader, still crosses validation.
        let mut invalid = workflow("t", "echo ok", &[]);
        invalid.name = String::new();
        assert!(render(&invalid, &HashMap::new())
            .unwrap_err()
            .contains("empty name"));
    }

    #[test]
    fn rendering_is_bounded_against_binding_amplification() {
        let repeated = "{{value}}".repeat(4_000);
        let workflow = workflow("bounded", &repeated, &[("value", None)]);
        let huge = values(&[("value", &"x".repeat(MAX_WORKFLOW_COMMAND_BYTES))]);
        assert!(render(&workflow, &huge)
            .unwrap_err()
            .contains("rendered command exceeds"));

        // The same amplification through the lenient seam: `substitute`
        // validates nothing, but it still counts every byte it emits.
        let template = "{value}".repeat(128);
        assert!(substitute(&template, &[("value".into(), "x".repeat(1_024))]).is_err());
    }

    #[test]
    fn an_oversized_template_is_refused_before_it_is_walked() {
        let template = "x".repeat(MAX_WORKFLOW_COMMAND_BYTES + 1);
        assert!(substitute(&template, &[])
            .unwrap_err()
            .contains("command exceeds"));
    }

    #[test]
    fn an_args_form_seeds_declared_defaults_and_leaves_the_rest_unset() {
        let workflow = workflow(
            "deploy",
            "deploy {service} --env={{env}}",
            &[("service", Some("api")), ("env", None)],
        );
        let mut form = ArgsForm::new(workflow);
        assert_eq!(form.len(), 2);
        assert_eq!(form.value(0), "api");
        assert!(form.is_set(0));
        // Visibly empty in the UI, and *not supplied* underneath — the
        // distinction every app's dialog used to flatten.
        assert_eq!(form.value(1), "");
        assert!(!form.is_set(1));
        assert_eq!(form.missing(), ["env"]);
        assert!(form.render().unwrap_err().contains("missing values: env"));

        form.set(1, "staging");
        assert!(form.missing().is_empty());
        assert_eq!(form.render().unwrap(), "deploy api --env=staging");

        // An emptied field with a declared default stays an explicit empty
        // value: it does not fall back to the default.
        form.set(0, "");
        assert_eq!(form.render().unwrap(), "deploy  --env=staging");

        // Clearing is the way back to the declared default — and, for an
        // argument with none, the way back to unset.
        form.clear(0);
        assert_eq!(form.value(0), "api");
        form.clear(1);
        assert!(!form.is_set(1));
        assert_eq!(form.missing(), ["env"]);
    }

    #[test]
    fn an_args_form_emptied_by_the_user_is_still_a_missing_value() {
        // The UI path that produced `kill -9 `: the user tabs into the field,
        // types, deletes it again, and presses Insert.
        let workflow = workflow("t", "kill -9 {pid}", &[("pid", None)]);
        let mut form = ArgsForm::new(workflow);
        form.set(0, "421");
        assert_eq!(form.render().unwrap(), "kill -9 421");
        form.set(0, "");
        assert!(form.render().unwrap_err().contains("missing values: pid"));
    }

    #[test]
    fn an_args_form_ignores_a_stale_row_index() {
        // A widget callback can outlive the form it was built for; it must not
        // panic and must not silently write into a different argument.
        let mut form = ArgsForm::new(workflow("t", "echo {a}", &[("a", None)]));
        form.set(7, "ignored");
        form.clear(7);
        assert_eq!(form.value(7), "");
        assert_eq!(form.len(), 1);
    }

    #[test]
    fn an_args_form_for_a_zero_argument_workflow_renders_immediately() {
        let form = ArgsForm::new(workflow("t", "git status --porcelain", &[]));
        assert!(form.is_empty());
        assert!(form.missing().is_empty());
        assert_eq!(form.render().unwrap(), "git status --porcelain");
    }

    #[test]
    fn an_args_form_reports_unsafe_input_rather_than_inserting_it() {
        let mut form = ArgsForm::new(workflow("unsafe", "echo {value}", &[("value", None)]));
        form.set(0, "ok\nrm -rf /");
        assert!(form.render().unwrap_err().contains("unsafe"));
    }

    #[test]
    fn the_values_map_omits_unset_slots() {
        let mut form = ArgsForm::new(workflow(
            "t",
            "echo {a} {b}",
            &[("a", Some("x")), ("b", None)],
        ));
        assert_eq!(form.values(), values(&[("a", "x")]));
        form.set(1, "y");
        assert_eq!(form.values(), values(&[("a", "x"), ("b", "y")]));
    }
}
