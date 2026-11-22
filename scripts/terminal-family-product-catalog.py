#!/usr/bin/env python3
"""Validate the product journey plan and optionally its immutable source anchors."""
import argparse
import json
import os
from pathlib import Path, PurePosixPath
import re
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
CATALOG = ROOT / "tests/terminal-family/product-cases.json"
ADAPTERS = {"aether", "anvil", "ember", "forge", "frost"}
REPOSITORIES = ADAPTERS | {"jterm_core"}
OBSERVATIONS = {"source-observed", "partial-source", "not-established"}
CASE_ID = re.compile(r"PRODUCT\.[A-Z]+\.[0-9]{3}")
SHA = re.compile(r"[0-9a-f]{40}")


def require(condition, message):
    if not condition:
        raise ValueError(message)


def text(value):
    return isinstance(value, str) and bool(value.strip())


def check_witness(witness):
    require(set(witness) == {"path", "anchor"}, "invalid source evidence shape")
    path = PurePosixPath(witness["path"])
    require(
        path.parts and not path.is_absolute() and ".." not in path.parts and path.as_posix() == witness["path"]
        and all(re.fullmatch(r"[A-Za-z0-9_.-]+", part) for part in path.parts),
        "invalid repository-relative source path",
    )
    anchor = witness["anchor"]
    require(text(anchor) and "\n" not in anchor and anchor == anchor.strip(), "invalid source anchor")


def validate(data):
    require(data["schema"] == 1, "unsupported product catalog schema")
    require(set(data["repos"]) == REPOSITORIES, "all six immutable baseline refs are required")
    require(all(SHA.fullmatch(sha) for sha in data["repos"].values()), "require full commit SHAs")
    rows = data["journeys"]
    ids = {r["id"] for r in rows}
    require(rows and len(rows) == len(ids), "missing or duplicate journey IDs")
    tests = set()
    for row in rows:
        case = row["id"]
        require(CASE_ID.fullmatch(case) and text(row["journey"]), f"invalid journey {case}")
        targets = row["target_adapters"]
        require("aether" in targets and set(targets) <= ADAPTERS and len(targets) == len(set(targets)),
                f"invalid delivery targets for {case}")
        require(row["work_package"] in range(11, 19), f"out-of-scope work package for {case}")
        check_witness(row["service"])
        test = row["test"]
        require(test["id"] == case + ".NATIVE.001" and test["id"] not in tests, f"invalid test ID for {case}")
        require(text(test["oracle"]) and text(test["platform"]), f"missing native oracle for {case}")
        require(test["status"] == "not-run", f"source catalog cannot award native pass for {case}")
        tests.add(test["id"])
        require(set(row["adapters"]) == ADAPTERS, f"missing adapter in {case}")
        for name, lane in row["adapters"].items():
            state = lane["observation"]
            require(state in OBSERVATIONS and lane["runtime"] == "not-run", f"invalid runtime scope: {case}/{name}")
            if state != "source-observed":
                require(text(lane.get("limit")), f"missing non-parity boundary: {case}/{name}")
                require(name in targets, f"unproven lane has no work target: {case}/{name}")
            if state == "not-established":
                require("source" not in lane, f"absence is not asserted by a source anchor: {case}/{name}")
            else:
                check_witness(lane["source"])


def run_git(directory, *arguments):
    env = dict(os.environ, GIT_NO_LAZY_FETCH="1", GIT_TERMINAL_PROMPT="0")
    return subprocess.run(["git", "-C", str(directory), *arguments], check=True, env=env,
                          stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, timeout=10).stdout


def verify_sources(data, checkouts):
    require(set(checkouts) == REPOSITORIES, "provide all six Git checkout paths")
    for name, directory in checkouts.items():
        sha = data["repos"][name]
        require(run_git(directory, "cat-file", "-t", sha).strip() == "commit",
                f"{name} lacks required exact commit {sha}")
    blobs = {}
    count = 0
    for row in data["journeys"]:
        sources = {"jterm_core": row["service"]}
        sources.update((name, lane["source"]) for name, lane in row["adapters"].items() if "source" in lane)
        for name, source in sources.items():
            key = name, source["path"]
            if key not in blobs:
                blob = f"{data['repos'][name]}:{source['path']}"
                blobs[key] = {line.strip() for line in run_git(checkouts[name], "cat-file", "blob", blob).splitlines()}
            require(source["anchor"] in blobs[key], f"source anchor missing: {row['id']}/{name}/{source['path']}")
            count += 1
    return count


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", action="append", default=[], metavar="NAME=CHECKOUT",
                        help="if used, provide all six local checkout paths; no fetching is done")
    args = parser.parse_args()
    data = json.loads(CATALOG.read_text())
    validate(data)
    dirs = {}
    for item in args.repo:
        name, sep, directory = item.partition("=")
        require(sep and name in REPOSITORIES and name not in dirs and directory, "invalid --repo argument")
        dirs[name] = Path(directory)
    count = verify_sources(data, dirs) if dirs else None
    print(json.dumps({
        "catalog": "valid",
        "source_anchors_verified": count,
        "journeys": len(data["journeys"]),
        "frontends": {name: "not-run" for name in sorted(ADAPTERS)},
        "scope": "source inventory only; no native tests were run",
    }))
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (ValueError, KeyError, TypeError, OSError, subprocess.SubprocessError) as error:
        sys.exit(str(error))
