# Agent Communication Protocol

`scode` speaks the **Agent Communication Protocol (ACP)** natively, in two
transports that share a single handler chain.

## Transports

```bash
# stdio — for editors, IDE plugins, and CLI orchestrators
scode acp

# WebSocket + embedded Web UI — for browsers and service backends
scode acp serve --port 8080
```

`scode acp serve --port 8080` exposes:

- JSON-RPC over WebSocket at `ws://localhost:8080/ws`
- An interactive Web UI at `http://localhost:8080/`

Both transports share streaming, tool use, elicitation, and permission
prompting.

## Use cases

- **Editor plugins (Zed, VS Code, JetBrains)** speak ACP over stdio.
- **Web apps and dashboards** connect to the WebSocket endpoint or point
  a browser at `/` to use the embedded UI.
- **Automation pipelines and microservices** run `scode acp serve` as a
  long-lived process behind a load balancer.
- **Sub-agents and orchestrators** fan out work to multiple `scode`
  instances over the wire.

## Binding

For local-only use, bind to `127.0.0.1`. For team access, expose the port
behind your own auth proxy.

## Sessions

One `scode acp` process serves **many sessions**. Requests are ordered per
session and independent across sessions:

- Requests on the **same** session (`session/prompt`, `session/setModel`,
  `session/setPermissionMode`, `session/close`) run strictly one at a time,
  in arrival order. Two prompts sent to one session never interleave.
- Requests on **different** sessions run concurrently. In particular, a
  session that is waiting on the user — a pending `session/request_permission`
  or `_scode/ask_user_question` — does not hold up `session/new` or prompts
  on any other session.
- `session/cancel` is never queued; it reaches a session mid-turn.
- Sessions in the same working directory run fully in parallel. Because tool
  execution is anchored to the *process* working directory, turns of sessions
  in **different** directories share that directory cooperatively: a turn
  waits for turns in another directory to finish or to pause on user input
  before it starts, and a paused turn gives the directory back while it waits.

### Per-session system prompt (`_meta.sudocode.*`)

`session/new` and `session/load` accept two optional, orthogonal keys under
the request's `_meta.sudocode` object. Both are plain strings, passed to the
model verbatim — no truncation, no escaping, no size cap beyond the model's
own context window, and no policy about who may use which: that is the
caller's (e.g. a multi-tenant service's) decision.

```json
{
  "cwd": "/work/tenant-a",
  "mcpServers": [],
  "_meta": {
    "sudocode": {
      "systemPrompt": "You are Tenant A's release bot. ...",
      "appendSystemPrompt": "House rules: never push to main. ..."
    }
  }
}
```

| Key | Effect |
|---|---|
| `systemPrompt` | **Override.** Replaces the built-in static system-prompt blocks (the `You are Sudo Code…` identity, `# System`, `# Working`, `# Risky actions`, `# Tools`, `# Git`) with this text as the single static block. |
| `appendSystemPrompt` | **Append.** Added as the last **static** block, after the built-in identity and behaviour blocks and before every dynamic block (environment / project context, `AGENTS.md` instructions, runtime-config summary, auto-memory, plugin inventory, skill listing). A caller preamble is stable for the life of the session, so it belongs in the aggressively cached prefix; the cost is that the workspace-derived dynamic blocks now follow it rather than precede it. |

The two compose: set both and the static blocks are replaced *and* the
extra block is appended after the replacement, still inside the static
prefix. Workspace-derived dynamic blocks (environment,
`AGENTS.md`, memory) are always kept, so an overridden prompt still knows
which directory it is operating in.

Rules:

- A present key must be a non-empty string; an empty/whitespace string or a
  non-string value is rejected with `invalid_params` (`-32602`) rather than
  silently ignored.
- The values are bound to the session for its whole lifetime — a
  `session/setModel` rebuilds the runtime with them re-applied. They are
  **not** persisted with the transcript; a client that wants them on a
  resumed session passes them again on `session/load`.
- They layer on top of the process-wide `--system-prompt` /
  `--append-system-prompt` CLI flags of the `scode acp` process, if any:
  a session `systemPrompt` replaces whatever the process default static
  block is, and a session `appendSystemPrompt` is appended after the
  process-level append.
