//! Resolve CLI installations without sourcing an interactive shell profile.
use super::{valid_id, SessionLaunch};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

const MAX_PATH_DIRS: usize = 128;
const MAX_NVM_VERSIONS: usize = 128;
const MAX_PATH_BYTES: usize = 32 * 1024;
struct ProviderCommand {
    name: &'static str,
    arguments: Vec<String>,
    assignments: Vec<String>,
    explicit_path: Option<PathBuf>,
}

impl SessionLaunch {
    /// Resolve only when the user activates a session. Desktop applications
    /// often inherit a minimal PATH without the user's npm/NVM installations.
    /// No profile, shell expression, or executable installer is evaluated.
    pub fn resolve_for_spawn(&self) -> Result<Self, String> {
        let home =
            dirs::home_dir().ok_or("Cannot locate your home directory to find the agent CLI.")?;
        self.resolve_with_environment(&home, std::env::var_os("PATH").as_deref())
    }

    fn resolve_with_environment(
        &self,
        home: &Path,
        inherited_path: Option<&OsStr>,
    ) -> Result<Self, String> {
        let ProviderCommand {
            name: provider,
            arguments,
            assignments,
            explicit_path,
        } = self.provider_command()?;
        let directories = search_directories(home, inherited_path);
        let executable = explicit_path.into_iter().chain(directories.iter().map(|directory| directory.join(provider)))
            .find(|path| executable_file(path)).ok_or_else(|| format!(
                "{provider} was not found in the desktop PATH or common user installations (including ~/.local/bin and ~/.nvm/versions/node/*/bin). Install {provider} or add its absolute bin directory to PATH, then retry."
            ))?;
        let bin = executable
            .parent()
            .ok_or("The agent executable has no installation directory.")?
            .to_path_buf();
        let mut child_dirs = Vec::new();
        let associated_bin = associated_node_bin(&executable);
        // NPM CLI shebangs use /usr/bin/env node. Prefer the runtime installed
        // alongside the CLI, then the remaining explicit/user installation paths.
        let node = associated_bin
            .iter()
            .chain(std::iter::once(&bin))
            .chain(directories.iter())
            .map(|directory| directory.join("node"))
            .find(|path| executable_file(path));
        if requires_env_node(&executable) && node.is_none() {
            return Err(format!("Found {provider} at {}, but its Node.js interpreter was not found. Install Node.js in that bin directory or add its absolute directory to PATH.", executable.display()));
        }
        if let Some(directory) = node.as_ref().and_then(|path| path.parent()) {
            push_directory(&mut child_dirs, directory.to_path_buf());
        }
        push_directory(&mut child_dirs, bin);
        let env = ["/usr/bin/env", "/bin/env", "/run/current-system/sw/bin/env"]
            .into_iter()
            .map(PathBuf::from)
            .chain(directories.iter().map(|directory| directory.join("env")))
            .find(|path| executable_file(path))
            .ok_or("Cannot find the system env executable needed to launch the agent.")?;
        for directory in directories {
            push_directory(&mut child_dirs, directory);
        }
        let path = std::env::join_paths(&child_dirs).map_err(|_| {
            "The agent installation directory cannot be represented in PATH.".to_owned()
        })?;
        let path = path.to_str().ok_or("The agent PATH is not valid UTF-8.")?;
        let executable = executable
            .to_str()
            .ok_or("The agent executable path is not valid UTF-8.")?;
        let mut args = vec![format!("PATH={path}")];
        args.extend(assignments);
        args.push(executable.to_owned());
        args.extend(arguments);
        Ok(Self {
            program: env
                .to_str()
                .ok_or("The env executable path is not valid UTF-8.")?
                .into(),
            args,
            cwd: self.cwd.clone(),
        })
    }

