#!/usr/bin/env bash
# Two nodes, one runner: the cross-machine A2A path without a second machine.
#
# `run.sh` stands up ONE daemon, which can only ever prove that a client and a
# server agree. The thing A2A actually rests on is a different mechanism: a
# receiver parks a blocking tail against ITS OWN node, a peer writes to ANOTHER,
# and the envelope arrives because the write is a raft proposal applied on both -
# with each node's own apply observer waking its local waiters. None of that
# exists with one node, so no single-node run says anything about it.
#
# Auth-on, cert-only: scode is cert-only (no plaintext dial), so both nodes run
# mTLS. The founder self-bootstraps a CA and accepts enrollments; the joiner
# auto-enrolls at boot with the founder's token (--token + --peers), the
# k3s/kubeadm model the daemon documents. Client bundles are minted offline from
# the founder's data dir. The bring-up primitives are shared via lib.sh.
#
# Usage:
#   e2e/nexus-a2a/run-cross-node.sh
#   NEXUSD_BIN=/path/to/nexusd-cluster e2e/nexus-a2a/run-cross-node.sh
set -euo pipefail
cd "$(dirname "$0")"

# `/agents=sharedzone` must reach the daemon as written; MSYS would rewrite it.
NO_CONV="MSYS_NO_PATHCONV=1"
# shellcheck source=lib.sh
. ./lib.sh

FOUNDER_PORT="${NEXUS_A2A_FOUNDER_PORT:-2141}"
JOINER_PORT="${NEXUS_A2A_JOINER_PORT:-2143}"  # NOT 2142: that is the founder's enrollment port (founder_port+1)
FOUNDER="127.0.0.1:${FOUNDER_PORT}"
JOINER="127.0.0.1:${JOINER_PORT}"
FOUNDER_ENDPOINT="https://127.0.0.1:${FOUNDER_PORT}"
JOINER_ENDPOINT="https://127.0.0.1:${JOINER_PORT}"
ZONE="${NEXUS_A2A_ZONE:-sharedzone}"
SECRET="${NEXUS_API_KEY_SECRET:-scode-e2e-secret}"
RUST_DIR="${RUST_DIR:-$(cd ../../rust && pwd)}"

if [ -z "${NEXUSD_BIN:-}" ]; then
  # Through `bash`, not `./`: a Windows checkout can arrive without the exec bit.
  NEXUSD_BIN="$(bash ./fetch-daemon.sh)" || exit 1
fi

DATA_DIR="$(mktemp -d "${TMPDIR:-/tmp}/scode-a2a-xnode.XXXXXX")"
FOUNDER_PID=
JOINER_PID=
cleanup() {
  local status=$?
  [ -n "$JOINER_PID" ] && kill "$JOINER_PID" 2>/dev/null || true
  [ -n "$FOUNDER_PID" ] && kill "$FOUNDER_PID" 2>/dev/null || true
  if [ "$status" -ne 0 ]; then
    for node in founder joiner; do
      echo "== $node log after cross-node failure ==" >&2
      # Startup logs contain a join token. Never expose it in CI output.
      tail -100 "$DATA_DIR/$node.log" 2>/dev/null | sed -E 's/K10[^[:space:]]+/[REDACTED JOIN TOKEN]/g' >&2 || true
    done
  fi
  if [ "${NEXUS_A2A_KEEP_WORK:-0}" = 1 ]; then
    echo "Retained cross-node work directory: $DATA_DIR"
  else
    rm -rf "$DATA_DIR" 2>/dev/null || true
  fi
}
trap cleanup EXIT

mkdir -p "$DATA_DIR"/{a,b}/{data,id}

wait_log() {
  local file="$1" needle="$2" budget="$3" i
  for i in $(seq 1 "$budget"); do
    grep -q "$needle" "$file" 2>/dev/null && { echo "   $needle (after ~${i}s)"; return 0; }
    sleep 1
  done
  echo "!! '$needle' never appeared in $file" >&2
  tail -40 "$file" >&2
  return 1
}

# Fresh data AND identity per node. `identity.json` carries the peer address
# book, so a reused one leaves a daemon rejoining a cluster that is not there.
# TLS is ON (NEXUS_NO_TLS unset); --accept-enrollments makes the founder sign
# joiner certs and print a ready join token at boot.
echo "== founder on :${FOUNDER_PORT} (owns ${ZONE}, TLS on, accepts enrollments) =="
env $NO_CONV \
NEXUS_DATA_DIR="$(native_path "$DATA_DIR/a/data")" \
NEXUS_IDENTITY_DIR="$(native_path "$DATA_DIR/a/id")" \
NEXUS_API_KEY_SECRET="$SECRET" \
NEXUS_ADVERTISE_ADDR="$FOUNDER" \
NEXUS_CLUSTER_INIT="$ZONE" \
NEXUS_CLUSTER_INIT_MOUNTS="/agents=$ZONE" \
RUST_LOG=info \
  "$NEXUSD_BIN" --bind-addr "0.0.0.0:${FOUNDER_PORT}" --accept-enrollments \
  >"$DATA_DIR/founder.log" 2>&1 &
FOUNDER_PID=$!

wait_log "$DATA_DIR/founder.log" "Static topology applied" 45

