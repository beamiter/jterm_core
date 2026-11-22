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
        if not isinstance(record, dict) or record.get("schema") != 1:
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
