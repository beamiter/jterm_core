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

/// A remote destination the way a serde-configured app declares one.
///
/// ember and frost keep their whole configuration in one serde struct, so
/// their `[[remote_hosts]]` entries deserialize into this shared type and the
/// argv both apps hand a new tab comes from here — one grammar, one builder,
/// tested once. anvil and forge predate it with hand-rolled TOML pipelines
/// of the same shape; the *semantics* stay aligned through [`RemoteTarget`],
/// which every app ultimately goes through.
///
/// Unknown `deploy` spellings and unusable fields are rejected by
/// [`RemoteHostConfig::validate`], not at deserialization: one bad host entry
/// should cost that host, not silently revert the application's entire
/// configuration to defaults.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RemoteHostConfig {
    /// Display name in a picker; the host when empty.
    #[serde(default)]
    pub name: String,
    /// The ssh destination host, or the running container's name.
    pub host: String,
    /// The ssh login, or the `docker exec -u` user inside the container.
    #[serde(default)]
    pub user: Option<String>,
    /// Reach `host` with `docker exec` instead of ssh.
    #[serde(default)]
    pub docker: bool,
    /// Shell to run when nothing is deployed (default `jsh`).
    #[serde(default = "default_remote_shell")]
    pub remote_shell: String,
    /// Stable session id forwarded for resume-on-reconnect.
    #[serde(default)]
    pub session: Option<String>,
    /// Extra ssh arguments, inserted verbatim. Meaningless for a container.
    #[serde(default)]
    pub ssh_args: Vec<String>,
    /// `off` (default), `persist`, or `incognito` — see [`Deploy`].
    #[serde(default)]
    pub deploy: String,
    /// A jsh built locally for `deploy` to push instead of lending the local
    /// one or fetching a release. Absolute path.
    #[serde(default)]
    pub deploy_artifact: Option<String>,
}

fn default_remote_shell() -> String {
    "jsh".to_string()
}

impl RemoteHostConfig {
    pub fn display_name(&self) -> &str {
        if self.name.is_empty() {
            &self.host
        } else {
            &self.name
        }
    }

    fn parsed_deploy(&self) -> Option<Deploy> {
        if self.deploy.is_empty() {
            Some(Deploy::Off)
        } else {
            Deploy::parse(&self.deploy)
        }
    }

    /// Why this entry cannot be used, or `Ok` when it can.
    ///
    /// Everything here ends up in an argv, so the grammar is the family's
    /// usual one: no whitespace or control characters where a token must stay
    /// one token, no leading `-` where a value must not read as a flag, and a
    /// `deploy` spelling this build does not understand rejects the host
    /// rather than quietly downgrading `incognito` to something that writes
    /// to a shared account.
    pub fn validate(&self) -> Result<(), String> {
        fn plain_token(value: &str, what: &str) -> Result<(), String> {
            if value.is_empty() {
                return Err(format!("{what} must not be empty"));
            }
            if value.len() > 512 {
                return Err(format!("{what} is too long"));
            }
            if value.chars().any(|c| c.is_whitespace() || c.is_control()) {
                return Err(format!(
                    "{what} must not contain whitespace or control characters"
                ));
            }
            Ok(())
        }
        plain_token(&self.host, "host")?;
        if self.host.starts_with('-') {
            return Err("host must not begin with '-'".to_string());
        }
        if !self.name.is_empty() && self.name.chars().any(char::is_control) {
            return Err("name must not contain control characters".to_string());
        }
        if let Some(user) = &self.user {
            plain_token(user, "user")?;
            if user.contains('@') {
                return Err("user must not contain '@'".to_string());
            }
        }
        plain_token(&self.remote_shell, "remote_shell")?;
        if let Some(session) = &self.session {
            if !crate::execution_journal::is_valid_jsh_session_id(session) {
                return Err("session must be 1-128 ASCII letters, digits, '_' or '-'".to_string());
            }
        }
        if self.ssh_args.len() > 64 {
            return Err("ssh_args must not contain more than 64 entries".to_string());
        }
        for arg in &self.ssh_args {
            if arg.is_empty() || arg.len() > 512 || arg.chars().any(char::is_control) {
                return Err("ssh_args entries must be short, non-empty, and printable".to_string());
            }
        }
        if self.parsed_deploy().is_none() {
            return Err(format!(
                "deploy '{}' is not understood (expected off, persist, or incognito)",
                self.deploy.escape_debug()
            ));
        }
        if let Some(artifact) = &self.deploy_artifact {
            if !Path::new(artifact).is_absolute() {
                return Err("deploy_artifact must be an absolute path".to_string());
            }
        }
        Ok(())
    }

