//! Read-only delivery observations through the existing bounded Git runner.
use super::*;

pub struct RepositoryDeliveryObservation {
    pub manifest: String,
    pub lockfile: String,
    pub remote_head: Option<String>,
    pub target_contains_candidate: bool,
    pub remote_error: Option<String>,
}

pub(super) fn read_only_service() -> Result<WorktreeService, String> {
    Ok(WorktreeService {
        git: trusted_git_path()?,
        managed_root: PathBuf::new(),
        archive_root: PathBuf::new(),
        timeout: Duration::from_secs(5),
        deadline: Some(Instant::now() + Duration::from_secs(15)),
        cancel: None,
    })
}

pub(super) fn pin_repository(
    service: &WorktreeService,
    repository: &Path,
) -> Result<crate::agent_task::pinned_dir::PinnedDirectory, String> {
    validate_absolute_clean_path(repository, "Mission repository")
        .map_err(|error| error.to_string())?;
    let canonical =
        canonical_exact(repository, "Mission repository").map_err(|error| error.to_string())?;
    let root = crate::agent_task::pinned_dir::PinnedDirectory::open(&canonical)
        .map_err(|error| error.to_string())?;
    service
        .reject_external_checkout_filters(&root.proc_path())
        .map_err(|error| error.to_string())?;
    let top = service
        .git_text(
            git_command(
                &service.git,
                &root.proc_path(),
                ["rev-parse", "--show-toplevel"],
            ),
            "Mission repository root",
        )
        .map_err(|error| error.to_string())?;
    if Path::new(&top) != canonical {
        return Err("Select the exact repository top-level for a Mission".into());
    }
    Ok(root)
}

/// Inspect the exact candidate's dependency files and a public GitHub target.
/// No checkout, fetch, credential helper, SSH or repository-configured URL is
/// used. Missing Cargo files are valid for non-Rust repositories; such files
/// cannot satisfy a declared core-pin requirement.
pub fn inspect_repository_delivery(
    repository: &Path,
    full_name: &str,
    candidate: &str,
    target_ref: &str,
) -> Result<RepositoryDeliveryObservation, String> {
    validate_remote_input(full_name, target_ref)?;
    validate_object_id(candidate.to_owned()).map_err(|error| error.to_string())?;
    if candidate.bytes().any(|byte| byte.is_ascii_uppercase()) {
        return Err("Delivery candidates require a full lowercase commit SHA".into());
    }
    let service = read_only_service()?;
    let root = pin_repository(&service, repository)?;
    let path = root.proc_path();
    let head = service
        .resolve_commit(&path, candidate)
        .map_err(|error| error.to_string())?;
    if head != candidate {
        return Err("Delivery candidate is not the exact commit object".into());
    }
    let (manifest, lockfile) = read_dependency_files(&service, &path, &head)?;
    let mut observation = RepositoryDeliveryObservation {
        manifest,
        lockfile,
        remote_head: None,
        target_contains_candidate: false,
        remote_error: None,
    };
    match observe_github_ref(&service, full_name, target_ref) {
        Ok(remote_head) => {
            match verify_ancestry(&service, &path, &head, &remote_head) {
                Ok(contains) => observation.target_contains_candidate = contains,
                Err(error) => observation.remote_error = Some(error),
            }
            observation.remote_head = Some(remote_head);
        }
        Err(error) => observation.remote_error = Some(error),
    }
    Ok(observation)
}

