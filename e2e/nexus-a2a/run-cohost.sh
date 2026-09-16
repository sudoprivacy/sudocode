#!/usr/bin/env bash
# A co-host agent reads its inbox, runs a turn, and replies — with no LLM key.
#
# The third plane. `run.sh` and its siblings cover agents that are CLIENTS of a
# daemon; a co-host agent runs INSIDE one, so its receive loop, its turn and its
# `send` are the daemon's own process rather than a `scode` talking to it.
# Nothing else here exercises that, and it has had real bugs of its own — the
# re-reply storm that a durable cursor fixed was this loop.
#
# ## Why no key
#
# The compose file reads the co-host's provider config from `SUDOROUTER_BASE_URL`
# at boot, so pointing it at this repo's mock Anthropic service makes the agent's
# turn deterministic and free. A funded key would buy nondeterminism: a live
# model that declines to call `send` fails a run for a reason that is nothing to
# do with the code, which is how `live_subagent_smoke_stdio` stayed red across
# five merges.
#
# The mock binds on all interfaces because the agent dials it from inside the
# container, through `host.docker.internal` — which the override pins to the
# IPv4 gateway, because Docker's own entry for that name resolves to an IPv6
# address the mock is not listening on.
#
# ## Building the image behind a TUN VPN
#
# `docker build` needs DNS, and a Clash-style TUN adapter leaves containers
# unable to resolve anything (`Temporary failure resolving …`) while the host's
# HTTP proxy keeps working. Hand the build the proxy instead of fighting DNS:
#
#   docker build --build-arg http_proxy=http://host.docker.internal:7897 #     --build-arg https_proxy=http://host.docker.internal:7897 #     --secret id=ghtoken,src=<token file> #     -f dockerfiles/Dockerfile.nexusd-cohost -t nexusd-cluster-cohost:<tag> .
#
# ## What it needs
#
# `nexusd-cluster-cohost:latest`, which is a nexus-repo build (`--features
# cohost-sudocode`, so the image carries sudocode). It is not on the release
# bucket — only the plain `nexusd-cluster` is — so unlike the other harnesses
# this one cannot fetch what it runs, and skips rather than failing when the
# image is absent.
#
# ## Which sudocode the image carries
#
# Not this checkout's. That build is `cargo install --locked --path rust/nexusd`
# against the NEXUS workspace, whose `Cargo.lock` pins the sudocode rev — so the
# agent inside the container is whatever that pin says, and a stale pin means
# this harness is exercising a sudocode from weeks ago. When the reply step
# fails, check that first: compare the image's build date and the nexus pin
# against the change you are trying to prove, and rebuild from a bumped pin
# rather than reading the failure as a bug in the code you just wrote.
#
# `COHOST_IMAGE` is therefore EXPORTED, so the compose file starts the same tag
# this script checked. It was read here and hardcoded there, and the two disagreed
# without saying so: a rebuild tagged `nexusd-cluster-cohost:<pin>` passed the
# presence check while compose started a `:latest` from three weeks earlier, and
# every run measured that old binary. The symptom — spawn succeeds, agent never
# answers — points at the code, which is why it cost a day.
#
# Usage:
#   e2e/nexus-a2a/run-cohost.sh
#   COHOST_IMAGE=nexusd-cluster-cohost:develop e2e/nexus-a2a/run-cohost.sh
set -euo pipefail
cd "$(dirname "$0")"

IMAGE="${COHOST_IMAGE:-nexusd-cluster-cohost:latest}"
# Exported, not just read: the compose file interpolates `COHOST_IMAGE` to pick
# which build starts. Checking the tag here and letting compose default to
# `:latest` is how a rebuilt image sits unused while the run exercises a months
# -old binary.
export COHOST_IMAGE="$IMAGE"
PORT="${NEXUS_A2A_COHOST_PORT:-2126}"
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

