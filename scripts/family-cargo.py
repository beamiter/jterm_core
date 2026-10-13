#!/usr/bin/env python3
"""Run Cargo against sibling checkouts without editing release pins or locks."""

import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile


PROJECTS = ("jagent", "jterm_core", "jsh", "anvil", "ember", "forge", "frost")


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
    selected = PROJECTS if args.project == "all" else (args.project,)
    required = set(selected) | {"jagent"}
    if any(project not in {"jagent", "jsh"} for project in selected):
        required.add("jterm_core")
    for project in sorted(required):
        if not (family / project / "Cargo.toml").is_file():
            parser.error(f"missing sibling checkout: {family / project}")

    failures = []
    with tempfile.TemporaryDirectory(prefix="jterm-family-") as directory:
        root = Path(directory)
        for project in sorted(required):
            mirror(family / project, root / project)
        patches = [
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
        for project in selected:
            print(f"\n[{project}] local family sources, private Cargo.lock", flush=True)
            environment = os.environ.copy()
            # Keep the existing cache unless the caller selected another one.
            environment.setdefault("CARGO_TARGET_DIR", str(family / project / "target"))
            environment["CARGO_TARGET_DIR"] = str(Path(environment["CARGO_TARGET_DIR"]).resolve())
            command = ["cargo", *patches, "--offline", *cargo_args]
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
