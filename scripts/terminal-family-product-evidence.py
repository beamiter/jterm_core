#!/usr/bin/env python3
"""Record native product journey evidence against an immutable family candidate."""

import argparse
import hashlib
import importlib.util
import json
import os
import shutil
from pathlib import Path
import subprocess
import sys
import tempfile


SCRIPTS = Path(__file__).resolve().parent
AETHER_CLI = "PRODUCT.CLI.001.NATIVE.001"
AETHER_REMOTE = "PRODUCT.REMOTE.001.NATIVE.001"
AETHER_REVIEW = "PRODUCT.REVIEW.001.NATIVE.001"
LOCAL_DRIVER = "scripts/test-two-local-webkit.py"
REMOTE_DRIVER = "scripts/test-remote-project-webkit.py"
WAYLAND_DRIVER = "scripts/test-wayland-ime.py"
PLATFORM = "Linux X11 / Xvfb / WebKitGTK"
WAYLAND_PLATFORM = "Linux Wayland / isolated Sway / WebKitGTK"
REVIEW_RECEIPT = {
    "native_output_review_exact_owned_records": True,
    "native_output_review_stays_frozen_after_retention_eviction": True,
    "native_output_review_preserves_pty_ranges_markers_and_geometry": True,
    "native_output_review_downloads_markdown_and_json": True,
}
UNAVAILABLE_RECEIPT = {"native_command_card_shows_pty_source_with_unavailable_output": True}


