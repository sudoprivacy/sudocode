# Unified Agent/PID Tool Design

> Status: **design draft** — not yet implemented.
> Context: [agent-context-storage-matrix](./agent-context-storage-matrix.html) §identity.

## Problem

sudocode exposes two parallel agent-communication systems to the LLM:

1. **Sub-agent (coordinator mode)** — `SendMessage` tool, local JSONL
   mailbox, broadcast + structured shutdown + live abort.
2. **A2A (managed-agent)** — `send_message` tool, nexus gRPC DT_STREAM,
   minimal schema (`to` + `body`).

Both exist because they evolved independently. From the LLM's perspective
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

Three IDs: `agent-name` (durable principal) · `session-id` (durable UUID,
`--resume` key, spans N pids) · `pid` (ephemeral runtime handle).

## Unified LLM tool set (agent/pid dimension split)

| Tool | Target | Description |
|------|--------|-------------|
| `ls /agents/` | agent | Discovery — who exists, what they're good at |
| `spawn` | agent → produces pid | Create a new run (`start_session`) |
| `send-message` | pid `chat-with-me` or agent durable inbox | Send message (transport auto-routed) |
| `stop` / `kill` | pid | Terminate a specific run; agent unaffected |
| `task-output` | pid | Retrieve a run's result |
| `get-status` | pid | Query run state (READY/BUSY/TERMINATED) |

**Key constraints:**

- `spawn` cannot target a pid — pid is the *output* of spawn, not an input.
- `stop` cannot target an agent — agent is a durable identity; the LLM has
  no authority to delete it.
- `send-message` is one tool; the transport layer auto-routes (local
  sub-agent → file mailbox; nexus co-host → gRPC DT_STREAM).

## Three spawn modes

| Mode | Description | nexus mapping | Use case |
|------|-------------|---------------|----------|
| **a. spawn from image** | Start another agent's pid | `start_session(agent_id="other")` | Delegate to a persistent specialist |
| **b. fork current** | Fork current agent, preserving session | `start_session(agent_id=self, session=current)` | Parallel subtask needing context |
| **c. spawn fresh** | Same agent type, no session | `start_session(agent_id=self, session=None)` | Stateless task (search, verification) |

Mode c is what the current `Agent` tool does today.  Mode b (fork) is the
hardest — requires copy-on-write semantics (share the session transcript
but diverge after fork).  nexus DT_LINK naturally supports this.

## Scenario coverage

| Scenario | Operation | Result |
|----------|-----------|--------|
| Parallel (needs context) | fork → pid-1, pid-2 | Both pids share session context |
| Parallel (stateless) | spawn fresh → pid-1, pid-2 | Clean context window |
| Delegate to expert | send-message → agent durable inbox | Expert's pid consumes |
| Cancel parallel worker | kill(pid-1) | pid destroyed, agent unaffected |
| Cancel delegation | kill(expert's task pid) | Expert agent stays online |

## Migration from current tools

| Current | Unified | Notes |
|---------|---------|-------|
| `SendMessage` + `send_message` | `send-message` | Superset schema, transport auto-routed |
| `Agent` tool | `spawn` (modes a, c) + `fork` (mode b) | Split by session semantics |
| `TaskStop` | `kill` | Target is explicitly a pid |
| `TaskGet` / `TaskList` | `get-status` | Target is explicitly a pid |
| `TaskOutput` | `task-output` | Target is explicitly a pid |
| *(new)* | `ls /agents/` | Agent discovery |

## Open questions

- **Q1**: Should `send-message` to an agent name (not a pid) auto-spawn a
  pid, or require explicit spawn first?
- **Q2**: How does fork interact with session-id?  Does the forked pid get
  a new session-id or share the parent's?
- **Q3**: Should `ls /agents/` return capabilities from `/memory/` or from
  a static descriptor in `config.toml`?
