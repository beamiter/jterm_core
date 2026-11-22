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
import tomllib


AETHER_CRATES = {
    "aether-session": "session",
    "aether-bridge": "bridge",
    "aether": "src-tauri",
}
AETHER_LOCAL_ENGINE = "../../jterm_core/crates/jterm_engine"
STANDALONE = {"jterm_engine", "jterm_runtime", *AETHER_CRATES}
PROJECTS = (
    "jterm_engine", "jterm_runtime", "jagent", "jterm_core", "jsh",
    "anvil", "ember", "forge", "frost", *AETHER_CRATES,
)


def mirror(source: Path, destination: Path, nested_crates: bool = False,
           private_core_crates: frozenset[str] = frozenset()) -> None:
    """Keep Cargo-owned manifests/locks private; compile the real source files."""
    destination.mkdir()
    for entry in source.iterdir():
        if entry.name in {".git", "target"}:
            continue
        target = destination / entry.name
        if nested_crates and entry.name in AETHER_CRATES.values():
            mirror(entry, target)
        elif entry.name == "crates" and private_core_crates:
            target.mkdir()
            for crate in entry.iterdir():
                crate_target = target / crate.name
                if crate.name in private_core_crates:
                    mirror(crate, crate_target)
                else:
                    crate_target.symlink_to(crate, target_is_directory=crate.is_dir())
        elif entry.name in {"Cargo.toml", "Cargo.lock"}:
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


