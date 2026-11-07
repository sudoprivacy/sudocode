# Codex network autojoin

## Acceptance target

After one installation and authorization on each machine, ordinary Codex startup
and resume in every workspace join the configured Nexus network without a join
prompt or an additional model request. Restoring an existing Codex conversation
preserves its Agent identity and history. Separate concurrently owned Agents do
not share a receiver or transcript writer. This includes real reception while
the original Codex thread is idle; having tools in its menu is insufficient.

This document records the source investigation and implementation boundaries.
It does not claim that this mode is implemented or installed. The current
fixed-session bridge and the live six-peer deployment retain their behavior.

The first code slice extracts that callback's native connection into the shared
`connectCodex` API. Inventory and notification consumers can use the same
transport implementation. Process integrations cover the real stdio proxy
boundary, installed-package exports, existing callback routing and response
loss. This connection helper supplies no enrollment or automatic discovery;
the admission and lifecycle gates below still apply.

## Reuse the existing layers

- `sdk/typescript` owns the portable ACP client, direct send/list, MCP host,
  external-command audit and optional Codex callback. Applications import that
  implementation. The autojoin adapter must use these same operations.
- Codex user-level MCP configuration starts the adapter in every workspace.
  Project configuration and retained thread overrides have higher precedence;
  installation must inspect conflicting entries rather than assume a global
  entry replaced them. No Codex executable wrapper is required for MCP startup.
- The existing callback attaches to an already-loaded native app-server thread.
  Its idle `turn/start` and active `turn/steer` paths remain the only notification
  delivery implementation. It never resumes, forks or creates a shadow thread.