if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
  # Loud, because a skip that scrolls past reads like a pass. This harness is
  # the only cover for the co-host receive loop, so "it did not run" and "it
  # passed" must not look alike at a glance.
  echo "!! ================================================================"
  echo "!! SKIPPED — NOTHING WAS VERIFIED"
  echo "!! $IMAGE is not present."
  echo "!! Build it in the nexus repo (dockerfiles/Dockerfile.nexusd-cohost),"
  echo "!! or set COHOST_IMAGE to a tag that exists."
  echo "!! ================================================================"
  exit 0
fi
# Printed on EVERY run, not only on failure: the agent under test is the
# sudocode the image carries, and an image built before the change being proved
# fails in the one way that looks least like a stale image — the agent spawns,
# answers nothing, and the run times out. Having the build date in the log turns
# that into a glance.
echo "== 0. image: $IMAGE (built $(docker image inspect "$IMAGE" --format '{{.Created}}')) =="

COMPOSE="${NEXUS_COHOST_COMPOSE:-}"
if [ -z "$COMPOSE" ]; then
  for candidate in \
    "$HOME/cursor-projects/nexus/dockerfiles/docker-compose.cohost-duet.yml" \
    "../../../nexus/dockerfiles/docker-compose.cohost-duet.yml"; do
    [ -f "$candidate" ] && COMPOSE="$candidate" && break
  done
fi
if [ -z "$COMPOSE" ]; then
  echo "== [skip] no cohost compose file — set NEXUS_COHOST_COMPOSE =="
  exit 0
fi

# Exported rather than prefixed onto `up`: the compose file marks
# SUDOROUTER_API_KEY required, so EVERY invocation has to interpolate it —
# including the `ps` this polls for health and the `down` in the cleanup. A
# prefix on `up` alone left both of those failing on a container that was in
# fact healthy.
# The base file guards SUDOROUTER_API_KEY with `:?`, and compose evaluates that
# when it PARSES the file — before any override is merged — so the value has to
# come from the shell no matter what the override says. Unused: the mock checks
# no credentials.
export SUDOROUTER_API_KEY="sk-mock-unused"

COMPOSE_FILES=("-f" "$COMPOSE" "-f" "cohost-mock.override.yml")

# The config the agents inside the container will use.
#
# ONE auth mode, exactly as the base compose file does with `proxy`: a co-host
# agent is spawned inside the daemon and never sees an `--auth` flag, so it
# takes whatever the config offers. Hand it all three modes and it picks
# `subscription`, which wants a token nobody here has — the spawn then fails
# with `no token available for subscription provider`.
#
# `api-key`/`anthropic` is the mode that speaks Anthropic's `/v1/messages`,
# which is the surface the mock serves; `proxy` resolves to the
# OpenAI-compatible provider and would speak `/v1/chat/completions` to it.
#
# The model has to be DECLARED too. The base file gets away with naming any
# model because a `proxy` provider passes unknown aliases through; with only
# `api-key` there is nothing to pass through to, and an undeclared alias fails
# as `model alias '<name>' not found in sudocode.json`. Declared here from the
# same variable the spawn uses, so the two cannot disagree.
MOCK_URL="http://host.docker.internal:${MOCK_PORT}"
export SCODE_MOCK_CONFIG='{"auth_modes":{"api-key":{"anthropic":{"baseUrl":"'"$MOCK_URL"'","apiKey":"mock-key-unused"}}},
  "models":{"'"$MODEL"'":{"alias":"'"$MODEL"'","name":"mock","input":["text"],
  "providers":{"api-key":{"provider":"anthropic","model":"'"$MODEL"'"}}}}}'
export SUDOROUTER_BASE_URL="http://host.docker.internal:${MOCK_PORT}/v1"
export NEXUS_A2A_HOST_PORT="$PORT"

MOCK_PID=
cleanup() {
  docker compose "${COMPOSE_FILES[@]}" down -v >/dev/null 2>&1 || true
  [ -n "$MOCK_PID" ] && kill "$MOCK_PID" 2>/dev/null || true
}
trap cleanup EXIT

