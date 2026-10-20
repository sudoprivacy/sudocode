#!/usr/bin/env bash
set -euo pipefail

if ! command -v git >/dev/null; then
  echo '::error::git is required for engine boundary checks' >&2
  exit 1
fi

reject_matches() {
  local scan_pattern="$1"
  shift
  for renderer_path in "$@"; do
    if [ ! -d "$renderer_path" ]; then
      echo "::error::missing renderer path: $renderer_path" >&2
      return 1
    fi
  done
  local scan_status=0
  git grep -n -E "$scan_pattern" -- "$@" || scan_status=$?
  case "$scan_status" in
    1) return 0 ;;
    0) echo '::error::renderer bypasses the engine command/event boundary' >&2; return 1 ;;
    *) echo "::error::git grep failed (exit $scan_status)" >&2; return "$scan_status" ;;
  esac
}

reject_matches 'runtime::(TurnSummary|RuntimeObserver)' \
  rust/crates/rusty-sudocode-cli rust/crates/engine-acp

reject_matches '\.run_turn(_with_blocks)?[[:space:]]*\(|ObserverAdapter|SubagentRelay|abort_subagent|abort_registered_agent|(^|[^[:alnum:]_])Completion(Journal|State)([^[:alnum:]_]|$)|subagent_completion::Completions' \
  rust/crates/rusty-sudocode-cli/src rust/crates/engine-acp/src