def load_tool(name):
    spec = importlib.util.spec_from_file_location(name, SCRIPTS / f"{name}.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


catalog_tool = load_tool("terminal-family-product-catalog")
release_tool = load_tool("terminal-family-release")


def sha256(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def json_records(log):
    for line in log.splitlines():
        if not line.startswith("{"):
            continue
        try:
            record = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(record, dict):
            yield record


def named_native_result(log, case, digests, exit_code, *, backend="x11", case_field="case"):
    """Require one exact named receipt; older CLI/Remote callers stay strict."""
    receipts = [record for record in json_records(log) if record.get(case_field) == case]
    expected = {case_field: case, "backend": backend, "result": "passed", **digests}
    return exit_code == 0 and receipts == [expected]


def exact_boolean_receipt(log, expected):
    # Frozen X11 drivers predate named Review receipts. Do not infer a pass
    # from individual flags, partial records, numeric truthiness or duplicates.
    records = [record for record in json_records(log) if expected.keys() & record.keys()]
    return (len(records) == 1 and records[0].keys() == expected.keys()
            and all(records[0][key] is True for key in expected))


def review_native_result(log, backend, digest, exit_code, receipt=None):
    if exit_code != 0:
        return False
    if backend == "x11":
        return exact_boolean_receipt(log, REVIEW_RECEIPT) and exact_boolean_receipt(log, UNAVAILABLE_RECEIPT)
    if backend != "wayland":
        return False
    expected = {"case_id": AETHER_REVIEW, "backend": "wayland", "result": "passed",
                "binary_sha256": digest}
    if receipt is None or receipt.is_symlink() or not receipt.is_file():
        return False
    try:
        saved = json.loads(receipt.read_text())
    except (OSError, ValueError):
        return False
    return saved == expected and named_native_result(
        log, AETHER_REVIEW, {"binary_sha256": digest}, 0, backend="wayland", case_field="case_id")


def review_observation(platforms):
    complete = set(platforms) == {"x11", "wayland"}
    passed = complete and all(lane["status"] == "native-pass" for lane in platforms.values())
    return {"status": "native-pass" if passed else "native-fail",
            "platform": "Linux X11 and Wayland / WebKitGTK",
            "platform_results": platforms}


def matrix(catalog, candidate, observations):
    rows = []
    for journey in catalog["journeys"]:
        test_id = journey["test"]["id"]
        adapters = {
            name: {
                "commit": candidate["repositories"][name]["commit"],
                "status": "not-run",
            }
            for name in sorted(catalog_tool.ADAPTERS)
        }
        if test_id in observations:
            adapters["aether"].update(observations[test_id])
        rows.append({
            "id": journey["id"],
            "journey": journey["journey"],
            "service": journey["service"],
            "test_id": test_id,
            "oracle": journey["test"]["oracle"],
            "target_adapters": journey["target_adapters"],
            "adapters": adapters,
        })
    return rows


def verify_candidate(root, candidate):
    commits = {name: repo["commit"] for name, repo in candidate["repositories"].items()}
    observed = release_tool.snapshot(root, commits)
    if candidate != observed or observed["common_core_mismatches"]:
        raise ValueError("candidate does not match the immutable common-core inputs")
    return commits


def verify_driver(repo, revision, relative):
    actual = release_tool.git(repo, "rev-parse", "HEAD").decode().strip()
    if actual != revision:
        raise ValueError(f"native test checkout is {actual}, expected {revision}")
    driver = repo / relative
    if driver.read_bytes() != release_tool.git(repo, "cat-file", "blob", f"{revision}:{relative}"):
        raise ValueError(f"{relative} differs from the candidate commit")
    return driver


def run_native(driver, arguments, log, *, backend="x11", vim=None, seconds=150):
    if backend not in {"x11", "wayland"}:
        raise ValueError("unknown native backend")
    command = ["dbus-run-session", "--"]
    if backend == "x11":
        command += ["xvfb-run", "-a", "-s", "-screen 0 1600x1200x24 -nolisten tcp"]
    command += ["timeout", "--kill-after=20s", f"{seconds}s", sys.executable, "-B", str(driver), *arguments]
    with tempfile.TemporaryDirectory(prefix="terminal-family-product-", ignore_cleanup_errors=True) as directory:
        state = Path(directory)
        cache = state / "cache"
        runtime = state / "runtime"
        cache.mkdir(mode=0o700)
        runtime.mkdir(mode=0o700)
        env = dict(os.environ, XDG_CACHE_HOME=str(cache), XDG_RUNTIME_DIR=str(runtime))
        env.pop("AT_SPI_BUS_ADDRESS", None)
        if vim is not None:
            # Wayland's frozen fixture resolves Vim by PATH, while X11 also
            # receives --vim-binary. Pin the exact verified file in both cases.
            tools = state / "bin"
            tools.mkdir()
            (tools / "vim").symlink_to(vim)
            env["PATH"] = str(tools) + os.pathsep + env.get("PATH", os.defpath)
        with log.open("wb") as stream:
            result = subprocess.run(command, env=env, stdout=stream, stderr=subprocess.STDOUT, check=False)
    return command, result.returncode


def verify_artifact(path, expected):
    executable = path.absolute()
    if executable.is_symlink() or not executable.is_file() or not os.access(executable, os.X_OK):
        raise ValueError("provide a regular executable for the native test")
    with executable.open("rb") as stream:
        if stream.read(4) != b"\x7fELF":
            raise ValueError(f"{executable.name} is not an ELF executable")
    if sha256(executable) != expected:
        raise ValueError(f"{executable.name} differs from the build digest")
    return executable


def runtime_tool(path, name):
    resolved = str(path) if path is not None else shutil.which(name)
    if not resolved:
        raise ValueError(f"{name} is required for Wayland output review")
    executable = Path(resolved).absolute()
    if executable.is_symlink() or not executable.is_file() or not os.access(executable, os.X_OK):
        raise ValueError(f"{name} must be a regular executable")
    return verify_artifact(executable, sha256(executable))


def review_inputs(args, parser, artifacts):
    pairs = (("jsh", "jsh_sha256"), ("vim_binary", "vim_sha256"), ("ime_client", "ime_client_sha256"))
    if not args.output_review:
        if any(getattr(args, name) for pair in pairs for name in pair) or any(
                (args.sway, args.wtype, args.compositor_libraries)):
            parser.error("output review fixture options require --output-review")
        return None
    verified = {}
    for path_name, digest_name in pairs:
        path, digest = getattr(args, path_name), getattr(args, digest_name)
        if path is None or not digest:
            parser.error(f"--output-review requires --{path_name.replace('_', '-')} and --{digest_name.replace('_', '-')}")
        executable = verify_artifact(path, digest)
        artifacts[executable] = digest
        verified[path_name] = executable
    verified["sway"] = runtime_tool(args.sway, "sway")
    verified["wtype"] = runtime_tool(args.wtype, "wtype")
    verified["wl_paste"] = runtime_tool(verified["wtype"].with_name("wl-paste"), "wl-paste")
    for name in ("sway", "wtype", "wl_paste"):
        artifacts[verified[name]] = sha256(verified[name])
    if args.compositor_libraries is not None and not args.compositor_libraries.is_dir():
        parser.error("--compositor-libraries must name a directory")
    return verified


def observation(driver, arguments, log, digests, *, backend="x11", case, review=False, receipt=None, vim=None):
    command, exit_code = run_native(driver, arguments, log, backend=backend, vim=vim,
                                   seconds=240 if review else 150)
    content = log.read_text(errors="replace")
    passed = (review_native_result(content, backend, digests["binary_sha256"], exit_code, receipt)
              if review else named_native_result(content, case, digests, exit_code))
    result = {"status": "native-pass" if passed else "native-fail",
              "platform": PLATFORM if backend == "x11" else WAYLAND_PLATFORM,
              "artifact_sha256": digests, "command": command, "exit_code": exit_code,
              "log": log.name, "log_sha256": sha256(log), "driver_sha256": sha256(driver)}
    if receipt is not None and receipt.is_file() and not receipt.is_symlink():
        result["receipt"] = {"path": str(receipt.relative_to(log.parent)), "sha256": sha256(receipt)}
    lane = f" / {backend}" if review else ""
    print(f"{case} / Aether{lane}: {result['status']}", flush=True)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, required=True, help="parent of the six Git worktrees")
    parser.add_argument("--candidate", type=Path, required=True, help="verified family release manifest")
    parser.add_argument("--aether-binary", type=Path, required=True, help="built Aether production candidate")
    parser.add_argument("--aether-sha256", required=True, help="artifact digest taken from the candidate build")
    parser.add_argument("--aether-host", type=Path, help="built Aether SSH host for the remote product journey")
    parser.add_argument("--aether-host-sha256", help="SSH host digest taken from the candidate build")
    parser.add_argument("--output", type=Path, required=True, help="new evidence directory")
    parser.add_argument("--output-review", action="store_true", help="also verify output review on BOTH X11 and Wayland")
    parser.add_argument("--jsh", type=Path, help="exact real jsh fixture ELF")
    parser.add_argument("--jsh-sha256")
    parser.add_argument("--vim-binary", type=Path, help="exact Vim fixture ELF for hidden fullscreen ownership")
    parser.add_argument("--vim-sha256")
    parser.add_argument("--ime-client", type=Path, help="existing input-method-v2 fixture ELF")
    parser.add_argument("--ime-client-sha256")
    parser.add_argument("--sway", type=Path, help="Sway executable (default: PATH)")
    parser.add_argument("--wtype", type=Path, help="wtype beside wl-paste (default: PATH)")
    parser.add_argument("--compositor-libraries", type=Path)
    args = parser.parse_args()
    root = args.root.resolve()
    candidate_digest = sha256(args.candidate)
    candidate = json.loads(args.candidate.read_text())
    commits = verify_candidate(root, candidate)
    catalog = json.loads(catalog_tool.CATALOG.read_text())
    catalog_tool.validate(catalog)
    repo = root / "aether"
    revision = commits["aether"]
    local = verify_driver(repo, revision, LOCAL_DRIVER)
    binary = verify_artifact(args.aether_binary, args.aether_sha256)
    artifacts = {binary: args.aether_sha256}
    fixtures = review_inputs(args, parser, artifacts)
    driver_paths = {LOCAL_DRIVER}
    wayland = None
    if fixtures is not None:
        wayland = verify_driver(repo, revision, WAYLAND_DRIVER)
        driver_paths.add(WAYLAND_DRIVER)
    recipes = [(AETHER_CLI, local, [str(binary), "--workspace-only"],
                {"binary_sha256": args.aether_sha256})]
    if bool(args.aether_host) != bool(args.aether_host_sha256):
        parser.error("provide both --aether-host and --aether-host-sha256")
    if args.aether_host:
        remote = verify_driver(repo, revision, REMOTE_DRIVER)
        driver_paths.add(REMOTE_DRIVER)
        host = verify_artifact(args.aether_host, args.aether_host_sha256)
        artifacts[host] = args.aether_host_sha256
        recipes.append((AETHER_REMOTE, remote, [str(binary), str(host), "--backend", "x11"],
                        {"desktop_sha256": args.aether_sha256, "helper_sha256": args.aether_host_sha256}))

    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    observations = {}
    for case, driver, arguments, digests in recipes:
        observations[case] = observation(driver, arguments, output / f"{case}.aether.log", digests, case=case)
    if fixtures is not None:
        jsh, vim, ime = (fixtures[name] for name in ("jsh", "vim_binary", "ime_client"))
        platforms = {}
        for backend in ("x11", "wayland"):
            receipt = None
            arguments = [str(binary), "--jsh", str(jsh), "--output-review"]
            driver = local
            if backend == "x11":
                arguments += ["--vim-binary", str(vim)]
            else:
                driver = wayland
                directory = output / "aether-review-wayland"
                receipt = directory / "result.json"
                arguments += ["--output", str(directory), "--ime-client", str(ime),
                              "--sway", str(fixtures["sway"]), "--wtype", str(fixtures["wtype"])]
                if args.compositor_libraries is not None:
                    arguments += ["--compositor-libraries", str(args.compositor_libraries.resolve())]
            lane = observation(driver, arguments, output / f"{AETHER_REVIEW}.aether.{backend}.log",
                               {"binary_sha256": args.aether_sha256}, backend=backend,
                               case=AETHER_REVIEW, review=True, receipt=receipt, vim=vim)
            lane["fixture_artifacts"] = {str(path): digest for path, digest in artifacts.items() if path != binary}
            platforms[backend] = lane
        observations[AETHER_REVIEW] = review_observation(platforms)
    for path, digest in artifacts.items():
        verify_artifact(path, digest)
    if sha256(args.candidate) != candidate_digest:
        raise ValueError("candidate manifest changed during the native test")
    for relative in sorted(driver_paths):
        verify_driver(repo, revision, relative)
    report = {
        "schema": 1,
        "candidate_sha256": candidate_digest,
        "core_commit": commits["jterm_core"],
        "journeys": matrix(catalog, candidate, observations),
    }
    (output / "summary.json").write_text(json.dumps(report, indent=2, ensure_ascii=False) + "\n")
    return 0 if all(result["status"] == "native-pass" for result in observations.values()) else 1


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (KeyError, OSError, TypeError, ValueError, subprocess.SubprocessError) as error:
        sys.exit(str(error))
