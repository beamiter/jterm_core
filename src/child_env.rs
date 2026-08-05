//! What the family tells a shell about the terminal it is running in.
//!
//! Seeded from the four repos' spawn paths: the variable set and the
//! `overridden`/`has_less` shape come from frost (`src/pty.rs`, the fullest
//! builder in the family) and ember (`src/pty.rs`, which added the locale
//! normalization); the `BTreeMap`-over-`vars_os` merge and the NUL rejection
//! come from anvil (`src/pty.rs`) and forge (`src/pty.rs::child_environment`);
//! the two libvte `envv` call sites are anvil `src/terminal/vte.rs` and forge
//! `src/terminal.rs`; the Flatpak `--env=` form is [`crate::host::wrap_argv`],
//! which already owned an incomplete copy of this policy (`TERM` and nothing
//! else).
//!
//! The bug that motivated the module: anvil and forge insert **only** `TERM`,
//! and both default to *block* mode, where libvte never spawns the child —
//! anvil hands libvte a foreign PTY master, forge uses libvte display-only —
//! so the `TERM`/`COLORTERM`/`VTE_VERSION` libvte would otherwise inject never
//! reach the shell. In the family's two GTK terminals, in their default mode,
//! `bat`, `delta` and `lazygit` therefore fell back to 256 colours.
//!
//! Family decisions frozen here (do not relitigate per-app):
//!
//! - **`TERM` is `xterm-256color` and `COLORTERM` is `truecolor`,
//!   unconditionally.** All four repos already agreed on `TERM`; two of them
//!   forgot `COLORTERM`. Truecolor is not a capability any of the four
//!   frontends can fail to provide, so it is not a flag.
//! - **`VTE_VERSION` is emulated, not reported.** [`EMULATED_VTE_VERSION`] is
//!   the value ember/frost already hardcode. Distro `vte.sh` gates its OSC 7
//!   emitter on this variable, and that emitter is how a block-mode pane learns
//!   the child's cwd, so the number has to be high enough to pass the gate
//!   whatever libvte the machine has (or has not) installed.
//! - **`LESS`, `LS_COLORS`/`CLICOLOR` and locale normalization are flags, not
//!   defaults.** anvil/forge chose `LESS=R` with a written rationale (keep the
//!   pager interactive and on the alternate screen); ember/frost chose
//!   `LESS=FR`. That is a product disagreement, and unifying it under cover of
//!   a colour fix would silently change two apps' pagers. `less_default` names
//!   the value so both survive.
//! - **`COLUMNS` and `LINES` are deliberately excluded**, and this is the one
//!   entry a future contributor is most likely to want to "complete". The
//!   family's companion shell caches its width once, at construction, from
//!   `$COLUMNS` in preference to `stty size` (jsh `src/environment.rs`), never
//!   refreshes that field on `SIGWINCH`, and picks its prompt layout from it
//!   (jsh `src/prompt.rs`). Exporting `COLUMNS` would freeze a jsh prompt at
//!   the PTY's initial size for the life of the shell. The kernel already tells
//!   the child its window size through the PTY; nothing here needs to.
//!
//! # Inherited-environment boundary
//!
//! Frontends must call [`capture_inherited_environment`] as the first operation
//! in `main`, before CLI parsing writes `JTERM*` variables, before input-method
//! setup changes `GTK_PATH`/`GTK_IM_MODULE`/`XMODIFIERS`, and before GTK, GLib,
//! logging, or any worker thread starts. Direct PTY spawns then use
//! [`envp_from_captured`]. A VTE spawn must use [`vte_envv_from_captured`] as a
//! complete environment together with VTE's `VTE_SPAWN_NO_PARENT_ENVV` flag;
//! the ordinary [`vte_envv`] remains an identity-only compatibility adapter and
//! cannot remove variables from VTE's live parent environment.
//!
//! The default snapshot preserves every launch-time variable, including
//! `JSH_*`, `JTERM*`, and the user's GTK input-method settings. Those
//! namespaces contain legitimate shell/provider and nested-terminal
//! configuration, so a blanket namespace scrub would silently change the
//! child. An embedding frontend that owns a process-only capability or test
//! variable can instead name it explicitly with
//! [`capture_inherited_environment_with_scrub`]. Freezing before frontend
//! setup still keeps later CLI/toolkit mutations out without guessing which
//! launch-time configuration belongs to the child.

