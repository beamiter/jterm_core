#!/usr/bin/env python3
"""Record immutable family build inputs and check a common-core candidate."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tomllib


FAMILY_ROOT = Path(__file__).resolve().parents[2]
RELEASE_SCHEMA = "jterm-family-release/v1"
CORE_URL = "https://github.com/beamiter/jterm_core"
SHA = re.compile(r"[0-9a-f]{40}")
CORE_PACKAGES = {"jterm_core", "jterm_engine", "jterm_runtime", "jterm_protocol",
                 "jterm_gtk_view", "jterm_remote_client"}
APP_LOCKS = {
    "aether": {
        "session/Cargo.lock": {"jterm_engine"},
        "bridge/Cargo.lock": {"jterm_engine"},
        "src-tauri/Cargo.lock": {"jterm_engine"},
    },
    "anvil": {"Cargo.lock": {"jterm_core"}},
    "ember": {"Cargo.lock": {"jterm_core", "jterm_engine"}},
    "forge": {"Cargo.lock": {"jterm_core"}},
    "frost": {"Cargo.lock": {"jterm_core", "jterm_engine"}},
}


def git(repository, *arguments):
    return subprocess.run(
        ["git", "-C", str(repository), *arguments],
        check=True,
        capture_output=True,
        env=dict(os.environ, GIT_NO_LAZY_FETCH="1", GIT_NO_REPLACE_OBJECTS="1", GIT_TERMINAL_PROMPT="0"),
        timeout=20,
    ).stdout


def dependency_tables(manifest):
    for tables in [manifest, *manifest.get("target", {}).values()]:
        for section in ("dependencies", "dev-dependencies", "build-dependencies"):
            yield from tables.get(section, {}).items()


def app_core_pins(manifest, lock, required, label):
    pins = {}
    for package in lock["package"]:
        name = package["name"]
        if name not in CORE_PACKAGES:
            continue
        source = package.get("source", "")
        match = re.fullmatch(
            rf"git\+{re.escape(CORE_URL)}(?:\.git)?\?rev=({SHA.pattern})#\1", source
        )
        if not match or name in pins:
            raise ValueError(f"{label}: {name} must resolve to one full Git pin at {CORE_URL}")
        pins[name] = match[1]

    missing = required - pins.keys()
    if missing:
        raise ValueError(f"{label}: missing core packages {', '.join(sorted(missing))}")
    if len(set(pins.values())) != 1:
        raise ValueError(f"{label}: the core packages resolve to different commits")

    for name, dependency in dependency_tables(manifest):
        if isinstance(dependency, dict):
            name = dependency.get("package", name)
        if name in CORE_PACKAGES and (
            not isinstance(dependency, dict)
            or dependency.get("git", "").removesuffix(".git") != CORE_URL
            or dependency.get("rev") != pins.get(name)
            or name not in pins
            or "path" in dependency
        ):
            raise ValueError(f"{label}: {name} manifest dependency disagrees with its Git pin")

    for patch in manifest.get("patch", {}).values():
        for name, dependency in patch.items():
            package = dependency.get("package", name) if isinstance(dependency, dict) else name
            if name in CORE_PACKAGES or package in CORE_PACKAGES:
                raise ValueError(f"{label}: manifest patch overrides {name}")

    return pins


def snapshot(root, commits=None):
    repositories = {}
    names = [*APP_LOCKS, "jterm_core"]
    if commits is not None and set(commits) != set(names):
        raise ValueError("record must identify all six repositories")
    for name in names:
        repository = root / name
        commit = git(repository, "rev-parse", "HEAD").decode().strip() if commits is None else commits[name]
        if not SHA.fullmatch(commit) or git(repository, "cat-file", "-t", commit) != b"commit\n":
            raise ValueError(f"{name}: expected a local immutable commit")
        paths = git(repository, "ls-tree", "-rz", "--name-only", commit).split(b"\0")
        locks = {
            path.decode(): git(repository, "cat-file", "blob", f"{commit}:{path.decode()}")
            for path in paths if path == b"Cargo.lock" or path.endswith(b"/Cargo.lock")
        }
        required_locks = APP_LOCKS.get(name, {
            "Cargo.lock": set(),
            "crates/jterm_engine/Cargo.lock": set(),
        })
        missing = required_locks.keys() - locks.keys()
        if missing:
            raise ValueError(f"{name}: missing lockfiles {', '.join(sorted(missing))}")
        pins = {}
        for path, required in APP_LOCKS.get(name, {}).items():
            manifest = str(Path(path).with_name("Cargo.toml"))
            manifest_data = git(repository, "cat-file", "blob", f"{commit}:{manifest}")
            pins[path] = app_core_pins(
                tomllib.loads(manifest_data.decode()), tomllib.loads(locks[path].decode()),
                required, f"{name}/{path}",
            )
        repositories[name] = {
            "commit": commit,
            "lock_sha256": {path: hashlib.sha256(data).hexdigest() for path, data in locks.items()},
            "app_core_pins": pins,
        }

    core = repositories["jterm_core"]["commit"]
    mismatches = {
        f"{name}/{path}": next(iter(pins.values()))
        for name, repo in repositories.items()
        for path, pins in repo["app_core_pins"].items()
        if set(pins.values()) != {core}
    }
    return {
        "schema": 1,
        "repositories": repositories,
        "common_core_mismatches": mismatches,
    }


def object_id(value, label):
    if not isinstance(value, str) or not SHA.fullmatch(value):
        raise ValueError(f"{label}: expected a full lowercase Git object ID")
    return value


def source_entry(row, name, commit_key):
    if not isinstance(row, dict) or row.get("repository") != f"beamiter/{name}":
        raise ValueError(f"{name}: expected repository beamiter/{name}")
    return (
        object_id(row.get(commit_key), f"{name}/{commit_key}"),
        object_id(row.get("tree"), f"{name}/tree"),
        object_id(row.get("validated_source_commit"), f"{name}/validated_source_commit"),
    )


def candidate_relation(repository, candidate, published_tree):
    """Local availability is not a proof of native execution or publication."""
    try:
        kind = git(repository, "cat-file", "-t", candidate)
    except subprocess.CalledProcessError:
        return {"availability": "not_available_locally"}
    if kind != b"commit\n":
        raise ValueError("validated_source_commit must name a commit when present locally")
    tree = git(repository, "rev-parse", "--verify", f"{candidate}^{{tree}}").decode().strip()
    return {
        "availability": "present_locally",
        "tree": tree,
        "equals_published_tree": tree == published_tree,
    }


def verify_published_release(root, record):
    """Verify public build inputs without treating attested test results as replayed."""
    shared = record.get("shared_core")
    apps = record.get("applications")
    if not isinstance(apps, dict) or set(apps) != set(APP_LOCKS):
        raise ValueError("published release must name all five applications")
    sources = {"jterm_core": source_entry(shared, "jterm_core", "release_source_commit")}
    core = sources["jterm_core"][0]
    for name, data in apps.items():
        sources[name] = source_entry(data, name, "release_commit")
        if object_id(data.get("core_pin"), f"{name}/core_pin") != core:
            raise ValueError(f"{name}: recorded app pin differs from the published core")

    acceptance = record.get("acceptance")
    if not isinstance(acceptance, dict) or not isinstance(acceptance.get("scope"), str):
        raise ValueError("published release must describe the scope of its recorded acceptance")
    observed = snapshot(root, {name: row[0] for name, row in sources.items()})
    if observed["common_core_mismatches"]:
        labels = ", ".join(sorted(observed["common_core_mismatches"]))
        raise ValueError(f"published locks disagree with the shared core: {labels}")

    for name, (commit, expected_tree, candidate) in sources.items():
        repository = root / name
        tree = git(repository, "rev-parse", "--verify", f"{commit}^{{tree}}").decode().strip()
        if tree != expected_tree:
            raise ValueError(f"{name}: published tree differs from the release record")
        info = observed["repositories"][name]
        info["tree"] = tree
        info["validated_source_commit"] = candidate
        info["validated_source_relation"] = candidate_relation(repository, candidate, tree)

    return {
        "schema": RELEASE_SCHEMA,
        "verification_scope": "git_source_and_production_locks_only",
        "recorded_acceptance_verified": False,
        "external_evidence_note": "This check does not replay native scenarios or verify binaries, NAR hashes, or receipt contents.",
        "production_lockfiles_verified": sum(len(locks) for locks in APP_LOCKS.values()),
        "repositories": observed["repositories"],
        "common_core_mismatches": observed["common_core_mismatches"],
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=FAMILY_ROOT, help="parent of the six Git checkouts")
    parser.add_argument("--verify", type=Path, help="verify a saved record against its exact Git commits")
    parser.add_argument("--require-common-core", action="store_true",
                        help="fail if an app lock resolves to a different commit than the recorded core")
    args = parser.parse_args()
    record = commits = None
    if args.verify:
        record = json.loads(args.verify.read_text())
        if isinstance(record, dict) and record.get("schema") == RELEASE_SCHEMA:
            report = verify_published_release(args.root, record)
            print(json.dumps(report, indent=2, sort_keys=True))
            return 0
        if not isinstance(record, dict) or type(record.get("schema")) is not int or record["schema"] != 1:
            raise ValueError("expected a schema 1 family release record")
        commits = {name: repo["commit"] for name, repo in record["repositories"].items()}
    observed = snapshot(args.root, commits)
    if record is not None and record != observed:
        raise ValueError("saved record differs from the immutable Git inputs")
    print(json.dumps(observed, indent=2, sort_keys=True))
    return int(args.require_common_core and bool(observed["common_core_mismatches"]))


if __name__ == "__main__":
    try:
        sys.exit(main())
    except subprocess.CalledProcessError as error:
        sys.exit(error.stderr.decode(errors="replace").strip() or str(error))
    except (ValueError, KeyError, TypeError, OSError, subprocess.SubprocessError) as error:
        sys.exit(str(error))
