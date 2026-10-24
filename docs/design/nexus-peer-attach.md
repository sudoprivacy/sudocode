# Nexus peer attach: one peer namespace per machine

Status: in progress. S1 (rename), S2 (one peer namespace) and federated attach are implemented. S3, S4, S5 and S6 are designed but not built. The live coordination record for the nexus side is kept outside the repo.

## Goal

Every machine has exactly one place its agents meet. Agents on the same machine and agents on other machines are reached through the same `agent_list` and `send`. A session attaches to that place without hand-set environment variables or restarts.

Target topology:
- One Nexus node per machine. The Windows node is the only data voter; the macOS node joins as a learner.
- Four agents (two Codex-driven sessions plus two interactive `scode` sessions) share one namespace.

## Where a session's peers are

The peer store depends on one durable fact: whether the machine publishes a node record (`<home>/.nexus/run/node.json`). The node owner writes it; `scode` only reads it.

| Machine state | Peer store | When the store is unusable |
|---|---|---|
| No node record | Same-machine pair root on disk (`local_pair_root()`) | n/a |
| Node record present | The machine's node | Error. Never falls back to disk. |

`engine_host::peer::attach` is the only place this decision is made. The session build uses it for:
- the system-prompt identity section;
- the tool scope, so that `send` and `agent_list` see one mailbox;
- the host shell's single receiver.
- the authenticated filesystem for Nexus model mounts. A `nexus:///mount`
  provider route sends model writes through the node's existing
  hooks. Local workspace files and history keep their local filesystem.

A co-hosted agent never calls it, because its daemon injects the mailbox it lives in.

