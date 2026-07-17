<!--
  Rust-native CLI coding agent for hackers — terminal-native,
  pipe-composable, scrollback-safe. Built because Claude Code chose
  non-coders. This README is the canonical voice; sudo-code-roadmap.html holds
  the engineering plan; docs/ holds mechanism-level reference.
-->

# SUDO CODE

<p align="center">
  <img src="assets/logo.svg" alt="Sudo Code" width="600" />
</p>

<p align="center">
  <a href="#license"><img alt="License: MIT" src="https://img.shields.io/badge/License-MIT-blue.svg"></a>
  <img alt="Rust 2021" src="https://img.shields.io/badge/rust-2021-orange?logo=rust">
  <img alt="Platform" src="https://img.shields.io/badge/platform-macOS%20%7C%20Linux-lightgrey">
  <img alt="Protocol" src="https://img.shields.io/badge/protocol-ACP-purple">
  <img alt="Model-agnostic" src="https://img.shields.io/badge/models-Anthropic%20%C2%B7%20OpenAI%20%C2%B7%20xAI%20%C2%B7%20Gemini-blueviolet">
  <a href="./CONTRIBUTING.md"><img alt="PRs Welcome" src="https://img.shields.io/badge/PRs-welcome-success.svg"></a>
</p>

## FOR HACKERS.

**Built because Claude Code chose non-coders.**

There was a time when Claude Code was a real coding agent —
opinionated, fast, debuggable. The pivot toward onboarding the 99%
changed that: hidden options because "users might misclick", forced
auto-updates overriding pinned versions, new surface shipping while
core surface stayed broken, heavy-user issues going unanswered.

Sudo Code is what happens when a heavy user gives up asking and
starts building.

Rust-native CLI. Inline only — never hijacks your terminal.
Pipe-composable — works with your shell, doesn't replace it.
Model-agnostic — your subscription, your choice. Open source — your
fork, your call.

<p align="center">
  <img src="assets/scode-demo.gif" alt="Sudo Code terminal demo" width="900" />
</p>

---

## Who this is for

| FOR | NOT FOR |
|---|---|
| The 1% by daily agent token-burn | First-time coders looking for tutorials |
| Engineers who live in `tmux`, `ssh`, `vscode terminal` | Anyone who wants a GUI |
| Engineers the system, not the steps — roles, DoD, reviewers; scales 1 → 10 → 100 agents | Drives one agent at a time, prompt by prompt |
| Wants the process fully exposed — "reading `src/auth.rs:42-89`" not "reading file" — to scan-and-catch | Wants the agent to handle details and surface only the result |
| Owns the stack they run — pinned versions, readable sessions, forks when needed | Runs whatever the vendor pushes next |

Every row is a productivity differentiator, not a status one. The
left column pushes productivity further by **engineering the
interaction**; the right column saves cognitive load by **trusting
the tool**. Sudo Code optimizes the first axis — which is why we
expose full process by default. Not for staring. For scan-and-catch
when something is off, and zero ceremony when it's not.

