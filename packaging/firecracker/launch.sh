#!/usr/bin/env bash
# launch.sh — start one scode microVM on the host.
#
# Wires up per-VM TAP networking (with NAT so the agent loop can reach
# the model API), generates the Firecracker vmconfig, and launches
# Firecracker in the foreground: the serial console on your terminal IS
# the scode REPL. Exit scode → VM powers off → script cleans up.
#
# Run several sessions in parallel by launching with distinct --index
# values, each with its own data volume:
#
#   ./launch.sh --index 0 --data out/data-alpha.ext4 &
#   ./launch.sh --index 1 --data out/data-beta.ext4  &
#
# The rootfs is attached read-only and may be shared by all VMs; a data
# volume must only ever be attached to one running VM.
#
# Usage:
#   ./launch.sh --data VOLUME.ext4 [--index N] [--kernel PATH]
#               [--rootfs PATH] [--vcpus N] [--mem MiB] [--no-nat]
#
# Requirements: firecracker in PATH, /dev/kvm access, root (or
# CAP_NET_ADMIN) for TAP/NAT setup.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
KERNEL="${HERE}/out/vmlinux"
ROOTFS="${HERE}/out/rootfs.ext4"
DATA=""
INDEX=0
VCPUS=2
MEM_MIB=2048
SETUP_NAT=1

while [ $# -gt 0 ]; do
    case "$1" in
        --data) DATA="$2"; shift 2 ;;
        --index) INDEX="$2"; shift 2 ;;
        --kernel) KERNEL="$2"; shift 2 ;;
        --rootfs) ROOTFS="$2"; shift 2 ;;
        --vcpus) VCPUS="$2"; shift 2 ;;
        --mem) MEM_MIB="$2"; shift 2 ;;
        --no-nat) SETUP_NAT=0; shift ;;
        -h|--help) grep '^#' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 1 ;;
    esac
done

[ -n "$DATA" ] || { echo "error: --data VOLUME.ext4 is required (see build-data-volume.sh)" >&2; exit 1; }
[ -f "$DATA" ] || { echo "error: data volume ${DATA} not found" >&2; exit 1; }
[ -f "$KERNEL" ] || { echo "error: kernel ${KERNEL} not found (run fetch-kernel.sh)" >&2; exit 1; }
[ -f "$ROOTFS" ] || { echo "error: rootfs ${ROOTFS} not found (run build-rootfs.sh)" >&2; exit 1; }
command -v firecracker >/dev/null || { echo "error: firecracker not in PATH" >&2; exit 1; }
[ -r /dev/kvm ] && [ -w /dev/kvm ] || { echo "error: /dev/kvm not accessible" >&2; exit 1; }
if [ "$INDEX" -lt 0 ] || [ "$INDEX" -gt 255 ]; then
    echo "error: --index must be 0..255" >&2; exit 1
fi

# --- per-VM network plumbing ------------------------------------------------
# Each VM index gets its own /30: host .1, guest .2, inside 172.30.0.0/16.
TAP="scode-tap${INDEX}"
HOST_IP="172.30.${INDEX}.1"
GUEST_IP="172.30.${INDEX}.2"
NETMASK="255.255.255.252"

RUN_DIR="${TMPDIR:-/tmp}/scode-fc-${INDEX}"
API_SOCK="${RUN_DIR}/firecracker.sock"
VMCONFIG="${RUN_DIR}/vmconfig.json"
mkdir -p "$RUN_DIR"
rm -f "$API_SOCK"

cleanup() {
    ip link del "$TAP" 2>/dev/null || true
    rm -f "$API_SOCK"
}
trap cleanup EXIT

ip link del "$TAP" 2>/dev/null || true
ip tuntap add dev "$TAP" mode tap
ip addr add "${HOST_IP}/30" dev "$TAP"
ip link set dev "$TAP" up

if [ "$SETUP_NAT" -eq 1 ]; then
    # Idempotent NAT: outbound masquerade for this VM's /30 via the
    # default route interface. Required for the agent loop to reach
    # the model API; skip with --no-nat if you handle egress yourself.
    OUT_IF="$(ip -o route get 8.8.8.8 2>/dev/null | sed -n 's/.* dev \([^ ]*\).*/\1/p' || true)"
    if [ -n "$OUT_IF" ]; then
        sysctl -qw net.ipv4.ip_forward=1
        iptables -t nat -C POSTROUTING -s "${HOST_IP}/30" -o "$OUT_IF" -j MASQUERADE 2>/dev/null \
            || iptables -t nat -A POSTROUTING -s "${HOST_IP}/30" -o "$OUT_IF" -j MASQUERADE
        iptables -C FORWARD -i "$TAP" -o "$OUT_IF" -j ACCEPT 2>/dev/null \
            || iptables -A FORWARD -i "$TAP" -o "$OUT_IF" -j ACCEPT
        iptables -C FORWARD -i "$OUT_IF" -o "$TAP" -m state --state RELATED,ESTABLISHED -j ACCEPT 2>/dev/null \
            || iptables -A FORWARD -i "$OUT_IF" -o "$TAP" -m state --state RELATED,ESTABLISHED -j ACCEPT
    else
        echo "warning: no default route found; skipping NAT setup" >&2
    fi
fi

# --- vmconfig ---------------------------------------------------------------
# ip= kernel-level autoconfig: guest::gateway:netmask::iface:off
BOOT_ARGS="console=ttyS0 reboot=k panic=1 pci=off quiet"
BOOT_ARGS+=" init=/sbin/scode-init"
BOOT_ARGS+=" ip=${GUEST_IP}::${HOST_IP}:${NETMASK}::eth0:off"

cat > "$VMCONFIG" <<EOF
{
  "boot-source": {
    "kernel_image_path": "$(realpath "$KERNEL")",
    "boot_args": "${BOOT_ARGS}"
  },
  "drives": [
    {
      "drive_id": "rootfs",
      "path_on_host": "$(realpath "$ROOTFS")",
      "is_root_device": true,
      "is_read_only": true
    },
    {
      "drive_id": "data",
      "path_on_host": "$(realpath "$DATA")",
      "is_root_device": false,
      "is_read_only": false
    }
  ],
  "network-interfaces": [
    {
      "iface_id": "eth0",
      "host_dev_name": "${TAP}"
    }
  ],
  "machine-config": {
    "vcpu_count": ${VCPUS},
    "mem_size_mib": ${MEM_MIB}
  }
}
EOF

echo "starting scode microVM #${INDEX}  (guest ${GUEST_IP}, data $(basename "$DATA"))"
echo "serial console = scode REPL; exiting scode powers the VM off."
# No exec: the EXIT trap must still run to tear down the TAP device.
firecracker --api-sock "$API_SOCK" --config-file "$VMCONFIG"
