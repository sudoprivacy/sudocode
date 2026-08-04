# scode on Firecracker

Run `scode` inside a [Firecracker](https://firecracker-microvm.github.io/)
microVM with **persistent sessions**: the VM is disposable, the session
is not. Boot → work → exit → boot again later → `--resume latest` picks
up where you left off.

## Model

```text
host
├── vmlinux            guest kernel   (fetch-kernel.sh, shared)
├── rootfs.ext4        READ-ONLY      (build-rootfs.sh, shared by all VMs)
│     Debian slim + git + ca-certs + scode + /sbin/scode-init
└── data-<name>.ext4   READ-WRITE     (build-data-volume.sh, ONE per VM)
      /home            → guest $HOME  → ~/.nexus/sudocode (auth, config, memory)
      /workspace       → repo + .scode/sessions/<hash>/   (session files)
      /env             → secrets + overrides, sourced by init
```

Firecracker cannot share host directories into a guest (no virtio-fs by
design) — persistence is a block device whose backing file lives on the
host. Everything mutable sits on the data volume; the rootfs is a golden
image you rebuild only to upgrade scode.

## Quick start

```bash
cd packaging/firecracker

# once per host
./fetch-kernel.sh
(cd ../../rust && cargo build --release)
./build-rootfs.sh

# once per session
ANTHROPIC_API_KEY=sk-ant-… ./build-data-volume.sh --name alpha \
    --workspace-from ~/src/myproject

# every boot (root or CAP_NET_ADMIN for TAP/NAT)
sudo ./launch.sh --data out/data-alpha.ext4
```

Your terminal becomes the scode REPL over the serial console. Exiting
scode powers the VM off; the next `launch.sh` with the same volume
resumes the latest session automatically (init runs `scode --resume
latest` whenever session files exist on the volume).

## Multiple sessions

One microVM per session, each with its own index and volume:

```bash
sudo ./launch.sh --index 0 --data out/data-alpha.ext4   # tmux pane 1
sudo ./launch.sh --index 1 --data out/data-beta.ext4    # tmux pane 2
```

`--index N` (0–255) gives each VM its own TAP device (`scode-tapN`) and
/30 subnet (`172.30.N.0/30`). The rootfs is attached read-only and safe
to share.

**Never attach one data volume to two running VMs.** ext4 is not a
cluster filesystem; concurrent writers corrupt it. One session = one
volume = at most one running VM.

## Configuration

| Knob | Where | Default |
|---|---|---|
| API key / secrets | `/env` on the data volume (or authenticate once in-VM; persists in `/home`) | template written at volume build |
| scode launch args | `SCODE_ARGS` in `/env` | resume-latest-or-fresh |
| vCPUs / memory | `launch.sh --vcpus N --mem MiB` | 2 vCPU, 2048 MiB |
| Egress | NAT via default route (skip with `--no-nat`) | on |
| Kernel line | `FC_CI_LINE` env for `fetch-kernel.sh` | v1.12 CI artifacts |

## Caveats

- **`/dev/kvm` required.** Inside another VM you need nested
  virtualization enabled.
- **In-guest sandbox degrades gracefully.** scode's `unshare`-based
  user-namespace sandbox needs `CONFIG_USER_NS` in the guest kernel;
  the Firecracker CI kernels may not enable it. `scode sandbox` will
  report the fallback. The microVM boundary itself is the stronger
  isolation layer here.
- **Snapshots are not persistence.** Firecracker's snapshot/restore can
  warm-start a VM but is not durable storage; the data volume is the
  source of truth.
- Guest DNS is baked into the rootfs (`1.1.1.1`, `8.8.8.8`); edit
  `build-rootfs.sh` if your network requires otherwise.