use std::collections::BTreeMap;
use std::ffi::{CString, OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::sync::OnceLock;

/// Terminal type every jterm claims. All four repos already sent this.
pub const TERM: &str = "xterm-256color";

/// 24-bit colour advertisement. The variable anvil/forge were missing.
pub const COLORTERM: &str = "truecolor";

/// The `VTE_VERSION` the family reports regardless of the libvte it links (or,
/// for ember/frost, does not link). Matches the constant ember and frost
/// already hardcode, so a distro `vte.sh` still installs its OSC 7 cwd emitter.
pub const EMULATED_VTE_VERSION: &str = "7802";

/// Locale used when [`ChildEnv::normalize_locale`] has to repair a non-UTF-8
/// environment. `C.UTF-8` is the choice ember made: available without any
/// locale being generated, and unambiguously UTF-8.
pub const UTF8_LOCALE: &str = "C.UTF-8";

/// `LS_COLORS` used when [`ChildEnv::color_defaults`] is on. Vendored from
/// frost, which is the only repo that ever set it.
pub const DEFAULT_LS_COLORS: &str = concat!(
    "rs=0:di=01;34:ln=01;36:mh=00:pi=40;33:so=01;35:",
    "do=01;35:bd=40;33;01:cd=40;33;01:or=40;31;01:",
    "mi=00:su=37;41:sg=30;43:ca=30;41:tw=30;42:",
    "ow=34;42:st=37;44:ex=01;32"
);

/// The parent environment as this module reads it. Sorted so a child's
/// environment block is byte-identical across runs, which is what makes the
/// spawn path diffable when a shell misbehaves.
type Environment = BTreeMap<OsString, OsString>;

static CAPTURED_ENVIRONMENT: OnceLock<Environment> = OnceLock::new();

/// Numeric value of VTE's `VTE_SPAWN_NO_PARENT_ENVV` flag (available since
/// VTE 0.60). GTK frontends combine this with their ordinary GLib spawn flags
/// when passing [`vte_envv_from_captured`], so VTE does not merge the live,
/// toolkit-mutated process environment back into the frozen block.
pub const VTE_SPAWN_NO_PARENT_ENVV_BITS: u32 = 1 << 25;

/// Freeze the launch environment exactly as the terminal inherited it.
///
/// Call exactly once, as the first operation in an executable's `main`. A
/// second call returns `AlreadyExists`; treating that as an initialization
/// error catches ordering mistakes instead of silently accepting a later,
/// already-mutated snapshot.
pub fn capture_inherited_environment() -> io::Result<()> {
    capture_inherited_environment_with_scrub(&[], &[])
}

/// [`capture_inherited_environment`] with exact names and prefixes owned only
/// by one embedding application's process.
///
/// Prefer exact names. Prefixes are available for genuinely private generated
/// capability families, but broad prefixes such as `JSH_` or `FORGE_` also
/// match legitimate user configuration and must not be used.
pub fn capture_inherited_environment_with_scrub(
    extra_names: &[&str],
    extra_prefixes: &[&str],
) -> io::Result<()> {
    validate_scrub_rules(extra_names, extra_prefixes)?;
    let environment = scrub_environment(std::env::vars_os().collect(), extra_names, extra_prefixes);
    CAPTURED_ENVIRONMENT.set(environment).map_err(|_| {
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            "the inherited child environment was already captured",
        )
    })
}

pub fn inherited_environment_is_captured() -> bool {
    CAPTURED_ENVIRONMENT.get().is_some()
}

/// Per-app spawn policy. The terminal identity is not configurable; everything
/// the four repos disagreed about is.
#[derive(Clone, Copy, Debug)]
pub struct ChildEnv<'a> {
    /// Value for `TERM_PROGRAM_VERSION`. The app's own `CARGO_PKG_VERSION`;
    /// [`ChildEnv::from_identity`] takes it from [`crate::identity`].
    pub app_version: &'a str,
    /// Value for `VTE_VERSION`, normally [`EMULATED_VTE_VERSION`]. `None` still
    /// *removes* an inherited `VTE_VERSION`; see [`overridden_names`].
    pub vte_version: Option<&'a str>,
    /// Value for `LESS` when the parent has none: `"R"` for anvil/forge,
    /// `"FR"` for ember/frost, `None` to leave the pager alone entirely.
    pub less_default: Option<&'a str>,
    /// Set `LS_COLORS` and `CLICOLOR` when the parent has neither. Only frost
    /// does this today.
    pub color_defaults: bool,
    /// Replace a non-UTF-8 locale with [`UTF8_LOCALE`]. Only ember does this
    /// today. A terminal that draws UTF-8 either way (anvil, frost, forge)
    /// should leave it off rather than overriding a deliberate `LANG`.
    pub normalize_locale: bool,
}

impl ChildEnv<'static> {
    /// Terminal identity only, versioned from [`crate::identity`]: no pager,
    /// colour or locale opinion. This is what a call site that only has to
    /// answer "which terminal is this?" wants — notably the Flatpak bridge.
    pub fn from_identity() -> Self {
        Self {
            app_version: crate::identity::get().app_version,
            vte_version: Some(EMULATED_VTE_VERSION),
            less_default: None,
            color_defaults: false,
            normalize_locale: false,
        }
    }
}

/// Variables this module takes ownership of.
///
/// Every name here is removed from the inherited environment by [`envp`], and
/// all of them except `VTE_VERSION` are then set unconditionally. Removal
/// matters even where the value is then rewritten: a jterm launched *from*
/// another terminal inherits that terminal's `TERM_PROGRAM_VERSION` and
/// `VTE_VERSION`, and a shell that reads the outer terminal's identity will
/// enable features this one does not have.
pub fn overridden_names() -> &'static [&'static str] {
    &[
        "TERM",
        "COLORTERM",
        "TERM_PROGRAM",
        "TERM_PROGRAM_VERSION",
        "VTE_VERSION",
    ]
}