The end-state of this workflow is one heavy user shipping at the
productivity of a small team. The arc looks like **1 session →
7-10 parallel sessions → copilot-worker fleets of 100+ agents**.
At stage 1 you drive one agent. At stage 2 you parallelise long
unattended tasks — until mental bandwidth caps around ten. At
stage 3 a copilot agent reviews worker agents; you supervise the
copilot, the copilot does the per-agent direction, and the fleet
scales past anything one person could track. Sudo Code is the
agent unit at every stage. The collaboration plane that makes
stage 3 real is described in
[Position in the larger picture](#position-in-the-larger-picture)
below.

If the right column is you, this isn't your tool — and that's not a
problem to solve. **Claude Code, Cursor, GitHub Copilot are excellent
for the 99%; use those.** Sudo Code is opinionated against the right
column on purpose. We don't ship modes that bridge the gap, and we
don't apologize for that.

---

## Design principles

Two columns: what we'll always do, what we'll never do. These aren't
aspirations — they're constraints that shape every PR.

### Always

| | |
|---|---|
| **Open source. MIT. Forever.** | No secret-sauce room, no "real enterprise tier behind a wall". |
| **Model-agnostic.** | Your subscription, your key, your proxy. We bind to no vendor. |
| **Headless first-class.** | What you see in REPL is what runs as a service — same binary, same surface. |
| **Local-first.** | Zero telemetry by default. Your prompts don't leave your machine unless you tell them to. |
| **Inline only.** | Your terminal stays yours. Scrollback, tmux, ssh, vscode terminal — all preserved. |
| **Sole goal: heavy-user productivity.** | Every design decision filters through "does this compress wall-clock between a heavy user's thought and outcome?" If not, it doesn't ship. Comfort, onboarding, compatibility for the 99% — explicitly not our problem. |
| **Stable surface. No silent updates.** | Semver. Breaking change = major bump + release notes. You pin a version, that version stays. |
| **Everything is file.** | Config in `.scode.json`. Sessions in jsonl. Plugins on the filesystem. Future state on the [nexus VFS](https://github.com/nexi-lab/nexus) — every secret, stream, agent, audit trace addressable through `sys_read` / `sys_write`. No opaque DBs. No sibling APIs hiding state. |
| **Session is yours.** | jsonl you can read, fork, replay, `awk` through. Zero lock-in. |
| **Dogfood non-negotiable.** | The team burns this binary daily. No-dogfood, no release. |
| **Polish before scope.** | New surface doesn't ship while existing surface is broken. Scope expansion is an admission of failure on the core, not an upgrade. |

### Never

| | |
|---|---|
| **Closed source.** | No proprietary fork. The repo you see is everything. |
| **Force auto-update.** | You pin, you stay. Forced silent updates is one of the reasons sudocode exists. |
| **Premium features behind a paywall.** | One binary. No free/pro split. |
| **Alternate-screen TUI.** | Never hijack your screen. Inline ANSI only. No `ratatui`, no split-pane, no `--tui` flag. |
| **In-CLI multi-agent dashboard.** | We're a unit. Dashboards belong in sudowork / your tmux / your IDE. |
| **Vendor lock.** | No proprietary model API. No proprietary protocol — ACP is open. |
| **Telemetry by default.** | Opt-in is explicit. Not buried in EULA clause 47. |
| **CLA / copyright assignment.** | Contributors keep their copyright. Commit directly. |
| **Pivot away from hackers.** | If we ever do, **we fork ourselves**. |
| **"Some users might misclick" as a reason.** | Hidden options behind config because the 99% might fumble = treating us like the 99%. We're not. Either ship a feature, or don't. |

---

## Position in the larger picture

**Sudo Code is a unit, not a hub.** It plugs into a larger
collaboration plane through one shared primitive: the `chat-with-me`
mailbox on the [nexus VFS](https://github.com/nexi-lab/nexus).
Orchestration, multi-agent UI, fleet management — none of that is
our job. Sudo Code is the well-behaved agent unit that other
surfaces can drive: a human in a [sudowork](https://sudowork.sudoprivacy.com)
chat, a [hydra](https://github.com/sudoprivacy/hydra)-style
orchestrator, another sudocode running as copilot, a peer agent like
Claude or Codex on ACP. Same primitive, same plane.

### Topology — sudocode is one box among peers

```mermaid
flowchart LR
    subgraph Orch["Orchestrator — NOT sudocode<br/>(sudowork main · hydra UI)"]
      O["ManagedAgentService<br/>start_session_v1 · cancel · get_session"]
    end

    subgraph Units["Agent units — sudocode is one of these"]
      direction TB
      H["👤 human"]
      SC["scode pid<br/>copilot OR worker role"]
      CL["claude pid (via ACP)"]
      CX["codex pid (via ACP)"]
    end

    subgraph Plane["nexus VFS plane"]
      MB["chat-with-me DT_STREAMs<br/>/agents/{name}/chat-with-me<br/>/proc/{pid}/chat-with-me<br/><br/>sys_watch + sys_write<br/>kernel stamps 'from' field"]
    end

    O -. spawn .-> SC
    O -. spawn .-> CL
    O -. spawn .-> CX

    H <--> MB
    SC <--> MB
    CL <--> MB
    CX <--> MB
```

Sudo Code is interchangeable with `claude` / `codex` / a human at the
mailbox primitive. It does not sit in the orchestrator box.

### One binary, three deployment modes

Same `scode` binary is a copilot, a worker, or a standalone CLI —
chosen by which `FsBackend` impl the runtime routes through:

```mermaid
flowchart TB
    subgraph Runtime["sudocode runtime (Rust)"]
      direction LR
      SES["SessionStore"]
      CFG["ConfigLoader"]
      FOPS["file_ops helpers"]
    end

    TRAIT["FsBackend trait<br/>(one trait · three impls)"]

    SES --> TRAIT
    CFG --> TRAIT
    FOPS --> TRAIT

    TRAIT --> B1["StdFsBackend<br/>host std::fs"]
    TRAIT --> B2["NexusVfsFsBackend<br/>gRPC → remote kernel"]
    TRAIT --> B3["KernelFsBackend&lt;Kernel&gt;<br/>in-process syscalls"]

    B1 -.deployed in.-> M1["standalone CLI<br/>(scode on hacker's laptop)"]
    B2 -.deployed in.-> M2["edge / dev CLI<br/>(scode → remote nexus)"]
    B3 -.deployed in.-> M3["sudocode-host binary<br/>(prod managed agent)"]
```

### Hydra evolution

[Hydra](https://github.com/sudoprivacy/hydra) — today a TypeScript
VS Code extension shelling out to tmux + git worktrees to spawn
Claude / Gemini / Codex — gets thin once `sudocode-host` lands:

```mermaid
flowchart LR
    subgraph Today["Today — hydra is a hack on system tools"]
      direction TB
      T1["VS Code extension<br/>(TypeScript)"]
      T2["tmux pane persistence"]
      T3["git worktree isolation"]
      T4["spawned claude / gemini / codex<br/>via shell"]
      T1 --> T2 --> T3 --> T4
    end

    subgraph Future["After sudocode-on-nexus-vfs ships"]
      direction TB
      F1["VS Code sidebar<br/>(thin — UI only)"]
      F2["ManagedAgentService<br/>start_session_v1()"]
      F3["WorkspaceBoundaryHook<br/>(VFS-enforced isolation)"]
      F4["scode / claude / codex<br/>as agent units"]
      F5["chat-with-me DT_STREAM<br/>(persistence + raft replication)"]
      F1 --> F2 --> F3 --> F4 --> F5
    end

    Today ==>|"hydra internals rewritten, becomes thin"| Future
```

Sudo Code is the agent unit underneath that whole future picture.
For the engineering plan to get there, see
[`sudo-code-roadmap.html` § Goal 4](./sudo-code-roadmap.html).

---

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/sudoprivacy/sudocode/main/install.sh | sh
```

`install.sh` downloads the prebuilt `scode` binary for the host
platform (macOS arm64/x64, Linux x64/arm64) and verifies a SHA-256
checksum. On macOS Apple Silicon: `/opt/homebrew/bin`. On macOS x64
and Linux: `/usr/local/bin`, prompting for `sudo` only when stdin is
a TTY. When the preferred system directory is unwritable and `sudo`
is unavailable, the script installs to `$HOME/.local/bin`. Windows
users grab the zip from the
[Releases page](https://github.com/sudoprivacy/sudocode/releases/latest).

Overrides:

- `SCODE_VERSION=v0.1.5 sh install.sh` — pin a specific release.
- `sh install.sh --no-sudo` — install to `$HOME/.local/bin`.
- `SCODE_INSTALL_DIR=$HOME/.local/bin sh install.sh` — explicit per-user install.
- `sh install.sh --prefix /usr/local` — explicit prefix.

China mirror (checksums still verified against GitHub):

```bash
SCODE_MIRROR=https://sudowork-download-1309794936.cos.ap-beijing.myqcloud.com/sudocode/release/latest \
  curl -fsSL https://raw.githubusercontent.com/sudoprivacy/sudocode/main/install.sh | sh
```

## Build from source

```bash
git clone https://github.com/sudoprivacy/sudocode.git
cd sudocode/rust
cargo build --release
# Binary at ./target/release/scode
```

Requires a recent stable Rust 2021 toolchain.

## Quick Start

```bash
# Pick an auth mode (see docs/authentication.md)
export CLAUDE_CODE_OAUTH_TOKEN="sk-ant-oat-..."

# Interactive REPL
scode

# One-shot prompt — pipe-composable, like every unix tool
scode "explain this codebase" | bat
scode "list failing tests" --output-format json | jq '.tests[]'

# Read a plan from stdin, resume a prior session
cat plan.md | scode --resume <session-id>

# Headless ACP server for editors / web clients
scode acp serve --port 8080

# Health check
scode doctor
```

For day-to-day workflows see [`docs/usage.md`](./docs/usage.md).

## Architecture — current implementation

```mermaid
flowchart LR
    subgraph Clients
      Term([Terminal user])
      Editor([Editor / IDE])
      Browser([Browser / Web UI])
      Service([Backend service])
    end

    Term -->|REPL · one-shot| CLI
    Editor -->|ACP stdio| STDIO[scode acp]
    Browser -->|WebSocket + HTML| WS[scode acp serve --port N]
    Service -->|WebSocket / JSON-RPC| WS

    CLI[scode CLI / REPL] --> RT
    STDIO --> RT
    WS --> RT

    RT[Runtime<br/>session · permissions · sandbox · config] --> API[API Client<br/>SSE streaming]
    RT --> TOOLS[Tools]
    RT --> MCP[MCP servers]
    RT --> PLUG[Plugins / Skills]

    TOOLS --> T1[Bash · Read · Write · Edit]
    TOOLS --> T2[Grep · Glob · WebSearch · WebFetch]

    API --> P1[Anthropic]
    API --> P2[OpenAI / Codex]
    API --> P3[xAI · Gemini]
    API --> P4[Proxy / Mock]
```

The Cargo workspace is described in
[`rust/README.md`](./rust/README.md).

## Documentation

- [`sudo-code-roadmap.html`](./sudo-code-roadmap.html) — goals, design notes, engineering sequencing.
- [`docs/usage.md`](./docs/usage.md) — REPL, one-shot, JSON output, resume, doctor.
- [`docs/authentication.md`](./docs/authentication.md) — auth modes and credentials.
- [`docs/permissions-and-sandbox.md`](./docs/permissions-and-sandbox.md) — permission modes, Linux sandbox.
- [`docs/acp.md`](./docs/acp.md) — ACP transports and the embedded Web UI.
- [`docs/models.md`](./docs/models.md) — aliases, provider-specific handling.
- [`docs/plugins.md`](./docs/plugins.md) — authoring and using `scode` plugins.
- [`docs/container.md`](./docs/container.md) — building and running inside a container.
- [`sudo-code-roadmap.html`](./sudo-code-roadmap.html) Goal 2 — what claude-code parity means and how it is tracked (reference sources, resolution taxonomy, sync markers).
- [`docs/mock-parity-harness.md`](./docs/mock-parity-harness.md) — the deterministic mock backend and harness.
- [`rust/README.md`](./rust/README.md) — Cargo workspace map.

## Contributing

See [`CONTRIBUTING.md`](./CONTRIBUTING.md). Contributors keep their
copyright — no CLA, no assignment, no waivers. Commit directly.

Chinese-speaking hackers: open issues and PRs in Chinese on the
GitHub issue tracker if it's faster for you. The canonical docs
(this README, ROADMAP, contracts) stay in English so there's one
source of truth, but conversations in any language are welcome.

## License

Released under the MIT License. See the per-crate license fields in
[`rust/Cargo.toml`](./rust/Cargo.toml).

---

Sudo Code is maintained by the Sudo Privacy community as the agent
unit underneath the [Sudowork](https://sudowork.sudoprivacy.com)
collaboration platform.