# Mint before admitting the second voter. An offline mint needs the founder
# stopped; stopping it after the join leaves a two-voter zone without quorum
# and races the test against leader recovery. No test write is retried. Both
# agents are CA-signed off the founder's data dir before replication starts.
echo "== stop the founder, mint client bundles, restart =="
kill "$FOUNDER_PID" 2>/dev/null || true
wait "$FOUNDER_PID" 2>/dev/null || true
FOUNDER_PID=
AUTHON_NO_CONV="$NO_CONV"
AUTHON_NEXUSD_BIN="$NEXUSD_BIN"
AUTHON_PORT="$FOUNDER_PORT"
AUTHON_ZONE="$ZONE"
AUTHON_DATA_DIR="$DATA_DIR/a"
PROBE_BUNDLE="$(authon_mint "live-probe")" || { tail -40 "$DATA_DIR/founder.log" >&2; exit 1; }
RECEIVER_BUNDLE="$(authon_mint "team-lead")" || exit 1
SENDER_BUNDLE="$(authon_mint "xnode-sender")" || exit 1

echo "== restart the founder =="
mv "$DATA_DIR/founder.log" "$DATA_DIR/founder-bootstrap.log"
env $NO_CONV \
NEXUS_DATA_DIR="$(native_path "$DATA_DIR/a/data")" \
NEXUS_IDENTITY_DIR="$(native_path "$DATA_DIR/a/id")" \
NEXUS_API_KEY_SECRET="$SECRET" \
NEXUS_ADVERTISE_ADDR="$FOUNDER" \
RUST_LOG=info \
  "$NEXUSD_BIN" --bind-addr "0.0.0.0:${FOUNDER_PORT}" --accept-enrollments \
  >"$DATA_DIR/founder.log" 2>&1 &
FOUNDER_PID=$!
wait_log "$DATA_DIR/founder.log" "stream-wakeup" 45

# The founder prints a join token (`K10<pw>::server:SHA256:<ca-fp>`) at boot when
# started with --accept-enrollments. Grab it for the joiner's auto-enroll.
echo "== reading the founder's join token =="
TOKEN=
for i in $(seq 1 30); do
  TOKEN="$(grep -oE 'K10[^[:space:]]+::server:SHA256:[A-Za-z0-9+/=]+' "$DATA_DIR/founder.log" 2>/dev/null | tail -1 | tr -d '\r')"
  [ -n "$TOKEN" ] && break
  sleep 1
done
[ -n "$TOKEN" ] || { echo "!! founder never printed a join token" >&2; tail -40 "$DATA_DIR/founder.log" >&2; exit 1; }
echo "   got a token"

# The joiner auto-enrolls at boot: --token presents it to the founder's
# enrollment port (derived as founder_port+1), which signs the joiner's mTLS
# cert; --peers points raft at the founder. No plaintext, no insecure-no-auth.
echo "== joiner on :${JOINER_PORT} (auto-enroll + DiscoverZones via the founder) =="
env $NO_CONV \
NEXUS_DATA_DIR="$(native_path "$DATA_DIR/b/data")" \
NEXUS_IDENTITY_DIR="$(native_path "$DATA_DIR/b/id")" \
NEXUS_ADVERTISE_ADDR="$JOINER" \
NEXUS_PEERS="$FOUNDER" \
RUST_LOG=info \
  "$NEXUSD_BIN" --bind-addr "0.0.0.0:${JOINER_PORT}" --token "$TOKEN" \
  >"$DATA_DIR/joiner.log" 2>&1 &
JOINER_PID=$!

# Ready when it has caught up to the leader's log, not merely when it answered a
# dial: the tests read a stream the founder created, and a joiner that is up but
# behind returns not-found.
wait_log "$DATA_DIR/joiner.log" "local raft state caught up to leader" 90

# Both observers must be armed or the wake below cannot happen on either side.
echo "== waiting for both nodes to arm their a2a stream-wakeup observers =="
for node in founder joiner; do
  wait_log "$DATA_DIR/$node.log" "stream-wakeup" 30
done

echo "== [cross-node] a peer node's write wakes a parked blocking tail =="
NEXUS_A2A_TEST_ENDPOINT="$JOINER_ENDPOINT" NEXUS_A2A_TEST_PEER_ENDPOINT="$FOUNDER_ENDPOINT" \
  NEXUS_A2A_TEST_CERT_DIR="$PROBE_BUNDLE" \
  cargo test --manifest-path "$RUST_DIR/Cargo.toml" -q -p runtime \
  --test mailbox_nexus_live live_blocking_read_wakes_on_a_peer_nodes_write -- --ignored --nocapture

# The same workflow `run.sh` runs on one node, with the sender moved to the
# other. `send` writes through the founder; every assertion reads the joiner.
echo "== [cross-node] two scode processes, one per node =="
NEXUS_A2A_TEST_ENDPOINT="$JOINER_ENDPOINT" NEXUS_A2A_TEST_PEER_ENDPOINT="$FOUNDER_ENDPOINT" \
  NEXUS_A2A_TEST_RECEIVER_CREDENTIAL="$RECEIVER_BUNDLE" \
  NEXUS_A2A_TEST_SENDER_CREDENTIAL="$SENDER_BUNDLE" \
  cargo test --manifest-path "$RUST_DIR/Cargo.toml" -q -p rusty-sudocode-cli \
  --test pty_agent_duet -- --nocapture

# Discovery, asserted from BOTH endpoints. The live bug was not an empty listing
# but a DIFFERENT listing per node, which only a second endpoint can catch.
echo "== [cross-node] every peer is discoverable from either node =="
NEXUS_A2A_TEST_ENDPOINT="$JOINER_ENDPOINT" NEXUS_A2A_TEST_PEER_ENDPOINT="$FOUNDER_ENDPOINT" \
  NEXUS_A2A_TEST_CERT_DIR="$PROBE_BUNDLE" \
  cargo test --manifest-path "$RUST_DIR/Cargo.toml" -q -p runtime \
  --test mailbox_nexus_live live_agent_list_sees_every_peer_from_either_node -- --ignored --nocapture

echo "CROSS-NODE E2E OK"
