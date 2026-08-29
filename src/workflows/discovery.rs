//! Where a workflow library lives: the one impure seam in this module.
//!
//! Everything else here is a pure function of bytes. Discovery has to ask the
//! machine, and *which* machine question to ask is exactly what the four apps
//! legitimately disagree about, so it is injected rather than decided here.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::MAX_WORKFLOW_DIRECTORIES;

/// The XDG lookups [`search_path`] needs, so the backend stays the caller's
/// choice.
///
/// anvil and forge ask `gtk::glib` (`user_config_dir`, `user_data_dir`,
/// `system_data_dirs`); ember and frost ask the `dirs` crate plus a raw
/// `XDG_DATA_DIRS` read. Those agree on a normal Linux desktop and differ at
/// the edges: glib's lookups never fail, `dirs::config_dir()` returns `None`
/// with `HOME` unset. Hardcoding either backend would silently change which
/// directories two of the four apps read, with nothing in the diff to explain
/// it, so both stay expressible.
///
/// # Contract
///
/// Every returned path must be absolute. Returning a relative one is how forge
/// came to scan `./.config/forge/workflows` when `HOME` was unset, so
/// [`search_path`] drops non-absolute paths from every method here rather than
/// trusting the implementation — a sloppy impl loses a tier instead of
/// resolving it against whatever directory the process was started in.
pub trait DirSources {
    /// `$XDG_CONFIG_HOME`, or `None` when no home directory can be determined.
    fn user_config_dir(&self) -> Option<PathBuf>;
    /// `$XDG_DATA_HOME`, or `None` when no home directory can be determined.
    fn user_data_dir(&self) -> Option<PathBuf>;
    /// `$XDG_DATA_DIRS`, in precedence order.
    fn system_data_dirs(&self) -> Vec<PathBuf>;
}

/// The `dirs`-crate backend: ember's and frost's current behaviour, and the
/// one an app without a GTK dependency wants.
///
/// A GTK app writes the three-method glib equivalent in its own shim; it is
/// about fifteen lines and it keeps glib's fallback chain, which is not the
/// same as this one.
#[derive(Clone, Copy, Debug, Default)]
pub struct XdgEnvDirs;

impl DirSources for XdgEnvDirs {
    fn user_config_dir(&self) -> Option<PathBuf> {
        dirs::config_dir()
    }

    fn user_data_dir(&self) -> Option<PathBuf> {
        dirs::data_dir()
    }

    fn system_data_dirs(&self) -> Vec<PathBuf> {
        // `dirs` has no `data_dirs()`, so read the variable the way the
        // freedesktop basedir spec defines it, including its default.
        match std::env::var_os("XDG_DATA_DIRS") {
            Some(value) if !value.is_empty() => std::env::split_paths(&value).collect(),
            _ => vec![
                PathBuf::from("/usr/local/share"),
                PathBuf::from("/usr/share"),
            ],
        }
    }
}

/// The per-app half of a search path: which directory segment to look under,
/// which environment variable adds to it, and where the source tree is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchPathSpec {
    app: String,
    env_var: String,
    dev_root: Option<PathBuf>,
}

impl SearchPathSpec {
    /// `app` is the path segment (`"anvil"`), `env_var` the additive override
    /// (`"ANVIL_WORKFLOW_DIR"`).
    ///
    /// `dev_root` is the source-tree tier, and it is a parameter for a reason:
    /// every copy wrote `env!("CARGO_MANIFEST_DIR").join("scripts/workflows")`
    /// inline, and `env!` is resolved at compile time against the crate being
    /// compiled. Moving that expression into this crate would point all four
    /// apps at `jterm_core/scripts/workflows` — and their bundled-library
    /// contract tests would keep passing, because they would then be asserting
    /// about a directory that does not exist. Each app passes its own.
    pub fn new(
        app: impl Into<String>,
        env_var: impl Into<String>,
        dev_root: Option<PathBuf>,
    ) -> Self {
        Self {
            app: app.into(),
            env_var: env_var.into(),
            dev_root,
        }
    }

    /// Both names derived from one app segment: directory `app`, variable
    /// `<APP>_WORKFLOW_DIR`.
    ///
    /// All four apps already spell their variable that way, and deriving it is
    /// the point — it is what stops an app from looking under one name while
    /// honouring another's override.
    pub fn for_app(app: &str, dev_root: Option<PathBuf>) -> Self {
        Self::new(
            app,
            format!("{}_WORKFLOW_DIR", app.to_ascii_uppercase()),
            dev_root,
        )
    }

