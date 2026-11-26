#!/usr/bin/env bash
set -euo pipefail

app=$1
output=$2
case "$app" in
  forge) target=(--lib); kind=lib ;;
  anvil) target=(--bin anvil); kind=bin ;;
  *) printf 'Expected forge or anvil\n' >&2; exit 2 ;;
esac

mkdir -p -- "$output"
build_log="$output/$app-test-artifacts.jsonl"
cargo_test=(cargo test)
if [[ -f tools/vte-oracle/run.py ]]; then
  cargo_test=(python3 -B tools/vte-oracle/run.py test)
fi
"${cargo_test[@]}" --locked --release --features shared-terminal "${target[@]}" \
  --no-run --message-format=json >"$build_log"
test_binary=$(python3 -B - "$app" "$kind" "$build_log" <<'PY'
import json
import sys

app, kind, path = sys.argv[1:]
with open(path, encoding="utf-8") as source:
    candidates = {
        artifact["executable"]
        for artifact in map(json.loads, source)
        if artifact.get("executable")
        and artifact.get("profile", {}).get("test")
        and artifact["target"]["name"] == app
        and kind in artifact["target"]["kind"]
    }
if len(candidates) != 1:
    raise SystemExit("Expected exactly one native application test executable")
print(candidates.pop())
PY
)

cases=(
  shared_finished_output_preserves_search_filter_and_cross_card_copy
  shared_finished_chat_preserves_capsule_search_filter_and_copy
)
run_case() {
  local scenario=$1
  shift
  local test_name="block_view::cross_selection::tests::$scenario"
  "$test_binary" --list | grep -Fx "$test_name: test"
  dbus-run-session -- xvfb-run -a env -u JTERM_SHARED_FINISHED_COMMANDS \
    -u JTERM_SHARED_FINISHED_OUTPUT \
    -u JTERM_SHARED_ACTIVE_TERMINAL -u JTERM_SHARED_TRANSCRIPT GSK_RENDERER=cairo "$@" \
    "$test_binary" --exact "$test_name" --ignored --test-threads=1 \
    | tee "$output/$scenario.log"
}
run_case shared_live_output_keeps_the_owned_shell_through_alternate_screen
for scenario in "${cases[@]}"; do
  run_case "$scenario"
done
run_case shared_finished_command_preserves_search_and_cross_card_copy JTERM_SHARED_FINISHED_OUTPUT=0
