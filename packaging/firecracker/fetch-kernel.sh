#!/usr/bin/env bash
# fetch-kernel.sh — download a Firecracker-compatible guest kernel.
#
# Firecracker boots an uncompressed vmlinux directly (no bootloader).
# The Firecracker project publishes CI guest kernels with the virtio
# devices scode needs (virtio-blk, virtio-net) already built in; we
# pin a CI release line and pick the newest kernel on it.
#
# Usage:
#   ./fetch-kernel.sh [--out DIR]
#
# Environment:
#   FC_CI_LINE      CI artifact line to search (default: v1.12)
#   FC_KERNEL_KEY   Full S3 key override; skips discovery entirely.
set -euo pipefail

OUT_DIR="$(dirname "$0")/out"
while [ $# -gt 0 ]; do
    case "$1" in
        --out) OUT_DIR="$2"; shift 2 ;;
        -h|--help) grep '^#' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 1 ;;
    esac
done

ARCH="$(uname -m)"
CI_LINE="${FC_CI_LINE:-v1.12}"
BUCKET="https://s3.amazonaws.com/spec.ccfc.min"

mkdir -p "$OUT_DIR"

if [ -n "${FC_KERNEL_KEY:-}" ]; then
    key="$FC_KERNEL_KEY"
else
    # List the CI bucket for the newest vmlinux on the pinned line.
    key="$(curl -fsSL "${BUCKET}?prefix=firecracker-ci/${CI_LINE}/${ARCH}/vmlinux-&list-type=2" \
        | grep -oE "firecracker-ci/${CI_LINE}/${ARCH}/vmlinux-[0-9]+\.[0-9]+\.[0-9]+" \
        | sort -uV | tail -1)"
    if [ -z "$key" ]; then
        echo "error: no guest kernel found for ${ARCH} on CI line ${CI_LINE}" >&2
        echo "hint: set FC_KERNEL_KEY to a full S3 key to bypass discovery" >&2
        exit 1
    fi
fi

echo "fetching guest kernel: ${key}"
curl -fSL --progress-bar -o "${OUT_DIR}/vmlinux" "${BUCKET}/${key}"
echo "wrote $(du -h "${OUT_DIR}/vmlinux" | cut -f1) ${OUT_DIR}/vmlinux"
