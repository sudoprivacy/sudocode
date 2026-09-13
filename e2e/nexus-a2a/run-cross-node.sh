#!/usr/bin/env bash
# Two nodes, one runner: the cross-machine A2A path without a second machine.
#
# `run.sh` stands up ONE daemon, which can only ever prove that a client and a
# server agree. The thing A2A actually rests on is a different mechanism: a
# receiver parks a blocking tail against ITS OWN node, a peer writes to ANOTHER,
# and the envelope arrives because the write is a raft proposal applied on both
# — with each node's own apply observer waking its local waiters. None of that
# exists with one node, so no single-node run says anything about it.
#
# Topology is the one `nexus-vfs`'s own federation tests use, so a failure here
# is about scode rather than about an invented cluster:
#
#   FOUNDER  owns `sharedzone`, mounts it at /agents  (NEXUS_CLUSTER_INIT*)
#   JOINER   reaches it purely by boot-time DiscoverZones  (NEXUS_PEERS)
#
# Auth is OFF. The auth-ON crossing is nexus's `mtls-federation-e2e`, which has
# the certificate machinery this harness deliberately does not.
#
# Usage:
#   e2e/nexus-a2a/run-cross-node.sh
#   NEXUSD_BIN=/path/to/nexusd-cluster e2e/nexus-a2a/run-cross-node.sh
set -euo pipefail
cd "$(dirname "$0")"

# `/agents=sharedzone` must reach the daemon as written. Git Bash's MSYS runtime
# rewrites anything that looks like an absolute Unix path into a Windows one
# before exec, so the mount would arrive as `C:/Program Files/Git/agents=…` and
# the daemon refuses to boot on a topology it cannot parse.
#
# Set per daemon launch rather than exported: cargo is invoked below with a
# POSIX `--manifest-path`, and it needs exactly the conversion the daemon must
# not get. Inert outside Git Bash.
NO_CONV="MSYS_NO_PATHCONV=1"
# shellcheck source=lib.sh
. ./lib.sh

FOUNDER_PORT="${NEXUS_A2A_FOUNDER_PORT:-2141}"
JOINER_PORT="${NEXUS_A2A_JOINER_PORT:-2142}"
FOUNDER="127.0.0.1:${FOUNDER_PORT}"
JOINER="127.0.0.1:${JOINER_PORT}"
ZONE="${NEXUS_A2A_ZONE:-sharedzone}"
RUST_DIR="${RUST_DIR:-$(cd ../../rust && pwd)}"

if [ -z "${NEXUSD_BIN:-}" ]; then
  # Through `bash`, not `./`: a repo cloned from a Windows checkout can arrive
  # without the exec bit, and the failure then reads as a missing file.
  NEXUSD_BIN="$(bash ./fetch-daemon.sh)" || exit 1
fi

DATA_DIR="$(mktemp -d "${TMPDIR:-/tmp}/scode-a2a-xnode.XXXXXX")"
FOUNDER_PID=
JOINER_PID=
cleanup() {
  [ -n "$JOINER_PID" ] && kill "$JOINER_PID" 2>/dev/null || true
  [ -n "$FOUNDER_PID" ] && kill "$FOUNDER_PID" 2>/dev/null || true
  rm -rf "$DATA_DIR" 2>/dev/null || true
}
trap cleanup EXIT

mkdir -p "$DATA_DIR"/{a,b}/{data,id}

# Fresh data AND identity per node. A data-only wipe is not a fresh node:
# `identity.json` carries the peer address book and per-zone membership, so a
# reused one leaves a daemon rejoining a cluster that is not there.
echo "== founder on :${FOUNDER_PORT} (owns ${ZONE}, mounts /agents) =="
env $NO_CONV \
NEXUS_DATA_DIR="$(native_path "$DATA_DIR/a/data")" \
NEXUS_IDENTITY_DIR="$(native_path "$DATA_DIR/a/id")" \
NEXUS_ADVERTISE_ADDR="$FOUNDER" \
NEXUS_NO_TLS=true \
NEXUS_INSECURE_NO_AUTH=true \
NEXUS_CLUSTER_INIT="$ZONE" \
NEXUS_CLUSTER_INIT_MOUNTS="/agents=$ZONE" \
RUST_LOG=info \
  "$NEXUSD_BIN" --bind-addr "0.0.0.0:${FOUNDER_PORT}" >"$DATA_DIR/founder.log" 2>&1 &