    /// The `[user@]host` ssh destination, or the bare container name —
    /// `docker exec` would read `user@container` as a container nobody has.
    fn destination(&self) -> String {
        match (&self.user, self.docker) {
            (Some(user), false) => format!("{user}@{}", self.host),
            _ => self.host.clone(),
        }
    }

    fn valid_session(&self) -> Option<&str> {
        self.session
            .as_deref()
            .filter(|id| crate::execution_journal::is_valid_jsh_session_id(id))
    }

    /// argv for a tab that deploys jsh onto the destination. Errs only when
    /// the launcher script cannot be published; callers degrade to
    /// [`Self::plain_argv`] and say so, exactly as anvil/forge do.
    pub fn deployed_argv(&self) -> io::Result<Vec<String>> {
        Ok(self.deployed_argv_with_script(&publish_launcher()?))
    }

    /// The argv half of [`Self::deployed_argv`], for a launcher already on
    /// disk — the same split [`launch_argv_with_script`] exists for, and for
    /// the same reason: order can be asserted without publishing anything.
    pub fn deployed_argv_with_script(&self, script: &Path) -> Vec<String> {
        let deploy = self.parsed_deploy().unwrap_or(Deploy::Off);
        let ssh_args: &[String] = if self.docker { &[] } else { &self.ssh_args };
        launch_argv_with_script(
            script,
            &RemoteTarget {
                destination: &self.destination(),
                docker: self.docker,
                docker_user: if self.docker {
                    self.user.as_deref()
                } else {
                    None
                },
                artifact: self.deploy_artifact.as_deref().map(Path::new),
                session: self.session.as_deref(),
                ssh_args,
                deploy,
            },
        )
    }

    /// argv for a tab that deploys nothing: plain ssh, or a plain
    /// `docker exec`, running `remote_shell` as found on the destination.
    pub fn plain_argv(&self) -> Vec<String> {
        if self.docker {
            let mut argv = vec!["docker".to_string(), "exec".to_string(), "-it".to_string()];
            if let Some(user) = &self.user {
                argv.push("-u".to_string());
                argv.push(user.clone());
            }
            argv.push(self.host.clone());
            argv.push(self.remote_shell.clone());
            if let Some(session) = self.valid_session() {
                argv.push("--session".to_string());
                argv.push(session.to_string());
            }
            argv
        } else {
            // ssh joins its trailing words and hands them to the remote login
            // shell; the session id is validated to a token that survives
            // that reparse unquoted.
            let mut remote_command = self.remote_shell.clone();
            if let Some(session) = self.valid_session() {
                remote_command.push_str(" --session ");
                remote_command.push_str(session);
            }
            let mut argv = vec!["ssh".to_string(), "-t".to_string()];
            argv.extend(self.ssh_args.iter().cloned());
            argv.push("--".to_string());
            argv.push(self.destination());
            argv.push(remote_command);
            argv
        }
    }

    /// The argv a tab should run: deployment when configured, with the plain
    /// connection as the fallback the caller is told about.
    pub fn tab_argv(&self) -> (Vec<String>, Option<io::Error>) {
        if self.parsed_deploy().unwrap_or(Deploy::Off).is_enabled() {
            match self.deployed_argv() {
                Ok(argv) => (argv, None),
                Err(error) => (self.plain_argv(), Some(error)),
            }
        } else {
            (self.plain_argv(), None)
        }
    }
}

/// Result of inspecting one real process argv for an interactive SSH target.
///
/// The distinction lets a frontend silently ignore other foreground commands
/// while showing one bounded, actionable notice for an SSH invocation whose
/// connection semantics cannot safely be replayed by a file-tree sidecar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ObservedSshTarget {
    NotSsh,
    Unsupported(&'static str),
    Target(RemoteHostConfig),
}

const UNSAFE_SSH_OPTION: &str =
    "this SSH command uses an option that cannot be safely reused for Files";
const UNSAFE_OBSERVED_CONTROL_PATH: &str =
    "SSH ControlPath reused by Files must be an absolute path or start with ~/";

fn valid_observed_control_path(path: &str) -> bool {
    Path::new(path).is_absolute()
        || path
            .strip_prefix("~/")
            .is_some_and(|relative| !relative.is_empty())
}

