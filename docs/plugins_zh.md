<!-- Language: [🇬🇧 English](./plugins.md) · 🇨🇳 简体中文 (this file) -->

# SudoCode 插件

一个 SudoCode 插件是一个本地目录，包含一份
`.sudocode-plugin/plugin.json` 清单。安装后，清单中声明的 MCP
servers / skills / hooks 会被投影到 `scode` runtime。

本文涵盖：

- 用户视角（§1）
- 作者视角（§2）
- 分发（§3）
- 与 Claude Code 插件的兼容性（§4）
- 安全模型（§5）
- 代码索引（§6）

参考实现位于
[`rust/crates/plugins/`](../rust/crates/plugins/)，
[`rust/crates/plugins/bundled/`](../rust/crates/plugins/bundled/) 下有两个最小例子。

---

## 1. 用户视角：怎么装、怎么用

### 1.1 命令总览

| 命令 | 作用 |
|---|---|
| `scode plugins` | 列出已安装插件 |
| `scode plugins install <path>` | 从本地路径安装（别名：`add`）|
| `scode plugins enable <name-or-id>` | 启用（安装默认即启用）|
| `scode plugins disable <name-or-id>` | 停用，保留磁盘文件 |
| `scode plugins remove <name-or-id>` | 卸载并删除安装目录（别名：`uninstall`）|
| `scode plugins update <name-or-id>` | 从原 source 路径重新拷贝到安装目录 |
| `scode plugins marketplace` | 列出 `.nexus/sudocode/plugins/marketplace.json` 里的条目（**只读展示**，见 §3）|
| `scode plugins available` | `marketplace` 的别名 |
| `scode mcp` | 列出已配置的 MCP servers，**含插件投影来的** |
| `scode mcp show <server>` | 单个 MCP server 详情，含归属哪个插件 |
| `scode skills` | 列出 skills，插件提供的归在「SudoCode plugin roots:」段 |
| `scode system-prompt` | 查看注入给模型的 system prompt，含插件能力摘要 |

`scode plugins` 和 `scode mcp` 都支持 `--output-format json`，返回结构化的
`plugins[]` / `servers[]` 数组，可供脚本/CI 消费。

### 1.2 装一个第三方插件

```bash
git clone https://github.com/some-author/cool-plugin /tmp/cool-plugin
scode plugins install /tmp/cool-plugin

scode plugins        # 列表里出现 cool-plugin
scode mcp            # 如果它带了 MCP server
scode skills         # 如果它带了 skill
```

### 1.3 settings.json 形态

装完一个插件后，scode 把启用状态写成**结构化**形式：

```json
{
  "plugins": {
    "enabled": {
      "cool-plugin@external": { "enabled": true }
    }
  }
}
```

旧格式仍然可读，**已经是旧格式的不会被强制迁移**：

```json
{
  "enabledPlugins": {
    "cool-plugin@external": true
  }
}
```

---

## 2. 作者视角：怎么写一个插件

### 2.1 最小可用插件

```
my-plugin/
└── .sudocode-plugin/
    └── plugin.json
```

清单只需要三个必填字段：

```json
{
  "name": "my-plugin",
  "version": "0.1.0",
  "description": "A short sentence about what the plugin does"
}
```

### 2.2 完整 manifest schema

```json
{
  "name": "github-tools",
  "version": "1.0.0",
  "description": "GitHub workflow helpers",

  "interface": {
    "display_name": "GitHub Tools",
    "short_description": "Plan / PR / issue helpers for GitHub repos",
    "keywords": ["github", "pr", "issues"]
  },

  "skills": "./skills",
  "mcpServers": "./.mcp.json",
  "hooks": {
    "PreToolUse":         ["./hooks/pre.sh"],
    "PostToolUse":        ["./hooks/post.sh"],
    "PostToolUseFailure": ["./hooks/fail.sh"]
  },

  "default_enabled": true
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `name` | string | **必填**。包名，参与生成 plugin id `<name>@<source>` |
| `version` | string | **必填**。建议语义化版本 |
| `description` | string | **必填**。一句话描述 |
| `interface.display_name` | string | CLI 列表里显示。**不进入** system prompt（防注入，见 §5.2）|
| `interface.short_description` | string | 同 display_name 的作用域 |
| `interface.keywords` | string[] | 自由 tag |
| `skills` | string | 相对路径，指向放 skill 目录的文件夹 |
| `mcpServers` | string | 相对路径，指向一个 `.mcp.json` |
| `hooks.PreToolUse` | string[] | 工具调用**前**执行的脚本路径，按顺序运行 |
| `hooks.PostToolUse` | string[] | 工具**成功**后执行 |
| `hooks.PostToolUseFailure` | string[] | 工具**失败**后执行 |
| `default_enabled` | bool | 默认 `true`。安装后是否默认启用 |

### 2.3 清单路径优先级

scode 按以下顺序查找 manifest（高优先级在前）：

1. `.sudocode-plugin/plugin.json` —— 官方推荐
2. 根目录的 `plugin.json`
3. `.claude-plugin/plugin.json` —— Claude Code 兼容
4. `.codex-plugin/plugin.json` —— Codex 兼容

只读取最高优先级那个。

### 2.4 Skills

`skills` 指向的目录下，每个子目录就是一个 skill：

```
my-plugin/
└── skills/
    ├── hello/
    │   └── SKILL.md
    └── plan/
        ├── SKILL.md
        └── helpers/
            └── template.md