    fn provider_command(&self) -> Result<ProviderCommand, String> {
        let mut assignments = Vec::new();
        let (program, arguments) = if matches!(
            self.program.as_str(),
            "env" | "/usr/bin/env" | "/bin/env" | "/run/current-system/sw/bin/env"
        ) {
            let mut start = 0;
            while let Some(argument) = self.args.get(start) {
                if let Some((name, value)) = argument.split_once('=') {
                    if !matches!(name, "CODEX_HOME" | "CLAUDE_CONFIG_DIR" | "PATH")
                        || crate::review_input::validate(value).is_err()
                    {
                        return Err("The saved agent launch contains an unsupported environment assignment.".into());
                    }
                    // A second resolution rebuilds PATH from current evidence;
                    // original provider-store assignments are kept byte-for-byte.
                    if name != "PATH" {
                        assignments.push(argument.clone());
                    }
                    start += 1;
                } else {
                    break;
                }
            }
            let program = self
                .args
                .get(start)
                .ok_or("The saved agent launch has no provider command.")?;
            (program.as_str(), &self.args[start + 1..])
        } else {
            (self.program.as_str(), self.args.as_slice())
        };
        let name = Path::new(program).file_name().and_then(OsStr::to_str);
        let (provider, flag) = match name {
            Some("codex") => ("codex", "resume"),
            Some("claude") => ("claude", "--resume"),
            _ => {
                return Err(
                    "The saved launch is not a Codex or Claude session resume command.".into(),
                )
            }
        };
        if arguments.len() != 2 || arguments[0] != flag || !valid_id(&arguments[1]) {
            return Err("The saved agent resume command has an invalid session identity.".into());
        }
        Ok(ProviderCommand {
            name: provider,
            arguments: arguments.to_vec(),
            assignments,
            explicit_path: Path::new(program)
                .is_absolute()
                .then(|| PathBuf::from(program)),
        })
    }
}

