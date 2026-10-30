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
before any send. The same native identity guard is shared by provisioning and
attachment; hostAudit version 1 is required.

Peer reception defaults to the configured exact-name allowlist, including when
the list is empty. For an explicitly selected network-wide installation,
`--init-codex --peer-policy node-namespace` admits untrusted peer data from that
verified Node namespace. It applies only after the actual owned session's
actor/SID/namespace audit succeeds, retains the same SID filter, bounded lane,
provenance wrapper and no-replay behavior, and does not grant send/tool authority.
Other authenticated Agents in that namespace can then supply peer context;
this is not a claim that they share an owner or are online. Fixed-session
callbacks and existing configs keep their exact-name allowlists. Directory
visibility alone never enables the wider policy.

Legacy responses retain the equal-cwd requirement. A trusted operator can now
select an actor-private ACP workspace by additionally returning `workspaceBinding`:

```json
{"version":1,"actor":"<same actor>","operationId":"<same operation>","threadId":"<same native root>","nativeCanonicalWorkspace":"<original canonical native cwd>","acpCanonicalWorkspace":"<operator-selected canonical ACP cwd>"}
```

This object belongs **inside** the successful response's `workspaceBinding`
field. All six fields must match the original claim and actual profile. It is
a response from the installed trusted local operator, not a peer assertion or
a cryptographic attestation. The version-2 binding freezes both contexts and
the operator response. A changed operation, actor, thread, native cwd, ACP cwd
or ACP SID fails closed. Existing imported bindings retain their original
contexts. Neither form grants access to an additional directory.

### Shared operator adapter

`src/codexEnrollment.ts` implements the operator boundary. It uses the existing
Nexus `ZoneApiService/MintAgent` path through the Nexus operator CLI; it does
not contain a CA loader, signer, registry, Node installer or process owner.
The new **client** command can target an older daemon that implements MintAgent.
Do not substitute the legacy `auth mint` CLI: its peer search/offline fallback
is unsuitable for this exact-endpoint operation.

Prepare explicit Node-side policy using the operator CLI's
`operator-agent-enroll --initialize --grant FILE --endpoint HTTPS_URI
--workspace-root DIRECTORY` command and the selected Box's `--data-dir`.
The [Nexus operator contract](https://github.com/nexi-lab/nexus-vfs/blob/main/docs/operator-agent-enrollment.md)
owns its Node identity/CA pins, workspace authorization and recovery semantics.
It issues nothing during preparation. Keep its grant and state in private
operator-controlled storage. A broader workspace subtree must be selected
explicitly; there is no implicit all-filesystem grant.

Then prepare the shared ACP adapter's private launch policy:

```text
scode-acp-bridge --init-codex-operator --config OPERATOR_POLICY
  --nexus-operator JSON_ARGV --scode-argv JSON_ARGV [--scode-env JSON_OBJECT]
```

`nexus-operator` names that exact operator executable and grant. `scode-argv`
is the owner's complete selected scode command ending in `acp`, including its
normal approval/tool policy. Optional environment values select normal scode
configuration and the existing Node record; never put Node/CA private keys in
them. The output contains `enrollmentArgv` for `--init-codex`. These two
initializers prepare private configuration only; they do not issue credentials,
register global MCP configuration or restart Codex.

At runtime, the adapter reserves the original actor/op/grant before invoking
the Node client, creates an actor-private workspace with its own `agentName`,
creates and persists one real ACP SID, checks `agent_list`'s native actor/SID/
Node-namespace audit, and closes that creator before returning its profile.
The original native project's `agentName` is never overwritten. No native
project directory is added to the ACP session by default. Any delegated
project access needs a separate explicit owner policy.

Missing/invalid SDK policy is `not_submitted` before reservation. After the
multi-stage operation is reserved, **every** failure is Unknown, including a
nested operator refusal, session creation failure, crash or lost public reply.
Only a completed public receipt may replay the original profile; an incomplete
operation requires explicit recovery using its original actor/op and saved
session, never a new name, operation or automatic second session. The bridge
does not auto-retry `recovery_required` bindings.

`test/operatorEnrollmentIntegration.ts` covers actual subprocesses with protocol
fixtures. `NEXUS_OPERATOR_BIN` plus its SHA/source in the common native fixture
selects `test/nativeEnrollment.cjs`, which exercises the operator client against
a private real mTLS daemon, same-project distinct identities, original SID
reload and actual A2A. Mock passes do not establish this real-Node acceptance,
global installation or native Codex new/resume composition. Deployment still
requires the selected binaries, policy and sole-owner migration to be qualified.
The fixture adapter under `test/fixtures` contains no Node/issuer/credential and
cannot fulfill those deployment gates.

## Durable state and recovery

The version-2 private binding file `<threadId>.json` contains the original actor,
operation, origin, native workspace, canonical workspace and state. A ready row
also freezes its original ACP session ID and original/canonical ACP workspace,
plus the optional explicit operator `workspaceBinding` described above.
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

Add `--managed-package-root /absolute/path/to/@openai/codex` to exercise the
official npm installation's ordinary daemon discovery. The same test first
prepares its private home through official start/stop, then launches ordinary
TUIs without a separately started server. It retains the original listener and
resume deadlines, configures and reads back a fixture-only one-second unload
delay, requires unload before actual resume, and verifies the bridge exits after
root retirement while the native daemon remains alive. The default test mode
continues to use an explicitly started isolated server. Neither mode selects a
production retirement policy or adopts a live daemon.

These tests do not establish authorized Node enrollment, real Node append or
cross-host receipt. Those require the supplied operator adapter and an exact
CLI/daemon pair on Mac and Windows. Keep active sessions and the shared Node
running until the migration coordinator announces a window. Preserve old
versioned files/configuration for rollback; never recreate a global resolver or
per-worktree scode launcher as part of this attachment work.
