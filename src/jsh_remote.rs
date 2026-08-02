//! Opening a tab on a machine that does not have jsh installed.
//!
//! A remote tab has always been "run ssh, hope the far side has a shell worth
//! having". When it does not, the terminal loses everything that makes it a
//! jterm: no OSC 133 blocks, no cwd tracking, no exit codes, no Commands
//! timeline — because those come from jsh, not from the terminal.
//!
//! `jsh-remote.sh` closes that by placing a static jsh on the destination for
//! the life of the session. This module is the part a terminal needs: it
//! publishes the vendored script and turns a host description into argv. It
//! deliberately does not connect to anything, so a frontend can build the argv
//! on the UI thread and hand it to whatever spawns panes.
//!
//! The two modes are the same two the script documents, and the distinction is
//! the one that decides whether a shared account is safe:
//!
//!   * [`Deploy::Persist`] lets jsh keep its dot-files in the destination's
//!     `$HOME` and caches the binary there, so history survives and repeat tabs
//!     skip the transfer.
//!   * [`Deploy::Incognito`] sandboxes `HOME` for the session and deletes it on
//!     exit, so nothing is written to an account other people also use.

use std::io;
use std::path::Path;

use crate::vendored_script::VendoredScript;

/// Vendored from the jsh repository (`scripts/jsh-remote.sh`).
const SCRIPT: VendoredScript = VendoredScript {
    name: "jsh-remote.sh",
    source: include_str!("../scripts/jsh-remote.sh"),
};

/// How much of itself a deployed session is allowed to leave behind.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Deploy {
    /// Do not deploy. The tab connects with plain ssh, exactly as before.
    #[default]
    Off,
    /// Keep jsh's dot-files and a cached binary in the destination's `$HOME`.
    Persist,
    /// Sandbox `HOME` for the session and delete it on exit.
    Incognito,
}

impl Deploy {
    /// Parse a configuration value. Unknown spellings are rejected rather than
    /// defaulted, so a typo in a config file cannot silently downgrade
    /// `incognito` to something that writes to a shared account.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" | "false" | "no" | "none" => Some(Self::Off),
            "persist" | "true" | "yes" | "on" => Some(Self::Persist),
            "incognito" | "private" | "ephemeral" => Some(Self::Incognito),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Persist => "persist",
            Self::Incognito => "incognito",
        }
    }

    pub fn is_enabled(self) -> bool {
        !matches!(self, Self::Off)
    }
}

/// What a terminal knows about a destination, in the shape the launcher wants.
///
/// Borrowed rather than owned because every caller already has these strings in
/// its own config struct; this type exists to keep two apps from disagreeing
/// about the argument order.
#[derive(Clone, Copy, Debug)]
pub struct RemoteTarget<'a> {
    /// `[user@]host` for ssh, or the container name when `docker` is set.
    pub destination: &'a str,
    /// Connect to a running container with `docker exec` instead of ssh.
    pub docker: bool,
    /// The user to become inside the container. Meaningless without `docker`,
    /// and ignored there, because an ssh destination carries its user in
    /// `destination` instead.
    pub docker_user: Option<&'a str>,
    /// A jsh built here to push, instead of fetching a published release.
    /// The only way to deploy on a machine with no release to fetch — an
    /// unreleased build, or no network at all. Dropped unless it is an
    /// absolute path: a relative one would resolve against whatever directory
    /// the tab happens to start in, and a leading `-` would be read as an
    /// option by the launcher.
    pub artifact: Option<&'a Path>,
    /// Stable session id forwarded to the remote jsh for resume-on-reconnect.
    /// Callers are expected to have validated it; an invalid id is dropped.
    pub session: Option<&'a str>,
    /// Extra ssh arguments, inserted verbatim.
    pub ssh_args: &'a [String],
    pub deploy: Deploy,
}

/// Publish the launcher and build the argv for a deployed remote tab.
///
/// Returns an error only when the script cannot be written; a caller that wants
/// to degrade to plain ssh should fall back on `Err`.
pub fn launch_argv(target: &RemoteTarget<'_>) -> io::Result<Vec<String>> {
    let script = publish_launcher()?;
    Ok(launch_argv_with_script(&script, target))
}