    /// [`for_app`](Self::for_app) with the name this process registered
    /// through [`crate::identity::init`] — or `None` when it registered none.
    ///
    /// # Why this is an `Option`
    ///
    /// [`crate::identity::get`] never fails: before `init` it answers with the
    /// neutral `"jterm"` identity, and *that* answer decides both policy values
    /// here — the directory segment and the override variable. A spec built on
    /// it reads `~/.config/jterm/workflows` and honours `$JTERM_WORKFLOW_DIR`,
    /// which is nobody's library and nobody's variable. Nothing downstream
    /// notices: [`load_all`](super::load_all) skips a directory that does not
    /// exist without a word, so the palette simply comes up empty. It is the
    /// same compile-time-versus-call-order trap `env!("CARGO_MANIFEST_DIR")`
    /// is a parameter to avoid, reached through a process global instead — and
    /// it is worst in tests, which never call `init` at all, where an app's
    /// own search-path assertions would keep passing while guarding the wrong
    /// directories.
    ///
    /// So the uninitialised case is not answerable, and a shim says which it
    /// is: `expect` it after `init`, or call
    /// [`for_app`](Self::for_app) with the name spelled out.
    pub fn for_current_app(dev_root: Option<PathBuf>) -> Option<Self> {
        crate::identity::try_get().map(|identity| Self::for_app(identity.app_name, dev_root))
    }

    pub fn app(&self) -> &str {
        &self.app
    }

    pub fn env_var(&self) -> &str {
        &self.env_var
    }

    pub fn dev_root(&self) -> Option<&Path> {
        self.dev_root.as_deref()
    }
}

/// The five-tier workflow search path, in precedence order: user config,
/// `$<APP>_WORKFLOW_DIR`, user data, each system data directory, then the
/// source tree.
///
/// The environment variable *adds* to the standard locations rather than
/// replacing them, and sits second so a directory named there overrides an
/// installed example but not the user's own config. Later duplicates are
/// dropped (first wins), which is what makes the whole path safe to
/// concatenate blindly, and the list is truncated to
/// [`MAX_WORKFLOW_DIRECTORIES`] *before* deduplication so a hostile
/// `$<APP>_WORKFLOW_DIR` cannot make the loader walk an unbounded number of
/// directories by listing distinct ones.
pub fn search_path(spec: &SearchPathSpec, sources: &dyn DirSources) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(dir) = absolute(sources.user_config_dir()) {
        dirs.push(dir.join(&spec.app).join("workflows"));
    }
    if let Some(extra) = std::env::var_os(&spec.env_var) {
        // Kept verbatim, absolute or not: a relative entry here is a choice
        // the user typed, not a lookup that silently failed.
        dirs.extend(std::env::split_paths(&extra).filter(|path| !path.as_os_str().is_empty()));
    }
    if let Some(dir) = absolute(sources.user_data_dir()) {
        dirs.push(dir.join(&spec.app).join("workflows"));
    }
    dirs.extend(
        sources
            .system_data_dirs()
            .into_iter()
            .filter(|dir| dir.is_absolute())
            .map(|dir| dir.join(&spec.app).join("workflows")),
    );
    if let Some(dev_root) = &spec.dev_root {
        dirs.push(dev_root.clone());
    }

    let mut unique = Vec::new();
    let mut seen = HashSet::new();
    for dir in dirs.into_iter().take(MAX_WORKFLOW_DIRECTORIES) {
        if seen.insert(dir.clone()) {
            unique.push(dir);
        }
    }
    unique
}