```

`SKILL.md` 用 YAML frontmatter：

```markdown
---
name: hello
description: One-line summary of what this skill does
---

# hello

模型 `/skills hello` 时读这个文件作为 prompt 内容。
```

**优先级**：插件 skill 的优先级**低于**项目本地 skill
（`.nexus/sudocode/skills/`）和用户 skill。同名时插件的会被标记为
`(shadowed by Project roots)`。

### 2.5 MCP servers

`mcpServers` 指向的 `.mcp.json`：

```json
{
  "mcpServers": {
    "github": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-github"],
      "env": { "GITHUB_TOKEN": "ghp_xxx" }
    },
    "files": {
      "command": "./bin/file-server.py",
      "args": []
    }
  }
}
```

关键规则：

- **只支持 stdio transport**。HTTP / SSE / WebSocket MCP server 不能用。
- **相对命令解析为插件安装根目录**。`./bin/...`、`../...` 会被重写；
  `npx`、`uvx`、绝对路径原样保留。
- **`current_dir` 设为插件安装根目录**。server 可以放心用相对路径访问自带的文件。
- **同名冲突时用户/全局的 MCP server 永远赢**。插件那个会被静默忽略。
- 多个插件提供同名 server 时，按 plugin 装载顺序先到先得。

工具暴露给模型的名字是 `<server>_<tool>` 格式，如 `github_list_issues`、
`files_read`。

`scode mcp` 列表里会给插件提供的 server 加上
`[SudoCode plugin <plugin-id>]` 标签。JSON 输出里每个 server 带
`plugin_source` 字段。

### 2.6 Hooks

Hook 入口是可执行脚本（或命令），脚本必须 `chmod +x`。

支持的事件：

- `PreToolUse` —— 任何工具被调用**前**触发（包括 MCP 工具）
- `PostToolUse` —— 工具**成功**后
- `PostToolUseFailure` —— 工具**失败**后

scode 把 tool 调用上下文以 JSON 形式从 **stdin** 喂给 hook：

```json
{
  "tool_name": "Bash",
  "tool_input": "{\"command\":\"pwd\"}",
  "tool_output": null,
  "is_error": false,
  "session_id": "..."
}
```

**退出码决定下一步**：

| Exit | 效果 |
|---|---|
| `0` | 放行。stdout 追加进 tool result（可影响后续 LLM 推理）|
| `2` | **拒绝**。阻止 tool 执行，stderr 内容作为拒绝原因传给模型 |
| 其他 | 作为 hook 失败处理 |

**两条通道都能看到归属**。终端会打印：

```
[hook PreToolUse]      Bash: /.../my-plugin/hooks/pre.sh (SudoCode plugin my-plugin@external)
[hook DENIED PreToolUse] Bash: /.../my-plugin/hooks/pre.sh (SudoCode plugin my-plugin@external)
```

发回给模型的 tool_result 错误消息里也含 `SudoCode plugin <id>` 归属。

**路径安全**：scode 对 manifest 声明的 hook 路径做 `canonicalize` 校验，
解析后必须在插件安装目录内部，否则拒绝加载。

仓库里
[`rust/crates/plugins/bundled/example-bundled/`](../rust/crates/plugins/bundled/example-bundled/)
和
[`sample-hooks/`](../rust/crates/plugins/bundled/sample-hooks/)
都是最小 hook 例子，可以照着抄。

---

## 3. 现状的分发方式

### 3.1 当前唯一可行路径

在远端安装能力（§7）落地之前，唯一支持的分发模式是
**「git 仓库 + 本地 install」**：

```
作者:    push 一个含 .sudocode-plugin/plugin.json 的目录到 git
用户:    git clone <url> /tmp/foo && scode plugins install /tmp/foo
```

插件 README 里通常贴这两行命令。

### 3.2 marketplace.json（只读发现）

当某个目录有 `.nexus/sudocode/plugins/marketplace.json` 时，
`scode plugins marketplace` 会把它渲染出来：

```json
{
  "plugins": [
    {
      "name": "github-tools",
      "version": "1.0.0",
      "description": "GitHub workflow helpers",
      "source": "git+https://github.com/some-author/github-tools.git",
      "tags": ["github"]
    }
  ]
}
```

`scode` **不会**根据 `source` 字段自动下载安装 —— 这只是个**展示**，
用户仍然要自己 `git clone` 再 `scode plugins install`。

遗留路径 `.agents/plugins/marketplace.json` 作为回退也会被读取。

---

## 4. 与 Claude Code 插件的兼容性

| 概念 | scode 里的行为 |
|---|---|
| `.claude-plugin/plugin.json` | 作为回退路径读取 |
| `hooks.PreToolUse` / `PostToolUse` | 支持 |
| `hooks.PostToolUseFailure` | 支持（scode 扩展）|
| `hooks.SessionStart`、`UserPromptSubmit`、`Stop`、`PreCompact` … | **不支持**。manifest 中出现这些字段会被显式拒绝并给出迁移提示 |
| `agents` 字段 | 拒绝并提示 |
| `commands` 字段（目录 glob 形式）| 拒绝并提示 |

最简单的迁移方式：保留 `.claude-plugin/plugin.json` 兼容其他工具，
**并** 在仓库里加一份 `.sudocode-plugin/plugin.json` 用于 scode 特定行为。
两者并存时 scode 优先用后者。

---

## 5. 安全注意

跑陌生人的插件前，这一节是最重要的。

### 5.1 没有沙箱

第三方插件的：

- **hook 脚本**以你的用户身份执行
- **MCP server 进程**以你的用户身份执行

都能读你的 SSH key、写主目录、调外部网络。scode **不会**在安装前
弹「这个插件会执行任意脚本」确认对话。

> 装陌生人的插件等同在你机器上跑陌生人的代码。`scode plugins install`
> 之前，检查 manifest 和 hook 脚本。

### 5.2 Manifest 元数据不进 system prompt

为了防止恶意作者通过 manifest 字段做 prompt injection，system prompt
里的插件能力摘要段是**匿名化**的：

```
# Available SudoCode plugins
…
 - Plugin 1; provides 2 tools, 1 hook, MCP servers