fn read_dependency_files(
    service: &WorktreeService,
    repository: &Path,
    head: &str,
) -> Result<(String, String), String> {
    let command = git_command(
        &service.git,
        repository,
        [
            "ls-tree",
            "-z",
            "--full-tree",
            head,
            "--",
            "Cargo.toml",
            "Cargo.lock",
        ],
    );
    let output = service
        .run(command, "candidate dependency entries", 16 * 1024)
        .and_then(|output| service.require_success("candidate dependency entries", output))
        .map_err(|error| error.to_string())?;
    if output.stdout.truncated {
        return Err("Candidate dependency entries exceed the evidence budget".into());
    }
    let entries = String::from_utf8(output.stdout.bytes)
        .map_err(|_| "Candidate dependency entries are not UTF-8")?;
    let mut manifest = None;
    let mut lockfile = None;
    for entry in entries.split('\0').filter(|entry| !entry.is_empty()) {
        let (metadata, name) = entry
            .split_once('\t')
            .ok_or("Malformed dependency tree entry")?;
        let fields: Vec<_> = metadata.split(' ').collect();
        if fields.len() != 3 || !matches!(fields[0], "100644" | "100755") || fields[1] != "blob" {
            return Err(
                "Dependency evidence requires committed regular files, not symlinks or submodules"
                    .into(),
            );
        }
        validate_object_id(fields[2].to_owned()).map_err(|error| error.to_string())?;
        let slot = match name {
            "Cargo.toml" => &mut manifest,
            "Cargo.lock" => &mut lockfile,
            _ => return Err("Unexpected dependency tree entry".into()),
        };
        if slot.is_some() {
            return Err("Duplicate dependency tree entry".into());
        }
        // Read the raw blob; no worktree path, textconv, checkout filter or
        // display formatting can change these committed bytes.
        let command = git_command(&service.git, repository, ["cat-file", "blob", fields[2]]);
        let output = service
            .run(command, "candidate dependency blob", MAX_GIT_STDOUT_BYTES)
            .and_then(|output| service.require_success("candidate dependency blob", output))
            .map_err(|error| error.to_string())?;
        if output.stdout.truncated {
            return Err("Candidate dependency manifest exceeds the evidence budget".into());
        }
        *slot = Some(
            String::from_utf8(output.stdout.bytes)
                .map_err(|_| "Candidate dependency manifest is not UTF-8")?,
        );
    }
    Ok((manifest.unwrap_or_default(), lockfile.unwrap_or_default()))
}

fn verify_ancestry(
    service: &WorktreeService,
    repository: &Path,
    candidate: &str,
    remote_head: &str,
) -> Result<bool, String> {
    if candidate == remote_head {
        return Ok(true);
    }
    let mut command = git_command(
        &service.git,
        repository,
        ["merge-base", "--is-ancestor", candidate, remote_head],
    );
    // Grafts are independent from replace refs. Neither may alter remote
    // reachability. Missing objects are not fetched and never imply ancestry.
    command
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_GRAFT_FILE", NULL_DEVICE);
    let output = service
        .run(command, "candidate ancestry", 1024)
        .map_err(|_| {
            "Remote target was observed, but local candidate ancestry could not be checked"
        })?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err("Remote target was observed, but its ancestry is unavailable in local Git objects; no fetch was performed".into()),
    }
}

fn validate_remote_input(full_name: &str, target_ref: &str) -> Result<(), String> {
    let parts: Vec<_> = full_name.split('/').collect();
    if full_name.len() > 256
        || parts.len() != 2
        || parts.iter().any(|part| {
            part.is_empty()
                || *part == "."
                || *part == ".."
                || !part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
        })
    {
        return Err("Remote delivery observation requires an exact GitHub owner/repository".into());
    }
    let branch = target_ref
        .strip_prefix("refs/heads/")
        .ok_or("Remote delivery observation requires an exact refs/heads/... branch")?;
    validate_branch_text(branch).map_err(|error| error.to_string())
}

struct RemoteDirectory(PathBuf);

impl RemoteDirectory {
    fn create_in(parent: &Path) -> Result<Self, String> {
        let path = parent.join(format!("jterm-mission-remote-{}", uuid::Uuid::new_v4()));
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&path).map_err(|error| error.to_string())?;
        let mut directory = Self(path);
        directory.0 = fs::canonicalize(&directory.0).map_err(|error| error.to_string())?;
        Ok(directory)
    }
}

impl Drop for RemoteDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn isolated_remote_command(service: &WorktreeService, directory: &Path) -> Result<Command, String> {
    let ceiling = directory
        .parent()
        .ok_or("Remote inspection directory has no parent")?;
    let mut command = base_git_command(&service.git, directory);
    // These settings apply only to this disposable child. In particular,
    // clearing HOME alone lets libcurl fall back to the user's passwd home
    // and read .netrc; an explicit empty home prevents that credential read.
    // Git's ceiling must be the parent: using the cwd itself does not prevent
    // repository discovery above the cwd.
    command
        .env("HOME", directory)
        .env("XDG_CONFIG_HOME", directory)
        .env("CURL_HOME", directory)
        .env("GIT_CEILING_DIRECTORIES", ceiling)
        .args([
            "-c",
            "protocol.allow=never",
            "-c",
            "protocol.https.allow=always",
            "-c",
            "http.followRedirects=false",
            "-c",
            "http.sslVerify=true",
        ]);
    Ok(command)
}

