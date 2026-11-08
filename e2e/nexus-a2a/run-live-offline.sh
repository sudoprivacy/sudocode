#!/usr/bin/env bash
# Real model + authenticated Nexus + first receiver start. Uses the caller's
# private live PTY config; no provider or transport fallback is permitted.
set -euo pipefail
cd "$(dirname "$0")"
. ./lib.sh

test -f "${SUDO_CODE_CONFIG_HOME:?Set the private live PTY configuration directory}/sudocode.json"
AUTHON_NEXUSD_BIN="${NEXUSD_BIN:-$(bash ./fetch-daemon.sh)}"
AUTHON_PORT="${NEXUS_A2A_HOST_PORT:-2147}"
AUTHON_ZONE=sharedzone
AUTHON_DATA_DIR="$(mktemp -d "${TMPDIR:-/tmp}/scode-a2a-live-offline.XXXXXX")"
AUTHON_DAEMON_PID=
RUST_DIR="${RUST_DIR:-$(cd ../../rust && pwd)}"
cleanup() {
  authon_stop
  if [ -z "${NEXUS_A2A_KEEP_WORK:-}" ]; then
    rm -rf "$AUTHON_DATA_DIR"
  else
    echo "== work dir kept: $AUTHON_DATA_DIR =="
  fi
}
trap cleanup EXIT
mkdir -p "$AUTHON_DATA_DIR"/{data,id}
authon_boot
authon_wait_log 'Static topology applied' 45
authon_stop
receiver_bundle="$(authon_mint team-lead)"
sender_bundle="$(authon_mint duet-sender)"
authon_boot
authon_wait_log 'VFS data plane ready' 45

if ! output="$(NEXUS_A2A_TEST_ENDPOINT="https://127.0.0.1:${AUTHON_PORT}" \
    NEXUS_A2A_TEST_RECEIVER_CREDENTIAL="$receiver_bundle" \
    NEXUS_A2A_TEST_SENDER_CREDENTIAL="$sender_bundle" \
    SCODE_TEST_BACKEND=live \
    cargo_in_workspace test --locked -p rusty-sudocode-cli --test pty_agent_duet \
      a_message_sent_before_the_receiver_starts_survives_on_every_transport \
      -- --exact --test-threads=1 --nocapture 2>&1)"; then
  printf '%s\n' "$output"
  tail -40 "$AUTHON_DATA_DIR/daemon.log" >&2
  exit 1
fi
printf '%s\n' "$output"
if printf '%s' "$output" | grep -q 'SKIP('; then
  echo '!! the funded offline Nexus journey skipped' >&2
  exit 1
fi
printf '%s' "$output" | grep -q '1 passed; 0 failed; 0 ignored'
echo 'Funded offline Nexus PTY OK'
