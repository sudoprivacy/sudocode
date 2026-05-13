# Sudo Code

<p align="center">
  <img src="assets/scode-hero.jpeg" alt="Sudo Code" width="300" />
</p>

<p align="center">
  <a href="#license"><img alt="License: MIT" src="https://img.shields.io/badge/License-MIT-blue.svg"></a>
  <img alt="Rust 2021" src="https://img.shields.io/badge/rust-2021-orange?logo=rust">
  <img alt="Version" src="https://img.shields.io/badge/version-0.1.6-brightgreen">
  <img alt="Platform" src="https://img.shields.io/badge/platform-macOS%20%7C%20Linux-lightgrey">
  <img alt="Protocol" src="https://img.shields.io/badge/protocol-ACP-purple">
  <a href="#contributing"><img alt="PRs Welcome" src="https://img.shields.io/badge/PRs-welcome-success.svg"></a>
</p>

<p align="center">
  <b>A fast, headless AI coding agent engine written in Rust.</b><br/>
  Multi-provider auth · Agent Communication Protocol (ACP) · Native tool execution.
</p>

---

## Why Sudo Code

`scode` is the open-source coding agent engine that powers the **Sudowork** platform. It is designed for developers who want a transparent, scriptable, and provider-agnostic agent that runs anywhere — from a terminal REPL to a headless server speaking ACP.

- **Fast boot, lean runtime.** Built in Rust for low-latency startup and predictable resource use.
- **Headless-first.** First-class ACP server mode for IDE integrations and orchestration backends.
- **Multi-provider.** Anthropic, OpenAI, xAI, DashScope, OAuth subscriptions, and custom proxies — switch with a flag.
- **Batteries included.** Rich slash-command surface for sessions, plugins, permissions, git, and review workflows.
- **Open lineage.** Forked from [`ultraworkers/claw-code`](https://github.com/ultraworkers/claw-code) (last synced: 2026-04-23) and evolved into a standalone ACP-native engine.

## Installation

### Build from source

```bash
git clone https://github.com/sudoprivacy/sudocode.git
cd sudocode/rust
cargo build --release

# Binary is at ./target/release/scode
```

Requires a recent stable Rust toolchain (2021 edition).

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

## Authentication

`scode` supports three authentication modes. Use `--auth` to select one explicitly, or let auto-detection pick (`subscription` > `proxy` > `api-key`).

```bash
scode --auth api-key          # uses ANTHROPIC_API_KEY, OPENAI_API_KEY, etc.
scode --auth subscription     # uses CLAUDE_CODE_OAUTH_TOKEN
scode --auth proxy            # uses PROXY_AUTH_TOKEN + PROXY_BASE_URL
```

| Mode | Environment variables | Endpoint |
|------|----------------------|----------|
| `api-key` | `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `XAI_API_KEY`, `DASHSCOPE_API_KEY` | Provider default |
| `subscription` | `CLAUDE_CODE_OAUTH_TOKEN` (obtain via `claude setup-token`) | `api.anthropic.com` |
| `proxy` | `PROXY_AUTH_TOKEN`, `PROXY_BASE_URL` | `PROXY_BASE_URL` |

## Model Aliases

Short aliases resolve to canonical model IDs:

```bash
scode --model opus      # claude-opus-4-6
scode --model sonnet    # claude-sonnet-4-6
scode --model haiku     # claude-haiku-4-5
scode --model grok      # grok-3 (xAI)
```

## Slash Commands

Inside the REPL, type `/` to access built-in commands. Selected highlights:

| Command | Purpose |
|---------|---------|
| `/help` | List all available commands |
| `/model` | Switch the active model mid-session |
| `/session`, `/resume`, `/history` | Manage and replay prior sessions |
| `/memory` | Inspect or edit persistent memory |
| `/permissions` | Review and adjust tool permissions |
| `/plugins` | Manage plugin lifecycle |
| `/commit`, `/pr`, `/diff`, `/status` | Git-aware workflow helpers |
| `/bughunter`, `/ultraplan`, `/teleport` | Higher-order agent workflows |
| `/cost`, `/stats` | Inspect token usage and runtime telemetry |
| `/doctor` | Diagnose configuration and credentials |
| `/compact`, `/clear`, `/fast`, `/brief` | Tune output verbosity and context |
| `/login`, `/logout`, `/upgrade` | Account and version management |

Run `scode` and type `/help` for the complete, version-accurate list.

## Documentation

- [Usage Guide](./USAGE.md) — commands, integration, local models
- [Rust Workspace](./rust/README.md) — crate architecture and internals
- [Model Compatibility](./docs/MODEL_COMPATIBILITY.md) — provider and model matrix
- [Container Guide](./docs/container.md) — running `scode` in containers

## Contributing

Contributions are warmly welcomed — bug reports, feature proposals, and pull requests alike.

```bash
# From the repo root
scripts/fmt.sh                                       # format
cd rust && cargo clippy --workspace --all-targets -- -D warnings
cd rust && cargo test --workspace
```

Please keep changes small and reviewable, and ensure `cargo fmt`, `clippy`, and the test suite all pass before opening a PR.

## License

Released under the [MIT License](./LICENSE).

## Acknowledgments

Sudo Code stands on the shoulders of the broader agent ecosystem and was originally forked from [`ultraworkers/claw-code`](https://github.com/ultraworkers/claw-code). It is a community-driven project and is **not affiliated with or endorsed by Anthropic**.

---

<details>
<summary><b>中文简介 (README_ZH)</b></summary>

### Sudo Code 简介

**Sudo Code** (`scode`) 是一款使用 Rust 编写的 AI 编码代理引擎，启动快、运行稳，原生支持 **Agent Communication Protocol (ACP)**，并是 **Sudowork** 平台的核心引擎。

### 核心特性

- ⚡ **极速启动**：Rust 实现，资源占用低、响应延迟小。
- 🛰 **Headless 优先**：内建 ACP 服务端模式，便于 IDE 与编排系统集成。
- 🔌 **多提供方**：支持 Anthropic、OpenAI、xAI、DashScope、订阅 OAuth 与自定义代理，可通过参数自由切换。
- 🧰 **开箱即用**：丰富的 Slash 命令覆盖会话、插件、权限、Git 与代码审查等工作流。

### 快速开始

```bash
git clone https://github.com/sudoprivacy/sudocode.git
cd sudocode/rust && cargo build --release

export ANTHROPIC_API_KEY="sk-ant-..."     # 或使用订阅 / 代理
./target/release/scode                    # 进入交互式 REPL
./target/release/scode "解释这个代码库"     # 一次性提示
./target/release/scode doctor             # 健康检查
```

### 鉴权方式

- `--auth api-key`：使用 `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` 等环境变量。
- `--auth subscription`：使用 `CLAUDE_CODE_OAUTH_TOKEN`（可通过 `claude setup-token` 获取）。
- `--auth proxy`：使用 `PROXY_AUTH_TOKEN` 与 `PROXY_BASE_URL`。

未显式指定时按 `subscription` > `proxy` > `api-key` 自动选择。

### 协议与许可

项目以 **MIT License** 开源，欢迎社区贡献。本项目最初 fork 自 [`ultraworkers/claw-code`](https://github.com/ultraworkers/claw-code)（最后同步：2026-04-23），与 Anthropic 无关联、未获官方背书。

更多内容详见上方英文文档与 [`USAGE.md`](./USAGE.md)。

</details>