echo "== 1. mock model on :${MOCK_PORT} (the agent dials it from the container) =="
# Built first, then run: `cargo run` would otherwise compile INTO the log this
# waits on, and a cold build outlasts any sane readiness window.
cargo build "${MANIFEST[@]}" -q -p mock-anthropic-service
cargo run "${MANIFEST[@]}" -q -p mock-anthropic-service --   --bind "0.0.0.0:${MOCK_PORT}" >/tmp/cohost-mock.log 2>&1 &
MOCK_PID=$!
for i in $(seq 1 20); do
  grep -q MOCK_ANTHROPIC_BASE_URL /tmp/cohost-mock.log 2>/dev/null && break
  sleep 1
done
grep -q MOCK_ANTHROPIC_BASE_URL /tmp/cohost-mock.log \
  || { echo "!! the mock never came up" >&2; cat /tmp/cohost-mock.log >&2; exit 1; }
echo "   up"

echo "== 2. co-host daemon, provider pointed at the mock =="
docker compose "${COMPOSE_FILES[@]}" up -d >/dev/null
ready=
for i in $(seq 1 40); do
  if [ "$(docker compose "${COMPOSE_FILES[@]}" ps --format '{{.Health}}' 2>/dev/null | head -1)" = healthy ]; then
    echo "   healthy after ~${i}s"
    ready=1
    break
  fi
  sleep 1
done
[ -n "$ready" ] || { echo "!! the co-host never became healthy" >&2; \
  docker compose "${COMPOSE_FILES[@]}" logs --tail 40 >&2; exit 1; }

# Both inboxes before the agent starts: the co-host arms its watch at its
# inbox's tail, so an inbox created afterwards is one it is not reading, and the
# operator's has to exist for the reply to have somewhere to land.
echo "== 3. provision both inboxes =="
for who in "$AGENT" "$OPERATOR"; do
  NEXUS_A2A_TEST_ENDPOINT="$ENDPOINT" NEXUS_A2A_TEST_INBOX="$who" \
    "${CARGO_TEST[@]}" live_ensure_inbox -- --ignored >/dev/null
  echo "   $who"
done

echo "== 4. spawn the co-host agent =="
NEXUS_A2A_TEST_ENDPOINT="$ENDPOINT" NEXUS_A2A_TEST_SPAWN="$AGENT" \
  NEXUS_A2A_TEST_MODEL="$MODEL" \
  "${CARGO_TEST[@]}" live_spawn_cohost -- --ignored --nocapture

echo "== 5. send it a message, and wait for the agent's own reply =="
# The agent runs inside the daemon, so when it does not answer, the daemon's log
# is the only place that says why — whether it woke at all, what its turn did,
# and what the provider told it. Dumping it on failure is the difference between
# a diagnosis and a guess.
if ! NEXUS_A2A_TEST_ENDPOINT="$ENDPOINT" \
  NEXUS_A2A_TEST_INBOX="$AGENT" \
  NEXUS_A2A_TEST_REPLY_TO="$OPERATOR" \
  NEXUS_A2A_TEST_REPLY_BODY="$REPLY" \
  "${CARGO_TEST[@]}" live_cohost_reads_its_inbox_and_replies -- --ignored --nocapture; then
  echo "!! the co-host did not reply. Before reading this as a code bug: the" >&2
  echo "!! agent in that image is the sudocode rev the NEXUS Cargo.lock pins," >&2
  echo "!! not this checkout — compare the build date printed by step 0 against" >&2
  echo "!! the change you are trying to prove, and rebuild from a bumped pin." >&2
  echo "---- co-host daemon log ----" >&2
  docker compose "${COMPOSE_FILES[@]}" logs --tail 120 >&2 || true
  echo "---- mock provider log ----" >&2
  tail -30 /tmp/cohost-mock.log >&2 || true
  exit 1
fi

echo "COHOST E2E OK"