/// Every variable the policy sets, in deterministic order: terminal identity
/// first (in [`overridden_names`] order), then the conditional defaults, then
/// `extra`.
///
/// This is the *overlay*, not a whole environment: a caller merges it over the
/// variables the child should inherit. Assigning a name twice is allowed and
/// the later assignment wins — including at the later position — so `extra`
/// beats both the identity block and the defaults, and a caller that repeats a
/// name inside `extra` cannot smuggle two values for it into an `execve` block.
pub fn pairs(options: &ChildEnv<'_>, extra: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
    pairs_for(&inherited_environment(), options, extra)
}

/// Strict [`pairs`] variant backed only by the early frozen environment.
/// Returns an initialization error instead of sampling a process environment
/// that GTK or CLI setup may already have changed.
pub fn pairs_from_captured(
    options: &ChildEnv<'_>,
    extra: &[(&str, &str)],
) -> io::Result<Vec<(OsString, OsString)>> {
    Ok(pairs_for(captured_environment()?, options, extra))
}

fn pairs_for(
    inherited: &Environment,
    options: &ChildEnv<'_>,
    extra: &[(&str, &str)],
) -> Vec<(OsString, OsString)> {
    policy_vars(inherited, options, extra)
        .into_iter()
        .map(|(name, value)| (OsString::from(name), OsString::from(value)))
        .collect()
}

/// The child's complete environment as a NUL-terminated `KEY=VALUE` block for
/// `execve`: the inherited environment with [`overridden_names`] and anything
/// [`pairs`] sets removed, then the policy appended.
///
/// **Every allocation here happens in the parent, before `fork`.** That is the
/// point of returning a ready block instead of having the child call `setenv`:
/// between `fork` and `exec` the child may only call async-signal-safe
/// functions, and `setenv`/`malloc` can deadlock on a lock another thread held
/// at the moment of the fork. Build this, keep the `CString`s alive, and let
/// the child do nothing but collect pointers and `execve`.
pub fn envp(options: &ChildEnv<'_>, extra: &[(&str, &str)]) -> io::Result<Vec<CString>> {
    envp_for(&inherited_environment(), options, extra)
}

/// Strict [`envp`] variant backed by [`capture_inherited_environment`].
pub fn envp_from_captured(
    options: &ChildEnv<'_>,
    extra: &[(&str, &str)],
) -> io::Result<Vec<CString>> {
    envp_for(captured_environment()?, options, extra)
}

/// Terminal identity as `KEY=VALUE` strings for libvte's `spawn_async` `envv`.
///
/// Identity only, by design. libvte *adds* `envv` to an environment it also
/// inherits and injects its own `TERM`/`VTE_VERSION` into, and an `envv` entry
/// wins unconditionally — so the conditional defaults cannot be expressed here
/// without clobbering a `LESS` or a `LANG` the user set on purpose. A
/// VTE-spawned child keeps the pager and locale it has today and gains only a
/// consistent terminal identity.
pub fn vte_envv(options: &ChildEnv<'_>, extra: &[(&str, &str)]) -> Vec<String> {
    identity_with_extra(options, extra)
        .into_iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect()
}

/// A complete UTF-8 environment block for VTE, built from the early frozen
/// snapshot rather than VTE's live GTK process environment.
///
/// The caller **must** combine this with
/// [`VTE_SPAWN_NO_PARENT_ENVV_BITS`]. Without that VTE adds the live parent
/// environment again, reintroducing exactly the frontend-private variables
/// this block removed. The gtk-rs VTE API accepts `&str`, so a non-UTF-8
/// inherited name or value is rejected rather than silently changed.
pub fn vte_envv_from_captured(
    options: &ChildEnv<'_>,
    extra: &[(&str, &str)],
) -> io::Result<Vec<String>> {
    vte_envv_for(captured_environment()?, options, extra)
}

fn vte_envv_for(
    inherited: &Environment,
    options: &ChildEnv<'_>,
    extra: &[(&str, &str)],
) -> io::Result<Vec<String>> {
    envp_for(inherited, options, extra)?
        .into_iter()
        .map(|entry| {
            String::from_utf8(entry.into_bytes()).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "the captured child environment contains non-UTF-8 data unsupported by VTE",
                )
            })
        })
        .collect()
}

/// Terminal identity as `--env=KEY=VALUE` arguments for `flatpak-spawn --host`.
///
/// Identity only, for a second reason on top of [`vte_envv`]'s: the host child
/// starts from the *host session's* environment, not ours, so the "only if the
/// user has not set it" checks below would be asking the wrong environment.
/// Defaulting `LESS` because the sandbox has none would override the value the
/// host user actually configured.
pub fn host_bridge_args(options: &ChildEnv<'_>, extra: &[(&str, &str)]) -> Vec<String> {
    identity_with_extra(options, extra)
        .into_iter()
        .map(|(name, value)| format!("--env={name}={value}"))
        .collect()
}

