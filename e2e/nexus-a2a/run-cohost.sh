#!/usr/bin/env bash
# A co-host agent reads its inbox, runs a turn, and replies - no image, no key.
#
# The third plane. `run.sh` and its siblings cover agents that are CLIENTS of a
# daemon; a co-host agent runs INSIDE one, so its receive loop, its turn and its
# `send` are the daemon's own process rather than a `scode` talking to it.
# Nothing else here exercises that, and it has had real bugs of its own - the
# re-reply storm that a durable cursor fixed was this loop.
#
# The daemon is built from THIS checkout (`rust/crates/nexusd-cohost`), so the
# agent inside it is the code in this diff. The provider config points at this
# repo's mock Anthropic service, so the agent's turn is deterministic and free.
#
# Auth-on, cert-only: scode dials mTLS with a minted credential (never
# plaintext), so this harness dials exactly what production does. The bring-up
# (boot TLS -> stop -> offline mint -> restart) is factored into lib.sh, shared
# with run.sh / run-auth-on.sh; here it boots the nexusd-cohost binary, which is
# a superset of nexusd-cluster and mints the same way.
#
# ## Usage
#
#   e2e/nexus-a2a/run-cohost.sh
#   NEXUSD_COHOST_BIN=/path/to/nexusd-cohost e2e/nexus-a2a/run-cohost.sh
#   NEXUS_A2A_MODEL_LIVE=1 NEXUS_A2A_MODEL_URL=https://api.sudorouter.ai \
#     NEXUS_A2A_MODEL_KEY=<key> e2e/nexus-a2a/run-cohost.sh
# Live mode spends model tokens and fails if its credentials are missing.
set -euo pipefail
cd "$(dirname "$0")"
# shellcheck source=lib.sh
. ./lib.sh

# NOT 2126: `serve-local` defaults to that, so a developer's own daemon is
# already there and the assertions below would reach it instead of the co-host
# this script starts.
AUTHON_PORT="${NEXUS_A2A_COHOST_PORT:-2144}"
AUTHON_ZONE="${NEXUS_A2A_ZONE:-sharedzone}"
ENDPOINT="https://127.0.0.1:${AUTHON_PORT}"
MOCK_PORT="${NEXUS_A2A_MOCK_PORT:-18080}"
AGENT="${NEXUS_A2A_COHOST_AGENT:-cohost-bot}"
# Must match the mock's `COHOST_REPLY_TO`: the scenario decides who the agent
# answers, and this is who waits for it.
OPERATOR="${NEXUS_A2A_COHOST_OPERATOR:-operator}"
REPLY="${NEXUS_A2A_COHOST_REPLY:-PONG from the co-host}"
# One model name for both the config and the spawn.
MODEL="${NEXUS_A2A_COHOST_MODEL:-claude-sonnet-4-6}"
RUST_DIR="${RUST_DIR:-$(cd ../../rust && pwd)}"
MANIFEST=("--manifest-path" "$RUST_DIR/Cargo.toml")
CARGO_TEST=(cargo test "${MANIFEST[@]}" -q -p runtime --test mailbox_nexus_live)

echo "== 0. the co-host daemon binary =="
BIN="${NEXUSD_COHOST_BIN:-}"
if [ -z "$BIN" ]; then
  # `--features daemon,driver-ai`: the bin is behind it so the workspace's own test and
  # clippy jobs do not compile a raft + tonic tree on three platforms (and do not
  # need `protoc`, which the macOS runners have not got).
  cargo build "${MANIFEST[@]}" -q -p nexusd-cohost --features daemon,driver-ai
  BIN="$(cargo metadata "${MANIFEST[@]}" --format-version 1 --no-deps \
         | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')/debug/nexusd-cohost"
  [ -x "$BIN" ] || BIN="$BIN.exe"
fi
[ -x "$BIN" ] || { echo "!! no nexusd-cohost binary at $BIN" >&2; exit 1; }
echo "   $BIN"

# The binary says which binary it is. This daemon and `nexusd-cluster` fail the
# same way when the wrong one is deployed - a session sits in `warming_up` -
# so "which binary is this pod running?" has to have an answer.
VERSION_SAID="$("$BIN" --version 2>&1 || true)"
case "$VERSION_SAID" in
  "nexusd-cohost "*) echo "   identity: $VERSION_SAID" ;;
  *) echo "!! --version must name this binary, got: $VERSION_SAID" >&2; exit 1 ;;
esac

MOCK_PID=
AUTHON_DAEMON_PID=
WORK_DIR=
cleanup() {
  [ -n "$AUTHON_DAEMON_PID" ] && kill "$AUTHON_DAEMON_PID" 2>/dev/null || true
  [ -n "$MOCK_PID" ] && kill "$MOCK_PID" 2>/dev/null || true
  # `NEXUS_A2A_KEEP_WORK=1` leaves the data dir, both logs and the generated
  # config behind. The failure dumps below are a tail; a real diagnosis usually
  # wants the whole daemon log and the transcript it wrote.
  if [ -n "$WORK_DIR" ] && [ -z "${NEXUS_A2A_KEEP_WORK:-}" ]; then
    rm -rf "$WORK_DIR" 2>/dev/null || true
  elif [ -n "$WORK_DIR" ]; then
    echo "== work dir kept: $WORK_DIR =="
  fi
}
trap cleanup EXIT

WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/scode-cohost.XXXXXX")"
# lib.sh's auth-on helpers boot AUTHON_NEXUSD_BIN against AUTHON_DATA_DIR; here
# that binary is the co-host daemon and the data lives under the work dir.
AUTHON_NEXUSD_BIN="$BIN"
AUTHON_DATA_DIR="$WORK_DIR"
mkdir -p "$AUTHON_DATA_DIR"/{data,id}

if [ "${NEXUS_A2A_MODEL_LIVE:-0}" = 1 ]; then
  : "${NEXUS_A2A_MODEL_URL:?live mode requires the provider URL}"
  : "${NEXUS_A2A_MODEL_KEY:?live mode requires a funded model key}"
  echo "== 1. live provider; credentials stay in the model mount =="
else
  echo "== 1. mock model on 127.0.0.1:${MOCK_PORT} =="
  # Built first, then run: `cargo run` would otherwise compile INTO the log this
  # waits on, and a cold build outlasts any sane readiness window.
  cargo build "${MANIFEST[@]}" -q -p mock-anthropic-service
  # Start the executable itself so cleanup owns the server PID. Killing
  # `cargo run` leaves its child holding the port on Windows.
  MOCK_BIN="$(cargo metadata "${MANIFEST[@]}" --format-version 1 --no-deps \
    | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')/debug/mock-anthropic-service"
  [ -x "$MOCK_BIN" ] || MOCK_BIN="$MOCK_BIN.exe"
  "$MOCK_BIN" --bind "127.0.0.1:${MOCK_PORT}" >"$WORK_DIR/mock.log" 2>&1 &
  MOCK_PID=$!
  for _ in $(seq 1 20); do
    grep -q MOCK_ANTHROPIC_BASE_URL "$WORK_DIR/mock.log" 2>/dev/null && break
    sleep 1
  done
  grep -q MOCK_ANTHROPIC_BASE_URL "$WORK_DIR/mock.log" \
    || { echo "!! the mock never came up" >&2; cat "$WORK_DIR/mock.log" >&2; exit 1; }
  echo "   up"

  NEXUS_A2A_MODEL_URL="http://127.0.0.1:${MOCK_PORT}"
fi

# ONE auth mode, deliberately. A co-host agent is spawned inside the daemon and
# never sees an `--auth` flag, so it takes whatever the config offers: hand it
# `api-key`/`anthropic`, the mode that speaks Anthropic's `/v1/messages`, which
# is the surface the mock serves. The model is declared from the same variable
# the spawn uses so the two cannot disagree.
echo "== 2. co-host config pointed at nexus:///model =="
CONFIG_HOME="$WORK_DIR/config"
mkdir -p "$CONFIG_HOME"
cat >"$CONFIG_HOME/sudocode.json" <<JSON
{
  "auth_modes": {
    "api-key": {
      "anthropic": {
        "baseUrl": "nexus:///model"
      }
    }
  },
  "models": {
    "${MODEL}": {
      "alias": "${MODEL}",
      "name": "mock",
      "input": ["text"],
      "providers": { "api-key": { "provider": "anthropic", "model": "${MODEL}" } }
    }
  }
}
JSON
export SUDO_CODE_CONFIG_HOME="$(native_path "$CONFIG_HOME")"
echo "   $CONFIG_HOME/sudocode.json"

# `info`, not the daemon's default `warn`: when the agent does not answer, the
# question is always WHERE it stopped, and at `warn` the log says none of it.
export RUST_LOG="${RUST_LOG:-info}"

echo "== 3. co-host daemon on :${AUTHON_PORT}, TLS on =="
authon_boot
authon_wait_log "Zone '$AUTHON_ZONE' registered" 45

# The mint opens the same data dir the daemon locks, so it must be down for it.
echo "== 3b. stop, mint the client bundle, restart =="
kill "$AUTHON_DAEMON_PID" 2>/dev/null || true
wait "$AUTHON_DAEMON_PID" 2>/dev/null || true
AUTHON_DAEMON_PID=
CLIENT_ID="live-probe"
[ "${NEXUS_A2A_MODEL_LIVE:-0}" != 1 ] || CLIENT_ID="$OPERATOR"
CLIENT_BUNDLE="$(authon_mint "$CLIENT_ID")" || { tail -40 "$WORK_DIR/daemon.log" >&2; exit 1; }
authon_boot
authon_wait_log "Zone '$AUTHON_ZONE' registered" 45

