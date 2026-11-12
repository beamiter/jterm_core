#!/usr/bin/env bash
# Capture metadata separately from CSV. Run from any working directory.
set -euo pipefail
engine=$(cd -- "$(dirname -- "$0")/.." && pwd)
repo=$(cd -- "$engine/../.." && pwd)
{
  printf 'utc='; date -u +%FT%TZ
  printf 'revision='; git -C "$repo" rev-parse HEAD
  printf 'working_diff_sha256='; git -C "$repo" diff HEAD --binary | sha256sum
  printf 'benchmark_source_sha256='; sha256sum "$engine/examples/parser_baseline.rs" | cut -d' ' -f1
  rustc -Vv
  uname -srvm
  if command -v lscpu >/dev/null; then lscpu 2>/dev/null || printf 'CPU topology unavailable in this environment\n'; fi
  printf 'CARGO_BUILD_JOBS=%s\n' "${CARGO_BUILD_JOBS:-unset}"
} >&2
cargo run --quiet --manifest-path "$engine/Cargo.toml" --locked --release --example parser_baseline -- "$@"
