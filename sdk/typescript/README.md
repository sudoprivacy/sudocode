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

## Receiver lifetime attached to an existing Windows frontend

For a fixed-session receiver which must retire with one original frontend,
capture its identity with `node dist/server.cjs --capture-receiver-owner PID`.
This read-only command returns `platform`, `pid`, decimal-string
`createdFiletime100ns` and `exe`. Place that object in `receiverOwner` in the
same private runtime config as `acpSessionId` and `codexCallback.threadId`.
Run the ordinary `--config` entry. There is no second launcher, identity
sidecar, repeated PID probe or application-maintained heartbeat.

The shared adapter verifies the exact identity and holds the original Windows
process handle. A timeout during initial qualification reports liveness UNKNOWN;
once ready, only the kernel exit event proves that the original owner exited.
Monitor failure is separately recorded and closes the receiver without claiming
owner exit. It never adopts a replacement PID. A named pipe keyed by the original
process identity and existing ACP session prevents duplicate receivers even when
the config is copied or renamed. Unconfirmed child cleanup retains ownership;
the lock is released after the owned receiver and monitor close successfully.
The monitor requests synchronization and limited query access, never termination
rights. Receiver EOF closes only its own helper. Windows PowerShell and .NET are
used from the OS installation, with no Python/psutil or native Node addon needed.
If PowerShell exits before its synchronous compiler-bootstrap completion
marker, descendant cleanup remains UNKNOWN and the receiver lock is retained.
The common CLI signal adapter waits for receiver cleanup before exiting.
Rejected cleanup keeps the original MCP endpoint available for status and
retains exclusivity; another graceful signal does not reinterpret failure as
success. Forced OS termination is outside this guarantee. The global inventory
owner uses this same shutdown adapter and releases its endpoint only after
startup and all owned connections finish cleanup.

MCP initialization remains independent of owner qualification and ACP startup;
tools refuse as `not_submitted` until ready. Once bound, the existing shared
same-SID connection recovery handles transport failure. Process lifetime,
durable inbox delivery, native acceptance and model completion remain distinct.
Approval settings remain the responsibility of the original foreground launcher;
this observer does not set or authorize them. This option is Windows-only. The
global autojoin owner still follows native-root inventory, not one frontend PID.

## Existing-thread native queue ingress

For an explicitly known legacy thread without a reachable native socket,
the fixed-session bridge also offers an opt-in `codexCallback.transport:
"native-queue"`. It reuses its existing SID/peer filter and records real queue
receipts separately from native turn acceptance. The [queue ingress
contract](../../docs/design/codex-native-queue-callback.md) specifies the explicit
native home, bounded unloaded producer, Unknown/no-replay behavior and actual
native/ConPTY acceptance. Socket callbacks remain the default.

## Claude Code channel attachment