fn safe_o_option(option: &str) -> bool {
    let key = option
        .split_once('=')
        .map_or(option, |(key, _)| key)
        .to_ascii_lowercase();
    matches!(
        key.as_str(),
        "addressfamily"
            | "batchmode"
            | "bindaddress"
            | "bindinterface"
            | "canonicalizehostname"
            | "certificatefile"
            | "checkhostip"
            | "ciphers"
            | "connectionattempts"
            | "connecttimeout"
            | "controlmaster"
            | "controlpath"
            | "controlpersist"
            | "fingerprinthash"
            | "globalknownhostsfile"
            | "hostbasedauthentication"
            | "hostkeyalgorithms"
            | "hostname"
            | "identityagent"
            | "identityfile"
            | "identitiesonly"
            | "ipqos"
            | "kbdinteractiveauthentication"
            | "kexalgorithms"
            | "loglevel"
            | "macs"
            | "numberofpasswordprompts"
            | "passwordauthentication"
            | "port"
            | "preferredauthentications"
            | "proxyjump"
            | "pubkeyacceptedalgorithms"
            | "pubkeyauthentication"
            | "rekeylimit"
            | "requiredrsasize"
            | "sendenv"
            | "serveralivecountmax"
            | "serveraliveinterval"
            | "stricthostkeychecking"
            | "tcpkeepalive"
            | "updatehostkeys"
            | "user"
            | "userknownhostsfile"
    )
}

fn valid_observed_port(port: &str) -> bool {
    port.parse::<u16>().is_ok_and(|port| port != 0)
}

fn parse_observed_destination(destination: &str) -> Option<(Option<String>, String)> {
    if destination.is_empty() || destination.starts_with('-') || destination.len() > 512 {
        return None;
    }
    let (user, host) = match destination.rsplit_once('@') {
        Some((user, host)) if !user.is_empty() && !host.is_empty() && !user.contains('@') => {
            (Some(user.to_string()), host.to_string())
        }
        Some(_) => return None,
        None => (None, destination.to_string()),
    };
    if host.starts_with('-') || host.is_empty() || host.len() > 512 {
        return None;
    }
    Some((user, host))
}

fn observed_name(user: Option<&str>, host: &str) -> String {
    match user {
        Some(user) => format!("{user}@{host}"),
        None => host.to_string(),
    }
}

/// Whether this is the structured launcher argv emitted by jsh and the four
/// terminal frontends for a deployed remote session.
///
/// The path itself may live below either jsh's or jterm's private cache, so the
/// stable authority is the real foreground process boundary plus the exact
/// `/bin/sh …/jsh-remote.sh` shape. The parser below still validates every
/// launcher field and every SSH argument before a Files sidecar can reuse it.
pub(crate) fn is_jsh_remote_launcher_argv(argv: &[String]) -> bool {
    argv.first().is_some_and(|command| command == "/bin/sh")
        && argv.get(1).is_some_and(|script| {
            let script = Path::new(script);
            script.is_absolute()
                && script
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name == "jsh-remote.sh")
        })
}

/// The narrower launcher form emitted by jsh's `ssh …` command upgrade.
/// Process inspection requires this fixed prefix before treating a script
/// process as equivalent to the SSH command the user entered.
pub(crate) fn is_jsh_ssh_upgrade_launcher_argv(argv: &[String]) -> bool {
    is_jsh_remote_launcher_argv(argv)
        && argv.get(2).is_some_and(|mode| mode == "--persist")
        && argv.get(3).is_some_and(|option| option == "--local-jsh")
        && argv
            .get(4)
            .is_some_and(|artifact| Path::new(artifact).is_absolute())
        && argv
            .get(5)
            .is_some_and(|destination| !destination.is_empty() && !destination.starts_with('-'))
        && (argv.len() == 6
            || (argv.get(6).is_some_and(|separator| separator == "--") && argv.len() > 7))
}

