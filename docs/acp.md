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
