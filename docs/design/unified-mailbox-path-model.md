# Unify the mailbox path model: one `PerRecipient { root }` shape + `SharedStream`

**Status:** design for review (implementation lands as follow-up commits on this branch).
**Scope:** `runtime` mailbox layer + its standalone REPL wiring. Aligns the on-disk
mailbox path model with the Nexus end-state in
`docs/design/agent-context-storage-matrix.html` (A2A row: everything converges on
`/agents/{name}/chat-with-me`).

## Problem

Agent-to-agent messaging today has **one envelope, one send/poll code path, one
`chat-with-me` leaf** — but **two different path shapes**, split by
`InboxConvention`:

- `LocalJsonl { root }` → `{root}/.sudocode-inbox/{name}.jsonl`
- `NexusA2a` → `/agents/{name}/chat-with-me`
- `SharedStream { path }` → one stream all names resolve to (managed-agent `/proc/{pid}/chat-with-me`)

The `.sudocode-inbox/{name}.jsonl` shape is a standalone-only historical form. It
causes two concrete gaps for standalone (no-daemon) usage:

1. **Same-machine pair can't message.** The REPL's local poller polls a hardcoded
   name (`team-lead`) rooted at `current_dir()`, and `send`'s fallback writes to
   *its own* workspace `.sudocode-inbox/`. Two scode processes started in
   different folders never share a path, so a message from A never reaches B.
2. **Two path builders for one concept.** `LocalJsonl` and `NexusA2a` are the same
   *per-recipient inbox* idea with different roots, but they're spelled as two enum
   variants with two `format!`s — a DRY/SSOT seam that has already drifted.

## Design

Collapse the three conventions to **two**, matching their real semantics:

```rust
enum InboxConvention {
    /// Per-recipient inbox: `{root}/agents/{name}/chat-with-me`.
    /// root = "" over nexus (daemon-absolute /agents/...), a host dir standalone.
    PerRecipient { root: String },
    /// One stream both parties read+write, each filtering its own writes
    /// (managed-agent /proc/{pid}/chat-with-me). Semantically distinct: NOT
    /// keyed by recipient, so it stays its own variant.
    SharedStream { path: String },
}
```

- **One path shape** `{root}/agents/{name}/chat-with-me` for every per-recipient
  inbox. `NexusA2a` becomes `PerRecipient { root: "" }`. `.sudocode-inbox/*.jsonl`
  retires.
- **`root` is the standalone analog of a Nexus zone** — the access/isolation
  prefix. It takes three values by scenario:
  - standalone same-machine pair → `~/.nexus/sudocode/local-mailbox` (shared, so
    two folders converse)
  - coordinator ↔ sub-agent → `<workspace>` (**per-workspace, preserved** — a
    sub-agent belongs to its parent scode; different scodes' sub-agents must not
    cross-talk)
  - nexus → `""` (kernel prepends the real zone)
- **Framing stays a backend concern** (unchanged). `StdFsBackend` keeps
  newline-delimited JSONL (human-readable, greppable); nexus keeps DT_STREAM
  frames. `Mailbox` asks `backend.is_append_stream(path)` and picks the framing —
  this is the correct abstraction seam, not a duplication. We are unifying the
  **path model**, not the physical byte format.

### SSOT additions

- `local_agent_name()` — resolves this process's mailbox identity: config
  `agentName` (Settings scope, per-project) if set, else the current folder
  basename. Both `send` and the REPL poller read this one function, so they cannot
  drift.
- `local_pair_root()` — the shared standalone-pair root
  (`~/.nexus/sudocode/local-mailbox`).
- `agentName` added to the `/config set` allowlist (Settings scope, string), so
  the name is runtime-adjustable and persisted (takes effect next launch).

## Why not go further (bounded on purpose)

- **Not** forcing `StdFsBackend` into binary framed streams — that trades away
  readable/greppable local mailbox files for zero user-visible benefit, and it
  does not fix any real bug (JSON escapes newlines, so JSONL-by-line is safe).
- **Not** moving coordinator/sub-agent to a global root — standalone has no Nexus
  zone, so per-workspace root *is* the isolation. Global would cross-talk between
  projects.
- The full "route coordinator/subagent/memory through `FsBackend`" item (matrix
  Q8) needs the Nexus kernel and is a separate, cross-repo change. This PR aligns
  the **logical path model** so that later swap is a root+backend change, not a
  reshape.

## Commit plan

1. SSOT: `local_agent_name()` + `local_pair_root()` + `agentName` config setting.
2. Refactor `InboxConvention` → `PerRecipient { root }` + `SharedStream`; unify
   `inbox_path`; update `nexus_a2a`, `spawn_task` construction. Behavior-preserving
   for nexus; path shape changes for local. Tests updated.
3. Retire `.sudocode-inbox/*.jsonl`: `coordinator_notification` + `send` fallback
   build `PerRecipient` paths (coordinator root stays `<workspace>`).
4. REPL wiring: poller + send use `local_agent_name()` / `local_pair_root()`;
   drop the hardcoded `team-lead` / `current_dir()`.
5. Tests: PTY same-machine pair exchange over `~/.nexus/sudocode/local-mailbox`;
   coordinator regression (per-workspace isolation intact).
