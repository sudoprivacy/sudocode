<!-- Language: English (this file) · [🇨🇳 简体中文](./plugins_zh.md) -->

# SudoCode plugins

A SudoCode plugin is a local directory containing a
`.sudocode-plugin/plugin.json` manifest. Once installed, the manifest's
declared MCP servers, skills, and hooks are projected into the `scode`
runtime.

This document covers:

- Using plugins (§1)
- Authoring a plugin (§2)
- Distribution (§3)
- Compatibility with Claude Code plugins (§4)
- Security model (§5)
- Where to look in the code (§6)

The reference implementation lives in
[`rust/crates/plugins/`](../rust/crates/plugins/); two minimal worked
examples ship under
[`rust/crates/plugins/bundled/`](../rust/crates/plugins/bundled/).

---

## 1. Using plugins

### 1.1 Command summary

| Command | Effect |
|---|---|
| `scode plugins` | List installed plugins |
| `scode plugins install <path>` | Install from a local directory (alias: `add`) |
| `scode plugins enable <name-or-id>` | Enable (install enables by default) |
| `scode plugins disable <name-or-id>` | Disable, keep files on disk |
| `scode plugins remove <name-or-id>` | Uninstall and delete the install directory (alias: `uninstall`) |
| `scode plugins update <name-or-id>` | Re-copy the original source path into the install directory |
| `scode plugins marketplace` | List entries from `.nexus/sudocode/plugins/marketplace.json` (display only — see §3) |
| `scode plugins available` | Alias of `marketplace` |
| `scode mcp` | List configured MCP servers, including plugin-provided ones |
| `scode mcp show <server>` | Detailed view, including the owning plugin |
| `scode skills` | List skills; plugin-provided ones appear under `SudoCode plugin roots:` |
| `scode system-prompt` | Render the system prompt; includes the plugin capability summary block |

Both `scode plugins` and `scode mcp` accept `--output-format json` and
emit structured payloads (`plugins[]`, `servers[]`) for scripting.

### 1.2 Installing a third-party plugin

```bash
git clone https://github.com/some-author/cool-plugin /tmp/cool-plugin
scode plugins install /tmp/cool-plugin

scode plugins        # cool-plugin appears here
scode mcp            # if it ships MCP servers
scode skills         # if it ships skills
```

### 1.3 settings.json shape

After installing a plugin, scode writes the enabled state into the
**structured form**:

```json
{
  "plugins": {
    "enabled": {
      "cool-plugin@external": { "enabled": true }
    }
  }
}
```

The legacy form remains readable and will be preserved if your
settings already use it (no surprise migration):

```json
{
  "enabledPlugins": {
    "cool-plugin@external": true
  }
}
```

---

## 2. Authoring a plugin

### 2.1 Minimum viable plugin

```
my-plugin/
└── .sudocode-plugin/
    └── plugin.json
```

The manifest only needs three required fields:

```json
{
  "name": "my-plugin",
  "version": "0.1.0",
  "description": "A short sentence about what the plugin does"
}
```

### 2.2 Full manifest schema

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

| Field | Type | Notes |
|---|---|---|
| `name` | string | **Required.** Package name. Plugin id is `<name>@<source>`. |
| `version` | string | **Required.** Semver recommended. |
| `description` | string | **Required.** One sentence. |
| `interface.display_name` | string | Shown in `scode plugins`. **Not** injected into the system prompt (prompt-injection defense — see §5.2). |
| `interface.short_description` | string | Same scope as `display_name`. |
| `interface.keywords` | string[] | Free-form tags. |
| `skills` | string | Path (relative to plugin root) to a directory of skill folders. |
| `mcpServers` | string | Path to a `.mcp.json` file. |
| `hooks.PreToolUse` | string[] | Script paths run **before** any tool call (in order). |
| `hooks.PostToolUse` | string[] | Run **after** a successful tool call. |
| `hooks.PostToolUseFailure` | string[] | Run **after** a failed tool call. |
| `default_enabled` | bool | Default `true`. Whether install enables the plugin automatically. |

### 2.3 Manifest discovery paths

