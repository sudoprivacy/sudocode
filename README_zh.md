[🇨🇳 简体中文](./README_zh.md) | [🇬🇧 English](./README.md)

# Sudo Code

<p align="center">
  <img src="assets/scode-hero.jpeg" alt="Sudo Code" width="300" />
</p>

## 简介

**Sudo Code** (`scode`) 是一款使用 Rust 编写的 AI 编码代理引擎，启动快、运行稳，原生支持 **Agent Communication Protocol (ACP)**，并是 **Sudowork** 平台的核心引擎。

## 核心特性

- ⚡ **极速启动**：Rust 实现，资源占用低、响应延迟小。
- 🛰 **Headless 优先**：内建 ACP 服务端模式，便于 IDE 与编排系统集成。
- 🔌 **多提供方**：支持 Anthropic、OpenAI、xAI、DashScope、订阅 OAuth 与自定义代理，可通过参数自由切换。
- 🧰 **开箱即用**：丰富的 Slash 命令覆盖会话、插件、权限、Git 与代码审查等工作流。

## 快速开始

```bash
git clone https://github.com/sudoprivacy/sudocode.git
cd sudocode/rust && cargo build --release

export ANTHROPIC_API_KEY="sk-ant-..."     # 或使用订阅 / 代理
./target/release/scode                    # 进入交互式 REPL
./target/release/scode "解释这个代码库"     # 一次性提示
./target/release/scode doctor             # 健康检查
```

## 鉴权方式

- `--auth api-key`：使用 `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` 等环境变量。
- `--auth subscription`：使用 `CLAUDE_CODE_OAUTH_TOKEN`（可通过 `claude setup-token` 获取）。
- `--auth proxy`：使用 `PROXY_AUTH_TOKEN` 与 `PROXY_BASE_URL`。

未显式指定时按 `subscription` > `proxy` > `api-key` 自动选择。

## 协议与许可

项目以 **MIT License** 开源，欢迎社区贡献。本项目最初 fork 自 [`ultraworkers/claw-code`](https://github.com/ultraworkers/claw-code)（最后同步：2026-04-23），与 Anthropic 无关联、未获官方背书。

更多内容详见上方英文文档与 [`USAGE.md`](./USAGE.md)。