There are no namespace qualifiers. `agent_list` returns bare names in `send_to`. `send` rejects `:`, `/`, `\` and `@` in a recipient, so a stale qualified name can't become a path segment.

## Federated attach

The node owner's bring-up publishes `<home>/.nexus/run/node.json` atomically once the node's TLS listener, agent authentication and subtree mounts are verified. `<home>` is `HOME`, else `USERPROFILE` (`runtime::user_paths::home_dir`). There is no other switch: no environment variable, no working-directory lookup, no per-agent override.

| Field | Meaning |
|---|---|
| `version` | `1`. Any other value is an error. |
| `endpoint` | `host:port` dialled over mTLS; no scheme. |
| `agents_dir` | Absolute directory holding one credential bundle per agent name. |
| `role`, `zone` | The node's deployment role and zone; `scode` does not read them. |

A session attaches as its own resolved agent name (the `agentName` setting, else the name derived from its workspace):

1. It loads `<agents_dir>/<name>/credential.json`. The manifest's `agent` must equal `<name>`.
2. It dials `endpoint` with that credential. The server certificate is checked against the bundle's `server_name` and CA, not against the dial address.
3. It reads `/agents` once. The client connects lazily, so without this probe an unreachable node, a failed handshake or a missing mount would only surface on the session's first message.

Every failure names its cause and stops the session: an unreadable or malformed record, another version, an endpoint with a scheme, a relative `agents_dir`, no credential for this agent, a credential minted for another agent, or a failed probe. None of them falls back to disk. Credentials are minted by the node owner; `scode` never mints one.

The attach decision is cached per agent name for the life of the process, so the session build and its receiver cannot disagree, and a host running sessions under several identities dials each with its own credential.

## Rename (S1)

`KernelFsBackend::rename` and `NexusVfsFsBackend::rename` call the node's own `sys_rename` / gRPC `Rename`.
- A file is replaced in one step.
- The node refuses streams, pipes and cross-mount moves, and the refusal is returned unchanged. Nothing is emulated with copy + delete, because that would silently turn a DT_STREAM into a regular file.
- `NexusVfsFsBackend::write_atomic` is a direct write, because the node's `sys_write` already replaces the object in one call.

## Receiver ownership

Every receive path uses one loop: `runtime::mailbox::spawn_inbox_poller`. Delivery is at-least-once, and the read position is committed only after the sink accepts a message.

- **Disk.** `StdFsBackend::try_lock` takes an OS file lock that is released only when its handle closes. Its renewal always succeeds.
  - A frozen holder keeps the lock, so there is no handoff and the cursor can't move backwards.
  - Handoff happens only when the holder exits, and a dead process can't write.
  - The one premise is that a held `.lock` file is never unlinked or replaced while held. S3 depends on this.
- **Node.** The lock is a renewable lease.
  - A renewal thread independent of the sink keeps a hung-but-alive holder alive, so a slow model turn never causes a handoff.
  - A whole-process freeze longer than the lease can hand off. That opens the window S6 closes.
- **One receiver per agent.** A receiver tails nothing until it holds its agent's receiver seat, `agents/<name>/receiver.lock`, taken with `FsBackend::try_lock` on the same backend as the conversations.
  - Until then it stands by and retries once per discovery interval, so a second instance of the agent takes over when the first stops, resuming at the read position.
  - Losing the seat stops every tail before the receiver stands by again.
  - Each conversation's reader seat stays as the guard on that conversation's read position, so a tail finishing a delivery while the agent seat changes hands can't be read twice.
  - On the node the seat is unique only within one machine until nexus routes locks by zone (N1).
  - `/status` and ACP's `_meta.sudocode.peer` report the agent, its namespace and whether this process is receiving, standing by, or running no receiver.
  - An ACP process runs one receiver per agent it serves, on the mailbox that agent's tools send through: the hosted agent's, else the namespace the session attached to, which is the pair root when the machine has no node. A message goes to a session of the agent it was sent to, rebinding to another session of that agent if the first closes; the receiver stops, freeing the seat, once no session of the agent is left.
  - The synchronous REPL runs the same receiver. Waiting messages run as turns before each prompt; rustyline cannot be interrupted from another thread, so a message arriving while the prompt waits is announced at once, and an empty Enter hands the prompt back so it runs. Until its turn has run, a message stays unread. `/status` and the peer status's `delivery` field (`next_prompt`, against `automatic` elsewhere) say so; an Enter pressed by a person is not automatic delivery, which the async REPL and ACP sessions provide.

## Cursor fencing (S6)

The contract and the consumer side are in place (`runtime/src/cursor_fence.rs`). The node implementation waits on N2, a storage-level conditional write.

```
FsBackend::read_versioned(path) -> io::Result<Option<(Vec<u8>, VersionToken)>>
FsBackend::write_if_version(path, expected: Option<&VersionToken>, data) -> io::Result<WriteOutcome>
WriteOutcome = Committed(VersionToken) | Conflict { current: Option<VersionToken> } | Unknown
```

- **Token.** The storage returns an opaque token. sudocode never interprets it as a content generation or a metadata version.
- **`expected = None`** means "must not exist".
- **Conflict** means nothing visible changed.
- **Unknown** means the request was sent but the commit result is not known.
- **No conditional write.** A backend without an atomic conditional write returns `ErrorKind::Unsupported`.

Which path a reader uses is decided once, when it acquires the reader, by `FsBackend::fenced_locks()` and then by whether `read_versioned` is supported:

1. **`true` (disk).** The holder's plain store is already isolated, so it stays a plain store.
2. **`false`, conditional write supported.** The new owner claims the cursor first:
   - It reads the register (token T0), then writes a fence `{holder, epoch+1, read_offset}` with `expected = T0`.
   - Only after `Committed` does it read or deliver.
   - On `Unknown`, it re-reads and proceeds only if the register shows its own fence.
   - Every commit carries the last token.
   - `Conflict` means the same thing to every actor, whether a new claimer or a holder that resumed after a freeze: this attempt failed and nothing visible changed. The actor stops delivering, leaves the message unacknowledged, and goes back through acquire and claim. No actor ever gets to force a write.
   - It never falls back to an unconditional write.
3. **`false`, no conditional write (the node until N2).** The reader keeps the plain store, and status states the weaker guarantee: "at-least-once only; the cursor may move backwards after a freeze longer than the lease". This is a declared capability gap, not a retry after a failed conditional write.

The decision is reported as `PeerStatus.cursor` (the `/status` Cursor line and ACP `_meta.sudocode.peer.cursor`): `fenced`, `fenced_cas` or `at_least_once`, or `none` until this process has claimed one of the agent's conversations. On the conditional path the register carries the claim's `epoch`; the plain store writes the register exactly as before.

`fenced_cas` holds only while every writer of a conversation's register fences. A build without S6, or a claim that found the backend answering `Unsupported` (for example a server not yet upgraded), writes unconditionally, so a deployment must not mix those with fencing writers and count on the guarantee. A register's stored epoch is input, not proof: a claim whose next epoch would overflow fails without writing.

**Node adaptation (S6b, after N2).** The node needs a new RPC, not new fields on `Write`: an older server ignores unknown proto3 fields and would turn the write into an unconditional one. Its minimal contract:
- `WriteIfVersion(path, content, precondition: if_gen | if_absent)` answers `Committed{gen}` or `Conflict{current_gen?}`. An older server answers UNIMPLEMENTED, which maps to `Unsupported`.
- The compare and the commit are one linearizable step in the zone. Content and `gen` become visible together, and a conflict changes nothing.
- `gen` increases strictly on every committed write and is never reused, even across delete and recreate.
- `Read` returns content and `gen` from the same committed version.
- A timeout after the request was sent is `Unknown` on the client, which settles it by reading back.

`NexusVfsFsBackend` then implements the two methods, and the receivers switch to `fenced_cas` without other changes.

## Disk → node switch (S3)

A one-time, explicit operation per machine:

1. **Version gate.** Refuse while older `scode` processes are running.
2. **Fence.** Take the leaf fence lock on the disk root, then try-lock every reader `.lock` under it. If any is held, refuse; never move a held lock file.
3. **Record the fact.** Write the durable "node enabled" fact.
4. **Archive.** Move the disk root aside read-only and leave a placeholder file at its path, so an un-upgraded binary fails loudly instead of writing to a namespace nobody reads.
5. **Drain.** A node-mode session drains its archived disk inbox once, within a bound, at-least-once. Unread messages are reported. Undelivered messages are never classified as abandoned.

Streams are never migrated. Native rename refuses them, and the node re-stamps `from` with the writer's identity. Archived history stays readable on disk.

## Attaching to the loopback node (S4) — open decision

The supervisor runs `serve-local` with Open auth on a private loopback port. Its capability channel is documented as carrying "capability calls, never kernel credentials". With Open auth, the node address is itself a credential.

| Option | How | Trade-off |
|---|---|---|
| (a), recommended | `service.attach` returns the node endpoint, only when `role=none` | Any local process can already reach that port, so this does not weaken the current posture. The capability documentation must name the exception. |
| (b) | Mailbox operations go through the supervisor as a `mailbox.*` capability domain | The boundary stays intact, but every operation adds an HTTP hop and blocking tails must be built in the supervisor. |

## Certificates (S5)

When `role` is founder or joiner, the agent certificate is checked before dialing. The certificate's subject alternative name must equal the resolved agent name, the certificate must chain to the cluster CA, it must not have expired, and the key must match. Each failure has its own explicit error.

Re-minting is never done implicitly. `allow_existing` is an administrator recovery tool, and an uncertain mint result is an error plus a manual runbook.

## Nexus dependencies

| Item | What | Blocks |
|---|---|---|
| N1 | Route VFS locks by mount/zone | Cross-machine uniqueness of the receiver lease |
| N2 | Storage-level conditional write. A failed attempt leaves visible content unchanged, and the storage returns the token. | S6 on the node |
| N3 | CSR + idempotent mint | Retrying an uncertain mint |
| N4 / N5 | Node / agent certificate renewal | Long-lived federation |
| N6 | Identity-provisioning injection for co-hosted agents | Co-hosted agents and CLI sessions sharing one identity across machines |

The learner bootstrap uses existing commands:
1. `enroll`
2. `offline join --as learner` on the whole-zone mount
3. A daemon resume, which restores the `/agents`, `/conversations` and `/sessions` subtrees.

Membership is verified against the real ConfState, including that a restart does not promote the learner.

## Out of scope this round

- Certificate renewal.
- Importing disk messages into the node.
- Exactly-once delivery to the sink.
- Cross-machine receiver uniqueness before N1.
- A node-side conditional cursor before N2.