fn observed_jsh_remote_target(argv: &[String]) -> ObservedSshTarget {
    if !crate::process::restorable_argv_within_limits(argv) {
        return ObservedSshTarget::Unsupported("this jsh remote launcher is too large or unsafe");
    }

    let mut destination: Option<String> = None;
    let mut ssh_args = Vec::new();
    let mut mode_seen = false;
    let mut index = 2usize;
    while index < argv.len() {
        let argument = &argv[index];
        if argument == "--" {
            if destination.is_none() {
                return ObservedSshTarget::Unsupported(
                    "this jsh remote launcher has no SSH destination",
                );
            }
            ssh_args.extend(argv[index + 1..].iter().cloned());
            break;
        }

        match argument.as_str() {
            "--persist" | "--incognito" => {
                if mode_seen {
                    return ObservedSshTarget::Unsupported(
                        "this jsh remote launcher has conflicting modes",
                    );
                }
                mode_seen = true;
            }
            // These fields control jsh deployment, not the connection Files
            // needs. Consume their structured operands without replaying them.
            "--session" | "--artifact" | "--local-jsh" => {
                index += 1;
                if argv.get(index).is_none_or(String::is_empty) {
                    return ObservedSshTarget::Unsupported(
                        "this jsh remote launcher option has no value",
                    );
                }
            }
            // A container launcher is real, but it is not an SSH target. The
            // four frontends already handle configured Docker locations.
            "--docker" => return ObservedSshTarget::NotSsh,
            option if option.starts_with('-') => {
                return ObservedSshTarget::Unsupported(
                    "this jsh remote launcher uses an unsupported option",
                );
            }
            _ if destination.is_none() => destination = Some(argument.clone()),
            _ => {
                return ObservedSshTarget::Unsupported(
                    "this jsh remote launcher has more than one destination",
                );
            }
        }
        index += 1;
    }

    if !mode_seen {
        return ObservedSshTarget::Unsupported(
            "this jsh remote launcher has no explicit deployment mode",
        );
    }
    let Some(destination) = destination else {
        return ObservedSshTarget::Unsupported("this jsh remote launcher has no SSH destination");
    };

    let mut ssh = Vec::with_capacity(2 + ssh_args.len());
    ssh.push("ssh".to_string());
    ssh.extend(ssh_args);
    // jsh-remote invokes `ssh ${ssh_extra} -- destination`, even when the
    // original command placed options after its destination. Preserve that
    // effective ordering for first-wins options such as `-l`.
    ssh.push(destination);
    observed_plain_ssh_target(&ssh)
}

/// Derive a transient file-tree profile from the argv of a *real* foreground
/// process.
///
/// Callers must obtain `argv` from an OS process boundary such as
/// `/proc/<pid>/cmdline`. Frontends should use
/// [`crate::process::observed_ssh_command`] (or its `via_stat` counterpart),
/// which additionally proves the provenance of jsh's deployment launcher;
/// generic session-restoration discovery intentionally does not replay that
/// launcher. Terminal output and OSC 133 command strings are untrusted text
/// and must not be passed here as proof that the user launched SSH.
///
/// Only an interactive login with no remote command is accepted. Connection
/// options needed by a second non-interactive probe are retained; TTY,
/// forwarding, logging, and presentation flags are deliberately dropped.
/// Options that can execute a local command or load code are rejected. An
/// observed ControlPath must be absolute or use `~/` so replay cannot silently
/// resolve the socket against the file sidecar's different working directory.
pub fn observed_ssh_target(argv: &[String]) -> ObservedSshTarget {
    if is_jsh_remote_launcher_argv(argv) {
        return observed_jsh_remote_target(argv);
    }
    observed_plain_ssh_target(argv)
}

