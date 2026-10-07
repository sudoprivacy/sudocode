#!/usr/bin/env bash
# Deterministic E2E for the standalone nexus-A2A client (X), auth-on.
#
# Brings up a real `nexusd-cluster` founder with TLS + auth and drives the
# ignored `runtime` integration tests (`mailbox_nexus_live`) against it - the
# one thing unit tests can't cover: that `ensure_stream` + `stream_write` +
# `stream_read_at` actually move an envelope through a real gRPC server and a
# real DT_STREAM. No LLM, no secrets - always safe to run.
#
# scode is cert-only: it dials mTLS with a minted credential and never
# plaintext, so this harness dials exactly what production does. The bring-up
# (boot TLS -> stop -> offline mint -> restart) is factored into lib.sh and
# shared with run-auth-on.sh / run-cross-node.sh.
#
# The optional 2-LLM co-host duet (a real `scode` sending to a daemon-hosted
# co-host agent that LLM-replies) runs only when SUDOROUTER_API_KEY (funded) and
# SCODE_BIN are both set - mirroring `subagent-parity-live.yml`'s gating.
#
# Usage:
#   e2e/nexus-a2a/run.sh
#   NEXUSD_BIN=/path/to/nexusd-cluster e2e/nexus-a2a/run.sh          # force a binary
#   SUDOROUTER_API_KEY=sk-... SCODE_BIN=/path/to/scode e2e/nexus-a2a/run.sh   # + duet
set -euo pipefail
cd "$(dirname "$0")"

# shellcheck source=lib.sh
. ./lib.sh

# NOT 2126. That is the port `serve-local` defaults to and therefore the one a
# developer's own daemon is already on - the harness would then either fail to
# bind or, worse, run its assertions against that daemon instead of the throwaway
# it thinks it started. Any override still works; only the default moved.
AUTHON_PORT="${NEXUS_A2A_HOST_PORT:-2143}"
AUTHON_ZONE="${NEXUS_A2A_ZONE:-sharedzone}"
ENDPOINT="https://127.0.0.1:${AUTHON_PORT}"
RUST_DIR="${RUST_DIR:-$(cd ../../rust && pwd)}"
CARGO_TEST=(cargo test --manifest-path "$RUST_DIR/Cargo.toml" -q -p runtime --test mailbox_nexus_live)

# A downloaded binary needs no image built by hand, which is what kept this
# harness off CI: building `nexusd-cluster` is a nexus-repo job and wants a
# GitHub token. `fetch-daemon.sh` resolves the version from this checkout's
# nexus-vfs pin, so the daemon MATCHES the client library rather than being
# whatever `latest` is. Through `bash`, not `./`: a repo cloned from a Windows
# checkout can arrive without the exec bit.
if [ -z "${NEXUSD_BIN:-}" ]; then
  NEXUSD_BIN="$(bash ./fetch-daemon.sh)" || exit 1
fi
AUTHON_NEXUSD_BIN="$NEXUSD_BIN"

AUTHON_DATA_DIR="$(mktemp -d "${TMPDIR:-/tmp}/scode-a2a-nexusd.XXXXXX")"
AUTHON_DAEMON_PID=
cleanup() {
  [ -n "$AUTHON_DAEMON_PID" ] && kill "$AUTHON_DAEMON_PID" 2>/dev/null || true
  rm -rf "$AUTHON_DATA_DIR" 2>/dev/null || true
}
trap cleanup EXIT
mkdir -p "$AUTHON_DATA_DIR"/{data,id}

daemon_logs() { tail -40 "$AUTHON_DATA_DIR/daemon.log" 2>/dev/null || true; }

echo "== 1. founder on :${AUTHON_PORT}, TLS on (CA bootstraps itself) =="
authon_boot
authon_wait_log "Static topology applied" 45

# The mint opens the same data dir the daemon holds an exclusive lock on, so the
# daemon has to be down for it. This is the documented posture, not a
# workaround: a credential is not a network resource. One bundle serves the
# identity-agnostic transport tests; the duet mints its own participants below.
echo "== 2. stop, for the offline mint =="
kill "$AUTHON_DAEMON_PID" 2>/dev/null || true
wait "$AUTHON_DAEMON_PID" 2>/dev/null || true
AUTHON_DAEMON_PID=

