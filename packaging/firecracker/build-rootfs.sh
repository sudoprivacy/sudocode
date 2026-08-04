#!/usr/bin/env bash
# build-rootfs.sh — build the read-only guest rootfs for the scode microVM.
#
# Produces an ext4 image containing a Debian bookworm-slim userland plus
# the tools scode's agent loop needs at runtime (bash, git, coreutils,
# CA certificates) and the scode binary itself. The image is populated
# with `mke2fs -d`, so no root privileges or loop mounts are required.
#
# The rootfs is a golden image: attach it read-only to any number of
# VMs. All mutable state (sessions, HOME, workspace) lives on the
# per-VM data volume built by build-data-volume.sh.
#
# Usage:
#   ./build-rootfs.sh [--scode PATH] [--out DIR] [--size MiB]
#
# Requirements: docker (to stage the Debian userland), mke2fs ≥ 1.43.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
SCODE_BIN="${HERE}/../../rust/target/release/scode"
OUT_DIR="${HERE}/out"
SIZE_MIB=1024
BASE_IMAGE="debian:bookworm-slim"

while [ $# -gt 0 ]; do
    case "$1" in
        --scode) SCODE_BIN="$2"; shift 2 ;;
        --out) OUT_DIR="$2"; shift 2 ;;
        --size) SIZE_MIB="$2"; shift 2 ;;
        -h|--help) grep '^#' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 1 ;;
    esac
done

if [ ! -x "$SCODE_BIN" ]; then
    echo "error: scode binary not found at ${SCODE_BIN}" >&2
    echo "build it first: (cd rust && cargo build --release) or pass --scode" >&2
    exit 1
fi
command -v docker >/dev/null || { echo "error: docker is required to stage the userland" >&2; exit 1; }
command -v mke2fs >/dev/null || { echo "error: mke2fs (e2fsprogs) is required" >&2; exit 1; }

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

# --- stage the Debian userland via docker export ----------------------------
# `docker export` of a stopped container gives a plain rootfs tarball —
# no daemon-side mounts, no privileged operations on our side.
echo "staging ${BASE_IMAGE} userland (+ git, ca-certificates)…"
cid="$(docker create "$BASE_IMAGE" true)"
docker export "$cid" | tar -C "$STAGE" -xf -
docker rm -f "$cid" >/dev/null

# Install runtime packages into the staged tree with a throwaway
# container sharing the same base image, then re-export. Doing it in
# one pass keeps apt state consistent.
cid="$(docker run -d "$BASE_IMAGE" sh -c \
    'apt-get update -qq && apt-get install -y -qq --no-install-recommends \
        git ca-certificates util-linux procps less curl \
        && rm -rf /var/lib/apt/lists/* && sync')"
docker wait "$cid" >/dev/null
rm -rf "$STAGE"
mkdir -p "$STAGE"
docker export "$cid" | tar -C "$STAGE" -xf -
docker rm -f "$cid" >/dev/null

# --- scode + init -----------------------------------------------------------
install -m 0755 "$SCODE_BIN" "$STAGE/usr/local/bin/scode"
install -m 0755 "$HERE/guest-init.sh" "$STAGE/sbin/scode-init"

# Static DNS: Firecracker guests get their IP from the ip= boot arg;
# nothing writes resolv.conf, so bake it in.
{
    echo "nameserver 1.1.1.1"
    echo "nameserver 8.8.8.8"
} > "$STAGE/etc/resolv.conf"
echo "scode-vm" > "$STAGE/etc/hostname"

# Mount points the guest init expects to exist on the read-only root.
mkdir -p "$STAGE/data" "$STAGE/proc" "$STAGE/sys" "$STAGE/dev" "$STAGE/tmp" "$STAGE/run"

# --- pack into ext4 ---------------------------------------------------------
mkdir -p "$OUT_DIR"
IMG="${OUT_DIR}/rootfs.ext4"
rm -f "$IMG"
echo "packing ${SIZE_MIB} MiB ext4 image…"
mke2fs -q -t ext4 -L scode-rootfs -d "$STAGE" "$IMG" "${SIZE_MIB}M"

echo "wrote ${IMG}"
echo "scode: $("$SCODE_BIN" --version 2>/dev/null || echo 'version unavailable')"
