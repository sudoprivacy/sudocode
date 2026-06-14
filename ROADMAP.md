# Sudo Code Roadmap

> **Single SSOT plan file.** Project goals plus the active design detail
> behind each goal live here. Reference docs that explain mechanism
> (parity, harness) live separately and are linked from this file.
> Day-to-day work — tasks, sprint boards, schedules — lives in PRs,
> issues, and 1:1 notes.

## What sudo code is

`sudocode` (binary: `scode`) is a Rust-native ACP engine for coding agents
— the hacker-facing CLI half of the sudo* family.

| | sudowork | sudocode |
|---|---|---|
| Audience | Non-technical end users | Developers, hackers, machines |
| Surface | GUI / Electron | CLI / headless / ACP |
| Defaults | Safe, hand-held, friendly copy | Composable, terse, full power |
| Relationship | sudowork uses sudocode as one of its execution engines | — |

The two are tuned for different audiences. The same capability can land
safely-defaulted in sudowork and exposed as a full knob in sudocode.

## North star

- **Rust-native** — single binary, deterministic shutdown, lean footprint.
- **Model-agnostic** — Anthropic, OpenAI, xAI, Gemini, OAuth subscriptions,
  arbitrary proxy backends.
- **Headless-first** — ACP over stdio and WebSocket; embeddable as
  "agent as a service."
- **Safe by design** — explicit permission modes plus a Linux
  user-namespace sandbox.

---

# Goal 1 · Lock the baseline — e2e ≥ 90%

`scode` CLI e2e coverage reaches and stays at **≥ 90%** of the
scode-native testable feature surface, green on every `main` commit of
sudocode CI.

## Test infrastructure — two layers