fn captured_environment() -> io::Result<&'static Environment> {
    CAPTURED_ENVIRONMENT.get().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "capture_inherited_environment must run before frontend initialization",
        )
    })
}

fn inherited_environment() -> Environment {
    // Compatibility path for embedders that have not adopted the strict API
    // yet. Only an explicit early capture can keep later GTK/CLI mutations out;
    // silently deleting launch-time configuration here would be worse.
    CAPTURED_ENVIRONMENT
        .get()
        .cloned()
        .unwrap_or_else(|| std::env::vars_os().collect())
}

fn validate_scrub_rules(exact_names: &[&str], prefixes: &[&str]) -> io::Result<()> {
    if exact_names
        .iter()
        .chain(prefixes)
        .any(|rule| !valid_scrub_rule(rule))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "child-environment scrub names and prefixes must be nonempty variable-name text",
        ));
    }
    Ok(())
}

fn valid_scrub_rule(rule: &str) -> bool {
    !rule.is_empty()
        && !rule.contains(['=', '\0'])
        && !rule.chars().any(char::is_control)
        && !crate::review_input::contains_visual_spoofing(rule)
}

fn scrub_environment(
    mut environment: Environment,
    exact_names: &[&str],
    extra_prefixes: &[&str],
) -> Environment {
    environment.retain(|name, _| {
        if exact_names
            .iter()
            .any(|private| name.as_os_str() == OsStr::new(private))
        {
            return false;
        }
        let Some(name) = name.to_str() else {
            return true;
        };
        !extra_prefixes.iter().any(|prefix| name.starts_with(prefix))
    });
    environment
}

/// Assign `name`, replacing any earlier assignment and taking its position.
fn set_var(vars: &mut Vec<(String, String)>, name: &str, value: &str) {
    if name.is_empty() || name.contains('=') {
        // Both consumers of this list parse `KEY=VALUE` positionally at the
        // first `=`, so `=v` and `a=b=v` do not fail — they define a
        // *different* variable in the child, or in flatpak-spawn's case get
        // rejected as an unknown option far from the code that built it. Drop
        // it here, where it can still be named in a log line.
        // Do not reflect the invalid name: callers can supply this API from a
        // config boundary, and control-bearing diagnostic text would become a
        // second log-injection surface.
        log::warn!("ignoring child environment variable with an invalid name");
        return;
    }
    vars.retain(|(existing, _)| existing != name);
    vars.push((name.to_string(), value.to_string()));
}

/// The five [`overridden_names`], minus `VTE_VERSION` when it is `None`.
fn identity_vars(options: &ChildEnv<'_>) -> Vec<(String, String)> {
    let mut vars = Vec::with_capacity(5);
    set_var(&mut vars, "TERM", TERM);
    set_var(&mut vars, "COLORTERM", COLORTERM);
    set_var(&mut vars, "TERM_PROGRAM", crate::identity::get().app_name);
    // Set even when the app passed an empty version. Omitting it would leave
    // the inherited `TERM_PROGRAM_VERSION` of whatever terminal launched this
    // one paired with *our* `TERM_PROGRAM`, which is a worse lie than blank.
    set_var(&mut vars, "TERM_PROGRAM_VERSION", options.app_version);
    if let Some(version) = options.vte_version {
        set_var(&mut vars, "VTE_VERSION", version);
    }
    vars
}

fn identity_with_extra(options: &ChildEnv<'_>, extra: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut vars = identity_vars(options);
    for (name, value) in extra {
        set_var(&mut vars, name, value);
    }
    // These two adapters feed APIs that eventually require C strings but do
    // not return `Result`. Drop impossible OS environment entries here;
    // `envp` deliberately keeps them until `CString::new` so its fallible API
    // continues to report invalid input to the caller.
    vars.retain(|(name, value)| !name.contains('\0') && !value.contains('\0'));
    vars
}

fn policy_vars(
    inherited: &Environment,
    options: &ChildEnv<'_>,
    extra: &[(&str, &str)],
) -> Vec<(String, String)> {
    let mut vars = identity_vars(options);

    // Presence, not a non-empty value: `LESS=` is how a user asks for no pager
    // options at all, and all four repos already treated it as the user's call.
    if let Some(less) = options.less_default {
        if !inherited.contains_key(OsStr::new("LESS")) {
            set_var(&mut vars, "LESS", less);
        }
    }
    if options.color_defaults {
        // GNU `ls --color` reads LS_COLORS for file-type and permission
        // classes; BSD/macOS `ls -G` keys off CLICOLOR instead.
        if !inherited.contains_key(OsStr::new("LS_COLORS")) {
            set_var(&mut vars, "LS_COLORS", DEFAULT_LS_COLORS);
        }
        if !inherited.contains_key(OsStr::new("CLICOLOR")) {
            set_var(&mut vars, "CLICOLOR", "1");
        }
    }
    if options.normalize_locale && !effective_ctype(inherited).is_some_and(is_utf8_locale) {
        set_var(&mut vars, "LANG", UTF8_LOCALE);
        set_var(&mut vars, "LC_CTYPE", UTF8_LOCALE);
        // Rewriting LANG and LC_CTYPE is not enough while LC_ALL is set: it
        // outranks both, so `LC_ALL=C` would keep the child in the C charset
        // and the normalization would be invisible. Rewrite it in place rather
        // than unsetting it, because this overlay is also consumed as a list of
        // assignments (`pairs`, `--env=`) where "remove this name" has no
        // spelling, and a silently-skipped removal is how the fix would rot.
        if non_empty(inherited, "LC_ALL").is_some() {
            set_var(&mut vars, "LC_ALL", UTF8_LOCALE);
        }
    }

    for (name, value) in extra {
        set_var(&mut vars, name, value);
    }
    vars
}