def aether_engine_is_local(source: Path) -> bool:
    declarations = [
        tomllib.loads((source / crate / "Cargo.toml").read_text())
        .get("dependencies", {}).get("jterm_engine", {}).get("path")
        for crate in ("session", "bridge")
    ]
    if any(declarations) and declarations != [AETHER_LOCAL_ENGINE] * 2:
        raise ValueError("Aether session and bridge need the same sibling core engine")
    return all(declarations)


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
    parser.add_argument("--forge-shared-terminal", action="store_true",
                        help="build Forge's opt-in shared terminal in the private checkout")
    parser.add_argument("--anvil-shared-terminal", action="store_true",
                        help="build Anvil's opt-in shared terminal in the private checkout")
    parser.add_argument("project", choices=(*PROJECTS, "all"))
    parser.add_argument("cargo_args", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    gtk_apps = {
        app for app, enabled in (("forge", args.forge_shared_terminal), ("anvil", args.anvil_shared_terminal))
        if enabled
    }
    for app in sorted(gtk_apps):
        if args.project not in {app, "all"}:
            parser.error(f"--{app}-shared-terminal requires {app} or all")
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
    sources = {project: family / project for project in PROJECTS if project not in AETHER_CRATES}
    sources["aether"] = family / "aether"
    # Canonical publication lives in the existing core repository. Retain the
    # legacy sibling layout for older local family checkouts only.
    nested_engine = sources["jterm_core"] / "crates" / "jterm_engine"
    if (nested_engine / "Cargo.toml").is_file():
        sources["jterm_engine"] = nested_engine
    sources["jterm_runtime"] = sources["jterm_core"] / "crates" / "jterm_runtime"
    sources["jterm_protocol"] = sources["jterm_core"] / "crates" / "jterm_protocol"
    sources["jterm_remote_client"] = sources["jterm_core"] / "crates" / "jterm_remote_client"
    sources["jterm_gtk_view"] = sources["jterm_core"] / "crates" / "jterm_gtk_view"
    selected = PROJECTS if args.project == "all" else (args.project,)
    required = {"aether" if project in AETHER_CRATES else project for project in selected}
    if any(project not in STANDALONE for project in selected):
        required.add("jagent")
    if any(project not in STANDALONE | {"jagent", "jsh"} for project in selected):
        required.add("jterm_core")
    if gtk_apps or any(project in {"ember", "frost", *AETHER_CRATES} for project in selected):
        required.add("jterm_engine")
    if gtk_apps:
        required.add("jterm_gtk_view")
    for project in sorted(required):
        manifest_roots = [sources[project]]
        if project == "aether":
            manifest_roots = [sources[project] / crate for crate in AETHER_CRATES.values()]
        for manifest_root in manifest_roots:
            if not (manifest_root / "Cargo.toml").is_file():
                parser.error(f"missing family manifest: {manifest_root / 'Cargo.toml'}")

    local_aether_engine = False
    aether_git_runtime = False
    aether_dependencies = {}
    if "aether" in required:
        try:
            local_aether_engine = aether_engine_is_local(sources["aether"])
        except ValueError as error:
            parser.error(str(error))
        session = tomllib.loads((sources["aether"] / "session/Cargo.toml").read_text())
        linux = session.get("target", {}).get('cfg(target_os = "linux")', {})
        runtime = linux.get("dependencies", {}).get("jterm_runtime", {})
        desktop = tomllib.loads((sources["aether"] / "src-tauri/Cargo.toml").read_text())
        if "aether" in selected and desktop.get("dependencies", {}).get("jterm_core"):
            for package in ("jterm_core", "jagent"):
                if not (sources[package] / "Cargo.toml").is_file():
                    parser.error(f"missing family manifest: {sources[package] / 'Cargo.toml'}")
                required.add(package)
        for crate, package in (("src-tauri", "jterm_remote_client"), ("bridge", "jterm_protocol")):
            manifest = desktop if crate == "src-tauri" else tomllib.loads(
                (sources["aether"] / crate / "Cargo.toml").read_text()
            )
            dependency = manifest.get("dependencies", {}).get(package)
            if dependency:
                aether_dependencies[package] = dependency
                if not (sources[package] / "Cargo.toml").is_file():
                    parser.error(f"missing family manifest: {sources[package] / 'Cargo.toml'}")
                if "git" in dependency:
                    required.add(package)
        aether_git_runtime = isinstance(runtime, dict) and "git" in runtime
        if aether_git_runtime:
            required.add("jterm_runtime")
        if aether_git_runtime or local_aether_engine:
            if not (sources["jterm_runtime"] / "Cargo.toml").is_file():
                parser.error(f"missing family manifest: {sources['jterm_runtime'] / 'Cargo.toml'}")

    failures = []
    with tempfile.TemporaryDirectory(prefix="jterm-family-") as directory:
        root = Path(directory)
        nested_packages = frozenset(
            package for package in ("jterm_engine", "jterm_runtime", "jterm_protocol",
                                    "jterm_gtk_view", "jterm_remote_client")
            if sources[package].parent == sources["jterm_core"] / "crates"
        ) if "jterm_core" in required else frozenset()
        mirror_paths = {
            package: root / "jterm_core" / "crates" / package if package in nested_packages
            else root / package
            for package in sources
        }
        # Cargo keys path packages by their declared path as well as their name.
        # A patched core and dependent frontends must see the same nested copy.
        for project in sorted(required - nested_packages):
            mirror(
                sources[project],
                mirror_paths[project],
                nested_crates=project == "aether",
                private_core_crates=nested_packages if project == "jterm_core" else frozenset(),
            )
        if local_aether_engine and "jterm_core" not in required:
            core_crates = root / "jterm_core/crates"
            core_crates.mkdir(parents=True)
            for crate in ("jterm_engine", "jterm_runtime"):
                mirror(sources[crate], core_crates / crate)
            for package, dependency in aether_dependencies.items():
                if "path" in dependency:
                    mirror(sources[package], core_crates / package)
        for app in sorted(gtk_apps):
            manifest = root / app / "Cargo.toml"
            text = manifest.read_text()
            if "[dependencies]\n" not in text:
                parser.error(f"{app} manifest has no dependencies table")
            manifest.write_text(text.replace("[dependencies]\n",
                                             '[dependencies]\njterm_gtk_view = { path = "../jterm_core/crates/jterm_gtk_view" }\n', 1))
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
        for project in selected:
            print(f"\n[{project}] local family sources, private Cargo.lock", flush=True)
            if project in AETHER_CRATES:
                original = sources["aether"] / AETHER_CRATES[project]
                cargo_root = root / "aether" / AETHER_CRATES[project]
            else:
                original = sources[project]
                cargo_root = mirror_paths[project]
            environment = os.environ.copy()
            if project in {"forge", "anvil"}:
                candidate = f"JTERM_{project.upper()}_SHARED_TERMINAL"
                environment.pop(candidate, None)
                if project in gtk_apps:
                    environment[candidate] = "1"
            # Keep the existing cache unless the caller selected another one.
            environment.setdefault("CARGO_TARGET_DIR", str(original / "target"))
            environment["CARGO_TARGET_DIR"] = str(Path(environment["CARGO_TARGET_DIR"]).resolve())
            if cargo_args[0] == "test":
                native_test_runner(root, original, environment)
            # External Cargo subcommands (notably clippy) must receive the
            # overrides themselves; global flags before the subcommand can
            # be lost when cargo-clippy launches its inner cargo check.
            project_patches = patches.copy()
            if project in {"ember", "frost"} or (project in AETHER_CRATES and not local_aether_engine):
                project_patches += [
                    "--config",
                    'patch."https://github.com/beamiter/jterm_core.git".jterm_engine.path='
                    + json.dumps(str(mirror_paths["jterm_engine"])),
                ]
            if project in AETHER_CRATES and aether_git_runtime:
                project_patches += [
                    "--config",
                    'patch."https://github.com/beamiter/jterm_core.git".jterm_runtime.path='
                    + json.dumps(str(mirror_paths["jterm_runtime"])),
                ]
            if project == "aether" and "jterm_remote_client" in required:
                project_patches += [
                    "--config",
                    'patch."https://github.com/beamiter/jterm_core.git".jterm_remote_client.path='
                    + json.dumps(str(mirror_paths["jterm_remote_client"])),
                ]
            if project in {"aether", "aether-bridge"} and "jterm_protocol" in required:
                project_patches += [
                    "--config",
                    'patch."https://github.com/beamiter/jterm_core.git".jterm_protocol.path='
                    + json.dumps(str(mirror_paths["jterm_protocol"])),
                ]
            command = ["cargo", cargo_args[0], *project_patches, "--offline", *cargo_args[1:]]
            result = subprocess.run(command, cwd=cargo_root, env=environment)
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
