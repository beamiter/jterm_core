# SQLite security update and Rust minimum

The root `jterm_core` package now requires Rust 1.89. Its independently locked
`crates/jterm_engine` package remains compatible with Rust 1.86; it does not
link SQLite or depend on the root package. CI checks both minimum versions.

[RUSTSEC-2026-0459](https://rustsec.org/advisories/RUSTSEC-2026-0459)
(CVE-2026-119375) affects bundled `libsqlite3-sys` versions from 0.6.0 through
0.38.2. A crafted database and sibling WAL file with inconsistent page sizes
can expose uninitialized page-cache memory when opened. The shared Agent
Workspace uses bundled SQLite and WAL, so the dependency is upgraded rather
than suppressing the advisory or relying on application-level access checks.

The root now uses `rusqlite =0.41.0` and locks `libsqlite3-sys 0.39.1`, which
bundles SQLite 3.54.1. The first patched native library line, 0.39, requires
Rust 1.88, and rusqlite 0.41 requires Rust 1.89. The previous Rust 1.86 toolchain
cannot build this official patched chain. The `fallible_uint` feature preserves
existing checked u64 row decoding after rusqlite moved it behind that feature.
No production persistence schema or query changes are required.

A regression checks that the linked SQLite runtime is at least 3.54.0 and
checks unsigned row decoding at the signed maximum and negative boundary.
This guards the selected runtime and API contract; it is not an exploit test.
Existing Agent Workspace tests cover persistence, migration, paging, receipts,
and search. The unmodified strict audit script checks all three Cargo locks
with cargo-audit 0.22.2 and does not ignore this advisory.

All four frontend consumers must pin the security-fix core commit and update
their locks and source-package metadata together. The older Rust 1.86 validation
entries remain historical evidence, not claims about the upgraded root crate.
Unrelated dependency pins remain unchanged.

Raising the manifest minimum also enables newer Clippy checks. Five mechanical
expressions use equivalent `is_multiple_of`, `as_chunks`, and `is_none_or` APIs
supported by Rust 1.89. They preserve the existing parity, pixel-tail, and cooldown
semantics; no lint or audit suppression is introduced.