```

`name`、`display_name`、`description` 故意不出现在模型可见通道里。
CLI 里能看到（`scode plugins`、`scode mcp`），但模型从 system prompt 看不到。

> 模型**仍然能看到** `everything_add` 这种 MCP 工具名 —— 那是 MCP server
> 自己签的契约，不归 manifest 管。工具描述由 server 负责。

### 5.3 Hook 脚本路径强制在插件根内

scode 对 manifest 里每条 hook 路径做 `canonicalize`，任何解析后跳出插件
安装目录的会被拒绝。插件没法塞一个指向 `/usr/bin/curl` 或 `../../etc/passwd` 的 hook。

### 5.4 MCP server spawn 有 cap

写错的 MCP server（如启动就退出）最多被重试 spawn 2 次，之后设
sticky `PermanentlyFailed` 状态，不再尝试。避免坏插件 fork-bomb。

---

## 6. 代码索引

| 关注点 | crate / 文件 |
|---|---|
| 清单解析、install/enable/disable、marketplace | [`rust/crates/plugins/src/lib.rs`](../rust/crates/plugins/src/lib.rs) |
| Hook 执行 + 进度事件 | [`rust/crates/runtime/src/hooks.rs`](../rust/crates/runtime/src/hooks.rs)、[`rust/crates/plugins/src/hooks.rs`](../rust/crates/plugins/src/hooks.rs) |
| MCP 投影 + 生命周期 | [`rust/crates/runtime/src/mcp_stdio.rs`](../rust/crates/runtime/src/mcp_stdio.rs)、[`rust/crates/rusty-sudocode-cli/src/cli/mcp.rs`](../rust/crates/rusty-sudocode-cli/src/cli/mcp.rs) |
| Slash 命令接口（`/plugins`、`/mcp`、`/skills`、`/marketplace`）| [`rust/crates/commands/src/lib.rs`](../rust/crates/commands/src/lib.rs) |
| CLI 接线（`scode plugins …`）| [`rust/crates/rusty-sudocode-cli/src/main.rs`](../rust/crates/rusty-sudocode-cli/src/main.rs)、[`rust/crates/rusty-sudocode-cli/src/cli/args.rs`](../rust/crates/rusty-sudocode-cli/src/cli/args.rs) |
| 仓库自带的示例插件 | [`rust/crates/plugins/bundled/`](../rust/crates/plugins/bundled/) |

---

另见：[`../README.md`](../README.md)、[`../rust/README.md`](../rust/README.md)、
[`../CONTRIBUTING.md`](../CONTRIBUTING.md)、[`./README.md`](./README.md)、
[English version](./plugins.md)。
