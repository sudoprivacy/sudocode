# scode ACP client and MCP bridge

`@sudocode/acp` owns the reusable TypeScript client, direct A2A helpers and
stdio MCP bridge beside the scode ACP implementation. It is extracted from
Hydra's tested ACP runtime. Applications such as Hydra consume this package;
they do not maintain another connection, direct-call or callback implementation.

The runtime archive bundles its JavaScript dependencies. It needs Node.js 20+
and a separately verified full `scode` executable for the machine architecture.
It does not install Hydra, a terminal UI, a Nexus daemon or native Node addons.
Linux ARM64 (including DGX Spark), Linux x64, macOS and Windows use the same
JavaScript payload. CI runs actual stdio/package workflows on these platforms;
a new machine still needs its own scode, credential and transport acceptance.

## Existing-thread native queue ingress

For an explicitly known legacy thread without a reachable native socket,
the fixed-session bridge also offers an opt-in `codexCallback.transport:
"native-queue"`. It reuses its existing SID/peer filter and records real queue
receipts separately from native turn acceptance. The [queue ingress
contract](../../docs/design/codex-native-queue-callback.md) specifies the explicit
native home, bounded unloaded producer, Unknown/no-replay behavior and actual
native/ConPTY acceptance. Socket callbacks remain the default.

## Global native Codex attachment

The `--codex` entry connects all loaded user-root threads through one per-user
bridge owner. It uses Codex's normal global MCP configuration. It does not copy
one fixed Agent/session config into every workspace. Same-directory roots get
distinct durable bindings; resuming a thread reuses its original Agent and ACP
session. Task prompts and exact direct messages keep the same implementations
as the fixed-session bridge below.

This mode requires native root inventory and host-supplied tool-call metadata
(qualified on native Codex 0.163.0), a compatible scode with native actor audit,
and an operator-authorized enrollment adapter. The shared `--enroll-codex`
adapter requires a private owner-created grant and the Nexus
`nexus-agent-enroll` client; it never installs a Node or supplies authority
implicitly. `--init-codex-operator` prepares that private launch policy without
issuing an identity. Protocol fixtures do not establish production enrollment
or current-node compatibility.
The [ownership, enrollment and recovery contract](../../docs/design/codex-a2a-autojoin.md)
defines that remaining integration boundary and rollout gates.

Reception defaults to the exact configured peer allowlist. A network-wide
installation can explicitly select `--peer-policy node-namespace` during
`--init-codex`; after its owned Node actor/session audit succeeds, other
authenticated Agents in that namespace may supply untrusted peer context.
This grants no tool authority and does not establish common ownership or
online presence. Existing fixed callbacks keep their original allowlists.

Prepare a private machine configuration with `--init-codex --config FILE
--native-codex EXECUTABLE --enrollment JSON_ARGV --peers JSON_NAMES`. The result
contains the exact native `codex mcp add nexus_scode -- ...` argv for review.
Use `--native-argv JSON_ARGV` instead of `--native-codex` when the official
installation uses an interpreter and wrapper. The full argv is preserved for
both proxy connections and MCP registration; it is never passed through a
shell. Keep that installation's native package context intact.
If native unload races inventory inspection, only its exact unloaded-thread
refusal plus a fresh complete inventory confirming absence permits retirement.
Other inspection errors refuse the command. This reads state again; it never
resubmits a tool call or an enrollment operation.
The initializer starts nothing and does not install global configuration. Use
an owner-protected parent directory; it exclusively creates the file and keeps
machine values outside the package. Keep the MCP server optional so an offline
node cannot prevent ordinary Codex startup. Existing project or launch-time
overrides need an explicit migration; a global entry cannot override them.

The result also includes the official `nativePreparationArgv` (shared daemon
start) and `nativeProxyArgv` for API frontends. Ordinary official CLI startup
uses its managed package context after one-time preparation. A legacy frontend
that directly spawns `app-server --listen stdio://` does not join that shared
inventory merely because global MCP was configured: its own controller must
switch to the official proxy during a sole-owner migration window, preserving
its thread, approval policy and native package context. The initializer only
prints those commands; it never starts/stops a native server or adopts a
running frontend. This package adds no native daemon lifecycle implementation.

