#!/usr/bin/env bash
# A co-host agent reads its inbox, runs a turn, and replies — no image, no key.
#
# The third plane. `run.sh` and its siblings cover agents that are CLIENTS of a
# daemon; a co-host agent runs INSIDE one, so its receive loop, its turn and its
# `send` are the daemon's own process rather than a `scode` talking to it.
# Nothing else here exercises that, and it has had real bugs of its own — the
# re-reply storm that a durable cursor fixed was this loop.
#
# ## The daemon is built from THIS checkout
#
# It used to be a published image (`nexusd-cluster-cohost:<tag>`) started through
# a compose file in the nexus repo, which meant the agent under test was the
# sudocode rev that image was built from — never this working tree. The harness
# carried a paragraph of instructions for telling that failure apart from a real
# one, which is the shape of a gate you cannot trust.
#
# `nexusd-cohost` is a crate in this repo now (`rust/crates/nexusd-cohost`), so
# the binary is `cargo build` away and the agent inside it is the code in this
# diff. That also retires the GitHub token the image build needed to clone this
# repo, the compose file, and the `host.docker.internal` IPv6 trap — the mock and
# the daemon are on one host.
#
# ## Why no LLM key
#
# The provider config points at this repo's mock Anthropic service, so the
# agent's turn is deterministic and free. A funded key would buy nondeterminism:
# a live model that declines to call `send` fails a run for a reason that is
# nothing to do with the code, which is how `live_subagent_smoke_stdio` stayed
# red across five merges. The live-model variant is a separate, manual run.
#
# ## Usage
#
#   e2e/nexus-a2a/run-cohost.sh
#   NEXUSD_COHOST_BIN=/path/to/nexusd-cohost e2e/nexus-a2a/run-cohost.sh
set -euo pipefail
cd "$(dirname "$0")"
# shellcheck source=lib.sh
. ./lib.sh

# NOT 2126: `serve-local` defaults to that, so a developer's own daemon is
# already there and the assertions below would reach it instead of the co-host
# this script starts.
PORT="${NEXUS_A2A_COHOST_PORT:-2144}"
ENDPOINT="127.0.0.1:${PORT}"
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
  # `--features daemon`: the bin is behind it so the workspace's own test and
  # clippy jobs do not compile a raft + tonic tree on three platforms (and do not
  # need `protoc`, which the macOS runners have not got).
  cargo build "${MANIFEST[@]}" -q -p nexusd-cohost --features daemon
  BIN="$(cargo metadata "${MANIFEST[@]}" --format-version 1 --no-deps \
         | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')/debug/nexusd-cohost"
  [ -x "$BIN" ] || BIN="$BIN.exe"
fi
[ -x "$BIN" ] || { echo "!! no nexusd-cohost binary at $BIN" >&2; exit 1; }
echo "   $BIN"