fn usable_directory(path: &Path) -> bool {
    path.is_absolute()
        && path
            .to_str()
            .is_some_and(|text| !text.contains(':') && crate::review_input::validate(text).is_ok())
}
fn push_directory(directories: &mut Vec<PathBuf>, directory: PathBuf) {
    if directories.len() < MAX_PATH_DIRS
        && usable_directory(&directory)
        && directory.is_dir()
        && !directories.contains(&directory)
    {
        directories.push(directory);
    }
}
fn search_directories(home: &Path, inherited_path: Option<&OsStr>) -> Vec<PathBuf> {
    let mut directories = Vec::new();
    if let Some(path) = inherited_path.filter(|value| value.len() <= MAX_PATH_BYTES) {
        // Relative/empty PATH entries would depend on the resumed project's cwd.
        for directory in std::env::split_paths(path).take(MAX_PATH_DIRS / 2) {
            push_directory(&mut directories, directory);
        }
    }
    for relative in [
        ".local/bin",
        ".npm-global/bin",
        ".volta/bin",
        ".asdf/shims",
        ".local/share/mise/shims",
        ".bun/bin",
        ".cargo/bin",
    ] {
        push_directory(&mut directories, home.join(relative));
    }
    let nvm_limit = MAX_PATH_DIRS.saturating_sub(directories.len() + 4);
    for directory in nvm_directories(home).into_iter().take(nvm_limit) {
        push_directory(&mut directories, directory);
    }
    for directory in [
        "/usr/local/bin",
        "/usr/bin",
        "/bin",
        "/run/current-system/sw/bin",
    ] {
        push_directory(&mut directories, PathBuf::from(directory));
    }
    directories
}
fn nvm_directories(home: &Path) -> Vec<PathBuf> {
    let nvm = home.join(".nvm");
    let Ok(reader) = fs::read_dir(nvm.join("versions/node")) else {
        return vec![];
    };
    let mut versions = reader
        .take(MAX_NVM_VERSIONS)
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let version = parse_version(name.to_str()?)?;
            let directory = entry.path().join("bin");
            (usable_directory(&directory) && directory.is_dir()).then_some((version, directory))
        })
        .collect::<Vec<_>>();
    let default = nvm_default(&nvm);
    let preferred = |version: &[u64; 3]| {
        if let Some(prefix) = default.as_deref().and_then(version_prefix) {
            version.starts_with(&prefix)
        } else {
            default
                .as_deref()
                .is_some_and(|name| name.starts_with("lts/"))
                && version[0] % 2 == 0
        }
    };
    versions.sort_by(|(a, _), (b, _)| preferred(b).cmp(&preferred(a)).then_with(|| b.cmp(a)));
    versions
        .into_iter()
        .map(|(_, directory)| directory)
        .collect()
}
fn nvm_default(nvm: &Path) -> Option<String> {
    let mut name = "default".to_owned();
    for _ in 0..8 {
        if name.len() > 128
            || name.starts_with('/')
            || name
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..")
            || !name.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'/' | b'-' | b'_' | b'*')
            })
        {
            return None;
        }
        let Ok(file) = File::open(nvm.join("alias").join(&name)) else {
            return Some(name);
        };
        let mut bytes = Vec::new();
        if file.take(129).read_to_end(&mut bytes).is_err() || bytes.len() > 128 {
            return None;
        }
        name = std::str::from_utf8(&bytes).ok()?.trim().to_owned();
        if version_prefix(&name).is_some() {
            return Some(name);
        }
    }
    None
}
fn version_prefix(name: &str) -> Option<Vec<u64>> {
    let name = name.strip_prefix('v').unwrap_or(name);
    let pieces = name
        .split('.')
        .map(str::parse::<u64>)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    (!pieces.is_empty() && pieces.len() <= 3).then_some(pieces)
}
fn parse_version(name: &str) -> Option<[u64; 3]> {
    let parts = version_prefix(name)?;
    (parts.len() == 3).then(|| [parts[0], parts[1], parts[2]])
}
#[cfg(unix)]
fn executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    usable_directory(path)
        && fs::metadata(path)
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}
#[cfg(not(unix))]
fn executable_file(path: &Path) -> bool {
    usable_directory(path) && path.is_file()
}
fn associated_node_bin(executable: &Path) -> Option<PathBuf> {
    let target = fs::canonicalize(executable).ok()?;
    for ancestor in target.ancestors().take(16) {
        if ancestor.file_name() == Some(OsStr::new("node_modules")) {
            let lib = ancestor.parent()?;
            if lib.file_name() == Some(OsStr::new("lib")) {
                let bin = lib.parent()?.join("bin");
                if executable_file(&bin.join("node")) {
                    return Some(bin);
                }
            }
        }
    }
    None
}
fn requires_env_node(path: &Path) -> bool {
    let Ok(file) = File::open(path) else {
        return false;
    };
    let mut bytes = Vec::new();
    if file.take(512).read_to_end(&mut bytes).is_err() {
        return false;
    }
    let line = bytes
        .split(|byte| *byte == b'\n')
        .next()
        .unwrap_or_default();
    let Some(line) = line.strip_prefix(b"#!") else {
        return false;
    };
    let tokens = line
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    tokens.first() == Some(&b"/usr/bin/env".as_slice())
        && (tokens.get(1) == Some(&b"node".as_slice())
            || (tokens.get(1) == Some(&b"-S".as_slice())
                && tokens.get(2) == Some(&b"node".as_slice())))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};
    const ID: &str = "11111111-1111-4111-8111-111111111111";
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("agent-desktop-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(path.join("project space")).unwrap();
            Self(path)
        }
        fn executable(&self, relative: &str, body: &str) -> PathBuf {
            let path = self.0.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, body).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            path
        }
        fn launch(&self, provider: &str) -> SessionLaunch {
            SessionLaunch {
                program: provider.into(),
                args: vec![
                    if provider == "codex" {
                        "resume"
                    } else {
                        "--resume"
                    }
                    .into(),
                    ID.into(),
                ],
                cwd: self.0.join("project space"),
            }
        }
        fn nvm(&self, provider: &str, version: &str) -> PathBuf {
            self.executable(&format!(".nvm/versions/node/{version}/bin/node"),"#!/bin/sh\nprintf 'node:%s\\n' \"$0\"\nprintf 'cwd:%s\\n' \"$PWD\"\nprintf 'codex:%s\\n' \"${CODEX_HOME-}\"\nprintf 'claude:%s\\n' \"${CLAUDE_CONFIG_DIR-}\"\nprintf 'arg:%s\\n' \"$@\"\n");
            self.executable(
                &format!(".nvm/versions/node/{version}/bin/{provider}"),
                "#!/usr/bin/env node\n// fixture handled by synthetic node interpreter\n",
            )
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn minimal_desktop_path_resumes_both_nvm_providers_with_their_node() {
        let f = Fixture::new();
        for provider in ["codex", "claude"] {
            let cli = f.nvm(provider, "v22.23.1");
            let mut launch = f.launch(provider);
            let variable = if provider == "codex" {
                "CODEX_HOME"
            } else {
                "CLAUDE_CONFIG_DIR"
            };
            let store = f.0.join("original home space");
            launch.program = "env".into();
            launch.args = vec![
                format!("{variable}={}", store.display()),
                provider.into(),
                launch.args[0].clone(),
                ID.into(),
            ];
            let resolved = launch
                .resolve_with_environment(&f.0, Some(OsStr::new("/usr/bin:/bin")))
                .unwrap();
            assert!(Path::new(&resolved.program).is_absolute());
            assert!(
                resolved.args[0].starts_with(&format!("PATH={}", cli.parent().unwrap().display()))
            );
            assert!(resolved
                .args
                .contains(&format!("{variable}={}", store.display())));
            let output = std::process::Command::new(&resolved.program)
                .args(&resolved.args)
                .env_clear()
                .current_dir(&resolved.cwd)
                .output()
                .unwrap();
            assert!(output.status.success());
            let stdout = String::from_utf8(output.stdout).unwrap();
            assert!(stdout.contains(&format!("node:{}/node", cli.parent().unwrap().display())));
            assert!(stdout.contains(&format!("cwd:{}", launch.cwd.display())));
            assert!(stdout.contains(&format!("arg:{}", cli.display())));
            assert!(stdout.contains(&format!("arg:{ID}")));
            assert!(stdout.contains(&store.to_string_lossy().into_owned()));
        }
    }
    #[test]
    fn inherited_absolute_path_has_priority_and_ignores_relative_entries() {
        let f = Fixture::new();
        let preferred = f.executable("chosen/codex", "#!/bin/sh\nexit 0\n");
        f.nvm("codex", "v22.23.1");
        let path = format!(
            ":.:relative:{}:/usr/bin:/bin",
            preferred.parent().unwrap().display()
        );
        let resolved = f
            .launch("codex")
            .resolve_with_environment(&f.0, Some(OsStr::new(&path)))
            .unwrap();
        assert_eq!(resolved.args[1], preferred.to_string_lossy());
        let dirs =
            std::env::split_paths(OsStr::new(resolved.args[0].strip_prefix("PATH=").unwrap()))
                .collect::<Vec<_>>();
        assert!(dirs.iter().all(|p| p.is_absolute()));
    }
    #[test]
    fn nvm_default_major_wins_then_highest_numeric_matching_version() {
        let f = Fixture::new();
        for version in ["v22.9.0", "v24.1.0", "v22.23.1"] {
            f.nvm("codex", version);
        }
        fs::create_dir_all(f.0.join(".nvm/alias")).unwrap();
        fs::write(f.0.join(".nvm/alias/default"), "22\n").unwrap();
        let resolved = f
            .launch("codex")
            .resolve_with_environment(&f.0, Some(OsStr::new("/usr/bin:/bin")))
            .unwrap();
        assert!(resolved.args[1].ends_with("v22.23.1/bin/codex"));
    }
    #[test]
    fn local_symlink_into_nvm_keeps_cli_path_and_matching_node_runtime() {
        let f = Fixture::new();
        let node = f.nvm("codex", "v22.23.1").parent().unwrap().join("node");
        let target = f.executable(
            ".nvm/versions/node/v22.23.1/lib/node_modules/@openai/codex/bin/codex.js",
            "#!/usr/bin/env node\n",
        );
        fs::create_dir_all(f.0.join(".local/bin")).unwrap();
        let cli = f.0.join(".local/bin/codex");
        symlink(target, &cli).unwrap();
        f.executable(".local/bin/node", "#!/bin/sh\nexit 77\n");
        let resolved = f
            .launch("codex")
            .resolve_with_environment(&f.0, Some(OsStr::new("/usr/bin:/bin")))
            .unwrap();
        assert_eq!(resolved.args[1], cli.to_string_lossy());
        assert!(resolved.args[0].starts_with(&format!("PATH={}", node.parent().unwrap().display())));
        let output = std::process::Command::new(&resolved.program)
            .args(&resolved.args)
            .env_clear()
            .current_dir(&resolved.cwd)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8(output.stdout)
            .unwrap()
            .contains(&format!("node:{}", node.display())));
    }
    #[test]
    fn missing_cli_has_actionable_diagnostic_and_nonexecutables_are_skipped() {
        let f = Fixture::new();
        let path = f.executable(".local/bin/codex", "#!/bin/sh\n");
        fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
        let error = f
            .launch("codex")
            .resolve_with_environment(&f.0, Some(OsStr::new("relative")))
            .unwrap_err();
        assert!(error.contains("codex was not found"));
        assert!(error.contains("~/.nvm/versions/node"));
    }
    #[test]
    fn malformed_resume_or_unexpected_env_assignment_is_rejected() {
        let f = Fixture::new();
        let mut launch = f.launch("codex");
        launch.args[1] = "--last".into();
        assert!(launch.resolve_with_environment(&f.0, None).is_err());
        launch.program = "env".into();
        launch.args = vec![
            "LD_PRELOAD=/unrelated.so".into(),
            "codex".into(),
            "resume".into(),
            ID.into(),
        ];
        assert!(launch.resolve_with_environment(&f.0, None).is_err());
    }
}