scode looks for the manifest in this order (highest priority first):

1. `.sudocode-plugin/plugin.json` — official path
2. `plugin.json` at root
3. `.claude-plugin/plugin.json` — Claude Code compatibility
4. `.codex-plugin/plugin.json` — Codex compatibility

Only the highest-priority match is loaded.

### 2.4 Skills

Each subdirectory under the path pointed to by `skills` is one skill:

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

`SKILL.md` uses YAML frontmatter:

```markdown
---
name: hello
description: One-line summary of what this skill does
---

# hello

The body becomes the prompt content when the model invokes
`/skills hello`.
```

**Precedence.** Plugin skills rank **below** project-local skills
(`.nexus/sudocode/skills/`) and user skills. If a plugin skill shadows
one of those, `scode skills` will tag it as
`(shadowed by Project roots)`.

### 2.5 MCP servers

The path under `mcpServers` points to a `.mcp.json`:

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

Important rules:

- **Stdio transport only.** HTTP / SSE / WebSocket MCP servers are
  not supported.
- **Relative commands resolve to the plugin install root.** `./bin/...`
  and `../...` are rewritten; `npx`, `uvx`, and absolute paths are
  passed through unchanged.
- **`current_dir` is set to the plugin install root.** Servers can use
  relative paths to bundled assets.
- **User and global MCP servers always win on name collisions.** The
  plugin's server is silently ignored when its name is already taken.
- Multiple plugins providing the same server name resolve first-come,
  first-served (insertion order).

Tools are exposed to the model as `<server>_<tool>`, e.g.
`github_list_issues`, `files_read`.

`scode mcp` annotates plugin-provided servers with
`[SudoCode plugin <plugin-id>]`. In JSON output each server carries a
`plugin_source` field.

### 2.6 Hooks

Hook entries are executable scripts (or commands). Scripts must be
`chmod +x`.

Supported events:

- `PreToolUse` — runs before every tool invocation (including MCP
  tools)
- `PostToolUse` — runs after a successful tool invocation
- `PostToolUseFailure` — runs after a failed tool invocation

The hook receives a JSON payload on **stdin**:

```json
{
  "tool_name": "Bash",
  "tool_input": "{\"command\":\"pwd\"}",
  "tool_output": null,
  "is_error": false,
  "session_id": "..."
}
```

**Exit codes** drive behaviour:

| Exit | Effect |
|---|---|
| `0` | Allow. stdout is appended to the tool result (so it can shape later LLM reasoning). |
| `2` | **Deny.** Block the tool call; stderr becomes the denial reason returned to the model. |
| other | Treat as hook failure. |

**Provenance is visible in two channels.** The CLI prints lines like

```
[hook PreToolUse]      Bash: /.../my-plugin/hooks/pre.sh (SudoCode plugin my-plugin@external)
[hook DENIED PreToolUse] Bash: /.../my-plugin/hooks/pre.sh (SudoCode plugin my-plugin@external)
```

and the tool-result returned to the model includes
`SudoCode plugin <id>` in fallback messages.

**Path safety.** `scode` canonicalises the manifest-declared hook path
and rejects any entry that resolves outside the plugin root.

The bundled
[`rust/crates/plugins/bundled/example-bundled/`](../rust/crates/plugins/bundled/example-bundled/)
and
[`sample-hooks/`](../rust/crates/plugins/bundled/sample-hooks/)
plugins are minimal hook-only examples worth copying from.

---

## 3. Distribution

### 3.1 Git + local install

The supported distribution model is **git + local install**:

```
Author:  pushes a directory with .sudocode-plugin/plugin.json to git
User:    git clone <url> /tmp/foo && scode plugins install /tmp/foo
```

The README of a plugin repo typically pins those two commands.

### 3.2 marketplace.json (read-only discovery)

When a directory has `.nexus/sudocode/plugins/marketplace.json`, the
file is rendered by `scode plugins marketplace`:

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

The `source` field is descriptive. Discovery is a listing surface; the
user clones the repo and runs `scode plugins install` to install.

