# Unified Agent/PID Tool Design

> Status: **design draft** — not yet implemented.
> Context: [agent-context-storage-matrix](./agent-context-storage-matrix.html) §identity.
> ShareOne discussion: https://s.shareone.vip/s/agent-context-storage-matrix

## Problem

sudocode exposes two parallel agent-communication systems to the LLM:

1. **Sub-agent (coordinator mode)** — `SendMessage` tool, local JSONL
   mailbox, broadcast + structured shutdown + live abort.
2. **A2A (managed-agent)** — `send_message` tool, nexus gRPC DT_STREAM,
   minimal schema (`to` + `body`).

Both exist because they evolved independently.  From the LLM's perspective
the only differences are (1) system prompt and (2) available tool set, but
the underlying spawn, messaging, stop, and result-collection layers are
fully duplicated with no shared code.

## Core insight: align to nexus agent/pid

nexus already implements the correct abstraction — the same split an OS
makes between an executable on disk and a running process:

- **`/agents/{name}/`** — persistent identity (image layer): config,
  prompts, skills, memory, durable `chat-with-me` inbox.
- **`/proc/{pid}/`** — ephemeral runtime handle: status, per-run
  `chat-with-me`, workspace links, fd tunnels.  Reaped on exit.

**Session is NOT a top-level concept for the LLM.**  Session is an agent
sub-resource (`/agents/{name}/sessions/{sid}/`) managed by the framework.
The LLM never sees session-ids — like shell `fork()` doesn't require manual
`/proc` fd management.  The framework decides which session to use based on
the spawn mode.

## Unified LLM tool set

Tool names are self-explanatory — no system prompt needed to explain modes.

| Tool | Target | Description |
|------|--------|-------------|
| `ls /agents/` | agent | Discovery — who exists, what they're good at |
| `spawn-agent` | agent → produces pid | Create a new pid (clean, no session history) |
| `fork` | current pid → new pid | Fork current agent + session context |
| `send-message` | pid or agent inbox | Send message (transport auto-routed) |
| `kill` | pid | Terminate a specific run; agent unaffected |
| `task-output` | pid | Retrieve a run's result |
| `get-status` | pid | Query run state (READY/BUSY/TERMINATED) |

**Key constraints:**

- `spawn-agent` / `fork` cannot target a pid — pid is the *output*, not an
  input.
- `kill` cannot target an agent — agent is a durable identity; the LLM has
  no authority to delete it.
- `send-message` is one tool; transport auto-routes (local sub-agent → file
  mailbox; nexus co-host → gRPC DT_STREAM).

## Three spawn modes

| Tool | Description | nexus mapping | Use case |
|------|-------------|---------------|----------|
| `spawn-agent` (other) | Start another agent's pid | `start_session(agent_id="expert")` | Delegate to a specialist |
| `spawn-agent` (self) | Start a clean pid of same agent type | `start_session(agent_id=self, session=None)` | Stateless task (search, verification) |
| `fork` | Fork current agent + inherit session | `start_session(agent_id=self, session=snapshot(current))` | Parallel subtask needing context |

`spawn-agent` (self) is what the current `Agent` tool does today.

`fork` is the hardest — requires snapshot semantics.  The forked pid gets a
**copy** of the current transcript (not a shared mutable reference), then
appends independently.  Like `git branch` — shared history, divergent
future.

### Fork result handling

| Strategy | Semantics | Analogy | Default? |
|----------|-----------|---------|----------|
| **drop** | Discard fork transcript, keep only final output | `git cherry-pick` one commit | ✅ default |
| **merge back** | Merge fork transcript into parent session | `git merge` | On explicit request |

Default **drop** avoids context pollution — parent session sees only
`[task-result from pid-xxx]: <summary>`.  For full details: `task-output(pid)`
or resume the fork's session.

Both strategies are implemented; LLM chooses via an optional parameter on
`task-output` (e.g. `merge: true`), not on `fork` itself — the decision
happens at collection time, not at fork time.

## send-message routing: no persistent pid required

`send-message` to an agent's durable inbox (`/agents/{name}/chat-with-me`)
does **NOT** require the agent to have a running pid.  Messages persist in
the inbox and are consumed when the agent next spawns.

Analogy: email — the recipient doesn't need to be online.

For immediate response: the framework can **auto-spawn** a pid on demand
(lazy activation), or the caller can explicitly `spawn-agent` + then
`send-message` to the pid.

## Scenario coverage

| Scenario | Operation | Result |
|----------|-----------|--------|
| Parallel (needs context) | `fork` → pid-1, pid-2 | Each pid snapshots session, works independently |
| Parallel (stateless) | `spawn-agent(self)` × 2 | Clean context window, no wasted tokens |
| Delegate to expert | `send-message` → agent inbox | Message persists; expert consumes on next spawn |
| Delegate + immediate | `spawn-agent("expert")` + `send-message` → pid | Explicit spawn + send |
| Cancel parallel worker | `kill(pid-1)` | pid destroyed, agent unaffected |
| Expert keeps context | `spawn-agent("expert")` | Framework auto-resumes most recent session |

## Migration from current tools

| Current | Unified | Notes |
|---------|---------|-------|
| `SendMessage` + `send_message` | `send-message` | Superset schema, transport auto-routed |
| `Agent` tool | `spawn-agent` + `fork` | Split by session semantics |
| `TaskStop` | `kill` | Target is explicitly a pid |
| `TaskGet` / `TaskList` | `get-status` | Target is explicitly a pid |
| `TaskOutput` | `task-output` | Target is explicitly a pid; optional `merge: true` |
| *(new)* | `ls /agents/` | Agent discovery |

## Open questions

- **Q1**: Should `send-message` to an offline agent auto-spawn a pid
  (lazy activation), or always require explicit `spawn-agent` first?
- **Q2**: How does `fork` interact with memory (`/agents/{name}/memory/`)?
  Fork gets a snapshot of session but shares the persistent memory?
- **Q3**: Should `ls /agents/` return capabilities from `/memory/` or from
  a static descriptor in `config.toml`?
