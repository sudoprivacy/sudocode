#!/usr/bin/env bash
# Auth ON: mTLS, a minted agent cert, and a `from` the sender cannot choose.
#
# `run.sh` and its siblings prove DELIVERY: an envelope moves, a blocking tail
# wakes, two nodes agree. None of them asserts identity, and `from` is an
# address — the convention turns it straight back into a path — so a forgeable
# one is a way to make a peer's reply go somewhere the sender chose. That is the
# one property asserted here.
#
# What auth-on changes is NOT whether the node stamps `from`. Measured against
# v0.7.20, a node stamps it with the dialling cert's agent id in both postures:
# `--insecure-no-auth` makes authentication optional, not the stamp absent. What
# it changes is whether that stamp is worth anything — under the flag an identity
# is recorded but not required, and an identity a node applies without requiring
# guarantees nothing. Auth-on is the posture with the credential store bound,
# which is the only posture the property can be CLAIMED in; the reason to run it
# is not that auth-off would show the forgery succeeding.
#
# The bring-up is the one `nexus-vfs`'s own `agent_signed_authorship` uses, so a
# failure here is about scode rather than about a posture this repo invented:
#
#   1. boot TLS-on — the CA and node cert bootstrap themselves into <data>/tls
#   2. STOP, because the mint is offline: it opens the data dir the daemon locks
#   3. mint a CA-signed agent bundle (agent.pem / agent-key.pem / ca.pem)
#   4. restart, and dial the mTLS plane with that bundle
#
# Its own daemon, port and data dir, because the mint is offline against that
# dir and the test pins `NEXUS_A2A_TEST_IDENTITY` to the id this script minted:
# the pair has to come from one mint. Sharing another script's daemon would mean
# sharing its bundle name too.
#
# Usage:
#   e2e/nexus-a2a/run-auth-on.sh
#   NEXUSD_BIN=/path/to/nexusd-cluster e2e/nexus-a2a/run-auth-on.sh
set -euo pipefail
cd "$(dirname "$0")"

# `/agents=sharedzone` must reach the daemon as written; Git Bash's MSYS runtime
# rewrites absolute-Unix-looking values before exec. Set per launch, because
# cargo below is invoked with a POSIX manifest path that needs the conversion
# the daemon must not get. Inert elsewhere.
NO_CONV="MSYS_NO_PATHCONV=1"
# shellcheck source=lib.sh
. ./lib.sh

PORT="${NEXUS_A2A_AUTHON_PORT:-2161}"
ENDPOINT="https://127.0.0.1:${PORT}"
ZONE="${NEXUS_A2A_ZONE:-sharedzone}"
AGENT="${NEXUS_A2A_AUTHON_AGENT:-authon-sender}"
RUST_DIR="${RUST_DIR:-$(cd ../../rust && pwd)}"
CARGO_TEST=(cargo test --manifest-path "$RUST_DIR/Cargo.toml" -q -p runtime --test mailbox_nexus_live)

if [ -z "${NEXUSD_BIN:-}" ]; then
  NEXUSD_BIN="$(bash ./fetch-daemon.sh)" || exit 1
fi

DATA_DIR="$(mktemp -d "${TMPDIR:-/tmp}/scode-a2a-authon.XXXXXX")"
DAEMON_PID=
cleanup() {
  [ -n "$DAEMON_PID" ] && kill "$DAEMON_PID" 2>/dev/null || true
  rm -rf "$DATA_DIR" 2>/dev/null || true
}
trap cleanup EXIT

mkdir -p "$DATA_DIR"/{data,id}

# The api-key secret has to be the same for the daemon and the offline mint, or
# the hashes do not line up and a minted credential authenticates as nobody.
# TLS is ON by its absence: NEXUS_NO_TLS is deliberately unset.
daemon_env=(
  "NEXUS_DATA_DIR=$(native_path "$DATA_DIR/data")"
  "NEXUS_IDENTITY_DIR=$(native_path "$DATA_DIR/id")"
  "NEXUS_API_KEY_SECRET=${NEXUS_API_KEY_SECRET:-scode-e2e-secret}"
  "NEXUS_ADVERTISE_ADDR=127.0.0.1:${PORT}"
  "NEXUS_CLUSTER_INIT=$ZONE"
  "NEXUS_CLUSTER_INIT_MOUNTS=/agents=$ZONE"
  "RUST_LOG=${RUST_LOG:-info}"
)

