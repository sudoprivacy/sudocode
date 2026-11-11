#!/usr/bin/env bash
# A real managed scode process, using ACP without a terminal or PTY broker.
set -euo pipefail
cd "$(dirname "$0")"
# shellcheck source=lib.sh
. ./lib.sh

RUST_DIR="${RUST_DIR:-$(cd ../../rust && pwd)}"
AUTHON_PORT="${NEXUS_A2A_MANAGED_PORT:-2145}"
AUTHON_ZONE="${NEXUS_A2A_ZONE:-sharedzone}"
ENDPOINT="https://127.0.0.1:$AUTHON_PORT"
MODEL="${NEXUS_A2A_MANAGED_MODEL:-claude-sonnet-4-6}"
MANIFEST=(--manifest-path "$RUST_DIR/Cargo.toml")
BUILD=(cargo_in_workspace build "${MANIFEST[@]}" --locked -q --bins -p rusty-sudocode-cli)
if [ "${NEXUS_A2A_MODEL_LIVE:-0}" != 1 ]; then
  BUILD+=(-p mock-anthropic-service)
fi
"${BUILD[@]}"
MANAGED_TARGET_DIR="$(cargo_in_workspace metadata "${MANIFEST[@]}" --locked --format-version 1 --no-deps \
  | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
SCODE_BIN="$MANAGED_TARGET_DIR/debug/scode"
[ -x "$SCODE_BIN" ] || SCODE_BIN="$SCODE_BIN.exe"
[ -x "$SCODE_BIN" ] || { echo "!! no current scode executable: $SCODE_BIN" >&2; exit 1; }
if [ -z "${NEXUSD_BIN:-}" ]; then
  NEXUSD_BIN="$(bash ./fetch-daemon.sh)"
fi
AUTHON_NEXUSD_BIN="$NEXUSD_BIN"
AUTHON_DATA_DIR="$(mktemp -d "${TMPDIR:-/tmp}/scode-managed-process.XXXXXX")"
AUTHON_DAEMON_PID=
MOCK_PID=
cleanup() {
  authon_stop
  if [ -n "$MOCK_PID" ]; then
    kill "$MOCK_PID" 2>/dev/null || true
    wait "$MOCK_PID" 2>/dev/null || true
  fi
  if [ -z "${NEXUS_A2A_KEEP_WORK:-}" ]; then
    rm -rf "$AUTHON_DATA_DIR"
  else
    echo "== work dir kept: $AUTHON_DATA_DIR =="
  fi
}
trap cleanup EXIT
mkdir -p "$AUTHON_DATA_DIR/data" "$AUTHON_DATA_DIR/id"

if [ "${NEXUS_A2A_MODEL_LIVE:-0}" = 1 ]; then
  : "${NEXUS_A2A_MODEL_URL:?live acceptance requires a provider URL}"
  : "${NEXUS_A2A_MODEL_KEY:?live acceptance requires an explicit funded key}"
else
  MOCK_BIN="$MANAGED_TARGET_DIR/debug/mock-anthropic-service"
  [ -x "$MOCK_BIN" ] || MOCK_BIN="$MOCK_BIN.exe"
  "$MOCK_BIN" --bind 127.0.0.1:0 >"$AUTHON_DATA_DIR/mock.log" 2>&1 &
  MOCK_PID=$!
  for _ in $(seq 1 20); do
    grep -q '^MOCK_ANTHROPIC_BASE_URL=' "$AUTHON_DATA_DIR/mock.log" && break
    sleep 1
  done
  NEXUS_A2A_MODEL_URL="$(sed -n 's/^MOCK_ANTHROPIC_BASE_URL=//p' "$AUTHON_DATA_DIR/mock.log" | head -1 | tr -d '\r')"
  [ -n "$NEXUS_A2A_MODEL_URL" ] || { cat "$AUTHON_DATA_DIR/mock.log" >&2; exit 1; }
fi

echo "== authenticated managed-agent control plane =="
authon_boot
authon_wait_log 'Static topology applied' 45
authon_stop
FRONTDOOR_ID=managed-process-front-door
FRONTDOOR_BUNDLE="$(authon_mint "$FRONTDOOR_ID")"
authon_boot
authon_wait_log "Zone '$AUTHON_ZONE' registered" 45
authon_wait_log 'VFS data plane ready' 45
session_mint_tools_init
allowed=
for _ in $(seq 1 30); do
  if allow_session_minter "$ENDPOINT" "$AUTHON_DATA_DIR/data" "$FRONTDOOR_ID"; then
    allowed=1
    break
  fi
  sleep 2
done
[ -n "$allowed" ] || { tail -40 "$AUTHON_DATA_DIR/daemon.log" >&2; exit 1; }
SESSION_BUNDLE="$(mint_session_bundle "$ENDPOINT" "$(shell_path "$FRONTDOOR_BUNDLE")" \
  operator "$AUTHON_DATA_DIR/session-bundle")"

echo "== independent scode: file tools, cancel, exit and ACP recovery =="
NEXUS_A2A_TEST_ENDPOINT="$ENDPOINT" NEXUS_A2A_TEST_CERT_DIR="$SESSION_BUNDLE" \
  NEXUS_A2A_SCODE_BIN="$(native_path "$SCODE_BIN")" \
  NEXUS_A2A_MODEL_URL="$NEXUS_A2A_MODEL_URL" NEXUS_A2A_TEST_MODEL="$MODEL" \
  cargo_in_workspace test "${MANIFEST[@]}" --locked -q -p runtime --test mailbox_nexus_live \
  managed_subprocess::managed_scode_reads_cancels_exits_and_resumes_in_a_new_process \
  -- --ignored --exact --nocapture | tee "$AUTHON_DATA_DIR/managed-subprocess.log"
grep -F 'test result: ok. 1 passed; 0 failed;' "$AUTHON_DATA_DIR/managed-subprocess.log" >/dev/null
echo 'MANAGED SCODE PROCESS E2E OK'

echo "== one remote context: transcript, lease, workspace, mailbox and model on the daemon =="
NEXUS_A2A_TEST_ENDPOINT="$ENDPOINT" NEXUS_A2A_TEST_CERT_DIR="$SESSION_BUNDLE" \
  cargo_in_workspace test "${MANIFEST[@]}" --locked -q -p engine-acp --test managed_remote_live \
  a_remote_managed_agent_keeps_every_effect_on_its_daemon \
  -- --ignored --exact --nocapture | tee "$AUTHON_DATA_DIR/managed-remote.log"
grep -F 'test result: ok. 1 passed; 0 failed;' "$AUTHON_DATA_DIR/managed-remote.log" >/dev/null
echo 'MANAGED REMOTE CONTEXT E2E OK'
