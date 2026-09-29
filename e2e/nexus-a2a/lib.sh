#!/usr/bin/env bash
# Path translation shared by the A2A harnesses.
#
# `MSYS_NO_PATHCONV=1` is required so `/agents=<zone>` reaches the daemon as
# written — Git Bash otherwise rewrites it to `C:/Program Files/Git/agents=…`
# and the daemon refuses to boot on a topology it cannot parse. The same switch
# also stops the conversion the daemon's OTHER path arguments need, and a
# Windows binary reads `/tmp/x` as `C:\tmp\x` — a different directory from this
# shell's `/tmp`.
#
# So the daemon wrote its data where the cleanup never looked: eight abandoned
# data directories and 2.2 GB in `C:\tmp` before anyone noticed, because
# nothing in the harness reads the filesystem the daemon writes to — every
# assertion goes over gRPC, and a leak is invisible from there.
#
# Translate explicitly instead. Both functions are the identity where `cygpath`
# does not exist, which is every CI runner these scripts normally run on.

# A path the daemon will resolve the same way this shell does.
native_path() {
  if command -v cygpath >/dev/null 2>&1; then
    cygpath -w "$1"
  else
    printf '%s' "$1"
  fi
}

# A path this shell can test and delete, from one the daemon printed.
#
# Also normalises separators: the daemon joins its own, so a printed path can
# mix the two (`/tmp/x/data\agents\name`), and a backslash is a literal
# character to this shell rather than a separator.
shell_path() {
  local p="${1//\\//}"
  if command -v cygpath >/dev/null 2>&1; then
    cygpath -u "$p" 2>/dev/null || printf '%s' "$p"
  else
    printf '%s' "$p"
  fi
}

# ── Auth-on bring-up, shared by every A2A harness ──────────────────────────────
#
# scode is cert-only: it dials mTLS with a minted credential and never plaintext,
# so dev exercises exactly what production does. These helpers factor out the
# bring-up run-auth-on.sh proved: boot TLS-on, mint each agent's bundle offline
# (the mint opens the data dir the daemon locks, so the daemon must be down for
# it), then restart and dial the mTLS plane.
#
# A caller sets AUTHON_DATA_DIR, AUTHON_PORT and AUTHON_ZONE, sources this, and
# uses: authon_daemon_env / authon_boot / authon_wait_log / authon_mint.

# MSYS must not rewrite `/agents=<zone>` or the daemon's path args (see top).
AUTHON_NO_CONV="MSYS_NO_PATHCONV=1"

# The daemon environment for an auth-on founder. TLS is ON by NEXUS_NO_TLS being
# unset; the api-key secret must match between the daemon and the offline mint or
# a minted credential authenticates as nobody.
authon_daemon_env() {
  printf '%s\n' \
    "NEXUS_DATA_DIR=$(native_path "$AUTHON_DATA_DIR/data")" \
    "NEXUS_IDENTITY_DIR=$(native_path "$AUTHON_DATA_DIR/id")" \
    "NEXUS_API_KEY_SECRET=${NEXUS_API_KEY_SECRET:-scode-e2e-secret}" \
    "NEXUS_ADVERTISE_ADDR=127.0.0.1:${AUTHON_PORT}" \
    "NEXUS_CLUSTER_INIT=$AUTHON_ZONE" \
    "NEXUS_CLUSTER_INIT_MOUNTS=/agents=$AUTHON_ZONE" \
    "RUST_LOG=${RUST_LOG:-info}"
}

# Boot the founder in the background, setting AUTHON_DAEMON_PID. Reads the daemon
# binary from AUTHON_NEXUSD_BIN.
authon_boot() {
  local env_lines
  mapfile -t env_lines < <(authon_daemon_env)
  env $AUTHON_NO_CONV "${env_lines[@]}" \
    "$AUTHON_NEXUSD_BIN" --bind-addr "0.0.0.0:${AUTHON_PORT}" \
    >>"$AUTHON_DATA_DIR/daemon.log" 2>&1 &
  AUTHON_DAEMON_PID=$!
}

# Wait until the daemon log contains a needle, or fail loud with a tail.
authon_wait_log() {
  local needle="$1" budget="${2:-45}" i
  for i in $(seq 1 "$budget"); do
    if grep -q "$needle" "$AUTHON_DATA_DIR/daemon.log" 2>/dev/null; then
      echo "   $needle (after ~${i}s)"
      return 0
    fi
    sleep 1
  done
  echo "!! daemon never logged '$needle'" >&2
  tail -40 "$AUTHON_DATA_DIR/daemon.log" >&2
  return 1
}

# Mint a CA-signed agent bundle offline and print its directory (daemon form).
# The daemon must be stopped first: the mint opens the data dir it locks. The
# printed path is the daemon's own spelling; a Rust `File::open` reads it as-is,
# so callers hand it straight to NEXUS_A2A_TEST_CERT_DIR.
authon_mint() {
  local agent="$1" env_lines bundle
  mapfile -t env_lines < <(authon_daemon_env)
  bundle="$(env $AUTHON_NO_CONV "${env_lines[@]}" RUST_LOG=error \
    "$AUTHON_NEXUSD_BIN" auth mint --subject-type agent --subject-id "$agent" \
    --name e2e --allow-existing 2>/dev/null | tail -1 | tr -d '\r')"
  [ -n "$bundle" ] || { echo "!! mint printed no bundle path for $agent" >&2; return 1; }
  local local_dir
  local_dir="$(shell_path "$bundle")"
  local f
  for f in agent.pem agent-key.pem ca.pem credential.json; do
    [ -f "$local_dir/$f" ] || { echo "!! the $agent bundle has no $f" >&2; return 1; }
  done
  printf '%s' "$bundle"
}