ready=
for i in $(seq 1 30); do
  if NEXUS_A2A_TEST_ENDPOINT="$ENDPOINT" NEXUS_A2A_TEST_CERT_DIR="$CLIENT_BUNDLE" \
       "${CARGO_TEST[@]}" live_inbox_roundtrip -- --ignored 2>/dev/null | grep -q "1 passed"; then
    echo "   writable after ~$((i * 4))s"
    ready=1
    break
  fi
  sleep 4
done
[ -n "$ready" ] || { echo "!! the co-host daemon never became writable" >&2; \
  tail -40 "$WORK_DIR/daemon.log" >&2; exit 1; }

echo "== 3c. provision the model mount =="
NEXUS_A2A_TEST_ENDPOINT="$ENDPOINT" \
  NEXUS_A2A_MODEL_TLS_DIR="$(native_path "$WORK_DIR/data/tls")" \
  NEXUS_A2A_MODEL_URL="$NEXUS_A2A_MODEL_URL" \
  NEXUS_A2A_MODEL_ZONE="model" \
  NEXUS_A2A_MODEL_STORAGE="$(native_path "$WORK_DIR/model-cache")" \
  "${CARGO_TEST[@]}" live_mount_cohost_model -- --ignored --nocapture

# THE PAIR, before the agent starts. The conversation must exist before the
# spawn (the co-host arms its tail on what its chat list names at startup), and
# it must be the AGENT<->OPERATOR conversation (a conversation is addressed by
# its pair).
echo "== 4. provision the ${AGENT}<->${OPERATOR} conversation =="
NEXUS_A2A_TEST_ENDPOINT="$ENDPOINT" NEXUS_A2A_TEST_CERT_DIR="$CLIENT_BUNDLE" \
  NEXUS_A2A_TEST_INBOX="$AGENT" NEXUS_A2A_TEST_PEER="$OPERATOR" \
  "${CARGO_TEST[@]}" live_ensure_inbox -- --ignored >/dev/null
echo "   both sides filed"

if [ "${NEXUS_A2A_MODEL_LIVE:-0}" = 1 ]; then
  echo "== 5. real-model child and two-turn VFS workflow =="
  if ! NEXUS_A2A_TEST_ENDPOINT="$ENDPOINT" NEXUS_A2A_TEST_CERT_DIR="$CLIENT_BUNDLE" \
    NEXUS_A2A_MODEL_TLS_DIR="$(native_path "$WORK_DIR/data/tls")" \
    NEXUS_A2A_TEST_INBOX="$AGENT" NEXUS_A2A_TEST_REPLY_TO="$OPERATOR" \
    NEXUS_A2A_TEST_MODEL="$MODEL" \
    "${CARGO_TEST[@]}" live_cohost_model_workflow -- --ignored --nocapture; then
    echo "!! the live model workflow failed." >&2
    tail -120 "$WORK_DIR/daemon.log" >&2 || true
    exit 1
  fi
  echo "LIVE COHOST E2E OK"
  exit 0
fi

echo "== 5. spawn the co-host agent =="
NEXUS_A2A_TEST_ENDPOINT="$ENDPOINT" NEXUS_A2A_TEST_CERT_DIR="$CLIENT_BUNDLE" \
  NEXUS_A2A_TEST_SPAWN="$AGENT" NEXUS_A2A_TEST_MODEL="$MODEL" \
  "${CARGO_TEST[@]}" live_spawn_cohost -- --ignored --nocapture

echo "== 6. send it a message, and wait for the agent's own reply =="
# The agent runs inside the daemon, so when it does not answer, the daemon's log
# is the only place that says why. Dumping it on failure is the difference
# between a diagnosis and a guess.
if ! NEXUS_A2A_TEST_ENDPOINT="$ENDPOINT" NEXUS_A2A_TEST_CERT_DIR="$CLIENT_BUNDLE" \
  NEXUS_A2A_TEST_INBOX="$AGENT" \
  NEXUS_A2A_TEST_REPLY_TO="$OPERATOR" \
  NEXUS_A2A_TEST_REPLY_BODY="$REPLY" \
  "${CARGO_TEST[@]}" live_cohost_reads_its_inbox_and_replies -- --ignored --nocapture; then
  echo "!! the co-host did not reply." >&2
  echo "---- co-host daemon log ----" >&2
  tail -120 "$WORK_DIR/daemon.log" >&2 || true
  echo "---- mock provider log ----" >&2
  tail -30 "$WORK_DIR/mock.log" >&2 || true
  exit 1
fi

echo "== 7. delegate with VFS context and conflicting host instructions =="
NEXUS_A2A_TEST_ENDPOINT="$ENDPOINT" NEXUS_A2A_TEST_CERT_DIR="$CLIENT_BUNDLE" \
  NEXUS_A2A_MODEL_TLS_DIR="$(native_path "$WORK_DIR/data/tls")" \
  NEXUS_A2A_TEST_INBOX="$AGENT" NEXUS_A2A_TEST_REPLY_TO="$OPERATOR" \
  NEXUS_A2A_TEST_MODEL="$MODEL" \
  "${CARGO_TEST[@]}" live_cohost_subagent_context -- --ignored --nocapture

echo "COHOST E2E OK"
