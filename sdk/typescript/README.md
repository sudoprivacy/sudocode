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

## Build a source-bound archive

From this directory in a clean committed checkout:

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