/// Put the vendored launcher on disk and return its path.
///
/// Separate from [`launch_argv`] so a caller can publish once and then build
/// argv for several hosts, and so the failure that needs a fallback (writing the
/// script) is distinguishable from the part that cannot fail (argument order).
pub fn publish_launcher() -> io::Result<std::path::PathBuf> {
    SCRIPT.path()
}

/// The argv half of [`launch_argv`], for a launcher that is already on disk.
///
/// Public because it is the only way to assert the argument order without
/// publishing anything: a test that went through [`launch_argv`] would write
/// into the developer's real cache directory, and would quietly exercise the
/// plain-ssh fallback instead on any machine where that write fails.
pub fn launch_argv_with_script(script: &Path, target: &RemoteTarget<'_>) -> Vec<String> {
    // `sh <script>` rather than executing the script directly: the published
    // copy is 0700 and this avoids depending on the cache directory being on a
    // filesystem mounted with exec.
    let mut argv = vec!["/bin/sh".to_string(), script.display().to_string()];

    match target.deploy {
        // Off never reaches here through `launch_argv`, but spelling it out
        // keeps the mapping total rather than relying on a caller's check.
        Deploy::Off | Deploy::Persist => argv.push("--persist".to_string()),
        Deploy::Incognito => argv.push("--incognito".to_string()),
    }

    if let Some(session) = target
        .session
        .filter(|id| crate::execution_journal::is_valid_jsh_session_id(id))
    {
        argv.push("--session".to_string());
        argv.push(session.to_string());
    }

    if let Some(artifact) = target.artifact.filter(|path| path.is_absolute()) {
        argv.push("--artifact".to_string());
        argv.push(artifact.display().to_string());
    }

    if target.docker {
        argv.push("--docker".to_string());
        argv.push(target.destination.to_string());
        if let Some(user) = target.docker_user {
            argv.push("--docker-user".to_string());
            argv.push(user.to_string());
        }
    } else {
        // Everything after `--` goes to ssh. The destination comes first so it
        // is never mistaken for one of those pass-through arguments.
        argv.push(target.destination.to_string());
        if !target.ssh_args.is_empty() {
            argv.push("--".to_string());
            argv.extend(target.ssh_args.iter().cloned());
        }
    }
    argv
}

#[cfg(test)]
mod tests {
    use super::{launch_argv_with_script, Deploy, RemoteTarget};
    use std::path::Path;

    fn target<'a>(destination: &'a str, ssh_args: &'a [String]) -> RemoteTarget<'a> {
        RemoteTarget {
            destination,
            docker: false,
            docker_user: None,
            artifact: None,
            session: None,
            ssh_args,
            deploy: Deploy::Persist,
        }
    }

    #[test]
    fn ssh_destination_and_pass_through_arguments() {
        let args = vec!["-p".to_string(), "2222".to_string()];
        let argv =
            launch_argv_with_script(Path::new("/c/jsh-remote.sh"), &target("yj@host", &args));
        assert_eq!(
            argv,
            vec![
                "/bin/sh",
                "/c/jsh-remote.sh",
                "--persist",
                "yj@host",
                "--",
                "-p",
                "2222"
            ]
        );
    }

    #[test]
    fn no_pass_through_separator_without_ssh_arguments() {
        let argv = launch_argv_with_script(Path::new("/c/jsh-remote.sh"), &target("host", &[]));
        assert_eq!(
            argv,
            vec!["/bin/sh", "/c/jsh-remote.sh", "--persist", "host"]
        );
    }

    #[test]
    fn incognito_and_a_session_id() {
        let mut t = target("host", &[]);
        t.deploy = Deploy::Incognito;
        t.session = Some("cloud-1");
        let argv = launch_argv_with_script(Path::new("/c/jsh-remote.sh"), &t);
        assert_eq!(
            argv,
            vec![
                "/bin/sh",
                "/c/jsh-remote.sh",
                "--incognito",
                "--session",
                "cloud-1",
                "host"
            ]
        );
    }