fn non_empty<'a>(inherited: &'a Environment, name: &str) -> Option<&'a OsStr> {
    inherited
        .get(OsStr::new(name))
        .map(OsString::as_os_str)
        .filter(|value| !value.is_empty())
}

/// The variable that actually decides the child's character encoding, by POSIX
/// precedence: `LC_ALL`, then `LC_CTYPE`, then `LANG`, each only when set to a
/// non-empty value.
///
/// ember used `.any()` over the three names instead, which reports UTF-8 as
/// long as *some* locale variable mentions it. `LC_ALL=C` together with
/// `LANG=en_US.UTF-8` — a common shape on a machine that pins the C locale for
/// tooling — therefore skipped normalization and forwarded `LC_ALL=C` verbatim,
/// so the child rendered mojibake in exactly the case the code existed to fix.
fn effective_ctype(inherited: &Environment) -> Option<&OsStr> {
    ["LC_ALL", "LC_CTYPE", "LANG"]
        .into_iter()
        .find_map(|name| non_empty(inherited, name))
}

fn is_utf8_locale(value: &OsStr) -> bool {
    let value = value.to_string_lossy().to_ascii_lowercase();
    value.contains("utf-8") || value.contains("utf8")
}

fn envp_for(
    inherited: &Environment,
    options: &ChildEnv<'_>,
    extra: &[(&str, &str)],
) -> io::Result<Vec<CString>> {
    let policy = policy_vars(inherited, options, extra);
    let mut block = Vec::with_capacity(inherited.len() + policy.len());
    for (name, value) in inherited {
        let owned = overridden_names()
            .iter()
            .any(|owned| OsStr::new(owned) == name.as_os_str())
            || policy
                .iter()
                .any(|(set, _)| OsStr::new(set.as_str()) == name.as_os_str());
        if owned {
            continue;
        }
        block.push(entry(name.as_os_str(), value.as_os_str())?);
    }
    for (name, value) in &policy {
        block.push(entry(
            OsStr::new(name.as_str()),
            OsStr::new(value.as_str()),
        )?);
    }
    Ok(block)
}

