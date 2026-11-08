#!/usr/bin/env python3
"""Run Cargo against sibling checkouts without editing release pins or locks."""

import argparse
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tempfile


PROJECTS = ("jterm_engine", "jagent", "jterm_core", "jsh", "anvil", "ember", "forge", "frost")


def mirror(source: Path, destination: Path) -> None:
    """Keep Cargo-owned manifests/locks private; compile the real source files."""
    destination.mkdir()
    for entry in source.iterdir():
        if entry.name in {".git", "target"}:
            continue
        target = destination / entry.name
        if entry.name in {"Cargo.toml", "Cargo.lock"}:
            shutil.copyfile(entry, target)
        else:
            target.symlink_to(entry, target_is_directory=entry.is_dir())


def native_test_runner(root: Path, original: Path, environment: dict) -> None:
    """Keep filesystem/Git tests in the real checkout, with the mirror alive."""
    version = subprocess.check_output(["rustc", "-vV"], text=True)
    host = next(
        line.removeprefix("host: ")
        for line in version.splitlines()
        if line.startswith("host: ")
    )
    key = "CARGO_TARGET_" + host.upper().replace("-", "_") + "_RUNNER"
    runner = root / "test-runner.py"
    runner.write_text(
        "import os, shlex, sys\n"
        "os.chdir(sys.argv[1])\n"
        "command = shlex.split(os.environ.get('JTERM_FAMILY_TEST_RUNNER', '')) + sys.argv[2:]\n"
        "os.execvp(command[0], command)\n"
    )
    # Preserve an explicitly supplied native runner (e.g. a debugger or wrapper).
    environment["JTERM_FAMILY_TEST_RUNNER"] = environment.get(key, "")
    environment[key] = shlex.join([sys.executable, str(runner), str(original)])


def main() -> int:
    parser = argparse.ArgumentParser(
        description=__doc__,
        epilog=(
            "Examples: family-cargo.py jterm_core test --lib organism; "
            "family-cargo.py jsh test --lib; "
            "family-cargo.py all check --all-targets. Cargo runs offline. "
            "Native frontends still require their normal system libraries."
        ),
    )
    parser.add_argument("project", choices=(*PROJECTS, "all"))
    parser.add_argument("cargo_args", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    cargo_args = args.cargo_args or ["check", "--all-targets"]
    if cargo_args[0] == "--":
        cargo_args = cargo_args[1:]
    if not cargo_args:
        parser.error("a Cargo subcommand is required after --")
    # These would redirect the command away from the private manifests/locks.
    for argument in cargo_args:
        if argument.split("=", 1)[0] in {"--manifest-path", "--lockfile-path"}:
            parser.error(f"{argument} is managed by this runner")

    family = Path(__file__).resolve().parents[2]
    sources = {project: family / project for project in PROJECTS}
    # Canonical publication lives in the existing core repository. Retain the
    # legacy sibling layout for older local family checkouts only.
    nested_engine = sources["jterm_core"] / "crates" / "jterm_engine"
    if (nested_engine / "Cargo.toml").is_file():
        sources["jterm_engine"] = nested_engine
    selected = PROJECTS if args.project == "all" else (args.project,)
    required = set(selected)
    if any(project != "jterm_engine" for project in selected):
        required.add("jagent")
    if any(project not in {"jterm_engine", "jagent", "jsh"} for project in selected):
        required.add("jterm_core")
    if any(project in {"ember", "frost"} for project in selected):
        required.add("jterm_engine")
    for project in sorted(required):
        if not (sources[project] / "Cargo.toml").is_file():
            parser.error(f"missing family checkout: {sources[project]}")

    failures = []
    with tempfile.TemporaryDirectory(prefix="jterm-family-") as directory:
        root = Path(directory)
        for project in sorted(required):
            mirror(sources[project], root / project)
        patches = []
        if "jagent" in required:
            patches += [
                "--config",
                'patch."https://github.com/beamiter/jagent.git".jagent.path='
                + json.dumps(str(root / "jagent")),
            ]
        if "jterm_core" in required:
            patches += [
                "--config",
                'patch."https://github.com/beamiter/jterm_core.git".jterm_core.path='
                + json.dumps(str(root / "jterm_core")),
            ]
        if "jterm_engine" in required and "jterm_core" in required:
            patches += [
                "--config",
                'patch."https://github.com/beamiter/jterm_core.git".jterm_engine.path='
                + json.dumps(str(root / "jterm_engine")),
            ]
        for project in selected:
            print(f"\n[{project}] local family sources, private Cargo.lock", flush=True)
            environment = os.environ.copy()
            # Keep the existing cache unless the caller selected another one.
            environment.setdefault("CARGO_TARGET_DIR", str(sources[project] / "target"))
            environment["CARGO_TARGET_DIR"] = str(Path(environment["CARGO_TARGET_DIR"]).resolve())
            if cargo_args[0] == "test":
                native_test_runner(root, sources[project], environment)
            # External Cargo subcommands (notably clippy) must receive the
            # overrides themselves; global flags before the subcommand can
            # be lost when cargo-clippy launches its inner cargo check.
            command = ["cargo", cargo_args[0], *patches, "--offline", *cargo_args[1:]]
            result = subprocess.run(command, cwd=root / project, env=environment)
            if result.returncode:
                failures.append(project)
    if failures:
        print(f"Failed: {', '.join(failures)}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        sys.exit(130)
