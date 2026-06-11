# TUI enhancement plan

A phased approach to evolving the `rusty-sudocode-cli` terminal UI from
the current REPL/prompt CLI toward a polished, modern TUI experience
while preserving the clean architecture and existing test coverage.

## 1. Architecture context

### Crate map

| Crate | Purpose |
|---|---|
| `rusty-sudocode-cli` | Main binary: REPL loop, arg parsing, rendering, API bridge |
| `runtime` | Session, conversation loop, config, permissions, compaction |
| `api` | Anthropic HTTP client + SSE streaming |
| `commands` | Slash command metadata, parsing, help |
| `tools` | Built-in tool implementations |

### Current TUI components

| Component | Source | Role |
|---|---|---|
| Input | `input.rs` | `rustyline`-based line editor with slash-command tab completion, Shift+Enter newline, history |
| Rendering | `render.rs` | Markdown→terminal rendering (headings, lists, tables, code blocks with syntect highlighting, blockquotes); spinner widget |
| App/REPL loop | `main.rs` | The `LiveCli` struct: REPL loop, slash command handlers, streaming output, tool call display, permission prompting, session management |

### Dependencies

- **crossterm 0.28** — terminal control (cursor, colors, clear)
- **pulldown-cmark 0.13** — Markdown parsing
- **syntect 5** — syntax highlighting
- **rustyline 15** — line editing with completion
- **serde_json** — tool I/O formatting

## 2. Enhancement plan

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
| 1.3 | Turn duration timer. Show elapsed time for the current turn (the `showTurnDuration` config exists in the Config tool and is wired through here). |
| 1.4 | Git branch indicator. Display the current git branch in the status bar (parsed via `parse_git_status_metadata`). |

### Phase 2 · Enhanced streaming output

Make the main response stream visually rich and responsive.

| Task | Description |
|---|---|
| 2.1 | Live markdown rendering. Buffer text deltas and incrementally render Markdown as it arrives (heading detection, bold/italic, inline code). The existing `TerminalRenderer::render_markdown` extends for incremental use. |
| 2.2 | Thinking indicator. When extended thinking/reasoning is active, show a distinct animated indicator (for example, `🧠 Reasoning...` with pulsing dots) alongside the generic `🦀 Thinking...`. |
| 2.3 | Streaming progress bar. Add an optional horizontal progress indicator below the spinner showing approximate completion (based on `max_tokens` vs. `output_tokens` so far). |
| 2.4 | Tune main-stream pacing. The current `stream_markdown` sleeps 8ms per chunk for tool results; make this immediate or configurable for the main response stream. |

### Phase 3 · Tool call visualization

Make tool execution legible and navigable.

| Task | Description |
|---|---|
| 3.1 | Collapsible tool output. For tool results longer than N lines (configurable, default 15), show a summary with an `[+] Expand` hint; pressing a key reveals the full output. Initial form: truncation with a "full output saved to file" fallback. |
| 3.2 | Syntax-highlighted tool results. When tool results contain code (detected by tool name — `bash` stdout, `read_file` content, `REPL` output), apply syntect highlighting alongside the plain text rendering. |
| 3.3 | Tool call timeline. For multi-tool turns, show a compact summary after all tool calls complete: `🔧 bash → ✓ | read_file → ✓ | edit_file → ✓ (3 tools, 1.2s)`. |
| 3.4 | Diff-aware `edit_file` display. When `edit_file` succeeds, render a colored unified diff of the change alongside `✓ edit_file: path`. The data round-trip is documented in [`spike-179.md`](./spike-179.md) (S1). |
| 3.5 | Permission prompt enhancement. Style the approval prompt with box drawing, color the tool name, show a one-line summary of what the tool will do. |

### Phase 4 · Slash commands and navigation

Improve information display and add capabilities.

