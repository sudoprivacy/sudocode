#!/usr/bin/env bash
# Deterministic Docker E2E for the standalone nexus-A2A client (X).
#
# Brings up a real `nexusd-cluster` founder in a container and drives the
# ignored `runtime` integration tests (`nexus_mailbox_live`) against it — the
# one thing unit tests can't cover: that `ensure_stream` + `stream_write` +
# `stream_read_at` actually move an envelope through a real gRPC server and a
# real DT_STREAM. No LLM, no secrets — always safe to run.
#
# The optional 2-LLM co-host duet (a real `scode` sending to a daemon-hosted
# co-host agent that LLM-replies) runs only when SUDOROUTER_API_KEY (funded) and
# SCODE_BIN are both set — mirroring `subagent-parity-live.yml`'s gating.
#
# Usage:
#   e2e/nexus-a2a/run.sh
#   NEXUS_DAEMON_IMAGE=nexusd-cluster:latest e2e/nexus-a2a/run.sh
#   SUDOROUTER_API_KEY=sk-... SCODE_BIN=/path/to/scode e2e/nexus-a2a/run.sh   # + duet
set -euo pipefail
cd "$(dirname "$0")"

PORT="${NEXUS_A2A_HOST_PORT:-2126}"
ENDPOINT="127.0.0.1:${PORT}"
RUST_DIR="${RUST_DIR:-$(cd ../../rust && pwd)}"
CARGO_TEST=(cargo test --manifest-path "$RUST_DIR/Cargo.toml" -q -p runtime --test nexus_mailbox_live)

cleanup() { docker compose down -v >/dev/null 2>&1 || true; }
trap cleanup EXIT

echo "== starting nexusd-cluster (${NEXUS_DAEMON_IMAGE:-nexusd-cluster-cohost:latest}) on :${PORT} =="
docker compose up -d

echo "== waiting for a writable single-voter leader =="
ready=
for i in $(seq 1 30); do
  if NEXUS_A2A_TEST_ENDPOINT="$ENDPOINT" "${CARGO_TEST[@]}" live_inbox_roundtrip -- --ignored 2>/dev/null | grep -q "1 passed"; then
    echo "   writable after ~$((i * 4))s"
    ready=1
    break
  fi
  sleep 4
done
if [ -z "$ready" ]; then
  echo "!! daemon never became writable" >&2
  docker compose logs --tail 40 || true
  exit 1
fi

echo "== [deterministic] standalone A2A client round-trip =="
NEXUS_A2A_TEST_ENDPOINT="$ENDPOINT" "${CARGO_TEST[@]}" live_inbox_roundtrip -- --ignored --nocapture

# ---- Optional: real 2-LLM co-host duet (gated) --------------------------------
if [ -n "${SUDOROUTER_API_KEY:-}" ] && [ -n "${SCODE_BIN:-}" ]; then
  echo "== [live] scode -> co-host duet =="
  R="${DUET_RESPONDER:-duet-bot}"
  MODEL="${DUET_MODEL:-claude-sonnet-4-6}"
  # Provision the responder's inbox BEFORE spawning it, so the co-host arms its
  # watch at an empty tail and sees scode's message as new (not skipped).
  NEXUS_A2A_TEST_ENDPOINT="$ENDPOINT" NEXUS_A2A_TEST_INBOX="$R" "${CARGO_TEST[@]}" live_ensure_inbox -- --ignored >/dev/null
  NEXUS_A2A_TEST_ENDPOINT="$ENDPOINT" NEXUS_A2A_TEST_SPAWN="$R" NEXUS_A2A_TEST_MODEL="$MODEL" \
    "${CARGO_TEST[@]}" live_spawn_cohost -- --ignored --nocapture
  sleep 8
  NEXUS_A2A_ENDPOINT="$ENDPOINT" NEXUS_A2A_AGENT="${DUET_SELF:-operator}" NEXUS_A2A_PEER="$R" \
    "$SCODE_BIN" --auth proxy --model "$MODEL" --permission-mode danger-full-access \
    --print "Call send_message once: to=$R body='reply with exactly one word: PONG'. Then stop."
  echo "   polling ${DUET_SELF:-operator}'s inbox for the co-host reply..."
  got=
  for i in $(seq 1 30); do
    out=$(NEXUS_A2A_TEST_ENDPOINT="$ENDPOINT" NEXUS_A2A_TEST_INBOX="${DUET_SELF:-operator}" \
      "${CARGO_TEST[@]}" live_collect_inbox -- --ignored --nocapture 2>&1 || true)
    if echo "$out" | grep -q "from=\"$R\""; then
      echo "   >>> DUET REPLY:"; echo "$out" | grep "from="; got=1; break
    fi
    sleep 3
  done
  [ -n "$got" ] || { echo "!! co-host never replied (see daemon logs)" >&2; docker compose logs --tail 40 nexusd || true; exit 1; }
else
  echo "== [skip] LLM duet — set SUDOROUTER_API_KEY + SCODE_BIN to enable =="
fi

echo "E2E OK"
