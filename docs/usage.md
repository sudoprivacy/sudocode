# Usage

Day-to-day `scode` workflows.

## Web search with Bocha

Set `web_search` in `~/.nexus/sudocode/sudocode.json`:

```json
{
  "web_search": {
    "provider": "bocha",
    "apiUrl": "https://api.bocha.cn/v1/web-search"
  }
}
```

Export `BOCHA_API_KEY` before starting `scode`, or set `web_search.apiKey`
in your private configuration. `.env` files are not loaded automatically.
`BOCHA_API_KEY` takes precedence over the config key. Bocha uses its own
credentials; it does not reuse the model proxy key.

The API URL above is the default when `provider` is `bocha`; it can be
omitted. `SUDOCODE_BOCHA_API_URL` overrides it, and
`SUDOCODE_WEB_SEARCH_PROVIDER=bocha` selects Bocha for a single process.
The existing `WebSearch` tool returns titles, URLs, and summaries, with
domain inclusion/exclusion, deduplication, and at most eight results.
Existing Tavily configurations continue to use `provider: "tavily"`.

```bash
scode --allowedTools WebSearch "Use WebSearch to find the Rust official website and cite the result URLs."
```

## Interactive REPL

```bash
scode
```

The REPL accepts prose and slash commands. Tab completion expands slash
command names, model aliases, permission modes, and recent session IDs.

A line starting with `!` runs the rest as a shell command instead of
sending it to the model:

```text
❯ ! git status --short
  ⎿  M docs/usage.md
```

The output is printed and recorded in the transcript (as
`<bash-input>` / `<bash-stdout>` / `<bash-stderr>` user messages, the
same shape Claude Code uses), so the next prompt you send can refer to
it. No model turn runs. The command executes outside the tool sandbox
with the default tool timeout; a bare `!` is sent to the model as text.
ACP clients get the same behaviour: a `session/prompt` whose text starts
with `!` runs in the session's workspace and streams the output back as
agent text.

For the canonical, live command list:

```bash
scode --help
```

## Bash tool results

Model-invoked `bash` returns compact JSON: `stdout` and the actual
`exit_code` for a completed process, with nonempty `stderr` when present.
Failures retain `returnCodeInterpretation`; interrupted runs include
`interrupted: true`. Background launches retain `backgroundTaskId` and
`noOutputExpected`, but do not claim a completed exit code. Signal termination
also has no numeric exit code and is described in `returnCodeInterpretation`.

Empty optional fields and routine sandbox capability flags are omitted. The
public Rust result type accepts omitted stderr and interruption fields as an
empty string and `false`, respectively. On failure, an available sandbox
fallback reason is included as `sandboxWarning`.
The full execution struct remains available internally; the compact text is
persisted before it reaches the provider, so resume sends identical results.
Large results still use the existing persisted-output marker and
`read_tool_output` pagination.

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

## Compacting a long conversation

`/compact` in the REPL, ACP, or `scode --resume <id> /compact` uses the same
model-backed checkpoint pipeline, even below the automatic pressure threshold.
The configured provider must be available. Older history and any previous
checkpoint are summarized together; recent messages and complete tool exchanges
are retained by token budget. Automatic compaction first tries trimming large
tool outputs to avoid an unnecessary model call.

The checkpoint prompt asks for a complete summary within 8,000 tokens where
possible. Generation has a fixed ceiling of 12,000 output tokens, capped by
the model's smaller output limit (including a `maxOutputTokens` override).
The model's context-window limit still applies.

Failed, empty, truncated, or non-shrinking summaries leave history intact and
report an error instead of continuing with a statistical or empty history.
Pre-request failure stops that request. A post-turn maintenance failure keeps
the already completed response. Successful replacements archive the original
JSONL at `<transcript>.before-compact-<timestamp>` before committing the new
history. Keep these files to inspect or recover older context; they are not
subject to automatic cleanup. See [ACP compaction](acp.md#slash-commands) for
budgets and persistence details.