# Where this boot's output starts. The log is opened in append mode so that a
# failure can be read across both boots, which also means every line the first
# boot wrote is still in the file when the second one starts — and
# `wait_for_log` below used to grep the whole file. Step 4 therefore matched
# boot 1's "Zone ... registered" and returned two milliseconds after asking,
# having waited for nothing:
#
#   == 4. restart TLS-on ==
#      Zone 'sharedzone' registered (after ~1s)     <- 2ms after the line above
#
# The test then dialled a listener that had not bound yet and failed with
# `tcp connect error`, about one CI run in three.
LOG_FROM=1

boot() {
  LOG_FROM=$(( $(wc -c <"$DATA_DIR/daemon.log" 2>/dev/null || echo 0) + 1 ))
  env $NO_CONV "${daemon_env[@]}" \
    "$NEXUSD_BIN" --bind-addr "0.0.0.0:${PORT}" >>"$DATA_DIR/daemon.log" 2>&1 &
  DAEMON_PID=$!
}

wait_for_log() {
  local needle="$1" budget="$2" i
  for i in $(seq 1 "$budget"); do
    # Only what this boot wrote — see LOG_FROM.
    if tail -c "+$LOG_FROM" "$DATA_DIR/daemon.log" 2>/dev/null | grep -q "$needle"; then
      echo "   $needle (after ~${i}s)"
      return 0
    fi
    sleep 1
  done
  echo "!! daemon never logged '$needle'" >&2
  tail -40 "$DATA_DIR/daemon.log" >&2
  return 1
}

# A log line says the daemon reached some internal state; it does not say the
# socket is accepting, and "accepting" is exactly what the next step needs —
# `tcp connect error` is the failure it reports when it dials too early. So
# dial it here first. Bash's own /dev/tcp is used rather than nc or a TLS
# client because it needs no extra tool on any runner, and a bare TCP connect
# is the whole question: an mTLS listener refuses the handshake, which is a
# different error and means the socket was up.
wait_for_port() {
  local budget="$1" i
  for i in $(seq 1 "$budget"); do
    if (exec 3<>"/dev/tcp/127.0.0.1/${PORT}") 2>/dev/null; then
      echo "   :${PORT} accepting (after ~${i}s)"
      return 0
    fi
    sleep 1
  done
  echo "!! :${PORT} never accepted a connection" >&2
  tail -40 "$DATA_DIR/daemon.log" >&2
  return 1
}

echo "== 1. founder on :${PORT}, TLS on (CA bootstraps itself) =="
boot
wait_for_log "Static topology applied" 45

# The mint opens the same data dir the daemon holds an exclusive lock on, so the
# daemon has to be down for it. This is the documented posture, not a
# workaround: a credential is not a network resource.
echo "== 2. stop, for the offline mint =="
kill "$DAEMON_PID" 2>/dev/null || true
wait "$DAEMON_PID" 2>/dev/null || true
DAEMON_PID=

echo "== 3. mint a CA-signed agent bundle for ${AGENT} =="
BUNDLE="$(env $NO_CONV "${daemon_env[@]}" RUST_LOG=error \
  "$NEXUSD_BIN" auth mint --subject-type agent --subject-id "$AGENT" \
  --name e2e --allow-existing 2>/dev/null | tail -1 | tr -d '\r')"
[ -n "$BUNDLE" ] || { echo "!! mint printed no bundle path" >&2; exit 1; }
# The daemon prints a path in its own form; `shell_path` is the one place
# that knows how to read one. The tests below get the daemon-form value,
# because they hand it back to a Rust `File::open` rather than to this shell.
BUNDLE_LOCAL="$(shell_path "$BUNDLE")"
echo "   $BUNDLE"
for f in agent.pem agent-key.pem ca.pem; do
  [ -f "$BUNDLE_LOCAL/$f" ] || { echo "!! the bundle has no $f" >&2; exit 1; }
done

echo "== 4. restart TLS-on =="
boot
wait_for_log "Zone '$ZONE' registered" 45
wait_for_port 45

echo "== [auth-on] the node decides who a message is from =="
NEXUS_A2A_TEST_ENDPOINT="$ENDPOINT" \
NEXUS_A2A_TEST_CERT_DIR="$BUNDLE" \
NEXUS_A2A_TEST_IDENTITY="$AGENT" \
  "${CARGO_TEST[@]}" live_authenticated_from_cannot_be_forged -- --ignored --nocapture

echo "AUTH-ON E2E OK"
