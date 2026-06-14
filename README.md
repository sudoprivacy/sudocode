<!-- Language: 🇬🇧 English (this file) · [🇨🇳 简体中文](./README_zh.md) -->

# Sudo Code

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

<p align="center">
  <b>An engine for the AI agent era.</b><br/>
  Rust-native · model-agnostic · headless-first · safe by design.
</p>

<p align="center">
  <img src="assets/scode-demo.gif" alt="Sudo Code terminal demo highlighting Rust-native speed, model-agnostic providers, headless ACP infrastructure, and safe-by-design permissions" width="900" />
</p>

---

## What is Sudo Code?

**Sudo Code** (`scode`) is a high-performance, Rust-native coding agent —
in the same family as Claude Code or Aider — built for two audiences from
day one: humans at a terminal and machines on a wire.

- **Model-agnostic.** First-class support for Anthropic, OpenAI, xAI, and
  Gemini, with OAuth subscriptions and arbitrary proxy backends. See
  [`docs/models.md`](./docs/models.md) for aliases and provider details.
- **Performance-first runtime.** Single native binary on Rust + `tokio`
  with a lean memory footprint, deterministic shutdown, and predictable
  resource use under load.
- **Headless infrastructure.** `scode acp` and `scode acp serve` expose
  the Agent Communication Protocol over stdio and WebSocket. See
  [`docs/acp.md`](./docs/acp.md).
- **Embedded Web UI.** `scode acp serve --port 8080` ships an
  interactive web client at `http://localhost:8080/`. See
  [`docs/acp.md`](./docs/acp.md).
- **Safe by design.** Explicit permission modes and a Linux
  user-namespace sandbox. See
  [`docs/permissions-and-sandbox.md`](./docs/permissions-and-sandbox.md).

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

The Cargo workspace is described in
[`rust/README.md`](./rust/README.md).

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/sudoprivacy/sudocode/main/install.sh | sh
```

`install.sh` downloads the prebuilt `scode` binary for the host platform
(macOS arm64/x64, Linux x64/arm64) and verifies a SHA-256 checksum. On
macOS Apple Silicon the script installs to `/opt/homebrew/bin`; on macOS
x64 and Linux it installs to `/usr/local/bin`, prompting for `sudo` only
when stdin is a TTY. When the preferred system directory is unwritable
and `sudo` is unavailable, the script installs to `$HOME/.local/bin`.
Windows users grab the zip from the
[Releases page](https://github.com/sudoprivacy/sudocode/releases/latest).

Overrides:

- `SCODE_VERSION=v0.1.5 sh install.sh` — pin a specific release.
- `sh install.sh --no-sudo` — install to `$HOME/.local/bin`.
- `SCODE_INSTALL_DIR=$HOME/.local/bin sh install.sh` — explicit per-user
  install.
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

# One-shot prompt
scode "explain this codebase"

# Health check
scode doctor
```

For day-to-day workflows see [`docs/usage.md`](./docs/usage.md).

## Documentation

- [`docs/usage.md`](./docs/usage.md) — REPL, one-shot, JSON output, resume, doctor.
- [`docs/authentication.md`](./docs/authentication.md) — auth modes and credentials.
- [`docs/permissions-and-sandbox.md`](./docs/permissions-and-sandbox.md) — permission modes, Linux sandbox.
- [`docs/acp.md`](./docs/acp.md) — ACP transports and the embedded Web UI.
- [`docs/models.md`](./docs/models.md) — aliases, provider-specific handling.
- [`docs/plugins.md`](./docs/plugins.md) — authoring and using `scode` plugins.
- [`docs/container.md`](./docs/container.md) — building and running inside a container.
- [`docs/parity.md`](./docs/parity.md) — what claude-code parity means and how it is tracked.
- [`docs/mock-parity-harness.md`](./docs/mock-parity-harness.md) — the deterministic mock backend and harness.
- [`ROADMAP.html`](./ROADMAP.html) — goals for the project.
- [`rust/README.md`](./rust/README.md) — Cargo workspace map.

## Contributing

See [`CONTRIBUTING.md`](./CONTRIBUTING.md) for the developer setup, the
required checks, and the PR workflow.

## License

Released under the MIT License. See the per-crate license fields in
[`rust/Cargo.toml`](./rust/Cargo.toml).

---

Sudo Code is a community-driven project, maintained by the Sudo Privacy
community.
