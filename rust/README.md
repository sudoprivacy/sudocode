# Rust workspace

The Cargo workspace for `scode`. This file describes the workspace
layout, the per-crate responsibilities, and the local commands a
contributor uses while working in `rust/`.

For project-level documentation — what `scode` is, how to install, how
to run it — see [`../README.md`](../README.md) and the topic pages under
[`../docs/`](../docs/).

## Layout

```text
rust/
├── Cargo.toml              # Workspace root
├── Cargo.lock
├── scripts/                # Workspace-scoped harnesses and helpers
└── crates/
    ├── api/                # Provider clients + streaming + request preflight
    ├── commands/           # Slash-command registry + help rendering
    ├── compat-harness/     # Tool/prompt manifest extraction
    ├── mock-anthropic-service/   # Deterministic /v1/messages mock
    ├── plugins/            # Plugin metadata, install/enable/disable surfaces
    ├── runtime/            # Session, config, permissions, MCP, prompts, auth loop
    ├── rusty-sudocode-cli/ # The `scode` binary
    ├── telemetry/          # Session trace events + usage telemetry types
    └── tools/              # Built-in tools, skill resolution, tool search
```

## Crate responsibilities

- **api** — provider clients, SSE streaming, request/response types,
  multi-mode auth (`--auth` / `AuthMode`), request-size and context-window
  preflight.
- **commands** — slash command definitions, parsing, help text
  generation, JSON / text rendering.
- **compat-harness** — extracts tool and prompt manifests from upstream
  TypeScript sources.
- **mock-anthropic-service** — deterministic `/v1/messages` mock for CLI
  parity tests and local harness runs. See
  [`../docs/mock-parity-harness.md`](../docs/mock-parity-harness.md).
- **plugins** — plugin metadata, install / enable / disable / update,
  plugin tool definitions, hook integration surfaces.
- **runtime** — `ConversationRuntime`, config loading, session
  persistence, permission policy, MCP client lifecycle, system prompt
  assembly, usage tracking.
- **rusty-sudocode-cli** — REPL, one-shot prompt, direct CLI
  subcommands, streaming display, tool call rendering, CLI argument
  parsing.
- **telemetry** — session trace events and supporting telemetry payloads.
- **tools** — tool specs and execution: Bash, ReadFile, WriteFile,
  EditFile, GlobSearch, GrepSearch, WebSearch, WebFetch, Agent,
  TodoWrite, NotebookEdit, Skill, ToolSearch, and the runtime-facing
  tool discovery surface.

## Local commands

All `cargo` commands assume `rust/` as the working directory.

```bash
# Build everything
cargo build --workspace

# Format, lint, and test before pushing
cargo fmt --all
cargo clippy --workspace
cargo test --workspace

# Run the CLI from a debug build
cargo run --bin scode -- --help
cargo run --bin scode -- prompt "explain this codebase"

# Run the deterministic mock parity harness
./scripts/run_mock_parity_harness.sh
```

For the canonical CLI surface, run `cargo run --bin scode -- --help`.

For PR expectations, the required check commands, and the workflow, see
[`../CONTRIBUTING.md`](../CONTRIBUTING.md).
