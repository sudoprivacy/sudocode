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

# Cargo discovers configuration from its working directory. Run at the Rust
# workspace so native harness builds use the same target policy as releases.
cargo_in_workspace() {
  (cd "$RUST_DIR" && cargo "$@")
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

# Tooling the session-credential helpers below need. Resolved lazily so a
# harness that never mints a session (run.sh, run-auth-on.sh) never requires
# them; run-cohost.sh calls session_mint_tools_init before its first use.
SESSION_MINT_PROTO="${SESSION_MINT_PROTO:-$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/raft-session-mint.proto}"
JQ="${JQ:-jq}"
GRPCURL="${GRPCURL:-}"

session_mint_tools_init() {
  command -v "$JQ" >/dev/null 2>&1 \
    || { echo "!! $JQ is required for session-credential minting" >&2; return 1; }
  [ -f "$SESSION_MINT_PROTO" ] \
    || { echo "!! no $SESSION_MINT_PROTO beside lib.sh" >&2; return 1; }
  GRPCURL="$(grpcurl_bin)" || return 1
  echo "   grpcurl: $GRPCURL"
}

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

# Run Cargo where the workspace's target and compiler configuration is discovered.
cargo_in_workspace() {
  (cd "$RUST_DIR" && cargo "$@")
}

# Boot the founder in the background, setting AUTHON_DAEMON_PID. Reads the daemon
# binary from AUTHON_NEXUSD_BIN.
authon_boot() {
  local env_lines
  export NEXUS_A2A_ZONE="$AUTHON_ZONE"
  AUTHON_LOG_OFFSET=0
  if [ -f "$AUTHON_DATA_DIR/daemon.log" ]; then
    AUTHON_LOG_OFFSET="$(wc -c <"$AUTHON_DATA_DIR/daemon.log")"
  fi
  mapfile -t env_lines < <(authon_daemon_env)
  env $AUTHON_NO_CONV "${env_lines[@]}" \
    "$AUTHON_NEXUSD_BIN" --bind-addr "0.0.0.0:${AUTHON_PORT}" \
    >>"$AUTHON_DATA_DIR/daemon.log" 2>&1 &
  AUTHON_DAEMON_PID=$!
}

# Stop the daemon tree owned by authon_boot, including an MSYS env wrapper.
authon_stop() {
  local signal="${1:-TERM}" native_pid
  [ -n "${AUTHON_DAEMON_PID:-}" ] || return 0
  if kill -0 "$AUTHON_DAEMON_PID" 2>/dev/null; then
    case "${OSTYPE:-}" in
      msys*|cygwin*)
        native_pid="$(ps -l -p "$AUTHON_DAEMON_PID" | awk 'NR == 2 { print $4 }')"
        [[ "$native_pid" =~ ^[0-9]+$ ]] || {
          echo "!! cannot resolve the owned daemon's Windows PID" >&2
          return 1
        }
        MSYS_NO_PATHCONV=1 taskkill.exe /PID "$native_pid" /T /F >/dev/null
        ;;
      *) kill -s "$signal" "$AUTHON_DAEMON_PID" ;;
    esac
  fi
  wait "$AUTHON_DAEMON_PID" 2>/dev/null || true
  AUTHON_DAEMON_PID=
}

