#!/usr/bin/env bash
# build-data-volume.sh — create the persistent per-session data volume.
#
# One volume per VM. The volume carries everything mutable, which is
# exactly what makes scode sessions persistent across VM restarts:
#
#   /home        → guest $HOME (→ ~/.nexus/sudocode auth, config, memory)
#   /workspace   → the repo scode works in; session files live inside it
#                  at .scode/sessions/<workspace-hash>/
#   /env         → sourced by /sbin/scode-init (secrets + overrides)
#
# NEVER attach one volume to two running VMs at once — ext4 is not a
# cluster filesystem and concurrent writers will corrupt it.
#
# Usage:
#   ./build-data-volume.sh --name mysession [--size MiB] [--out DIR]
#                          [--workspace-from DIR] [--env-file FILE]
#
# If ANTHROPIC_API_KEY is set in the calling environment and no
# --env-file is given, it is written into /env on the volume.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
OUT_DIR="${HERE}/out"
NAME=""
SIZE_MIB=4096
WORKSPACE_FROM=""
ENV_FILE=""

while [ $# -gt 0 ]; do
    case "$1" in
        --name) NAME="$2"; shift 2 ;;
        --size) SIZE_MIB="$2"; shift 2 ;;
        --out) OUT_DIR="$2"; shift 2 ;;
        --workspace-from) WORKSPACE_FROM="$2"; shift 2 ;;
        --env-file) ENV_FILE="$2"; shift 2 ;;
        -h|--help) grep '^#' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 1 ;;
    esac
done

[ -n "$NAME" ] || { echo "error: --name is required (one volume per session/VM)" >&2; exit 1; }
command -v mke2fs >/dev/null || { echo "error: mke2fs (e2fsprogs) is required" >&2; exit 1; }

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

mkdir -p "$STAGE/home" "$STAGE/workspace"

if [ -n "$WORKSPACE_FROM" ]; then
    [ -d "$WORKSPACE_FROM" ] || { echo "error: --workspace-from ${WORKSPACE_FROM} is not a directory" >&2; exit 1; }
    echo "seeding workspace from ${WORKSPACE_FROM}…"
    cp -a "$WORKSPACE_FROM/." "$STAGE/workspace/"
fi

if [ -n "$ENV_FILE" ]; then
    install -m 0600 "$ENV_FILE" "$STAGE/env"
elif [ -n "${ANTHROPIC_API_KEY:-}" ]; then
    umask 077
    {
        echo "# written by build-data-volume.sh — sourced by /sbin/scode-init"
        echo "export ANTHROPIC_API_KEY='${ANTHROPIC_API_KEY}'"
        echo "# export SCODE_ARGS='--resume latest'   # override launch args"
    } > "$STAGE/env"
else
    umask 077
    {
        echo "# sourced by /sbin/scode-init — put API keys / overrides here"
        echo "# export ANTHROPIC_API_KEY=sk-ant-…"
        echo "# export SCODE_ARGS='--resume latest'"
    } > "$STAGE/env"
    echo "note: no ANTHROPIC_API_KEY in environment and no --env-file;" >&2
    echo "      wrote a template /env — fill it in before first boot or" >&2
    echo "      authenticate interactively inside the VM (persists in /home)." >&2
fi

mkdir -p "$OUT_DIR"
IMG="${OUT_DIR}/data-${NAME}.ext4"
if [ -e "$IMG" ]; then
    echo "error: ${IMG} already exists — refusing to overwrite a session volume" >&2
    exit 1
fi
echo "packing ${SIZE_MIB} MiB ext4 data volume…"
mke2fs -q -t ext4 -L "scode-${NAME}" -d "$STAGE" "$IMG" "${SIZE_MIB}M"
echo "wrote ${IMG}"
