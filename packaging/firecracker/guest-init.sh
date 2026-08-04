#!/bin/bash
# guest-init.sh — PID 1 inside the scode microVM.
#
# Installed into the rootfs as /sbin/scode-init by build-rootfs.sh and
# selected with `init=/sbin/scode-init` on the kernel command line.
#
# Layout at runtime:
#   /dev/vda   read-only rootfs (Debian userland + scode binary)
#   /dev/vdb   read-write persistent data volume (built by
#              build-data-volume.sh):
#                /data/home        → becomes $HOME (so ~/.nexus/sudocode
#                                    auth/config/memory persist)
#                /data/workspace   → working directory; sessions live in
#                                    .scode/sessions/<hash>/ inside it
#                /data/env         → sourced for secrets and overrides
#                                    (ANTHROPIC_API_KEY, SCODE_ARGS, …)
#
# The serial console (ttyS0) is the interactive scode REPL. When scode
# exits, the VM powers off; the data volume keeps everything needed for
# `--resume latest` in the next boot.
set -u

log() { echo "[scode-init] $*"; }

# --- kernel filesystems -----------------------------------------------------
mount -t proc proc /proc
mount -t sysfs sys /sys
mount -t devtmpfs dev /dev 2>/dev/null || true
mkdir -p /dev/pts /dev/shm
mount -t devpts devpts /dev/pts 2>/dev/null || true
mount -t tmpfs tmpfs /dev/shm 2>/dev/null || true
mount -t tmpfs tmpfs /tmp
mount -t tmpfs tmpfs /run

# --- persistent data volume -------------------------------------------------
mkdir -p /data
if ! mount /dev/vdb /data; then
    log "FATAL: no data volume on /dev/vdb — sessions would not persist."
    log "Build one with build-data-volume.sh and pass it to launch.sh."
    sync
    echo o > /proc/sysrq-trigger
    sleep 5
    exit 1
fi
mkdir -p /data/home /data/workspace

# --- environment ------------------------------------------------------------
export HOME=/data/home
export USER=root
export SHELL=/bin/bash
export TERM="${TERM:-xterm-256color}"
export LANG=C.UTF-8
# /data/env carries secrets (API keys) and overrides (SCODE_ARGS, proxy
# settings). It lives on the data volume so the read-only rootfs stays
# credential-free and shareable between VMs.
if [ -f /data/env ]; then
    # shellcheck disable=SC1091
    . /data/env
fi

cd /data/workspace || cd /

# --- resume-or-fresh --------------------------------------------------------
# Sessions are stored at <workspace>/.scode/sessions/<hash>/*. If any
# exist, resume the latest one; otherwise start fresh. SCODE_ARGS from
# /data/env overrides this default entirely.
scode_cmd=(scode)
if [ -n "${SCODE_ARGS:-}" ]; then
    # shellcheck disable=SC2206
    scode_cmd=(scode ${SCODE_ARGS})
elif [ -n "$(find .scode/sessions -type f -print -quit 2>/dev/null)" ]; then
    scode_cmd=(scode --resume latest)
fi

log "data volume mounted; HOME=$HOME cwd=$PWD"
log "starting: ${scode_cmd[*]}"

# setsid -c makes ttyS0 the controlling terminal so line editing and
# Ctrl-C work in the REPL. This bash (PID 1) keeps running to reap any
# orphaned children scode's tools leave behind.
setsid -c "${scode_cmd[@]}" <>/dev/ttyS0 >&0 2>&1
status=$?

log "scode exited (status ${status}); syncing and powering off"
sync
umount /data 2>/dev/null || true
echo o > /proc/sysrq-trigger
sleep 5