- Nexus preparation belongs to scode's existing shared-service owner. The
  [setup/recovery slice](https://github.com/sudoprivacy/sudocode/pull/1071) selects
  an existing Box and can recover explicitly registered local ownership. ACP and
  headless startup do not implicitly provision services. Machine connection,
  process ownership and Agent authorization remain separate facts.

## Native Codex mechanisms and a concrete timing limit

The official [MCP guide](https://learn.chatgpt.com/docs/extend/mcp?surface=cli)
documents per-user stdio MCP configuration. The
[configuration guide](https://learn.chatgpt.com/docs/config-file/config-basic)
defines user/project precedence. The [hooks guide](https://learn.chatgpt.com/docs/hooks)
documents `SessionStart`, startup/resume sources, native session/cwd input and
the one-time review of non-managed hook definitions.

Source inspection at local Codex commit
`970111f6cb62cc02fe36a82ea7d71b37a2889c97` establishes two narrower facts:

1. `core/src/hook_mcp_executor.rs` overwrites request metadata `threadId` with
   the host's thread ID and calls the existing MCP runtime with
   `wait_for_server: true`. Hook argument `session_id` is not itself proof of
   sender identity. MCP tool calls must select their binding from native context,
   never from a model-selected sender or a target thread in tool arguments.
2. `core/src/hook_runtime.rs::run_pending_session_start_hooks` is called from
   `core/src/session/turn.rs`. In that source, a pending startup hook is consumed
   inside a turn. A hook-only design therefore does not prove automatic attach
   of a newly loaded idle thread before its first turn.

The installed Mac CLI reports `0.163.0`; that version string alone does not prove
its bytes equal the inspected checkout. Its earlier source-bound native protocol
receipt proves the existing namespaced callback shape. New lifecycle behavior
still needs an actual installed-binary acceptance test.

Use native MCP startup for process launch and the existing app-server adapter
for authoritative loaded-thread binding. A hook can supply or refresh context,
but must not be the sole source of an idle-start guarantee. Do not guess a thread
from the newest transcript, process age, workspace basename, or an inherited
`CODEX_THREAD_ID`; the local MCP environment builder does not establish a fresh
thread identity. The native protocol includes `thread/loaded/list`,
`thread/started` and thread status events, but their visibility and ordering on
the existing proxy must be proven before selecting a discovery strategy. A
protocol name alone is not proof that a connection receives all lifecycle events.

A read-only check of the existing Mac native `0.163.0` endpoint succeeded with
`initialize`, `thread/loaded/list` and `thread/read(includeTurns:false)`. It
returned the original active root thread and its expected workspace; no new
thread, turn, model request or scode process was started. That endpoint contained
one loaded thread. The standard per-user control socket was absent, while this
installation used an explicitly configured private socket. These observations
prove the existing binding, not universal discovery. In the inspected CLI source,
`app-server proxy` without `--sock` only selects the standard socket and connects;
it does not discover every independently embedded or custom-socket frontend.
An installer must therefore qualify native endpoint ownership and discovery for
its supported Codex launch mode before claiming that restart covers every root.

The opt-in [native lifecycle fixture](../../sdk/typescript/README.md#opt-in-native-codex-lifecycle-fixture)
now exercises actual CLI processes through the shared connection helper. On Mac
`0.163.0`, an isolated default endpoint exposed two distinct same-workspace idle
roots before a model request. Resume restored the original ID, canonical cwd and
visible cancelled user history; a controlled completed-assistant-history case
also passed with one explicit mock seed request and zero additional requests on
resume. The CLIs exited normally. This fixture starts its own private native
server; it does not prove cold discovery or ownership of every launch mode.

The native TUI can create a hidden title-generation thread after the first user
item. The fixture names its own thread before seeding to keep that separate
feature out of the lifecycle case, while continuing to count every HTTP request.
An unnamed-thread diagnostic captured the title prompt as an actual additional
request. Production discovery must therefore distinguish root Agents from
native feature workers, and cannot treat every new thread notification as a
request for another credential or receiver. The inspected native schema exposes
`parentThreadId`, `ephemeral` and `threadSource`; their availability and meaning
still need qualification on every supported runtime.

Loaded threads can remain after their frontend closes. Loaded inventory is not
a frontend-ownership or receiver-retirement signal. Windows qualification uses
the same orchestrator with a ConPTY adapter; platform probe results remain
distinct from qualification of the final composed source. Prefer the smallest
owner these proofs support, without another controller or reconnect implementation
to work around an untested race.

## Durable identity and transient binding

Keep these separate:

| Fact | Owner |
| --- | --- |
| Stable Agent name, authenticated owner and namespace | Existing Nexus authorization and Agent context |
| Native Codex thread ID and current cwd | Native Codex runtime |
| ACP SID and owned scode lifetime | Existing ACP connection owner |
| Native-thread-to-Agent/ACP reference | Private adapter configuration, not a second authorization registry |
| Receiver lease, consumed position and message durability | Existing mailbox/backend |

A native session ID, PID or static profile name is not a certificate subject.
Allocate a durable Agent instance through the existing authorized identity
mechanism and persist its reference before attachment. Resume reuses that
reference. A genuinely distinct Agent gets a distinct identity; it cannot adopt
another Agent's credential because they share a workspace. Losing a reference
or encountering an unknown owner requires explicit recovery, not silent minting.

The current MCP host requires a fixed `acpSessionId`, and its callback requires a
fixed `threadId`. Copying that file into global Codex configuration would make
unrelated sessions contend for the same native writer/receiver. The adapter must
bind each actual native context to its own authorized connection while retaining
one shared implementation of send/list/prompt/audit/callback behavior. Missing
or ambiguous context is an explicit refusal, not a fallback to another session.

## Preparation and admission

An explicitly enabled installation may invoke the existing shared-service
preparation entry before attachment. It must reuse a live external owner, resume
only a registered local owner, and retain its pinned binary, data, identity and
zones. The node record alone grants no right to restart or replace a process.
Restarting Codex must not change voter membership or select a second Box because
the configured Box is temporarily unavailable.

The setup slice reports `domainJoinAccepted: false` and pending Agent
authentication. It does not supply an Agent enrollment grant. The adapter needs
an existing authenticated, owner-scoped admission capability for new Agents.
That capability has not been established by this investigation. Until it exists,
an uncredentialed Agent is `needs_enrollment`, not joined. Reuse the current
credential authority and its committed ownership checks; do not copy a CA,
reuse an unrelated certificate, add an issuer, overwrite an unknown owner, use
`allow_existing`, or silently fall back to disk.

Transport interruption does not block ordinary Codex work. Preparation,
authorization and receiver progress remain inspectable. Admission or send
ambiguity retains the original identity/operation and Unknown result; startup
does not automatically resend a message or elect another identity.

## Installation and qualification

The installable entry, shared package and CLI must have source/hash receipts and
rollback paths. Machine paths, model/account choices, credentials and native
thread references remain private state. Mac and Windows use the same adapter
code; path/ACL differences belong to installation. A native hook's initial trust
review is part of one-time installation, not something hidden by a bypass flag.
Do not change approvals, sandbox policy, shared launchers or the running node to
make a startup test pass.

Qualification must include:

1. Actual Codex MCP startup and authoritative context binding on Mac and Windows,
   including idle resume and a new root thread before any additional model call.
2. Two simultaneous roots, including the same workspace, with distinct owned
   bindings; no arbitrary caller-selected sender or cross-thread result routing.
3. Actual authorized Node append, incoming delivery and original-thread native
   callback. Count direct-operation inference separately from a recipient's
   later ordinary reply. Directory visibility is not online or delivery proof.
4. Node absent/offline, missing credential, refused enrollment and response-loss
   cases: ordinary Codex still works, status is accurate, and no uncertain send
   is replayed.
5. Restart/close cleanup using the existing ownership boundary: old children
   exit, only the correct new owner acquires naturally, and original SIDs,
   transcripts, receiver positions and namespace remain intact.

The separate cumulative CLI work in #1049 keeps its own exact-head CI, review
and normal merge-queue gates. This investigation neither replaces that work nor
qualifies a live upgrade of either Mac or Windows.

## Integration handoff after native lifecycle qualification

The shared connection (#1080), lifecycle fixture (#1089), and diagnostic work
(#1049) have landed. The installable autojoin adapter has not. Root owns the
overall integration, package qualification, Windows installation and staged
six-party acceptance. The Mac owner has completed the common connection work
and supplies Mac qualification and local installation/rollback preparation.
Further common runtime work must keep one writer; the runtime/file split below
is a proposed handoff until that owner confirms it.

First reconnect the existing six authorized Agents. Their private bindings may
reference their existing credentials and ACP sessions after owner verification.
This does not enroll every newly discovered native thread. An unknown root is
`needs_enrollment` until the existing authority grants admission. There is no
CA copy, alternate issuer, or workspace-based adoption of another Agent.

| Delivery | Owner and boundary |
| --- | --- |
| Overall integration and acceptance sequence | Root: one release/rollback contract and the final go/no-go decision |
| Common native discovery, binding and retirement adapter | Proposed Mac continuation in `sdk/typescript`, reusing `connectCodex`, the ACP client and callback; confirm before editing shared host files |
| Common installer and Windows installation | Root in `sdk/typescript`; consume the same adapter artifact, preserve native user configuration and reject conflicting legacy entries |
| Mac installation and actual local owner facts | Mac: consume the same package, preserve original thread/SID/approvals and prepare rollback |
| Agent admission and local Box preparation | Existing Nexus authority and scode shared-service owner; the adapter stores references and invokes their supported entrypoints |

The proposed installer/runtime boundary has three responsibilities:

1. A private machine configuration selects the existing Box and native endpoint,
   records the explicit multi-agent choice, and references approved bindings.
   A binding includes the native runtime instance, exact root thread, authorized
   Agent/namespace and existing ACP-session reference. Paths and credentials
   are private installation state. These references do not grant authorization.
2. The runtime verifies native context before attaching a binding and before
   routing a tool call or callback. Tool arguments cannot select an actor or
   substitute a thread. Native roots, feature workers, loaded inventory and live
   frontend ownership are separate facts. The existing callback remains the
   only implementation of active/idle delivery.
3. The connection owner retires its actual owned children and observes exit
   before a replacement attaches. Native inventory remaining loaded after a
   frontend exits is not sufficient to retain ownership. Do not create another
   receiver or a new lock manager to compensate for a missing lifetime signal.

Resolve the endpoint and frontend-lifetime contract in the shared adapter before
marking an installer ready. In the inspected native source, ordinary CLI startup
probes the default daemon socket and otherwise uses an embedded server; the
proxy only connects to an existing endpoint. The private fixture's explicit
server launch therefore does not establish automatic production startup. Reuse
the native daemon-management entry if supported by the actual installed binary,
and qualify that choice privately first. No generated npm shim or shared shell
launcher needs to be patched to disguise an undiscoverable embedded server.

The first installable candidate must prove ordinary `codex` and `codex resume`
with two roots in the same workspace, original-history restoration, native
caller binding, zero additional startup/resume model requests, actual Node
send/receive and original-thread active/idle callback. It must also prove close
and replacement ownership, offline behavior, conflicting configuration refusal
and rollback. Preserve the actual CLI/account/model and on-request/user route.

The Windows `55e79179` fixture passed cancelled and completed resume against
native 0.163.0: one explicit mock seed request per case, zero additional requests
on resume, three normal CLI exits per case. The observed roots report
`source: vscode`; filtering exclusively on `source == cli` would reject these
real roots. Windows server termination in that fixture does not prove graceful
server shutdown. These limits remain distinct from adapter acceptance.

No installation or restart follows from this document. Before a live window,
publish each participant's exact steps, versions/identities, expected receipts,
stop conditions, rollback and an out-of-band contact path. Confirm actual owner
readiness, then perform single-side recovery before a joint outage. The final
six-party result includes 30 directed edges and natural commits. HA and the real
DGX deploy/cross-domain join retain their subsequent acceptance stages.