echo "== 3. mint a CA-signed agent bundle for the client =="
CLIENT_BUNDLE="$(authon_mint "live-probe")" || { daemon_logs; exit 1; }
echo "   $CLIENT_BUNDLE"

echo "== 4. restart TLS-on =="
authon_boot
authon_wait_log "Zone '$AUTHON_ZONE' registered" 45

echo "== waiting for a writable single-voter leader =="
# The probe's output is KEPT, not discarded. This loop retries because a fresh
# founder needs a moment to become writable, so every early failure is expected
# and printing each one is noise - but the LAST one is the diagnosis, and
# `2>/dev/null` threw it away. Everything that can go wrong before the daemon is
# reachable lands in this loop and used to read as "daemon never became
# writable": a panic on a missing `NEXUS_A2A_TEST_CERT_DIR`, and a build that
# never produced the test binary at all (`failed to find tool "cl.exe"`).
ready=
probe=
for i in $(seq 1 30); do
  if probe=$(NEXUS_A2A_TEST_ENDPOINT="$ENDPOINT" NEXUS_A2A_TEST_CERT_DIR="$CLIENT_BUNDLE" \
      "${CARGO_TEST[@]}" live_inbox_roundtrip -- --ignored 2>&1) \
      && printf '%s' "$probe" | grep -q "1 passed"; then
    echo "   writable on attempt $i"
    ready=1
    break
  fi
  sleep 4
done
if [ -z "$ready" ]; then
  echo "!! daemon never became writable - the last probe said:" >&2
  printf '%s\n' "$probe" >&2
  daemon_logs
  exit 1
fi

echo "== [deterministic] standalone A2A client round-trip =="
NEXUS_A2A_TEST_ENDPOINT="$ENDPOINT" NEXUS_A2A_TEST_CERT_DIR="$CLIENT_BUNDLE" \
  "${CARGO_TEST[@]}" live_inbox_roundtrip -- --ignored --nocapture

# One session on BOTH a local same-machine pair and this nexus daemon sees every
# peer from both in ONE listing, each tagged with where it was found. The union
# the Directory exists for, proven against a real daemon rather than a temp dir -
# so a regression that silently drops a namespace from `agent_list` fails here.
# /agents and /sessions are mounted from one zone; a router that drops the mount
# prefix aliases them, and `readdir /agents` then returns session ids. The daemon
# version comes from the nexus-vfs lock, so pin the invariant here: a bump or
# rollback past the fix must fail loudly instead of degrading every agent_list.
echo "== [deterministic] /agents and /sessions are distinct namespaces =="
NEXUS_A2A_TEST_ENDPOINT="$ENDPOINT" NEXUS_A2A_TEST_CERT_DIR="$CLIENT_BUNDLE" \
  "${CARGO_TEST[@]}" live_agents_and_sessions_are_not_the_same_namespace -- --ignored --nocapture

echo "== [deterministic] agent_list unions local + nexus peers =="
NEXUS_A2A_TEST_ENDPOINT="$ENDPOINT" NEXUS_A2A_TEST_CERT_DIR="$CLIENT_BUNDLE" \
  "${CARGO_TEST[@]}" live_directory_unions_local_and_nexus_peers -- --ignored --nocapture

# The seam the round-trip above leaves out. That one drives `Mailbox` directly,
# so it proves the transport while saying nothing about whether the tool reaches
# it, nor whether a receiver surfaces what arrives - and a `send` that wrote a
# local file while reporting success is the failure this whole path exists
# because of. This runs two real binaries: one calls the tool, the other's REPL
# is parked on its inbox. Mock model, so no key is needed; set
# SCODE_TEST_BACKEND=live to have a real model choose the call instead.
echo "== [deterministic] two scode processes, one daemon =="
NEXUS_A2A_TEST_ENDPOINT="$ENDPOINT" \
  NEXUS_A2A_TEST_RECEIVER_CREDENTIAL="$(authon_mint "team-lead")" \
  NEXUS_A2A_TEST_SENDER_CREDENTIAL="$(authon_mint "duet-sender")" \
  cargo test --manifest-path "$RUST_DIR/Cargo.toml" -q -p rusty-sudocode-cli \
  --test pty_agent_duet -- --nocapture