MOCK_PID=
DAEMON_PID=
WORK_DIR=
cleanup() {
  [ -n "$DAEMON_PID" ] && kill "$DAEMON_PID" 2>/dev/null || true
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

echo "== 1. mock model on 127.0.0.1:${MOCK_PORT} =="
# Built first, then run: `cargo run` would otherwise compile INTO the log this
# waits on, and a cold build outlasts any sane readiness window.
cargo build "${MANIFEST[@]}" -q -p mock-anthropic-service
cargo run "${MANIFEST[@]}" -q -p mock-anthropic-service -- \
  --bind "127.0.0.1:${MOCK_PORT}" >"$WORK_DIR/mock.log" 2>&1 &
MOCK_PID=$!
for _ in $(seq 1 20); do
  grep -q MOCK_ANTHROPIC_BASE_URL "$WORK_DIR/mock.log" 2>/dev/null && break
  sleep 1
done
grep -q MOCK_ANTHROPIC_BASE_URL "$WORK_DIR/mock.log" \
  || { echo "!! the mock never came up" >&2; cat "$WORK_DIR/mock.log" >&2; exit 1; }
echo "   up"

# ONE auth mode, deliberately. A co-host agent is spawned inside the daemon and
# never sees an `--auth` flag, so it takes whatever the config offers: hand it
# all three and it picks `subscription`, which wants a token nobody here has, and
# the spawn fails with `no token available for subscription provider`.
#
# `api-key`/`anthropic` is the mode that speaks Anthropic's `/v1/messages`, which
# is the surface the mock serves; `proxy` resolves to the OpenAI-compatible
# provider and would speak `/v1/chat/completions` to it.
#
# The model has to be DECLARED too, from the same variable the spawn uses so the
# two cannot disagree — with only `api-key` there is no proxy to pass an unknown
# alias through to, and an undeclared one fails as `model alias '<name>' not
# found in sudocode.json`.
echo "== 2. provider config pointed at the mock =="
CONFIG_HOME="$WORK_DIR/config"
mkdir -p "$CONFIG_HOME"
cat >"$CONFIG_HOME/sudocode.json" <<JSON
{
  "auth_modes": {
    "api-key": {
      "anthropic": {
        "baseUrl": "http://127.0.0.1:${MOCK_PORT}",
        "apiKey": "mock-key-unused"
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

echo "== 3. co-host daemon on :${PORT} =="
# `info`, not the daemon's default `warn`: when the agent does not answer, the
# question is always WHERE it stopped — was the service installed, did the spawn
# reach a loop, did the turn call the provider — and at `warn` the log says none
# of it. A gate that cannot explain itself sends you to the source instead.
export RUST_LOG="${RUST_LOG:-info}"
# Fresh data AND identity dir: `identity.json` carries the peer address book and
# per-zone membership by design, so a reused one leaves the daemon rejoining an
# old cluster with a stale identity and no quorum.
"$BIN" \
  --bind-addr "0.0.0.0:${PORT}" \
  --data-dir "$(native_path "$WORK_DIR/data")" \
  --identity-dir "$(native_path "$WORK_DIR/identity")" \
  --no-tls \
  --insecure-no-auth \
  >"$WORK_DIR/daemon.log" 2>&1 &
DAEMON_PID=$!

ready=
for i in $(seq 1 30); do
  if NEXUS_A2A_TEST_ENDPOINT="$ENDPOINT" "${CARGO_TEST[@]}" live_inbox_roundtrip \
       -- --ignored 2>/dev/null | grep -q "1 passed"; then
    echo "   writable after ~$((i * 4))s"
    ready=1
    break
  fi
  sleep 4
done
[ -n "$ready" ] || { echo "!! the co-host daemon never became writable" >&2; \
  tail -40 "$WORK_DIR/daemon.log" >&2; exit 1; }

# THE PAIR, before the agent starts. Two things have to be true and only one of
# them is obvious: the conversation must exist before the spawn, because the
# co-host arms its tail on what its chat list names at startup; and it must be the
# AGENT<->OPERATOR conversation, because a conversation is addressed by its pair.
# Provisioning each side against a probe peer instead left the agent parked on
# `cohost-bot`<->`live-probe-peer` while the operator wrote to
# `cohost-bot`<->`operator` — a reader at offset 0 holding a valid lease, which
# looks identical to a loop that never woke.
echo "== 4. provision the ${AGENT}<->${OPERATOR} conversation =="
NEXUS_A2A_TEST_ENDPOINT="$ENDPOINT" NEXUS_A2A_TEST_INBOX="$AGENT"   NEXUS_A2A_TEST_PEER="$OPERATOR"   "${CARGO_TEST[@]}" live_ensure_inbox -- --ignored >/dev/null
echo "   both sides filed"

echo "== 5. spawn the co-host agent =="
NEXUS_A2A_TEST_ENDPOINT="$ENDPOINT" NEXUS_A2A_TEST_SPAWN="$AGENT" \
  NEXUS_A2A_TEST_MODEL="$MODEL" \
  "${CARGO_TEST[@]}" live_spawn_cohost -- --ignored --nocapture

echo "== 6. send it a message, and wait for the agent's own reply =="
# The agent runs inside the daemon, so when it does not answer, the daemon's log
# is the only place that says why — whether it woke at all, what its turn did,
# and what the provider told it. Dumping it on failure is the difference between
# a diagnosis and a guess.
if ! NEXUS_A2A_TEST_ENDPOINT="$ENDPOINT" \
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

echo "COHOST E2E OK"