# Wait until the daemon log contains a needle, or fail loud with a tail.
authon_wait_log() {
  local needle="$1" budget="${2:-45}" i
  for i in $(seq 1 "$budget"); do
    if tail -c "+$((AUTHON_LOG_OFFSET + 1))" "$AUTHON_DATA_DIR/daemon.log" 2>/dev/null \
      | grep -F -- "$needle" >/dev/null; then
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
# Extra args pass through to `auth mint`.
authon_mint() {
  local agent="$1"; shift
  local env_lines bundle
  mapfile -t env_lines < <(authon_daemon_env)
  bundle="$(env $AUTHON_NO_CONV "${env_lines[@]}" RUST_LOG=error \
    "$AUTHON_NEXUSD_BIN" auth mint --subject-type agent --subject-id "$agent" \
    --name e2e --allow-existing "$@" 2>/dev/null | tail -1 | tr -d '\r')"
  [ -n "$bundle" ] || { echo "!! mint printed no bundle path for $agent" >&2; return 1; }
  local local_dir
  local_dir="$(shell_path "$bundle")"
  local f
  for f in agent.pem agent-key.pem ca.pem credential.json; do
    [ -f "$local_dir/$f" ] || { echo "!! the $agent bundle has no $f" >&2; return 1; }
  done
  printf '%s' "$bundle"
}

# ── session credentials ─────────────────────────────────────────────────
# v0.8.5's agent control plane takes `managed_agent.start_session_v1` from a
# SESSION credential only: an agent cert is a pure identity (the mint refuses
# `--zone` for one, and the zone check would refuse it anyway), and a user or
# service key is turned away outright. The legitimate shape is the front door
# nexus-vfs's own e2e drives — an allow-listed agent that mints a per-session
# identity bound to a person. The harness drives the same two raft gRPCs with
# grpcurl, because the Rust tests must not link the tonic/protoc tree (the
# workspace's plain `cargo test` has no protoc).

# A usable grpcurl: PATH, a previously downloaded copy, or a fresh download.
# Nothing is installed system-wide; the cache lives under TMPDIR.
grpcurl_bin() {
  if command -v grpcurl >/dev/null 2>&1; then command -v grpcurl; return; fi
  local exe=""
  case "$(uname -s)" in
    MINGW*|MSYS*|CYGWIN*) exe=".exe" ;;
  esac
  local cache="${GRPCURL_CACHE:-${TMPDIR:-/tmp}/scode-grpcurl}/grpcurl${exe}"
  if [ -x "$cache" ]; then printf '%s' "$cache"; return; fi
  local version="1.9.3" os suffix
  case "$(uname -s)" in
    Linux) os=linux; suffix=tar.gz ;;
    Darwin) os=osx; suffix=tar.gz ;;
    MINGW*|MSYS*|CYGWIN*) os=windows; suffix=zip ;;
    *) echo "!! no grpcurl and no prebuilt binary for $(uname -s) — install grpcurl and retry" >&2; return 1 ;;
  esac
  local arch
  case "$(uname -m)" in
    aarch64|arm64) arch=arm64 ;;
    *) arch=x86_64 ;;
  esac
  mkdir -p "$(dirname "$cache")"
  local url="https://github.com/fullstorydev/grpcurl/releases/download/v${version}/grpcurl_${version}_${os}_${arch}.${suffix}"
  local download="$cache.dl.$suffix"
  curl -fsSL --retry 3 -o "$download" "$url" || { echo "!! could not fetch $url" >&2; return 1; }
  if [ "$suffix" = zip ]; then
    # git bash ships GNU tar (no zip); PowerShell's Expand-Archive is always
    # there and requires a .zip suffix plus Windows paths.
    powershell -NoProfile -Command "Expand-Archive -LiteralPath '$(cygpath -w "$download")' -DestinationPath '$(cygpath -w "$(dirname "$cache")")' -Force" \
      || { echo "!! could not unzip $download" >&2; return 1; }
    rm -f "$download"
  else
    tar -xzf "$download" -C "$(dirname "$cache")" grpcurl && rm -f "$download"
  fi
  chmod +x "$cache"
  printf '%s' "$cache"
}

# Add `agent_id` to the daemon's session-mint allow-list, as the node itself
# (the operator path: the node cert the data dir already holds).
allow_session_minter() {
  local endpoint="$1" data_dir="$2" agent_id="$3"
  local resp
  resp="$("$GRPCURL" -import-path "$(dirname "$SESSION_MINT_PROTO")" -proto raft-session-mint.proto \
    -cacert "$data_dir/tls/ca.pem" \
    -cert "$data_dir/tls/node.pem" -key "$data_dir/tls/node-key.pem" \
    -servername "$("$JQ" -r .server_name "$data_dir/agents/$agent_id/credential.json")" \
    -d "{\"agentId\": \"$agent_id\"}" \
    "${endpoint#https://}" nexus.raft.ZoneApiService/AllowSessionMinter)" \
    || { echo "!! AllowSessionMinter rpc failed" >&2; return 1; }
  "$JQ" -e '.success == true' >/dev/null <<<"$resp" \
    || { echo "!! AllowSessionMinter refused: $resp" >&2; return 1; }
}

# Mint a session credential through the (already allow-listed) front-door
# bundle and write it as a bundle directory the Rust tests can dial with.
# `out_dir` must not exist yet; the bundle shape mirrors `authon_mint`'s.
mint_session_bundle() {
  local endpoint="$1" frontdoor="$2" owner="$3" out_dir="$4"
  local resp subject server_name
  resp="$("$GRPCURL" -import-path "$(dirname "$SESSION_MINT_PROTO")" -proto raft-session-mint.proto \
    -cacert "$frontdoor/ca.pem" \
    -cert "$frontdoor/agent.pem" -key "$frontdoor/agent-key.pem" \
    -servername "$("$JQ" -r .server_name "$frontdoor/credential.json")" \
    -d "{\"ownerId\": \"$owner\", \"validitySecs\": 86400}" \
    "${endpoint#https://}" nexus.raft.ZoneApiService/MintSessionAgent)" \
    || { echo "!! MintSessionAgent rpc failed" >&2; return 1; }
  "$JQ" -e '.success == true' >/dev/null <<<"$resp" \
    || { echo "!! MintSessionAgent refused: $resp" >&2; return 1; }
  mkdir -p "$out_dir"
  subject="$("$JQ" -r .subjectId <<<"$resp")"
  server_name="$("$JQ" -r .server_name "$frontdoor/credential.json")"
  "$JQ" -r .agentCertPem <<<"$resp" | base64 -d >"$out_dir/agent.pem"
  "$JQ" -r .agentKeyPem <<<"$resp" | base64 -d >"$out_dir/agent-key.pem"
  "$JQ" -r .caPem <<<"$resp" | base64 -d >"$out_dir/ca.pem"
  cat >"$out_dir/credential.json" <<JSON
{
  "version": 1,
  "agent": "$subject",
  "server_name": "$server_name",
  "ca": "ca.pem",
  "cert": "agent.pem",
  "key": "agent-key.pem"
}
JSON
  printf '%s' "$out_dir"
}
