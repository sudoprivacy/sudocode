#!/usr/bin/env bash
# Fetch the `nexusd-cluster` binary this checkout is pinned against.
#
# The A2A harness needs a real daemon. Building one is a nexus-repo job and
# needs a GitHub token, which is why the Docker path here has always been a
# developer prereq rather than something CI could do. The release artifacts are
# public, so a binary is something CI can just download.
#
# ## The version comes from the pin, and only the pin
#
# `rust/Cargo.lock` already records the exact nexus-vfs rev the client library
# is built from. The release tag is derived from it by asking the repo which tag
# points at that commit — no auth, and nothing to keep in step by hand.
#
# That matters more than the convenience. A daemon at `latest` against a client
# pinned to some older rev tests a pairing nobody ships, and the failures land
# on whatever changed between them. This resolves the daemon that MATCHES.
#
# ## Usage
#
#   e2e/nexus-a2a/fetch-daemon.sh                 # prints the binary path
#   NEXUSD_BIN=$(e2e/nexus-a2a/fetch-daemon.sh)   # what run.sh does
#
# Re-running is cheap: an already-downloaded binary for the same tag is reused.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOCKFILE="$REPO_ROOT/rust/Cargo.lock"
NEXUS_REPO="${NEXUS_VFS_REPO:-https://github.com/nexi-lab/nexus-vfs}"
COS_BASE="${NEXUS_RELEASE_BASE:-https://sudowork-runtime-1309794936.cos.accelerate.myqcloud.com/nexus-vfs/release}"

log() { echo "[fetch-daemon] $*" >&2; }
die() { echo "[fetch-daemon] $*" >&2; exit 1; }

# ── 1. the pinned rev ────────────────────────────────────────────────────
[ -f "$LOCKFILE" ] || die "no lockfile at $LOCKFILE"
REV="$(grep -oE 'nexus-vfs\?rev=[0-9a-f]{40}' "$LOCKFILE" | head -1 | cut -d= -f2)"
[ -n "$REV" ] || die "no nexus-vfs rev in $LOCKFILE — has the dependency moved?"

# ── 2. the tag that points at it ─────────────────────────────────────────
# `^{}` marks the commit an annotated tag dereferences to, which is the one the
# lockfile records. Matching the tag object itself would miss every annotated
# release.
TAG="$(git ls-remote --tags "$NEXUS_REPO" 2>/dev/null \
        | awk -v rev="$REV" '$1 == rev && $2 ~ /\^\{\}$/ { sub(/^refs\/tags\//, "", $2); sub(/\^\{\}$/, "", $2); print $2; exit }')"
if [ -z "$TAG" ]; then
    die "rev ${REV:0:9} is not at any nexus-vfs tag.

A pin between releases has no published daemon to match it. Either bump the pin
to a tagged rev, or build the daemon from the nexus repo and point the harness
at it with NEXUS_DAEMON_IMAGE / NEXUSD_BIN."
fi
log "pinned rev ${REV:0:9} → $TAG"

# ── 3. the artifact for this platform ────────────────────────────────────
case "$(uname -s)" in
    Linux)   os=linux ;;
    Darwin)  os=macos ;;
    MINGW*|MSYS*|CYGWIN*) os=windows ;;
    *) die "unsupported platform $(uname -s)" ;;
esac
case "$(uname -m)" in
    x86_64|amd64) arch=x86_64 ;;
    arm64|aarch64) arch=aarch64 ;;
    *) die "unsupported architecture $(uname -m)" ;;
esac

if [ "$os" = windows ]; then
    archive="nexusd-cluster-${os}-${arch}.zip"
    binary="nexusd-cluster.exe"
else
    archive="nexusd-cluster-${os}-${arch}.tar.gz"
    binary="nexusd-cluster"
fi

DEST="${NEXUSD_CACHE:-$REPO_ROOT/rust/target/nexusd}/$TAG"
BIN="$DEST/$binary"
if [ -x "$BIN" ]; then
    log "cached $BIN"
    echo "$BIN"
    exit 0
fi

mkdir -p "$DEST"
URL="$COS_BASE/$TAG/$archive"
log "downloading $URL"
tmp="$DEST/.$archive.part"
curl -fsSL --retry 3 --retry-delay 2 --max-time 300 -o "$tmp" "$URL" \
    || die "download failed: $URL

The release publishes six targets; if this one is missing for $TAG the harness
can still run against a locally built daemon via NEXUS_DAEMON_IMAGE / NEXUSD_BIN."

case "$archive" in
    *.zip)    unzip -oq "$tmp" -d "$DEST" ;;
    *.tar.gz) tar -xzf "$tmp" -C "$DEST" ;;
esac
rm -f "$tmp"

# The archives carry a top-level directory; the binary may land a level down.
if [ ! -f "$BIN" ]; then
    found="$(find "$DEST" -name "$binary" -type f | head -1)"
    [ -n "$found" ] || die "no $binary inside $archive"
    mv "$found" "$BIN"
fi
chmod +x "$BIN"
log "ready $BIN"
echo "$BIN"
