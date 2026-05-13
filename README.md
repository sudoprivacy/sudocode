# Sudo Code (scode)

<p align="center">
  <img src="assets/scode-demo.gif" alt="Sudo Code demo" width="720" />
</p>

<p align="center">
  <strong>一个为终端、编辑器和自动化而生的快速、本地优先的 AI 编程 Agent 引擎。</strong>
</p>

<p align="center">
  <a href="https://github.com/sudoprivacy/sudocode/actions/workflows/rust-ci.yml">
    <img alt="Rust CI" src="https://github.com/sudoprivacy/sudocode/actions/workflows/rust-ci.yml/badge.svg" />
  </a>
  <a href="https://github.com/sudoprivacy/sudocode/releases">
    <img alt="Latest release" src="https://img.shields.io/github/v/release/sudoprivacy/sudocode?display_name=tag" />
  </a>
  <img alt="Rust 2021" src="https://img.shields.io/badge/Rust-2021-orange?logo=rust" />
  <img alt="Unsafe forbidden" src="https://img.shields.io/badge/unsafe-forbidden-success" />
  <img alt="Platforms" src="https://img.shields.io/badge/platform-macOS%20%7C%20Linux%20%7C%20Windows-blue" />
</p>

**Sudo Code** (`scode`) 是一个由 Rust 驱动的 AI 编程 Agent。它在本地运行，支持多个 LLM 供应商，专为交互式终端、无头自动化以及编辑器/编排系统集成而设计。

自带 API Key、订阅 Token 或代理即可使用。它既是一个开箱即用的强大 CLI 工具，也是一个可扩展的开放 Agent 运行时。

> Sudo Code 是 **Sudowork** 平台的开源引擎，但它完全可以作为独立工具使用，无需依赖平台。

## 为什么选择 Sudo Code？

- 🚀 **极速原生运行时** — Rust 编写，启动极快，原生工具执行，无 Node/Python 运行时负担。
- 🔌 **自带模型 (BYO Model)** — 支持 Anthropic, OpenAI, xAI/Grok, DashScope/Qwen 或自定义代理。
- 🔐 **本地优先** — 在你的机器上运行，使用你的凭据，处理你的本地代码，无中间层侵入。
- 🤖 **交互与自动化兼得** — 支持 REPL 交互、单次指令 (one-shot)、JSON 输出以及 ACP 协议。
- 🧩 **Agent 基础设施** — 内置文件/Shell/搜索工具、斜杠命令、MCP 接口、插件与 Skill 系统。
- 🛡️ **安全第一** — 项目代码**禁止使用 unsafe Rust**，并拥有精细的工具执行权限管理。
- 🛠️ **高度可定制** — 模块化 Rust 工作区，方便开发者扩展 provider、工具或编辑器插件。

---

## 快速安装

### 方案 A：下载预编译二进制文件 (推荐)

从 [Releases](https://github.com/sudoprivacy/sudocode/releases/latest) 页面下载对应系统的压缩包：

| 平台 | x64 | arm64 |
| --- | --- | --- |
| Linux | `scode-linux-x64.tar.gz` | `scode-linux-arm64.tar.gz` |
| macOS | `scode-macos-x64.tar.gz` | `scode-macos-arm64.tar.gz` |
| Windows | `scode-windows-x64.zip` | `scode-windows-arm64.zip` |

**macOS (Apple Silicon) 快速安装示例：**
```bash
curl -L -o scode.tar.gz https://github.com/sudoprivacy/sudocode/releases/latest/download/scode-macos-arm64.tar.gz
tar -xzf scode.tar.gz
sudo mv scode-macos-arm64/scode /usr/local/bin/scode
scode --version
```

### 方案 B：从源码构建
```bash
git clone https://github.com/sudoprivacy/sudocode.git
cd sudocode/rust
cargo build --release -p rusty-sudocode-cli --bin scode
./target/release/scode --version
```

---

## 5 分钟上手

设置你的 API Key：
```bash
export ANTHROPIC_API_KEY="sk-ant-..."
# 或者使用 OPENAI_API_KEY / XAI_API_KEY / DASHSCOPE_API_KEY
```

开始对话：
```bash
scode "用 5 句话总结这个仓库"
scode          # 进入交互式 REPL
scode doctor   # 检查环境配置
```

### 强大的斜杠命令
在 REPL 模式下，你可以直接控制 Agent 运行时：
- `/help` — 显示所有命令
- `/status` — 查看当前会话状态
- `/mcp` — 检查 MCP 服务器
- `/skills` — 管理已安装的技能
- `/commit` — 让 Agent 帮你写 Commit Message 并提交

---

## 社区与贡献

Sudo Code 是一个社区驱动的项目，我们非常欢迎各种形式的贡献：
- 修复模型兼容性问题
- 改进 CLI/REPL 的用户体验
- 贡献新的 Skill、插件或 MCP 集成
- 完善文档和测试

在提交 PR 前，请确保运行：
```bash
cd rust
cargo fmt --all --check
cargo clippy --workspace
cargo test --workspace
```

## 开源协议
本项目采用 **MIT 协议**。详见 [LICENSE](LICENSE)。
