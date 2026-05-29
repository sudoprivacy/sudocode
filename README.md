<!-- Language: 🇬🇧 English (this file) · [🇨🇳 简体中文](./README_zh.md) -->

# Sudo Code

<p align="center">
  <img src="assets/logo.svg" alt="Sudo Code" width="600" />
</p>

<p align="center">
  <a href="#license"><img alt="License: MIT" src="https://img.shields.io/badge/License-MIT-blue.svg"></a>
  <img alt="Rust 2021" src="https://img.shields.io/badge/rust-2021-orange?logo=rust">
  <img alt="Version" src="https://img.shields.io/badge/version-0.1.6-brightgreen">
  <img alt="Platform" src="https://img.shields.io/badge/platform-macOS%20%7C%20Linux-lightgrey">
  <img alt="Protocol" src="https://img.shields.io/badge/protocol-ACP-purple">
  <img alt="Model-agnostic" src="https://img.shields.io/badge/models-Anthropic%20%C2%B7%20OpenAI%20%C2%B7%20xAI%20%C2%B7%20Gemini-blueviolet">
  <a href="#contributing"><img alt="PRs Welcome" src="https://img.shields.io/badge/PRs-welcome-success.svg"></a>
</p>

<p align="center">
  <b>An engine for the AI agent era.</b><br/>
  Rust-native · model-agnostic · headless-first · safe by design.
</p>

<p align="center">
  <img src="assets/scode-demo.gif" alt="Sudo Code terminal demo highlighting Rust-native speed, model-agnostic providers, headless ACP infrastructure, and safe-by-design permissions" width="900" />
</p>

---

## What is Sudo Code?

**Sudo Code** (`scode`) is a high-performance, Rust-native implementation of a coding agent — in the same family as Claude Code or Aider — but built for two audiences from day one: **humans at a terminal** and **machines on a wire**.

- **Model-agnostic.** First-class support for Anthropic, OpenAI, xAI, and Gemini, plus OAuth subscriptions and arbitrary proxy backends. Swap providers with a single `--auth` / `--model` flag.
- **Performance-first runtime.** Single native binary, no Node/Python startup tax. Built on Rust + `tokio` for a lean memory footprint, deterministic shutdown, and predictable resource use under load.
- **Headless infrastructure.** `scode acp serve` exposes the **Agent Communication Protocol** over both **stdio** (for editors and CLI orchestrators) and **WebSocket** (for browsers, IDE plugins, and service backends) — turning `scode` into "agent as a service."
- **Embedded Web UI.** The WebSocket mode ships with a built-in interactive web client. Run `scode acp serve --port 8080`, open `http://localhost:8080/`, and you have a working agent UI without installing anything else.
- **Safe by design.** A hardened permission system with explicit modes (`read-only`, `workspace-write`, `danger-full-access`) plus a Linux sandbox using user namespaces for filesystem and network isolation.

Sudo Code is flexible infrastructure for building, running, and embedding coding agents — anywhere from a developer's laptop to a production service mesh.

## Architecture

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

Nine crates, one binary. See [`rust/README.md`](./rust/README.md) for crate-level responsibilities.

## Installation

```bash
git clone https://github.com/sudoprivacy/sudocode.git
cd sudocode/rust
cargo build --release

# Binary is at ./target/release/scode
```

Requires a recent stable Rust toolchain (2021 edition). macOS and Linux are supported.

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/sudoprivacy/sudocode/main/install.sh | sh
```

Downloads the latest prebuilt `scode` binary and verifies its SHA-256 checksum. On macOS Apple Silicon we install to `/opt/homebrew/bin`; on macOS x64 and Linux we install to `/usr/local/bin`, prompting for `sudo` if needed (only when stdin is a TTY — `curl … | sh` won't hang). If the preferred system dir isn't writable and sudo isn't available, we fall back to `$HOME/.local/bin`. macOS (arm64/x64) and Linux (x64/arm64) are supported; Windows users should grab the zip from the [Releases page](https://github.com/sudoprivacy/sudocode/releases/latest).

Overrides:

- `SCODE_VERSION=v0.1.5 sh install.sh` — pin a specific release.
- `sh install.sh --no-sudo` — never prompt for sudo; install to `$HOME/.local/bin` instead.
- `SCODE_INSTALL_DIR=$HOME/.local/bin sh install.sh` — explicit per-user install (no `sudo`).
- `sh install.sh --prefix /usr/local` — explicit prefix (no `sudo`).

**China mirror** — faster downloads for mainland China users (checksums still verified against GitHub):

```bash
SCODE_MIRROR=https://sudowork-download-1309794936.cos.ap-beijing.myqcloud.com/sudocode/release/latest \
  curl -fsSL https://raw.githubusercontent.com/sudoprivacy/sudocode/main/install.sh | sh
