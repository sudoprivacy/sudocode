# Codex network autojoin

## Acceptance target

After one installation and authorization on each machine, ordinary Codex startup
and resume in every workspace join the configured Nexus network without a join
prompt or an additional model request. Restoring an existing Codex conversation
preserves its Agent identity and history. Separate concurrently owned Agents do
not share a receiver or transcript writer. This includes real reception while
the original Codex thread is idle; having tools in its menu is insufficient.

This document records the historical source investigation, current implementation
status and remaining rollout gates. The shared adapter and prepared-machine
installer now exist; their presence does not qualify a particular installation
or the live six-peer deployment. See the reviewed-source status below.

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

The historical Mac observations in this investigation used CLI `0.163.0`; they
do not identify the currently installed version or prove its bytes equal the
inspected checkout. Its earlier source-bound native protocol receipt establishes
the namespaced callback shape within that qualification. Later lifecycle or
installation claims require their own actual binary and source receipts.

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

The fixed-session MCP mode requires an `acpSessionId`, and its callback requires
a fixed `threadId`. Copying that file into global Codex configuration would make
unrelated sessions contend for the same native writer/receiver. The adapter must
instead compose each actual native context with its own authorized connection,
as the shared global adapter now does, while retaining one implementation of
send/list/prompt/audit/callback behavior. Missing
or ambiguous context is an explicit refusal, not a fallback to another session.

## Preparation and admission

An explicitly enabled installation may invoke the existing shared-service
preparation entry before attachment. It must reuse a live external owner, resume
only a registered local owner, and retain its pinned binary, data, identity and
zones. The node record alone grants no right to restart or replace a process.
Restarting Codex must not change voter membership or select a second Box because
the configured Box is temporarily unavailable.

The earlier setup slice reports `domainJoinAccepted: false` and pending Agent
authentication; setup alone supplies no enrollment grant. The shared operator
adapter now implements the admission boundary, but requires an existing,
authenticated owner grant and the selected Node's enrollment capability. An
uncredentialed Agent remains `needs_enrollment`, not joined. Reuse the current
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

## Reviewed implementation and remaining handoff

The source review for this update uses protected main
[`3511acd5`](https://github.com/sudoprivacy/sudocode/commit/3511acd58ecbb0f3e07947d3d7a70d2ae8050e60).
The earlier proposed runtime/installer split is superseded by the shared
implementation. This documentation update runs no native session, model,
enrollment, installation or live migration.

| Implemented boundary | Reviewed source and remaining qualification |
| --- | --- |
| Native transport | `codexConnection.ts::connectCodex` supplies the shared proxy connection. The actual selected native package and endpoint still need source-bound qualification. |
| Root discovery and binding | `codexAutojoin.ts::connectCodexAutojoin` reads loaded inventory, qualifies user roots and preserves durable actor/ACP bindings. Missing classification fields refuse attachment; inventory errors block tools. |
| MCP ownership and direct tools | `codexMcp.ts` owns one authenticated local bridge and routes host-supplied thread context through the shared tools and callbacks. A workspace or model-selected argument does not select the sender. |
| Enrollment | `codexEnrollment.ts` invokes the existing operator client under an explicit private grant. An uncertain operation remains recovery-required; it does not mint a replacement identity or install a Node. |
| Prepared-machine installation | `codexSetup.ts::installCodexMcp` performs the selected native preparation, MCP readback and launcher installation with a create-only journal. It preserves a backup and reports `a2aJourneyVerified=false`; installation is not end-to-end acceptance. |
| Native user entry | `codexLaunch.ts::runCodexUserEntry` preserves the selected native argv and review policy. It implements neither a native package manager nor a Nexus owner. |

The operating instructions remain in the
[SDK README](https://github.com/sudoprivacy/sudocode/blob/main/sdk/typescript/README.md#global-native-codex-attachment).
The
[attachment contract](https://github.com/sudoprivacy/sudocode/blob/main/docs/design/codex-a2a-autojoin.md)
owns binding, enrollment, loaded-runtime lifetime and recovery details. Keep
commands and those rules there rather than copying another installer or
callback specification into this investigation.

The official [App Server guide](https://learn.chatgpt.com/docs/app-server)
documents loaded-thread inventory, reads without resuming, runtime-status
notifications and explicit resume. Those APIs do not attest the selected
endpoint, SudoCode attachment, Node authorization or installation. A loaded
thread's lifecycle and a visible frontend's lifetime remain separate facts.

The adapter currently retains ownership for qualified loaded roots, including
work that outlives a frontend. It retires owned connections after authoritative
absence or closure; failed inventory leaves tools unavailable and cannot prove
that an owner exited. Qualify actual unload, retirement and subsequent resume
on the supported native launch mode before changing a live installation. The
older frontend-retirement proposal is not permission to stop a still-owned
runtime merely because its window closed.

Acceptance remains per composed artifact and deployment. The SDK's existing
fixtures cover ordinary startup/resume, human-review entry and isolated
prepared-machine installation; their presence is not a new execution result.
Record actual native/scode/operator/package identities, original Agent and SID,
request counts, direct Node operations and original-thread callbacks. Retain
failure, offline, unknown-enrollment and rollback evidence. Historical Mac and
Windows `0.163.0` lifecycle receipts do not certify a newer source or install.

The existing six-party rollout still requires explicit participant readiness,
exact steps and versions, stop conditions, rollback and an out-of-band contact
path. Verify single-side recovery before a joint outage and all 30 directed
edges before claiming the six-party result. HA and DGX cross-domain acceptance
remain subsequent stages. Merging this documentation grants no installation,
restart, credential, native-thread or live-window action.
