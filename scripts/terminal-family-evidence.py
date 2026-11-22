#!/usr/bin/env python3
"""Run exact pinned engine probes; never infer frontend parity from engine output."""
import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]
SHA = re.compile(r"[0-9a-f]{40}")
IDENT = re.compile(r"[a-z][a-z0-9_]*")
CASE_ID = re.compile(r"TERM\.[A-Z]+\.[0-9]{3}")
ADAPTERS = {"aether", "anvil", "ember", "forge", "frost"}
ORACLES = {"literal", "invariant", "historical-differential"}


def require(condition, error):
    if not condition:
        raise ValueError(error)


def git(root, *args):
    return subprocess.check_output(["git", "-C", str(root), *args], text=True).strip()


def catalog(root):
    result = json.loads((root / "tests/terminal-family/engine-cases.json").read_text())
    require(result["schema"] == 1, "unexpected catalog version")
    require(SHA.fullmatch(result["core_baseline"]), "expected immutable core baseline")
    require(set(result["frontends"]) == ADAPTERS, "expected all five frontend identities")
    require(all(SHA.fullmatch(v) for v in result["frontends"].values()), "invalid frontend SHA")
    cases = result["cases"]
    require(cases and len({case["id"] for case in cases}) == len(cases), "empty or duplicate case IDs")
    require(len({(c["target"], c["test"]) for c in cases}) == len(cases), "duplicate test mapping")
    for case in cases:
        require(CASE_ID.fullmatch(case["id"]), "invalid case ID")
        require(IDENT.fullmatch(case["target"]) and IDENT.fullmatch(case["test"]), "invalid test selector")
        require(case["oracle"] in ORACLES and case["profile"] and case["service"], "incomplete case scope")
    return result


def verify_source(root, expected):
    commit = git(root, "rev-parse", "HEAD")
    require(commit == expected, f"expected core {expected}, found {commit}")
    dirty = git(root, "status", "--porcelain=v1", "--untracked-files=all", "--", "crates/jterm_engine")
    require(not dirty, "engine source tree differs from the stated core commit")
    return commit


def passed_once(output, name, returncode):
    """Cargo returns success even when an exact test selector matches zero."""
    return (
        returncode == 0
        and output.count(f"test {name} ... ok\n") == 1
        and re.search(r"(?m)^running 1 test$", output) is not None
        and re.search(r"(?m)^test result: ok\. 1 passed; 0 failed; 0 ignored;", output) is not None
    )


def run_case(root, case, offline, target_dir):
    command = ["cargo", "test", "--manifest-path", "crates/jterm_engine/Cargo.toml", "--locked"]
    if offline:
        command.append("--offline")
    command.extend(["--test", case["target"], "--", case["test"], "--exact", "--color", "never"])
    env = dict(os.environ, CARGO_TARGET_DIR=str(target_dir), CARGO_TERM_COLOR="never")
    result = subprocess.run(command, cwd=root, env=env, stdout=subprocess.PIPE,
                            stderr=subprocess.STDOUT, text=True, check=False, timeout=180)
    return command, result.stdout, passed_once(result.stdout, case["test"], result.returncode)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True, help="new evidence directory")
    parser.add_argument("--offline", action="store_true")
    parser.add_argument("--case", action="append", help="exact capability ID, repeatable")
    parser.add_argument("--expected-core", help="explicit full core SHA; default: catalog baseline")
    args = parser.parse_args()
    data = catalog(ROOT)
    expected = args.expected_core or data["core_baseline"]
    require(SHA.fullmatch(expected), "invalid expected core SHA")
    commit = verify_source(ROOT, expected)
    cases = data["cases"]
    if args.case:
        require(set(args.case) <= {case["id"] for case in cases}, "unknown requested case")
        require(len(set(args.case)) == len(args.case), "duplicate requested case")
        cases = [case for case in cases if case["id"] in args.case]
    host = next(line.removeprefix("host: ") for line in
                subprocess.check_output(["rustc", "-vV"], text=True).splitlines()
                if line.startswith("host: "))
    # Refuse to overwrite prior evidence and avoid dirtying the core checkout.
    args.output.mkdir(parents=True, exist_ok=False)
    results = []
    with tempfile.TemporaryDirectory(prefix="terminal-family-target-") as target_dir:
        for case in cases:
            filename = case["id"] + ".log"
            try:
                command, output, passed = run_case(ROOT, case, args.offline, target_dir)
            except (subprocess.TimeoutExpired, OSError) as error:
                command, output, passed = None, str(error) + "\n", False
            (args.output / filename).write_text(output)
            results.append(dict(case, command=command, evidence=filename,
                                source=f"crates/jterm_engine/tests/{case['target']}.rs",
                                status="pass" if passed else "fail"))
            print(case["id"], results[-1]["status"], flush=True)
            if not passed:
                print(output[-10_000:], end="", flush=True)
    summary = {
        "schema": 1, "scope": "headless shared-engine tests only", "core_commit": commit,
        "platform": host, "cases": results,
        "frontends": {n: {"baseline": sha, "status": "not-run"}
                      for n, sha in data["frontends"].items()},
    }
    require(verify_source(ROOT, expected) == commit, "core changed during verification")
    (args.output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    return 0 if all(result["status"] == "pass" for result in results) else 1


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (ValueError, OSError, subprocess.SubprocessError) as error:
        sys.exit(str(error))