Existing fixed-session Agents can be prepared offline with `--bind-codex`.
The [binding contract](../../docs/design/codex-a2a-autojoin.md#durable-state-and-recovery)
specifies its explicit mapping document, original SID and separate native/ACP
workspaces. Preparation refuses a running global owner or an existing mapping.
It does not start a receiver; the previous owner must exit in the coordinated
window before the normal global entry loads that same actor/session.

To qualify ordinary native startup after its one-time machine preparation, run
the existing lifecycle test with `--autojoin --managed-package-root
/absolute/path/to/@openai/codex` as well as `--codex`, `--output` and the selected
`--case`. This uses only a private Codex home: official daemon start/stop prepares
that home, then ordinary TUIs start their default daemon themselves. The test
requires original-thread unload/resume, unchanged actor/SID, zero resume model
requests and bridge retirement while the native server stays running. Its
one-second unload setting is fixture-only; it does not select a production
retirement policy. No Nexus enrollment or production frontend is used.

## Build a source-bound archive

From this directory in a clean committed checkout:

Source tests need Python 3 for the PTY/ConPTY guard and real AF_UNIX endpoint checks; the installed
ACP runtime uses Node only. The guard runs a mock terminal and verifies that a
close request cannot confirm an installer dialog.

```sh
npm ci
npm test
npm run pack:runtime
```

`artifacts/sudocode-acp-0.1.0.tgz` is an ordinary npm package with no install
scripts or runtime dependency downloads. Its adjacent receipt records the
source commit, archive SHA-256 and npm integrity. `manifest.json` records the
commit and each runtime file's size/SHA-256. Dirty builds cannot be packaged.
These checks detect corruption; authenticate the receipt/source through your
trusted repository or artifact channel before installation.

## Install on another machine

Transfer the archive and receipt. Verify the archive SHA-256 against the
trusted receipt, then install into a new versioned per-user directory:

```sh
npm install --offline --ignore-scripts --no-audit --no-fund --prefix /absolute/versioned/install /absolute/sudocode-acp-0.1.0.tgz
node /absolute/versioned/install/node_modules/@sudocode/acp/dist/server.cjs --verify
```

The same commands work in PowerShell with Windows absolute paths. Keep the
previous version directory for rollback. Use a versioned, verified scode path,
not a shared launcher that another process can repoint. This archive is not a
scode binary, node record, certificate or model credential bundle.

### Pair the native CLI with its Node protocol

The bridge's ACP capability handshake does not certify the Nexus backend
protocol. Record the full CLI source/hash and the daemon release separately.
Starting with scode 0.2.36 (Nexus pin v0.8.23), `InspectTarget` resolves the
operation-authorized target, including linked streams. The current backend also
supports older nodes for classification: only an `Unsupported` target query
uses the entry's own authenticated `Stat` type, without following a link.
Target-based authorization still requires `InspectTarget` and fails closed when
it is unavailable. Permission, transport and other errors do not select this
compatibility branch. Qualify the exact CLI/node pair rather than infer backend
compatibility from an ACP initialize response.

For a frozen older node, retain its independently tested compatible CLI.
Changing the bridge package or successfully initializing ACP does not upgrade
that node. Qualify a daemon upgrade and rollback separately before switching
the CLI; do not upgrade a shared running node as a package-install side effect.

Generate a private machine configuration with an explicitly existing session:

```sh
node /absolute/install/node_modules/@sudocode/acp/dist/server.cjs --init --config /absolute/private/runtime.json --scode /absolute/scode --workspace /absolute/workspace --session-id EXISTING_SESSION_ID
```

`--init` creates a new file exclusively and never overwrites a configuration,
creates an agent/session, or performs a network call. New POSIX directories use
0700 and the file uses 0600. Existing directories retain their permissions;
on Windows use an owner-protected parent ACL. The generator does not claim
cross-platform secret-file durability or repair existing parent permissions.
It writes no credentials. Review any required scode flags in `scodeArgv`
(model/account/tool allowlist) to preserve the existing approval policy.

A minimal configuration is:

```json
{
  "version": 1,
  "scodeArgv": ["/absolute/verified/scode", "acp"],
  "workspace": "/absolute/workspace",
  "acpSessionId": "EXISTING_SESSION_ID",
  "auditLog": "/absolute/private/external-commands.jsonl"
}
```

Launch the MCP server with `node .../dist/server.cjs --config /absolute/private/runtime.json`.
Configure that command/args in the frontend's supported stdio MCP configuration.
Configuration and audit logs are private machine state, outside the package.
Do not copy another device's identity, credentials or original thread ID.

## Library API and ownership

```ts
import { connectScode, sendScodeMessage, listScodeAgents } from '@sudocode/acp';
const connection = await connectScode(config, notification => receive(notification));
try {
  const result = await sendScodeMessage(connection, {
    to: 'authorized-peer', message: 'Exact plain text', summary: 'Coordination'
  });
  // Preserve result.content and result.isError, including an error result.
} finally {
  await connection.close();
}
```

Exports are `@sudocode/acp`, `/scodeAcp`, `/scodeA2a`, `/mcp` and
`/codexCallback`. `runScodeMcp(configFile, { registerShutdown })` lets an upper
application reuse its existing lifecycle owner. Omitting that hook uses the
standalone shutdown registry. Every export resolves to one bundled library;
the bridge entrypoint also uses that library, avoiding a second singleton.
TypeScript consumers of ACP SDK types can install the optional exact SDK peer
listed in package.json. The runtime needs no peer installation.

The MCP bridge exposes `scode_send`, `scode_agent_list`, `scode_prompt` and
`scode_status`. Direct send/list use the advertised native v1 capability and
loaded SID, with no model prompt, automatic retry or prompt fallback. Task
prompts can use the model. Native tool output is wrapped once as an unchanged
text block. A lost/ambiguous response remains Unknown. Only proven local or
advertised typed pre-admission refusal is not_submitted. Listing means
visibility/addressability, not online proof. The host binds actual actor and
namespace; the wrapper does not accept a caller-selected sender.

## Optional Codex callback adapter and existing-thread migration

The current bridge binds one explicitly configured ACP SID and callback thread.
It is not a global autojoin entry for arbitrary Codex conversations. The
[Codex autojoin investigation](../../docs/design/codex-network-autojoin.md)
records the native startup/context boundaries and acceptance required for that
mode, including idle-start behavior and authorized Agent enrollment.

`connectCodex({codexArgv, socket?, env?, closeTimeoutMs?, onNotificationError?}, onNotification?)`
exposes the existing native proxy transport for context inspection. It returns
`initialized`, `request`, `exited`, `lastNotificationError` and an idempotent
`close` that waits for its own spawned process, with bounded forced exit. Both
ACP and native shutdown use the same timeout primitive. This never stops the
native server; an arbitrary wrapper's other descendants are outside that exit
receipt. Pass the actual executable, or Node plus a script path, rather than a
Windows `.cmd` shell wrapper: spawning uses `shell:false`.

An optional `env` is the complete environment for that owned proxy process,
matching Node's spawn semantics. It is not merged with the controller's
environment. Omission retains normal inheritance. An isolated caller can
therefore supply private `CODEX_HOME`, `HOME`/`USERPROFILE` and Windows application
directories without a shell wrapper or mutation of the controller's environment.
Include required platform variables such as `PATH` and `SystemRoot` explicitly.

Server requests are ignored without responding, even when their IDs overlap
local pending calls. They belong to the actual frontend, including broadcast
authentication refresh. Only notifications reach the optional observer. Its
exceptions/rejections are retained in `lastNotificationError` and sent to the
optional diagnostic observer without failing requests or printing to stderr.
The callback below uses this same transport implementation.

Native requests are sent once. `CodexRequestError.outcome` is `not_sent` for a
local pre-send refusal; once submitted, transport failure is `unknown`. A remote
JSON-RPC error is retained as `rpcError` with outcome `unknown`: the transport
does not certify that an arbitrary failed RPC had no side effects. No error or
active-to-idle race triggers an automatic resend. The helper does not start a
native server, select an Agent, enroll an
identity or discover other frontend endpoints. Native notifications are
observations, not proof of network admission. Close the connection in `finally`
after a read-only `thread/loaded/list` / `thread/read` inspection.

### Opt-in native Codex lifecycle fixture

Run from this SDK checkout after `npm run build`, or from a transferred fixture
kit containing its canonical `manifest.json` and hash-matched `dist/index.cjs`,
using an already installed native Codex executable. A transferred kit needs no
Git checkout, npm install or rebuild. This test does not download or install Codex, connect
to Nexus, enroll an Agent, change the user's configuration or modify a live
frontend. It retains its temporary configuration and receipts for inspection.

```sh
npm run test:codex-lifecycle -- --codex /absolute/path/to/codex --output /absolute/evidence/path --case idle
npm run test:codex-lifecycle -- --codex /absolute/path/to/codex --output /absolute/evidence/path --case cancelled-resume
```

The same `test/nativeCodexLifecycle.cjs` orchestrator runs on both platforms.
Unix defaults to `test/fixtures/nativeCodexTui.py`. Windows uses the bundled
`test/fixtures/nativeCodexTui.windows.py`, which calls ConPTY through Python's
standard library without another package. Select that driver with
`--driver '<JSON argv>'`, containing the available Python executable and the
absolute script path. Keep the shared `nativeTuiPrompt.py` beside either driver.
A missing driver fails before starting native processes;
there is no protocol-only fallback. Use the actual `codex.exe`, not `codex.cmd`.
`--temp-root` selects an existing private temporary directory when the platform's
default socket path would be too long.

Both native fixtures disable startup update checks and set npm's prefix inside
their private root. Both drivers refuse upgrade dialogs before typing `/exit`.
The managed-package variant lets the official native CLI create its private
state directory before writing test settings, preserving its Windows DACL checks.

The fixture performs these steps:

1. Launch one isolated `codex app-server --listen unix://`. Its temporary
   `CODEX_HOME` owns the default control socket. Two real CLI processes run
   `codex --no-alt-screen --cd <same workspace>` and discover that existing
   native endpoint. No `--remote` override substitutes for this discovery.
   Socket readiness lists its directory entry: Windows AF_UNIX reparse points
   cannot be stat'ed. The actual canonical connection is the next barrier.
2. An independent connection uses the shared `connectCodex`, observes actual
   `thread/started` notifications and checks both original IDs, idle state and
   canonical workspace with `thread/loaded/list` and `thread/read`. A listed ID
   can still read as `notLoaded` during initialization. The original 20-second
   startup deadline covers both inventory and actual idle readiness; a started
   event alone is not the barrier.
3. Each terminal driver types `/exit` one key at a time and then Return while
   continuing to drain output. A fast input burst can be treated as a multiline
   paste. All CLI exit codes must be zero; forced CLI termination fails the case.
4. After the initial idle checks, both resume cases explicitly name the original
   fixture thread with `thread/name/set`, preventing an unrelated automatic
   title-generation worker. All HTTP requests still count. `cancelled-resume`
   starts a synthetic user turn and holds its mock response open; a 503 can
   drive retries before some native builds display the user item. It waits for
   actual terminal history, explicitly interrupts the turn, closes both CLIs and runs
   `codex resume <original ID> --no-alt-screen --cd <workspace>`. It checks the
   restored ID, canonical cwd and visible original user history. The loopback
   mock's request count must not increase during resume.
5. The private child environment explicitly excludes loopback from proxies;
   system proxy fallback otherwise caused HTTP 502 before the mock saw any
   request on Mac and Windows. Outside the explicit seed phase the endpoint
   rejects model requests. `completed-resume` serves exactly one minimal
   Responses SSE seed and requires actual assistant history before testing
   resume. Resume must add zero requests. Cancelled seed responses are destroyed
   during cleanup. Endpoint counters do not establish a global inference-attempt
   count.

The platform driver's argv is followed by the complete native Codex argv. It
owns exactly that CLI and reads `{"action":"close"}` from stdin. JSON lines on
stdout report `{"event":"spawn","pid":...}` and finally
`{"event":"exit","pid":...,"returncode":0}`. Diagnostic events `terminate`,
`kill` or `normal_exit_timeout` prevent normal-exit acceptance. Its stderr is
captured separately. `NATIVE_LIFECYCLE_PTY_TRACE` names a temporary JSON snapshot
containing `pid` and a bounded UTF-8 `outputTail`; replace that file atomically so
the orchestrator cannot read a partially written snapshot.

Receipts record executable/library/fixture hashes, versions, method names,
notification IDs, per-phase mock requests and real process exits. Native server
cleanup has bounded TERM/KILL phases and reports the actual result; forced
server cleanup does not prove graceful server quiescence. On Windows, both
signals terminate the process, so graceful server shutdown always remains
unproven regardless of the reported signal name. Loaded threads may
outlive a disconnected CLI, and source labels alone do not establish root
identity. These checks do not qualify Agent enrollment, a production handoff,
all frontend launch modes or automatic network admission.

`codexCallback` is an optional adapter, separate from the ACP transport. It
connects to the existing native app-server proxy/socket and original loaded
thread. It steers an active turn or starts an idle turn through the native
frontend; it does not resume, fork, create another controller or weaken the
frontend's approvals. Configure it using `CodexCallbackConfig` from the
`/codexCallback` export. Notification content remains peer data, not user
authority. An active steer or a delayed callback is not proof of true idle
wakeup: acceptance must observe native idle and an accepted `turn/start`.

A pre-existing thread can retain a temporary MCP config override. On Codex
0.162.1 an isolated actual app-server test showed that changing disk config
and calling `config/mcpServer/reload` still retained that thread override.
Do not treat disk configuration alone as a successful handoff. Verify the
supported same-thread migration independently, stop only the exact old owned
MCP/child pair, then verify the replacement command, same SID/thread, approvals
and single receiver. If the override cannot be removed safely, retain the old
owner and report the unresolved handoff. The presence of CODEX_THREAD_ID is an
optional consistency check, not proof of actual caller identity or exclusion.
Never hardcode that variable as a substitute for native binding/ownership.

## Validation boundaries

The committed process integrations exercise real ACP SDK stdio and MCP
clients, one child/session, permissions, exact Unicode input/output, typed
refusal, response loss, no retries, audit metadata and clean offline package
installation. Fixtures are protocol peers, not a real scode/Node/model backend.
They do not establish native worker pause/cancellation, mTLS policy, a real
Node append, actual idle wakeup or safe live migration. Those paired acceptance
receipts must identify the full CLI binary/source and each actual device.

### Real paired process acceptance

`test/nativeFixture.py` drives an installed archive against a separately
verified full scode and matching Nexus daemon, with fresh fixture identities.
It boots/stops only its own throwaway founder, uses its existing offline mint
CLI, and deletes fixture keys/database/workspaces after stopping the daemon.
No running node, existing identity or shared launcher is used. Python 3 is a
test prerequisite only; the installed bridge requires Node alone.

After `npm test` and `npm run pack:runtime`, provide these environment values:

- `SCODE_TEST_BIN`, `SCODE_TEST_SHA256`, `SCODE_TEST_SOURCE`: absolute full CLI,
  its verified hash and source receipt.
- `NEXUSD_BIN`, `NEXUSD_SHA256`, `NEXUSD_TEST_TAG`: absolute matching daemon,
  its verified hash and pinned release tag.
- Optional `SCODE_TEST_PYTHON`: Python 3 executable (defaults to `python` on
  Windows and `python3` elsewhere).
- Optional `SCODE_NATIVE_TEST_ROOT`: a private evidence directory.

Run `npm run test:native`. npm supplies its own Node/npm paths. The fixture keeps
protocol/log/receipt evidence and always stops its own daemon. It installs the
archive offline, creates then reloads the sender's original SID through the
actual MCP bridge, verifies an actual mTLS Node append and native actor audit,
and requires zero inference requests while send/list run with an offline
recipient. It then starts the recipient for the first time and checks exact durable delivery.
The recipient's ordinary response inference is measured separately: it is not
part of the direct send and must not be silently counted as zero. Earlier
all-participant zero assertions exposed this distinction during bring-up.
In-flight blocking-worker pause, lost responses and original Codex thread
active/idle callbacks remain separate native acceptance gates.

The first native fixture also tried starting and immediately closing/reloading
the recipient before the send; one run did not deliver within its unchanged
30-second budget. That unresolved receiver-reentry observation is retained as
a separate native lifetime/lease concern. The append fixture avoids that
unnecessary setup and does not claim to validate same-agent cold handoff.
