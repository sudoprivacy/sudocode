# Usage

Day-to-day `scode` workflows.

## Interactive REPL

```bash
scode
```

The REPL accepts prose and slash commands. Tab completion expands slash
command names, model aliases, permission modes, and recent session IDs.

For the canonical, live command list:

```bash
scode --help
```

## One-shot prompt

```bash
scode "explain this codebase"
```

A one-shot prompt streams to stdout and exits when the turn completes.

## JSON output

```bash
scode --output-format json prompt "summarize src/main.rs"
```

`--output-format json` switches the streaming surface to a
machine-readable event stream. Pair with `scode acp` for an editor or
service integration; see [`acp.md`](./acp.md).

## Resuming a session

```bash
scode --resume latest
scode --resume <session-id>
scode --resume path/to/session.jsonl
```

`--resume` replays the named session into the REPL with full context.

## Health check

```bash
scode doctor
```

`scode doctor` reports auth mode resolution, provider reachability, MCP
server status, config resolution, the permission policy, the sandbox
mode, and the tool / skill inventory.

## Models

Select a model with `--model`. See [`models.md`](./models.md) for aliases
and provider-specific behavior.

```bash
scode --model opus
scode --model sonnet --auth subscription
```

## Authentication

See [`authentication.md`](./authentication.md).

## Permissions and sandbox

See [`permissions-and-sandbox.md`](./permissions-and-sandbox.md).