| Task | Description |
|---|---|
| 4.1 | Colored `/diff` output. Parse the git diff and render it with red/green coloring for removals and additions, similar to `delta` or `diff-so-fancy`. |
| 4.2 | Pager for long outputs. When `/status`, `/config`, `/memory`, or `/diff` produce output longer than the terminal height, pipe through an internal pager (scroll with j/k/q) or external `$PAGER`. |
| 4.3 | `/search` command. Add a command to search conversation history by keyword. Scope is documented in [`spike-179.md`](./spike-179.md) (S2). |
| 4.4 | `/undo` command. Undo the last file edit by restoring from the `original_file` data in `write_file`/`edit_file` tool results. Data round-trip is documented in [`spike-179.md`](./spike-179.md) (S3). |
| 4.5 | Interactive session picker. Replace the text-based `/session list` with an interactive fuzzy-filterable list (up/down arrows to select, enter to switch). |
| 4.6 | Tab completion for tool arguments. Extend `SlashCommandHelper` to complete file paths after `/export`, model names after `/model`, session IDs after `/session switch`. |

### Phase 5 · Color themes and configuration

User-customizable visual appearance.

| Task | Description |
|---|---|
| 5.1 | Named color themes. Add `dark` (the current default), `light`, `solarized`, `catppuccin`. Wire to the `Config` tool's `theme` setting. |
| 5.2 | ANSI-256 / truecolor detection. Detect terminal capabilities and select a tier (16 / 256 / truecolor / no color) at startup. |
| 5.3 | Configurable spinner style. Allow choosing among braille dots, bars, moon phases, etc. |
| 5.4 | Banner customization. Make the ASCII art banner optional or configurable via settings. |

### Phase 6 · Full-screen TUI mode

Optional alternate-screen layout for power users.

| Task | Description |
|---|---|
| 6.1 | Add `ratatui` as an optional dependency behind a `full-tui` feature flag. |
| 6.2 | Split-pane layout. Top pane: conversation with scrollback. Bottom pane: input area. Right sidebar (optional): tool status / todo list. |
| 6.3 | Scrollable conversation view. PgUp/PgDn for navigation, search within the conversation. |
| 6.4 | Keyboard shortcuts panel. A `?` help overlay listing all keybindings. |
| 6.5 | Mouse support. Click to expand tool results, scroll the conversation, select text for copy. |

## 3. Sequencing

Phase 0 lands first as the foundation. Within the remaining phases, the
sequencing follows highest user-facing impact, lowest implementation
cost first:

1. **Phase 0** — module decomposition.
2. **Phase 1.1–1.2** — status bar with live tokens.
3. **Phase 2.4** — main-stream pacing.
4. **Phase 3.1** — collapsible tool output.
5. **Phase 2.1** — live markdown rendering.
6. **Phase 3.2** — syntax-highlighted tool results.
7. **Phase 3.4** — diff-aware edit display.
8. **Phase 4.1** — colored `/diff`.
9. **Phase 5** — color themes (driven by user demand).
10. **Phase 4.2–4.6** — enhanced navigation and commands.
11. **Phase 6** — full-screen mode, scoped after the earlier phases ship.

## 4. Module layout after Phase 0

```
crates/rusty-sudocode-cli/src/
├── main.rs              # Entrypoint, arg dispatch
├── args.rs              # CLI argument parsing
├── app.rs               # LiveCli struct, REPL loop, turn execution
├── format.rs            # Report formatting (status, cost, model, permissions, ...)
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

## 5. Design principles

1. The inline REPL stays the default; full-screen TUI is opt-in
   (`--tui` flag).
2. Every formatting function takes `&mut impl Write` so it stays
   testable without a terminal.
3. Rendering works incrementally; the response stream renders as it
   arrives.
4. Terminal control goes through `crossterm` uniformly; raw ANSI
   escape codes stay out of the codepath.
5. Heavy dependencies (`ratatui`) sit behind a feature flag.

## 6. Risk and mitigation

| Risk | Mitigation |
|---|---|
| Refactor changes REPL behavior | Phase 0 stays a pure restructuring with the existing test coverage as safety net. |
| Terminal compatibility (tmux, SSH, Windows) | Rely on `crossterm`'s abstraction; verify in degraded environments. |
| Rich rendering regresses performance | Profile before/after; keep the raw streaming path always available as a fast fallback. |
| Phase 6 scope expansion | Ship Phases 0–3 as a coherent release before opening Phase 6. |