- The `initialize` response advertises `_meta.sudocode.systemPromptOverride:
  true` and `_meta.sudocode.systemPromptAppend: true` so clients can
  feature-detect.

### Slash commands

A `session/prompt` whose text starts with `/` is a slash command, not a model
turn: it runs on the session's lane like any prompt, streams its result as
`agent_message_chunk` text, and completes with `stopReason: "end_turn"` and
no `usage`. The ACP agent implements a fixed subset of the REPL commands:

| Command | Effect |
|---|---|
| `/help` | List the commands in this table (the REPL-only ones are not shown). |
| `/status` | Model, usage, git and config status for this session. |
| `/cost` | Cumulative token usage for this session. |
| `/model [<model-id>]` | Show the current model, or switch this session to another model. |
| `/compact` | Summarise older messages to free context. LLM summary first; if the model call is unavailable or fails, the local structural summary. No token threshold — an explicit request always compacts when there is anything beyond the preserved recent tail. The compacted transcript is persisted immediately. The reply reports the method used, messages removed / kept, and the estimated token count before and after. |
| `/config [section]` | Show the effective configuration (read-only; `/config set` is REPL-only). |
| `/diff` | Staged and unstaged git changes in the session directory. |
| `/doctor` | Local health checks for auth, config and workspace. |

Any other `/command` is answered with a one-line text hint naming the command
and listing this table.

**Discovery.** Right after a successful `session/new` or `session/load`
response, the agent sends one `session/update` notification advertising the
table, so clients can build a command palette without hard-coding it:

```json
{
  "jsonrpc": "2.0",
  "method": "session/update",
  "params": {
    "sessionId": "…",
    "update": {
      "sessionUpdate": "available_commands_update",
      "availableCommands": [
        { "name": "compact", "description": "Summarise older messages to free context (LLM summary, local fallback)" },
        { "name": "model", "description": "…", "input": { "hint": "<model-id>" } }
      ]
    }
  }
}
```

Names carry no leading slash; send them as `/name …` in `session/prompt`.
`input.hint` is present only for commands that take arguments. The
notification follows the response on the same connection, so the client
already knows the session id when it arrives; be ready to receive
`session/update` for a session as soon as its `session/new` response is in,
because this one follows immediately. The same table drives `/help` and the
unknown-command hint, so the three never disagree.

`session/cancel` applies to `/compact`: a cancel during the model round-trip
drops the call, a cancel that lands after it discards the result; either way
the transcript in memory and on disk is left untouched and the prompt ends
with `stopReason: "cancelled"`. The other commands are local and complete
before a cancel could matter.

**Automatic compaction.** When a turn compacts the transcript on its own —
either the pre-turn overflow guard or the in-turn threshold path — the
`session/prompt` response carries `_meta.sudocode.autoCompacted: true`
alongside the existing `contextWindowTokens`, `estimatedSessionTokens`,
cost and `cumulativeUsage` fields. The key is absent when no automatic
compaction happened; `/compact` itself never sets it (its report is the
text reply). Like the rest of `_meta.sudocode`, it rides on the success
response, so a turn that fails after compacting reports the error and no
`autoCompacted`.

### `session/load`

`agentCapabilities.loadSession` is advertised. `session/load
{sessionId, cwd, mcpServers}` re-opens a session persisted by an earlier
process from `<cwd>/.scode/sessions/<workspace-fingerprint>/<sessionId>.jsonl`
and the next `session/prompt` continues the conversation with the full prior
transcript (user turns, assistant turns, thinking, tool calls and tool
results) sent to the model.

What is restored is the **transcript**: message history, the session's model,
compaction state and fork lineage. What is not: in-memory turn state such as
a permission mode set through `session/setPermissionMode` (the loaded session
starts from the configured default again), per-turn "allow always" answers,
running background commands, and MCP servers other than the ones passed in
the `session/load` request.

`cwd` must be the directory the session was created in — a session's store
is keyed by its workspace and the persisted `workspace_root` is validated on
load, so loading a session id from another directory is rejected. Continuing
a conversation in a new directory is a fork, not a load. `session/load` does
not currently replay the history to the client as `session/update`
notifications; the client is expected to keep its own copy of the
conversation.