```

Already built from source? Skip to [Quick Start](#quick-start).

## Quick Start

```bash
# Set your credentials (pick one)
export ANTHROPIC_API_KEY="sk-ant-..."             # direct API key
export CLAUDE_CODE_OAUTH_TOKEN="sk-ant-oat-..."   # Claude subscription token
# or use a proxy:
export PROXY_AUTH_TOKEN="your-token"
export PROXY_BASE_URL="https://your-proxy.com"

# Interactive REPL
scode

# One-shot prompt
scode "explain this codebase"

# Health check
scode doctor
```

## Run as infrastructure — `scode acp serve`

`scode` speaks the **Agent Communication Protocol (ACP)** natively, in two transports:

```bash
# 1) stdio — for editors, IDE plugins, and CLI orchestrators
scode acp

# 2) WebSocket + embedded Web UI — for browsers and service backends
scode acp serve --port 8080
#   → JSON-RPC over WebSocket at  ws://localhost:8080/ws
#   → Interactive Web UI at       http://localhost:8080/
```

Both transports share the **same handler chain**, so WebSocket clients get full feature parity with stdio — including streaming, tool use, elicitation, and permission prompting. This makes `scode` a drop-in agent core for:

- **Editor plugins** (Zed, VS Code, JetBrains) — speak ACP over stdio.
- **Web apps and dashboards** — connect to the WebSocket endpoint, or just point a browser at `/` to use the embedded UI.
- **Automation pipelines and microservices** — run `scode acp serve` as a long-lived process behind a load balancer.
- **Sub-agents and orchestrators** — fan out work to multiple `scode` instances over the wire.

> [!TIP]
> The embedded Web UI is a zero-install way to demo, debug, or share an agent session. Bind to `127.0.0.1` for local-only use, or expose the port behind your own auth proxy for team access.

## Developer Mode: Zero-config Protocol Debugging

`scode` ships with a deterministic, Anthropic-compatible mock service designed for engineering work against the agent harness — not for live reasoning. Use it to exercise the **ACP integration**, **tool-dispatch logic**, and **UI / streaming behavior** end-to-end **without consuming API credits**.

Typical use cases:

- Validating editor or IDE plugins that speak ACP over stdio.
- Smoke-testing WebSocket clients (including the embedded Web UI) against the same handler chain that production traffic hits.
- Writing CI integration tests for tool dispatch, permission prompts, and SSE streaming without flakiness or quota.
- Debugging a new provider adapter or proxy without burning real tokens.

**Workflow — point `scode` at the local mock:**

```bash
# Terminal 1 — start the deterministic mock on a fixed port
cd rust
cargo run -p mock-anthropic-service -- --bind 127.0.0.1:8787

# Terminal 2 — route scode through the proxy auth mode to the mock
export PROXY_BASE_URL="http://127.0.0.1:8787"
export PROXY_AUTH_TOKEN="mock"
cargo run --bin scode -- --auth proxy "say hi"
```

**Workflow — run the scripted parity harness:**

This is the same harness the workspace uses for CI parity checks. Responses are deterministic and reproducible across runs, machines, and CI shards.

```bash
cd rust && ./scripts/run_mock_parity_harness.sh
```

> [!NOTE]
> The mock service returns scripted, fixture-backed responses. It is the right tool for protocol, transport, and UI verification — and the wrong tool for evaluating model quality. Point at a real provider for the latter.

## Authentication

`scode` supports three authentication modes. Use `--auth` to select one explicitly, or let auto-detection pick (`subscription` > `proxy` > `api-key`).

```bash
scode --auth api-key          # uses ANTHROPIC_API_KEY, OPENAI_API_KEY, etc.
scode --auth subscription     # uses CLAUDE_CODE_OAUTH_TOKEN
scode --auth proxy            # uses PROXY_AUTH_TOKEN + PROXY_BASE_URL
```

| Mode | Environment variables | Endpoint |
|------|----------------------|----------|
| `api-key` | `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `XAI_API_KEY`, `GEMINI_API_KEY`, `DASHSCOPE_API_KEY` | Provider default |
| `subscription` | `CLAUDE_CODE_OAUTH_TOKEN` (run `claude setup-token` to get one) | `api.anthropic.com` |
| `proxy` | `PROXY_AUTH_TOKEN` + `PROXY_BASE_URL` | `PROXY_BASE_URL` |

