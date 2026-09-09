# Sudo Code v0.2.0

A 0.x minor bump: this release changes model-facing behavior (system prompt, tool surface, tool output limits) and on-screen rendering, and removes tools — not just fixes. Pinned 0.1.x installs are unaffected until you upgrade.

## What's New

### Features
- **Nightly channel + RC promotion path** (#518) — a rolling `nightly` prerelease builds from `main` every day at midnight CST across the full platform matrix, skipping days where `main` didn't move. RCs promote by tagging the soaked nightly's commit (`vX.Y.Z-rc.N`, auto-published as prerelease), stable tags the same commit. See CONTRIBUTING § Release channels.
- **`scode update`** (#514) — self-update from GitHub Releases with SHA-256 verification, then restart in place.
- **CC-parity tool-output handling** (#509) — `read_file` no longer spills to the offload file: it auto-paginates (2000-line default window, ~100 KB page cap) with a `[Truncated: PARTIAL view …]` banner and a structured `truncatedBySizeCap` flag. Offload thresholds are per tool (bash 30 000 · grep 20 000 · other 50 000 bytes) instead of a flat 16 KiB, and `read_tool_output` gained a `pattern` seek mode returning line numbers + byte offsets — a build log's error at the tail is one seek + one window read.
- **Per-model `extraBody`, `maxOutputTokens`, `contextWindow` overrides in sudocode.json** (#499), and `none`/`minimal` reasoning-effort values.

### Breaking / behavior changes
- **Builtin tool surface pruned** (#521) — removed `LSP`, `MCP`, `McpAuth`, `ListMcpResources`, `ReadMcpResource` (hollow or dead-duplicate paths that always errored in production), `NotebookEdit` and `REPL` (working but cut: schema cost vs. redundancy with `bash`), and `SendUserMessage` (echo-ware). The live MCP path — runtime-registered `mcp__<server>__<tool>` tools and ACP session-injected servers — is unaffected.
- **Config/plugin MCP servers are OFF by default** (#521, #522) — enable with `SUDOCODE_ENABLE_MCP=1` or `"experimental": {"mcpConfigServers": true}`. ACP session-injected MCP stays unconditional; `scode mcp` / `/mcp` config management keeps working.
- **Experimental feature-flag registry** (#522) — every experimental feature now ships behind one registry (`experimental` settings section + `SUDOCODE_EXPERIMENT_<NAME>` env), OFF by default, graduating by flag deletion; unknown keys are config load errors. `coordinatorMode` and `mcpConfigServers` migrated; legacy env vars keep working at highest precedence.

### Changes
- **System prompt cut to ~2.5k tokens** (#516) — six inherited prose sections consolidated to four with each rule stated once, real scode tool names, a 1.7k auto-memory preamble, and skill descriptions capped at 120 chars. `--system-prompt` / ACP `systemPrompt` now replace the `# System` / `# Working` / `# Risky actions` / `# Tools` / `# Git` blocks.
- **Tool results render as digests** (#517) — one `key: value` line per field with `(+N lines)` counts instead of pretty-printed JSON walls; `Skill` and `read_tool_output` get purpose-built one-line summaries; any displayed line caps at 200 chars. Full results stay in the session transcript.
- **TUI polish** (#515) — thinking blocks are no longer surfaced in the transcript (the `Reasoning…` spinner is the cue; the `▼ Thinking (0 chars hidden)` header is gone), the turn status line prints exactly once into scrollback, top-level bullet lists indent two columns, and stub-command replies no longer ghost the footer.

### Fixes
- **linux-arm64 release built on ubuntu-22.04** (#519) — the glibc floor for the shipped arm64 binary drops from 2.39 to 2.35 (Debian 12 / RHEL 9-era hosts work again). linux-x64 remains static musl.
- **Co-hosted agents get an A2A reply-contract prompt** (#510); ACP emits `ToolCallContent` alongside `rawOutput` (#489).

### Internal
- `LazyLock` for the seek regex + `extraBody` parse-fallback warning (#511), nexus-vfs pin bump (#513), dead `ModelFamilyIdentity` plumbing removed (#478).
