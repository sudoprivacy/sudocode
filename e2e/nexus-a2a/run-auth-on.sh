#!/usr/bin/env bash
# Auth ON: mTLS, a minted agent cert, and a `from` the sender cannot choose.
#
# `run.sh` and `run-cross-node.sh` both boot `--insecure-no-auth`, where the
# daemon's stamp hook is fail-open and the authored `from` is preserved by
# design. That is fine for proving delivery, and useless for proving identity:
# `from` is an address — the convention turns it straight back into a path — so
# a forgeable one is a way to make a peer's reply go somewhere the sender chose.
#
# Only an auth-on node decides who a message is from, so only an auth-on run can
# show that it does. The bring-up is the one `nexus-vfs`'s own
# `agent_signed_authorship` uses, so a failure here is about scode rather than
# about a posture this repo invented:
#
#   1. boot TLS-on — the CA and node cert bootstrap themselves into <data>/tls
#   2. STOP, because the mint is offline: it opens the data dir the daemon locks
#   3. mint a CA-signed agent bundle (agent.pem / agent-key.pem / ca.pem)
#   4. restart, and dial the mTLS plane with that bundle
#
# Auth-ON is also why this cannot reuse the other scripts' daemon: a plaintext
# client is rejected outright, which the run below relies on rather than works
# around.
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
  "NEXUS_DATA_DIR=$DATA_DIR/data"
  "NEXUS_IDENTITY_DIR=$DATA_DIR/id"
  "NEXUS_API_KEY_SECRET=${NEXUS_API_KEY_SECRET:-scode-e2e-secret}"
  "NEXUS_ADVERTISE_ADDR=127.0.0.1:${PORT}"
  "NEXUS_CLUSTER_INIT=$ZONE"
  "NEXUS_CLUSTER_INIT_MOUNTS=/agents=$ZONE"
  "RUST_LOG=${RUST_LOG:-info}"
)

boot() {
  env $NO_CONV "${daemon_env[@]}" \
    "$NEXUSD_BIN" --bind-addr "0.0.0.0:${PORT}" >>"$DATA_DIR/daemon.log" 2>&1 &
  DAEMON_PID=$!
}

wait_for_log() {
  local needle="$1" budget="$2" i
  for i in $(seq 1 "$budget"); do
    if grep -q "$needle" "$DATA_DIR/daemon.log" 2>/dev/null; then
      echo "   $needle (after ~${i}s)"
      return 0
    fi
    sleep 1
  done
  echo "!! daemon never logged '$needle'" >&2
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
echo "   $BUNDLE"
for f in agent.pem agent-key.pem ca.pem; do
  # `ls` rather than `test -f`: the mint prints a Windows path, which a POSIX
  # test would miss while `ls` resolves it either way.
  ls "$BUNDLE/$f" >/dev/null 2>&1 || ls "$(cygpath -u "$BUNDLE" 2>/dev/null)/$f" >/dev/null 2>&1 \
    || { echo "!! the bundle has no $f" >&2; exit 1; }
done

echo "== 4. restart TLS-on =="
boot
wait_for_log "Zone '$ZONE' registered" 45

echo "== [auth-on] the node decides who a message is from =="
NEXUS_A2A_TEST_ENDPOINT="$ENDPOINT" \
NEXUS_A2A_TEST_CERT_DIR="$BUNDLE" \
NEXUS_A2A_TEST_IDENTITY="$AGENT" \
  "${CARGO_TEST[@]}" live_authenticated_from_cannot_be_forged -- --ignored --nocapture

echo "AUTH-ON E2E OK"