## Model Aliases

Short names resolve to the current pinned versions:

| Alias | Resolves to | Provider |
|-------|-------------|----------|
| `opus` | `claude-opus-4-6` | Anthropic |
| `sonnet` | `claude-sonnet-4-6` | Anthropic |
| `haiku` | `claude-haiku-4-5` | Anthropic |
| `grok` | `grok-3` | xAI |

```bash
scode --model opus
scode --model sonnet --auth subscription
```

## Slash Commands

The REPL surface is broad. Tab-complete from `/` to discover. A representative subset:

| Category | Commands |
|----------|----------|
| Session & visibility | `/help` · `/status` · `/sandbox` · `/cost` · `/resume` · `/session` · `/usage` · `/stats` · `/version` |
| Workspace & git | `/compact` · `/clear` · `/config` · `/memory` · `/init` · `/diff` · `/commit` · `/pr` · `/issue` · `/export` · `/files` · `/release-notes` |
| Discovery & debugging | `/mcp` · `/agents` · `/skills` · `/doctor` · `/tasks` · `/context` · `/desktop` · `/hooks` |
| Automation & analysis | `/review` · `/advisor` · `/insights` · `/security-review` · `/subagent` · `/telemetry` · `/providers` · `/cron` |
| Plugin management | `/plugin` (aliases: `/plugins`, `/marketplace`) |

For the canonical, live command list:

```bash
cargo run --bin scode -- --help
```

## Safety: Permissions & Sandbox

Coding agents touch your filesystem and your shell — `scode` treats that seriously.

**Permission modes** gate every tool call:

| Mode | Behavior |
|------|----------|
| `read-only` | All filesystem and shell mutations blocked. Read tools and web tools still work. |
| `workspace-write` | Writes restricted to the current workspace; ambient shell mutations blocked. |
| `prompt` | Each privileged tool call requires interactive approval. |
| `allow` | Pre-approved by the runner — used for non-interactive automation. |
| `danger-full-access` | No restrictions. The current default — explicit by design. |

Set via `--permission-mode <MODE>` or `permissionMode` in `.scode.json`.

**Linux sandbox** (when running on Linux):

- User-namespace isolation via `unshare` — no root required.
- Filesystem modes: `off`, `workspace-only`, `allow-list` (with an explicit mount list).
- Optional network isolation.
- Container-aware: detects Docker / Podman and reports back through `scode doctor` and `/sandbox`.

```bash
scode --permission-mode workspace-write
scode sandbox --status               # inspect current sandbox state
```

> [!WARNING]
> The default permission mode is `danger-full-access` because `scode` is designed to *do work*, not just answer questions. Tighten it before running against untrusted prompts or in shared environments.

## Diagnostics: `scode doctor`

One command, full picture. `scode doctor` reports:

- Auth mode resolution (which env vars are present, which mode would be selected)
- Provider reachability and credential validation
- MCP server status (configured, running, last error)
- Config file resolution (`.scode.json` hierarchy + merged result)
- Permission policy and sandbox mode
- Tool registry and skills inventory

Run it before filing an issue — most setup problems surface here first.

```bash
scode doctor
```

## Documentation

- [Usage Guide](./rust/USAGE.md) — commands, integration, local models
- [Rust Workspace](./rust/README.md) — crate architecture, mock parity harness, internals
- [Plugins](./docs/plugins.md) — authoring and using `scode` plugins
- [Model Compatibility](./docs/MODEL_COMPATIBILITY.md) — provider/model support matrix
- [Container build](./docs/container.md) — `Containerfile` usage

## Contributing

Issues and pull requests are welcome. Before opening a PR:

```bash
cd rust
scripts/fmt.sh                                   # or: cargo fmt
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Star the repo if `scode` is useful to you — it helps others find the project.

## Project

Sudo Code is maintained by the Sudo Privacy community as a standalone, ACP-native, model-agnostic agent engine.

## License

Sudo Code is released under the **MIT License**. See the per-crate license fields in [`rust/Cargo.toml`](./rust/Cargo.toml).

---

Sudo Code is a community-driven project. Not affiliated with or endorsed by Anthropic.
