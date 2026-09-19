# Permissions and Sandbox

`scode` gates every filesystem and shell tool call through a permission
mode and, where the platform provides one, through a process sandbox:
Linux user namespaces or macOS Seatbelt.

## Permission modes

| Mode | Behavior |
|---|---|
| `read-only` | Read tools and web tools execute. Filesystem and shell mutations are gated to no-op. |
| `workspace-write` | Writes execute inside the current workspace. Ambient shell mutations are gated. |
| `prompt` | Each privileged tool call surfaces an interactive approval. |
| `allow` | Tool calls execute as approved by the runner — for non-interactive automation. |
| `danger-full-access` | All tool calls execute. |

Select a mode with `--permission-mode <MODE>` or set `permissionMode` in
`.scode.json`. The runtime default is `danger-full-access`.

```bash
scode --permission-mode workspace-write
```

## Linux sandbox

On Linux `scode` can run tools inside a user-namespace sandbox via
`unshare` (no root required).

Filesystem modes:

- `off` — tools share the host filesystem.
- `workspace-only` — tools see the current workspace and the standard
  read-only mounts.
- `allow-list` — tools see the workspace plus an explicit set of mounts.

Network isolation is independently configurable.

On Linux the filesystem mode is advisory: it redirects `HOME` and
`TMPDIR` into the workspace and is exported to the command as
`SUDOCODE_SANDBOX_FILESYSTEM_MODE`, but the kernel does not enforce it.

## macOS sandbox (Seatbelt)

On macOS `scode` can wrap `bash` in `/usr/bin/sandbox-exec` with a
generated Seatbelt profile. Unlike the Linux backend this one is
enforced: with `filesystemMode` `workspace-only` or `allow-list`, writes
outside the workspace are denied by the OS, and `networkIsolation`
denies all network access.

The writable set is the workspace, the repository's shared `.git`
directory (so `git commit` works from a linked worktree), the system and
per-user temp trees, the `/dev` devices a shell needs, and any
`allowedMounts`. Reads are never restricted.

Seatbelt is **opt-in**. A default install leaves `sandbox.enabled` unset
and macOS sessions run unconfined, so turning it on for everyone would
start denying writes to package caches and global installs overnight.
Enable it per project or per user:

```json
{
  "sandbox": {
    "enabled": true,
    "filesystemMode": "workspace-only",
    "networkIsolation": false,
    "allowedMounts": ["~/.cargo", "~/.npm"]
  }
}
```

Passing `filesystemMode` or `isolateNetwork` on a single `bash` call also
counts as opting in for that call. `scode sandbox` shows the
resolved backend (`none`, `linux-namespaces`, or `macos-seatbelt`) and,
while Seatbelt is off, the fallback reason says how to turn it on.

A denied write surfaces to the model as `Operation not permitted` in the
command's stderr. The bash tool description tells the model that this is
policy rather than a bug in the command, and that
`dangerouslyDisableSandbox` is only for runs the user explicitly asked to
be unsandboxed.

`scode` detects Docker, Podman, and other container markers via
`/.dockerenv`, `/run/.containerenv`, env hints, and `/proc/1/cgroup`, and
surfaces the detection through `scode sandbox` and `scode doctor`.

```bash
scode sandbox
```

## Inspecting the current state

```bash
scode doctor
```

`scode doctor` reports the resolved permission mode, the active sandbox
configuration, and any container markers detected on the host.
