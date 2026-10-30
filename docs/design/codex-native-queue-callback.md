# Explicit native queue ingress for an existing Codex thread

The normal callback remains the socket transport. This alternative is opt-in
with `codexCallback.transport: "native-queue"` on an existing fixed-session
bridge. It targets exactly the configured original thread and native home. It
does not discover roots, enroll an Agent, create a session or migrate a frontend.

Use the existing scode profile, ACP session ID, workspace, credential bundle and
peer allowlist. Add the callback's verified `codexArgv`, absolute existing
`nativeHome`, original `threadId` and private `log`. Do not set `socket` in queue
mode. Where the owning process has `CODEX_THREAD_ID`, it must still match the
configured target. The scode and native workspaces need not be identical.

## Ownership and delivery

For each admitted peer notification, one bounded native stdio producer performs
only initialize, complete empty `thread/loaded/list`, `thread/queue/add`, then
another complete empty inventory check. It checks notifications through EOF,
refuses a loaded thread or client-action request, and must exit normally to
produce a queued receipt. It never calls start, resume, fork, turn/start or steer.
Only that spawned producer may be terminated during bounded cleanup; the target
frontend and Node are not controlled by this adapter.

The original native runtime owns queue scheduling. An idle loaded target can
start the queued message; an active target waits for its native queue policy.
This is queued **UserInput**, with the same explicit peer-data wrapper as active
socket steering. It is not the idle socket's namespaced `toolOutput` contract.
It cannot grant approvals or alter the existing turn's policy.

Both transports share one SID/peer filter, bounded serial delivery lane and
provenance log. Source, peer metadata, ACP SID and a unique event UUID stay in
the envelope. That UUID also becomes native `clientUserMessageId`. Independent
events with identical bodies are not deduplicated by a content hash.

A `queued` record confirms the real queue receipt, not a read or native turn
acceptance. After possible submission, cancellation, malformed/lost responses,
RPC errors, late producer loading and forced cleanup are **Unknown**. Available
queue/event/process evidence is retained. No automatic resend or transport
fallback occurs. Before submission, a local configuration or producer admission
failure is recorded as not sent. Operator recovery must inspect the original
event; an absent queue entry does not establish that it was never consumed.

## Actual native acceptance

The reusable fixture uses a private home, the real native executable, an actual
TUI driver and a loopback model endpoint. It launches the target with
`--no-daemon`, confirms original idle completion and no control socket, then
feeds one authorized peer notification through the shared callback. It verifies
empty producer inventories, zero producer inference, original target PID/thread,
exactly one queued native turn, the preserved envelope and normal producer/TUI
exit. HTTP requests and native turns are reported separately.

Run `npm run test:codex-queue -- --codex ABS_NATIVE --output ABS_EVIDENCE --case
idle`, then `--case response-loss`. Windows supplies `--driver JSON_ARGV` with
Python and the committed ConPTY driver. Process attribution uses `lsof` on
macOS/Linux or built-in PowerShell `Get-NetTCPConnection` on Windows; inability
to attribute a request fails the fixture. `--library ABS_INDEX_CJS` tests a
clean source-bound offline runtime outside the checkout.

The loss fixture wraps a real producer, drops its actual committed queue reply,
proves its inventory remains empty and lets it exit. The shared callback must
record Unknown; the original target alone consumes one event, with another
native scheduling interval observed for no automatic replay. This relay is test
instrumentation and is never installed as the production producer.

These fixtures do not prove Node authentication, Windows behavior before the
Windows fixture runs, or recovery of a production thread. Qualification and the
existing-owner activation remain the migration coordinator's responsibility.
