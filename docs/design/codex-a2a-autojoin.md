# Native Codex attachment through the shared ACP bridge

This is the candidate implementation contract. Rollout ownership and final
acceptance remain in the [integration handoff, draft #1101](https://github.com/sudoprivacy/sudocode/pull/1101).
In particular, this candidate's loaded-runtime lifetime must be reconciled with
that handoff's frontend-retirement requirement before installation. It does not
claim that retained inventory proves frontend presence.

The intended user workflow is one machine setup followed by ordinary new or
resumed Codex sessions in any trusted workspace. No model prompt is needed to
join. Existing workspace instructions, approvals, model settings, Node ownership
and Agent credentials remain under their existing owners.

## Ownership and routing

Native Codex 0.163.0 starts an MCP process for each root. A real two-TUI test
shows that initialize has neither a thread ID nor `CODEX_THREAD_ID`; same-cwd
roots have identical startup contexts. Binding by cwd, inherited environment,
PID or a static profile therefore cannot distinguish these roots.

All global stdio entries connect to one authenticated loopback owner. Exclusive
TCP binding elects it; a private random token and the exact config hash prevent
an entry from adopting another configuration on that port. Each entry forwards
the standard MCP stream; it has no Agent receiver. The owner reuses
`connectCodex`, `connectScode`, `registerScodeMcpTools`, the direct A2A helpers and
the existing callback implementation. There is one native observer and one
scode connection per Agent. No Nexus daemon, CA or credential issuer is added.

Inventory is read through the existing native proxy. A qualifying thread has
`threadSource=user`, `parentThreadId=null`, `ephemeral=false`,
`canAcceptDirectInput=true` and an idle/active status. Missing fields fail closed.
Subagents, guardian reviews and feature workers are excluded. `source=cli` is
not a root discriminator: actual CLI roots can report `source=vscode`.

The configured native endpoint must already be discoverable through the native
proxy. The private fixture explicitly starts its own server; that does not
prove normal embedded CLI startup is discoverable. Qualification of the native
daemon-management entry and actual production launch modes remains a rollout
gate. The config initializer only prepares a global MCP command, not a complete
installer or a native daemon migration.

Tools select the binding using native host `_meta.threadId`, never a sender or
thread argument supplied to the tool. Each invocation refreshes native inventory
before selecting the authenticated connection, so a resumed root's first call
waits for its existing binding to load. This does not retry a task or send.
Real-native acceptance deliberately
passes a conflicting thread ID to `mcpServer/tool/call`; the host overwrites it
and each resulting native audit identifies the correct Agent. Outgoing command
audits retain that native-thread correlation. The shared send path keeps its
real output, policy, approvals, actor audit and conservative Unknown semantics.

Ownership follows **loaded native root runtimes**, including work that outlives
a frontend. This does not establish whether a TUI window is currently visible.
An unloaded root closes its owned scode; loading it again uses the saved profile.
When the native proxy exits, all of this owner's scode connections close. After
the last MCP connection closes, the shared owner refreshes inventory and exits
only when no qualified root remains. Unavailable inventory keeps the owner
alive and tools unavailable; it does not establish that roots have exited.
It never stops the native server or the Nexus Node.

## Existing enrollment capability

`enrollmentArgv` is an explicit operator-owned executable/argv, run without a
shell. Its stdin receives exactly one public claim:

```json
{
  "version": 1,
  "actor": "codex-<random 32 hex digits>",
  "operationId": "<stable UUID>",
  "threadId": "<native UUID>",
  "workspace": "<actual native absolute cwd>"
}
```

It returns one bounded JSON response, then exits:

```json
{"actor":"<same actor>","configFile":"<absolute private ACP profile>"}
```

Before any issuance is submitted it may instead return
`{"outcome":"not_submitted","detail":"..."}`. The bridge can then retry the
**same** saved actor/operation after a bounded delay, allowing an offline node
to recover automatically. `{"outcome":"unknown","detail":"..."}`, an untyped
failure, malformed response or a lost response requires operator recovery.
The adapter must never label a possibly submitted issuance `not_submitted`.

The adapter must use the installation's existing authenticated claim/issuance
authority. The adapter owns authorized enrollment, an explicitly selected Agent
context, and creation/persistence of that Agent's ACP session before returning.
Its profile contains the actual workspace, full scode argv and an **existing**
`acpSessionId`. It must not return Node or CA authority to the bridge. It must not
use `allow_existing`, silently rename/remint after an uncertain request, or treat
directory visibility as proof of successful issuance. Credential files remain
in the existing Agent credential store; the bridge persists only binding and
profile references.

Before accepting a binding, the bridge executes the real `agent_list` helper
and requires native audit to match the intended Agent and ACP session, with the
Node namespace root (`""`). A copied profile or workspace-disk namespace fails
before any send. Peer callbacks remain subject to the configured peer allowlist.

The bridge provides this adapter boundary, not a second issuer. Deployment
requires the Node owner to supply and qualify an actual authorized adapter.
The committed adapter under `test/fixtures` is strictly a protocol fixture; it
has no Node, credential or issuer and cannot fulfill that deployment gate.

## Durable state and recovery

The version-2 private binding file `<threadId>.json` contains the original actor,
operation, origin, native workspace, canonical workspace and state. A ready row
also freezes its original ACP session ID and original/canonical ACP workspace.
The original enrollment workspace
bytes stay unchanged; canonical filesystem resolution accepts aliases such as
macOS `/var` and `/private/var` across unload/resume. The frozen canonical path
also rejects an alias redirected to another workspace.
The shared inventory refresh applies the same workspace check to cached
connections before admitting another tool, not only during first attachment.
Older preview bindings require explicit operator migration after verifying the
original identity and both contexts; the bridge never guesses their original
SID or replaces their identity or enrollment operation. Changing a ready
profile's SID or redirecting its workspace is refused even for a cached
connection with the same actor.
A new random actor is independent of the native thread ID,
session ID and profile. The original claim is written and synced before invoking
the adapter. Atomic replacement publishes ready profile references. POSIX also
syncs the containing directory; this does not claim equivalent directory-sync
durability on Windows or power-loss acceptance.

| State | Meaning | Next action |
|---|---|---|
| pending | No issuance submitted, including a typed offline refusal | Retry the same actor/op after a bounded delay |
| provisioning | Original operation saved; adapter may be in flight | No second operation |
| ready | Exact existing profile selected | Load its ACP session; never create a replacement |
| recovery_required | Issuance outcome or response is uncertain | Operator resolves the original actor/op; no automatic retry or remint |

A restarted owner turns an interrupted provisioning record into
`recovery_required`. A failed ready profile keeps its actor/profile and reports
disconnected. Refresh, unload/reload and restart do not replay sends. Logs and
status distinguish attachment from directory visibility; an unavailable native
inventory prevents tool admission. Callback errors remain pending/Unknown and
are not automatically replayed.

For an existing fixed-session Agent, prepare a private mapping document with
exactly `threadId`, `workspace` (native cwd), `actor`, `configFile` (existing ACP
profile), and `acpSessionId` (original SID). Run the common package's
`--bind-codex --config GLOBAL_FILE --binding MAPPING_FILE` while the global bridge
owner is stopped. This is an offline preparation operation: it starts neither
native nor ACP sessions, performs no enrollment, and returns
`prepared:true, startedAgent:false`. Its exclusive endpoint is the same one
used by the runtime, so it cannot prepare against a running global owner.

An explicit existing mapping can retain deliberately different native and ACP
workspaces. Both canonical paths and the original ACP SID are frozen separately;
new enrollments still require their claimed native and ACP workspace to match.
The same actor cannot be assigned to two threads, and an existing row cannot be
overwritten, including by repeating an import. Preview-state recovery needs
separate operator verification; import never repairs or overwrites it.

Offline preparation does not prove the actor's current credentials or retire a
legacy receiver. At the coordinated window, let the previous fixed owner exit
naturally before the global owner loads its profile. The normal authenticated
`agent_list` audit then verifies actor/SID/Node namespace before sends become
available. Merely installing the global entry does not retire a private
launch-time override or old receiver.

## Validation and activation

`npm test` runs real proxy/ACP processes for root filtering, same-cwd identity
separation, unload/reload, owner restart, ambiguous enrollment, wrong-actor
rejection, original SID/workspace substitution refusal, offline import and owner
retirement, exact direct sends, absence of inner prompts and absence of retries.
Offline package tests verify exclusive private configuration and a global MCP
registration command without installing it.

`npm run test:codex-lifecycle -- --codex /absolute/native --output /private/evidence
--autojoin` uses two actual native TUIs in an isolated Codex home. Their normal
MCP startup elects one owner, creates two fixture-bound Agents, and routes real
native MCP calls independently with zero model requests. Use `--case
cancelled-resume` or `--case completed-resume` for retained-history acceptance.
`--mcp-context` separately records the bounded actual initialize context, without
reading credentials. A loopback mock supplies any explicit seed response.

These tests do not establish authorized Node enrollment, real Node append or
cross-host receipt. Those require the supplied operator adapter and an exact
CLI/daemon pair on Mac and Windows. Keep active sessions and the shared Node
running until the migration coordinator announces a window. Preserve old
versioned files/configuration for rollback; never recreate a global resolver or
per-worktree scode launcher as part of this attachment work.