/// A directory a [`DirSources`] resolved, or nothing. See the trait contract:
/// a relative answer means the lookup failed in a way that would otherwise
/// resolve against the process's working directory.
fn absolute(dir: Option<PathBuf>) -> Option<PathBuf> {
    dir.filter(|dir| dir.is_absolute())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `DirSources` whose answers the test states outright, including the
    /// broken ones.
    struct FakeDirs {
        config: Option<PathBuf>,
        data: Option<PathBuf>,
        system: Vec<PathBuf>,
    }

    impl DirSources for FakeDirs {
        fn user_config_dir(&self) -> Option<PathBuf> {
            self.config.clone()
        }
        fn user_data_dir(&self) -> Option<PathBuf> {
            self.data.clone()
        }
        fn system_data_dirs(&self) -> Vec<PathBuf> {
            self.system.clone()
        }
    }

    fn spec() -> SearchPathSpec {
        SearchPathSpec::new("app", "JTERM_CORE_TEST_WORKFLOW_DIR_UNSET", None)
    }

    #[test]
    fn search_path_lists_every_tier_in_precedence_order() {
        let dirs = FakeDirs {
            config: Some(PathBuf::from("/home/u/.config")),
            data: Some(PathBuf::from("/home/u/.local/share")),
            system: vec![
                PathBuf::from("/usr/local/share"),
                PathBuf::from("/usr/share"),
            ],
        };
        let spec = SearchPathSpec::new(
            "app",
            "JTERM_CORE_TEST_WORKFLOW_DIR_UNSET",
            Some(PathBuf::from("/src/app/scripts/workflows")),
        );
        assert_eq!(
            search_path(&spec, &dirs),
            [
                PathBuf::from("/home/u/.config/app/workflows"),
                PathBuf::from("/home/u/.local/share/app/workflows"),
                PathBuf::from("/usr/local/share/app/workflows"),
                PathBuf::from("/usr/share/app/workflows"),
                PathBuf::from("/src/app/scripts/workflows"),
            ]
        );
    }

    #[test]
    fn a_relative_directory_source_contributes_nothing() {
        // forge derived its user tier from `HOME` with `unwrap_or_default()`,
        // so with HOME unset it scanned `./.config/forge/workflows`: a
        // repository checkout containing that directory became the
        // highest-precedence workflow library. A relative answer is a failed
        // lookup, and a failed lookup is a skipped tier.
        let dirs = FakeDirs {
            config: Some(PathBuf::from(".config")),
            data: Some(PathBuf::from("")),
            system: vec![PathBuf::from("share"), PathBuf::from("/usr/share")],
        };
        assert_eq!(
            search_path(&spec(), &dirs),
            [PathBuf::from("/usr/share/app/workflows")]
        );
    }

    #[test]
    fn missing_tiers_are_skipped_rather_than_guessed() {
        let dirs = FakeDirs {
            config: None,
            data: None,
            system: Vec::new(),
        };
        assert!(search_path(&spec(), &dirs).is_empty());
    }

    #[test]
    fn duplicate_tiers_collapse_to_the_highest_precedence_one() {
        let dirs = FakeDirs {
            config: Some(PathBuf::from("/same")),
            data: Some(PathBuf::from("/same")),
            system: vec![PathBuf::from("/same")],
        };
        assert_eq!(
            search_path(&spec(), &dirs),
            [PathBuf::from("/same/app/workflows")]
        );
    }

    #[test]
    fn the_search_path_is_bounded_before_deduplication() {
        // The cap must bite on the raw list: 64 distinct directories from the
        // environment must not become 64 *plus* the standard tiers.
        let dirs = FakeDirs {
            config: Some(PathBuf::from("/home/u/.config")),
            data: Some(PathBuf::from("/home/u/.local/share")),
            system: (0..MAX_WORKFLOW_DIRECTORIES * 2)
                .map(|index| PathBuf::from(format!("/sys/{index}")))
                .collect(),
        };
        let path = search_path(&spec(), &dirs);
        assert_eq!(path.len(), MAX_WORKFLOW_DIRECTORIES);
        assert_eq!(path[0], PathBuf::from("/home/u/.config/app/workflows"));
    }

    #[test]
    fn the_environment_variable_adds_a_tier_below_user_config() {
        let variable = "JTERM_CORE_TEST_WORKFLOW_DIR_TIER";
        let joined =
            std::env::join_paths([PathBuf::from("/extra/one"), PathBuf::from("/extra/two")])
                .unwrap();
        // Private to this test binary, and read only by the call below.
        std::env::set_var(variable, &joined);
        let dirs = FakeDirs {
            config: Some(PathBuf::from("/home/u/.config")),
            data: None,
            system: Vec::new(),
        };
        let path = search_path(&SearchPathSpec::new("app", variable, None), &dirs);
        std::env::remove_var(variable);
        assert_eq!(
            path,
            [
                PathBuf::from("/home/u/.config/app/workflows"),
                PathBuf::from("/extra/one"),
                PathBuf::from("/extra/two"),
            ]
        );
    }

    #[test]
    fn an_app_spec_derives_its_override_variable_from_its_segment() {
        // The derivation itself, stated without the process global, so this
        // assertion cannot be satisfied by the wrong app name.
        let spec = SearchPathSpec::for_app("anvil", None);
        assert_eq!(spec.app(), "anvil");
        assert_eq!(spec.env_var(), "ANVIL_WORKFLOW_DIR");
        assert!(spec.dev_root().is_none());
    }

    #[test]
    fn the_current_app_spec_refuses_an_unregistered_identity() {
        // This test binary never calls `identity::init` — no test in this
        // crate does, and app test binaries do not either. `identity::get()`
        // would hand back the neutral "jterm" here, and a spec built on it
        // would read `~/.config/jterm/workflows` and honour
        // `$JTERM_WORKFLOW_DIR` with no Option, no log line and no panic:
        // just a palette that is empty for a reason nothing reports.
        //
        // The predecessor of this test asserted only that the variable
        // matched the segment, which is true of "jterm" as well — so it was
        // green precisely when the bug was present. This one is not.
        assert!(
            SearchPathSpec::for_current_app(None).is_none(),
            "an unregistered identity must not resolve to a search path"
        );
    }
}