fn entry(name: &OsStr, value: &OsStr) -> io::Result<CString> {
    let name = name.as_bytes();
    let value = value.as_bytes();
    let mut bytes = Vec::with_capacity(name.len() + value.len() + 1);
    bytes.extend_from_slice(name);
    bytes.push(b'=');
    bytes.extend_from_slice(value);
    CString::new(bytes).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "child environment variable '{}' contains an embedded NUL byte",
                String::from_utf8_lossy(name)
            ),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests never call `identity::init`, so `TERM_PROGRAM` is the neutral
    /// "jterm" identity throughout.
    const APP: &str = "jterm";

    fn options() -> ChildEnv<'static> {
        ChildEnv {
            app_version: "9.9.9",
            vte_version: Some(EMULATED_VTE_VERSION),
            less_default: None,
            color_defaults: false,
            normalize_locale: false,
        }
    }

    fn env(pairs: &[(&str, &str)]) -> Environment {
        pairs
            .iter()
            .map(|(name, value)| (OsString::from(name), OsString::from(value)))
            .collect()
    }

    fn names(vars: &[(String, String)]) -> Vec<&str> {
        vars.iter().map(|(name, _)| name.as_str()).collect()
    }

    fn value<'a>(vars: &'a [(String, String)], name: &str) -> Option<&'a str> {
        vars.iter()
            .find(|(existing, _)| existing == name)
            .map(|(_, value)| value.as_str())
    }

    fn block_value(block: &[CString], name: &str) -> Option<String> {
        let prefix = format!("{name}=");
        block.iter().find_map(|entry| {
            entry
                .to_str()
                .ok()?
                .strip_prefix(&prefix)
                .map(str::to_string)
        })
    }

    #[test]
    fn identity_comes_first_in_a_fixed_order() {
        let vars = policy_vars(&env(&[]), &options(), &[]);
        assert_eq!(
            names(&vars),
            [
                "TERM",
                "COLORTERM",
                "TERM_PROGRAM",
                "TERM_PROGRAM_VERSION",
                "VTE_VERSION"
            ]
        );
        assert_eq!(value(&vars, "TERM"), Some(TERM));
        assert_eq!(value(&vars, "COLORTERM"), Some(COLORTERM));
        assert_eq!(value(&vars, "TERM_PROGRAM"), Some(APP));
        assert_eq!(value(&vars, "TERM_PROGRAM_VERSION"), Some("9.9.9"));
        assert_eq!(value(&vars, "VTE_VERSION"), Some(EMULATED_VTE_VERSION));
    }

    #[test]
    fn overridden_names_are_exactly_the_always_set_identity_block() {
        let bare = ChildEnv {
            vte_version: None,
            ..options()
        };
        let vars = policy_vars(&env(&[]), &bare, &[]);
        // VTE_VERSION is the one owned name that is conditional.
        assert_eq!(names(&vars), &overridden_names()[..4]);
        let full = policy_vars(&env(&[]), &options(), &[]);
        assert_eq!(names(&full), overridden_names());
    }

    #[test]
    fn extra_wins_over_identity_and_defaults() {
        let with_pager = ChildEnv {
            less_default: Some("R"),
            ..options()
        };
        let vars = policy_vars(
            &env(&[]),
            &with_pager,
            &[("TERM", "dumb"), ("LESS", "FRX"), ("JSH_SESSION_ID", "7")],
        );
        assert_eq!(value(&vars, "TERM"), Some("dumb"));
        assert_eq!(value(&vars, "LESS"), Some("FRX"));
        assert_eq!(value(&vars, "JSH_SESSION_ID"), Some("7"));
        // A replaced name is assigned once, at the position of the winner, so
        // the execve block cannot carry two values for it.
        assert_eq!(
            names(&vars),
            [
                "COLORTERM",
                "TERM_PROGRAM",
                "TERM_PROGRAM_VERSION",
                "VTE_VERSION",
                "TERM",
                "LESS",
                "JSH_SESSION_ID",
            ]
        );
    }

    #[test]
    fn a_repeated_extra_name_collapses_to_the_last_value() {
        let vars = policy_vars(&env(&[]), &options(), &[("A", "1"), ("A", "2")]);
        assert_eq!(value(&vars, "A"), Some("2"));
        assert_eq!(vars.iter().filter(|(name, _)| name == "A").count(), 1);
    }

    #[test]
    fn an_invalid_extra_name_is_dropped_rather_than_defining_another_variable() {
        let vars = policy_vars(&env(&[]), &options(), &[("", "x"), ("A=B", "y")]);
        assert_eq!(names(&vars), overridden_names());
    }

    #[test]
    fn pager_and_color_defaults_are_opt_in_and_yield_to_the_parent() {
        assert_eq!(
            value(&policy_vars(&env(&[]), &options(), &[]), "LESS"),
            None
        );

        let anvil = ChildEnv {
            less_default: Some("R"),
            ..options()
        };
        let frost = ChildEnv {
            less_default: Some("FR"),
            color_defaults: true,
            ..options()
        };
        assert_eq!(
            value(&policy_vars(&env(&[]), &anvil, &[]), "LESS"),
            Some("R")
        );
        assert_eq!(
            value(&policy_vars(&env(&[]), &frost, &[]), "LESS"),
            Some("FR")
        );
        assert_eq!(
            value(&policy_vars(&env(&[]), &frost, &[]), "LS_COLORS"),
            Some(DEFAULT_LS_COLORS)
        );
        assert_eq!(
            value(&policy_vars(&env(&[]), &frost, &[]), "CLICOLOR"),
            Some("1")
        );

        // An explicit empty LESS is still the user's choice, not an absence.
        let configured = env(&[("LESS", ""), ("LS_COLORS", "di=0"), ("CLICOLOR", "0")]);
        let vars = policy_vars(&configured, &frost, &[]);
        assert_eq!(value(&vars, "LESS"), None);
        assert_eq!(value(&vars, "LS_COLORS"), None);
        assert_eq!(value(&vars, "CLICOLOR"), None);
    }

    #[test]
    fn locale_precedence_is_lc_all_then_lc_ctype_then_lang() {
        let normalizing = ChildEnv {
            normalize_locale: true,
            ..options()
        };
        let normalized = |pairs: &[(&str, &str)]| {
            let vars = policy_vars(&env(pairs), &normalizing, &[]);
            value(&vars, "LANG").map(str::to_string)
        };

        // The ember bug: LC_ALL=C outranks a UTF-8 LANG, so this must
        // normalize. ember's `.any()` saw the UTF-8 LANG and forwarded
        // LC_ALL=C untouched.
        assert_eq!(
            normalized(&[("LC_ALL", "C"), ("LANG", "en_US.UTF-8")]),
            Some(UTF8_LOCALE.to_string())
        );
        // LC_CTYPE outranks LANG the same way.
        assert_eq!(
            normalized(&[("LC_CTYPE", "C"), ("LANG", "en_US.UTF-8")]),
            Some(UTF8_LOCALE.to_string())
        );
        // LC_ALL outranks a non-UTF-8 LC_CTYPE in the other direction too.
        assert_eq!(
            normalized(&[("LC_ALL", "en_US.UTF-8"), ("LC_CTYPE", "C")]),
            None
        );
        // An empty value is not set, per POSIX, so precedence falls through.
        assert_eq!(normalized(&[("LC_ALL", ""), ("LANG", "en_US.UTF-8")]), None);
        // Nothing set at all is not UTF-8.
        assert_eq!(normalized(&[]), Some(UTF8_LOCALE.to_string()));
        // Both spellings count as UTF-8, case-insensitively.
        assert_eq!(normalized(&[("LANG", "en_US.utf8")]), None);
        assert_eq!(normalized(&[("LANG", "C.UTF-8")]), None);

        // Off by default: a non-UTF-8 locale is left exactly as the user set it.
        let vars = policy_vars(&env(&[("LC_ALL", "C")]), &options(), &[]);
        assert_eq!(value(&vars, "LANG"), None);
        assert_eq!(value(&vars, "LC_ALL"), None);
    }

    #[test]
    fn normalization_rewrites_lc_all_only_when_the_parent_had_one() {
        let normalizing = ChildEnv {
            normalize_locale: true,
            ..options()
        };
        // Left alone, LC_ALL=C would outrank the LC_CTYPE we just set and the
        // child would still be in the C charset.
        let vars = policy_vars(&env(&[("LC_ALL", "C")]), &normalizing, &[]);
        assert_eq!(value(&vars, "LC_ALL"), Some(UTF8_LOCALE));
        assert_eq!(value(&vars, "LC_CTYPE"), Some(UTF8_LOCALE));

        // With no LC_ALL there is nothing to outrank, and inventing one would
        // flatten LC_TIME/LC_NUMERIC the user chose deliberately.
        let vars = policy_vars(
            &env(&[("LANG", "C"), ("LC_TIME", "de_DE.UTF-8")]),
            &normalizing,
            &[],
        );
        assert_eq!(value(&vars, "LC_ALL"), None);
    }

    #[test]
    fn envp_merges_the_inherited_environment_and_drops_owned_names() {
        let inherited = env(&[
            ("PATH", "/usr/bin"),
            ("HOME", "/home/alice"),
            ("TERM", "vt100"),
            ("TERM_PROGRAM", "Apple_Terminal"),
            ("TERM_PROGRAM_VERSION", "455"),
        ]);
        let block = envp_for(&inherited, &options(), &[("JSH_SESSION_ID", "7")]).unwrap();
        let entries: Vec<&str> = block.iter().map(|entry| entry.to_str().unwrap()).collect();
        assert_eq!(
            entries,
            [
                "HOME=/home/alice",
                "PATH=/usr/bin",
                "TERM=xterm-256color",
                "COLORTERM=truecolor",
                "TERM_PROGRAM=jterm",
                "TERM_PROGRAM_VERSION=9.9.9",
                "VTE_VERSION=7802",
                "JSH_SESSION_ID=7",
            ]
        );
        // One entry per name: a duplicate would leave the child's getenv
        // answering with whichever copy libc reached first.
        assert_eq!(entries.iter().filter(|e| e.starts_with("TERM=")).count(), 1);
    }

    #[test]
    fn an_inherited_vte_version_is_removed_even_when_none_is_reported() {
        let inherited = env(&[("VTE_VERSION", "6003"), ("PATH", "/usr/bin")]);
        let bare = ChildEnv {
            vte_version: None,
            ..options()
        };
        let block = envp_for(&inherited, &bare, &[]).unwrap();
        assert_eq!(block_value(&block, "VTE_VERSION"), None);
        assert_eq!(block_value(&block, "PATH").as_deref(), Some("/usr/bin"));
    }

    #[test]
    fn envp_rejects_a_nul_bearing_value_instead_of_truncating_it() {
        let error = envp_for(&env(&[]), &options(), &[("A", "b\0c")]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains('A'));
    }

    #[test]
    fn vte_envv_carries_identity_only() {
        let full = ChildEnv {
            less_default: Some("FR"),
            color_defaults: true,
            normalize_locale: true,
            ..options()
        };
        let envv = vte_envv(&full, &[("JTERM_CWD_TOKEN", "abc")]);
        assert_eq!(
            envv,
            [
                "TERM=xterm-256color",
                "COLORTERM=truecolor",
                "TERM_PROGRAM=jterm",
                "TERM_PROGRAM_VERSION=9.9.9",
                "VTE_VERSION=7802",
                "JTERM_CWD_TOKEN=abc",
            ]
        );
    }

    #[test]
    fn infallible_environment_adapters_drop_nul_entries() {
        assert!(vte_envv(&options(), &[("BAD", "a\0b")])
            .iter()
            .all(|entry| !entry.starts_with("BAD=")));
        assert!(host_bridge_args(&options(), &[("BAD\0NAME", "value")])
            .iter()
            .all(|entry| !entry.contains("BAD")));
    }

    #[test]
    fn host_bridge_args_carry_colorterm_across_the_sandbox_boundary() {
        let args = host_bridge_args(&options(), &[("LESS", "R")]);
        assert!(args.contains(&"--env=COLORTERM=truecolor".to_string()));
        assert!(args.contains(&"--env=TERM=xterm-256color".to_string()));
        assert!(args.contains(&"--env=LESS=R".to_string()));
        assert!(args.iter().all(|arg| arg.starts_with("--env=")));
    }

    #[test]
    fn from_identity_is_identity_only() {
        let options = ChildEnv::from_identity();
        assert_eq!(options.less_default, None);
        assert!(!options.color_defaults);
        assert!(!options.normalize_locale);
        assert_eq!(options.vte_version, Some(EMULATED_VTE_VERSION));
        assert_eq!(
            names(&policy_vars(&env(&[("LESS", "")]), &options, &[])),
            overridden_names()
        );
    }

    #[test]
    fn explicit_scrub_preserves_launch_configuration_outside_the_named_capabilities() {
        let frozen = scrub_environment(
            env(&[
                ("PATH", "/usr/bin"),
                ("GTK_PATH", "/user/gtk"),
                ("GTK_IM_MODULE", "ibus"),
                ("XMODIFIERS", "@im=ibus"),
                ("ANVIL_CONFIG", "/user/config"),
                ("FORGE_AI_API_KEY", "user-provider-key"),
                ("JSH_AI_PROVIDER", "openai"),
                ("JSH_SESSION_ID", "stale-pane"),
                ("EMBEDDER_TOKEN", "private"),
                ("EMBEDDER_DEBUG_LEVEL", "2"),
            ]),
            &["EMBEDDER_TOKEN"],
            &["EMBEDDER_DEBUG_"],
        );

        for private in ["EMBEDDER_TOKEN", "EMBEDDER_DEBUG_LEVEL"] {
            assert!(!frozen.contains_key(OsStr::new(private)), "{private}");
        }
        for preserved in [
            "ANVIL_CONFIG",
            "FORGE_AI_API_KEY",
            "JSH_AI_PROVIDER",
            "JSH_SESSION_ID",
        ] {
            assert!(frozen.contains_key(OsStr::new(preserved)), "{preserved}");
        }
        assert_eq!(
            frozen.get(OsStr::new("GTK_PATH")).map(OsString::as_os_str),
            Some(OsStr::new("/user/gtk"))
        );
        assert_eq!(
            frozen
                .get(OsStr::new("GTK_IM_MODULE"))
                .map(OsString::as_os_str),
            Some(OsStr::new("ibus"))
        );
        assert_eq!(
            frozen
                .get(OsStr::new("XMODIFIERS"))
                .map(OsString::as_os_str),
            Some(OsStr::new("@im=ibus"))
        );
    }

    #[test]
    fn full_vte_block_uses_only_frozen_values_and_current_per_pane_extras() {
        let frozen = env(&[
            ("PATH", "/usr/bin"),
            ("GTK_PATH", "/launch-time/gtk"),
            ("FORGE_CONFIG", "/user/forge.toml"),
            ("JSH_AI_PROVIDER", "openai"),
            ("JSH_SESSION_ID", "stale"),
        ]);
        // This represents a process-only value written after the early
        // snapshot. It is absent from `frozen`, so a complete VTE block cannot
        // leak it even if the live frontend environment now contains it.
        let mut live_after_capture = frozen.clone();
        live_after_capture.insert(OsString::from("FORGE_SAFE_MODE"), OsString::from("1"));
        assert!(live_after_capture.contains_key(OsStr::new("FORGE_SAFE_MODE")));

        let envv =
            vte_envv_for(&frozen, &options(), &[("JSH_SESSION_ID", "current-pane")]).unwrap();

        assert!(envv.contains(&"PATH=/usr/bin".to_string()));
        assert!(envv.contains(&"GTK_PATH=/launch-time/gtk".to_string()));
        assert!(envv.contains(&"FORGE_CONFIG=/user/forge.toml".to_string()));
        assert!(envv.contains(&"JSH_AI_PROVIDER=openai".to_string()));
        assert!(envv.contains(&"JSH_SESSION_ID=current-pane".to_string()));
        assert!(envv.iter().all(|entry| entry != "JSH_SESSION_ID=stale"));
        assert!(envv
            .iter()
            .all(|entry| !entry.starts_with("FORGE_SAFE_MODE=")));
        assert_eq!(VTE_SPAWN_NO_PARENT_ENVV_BITS, 33_554_432);
    }

    #[test]
    fn scrub_rules_reject_ambiguous_or_control_bearing_names() {
        for rule in ["", "A=B", "BAD\nNAME", "SAFE\u{202e}NAME"] {
            assert!(validate_scrub_rules(&[rule], &[]).is_err(), "{rule:?}");
            assert!(validate_scrub_rules(&[], &[rule]).is_err(), "{rule:?}");
        }
        assert!(validate_scrub_rules(&["EMBEDDER_TOKEN"], &["EMBEDDER_"]).is_ok());
    }
}