fn observe_github_ref(
    service: &WorktreeService,
    full_name: &str,
    target_ref: &str,
) -> Result<String, String> {
    validate_remote_input(full_name, target_ref)?;
    let directory = RemoteDirectory::create_in(&std::env::temp_dir())?;
    let mut command = isolated_remote_command(service, &directory.0)?;
    command.args([
        "ls-remote",
        "--refs",
        "--exit-code",
        &format!("https://github.com/{full_name}.git"),
        target_ref,
    ]);
    let output = service
        .run(command, "remote target observation", 16 * 1024)
        .and_then(|output| service.require_success("remote target observation", output))
        .map_err(|error| {
            format!("Remote target is unverified (offline, private or unavailable): {error}")
        })?;
    if output.stdout.truncated {
        return Err("Remote target reply exceeds the evidence budget".into());
    }
    parse_remote_ref(&output.stdout.bytes, target_ref)
}

fn parse_remote_ref(bytes: &[u8], target_ref: &str) -> Result<String, String> {
    let value = std::str::from_utf8(bytes).map_err(|_| "Remote target reply is not UTF-8")?;
    let mut rows = value.lines();
    let line = rows.next().ok_or("Remote target did not return a ref")?;
    let (sha, reference) = line
        .split_once('\t')
        .ok_or("Malformed remote target reply")?;
    if rows.next().is_some()
        || reference != target_ref
        || !matches!(sha.len(), 40 | 64)
        || !sha.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("Ambiguous remote target reply".into());
    }
    Ok(sha.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        root: PathBuf,
        repo: PathBuf,
        service: WorktreeService,
    }

    impl Fixture {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("jterm-delivery-test-{}", uuid::Uuid::new_v4()));
            let repo = root.join("repo");
            fs::create_dir_all(&repo).unwrap();
            let mut service = read_only_service().unwrap();
            // Unit fixtures test explicit deadlines separately, without a
            // whole-suite wall-clock assumption under parallel CI load.
            service.deadline = None;
            let fixture = Self {
                root,
                repo,
                service,
            };
            fixture.git(&["init", "-b", "master"]);
            fs::write(fixture.repo.join("README"), "fixture\n").unwrap();
            fixture.commit();
            fixture
        }

        fn git(&self, args: &[&str]) -> String {
            let mut command = base_git_command(&self.service.git, &self.repo);
            command
                .args(args)
                .env("GIT_AUTHOR_NAME", "Fixture")
                .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
                .env("GIT_COMMITTER_NAME", "Fixture")
                .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid");
            let output = self
                .service
                .run(command, "delivery fixture", 64 * 1024)
                .unwrap();
            let output = self
                .service
                .require_success("delivery fixture", output)
                .unwrap();
            String::from_utf8(output.stdout.bytes)
                .unwrap()
                .trim_end()
                .into()
        }

        fn commit(&self) {
            self.git(&["add", "."]);
            self.git(&["commit", "-m", "fixture"]);
        }
        fn head(&self) -> String {
            self.git(&["rev-parse", "HEAD"])
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn raw_multiline_manifests_survive_pinned_directory_rename() {
        let fixture = Fixture::new();
        let manifest = "[package]\nname = 'fixture'\nversion = '0.1.0'\n\n[dependencies]\n";
        let lockfile = "version = 3\n\n[[package]]\nname = 'fixture'\nversion = '0.1.0'\n";
        fs::write(fixture.repo.join("Cargo.toml"), manifest).unwrap();
        fs::write(fixture.repo.join("Cargo.lock"), lockfile).unwrap();
        fixture.commit();
        let head = fixture.head();
        let pinned = pin_repository(&fixture.service, &fixture.repo).unwrap();
        fs::rename(&fixture.repo, fixture.root.join("moved")).unwrap();
        fs::create_dir(&fixture.repo).unwrap();
        fs::write(fixture.repo.join("Cargo.toml"), "replacement").unwrap();
        assert_eq!(
            read_dependency_files(&fixture.service, &pinned.proc_path(), &head).unwrap(),
            (manifest.into(), lockfile.into())
        );
    }

    #[test]
    fn absent_cargo_files_are_not_fabricated_pin_evidence() {
        let fixture = Fixture::new();
        let pinned = pin_repository(&fixture.service, &fixture.repo).unwrap();
        assert_eq!(
            read_dependency_files(&fixture.service, &pinned.proc_path(), &fixture.head()).unwrap(),
            (String::new(), String::new())
        );
    }

    #[cfg(unix)]
    #[test]
    fn committed_symlink_is_not_a_dependency_manifest() {
        let fixture = Fixture::new();
        std::os::unix::fs::symlink("README", fixture.repo.join("Cargo.toml")).unwrap();
        fixture.commit();
        let pinned = pin_repository(&fixture.service, &fixture.repo).unwrap();
        assert!(
            read_dependency_files(&fixture.service, &pinned.proc_path(), &fixture.head()).is_err()
        );
    }

    #[test]
    fn remote_process_cannot_discover_parent_repository_configuration() {
        let fixture = Fixture::new();
        fixture.git(&["config", "mission.fixture", "parent-config"]);
        let directory = RemoteDirectory::create_in(&fixture.repo).unwrap();
        let mut command = isolated_remote_command(&fixture.service, &directory.0).unwrap();
        for name in ["HOME", "XDG_CONFIG_HOME", "CURL_HOME"] {
            let value = command
                .get_envs()
                .find(|(key, _)| *key == OsStr::new(name))
                .unwrap()
                .1;
            assert_eq!(value, Some(directory.0.as_os_str()));
        }
        command.args(["config", "--get", "mission.fixture"]);
        let output = fixture
            .service
            .run(command, "isolated configuration", 1024)
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.bytes.is_empty());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&directory.0).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        let path = directory.0.clone();
        drop(directory);
        assert!(!path.exists());
    }

    #[test]
    fn replacement_and_grafts_cannot_forge_remote_candidate_ancestry() {
        let fixture = Fixture::new();
        let ancestor = fixture.head();
        let tree = fixture.git(&["rev-parse", "HEAD^{tree}"]);
        let unrelated = fixture.git(&["commit-tree", &tree, "-m", "independent root"]);
        let descendant = fixture.git(&["commit-tree", &tree, "-p", &ancestor, "-m", "descendant"]);
        let pinned = pin_repository(&fixture.service, &fixture.repo).unwrap();
        assert!(verify_ancestry(
            &fixture.service,
            &pinned.proc_path(),
            &ancestor,
            &descendant
        )
        .unwrap());
        assert!(
            !verify_ancestry(&fixture.service, &pinned.proc_path(), &ancestor, &unrelated).unwrap()
        );
        fs::write(
            fixture.repo.join(".git/info/grafts"),
            format!("{unrelated} {ancestor}\n"),
        )
        .unwrap();
        fixture.git(&["replace", &unrelated, &descendant]);
        assert!(
            !verify_ancestry(&fixture.service, &pinned.proc_path(), &ancestor, &unrelated).unwrap()
        );
        assert!(verify_ancestry(
            &fixture.service,
            &pinned.proc_path(),
            &ancestor,
            &"0".repeat(40)
        )
        .is_err());
    }

    #[test]
    fn exact_repo_root_and_safe_fixed_remote_input_are_required() {
        let fixture = Fixture::new();
        let nested = fixture.repo.join("nested");
        fs::create_dir(&nested).unwrap();
        assert!(pin_repository(&fixture.service, &nested).is_err());
        assert!(validate_remote_input("beamiter/jterm_core", "refs/heads/master").is_ok());
        for repository in [
            "https://github.com/beamiter/jterm_core",
            "../other",
            "user/repo/extra",
            "user@github.com/repo",
        ] {
            assert!(validate_remote_input(repository, "refs/heads/master").is_err());
        }
        for reference in [
            "master",
            "refs/heads/",
            "refs/heads/master *",
            "refs/heads/../master",
        ] {
            assert!(validate_remote_input("beamiter/jterm_core", reference).is_err());
        }
        assert!(inspect_repository_delivery(
            &fixture.repo,
            "beamiter/jterm_core",
            "HEAD",
            "refs/heads/master"
        )
        .is_err());
    }

    #[test]
    fn remote_reply_requires_one_exact_ref_and_full_commit() {
        let sha = "a".repeat(40);
        assert_eq!(
            parse_remote_ref(
                format!("{sha}\trefs/heads/master\n").as_bytes(),
                "refs/heads/master"
            )
            .unwrap(),
            sha
        );
        for response in [
            format!("{sha}\trefs/heads/other\n"),
            format!("{sha}\trefs/heads/master\n{sha}\trefs/heads/other\n"),
            "abc\trefs/heads/master\n".into(),
        ] {
            assert!(parse_remote_ref(response.as_bytes(), "refs/heads/master").is_err());
        }
    }

    #[test]
    fn inspection_uses_an_absolute_deadline_in_addition_to_each_command_limit() {
        let mut service = read_only_service().unwrap();
        assert!(service.deadline.is_some());
        service.deadline = Some(Instant::now() - Duration::from_secs(1));
        let command = git_command(&service.git, &std::env::temp_dir(), ["--version"]);
        assert!(service
            .run(command, "expired delivery budget", 1024)
            .is_err());
    }
}