Claude Code joins the agent network through the fixed-session bridge with
`claudeChannel` in its config. The bridge declares the Claude Code
[`claude/channel`](https://code.claude.com/docs/en/channels-reference) capability
and asks scode for [client-owned peer input](../../docs/acp.md#client-owned-peer-input-clientcapabilities_metasudocodepeerinput):
each peer message arrives in the Claude Code session as
`<channel source="…" from="…" verified="…">` holding the same
`<mailbox-message>` block a scode model reads, and the scode session's own model
never answers it. Claude replies with `scode_send`; `scode_agent_list`,
`scode_prompt` and `scode_status` are unchanged.

```json
{ "version": 1, "scodeArgv": ["/abs/scode", "acp"], "workspace": "/abs/workspace",
  "acpSessionId": "EXISTING_NODE_SESSION", "auditLog": "/abs/private/audit.jsonl",
  "claudeChannel": { "peerPolicy": "node-namespace" } }
```

`node-namespace` accepts any sender the Node stamped, after `agent_list` proves
the session's Node identity. `allowlist` with `peers` accepts only those names.
`verified` comes from scode's per-message provenance (`true` when the session's
mailbox authenticates writers); for a scode that does not report it, the bridge
marks events verified only after its own Node identity check. Register the bridge as an
MCP server (for example `claude mcp add nexus_scode -- node /abs/server.cjs
--config /abs/private/claude.json`) and start Claude Code with
`claude --dangerously-load-development-channels server:nexus_scode` while
channels are in research preview. A scode without client-owned peer input is
refused at startup instead of answering peers alongside Claude.

## Global native Codex attachment

The `--codex` entry connects all loaded user-root threads through one per-user
bridge owner. It uses Codex's normal global MCP configuration. It does not copy
one fixed Agent/session config into every workspace. Same-directory roots get
distinct durable bindings; resuming a thread reuses its original Agent and ACP
session. Task prompts and exact direct messages keep the same implementations
as the fixed-session bridge below. Each owned scode session uses
[client-owned peer input](../../docs/acp.md#client-owned-peer-input-clientcapabilities_metasudocodepeerinput),
so peer messages reach the Codex thread and the scode session's own model never
answers them; a scode without it is refused.

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

Initial Node queries validate the native audit and actor/session binding before
reporting an operation failure. An authenticated `agent_list` failure retains
its original result as `Error.cause`; it does not imply an identity mismatch.
The owner reconnects with the same durable binding after confirmed child cleanup.
Unknown cleanup keeps the binding disconnected and prevents a replacement.

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

On Windows, the initializer also returns `ownerPreparationArgv`. Run that
preparation command before installing the MCP entry, review the generated
current-user Task Scheduler XML and run its returned `installationArgv` once.
Preparation writes a plan; it does not register or start the task. Registration
does not silently overwrite an existing task. The action uses the signed-in user's token
without a password, elevation or a scheduled trigger. When an MCP entry needs
the shared owner, it requests that action once and authenticates the same
exclusive loopback endpoint. The existing native-root inventory still decides
when the owner can retire. A missing task reports the preparation step; it
does not launch a temporary replacement under the MCP process.

This distinction matters on Windows: native Codex contains each stdio MCP in a
Job that terminates its descendants when that root unloads. Node's `detached`
option does not give a shared owner independent lifetime in that Job. Task
Scheduler launches the same canonical owner outside the individual MCP's
lifetime. No native Job flags or permissions are changed. Mac and Linux keep
their existing detached owner launch.

For rollback, let the selected owner exit normally before running the returned
`removalArgv`, then restore the prior reviewed MCP/config entry. Removing the
registration does not serve as an owner-stop command. Retain the original
bindings and ACP sessions. Install version updates in a new private directory;
preparation refuses an existing XML plan with different bytes. Use an
owner-protected parent ACL on Windows. Current task plans reject percent
placeholders in action paths so Windows cannot expand them to a different
command. This setup manages only the Codex bridge; it does not install or
restart Nexus or the native Codex daemon.
The task runs in the selected user's environment. For a nondefault native home
or endpoint, pin it in the existing `native.env`/`native.socket` config and keep
the full official wrapper argv. Do not rely on per-MCP inherited variables to
select the shared native server; `native.env`, when present, is the complete
proxy environment.

The result also includes the official `nativePreparationArgv` (shared daemon
start) and `nativeProxyArgv` for API frontends. Ordinary official CLI startup
uses its managed package context after one-time preparation. A legacy frontend
that directly spawns `app-server --listen stdio://` does not join that shared
inventory merely because global MCP was configured: its own controller must
switch to the official proxy during a sole-owner migration window, preserving
its thread, approval policy and native package context. The initializer only
prints those commands; it never starts/stops a native server or adopts a
running frontend. This package adds no native daemon lifecycle implementation.

`userEntryArgv` is an optional foreground entry for an existing command that
repeats `-c approvals_reviewer=user`. Native Codex treats that CLI override as
a request for an embedded server, which the shared bridge cannot observe.
The entry reuses the official daemon start and read-only config API, removes
only a redundant user-review override after confirming effective `user`
review for the selected cwd, then delegates all remaining argv and exit status
to the official native command. A conflicting or absent reviewer refuses
normalization. Other overrides, explicit private/remote modes and noninteractive
commands remain unchanged. No policy is rewritten, actor enrolled, package
updated, or Nexus daemon started by this foreground compatibility step.
Install the returned entry as the user's ordinary `codex` command after
review and verify resolution in the actual existing terminal, including shell
functions and cached paths. Rehashing cannot move an entry ahead of another
directory in that terminal's PATH. A plain native command remains available by its original
absolute path. Its incompatible CLI overrides still select native private mode.

### One installation command for a prepared machine

An operator first selects the existing Nexus Box, enrollment authority, verified
scode and official native Codex installation. Keep those credentials on that
machine. `--init-codex-operator` and `--init-codex` prepare their existing private
policies; they do not enroll an actor or install/start Nexus.

Supply `--native-env-file /absolute/private/environment.json` to `--init-codex`
to pin the complete selected native environment, including an existing absolute
`CODEX_HOME`. Supply `--command-entry /absolute/public/codex` (or `codex.ps1`
for PowerShell) to receive a complete `installationArgv`. The native executable
and official wrapper in the runtime must be real independent paths, so replacing
the public launcher cannot recurse into itself. Keep environment values in the
private file, rather than putting credentials in command arguments.

Run that emitted argv once, equivalently:

```sh
node /absolute/package/dist/server.cjs --install-codex \
  --config /absolute/private/runtime.json --command-entry /absolute/public/codex
```

It registers the existing Windows owner Task when applicable, asks official
Codex to prepare its managed daemon, installs and reads back the global
`nexus_scode` MCP entry, then replaces only the explicitly selected ordinary
launcher. The previous small script or symlink is backed up in the private
installation record. Native binaries are refused. An entry or runtime changed
during preparation is preserved; a different existing MCP registration needs
explicit migration. Each native home admits one create-only installation, so
concurrent configurations and uncertain partial attempts cannot replay writes.

This configures **new Codex launches**. A shell function already loaded in an
existing terminal can still shadow the selected disk entry; verify resolution
in the actual user terminal. Existing private/legacy roots retain their separate
migration window. No prompt, mint, root load, Node stop or Job-policy change is
part of installation. The result deliberately leaves `a2aJourneyVerified=false`:
fresh enrollment and actual list/send/receive plus resume still need acceptance.
On failure, retain the named installation record and backup for operator recovery;
there is no automatic retry or rollback over another writer's changes.

For assisted onboarding, ask Codex to select the verified native/scode paths and
owner-supplied enrollment grant, prepare the two private policies, run the emitted
installation argv, and verify the actual ordinary command and original resume
identity. Missing Box/authority is an input to resolve, never implicit permission
to create another Node or use a different network. This is not yet a zero-input
fresh-machine installer.

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

The same lifecycle fixture accepts `--human-review-entry` to launch the actual
`--sandbox danger-full-access --ask-for-approval on-request -c
approvals_reviewer=user` prefix through the canonical foreground entry. Use
`npm run test:codex-entry -- --codex /absolute/native/codex
--managed-package-root /absolute/path/to/@openai/codex --case cancelled-resume
--output /absolute/evidence` (or `completed-resume`). It verifies the effective
human-review policy, two shared user roots, visible history before interruption,
actual unload before resume and zero extra resume requests. It uses the same
private fake provider and preserves the existing lifecycle deadlines and
request counts; it does not qualify a production installation or new machine.

The isolated real-Node fixture can also exercise the one-command installation.
Alongside its pinned scode/daemon inputs, set `NEXUS_OPERATOR_BIN`,
`NEXUS_OPERATOR_SHA256`, `NEXUS_OPERATOR_SOURCE`, `CODEX_ENROLLMENT_TEST_BIN`
and `CODEX_ENROLLMENT_PACKAGE_ROOT`, then run
`CODEX_ENROLLMENT_INSTALL_ENTRY=1 npm run test:native` after creating the clean
source-bound archive. Windows also supplies `CODEX_ENROLLMENT_DRIVER` as JSON
argv for the existing ConPTY driver. This selects the same idle,
cancelled-resume and completed-resume journeys, installs one private `codex`
command (`codex.ps1` on Windows), and launches every TUI through that command
with the human-review flags. Actual enrollment, MCP send/receive, identity,
history, request-count and retirement assertions remain in the same fixture.
The first frontend uses the daemon started by installation, without a restart
to pick up registration. This installation variant is separate from the
unchanged cold-daemon and legacy-import cases; it does not modify a live user
home or certify a production migration. A setup result alone still reports
`a2aJourneyVerified: false`.

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

The MCP handshake is available while the original ACP session is connecting.
`scode_status` reports that state, and commands return `not_submitted` until
the session is ready. A slow first connection cannot hold up the MCP handshake;
the same-session supervisor continues independently. Refused commands are not
buffered for later execution.

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

`codexCallback` owns a private durable inbox beside its diagnostic log
(`<log>.inbox`). The scode connection negotiates `peerInput` v2: the mailbox
cursor advances only after that inbox has fsynced the exact message. Upstream
redelivery uses the same identity. A full inbox (64 pending messages), storage
failure or closed owner refuses the handoff rather than dropping it.
Current scode sends carry a distinct `send_id`; truly identical historical
envelopes without that ID cannot be distinguished from a cursor replay.
Receipt files are retained without a time-based deduplication expiry.

Both socket and native-queue transports submit through the versioned
`thread/queue/addOnce` method. It must be supplied by the matching Codex native
build. There is **no fallback** to `turn/start`, `turn/steer` or ordinary
`thread/queue/add`: those APIs cannot safely retry an ambiguous outcome. The
native store accepts each `(thread, clientUserMessageId)` once, rejects changed
content for an existing ID, and retains acceptance after dequeue/edit/delete.
Acceptance means durable queuing; the existing owner decides when it is idle
and eligible to run. Paused/interrupted threads are not forcibly resumed.

The callback reloads pending inbox records after restart, retries with the same
ID using 1–30 second backoff, and writes a done receipt before releasing the
message bytes. Unsupported native versions retain pending records and an error.
Legacy JSONL `pending` records have no safe idempotent submission contract and
are **not** automatically imported or replayed. Do not delete either inbox or
native delivery receipts to resolve an uncertain delivery.

The callback and autojoin owner reconnect their own native proxy to the same
endpoint after disconnect. Autojoin can reopen a failed owned scode using its
unchanged ready binding, actor and ACP session; it never repeats an ambiguous
enrollment or falls back to a new session. Ordinary tool/send RPCs are still
sent once. Reconnection does not replay them. The fixed-session MCP bridge
also reopens only its configured ACP session after startup failure or child
exit, with 5–30 second backoff. Its MCP status remains available; commands
attempted while disconnected return `not_submitted`. An in-flight call with
a lost reply remains `unknown`. Unconfirmed child cleanup stops replacement.
These changes require rebuilt
SDK, scode and Codex native artifacts; changing the shared scode symlink does
not upgrade existing processes or absolute profile pins.

Messages remain peer data, not user authority. Queue acceptance does not prove
final handling, global idle, or exactly-once tool side effects. This recovery
protocol does not restart a native server, change credentials, create threads
or override approvals.

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
