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

## Custom system prompt

```bash
# Replace the built-in identity + behaviour blocks
scode --system-prompt "You are a terse release bot." "cut a release"

# Keep the defaults, add house rules as the final *static* block
# (cached with the built-ins; dynamic workspace context still follows it)
scode --append-system-prompt "Never push to main." "cut a release"

# Both at once — they compose
scode --system-prompt "..." --append-system-prompt "..." "cut a release"

# Preview what the model will receive
scode system-prompt --append-system-prompt "Never push to main."
```

`--system-prompt` swaps out the static blocks (`You are Sudo Code…`,
`# System`, `# Working`, …); `--append-system-prompt` adds a trailing
block after the workspace context (environment, `AGENTS.md`, auto-memory).
Neither is truncated or escaped, and the workspace context is always kept.
Both are global flags, so they also apply to `scode acp` as the process
default that ACP sessions can further adjust per session via
`_meta.sudocode.systemPrompt` / `appendSystemPrompt` — see
[`acp.md`](./acp.md#per-session-system-prompt-_metasudocode).

## Memory

Every session gets a `# auto memory` block in the system prompt: the
entries under the workspace's memory directory (`~/.scode/projects/<slug>/memory/`
by default, `SUDOCODE_MEMORY_DIR` to relocate) plus the instructions that
tell the model how to write new ones. `/memory` in the REPL opens the
files.

To turn memory off, set `autoMemoryEnabled` to `false` in settings —
globally in `~/.nexus/sudocode/settings.json`, or per project in
`<cwd>/.nexus/sudocode/settings.local.json` (the scope
`/config set autoMemoryEnabled false` writes to):

```json
{ "autoMemoryEnabled": false }
```

With the key off the block is dropped entirely — no remembered entries,
no write instructions — and the memory directory is left untouched.
`scode system-prompt` shows the result. The key is read at session start
(and on `scode acp` per session `cwd`), so a running REPL needs a restart
to pick up a change. Note that `SUDOCODE_MEMORY_DIR` pointed at an empty
directory is *not* an off switch: the write instructions still go in and
the model still writes there.

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
