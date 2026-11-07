#!/usr/bin/env bash
# Lockfile-only audit: no project builds or runtime policies.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

# A new config could suppress diagnostics or disable coverage. Revalidate first.
if [[ -e .cargo/audit.toml || -L .cargo/audit.toml ]]; then
    printf 'Error: .cargo/audit.toml requires revalidation of the audit configuration/diagnostic contract.\n' >&2
    exit 1
fi

# The diagnostic guard below is tested against this exact release.
version="$(cargo audit --version)"
if [[ ! "$version" =~ ^cargo-audit(-audit)?[[:space:]]0\.22\.2$ ]]; then
    printf 'Error: expected cargo-audit 0.22.2, got %s.\n' "$version" >&2
    exit 1
fi

audit_lockfile() {
    local lockfile="$1" output status=0 diagnostic_status=0
    # JSON/quiet hides index update/open errors in 0.22.2.
    output="$(cargo audit --format terminal --color never --deny warnings \
        --file "$lockfile" 2>&1)" || status=$?
    printf '%s\n' "$output"
    if ((status != 0)); then
        return "$status"
    fi
    grep -Eq "^[[:space:]]*(error: couldn't check if the package is yanked:|warning: couldn't (update|open) crates[.]io index:)" \
        <<<"$output" || diagnostic_status=$?
    case "$diagnostic_status" in
        0) printf 'Error: incomplete yanked-crate check for %s.\n' "$lockfile" >&2; return 1 ;;
        1) return 0 ;;
        *) printf 'Error: could not validate audit diagnostics for %s (grep exited %s).\n' \
            "$lockfile" "$diagnostic_status" >&2; return "$diagnostic_status" ;;
    esac
}

audit_lockfile Cargo.lock
# Independent workspace and lockfile; do not disable its index fetch.
audit_lockfile tools/companion-host-harness/Cargo.lock
