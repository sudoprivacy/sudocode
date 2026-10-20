# Nexus peer attach: one peer namespace per machine

Status: in progress. S1 and S2 are implemented. S3, S4, S5 and S6 are designed but not built. The live coordination record for the nexus side is kept outside the repo.

## Goal

Every machine has exactly one place its agents meet. Agents on the same machine and agents on other machines are reached through the same `agent_list` and `send`. A session attaches to that place without hand-set environment variables or restarts.

Target topology:
- One Nexus node per machine. The Windows node is the only data voter; the macOS node joins as a learner.
- Four agents (two Codex-driven sessions plus two interactive `scode` sessions) share one namespace.

## Where a session's peers are

The peer store depends on one durable fact, whether the node is enabled for the machine. Only the explicit disk→node switch (S3) writes that fact.

| Machine state | Peer store | When the store is unreachable |
|---|---|---|
| Node never enabled | Same-machine pair root on disk (`local_pair_root()`) | n/a |
| Node enabled, `role=none` | The supervisor's loopback `serve-local` node | Error. Never falls back to disk. |
| `role=founder` / `role=joiner` | The federated node | Error. Never falls back to disk. |

`engine_host::peer::attach` is the only place this decision is made. The session build uses it for three things:
- the system-prompt identity section;
- the tool scope, so that `send` and `agent_list` see one mailbox;
- the host shell's single receiver.

A co-hosted agent never calls it, because its daemon injects the mailbox it lives in.

There are no namespace qualifiers. `agent_list` returns bare names in `send_to`. `send` rejects `:`, `/`, `\` and `@` in a recipient, so a stale qualified name can't become a path segment.

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
- **One receiver per identity (PR-D).** An identity-level lease at `/agents/<name>/receiver`, built on `FsBackend::try_lock`, replaces per-conversation holder fields.
  - It is unique only within one machine until nexus routes locks by zone (N1).
  - PR-D also brings receiving to the synchronous REPL and to non-hosted ACP sessions.

## Cursor fencing (S6)

The contract is fixed now. The node implementation waits on N2, a storage-level conditional write.

```
write_if_version(path, expected: Option<Token>, data) -> Result<Outcome, io::Error>
Outcome = Committed(Token) | Conflict { current: Option<Token> } | Unknown
```

- **Token.** The storage returns an opaque token. sudocode never interprets it as a content generation or a metadata version.
- **`expected = None`** means "must not exist".
- **Conflict** means nothing visible changed.
- **Unknown** means the request was sent but the commit result is not known.
- **No conditional write.** A backend without an atomic conditional write returns `ErrorKind::Unsupported`.

Which path a reader uses is decided once, when it acquires the reader, by `FsLock::fences_writes()`:

1. **`true` (disk).** The holder's plain store is already isolated, so it stays a plain store.
2. **`false`, conditional write supported.** The new owner claims the cursor first:
   - It reads the register (token T0), then writes a fence `{holder, epoch+1, read_offset}` with `expected = T0`.
   - Only after `Committed` does it read or deliver.
   - On `Conflict`, it re-reads and either claims again or yields.
   - On `Unknown`, it re-reads and proceeds only if the register shows its own fence.
   - Every commit carries the last token. A `Conflict` on commit means ownership was lost: delivery stops and the message stays unacknowledged.
   - It never falls back to an unconditional write.
3. **`false`, no conditional write (the node until N2).** The reader keeps the plain store, and status states the weaker guarantee: "at-least-once only; the cursor may move backwards after a freeze longer than the lease". This is a declared capability gap, not a retry after a failed conditional write.

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