fn observed_plain_ssh_target(argv: &[String]) -> ObservedSshTarget {
    if argv
        .first()
        .and_then(|command| Path::new(command).file_name())
        .and_then(|command| command.to_str())
        != Some("ssh")
    {
        return ObservedSshTarget::NotSsh;
    }
    if !crate::process::restorable_argv_within_limits(argv) {
        return ObservedSshTarget::Unsupported("this SSH command is too large or unsafe");
    }

    let mut destination: Option<String> = None;
    let mut user: Option<String> = None;
    let mut ssh_args = Vec::new();
    let mut end_options = false;
    let mut index = 1usize;
    let mut port_seen = false;
    let mut address_family_seen = false;

    while index < argv.len() {
        let argument = &argv[index];
        if !end_options && argument == "--" {
            end_options = true;
            index += 1;
            continue;
        }

        if !end_options && argument.starts_with('-') && argument != "-" {
            if argument.starts_with("--") {
                return ObservedSshTarget::Unsupported(
                    "long SSH options cannot be safely reused for Files",
                );
            }
            let bytes = argument.as_bytes();
            let mut offset = 1usize;
            while offset < bytes.len() {
                let flag = bytes[offset] as char;
                let takes_operand = "BbcDEeFIiJLlmOoPpQRSWw".contains(flag);
                let operand = if takes_operand {
                    if offset + 1 < bytes.len() {
                        Some(argument[offset + 1..].to_string())
                    } else {
                        index += 1;
                        argv.get(index).cloned()
                    }
                } else {
                    None
                };

                if flag == 'S'
                    && operand
                        .as_deref()
                        .is_none_or(|path| !valid_observed_control_path(path))
                {
                    return ObservedSshTarget::Unsupported(UNSAFE_OBSERVED_CONTROL_PATH);
                }
                if takes_operand && operand.as_deref().is_none_or(str::is_empty) {
                    return ObservedSshTarget::Unsupported("this SSH option has no value");
                }

                match flag {
                    '4' | '6' => {
                        if !address_family_seen {
                            ssh_args.push(format!("-{flag}"));
                            address_family_seen = true;
                        }
                    }
                    // These affect reachability or authentication and are safe
                    // to preserve as structured argv.
                    'B' | 'b' | 'c' | 'i' | 'J' | 'm' | 'P' | 'S' => {
                        ssh_args.push(format!("-{flag}"));
                        ssh_args.push(operand.as_ref().expect("operand option").clone());
                    }
                    'l' => {
                        if user.is_none() {
                            user = operand;
                        }
                    }
                    'p' => {
                        let port = operand.as_deref().expect("operand option");
                        if !valid_observed_port(port) {
                            return ObservedSshTarget::Unsupported(
                                "the SSH port must be between 1 and 65535",
                            );
                        }
                        if !port_seen {
                            ssh_args.push("-p".to_string());
                            ssh_args.push(port.to_string());
                            port_seen = true;
                        }
                    }
                    'o' => {
                        let option = operand.as_deref().expect("operand option");
                        if !safe_o_option(option) {
                            return ObservedSshTarget::Unsupported(UNSAFE_SSH_OPTION);
                        }
                        let (key, value) = option
                            .split_once('=')
                            .map_or((option, None), |(key, value)| (key, Some(value)));
                        if key.eq_ignore_ascii_case("controlpath")
                            && value.is_none_or(|path| !valid_observed_control_path(path))
                        {
                            return ObservedSshTarget::Unsupported(UNSAFE_OBSERVED_CONTROL_PATH);
                        }
                        ssh_args.push("-o".to_string());
                        ssh_args.push(option.to_string());
                    }
                    // These modes are not an interactive remote shell.
                    'G' | 'N' | 'O' | 'Q' | 'V' | 'W' | 's' | 'w' => {
                        return ObservedSshTarget::Unsupported(
                            "this SSH invocation is not an interactive login",
                        );
                    }
                    // Loading a provider library or executing configured local
                    // helpers must never happen merely because Files follows a
                    // terminal command.
                    'F' | 'I' => return ObservedSshTarget::Unsupported(UNSAFE_SSH_OPTION),
                    // Forwarding, TTY, agent, X11, compression, verbosity and
                    // escape/log options do not belong on a filesystem probe.
                    'A' | 'a' | 'C' | 'D' | 'E' | 'e' | 'f' | 'g' | 'K' | 'k' | 'L' | 'M' | 'n'
                    | 'q' | 'R' | 'T' | 't' | 'v' | 'X' | 'x' | 'Y' | 'y' => {}
                    _ => {
                        return ObservedSshTarget::Unsupported(
                            "this SSH command uses an unknown option",
                        )
                    }
                }

                if takes_operand {
                    break;
                }
                offset += 1;
            }
            index += 1;
            continue;
        }

        if destination.is_some() {
            return ObservedSshTarget::Unsupported(
                "SSH remote commands are not automatically reused for Files",
            );
        }
        let Some((destination_user, host)) = parse_observed_destination(argument) else {
            return ObservedSshTarget::Unsupported("the SSH destination is invalid");
        };
        if user.is_none() {
            user = destination_user;
        }
        destination = Some(host);
        index += 1;
    }

    let Some(host) = destination else {
        return ObservedSshTarget::Unsupported("this SSH command has no destination");
    };
    let target = RemoteHostConfig {
        name: observed_name(user.as_deref(), &host),
        host,
        user,
        docker: false,
        remote_shell: default_remote_shell(),
        session: None,
        ssh_args,
        deploy: Deploy::Off.as_str().to_string(),
        deploy_artifact: None,
    };
    if target.validate().is_err() {
        ObservedSshTarget::Unsupported("the SSH destination is invalid")
    } else {
        ObservedSshTarget::Target(target)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        launch_argv_with_script, observed_ssh_target, Deploy, ObservedSshTarget, RemoteTarget,
    };
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

    fn ssh(args: &[&str]) -> ObservedSshTarget {
        observed_ssh_target(
            &args
                .iter()
                .map(|argument| argument.to_string())
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn observed_ssh_accepts_destination_then_port_without_reparsing_text() {
        let ObservedSshTarget::Target(target) =
            ssh(&["/usr/bin/ssh", "root@dsw-notebook.example.com", "-p", "22"])
        else {
            panic!("expected an observed SSH target");
        };
        assert_eq!(target.name, "root@dsw-notebook.example.com");
        assert_eq!(target.host, "dsw-notebook.example.com");
        assert_eq!(target.user.as_deref(), Some("root"));
        assert_eq!(target.ssh_args, ["-p", "22"]);
        assert_eq!(target.deploy, "off");
    }

    #[test]
    fn observed_ssh_accepts_the_real_jsh_upgrade_launcher_shape() {
        let argv = [
            "/bin/sh",
            "/home/alice/.cache/jsh/jsh-remote.sh",
            "--persist",
            "--local-jsh",
            "/home/alice/.local/bin/jsh",
            "root@dsw-notebook-dsw-l8rnh0wm7vs81o7z6j-22.vpc.example.com",
            "--",
            "-p",
            "22",
        ];
        assert!(super::is_jsh_ssh_upgrade_launcher_argv(
            &argv
                .iter()
                .map(|value| value.to_string())
                .collect::<Vec<_>>()
        ));
        let ObservedSshTarget::Target(target) = ssh(&argv) else {
            panic!("expected the jsh launcher to retain its SSH target");
        };
        assert_eq!(
            target.host,
            "dsw-notebook-dsw-l8rnh0wm7vs81o7z6j-22.vpc.example.com"
        );
        assert_eq!(target.user.as_deref(), Some("root"));
        assert_eq!(target.ssh_args, ["-p", "22"]);
    }

    #[test]
    fn observed_jsh_launcher_preserves_the_effective_ssh_option_order() {
        let ObservedSshTarget::Target(target) = ssh(&[
            "/bin/sh",
            "/home/alice/.cache/jsh/jsh-remote.sh",
            "--persist",
            "--local-jsh",
            "/home/alice/.local/bin/jsh",
            "bob@box.example",
            "--",
            "-l",
            "alice",
        ]) else {
            panic!("expected the jsh launcher to retain its SSH target");
        };
        assert_eq!(target.user.as_deref(), Some("alice"));
        assert_eq!(target.host, "box.example");
    }

    #[test]
    fn observed_ssh_accepts_the_terminal_launcher_without_deployment_fields() {
        let argv = [
            "/bin/sh",
            "/home/alice/.cache/jterm/jsh-remote.sh",
            "--incognito",
            "--session",
            "work_7",
            "--artifact",
            "/opt/jsh",
            "dev@box.example",
            "--",
            "-J",
            "jump.example",
            "-p2200",
        ];
        assert!(!super::is_jsh_ssh_upgrade_launcher_argv(
            &argv
                .iter()
                .map(|value| value.to_string())
                .collect::<Vec<_>>()
        ));
        let ObservedSshTarget::Target(target) = ssh(&argv) else {
            panic!("expected the terminal launcher to retain its SSH target");
        };
        assert_eq!(target.host, "box.example");
        assert_eq!(target.user.as_deref(), Some("dev"));
        assert_eq!(target.ssh_args, ["-J", "jump.example", "-p", "2200"]);
        assert_eq!(target.deploy, "off");
        assert!(target.session.is_none());
        assert!(target.deploy_artifact.is_none());
    }

    #[test]
    fn observed_jsh_launcher_rejects_ambiguous_or_side_effecting_shapes() {
        for args in [
            &[
                "/bin/sh",
                "/cache/jsh-remote.sh",
                "--persist",
                "host",
                "--",
                "-o",
                "ProxyCommand=sh -c evil",
            ] as &[&str],
            &[
                "/bin/sh",
                "/cache/jsh-remote.sh",
                "--persist",
                "host",
                "second-host",
            ],
            &[
                "/bin/sh",
                "/cache/jsh-remote.sh",
                "--persist",
                "--incognito",
                "host",
            ],
            &[
                "/bin/sh",
                "/cache/jsh-remote.sh",
                "--local-jsh",
                "/opt/jsh",
                "host",
            ],
            &[
                "/bin/sh",
                "/cache/jsh-remote.sh",
                "--persist",
                "--dry-run",
                "host",
            ],
        ] {
            assert!(matches!(ssh(args), ObservedSshTarget::Unsupported(_)));
        }

        assert!(matches!(
            ssh(&[
                "/bin/sh",
                "/cache/jsh-remote.sh",
                "--persist",
                "--docker",
                "container"
            ]),
            ObservedSshTarget::NotSsh
        ));
        assert!(matches!(
            ssh(&["/bin/sh", "/cache/not-jsh-remote.sh", "--persist", "host"]),
            ObservedSshTarget::NotSsh
        ));
    }

    #[test]
    fn observed_ssh_respects_first_user_and_port_like_openssh() {
        let ObservedSshTarget::Target(target) = ssh(&[
            "ssh",
            "-l",
            "alice",
            "bob@example.com",
            "-lcarol",
            "-p2200",
            "-p",
            "22",
        ]) else {
            panic!("expected an observed SSH target");
        };
        assert_eq!(target.user.as_deref(), Some("alice"));
        assert_eq!(target.ssh_args, ["-p", "2200"]);
    }

    #[test]
    fn observed_ssh_keeps_connection_options_and_drops_ui_and_forwarding_flags() {
        let ObservedSshTarget::Target(target) = ssh(&[
            "ssh",
            "-vvCt",
            "-i",
            "/tmp/key with spaces",
            "-Jjump@example.com",
            "-L",
            "8080:localhost:80",
            "-o",
            "StrictHostKeyChecking=no",
            "example.com",
        ]) else {
            panic!("expected an observed SSH target");
        };
        assert_eq!(
            target.ssh_args,
            [
                "-i",
                "/tmp/key with spaces",
                "-J",
                "jump@example.com",
                "-o",
                "StrictHostKeyChecking=no"
            ]
        );
    }

    #[test]
    fn observed_ssh_accepts_only_stable_control_path_operands() {
        const REASON: &str =
            "SSH ControlPath reused by Files must be an absolute path or start with ~/";

        for (args, expected) in [
            (
                &["ssh", "-S", "/tmp/cm-%C", "host"] as &[&str],
                &["-S", "/tmp/cm-%C"] as &[&str],
            ),
            (&["ssh", "-S~/.ssh/cm-%C", "host"], &["-S", "~/.ssh/cm-%C"]),
            (
                &["ssh", "-o", "ControlPath=/tmp/cm-%C", "host"],
                &["-o", "ControlPath=/tmp/cm-%C"],
            ),
            (
                &["ssh", "-ocontrolpath=~/.ssh/cm-%C", "host"],
                &["-o", "controlpath=~/.ssh/cm-%C"],
            ),
        ] {
            let ObservedSshTarget::Target(target) = ssh(args) else {
                panic!("expected an observed SSH target for {args:?}");
            };
            assert_eq!(target.ssh_args, expected, "argv: {args:?}");
        }

        for args in [
            &["ssh", "-S"] as &[&str],
            &["ssh", "-S", "", "host"] as &[&str],
            &["ssh", "-S", "cm-%C", "host"],
            &["ssh", "-S./cm-%C", "host"],
            &["ssh", "-S../cm-%C", "host"],
            &["ssh", "-S~alice/.ssh/cm-%C", "host"],
            &["ssh", "-S~/", "host"],
            &["ssh", "-o", "ControlPath", "host"],
            &["ssh", "-o", "ControlPath=", "host"],
            &["ssh", "-oControlPath=", "host"],
            &["ssh", "-oControlPath=cm-%C", "host"],
            &["ssh", "-o", "controlpath=./cm-%C", "host"],
            &["ssh", "-oControlPath=../cm-%C", "host"],
            &["ssh", "-o", "ControlPath=~alice/.ssh/cm-%C", "host"],
        ] {
            assert_eq!(
                ssh(args),
                ObservedSshTarget::Unsupported(REASON),
                "argv: {args:?}"
            );
        }
    }

    #[test]
    fn observed_jsh_launcher_applies_the_control_path_gate_to_pass_through_args() {
        const REASON: &str =
            "SSH ControlPath reused by Files must be an absolute path or start with ~/";

        let prefix = [
            "/bin/sh",
            "/home/alice/.cache/jsh/jsh-remote.sh",
            "--persist",
            "--local-jsh",
            "/home/alice/.local/bin/jsh",
            "dev@box.example",
            "--",
        ];

        let mut accepted = prefix.to_vec();
        accepted.extend(["-oControlPath=~/.ssh/cm-%C", "-S", "/tmp/cm-override-%C"]);
        let ObservedSshTarget::Target(target) = ssh(&accepted) else {
            panic!("expected absolute and SSH-home ControlPaths to pass through");
        };
        assert_eq!(
            target.ssh_args,
            [
                "-o",
                "ControlPath=~/.ssh/cm-%C",
                "-S",
                "/tmp/cm-override-%C"
            ]
        );

        for rejected_args in [
            ["-S", "relative/cm-%C"].as_slice(),
            ["-oControlPath=~alice/.ssh/cm-%C"].as_slice(),
        ] {
            let mut rejected = prefix.to_vec();
            rejected.extend_from_slice(rejected_args);
            assert_eq!(ssh(&rejected), ObservedSshTarget::Unsupported(REASON));
        }
    }

    #[test]
    fn observed_ssh_rejects_commands_and_side_effecting_options() {
        for args in [
            &["ssh", "host", "uname"] as &[&str],
            &["ssh", "-o", "ProxyCommand=sh -c evil", "host"],
            &["ssh", "-oLocalCommand=touch /tmp/side-effect", "host"],
            &["ssh", "-I", "/tmp/provider.so", "host"],
            &["ssh", "-F", "/tmp/config", "host"],
            &["ssh", "-W", "target:22", "jump"],
            &["ssh", "-N", "host"],
        ] {
            assert!(matches!(ssh(args), ObservedSshTarget::Unsupported(_)));
        }
    }

    #[test]
    fn observed_ssh_handles_option_separator_and_non_ssh_commands() {
        assert!(matches!(
            ssh(&["ssh", "--", "-host"]),
            ObservedSshTarget::Unsupported(_)
        ));
        assert!(matches!(ssh(&["mosh", "host"]), ObservedSshTarget::NotSsh));
        assert!(matches!(
            ssh(&["ssh", "--", "host", "-p", "22"]),
            ObservedSshTarget::Unsupported(_)
        ));
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

    fn toml_host(text: &str) -> super::RemoteHostConfig {
        toml::from_str(text).expect("deserialize host")
    }

    #[test]
    fn a_minimal_serde_entry_is_a_plain_ssh_host() {
        let host = toml_host(r#"host = "build-box""#);
        assert!(host.validate().is_ok());
        assert_eq!(host.display_name(), "build-box");
        let (argv, degraded) = host.tab_argv();
        assert!(degraded.is_none());
        assert_eq!(argv, ["ssh", "-t", "--", "build-box", "jsh"]);
    }

    #[test]
    fn a_serde_container_entry_builds_the_docker_exec() {
        let host = toml_host(
            r#"
host = "myubuntu"
docker = true
user = "devuser"
session = "cloud-1"
"#,
        );
        assert!(host.validate().is_ok());
        let (argv, _) = host.tab_argv();
        assert_eq!(
            argv,
            [
                "docker",
                "exec",
                "-it",
                "-u",
                "devuser",
                "myubuntu",
                "jsh",
                "--session",
                "cloud-1"
            ]
        );
        // user@container would be read as a container name nobody has.
        assert!(!argv.iter().any(|a| a.contains('@')), "{argv:?}");
    }

    #[test]
    fn a_deploying_serde_entry_routes_through_the_launcher() {
        let host = toml_host(
            r#"
host = "myubuntu"
docker = true
deploy = "incognito"
"#,
        );
        assert!(host.validate().is_ok());
        let argv = host.deployed_argv_with_script(Path::new("/c/jsh-remote.sh"));
        assert_eq!(argv[0], "/bin/sh");
        assert_eq!(argv[1], "/c/jsh-remote.sh");
        assert!(argv.contains(&"--incognito".to_string()), "{argv:?}");
        let docker = argv.iter().position(|a| a == "--docker").expect("--docker");
        assert_eq!(argv[docker + 1], "myubuntu");
    }

    #[test]
    fn entries_that_cannot_become_one_argv_token_are_refused() {
        for (text, why) in [
            (r#"host = """#, "empty host"),
            (r#"host = "has space""#, "whitespace in host"),
            (r#"host = "-oProxyCommand=x""#, "host reading as a flag"),
            ("host = \"h\"\nuser = \"a@b\"", "user with @"),
            ("host = \"h\"\nsession = \"a b\"", "unsafe session id"),
            (
                "host = \"h\"\ndeploy = \"incognitoo\"",
                "unknown deploy spelling",
            ),
            (
                "host = \"h\"\ndeploy = \"persist\"\ndeploy_artifact = \"target/jsh\"",
                "relative artifact",
            ),
        ] {
            let host = toml_host(text);
            assert!(host.validate().is_err(), "accepted {why}: {text}");
        }
    }

    #[test]
    fn an_ssh_session_id_travels_validated_and_container_args_drop_ssh_flags() {
        let host = toml_host(
            r#"
host = "box"
user = "yj"
session = "dev-main"
ssh_args = ["-p", "2222"]
"#,
        );
        let (argv, _) = host.tab_argv();
        assert_eq!(
            argv,
            [
                "ssh",
                "-t",
                "-p",
                "2222",
                "--",
                "yj@box",
                "jsh --session dev-main"
            ]
        );

        let container = toml_host(
            r#"
host = "box"
docker = true
deploy = "persist"
ssh_args = ["-p", "2222"]
"#,
        );
        let argv = container.deployed_argv_with_script(Path::new("/c/jsh-remote.sh"));
        assert!(!argv.iter().any(|a| a == "-p"), "{argv:?}");
    }
}