| Layer | Purpose | Source |
|---|---|---|
| Mock parity harness | Fast, in-process; scripts a mock Anthropic backend and asserts on the byte stream `scode` produces. Catches API integration / tool dispatch / streaming regressions without any TTY. | [`docs/mock-parity-harness.md`](./docs/mock-parity-harness.md) |
| `pty-expect` PTY harness | Human-fidelity; drives `scode` through a real PTY (Unix `/dev/pty`, Windows ConPTY) and asserts against either the raw stream or a VT100-rendered view. Catches REPL UX, signals, tab completion, ANSI redraw. | [`sudoprivacy/pty-expect`](https://github.com/sudoprivacy/pty-expect) (own crate, sudoprivacy fork) |

Coverage is measured as: a feature is `Covered` when **at least one**
of the two harnesses exercises it end-to-end on `main` CI.

## Feature inventory

Mirrored from the original sudowork-side coverage tracker
(`sudowork/docs/scode-e2e-coverage.md`) and **status reset** for the
new sudocode-side harness. The previous sudowork-YAML coverage drove
features through the sudowork UI (mouse / Electron / ACP); the new
infra exercises them inside the sudocode crate itself, so coverage
starts fresh and is tracked here as the single source of truth.

Status values:

- **Covered** — at least one sudocode-side harness test exists on `main`.
- **Gap** — needs a test.
- **N/A** — out of scope for `scode` itself (e.g. sudowork-UI-only flows).

### 1. Core conversation

| Feature | Status | Test | Notes |
|---|---|---|---|
| Single-turn prompt | Gap | — | |
| Multi-turn context | Gap | — | Follow-up references prior turn |
| Streaming response | Gap | — | Implicit in every interaction |
| Graceful cancel mid-execution | Gap | — | Stop during sleep, verify context preserved |
| Agent switching (Sudoclaw ↔ scode) | N/A | — | sudowork-UI concern |

### 2. Tool usage — file operations

| Feature | Status | Test | Notes |
|---|---|---|---|
| `read_file` | Gap | — | |
| `write_file` | Gap | — | |
| `edit_file` | Gap | — | Write → edit → read back |
| `glob_search` | Gap | — | |
| `grep_search` | Gap | — | |

### 3. Tool usage — execution

| Feature | Status | Test | Notes |
|---|---|---|---|
| `bash` | Gap | — | Run script, echo, sleep |
| REPL (persistent Python/JS subprocess) | Gap | — | Different from one-shot bash |
| PowerShell | Gap | — | Windows-only path |
| `NotebookEdit` (Jupyter) | Gap | — | Needs `.ipynb` fixture |

### 4. Tool usage — search & web

| Feature | Status | Test | Notes |
|---|---|---|---|
| `WebSearch` | Gap | — | Searches current info, verifies results |
| `WebFetch` (URL → extract) | Gap | — | |

### 5. Tool usage — code intelligence

| Feature | Status | Test | Notes |
|---|---|---|---|
| LSP `goToDefinition` | Gap (L2) | — | Needs mock language server |
| LSP `findReferences` | Gap (L2) | — | |
| LSP `hover` | Gap (L2) | — | |
| LSP `documentSymbol` | Gap (L2) | — | |
| `ToolSearch` | Gap | — | Search for tools by keyword |

### 6. Tool usage — planning & structured

| Feature | Status | Test | Notes |
|---|---|---|---|
| `EnterPlanMode` / `ExitPlanMode` | Gap | — | Plan → implement → test → iterate |
| `TodoWrite` | Gap | — | Task list management |
| `AskUserQuestion` | Gap | — | scode prompts for clarification |
| `StructuredOutput` | Gap | — | Return structured data |
| `Sleep` | Gap | — | Low priority |

### 7. Tool usage — background & parallel

| Feature | Status | Test | Notes |
|---|---|---|---|
| `Agent` (sub-agents) | Gap (L2) | — | Launch parallel sub-agents |
| `TaskCreate` / `TaskGet` / `TaskList` | Gap (L2) | — | Background task management |
| Workers (boot, trust, prompt) | Gap (L2) | — | Worker lifecycle |
| Teams (parallel sub-agents) | Gap (L2) | — | Team coordination |
| `CronCreate` / `CronDelete` | Gap (L2) | — | Needs wall-clock or fake timer |

### 8. Git workflow

| Feature | Status | Test | Notes |
|---|---|---|---|
| `/commit` (generate + create) | Gap | — | Init repo → write → commit → verify log |
| `/pr` (draft / create PR) | Gap | — | **P0** — core workflow |
| `/diff` (show changes) | Gap | — | |
| `/issue` (create GitHub issue) | Gap | — | |
| `/review` (code review) | Gap | — | |

### 9. Session management

| Feature | Status | Test | Notes |
|---|---|---|---|
| Session auto-save | Gap | — | Persists across turns |
| `/resume` (load saved session) | Gap | — | send → close → resume → verify |
| `/session list` / `switch` / `fork` | Gap | — | |
| `/export` (session to markdown) | Gap | — | |
| `/compact` (compress history) | Gap | — | |

### 10. Configuration & discovery

| Feature | Status | Test | Notes |
|---|---|---|---|
| `/model` (switch model) | Gap | — | Show current + switch |
| `/permissions` (switch mode) | Gap | — | read-only / workspace-write / danger |
| `/auth` (switch auth mode) | N/A | — | sudowork credential injection concern |
| `/skills` (list / invoke) | Gap | — | |
| `/agents` (list) | Gap | — | |
| `/mcp` (list / show servers) | Gap (L2) | — | Plugin lifecycle |
| `/plugins` (manage) | Gap (L2) | — | Plugin install/enable/disable |
| `/config` (inspect) | Gap | — | |

### 11. Diagnostics

| Feature | Status | Test | Notes |
|---|---|---|---|
| `/doctor` | Gap | — | |
| `/status` | Gap | — | Model, permissions, usage |
| `/sandbox` | Gap | — | Isolation status |
| `/cost` (token usage) | Gap | — | |
| Prompt-response token usage | Gap | — | |

### 12. Auth

| Feature | Status | Test | Notes |
|---|---|---|---|
| Subscription auth (CC OAuth) | Gap | — | Uses injected `CLAUDE_CODE_OAUTH_TOKEN` |
| Proxy auth (sudorouter) | Gap | — | Via `ANTHROPIC_API_KEY` injection |
| API key auth | Gap | — | |
| `scode login` (CLI) | Gap | — | |
| `scode logout` (CLI) | Gap | — | |

### Coverage denominator (the 90% target)

| Category | Total | N/A | L2 deferred | In current denominator |
|---|---|---|---|---|
| Core Conversation | 5 | 1 | 0 | 4 |
| File Operations | 5 | 0 | 0 | 5 |
| Execution | 4 | 0 | 0 | 4 |
| Search & Web | 2 | 0 | 0 | 2 |
| Code Intelligence | 5 | 0 | 4 | 1 |
| Planning & Structured | 5 | 0 | 0 | 5 |
| Background & Parallel | 5 | 0 | 5 | 0 |
| Git Workflow | 5 | 0 | 0 | 5 |
| Session Management | 5 | 0 | 0 | 5 |
| Config & Discovery | 8 | 1 | 2 | 5 |
| Diagnostics | 5 | 0 | 0 | 5 |
| Auth | 5 | 0 | 0 | 5 |
| **Total** | **59** | **2** | **11** | **46** |

**90% target = 42 features covered** out of the 46 in-denominator
features. `L2 deferred` items are real surface but need heavier setup
(mock LSP server, wall-clock, plugin lifecycle); they are tracked but
do not count toward the Q2 number.

## Priority sequencing

| Tier | Items |
|---|---|
| **P0 — core differentiators** | `/commit`, `/pr` (git workflow); EnterPlanMode/ExitPlanMode → execute; `edit_file` text-replace verification; **true async cancel** (interrupt in-flight API mid-request, not only between iterations) |
| **P1 — high-value tools** | `WebFetch`, sub-agents (`Agent`), background tasks (`TaskCreate` + `TaskGet`) — these three are L2 deferred but earn early follow-up |
| **P2 — session & config** | `/resume`, permission modes, `/doctor` through the REPL |
| **P3 — advanced (post-Q2)** | LSP code navigation, REPL persistent sessions, cron scheduling, MCP / Skills / Plugins lifecycle |

The async-cancel item under P0 is a behavioural P0, not just a coverage
gap: today the runtime only checks `hook_abort_signal.is_aborted()`
between API call iterations, so a cancel during a long-running API
(e.g. `WebSearch`) does not interrupt the request — it takes effect
only on the next loop iteration. Cancel needs to flow through to the
streaming response path (e.g. `tokio::time::timeout` or per-call abort
checking) so `sleep 999` + API call is truly interruptible.

---

# Goal 2 · claude-code parity

Every feature gap between `scode` and `anthropics/claude-code` carries a
written resolution — `[BUILD]` / `[CHERRY-PICK]` / `[SKIP]` / `[N/A]` /
`[OBSERVE]` — with a one-line rationale.

Three reference sources, with distinct roles:

- **Source of truth** — `anthropics/claude-code` itself. The source is
  private; the signal comes from the public CHANGELOG, the npm bundle's
  tool and slash surfaces, and the official docs.
- **Behavioral reference** — `claude-code-best/claude-code` (CCB), a
  TypeScript reconstruction of Claude Code that aims for high
  source-level fidelity. We **always** open CCB while making a parity
  decision: it converts CHANGELOG entries into readable source so we
  can confirm what CC actually does. We read it; we do not lift its
  TypeScript into our Rust tree.
- **Cherry-pick source** — `ultraworkers/claw-code`, a Rust port we can
  lift commits from when the feature shape overlaps. Optional input,
  not upstream-of-truth.

The mechanism, the standing assumption about CCB, the mandatory
"CHANGELOG → grep CCB → align understanding" loop, and the two sync
markers (`LAST_PARITY_SYNC_COMMIT` for claw-code,
`LAST_CCB_REF_VERSION` for CCB) all live in
[`docs/parity.md`](./docs/parity.md). The current sync cycle report is
at [`docs/parity-claw-code-sync-2026-W24.md`](./docs/parity-claw-code-sync-2026-W24.md).

---

# Goal 3 · Ship features real users miss

When an actual user — internal or external — hits a sharp edge in `scode`
that `claude-code` has already smoothed, the feature lands here as a
committed item.

| Feature | Source signal |
|---|---|
| `!` bash mode (inline shell from prompt) | 武鹏 — 2026-06-10 (内部用户) |

## `!` bash mode — design

The standing rule on parity work (every parity decision opens CCB and
confirms what CC actually does, documented in [`docs/parity.md`](./docs/parity.md)
and [`CLAUDE.md`](./CLAUDE.md)) was first run for this feature against
CCB `@91cffe16` on 2026-06-13.

### What `!` bash mode is

A prompt that starts with `!` bypasses the LLM round-trip and dispatches
the rest of the line directly to a shell — `!ls`, `!git status`,
`!cd path`, and so on. Muscle-memory parity with claude-code plus two
scode-specific guarantees:

- The resolved `pwd` is displayed on every bash-mode prompt redraw.
- `!cd` updates the session `cwd`; subsequent prompt-driven and
  LLM-driven tool calls share the same working directory.
- Every `!` command routes through the same validators as the
  LLM-driven `bash` tool path, so the active permission mode applies
  identically.

### CCB validation — concrete findings

#### stdin handling: `Stdio::null()` vs `pipe` — landing on `pipe`

CCB uses `child_process.spawn({ stdio: ['pipe', ...] })` for the bash
tool path. PR #192 landed `Stdio::null()` on our side. The two are
functionally equivalent for the current LLM-driven bash tool —
reading stdin in the child closes/EOFs immediately under both — and
the original `null` decision was made to minimise surface.

Ethan's call on 2026-06-13 overrides: mirror CCB and move to
`Stdio::piped()`. Reasoning:

- The `pipe` choice is forward-compatible. Keystroke relay for a
  future interactive `!`-mode v2 (passing user input through to
  `!python`, `!ssh`, `!vim`-style commands) does not need a
  re-architecture — only a parent-side writer.
- Mirroring CCB at the spawn site keeps the standing rule's
  invariant clean: when CCB and our implementation diverge, we
  document why, and "we chose less surface area to start" is a
  weaker reason than "we want the optionality CCB demonstrates."
- Pairing `pipe` with the rearranger below preserves the
  hang-prevention behaviour PR #192 was about.

**Decision:** switch `prepare_tokio_command` in `runtime::bash` from
`Stdio::null()` to `Stdio::piped()` for stdin. The parent side closes
its writer end immediately for the current scope — the child still
sees EOF, exactly as today — but the spawn shape is now CCB-aligned.

#### `< /dev/null` injection for piped commands — adopting the rearranger

CCB's `bashProvider.ts:152-154` rearranges piped commands to inject
`< /dev/null` after the first command (e.g. transforming
`rg foo | wc -l` into `rg foo < /dev/null | wc -l`). The CCB inline
comment captures the failure mode: when the spawn-level stdin is a
`pipe`, piped commands inside `eval` inherit the spawn pipe on the
first command's stdin, and a no-path `rg` waits on that pipe forever.

Because the stdin decision above moves us to `pipe`, the rearranger
ships with it. Without the rearranger, `pipe` regresses the hang
behaviour that PR #192 fixed.

**Decision:** port `rearrangePipeCommand` into `runtime::bash`. The
function inspects the command, detects the first `|` outside quoted
segments, and inserts `< /dev/null` before it. Re-implement in Rust
from understanding, no TS lift. Cover the cases CCB covers — quoted
pipes (`echo "a|b"`) stay untouched, heredocs (`<<EOF`) and process
substitution (`<( ... )`) keep their semantics.

#### Why we are not switching the underlying pipe to nexus-vfs DT_PIPE

`nexus-vfs/rust/kernel/benches/syscall_bench.rs` records DT_PIPE at
~246 ns round-trip versus host OS pipe at ~1–2 µs — roughly
4–8× faster — for **in-process Rust ↔ Rust** byte handoffs.

That win does not transfer to bash subprocess stdin, for two
structural reasons:

- bash is a foreign OS process. Its `read(stdin_fd)` is an honest host
  syscall against an fd the kernel gave it. There is no path for
  bash to call `kernel.pipe_read_nowait()` directly.
- The only way to expose DT_PIPE bytes to a foreign process is via
  nexus-fuse mount or LD_PRELOAD. Both add a userspace round-trip on
  top of host pipe — strictly slower than a direct host pipe.

`nexus/rust/services/src/acp/subprocess.rs` matches this reasoning
in practice: it spawns its ACP CLI subprocess with normal host OS
pipes, then `dup`s the parent-side fds and hands the duplicates to
the kernel as stdio-backed DT_PIPE entries. The DT_PIPE wrapping is
for **VFS observability of the ACP traffic**, not for raw throughput.
The child still uses host pipes.

**Decision:** the bash subprocess pipe is host OS pipe. The DT_PIPE
optionality is reserved for two future scopes:

- ACP-style stdio observability for `!`-mode commands (parent-side
  wrap, child unchanged) — only when we want audit / replay /
  cross-session inspection of bash output.
- Any future Rust ↔ Rust in-process pipe inside sudocode that does
  not cross a process boundary — there DT_PIPE is the right default.

A regression benchmark for the host pipe path lives in
[`rust/crates/runtime/benches/bash_pipe_throughput.rs`](./rust/crates/runtime/benches/bash_pipe_throughput.rs)
so any change to the spawn shape surfaces in CI.

#### cwd persistence: the `pwd -P >| <track_file>` pattern

CCB tracks cwd by appending `pwd -P >| ${shellCwdFilePath}` to the
command string and reading the file back from the host side after the
command completes (`src/utils/bootstrap/state.ts` exposes
`setCwdState`). This is the same problem `!cd` persistence solves for
us, and CCB's pattern is the blueprint:

- Each bash-mode command appends a `pwd -P` write to a per-session
  temp file.
- After the command returns, the host reads the file, parses the
  cwd, and updates the session state used to launch the next command.
- The next launch picks up the new cwd as its `current_dir`.

**Decision:** port this pattern into our `runtime::bash` extension for
`!`-mode. Re-implement in Rust from understanding; do not lift TS.

**Verification gap (please test on macOS / Linux when convenient):**
the user reports that CC's `!cd` does not actually persist cwd in
their daily use. CCB's source suggests it does. The discrepancy may be
that CC routes `!`-mode through a different code path, or there is a
platform-specific regression. Confirm before declaring the pattern
shipped.

### CCB extras observed on the bash spawn path

Six patterns CCB applies on the bash spawn path that we have not yet
adopted. Each is real and well-motivated; none is on the critical
path for shipping `!`-mode v1. Captured here so the corresponding
trigger surfaces the question at the right time.

| Pattern | Why CCB does it | When we revisit |
|---|---|---|
| Snapshot file `source` on every command | Preserve user aliases and environment so commands behave like the user's interactive shell | When users report "this works in my terminal but not in scode" alias/env divergence |
| Session env hook script (`getSessionEnvironmentScript`) | Apply env vars set by session-start hooks before each command | When we land session-start hooks |
| Tmux socket isolation (`TMUX` env override) | Prevent the LLM from reading/writing the user's tmux session | When we ship multi-session / agents-in-tmux features |
| Extended glob disable (`shopt -u extglob` style) | Reduce attack surface for glob-based injection | When we land a permission tightening pass |
| `CLAUDE_CODE_SHELL_PREFIX` env wrapper | Let users wrap every command (e.g. `nice -n 19 <cmd>`, sudo policy injection) | When users ask for cross-cutting command wrappers |
| `pwd -P >\| <track_file>` written by command | Persist cwd across commands | **Now** — directly informs `!cd` persistence above |

### PowerShell side: already aligned

CCB's PowerShell provider invokes pwsh with `-NoProfile -NonInteractive
-Command <cmd>`. Our `tools::execute_shell_command` already uses the
same flag set (`-NoProfile -NonInteractive`) plus `Stdio::null()` on
the stdin handle. No gap.

### Implementation steps for `!`-mode v1

1. Prompt parser: treat `!`-prefix lines specially in the REPL input path.
2. Bash dispatch: `runtime::bash` entry that takes the rest of the line,
   applies the cwd from session state, and appends the CCB-style
   `pwd -P >| <track_file>` to the command.
3. cwd persistence: read the track file after each `!`-command, update
   `Session::cwd`, surface the resolved `pwd` in the prompt redraw.
4. Permission gating: route every `!`-command through the same
   validator chain as the LLM-driven `bash` tool path. No permission
   shortcut for `!`.
5. PTY-level acceptance test in `pty-expect` driving `!ls` / `!cd /tmp`
   / `!pwd` and asserting the rendered prompt shows the new pwd.

### Reference

- Tier 2 source: `claude-code-best/claude-code @ 91cffe16e23fc886f6860a7edfe8754d74a4abbf`
  (recorded in `LAST_CCB_REF_VERSION`).
- Files read for this design:
  - `src/utils/Shell.ts` (spawn site, stdio config)
  - `src/utils/ShellCommand.ts` (wrapSpawn wrapper)
  - `src/utils/shell/bashProvider.ts` (bash command assembly, pwd tracking, pipe rearrangement)
  - `src/utils/shell/powershellProvider.ts` (pwsh flags)
- Our current state: `rust/crates/runtime/src/bash.rs`,
  `rust/crates/tools/src/lib.rs` (around `execute_shell_command`,
  `run_powershell`).

## TUI enhancement — phased plan

Evolving the `rusty-sudocode-cli` terminal UI from the current
REPL/prompt CLI toward a polished, modern TUI experience while
preserving the clean architecture and existing test coverage.

### TUI architecture context

| Crate | Purpose |
|---|---|
| `rusty-sudocode-cli` | Main binary: REPL loop, arg parsing, rendering, API bridge |
| `runtime` | Session, conversation loop, config, permissions, compaction |
| `api` | Anthropic HTTP client + SSE streaming |
| `commands` | Slash command metadata, parsing, help |
| `tools` | Built-in tool implementations |

Current TUI components:

| Component | Source | Role |
|---|---|---|
| Input | `input.rs` | `rustyline`-based line editor with slash-command tab completion, Shift+Enter newline, history |
| Rendering | `render.rs` | Markdown→terminal rendering (headings, lists, tables, code blocks with syntect highlighting, blockquotes); spinner widget |
| App/REPL loop | `main.rs` | The `LiveCli` struct: REPL loop, slash command handlers, streaming output, tool call display, permission prompting, session management |

Dependencies: **crossterm 0.28**, **pulldown-cmark 0.13**,
**syntect 5**, **rustyline 15**, **serde_json**.

### Phase 0 · Structural cleanup

Break `main.rs` into focused modules and establish the namespace for the
new TUI work.

| Task | Description |
|---|---|
| 0.1 | Extract `LiveCli` into `app.rs`. Move the struct, its impl, and helpers (`format_*`, `render_*`, session management) into focused modules: `app.rs` (core), `format.rs` (report formatting), `session_manager.rs` (session CRUD). |
| 0.2 | Introduce module decomposition intentionally. Stream event handler patterns and other ideas from earlier prototypes land inside the active `LiveCli` extraction. |
| 0.3 | Extract `main.rs` arg parsing into a dedicated module when the work begins. |
| 0.4 | Create a `tui/` module at `crates/rusty-sudocode-cli/src/tui/mod.rs` as the namespace for new TUI components: `status_bar.rs`, `layout.rs`, `tool_panel.rs`, etc. |

### Phase 1 · Status bar and live HUD

Persistent information display during interaction.

| Task | Description |
|---|---|
| 1.1 | Terminal-size-aware status line. Use `crossterm::terminal::size()` to render a bottom-pinned status bar showing model name, permission mode, session ID, cumulative token count, estimated cost. |
| 1.2 | Live token counter. Update the status bar in real-time as `AssistantEvent::Usage` and `AssistantEvent::TextDelta` events arrive during streaming. |
| 1.3 | Turn duration timer. Show elapsed time for the current turn. |
| 1.4 | Git branch indicator. Display the current git branch in the status bar (parsed via `parse_git_status_metadata`). |

### Phase 2 · Enhanced streaming output

Make the main response stream visually rich and responsive.

| Task | Description |
|---|---|
| 2.1 | Live markdown rendering. Buffer text deltas and incrementally render Markdown as it arrives. |
| 2.2 | Thinking indicator. When extended thinking/reasoning is active, show a distinct animated indicator alongside the generic `🦀 Thinking...`. |
| 2.3 | Streaming progress bar. Optional horizontal indicator showing approximate completion. |
| 2.4 | Tune main-stream pacing. The current `stream_markdown` sleeps 8ms per chunk for tool results; make this immediate or configurable for the main response stream. |

### Phase 3 · Tool call visualization

Make tool execution legible and navigable.

| Task | Description |
|---|---|
| 3.1 | Collapsible tool output. For tool results longer than N lines (default 15), show a summary with an `[+] Expand` hint. |
| 3.2 | Syntax-highlighted tool results. Apply syntect highlighting to bash stdout / `read_file` content / `REPL` output. |
| 3.3 | Tool call timeline. Show a compact summary after all tool calls complete: `🔧 bash → ✓ | read_file → ✓ | edit_file → ✓ (3 tools, 1.2s)`. |
| 3.4 | Diff-aware `edit_file` display. Render a colored unified diff. The `EditFileOutput` schema already carries `original_file`, `old_string`, and `new_string`, so no schema change is needed; the CLI computes the diff in-process. Defensively strip any `\n\nHook feedback:` suffix before `serde_json::from_str` so hook fire does not silently disable the preview. |
| 3.5 | Permission prompt enhancement. Style the approval prompt with box drawing, color the tool name, show a one-line summary. |

### Phase 4 · Slash commands and navigation

| Task | Description |
|---|---|
| 4.1 | Colored `/diff` output. |
| 4.2 | Pager for long outputs (`/status`, `/config`, `/memory`, `/diff`). |
| 4.3 | `/search` command. Default scope: current-session messages only (linear scan over `Session.messages`, ~50 ms even on large sessions, no storage redesign). `--all` opts in to all persisted JSONL sessions in the workspace; `--tools` opts in to scanning tool-result bodies. Cross-session and tool-output scopes are deferred until the default is in use. |
| 4.4 | `/undo` command. Walk `session.messages.iter().rev()` for the most recent `ToolResult` whose `tool_name == "edit_file" \| "write_file"` and matching `file_path`, then apply its `original_file`. Pre-image survives a single round-trip on the active JSONL but can be erased by session rotation (256 KiB threshold, 3 rotated keepers) and by `/compact` (default keeps last 4 messages plus summary). Defensive parse strips `\n\nHook feedback:` suffix before `serde_json::from_str`. |
| 4.5 | Interactive session picker. Replace text-based `/session list` with a fuzzy list. |
| 4.6 | Tab completion for tool arguments. |

### Phase 5 · Color themes and configuration

| Task | Description |
|---|---|
| 5.1 | Named color themes (`dark`, `light`, `solarized`, `catppuccin`). |
| 5.2 | ANSI-256 / truecolor detection. |
| 5.3 | Configurable spinner style. |
| 5.4 | Banner customization. |

### Phase 6 · Full-screen TUI mode

Optional alternate-screen layout for power users.

| Task | Description |
|---|---|
| 6.1 | Add `ratatui` behind a `full-tui` feature flag. |
| 6.2 | Split-pane layout. |
| 6.3 | Scrollable conversation view (PgUp/PgDn). |
| 6.4 | Keyboard shortcuts panel (`?` overlay). |
| 6.5 | Mouse support. |

### TUI sequencing

Phase 0 first (foundation). Within the rest, highest user-facing
impact, lowest implementation cost first:

1. **Phase 0** — module decomposition
2. **Phase 1.1–1.2** — status bar with live tokens
3. **Phase 2.4** — main-stream pacing
4. **Phase 3.1** — collapsible tool output
5. **Phase 2.1** — live markdown rendering
6. **Phase 3.2** — syntax-highlighted tool results
7. **Phase 3.4** — diff-aware edit display
8. **Phase 4.1** — colored `/diff`
9. **Phase 5** — color themes (driven by user demand)
10. **Phase 4.2–4.6** — enhanced navigation and commands
11. **Phase 6** — full-screen mode, after the earlier phases ship

### TUI design principles

1. The inline REPL stays the default; full-screen TUI is opt-in (`--tui`).
2. Every formatting function takes `&mut impl Write` so it stays
   testable without a terminal.
3. Rendering works incrementally; the response stream renders as it
   arrives.
4. Terminal control goes through `crossterm` uniformly; raw ANSI
   escape codes stay out of the codepath.
5. Heavy dependencies (`ratatui`) sit behind a feature flag.

### TUI module layout after Phase 0

```
crates/rusty-sudocode-cli/src/
├── main.rs              # Entrypoint, arg dispatch
├── args.rs              # CLI argument parsing
├── app.rs               # LiveCli struct, REPL loop, turn execution
├── format.rs            # Report formatting (status, cost, model, permissions, …)
├── session_mgr.rs       # Session CRUD: create, resume, list, switch, persist
├── init.rs              # Repo initialization
├── input.rs             # Line editor
├── render.rs            # TerminalRenderer, Spinner
└── tui/
    ├── mod.rs           # TUI module root
    ├── status_bar.rs    # Persistent bottom status line
    ├── tool_panel.rs    # Tool call visualization (boxes, timelines, collapsible)
    ├── diff_view.rs     # Colored diff rendering
    ├── pager.rs         # Internal pager for long outputs
    └── theme.rs         # Color theme definitions and selection
```

### TUI risk and mitigation

| Risk | Mitigation |
|---|---|
| Refactor changes REPL behavior | Phase 0 stays a pure restructuring; existing test coverage as safety net. |
| Terminal compatibility (tmux, SSH, Windows) | Rely on `crossterm`'s abstraction; verify in degraded environments. |
| Rich rendering regresses performance | Profile before/after; keep the raw streaming path as a fast fallback. |
| Phase 6 scope expansion | Ship Phases 0–3 as a coherent release before opening Phase 6. |

---

# Working agreement on this document

Scope changes update this document in the same PR. External
communications can mirror the current state into other surfaces; this
document remains the canonical reference.

# Reference docs (mechanism, not plan)

- [`docs/parity.md`](./docs/parity.md) — what claude-code parity
  means, the three-tier reference model, the standing rule, the sync
  markers.
- [`docs/mock-parity-harness.md`](./docs/mock-parity-harness.md) — the
  in-process mock backend layer.
- [`docs/parity-claw-code-sync-2026-W24.md`](./docs/parity-claw-code-sync-2026-W24.md)
  — current cycle's 614-commit triage report.
- [`README.md`](./README.md) — project entry, install, quick start.
- [`rust/README.md`](./rust/README.md) — Cargo workspace map.
- [`sudoprivacy/pty-expect`](https://github.com/sudoprivacy/pty-expect)
  — PTY harness crate, source of human-fidelity e2e tests.