The legacy path `.agents/plugins/marketplace.json` is read as a
fallback when the primary path is missing.

---

## 4. Compatibility with Claude Code plugins

| Concept | Behaviour in scode |
|---|---|
| `.claude-plugin/plugin.json` | Read as a fallback manifest path |
| `hooks.PreToolUse` / `PostToolUse` | Supported |
| `hooks.PostToolUseFailure` | Supported (scode-specific extension) |
| `hooks.SessionStart`, `UserPromptSubmit`, `Stop`, `PreCompact`, … | **Not supported.** Manifest is rejected with a clear migration message. |
| `agents` field | Rejected with guidance. |
| `commands` field as a directory glob | Rejected with guidance. |

The simplest migration: keep the existing `.claude-plugin/plugin.json`
for compatibility with other tools, **and** add a
`.sudocode-plugin/plugin.json` for scode-specific behaviour. scode
picks the latter when both are present.

---

## 5. Security model

Read this section before installing a third-party plugin.

### 5.1 Execution context

A plugin's hook scripts and MCP server processes run as the current
user, with the current user's filesystem and network access.

> Installing a stranger's plugin is equivalent to running their code
> on your machine. Inspect the manifest and hook scripts before
> `scode plugins install`.

### 5.2 Manifest metadata stays out of the system prompt

To defend against prompt-injection authored into manifest fields, the
plugin capability section in the system prompt lists plugins
anonymously:

```
# Available SudoCode plugins
…
 - Plugin 1; provides 2 tools, 1 hook, MCP servers
```

Plugin `name`, `display_name`, and `description` surface only in the
CLI (`scode plugins`, `scode mcp`); the model-facing system prompt sees
only the anonymous capability summary.

> MCP tool names like `everything_add` are visible to the model — those
> are contracts published by the MCP server itself, separate from the
> manifest. Tool descriptions are the server's responsibility.

### 5.3 Hook script paths are constrained to the plugin root

scode `canonicalize`s every manifest-declared hook path and rejects
anything that resolves outside the plugin install directory. A plugin
cannot smuggle a hook that points at `/usr/bin/curl` or `../../etc/passwd`.

### 5.4 MCP server spawn is capped

A misconfigured plugin MCP server that exits immediately after spawn is
re-tried at most twice per manager lifetime, then disabled with a
sticky `PermanentlyFailed` state. This prevents fork-bombs from broken
plugins.

---

## 6. Where to look in the code

| Concern | Crate / file |
|---|---|
| Manifest parsing, install / enable / disable, marketplace | [`rust/crates/plugins/src/lib.rs`](../rust/crates/plugins/src/lib.rs) |
| Hook execution + progress events | [`rust/crates/runtime/src/hooks.rs`](../rust/crates/runtime/src/hooks.rs), [`rust/crates/plugins/src/hooks.rs`](../rust/crates/plugins/src/hooks.rs) |
| MCP projection + lifecycle | [`rust/crates/runtime/src/mcp_stdio.rs`](../rust/crates/runtime/src/mcp_stdio.rs), [`rust/crates/rusty-sudocode-cli/src/cli/mcp.rs`](../rust/crates/rusty-sudocode-cli/src/cli/mcp.rs) |
| Slash command surface (`/plugins`, `/mcp`, `/skills`, `/marketplace`) | [`rust/crates/commands/src/lib.rs`](../rust/crates/commands/src/lib.rs) |
| CLI wiring (`scode plugins …`) | [`rust/crates/rusty-sudocode-cli/src/main.rs`](../rust/crates/rusty-sudocode-cli/src/main.rs), [`rust/crates/rusty-sudocode-cli/src/cli/args.rs`](../rust/crates/rusty-sudocode-cli/src/cli/args.rs) |
| Bundled example plugins | [`rust/crates/plugins/bundled/`](../rust/crates/plugins/bundled/) |

---

See also: [`../README.md`](../README.md), [`../rust/README.md`](../rust/README.md),
[`../CONTRIBUTING.md`](../CONTRIBUTING.md), [`./README.md`](./README.md),
[简体中文版](./plugins_zh.md).