# A daemon that is alive and SILENT - the failure a unit test cannot construct.
#
# A dropped connection reports itself; a stopped process holds the socket open
# and answers nothing, which is how a standing receiver went deaf for four hours
# while looking idle (#696). `SIGSTOP` reproduces exactly that, and only the
# harness can do it, because only the harness knows the pid of the daemon it
# started - which is all the gate below checks.
#
# The PLATFORM skip lives in the test, not here. Windows has no SIGSTOP that
# leaves the socket open, and this script cannot tell: `AUTHON_DAEMON_PID` is set
# there like anywhere else, so a gate on it skips nothing. The test prints its
# own SKIP line and returns.
if [ -n "$AUTHON_DAEMON_PID" ]; then
  echo "== [deterministic] a silent server errors, then recovers =="
  NEXUS_A2A_TEST_ENDPOINT="$ENDPOINT" NEXUS_A2A_TEST_CERT_DIR="$CLIENT_BUNDLE" \
    NEXUS_A2A_TEST_DAEMON_PID="$AUTHON_DAEMON_PID" \
    "${CARGO_TEST[@]}" live_a_silent_server_errors_and_then_recovers -- --ignored --nocapture
else
  echo "== [skip] silent-server fault injection - needs the daemon's pid =="
fi

# ---- Optional: real 2-LLM co-host duet (gated) --------------------------------
if [ -n "${SUDOROUTER_API_KEY:-}" ] && [ -n "${SCODE_BIN:-}" ]; then
  echo "== [live] scode -> co-host duet =="
  R="${DUET_RESPONDER:-duet-bot}"
  MODEL="${DUET_MODEL:-claude-sonnet-4-6}"
  SELF="${DUET_SELF:-operator}"
  # Mint the two participants the duet needs: the operator scode dials with its
  # own bundle, and the responder inbox is provisioned + spawned by the co-host.
  SELF_BUNDLE="$(authon_mint "$SELF")"
  R_BUNDLE="$(authon_mint "$R")"
  # Provision the responder's inbox BEFORE spawning it, so the co-host arms its
  # watch at an empty tail and sees scode's message as new (not skipped).
  NEXUS_A2A_TEST_ENDPOINT="$ENDPOINT" NEXUS_A2A_TEST_CERT_DIR="$R_BUNDLE" \
    NEXUS_A2A_TEST_INBOX="$R" "${CARGO_TEST[@]}" live_ensure_inbox -- --ignored >/dev/null
  NEXUS_A2A_TEST_ENDPOINT="$ENDPOINT" NEXUS_A2A_TEST_CERT_DIR="$R_BUNDLE" \
    NEXUS_A2A_TEST_SPAWN="$R" NEXUS_A2A_TEST_MODEL="$MODEL" \
    "${CARGO_TEST[@]}" live_spawn_cohost -- --ignored --nocapture
  sleep 8
  NEXUS_A2A_ENDPOINT="$ENDPOINT" NEXUS_A2A_CREDENTIAL="$SELF_BUNDLE" \
    "$SCODE_BIN" --auth proxy --model "$MODEL" --permission-mode danger-full-access \
    --print "Call send once: to=$R message='reply with exactly one word: PONG' summary='ping'. Then stop."
  echo "   polling ${SELF}'s inbox for the co-host reply..."
  got=
  for i in $(seq 1 30); do
    out=$(NEXUS_A2A_TEST_ENDPOINT="$ENDPOINT" NEXUS_A2A_TEST_CERT_DIR="$SELF_BUNDLE" \
      NEXUS_A2A_TEST_INBOX="$SELF" \
      "${CARGO_TEST[@]}" live_collect_conversations -- --ignored --nocapture 2>&1 || true)
    if echo "$out" | grep -q "from=\"$R\""; then
      echo "   >>> DUET REPLY:"; echo "$out" | grep "from="; got=1; break
    fi
    sleep 3
  done
  [ -n "$got" ] || { echo "!! co-host never replied (see daemon logs)" >&2; daemon_logs; exit 1; }
else
  echo "== [skip] LLM duet - set SUDOROUTER_API_KEY + SCODE_BIN to enable =="
fi

echo "E2E OK"