FOUNDER_PID=$!

# Gate on the founder having APPLIED its topology, not on the port opening. A
# joiner that arrives before the mount is committed discovers a zone with no
# `/agents` in it and never retries, which surfaces much later as an empty
# inbox rather than as a boot failure.
echo "== waiting for the founder's static topology =="
ready=
for i in $(seq 1 45); do
  if grep -q "Static topology applied" "$DATA_DIR/founder.log" 2>/dev/null; then
    echo "   applied after ~${i}s"
    ready=1
    break
  fi
  sleep 1
done
if [ -z "$ready" ]; then
  echo "!! founder never applied its topology" >&2
  tail -40 "$DATA_DIR/founder.log" >&2
  exit 1
fi

echo "== joiner on :${JOINER_PORT} (DiscoverZones via the founder) =="
env $NO_CONV \
NEXUS_DATA_DIR="$(native_path "$DATA_DIR/b/data")" \
NEXUS_IDENTITY_DIR="$(native_path "$DATA_DIR/b/id")" \
NEXUS_ADVERTISE_ADDR="$JOINER" \
NEXUS_NO_TLS=true \
NEXUS_INSECURE_NO_AUTH=true \
NEXUS_PEERS="$FOUNDER" \
RUST_LOG=info \
  "$NEXUSD_BIN" --bind-addr "0.0.0.0:${JOINER_PORT}" >"$DATA_DIR/joiner.log" 2>&1 &
JOINER_PID=$!

# The joiner is ready when it has caught up to the leader's log, which is a
# stronger statement than "it answered a dial": the tests below read a stream
# the founder created, and a joiner that is up but behind returns not-found.
echo "== waiting for the joiner to catch up to the leader =="
ready=
for i in $(seq 1 60); do
  if grep -q "local raft state caught up to leader" "$DATA_DIR/joiner.log" 2>/dev/null; then
    echo "   caught up after ~${i}s"
    ready=1
    break
  fi
  sleep 1
done
if [ -z "$ready" ]; then
  echo "!! joiner never joined the zone" >&2
  tail -40 "$DATA_DIR/joiner.log" >&2
  tail -20 "$DATA_DIR/founder.log" >&2
  exit 1
fi

# Both observers have to be armed or the wake below cannot happen on either
# side. Asserting it here turns "the wake test timed out" into "the node never
# armed its observer", which is a different bug with a different owner.
for node in founder joiner; do
  grep -q "stream-wakeup" "$DATA_DIR/$node.log" \
    || { echo "!! $node never armed its a2a stream-wakeup observer" >&2; exit 1; }
done
echo "== both nodes armed their a2a stream-wakeup observers =="

echo "== [cross-node] a peer node's write wakes a parked blocking tail =="
NEXUS_A2A_TEST_ENDPOINT="$JOINER" NEXUS_A2A_TEST_PEER_ENDPOINT="$FOUNDER" \
  cargo test --manifest-path "$RUST_DIR/Cargo.toml" -q -p runtime \
  --test mailbox_nexus_live live_blocking_read_wakes_on_a_peer_nodes_write -- --ignored --nocapture

# The same workflow `run.sh` runs on one node, with the sender moved to the
# other. `send` writes through the founder; every assertion reads the joiner.
echo "== [cross-node] two scode processes, one per node =="
NEXUS_A2A_TEST_ENDPOINT="$JOINER" NEXUS_A2A_TEST_PEER_ENDPOINT="$FOUNDER" \
  cargo test --manifest-path "$RUST_DIR/Cargo.toml" -q -p rusty-sudocode-cli \
  --test pty_agent_duet -- --nocapture

echo "CROSS-NODE E2E OK"