    #[test]
    fn an_invalid_session_id_is_dropped_not_forwarded() {
        let mut t = target("host", &[]);
        t.session = Some("has spaces; rm -rf /");
        let argv = launch_argv_with_script(Path::new("/c/jsh-remote.sh"), &t);
        assert!(!argv.iter().any(|a| a == "--session"), "{argv:?}");
    }

    #[test]
    fn docker_takes_a_container_and_ignores_ssh_arguments() {
        let args = vec!["-p".to_string(), "22".to_string()];
        let mut t = target("my-service", &args);
        t.docker = true;
        let argv = launch_argv_with_script(Path::new("/c/jsh-remote.sh"), &t);
        assert_eq!(
            argv,
            vec![
                "/bin/sh",
                "/c/jsh-remote.sh",
                "--persist",
                "--docker",
                "my-service"
            ]
        );
    }

    #[test]
    fn deploy_parsing_rejects_what_it_does_not_understand() {
        assert_eq!(Deploy::parse("persist"), Some(Deploy::Persist));
        assert_eq!(Deploy::parse(" Incognito "), Some(Deploy::Incognito));
        assert_eq!(Deploy::parse("off"), Some(Deploy::Off));
        assert_eq!(Deploy::parse("true"), Some(Deploy::Persist));
        // A typo must not resolve to a mode that writes to the destination.
        assert_eq!(Deploy::parse("incognito!"), None);
        assert_eq!(Deploy::parse("privat"), None);
        assert_eq!(Deploy::parse(""), None);
    }

    #[test]
    fn the_vendored_script_is_the_launcher_we_expect() {
        let source = super::SCRIPT.source;
        assert!(source.starts_with("#!/bin/sh"), "not a shell script");
        assert!(
            source.contains("--incognito") && source.contains("--persist"),
            "vendored jsh-remote.sh predates the two-mode interface; re-vendor it"
        );
    }

    #[test]
    fn a_container_user_travels_as_its_own_option() {
        let mut t = target("my-service", &[]);
        t.docker = true;
        t.docker_user = Some("devuser");
        let argv = launch_argv_with_script(Path::new("/c/jsh-remote.sh"), &t);
        // Never `user@container`: `docker exec` would read that as the name of
        // a container nobody has.
        assert!(!argv.iter().any(|a| a.contains('@')), "{argv:?}");
        let user = argv
            .iter()
            .position(|a| a == "--docker-user")
            .expect("--docker-user");
        assert_eq!(argv[user + 1], "devuser");
    }

    #[test]
    fn a_container_user_means_nothing_over_ssh() {
        let mut t = target("yj@host", &[]);
        t.docker_user = Some("devuser");
        let argv = launch_argv_with_script(Path::new("/c/jsh-remote.sh"), &t);
        assert!(!argv.iter().any(|a| a == "--docker-user"), "{argv:?}");
    }

    #[test]
    fn a_local_artifact_is_deployed_instead_of_a_release() {
        let mut t = target("host", &[]);
        t.artifact = Some(Path::new("/home/yj/jsh/target/release/jsh"));
        let argv = launch_argv_with_script(Path::new("/c/jsh-remote.sh"), &t);
        assert_eq!(
            argv,
            vec![
                "/bin/sh",
                "/c/jsh-remote.sh",
                "--persist",
                "--artifact",
                "/home/yj/jsh/target/release/jsh",
                "host"
            ]
        );
    }

    #[test]
    fn an_artifact_that_is_not_an_absolute_path_is_dropped() {
        // A relative path would resolve against whatever directory the tab
        // started in, and `-foo` would be read as an option. Neither is worth
        // guessing about: without it the launcher fetches a release, which is
        // the behaviour of every host that never named one.
        for candidate in ["target/release/jsh", "-artifact"] {
            let mut t = target("host", &[]);
            t.artifact = Some(Path::new(candidate));
            let argv = launch_argv_with_script(Path::new("/c/jsh-remote.sh"), &t);
            assert!(!argv.iter().any(|a| a == "--artifact"), "{argv:?}");
        }
    }
}
