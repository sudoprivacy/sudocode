# Permissions and Sandbox

`scode` gates every filesystem and shell tool call through a permission
mode and, on Linux, optionally through a user-namespace sandbox.

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

`scode` detects Docker, Podman, and other container markers via
`/.dockerenv`, `/run/.containerenv`, env hints, and `/proc/1/cgroup`, and
surfaces the detection through `scode sandbox` and `scode doctor`.

```bash
scode sandbox --status
```

## Inspecting the current state

```bash
scode doctor
```

`scode doctor` reports the resolved permission mode, the active sandbox
configuration, and any container markers detected on the host.
