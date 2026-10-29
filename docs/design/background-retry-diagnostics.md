# Background retry diagnostics

A laptop waking or changing networks can interrupt the metadata writes that
claim conversation readers. Independent receiver threads previously printed
the entire backend error on every attempt. With several peers, one interruption
scrolled the prompt repeatedly even though the receiver was still retrying.

`telemetry::RetryDiagnostics` owns the reporting policy. Callers retain their
existing retry policy and error results. One service shares a reporter across
its workers; the reporter has no connection, receiver, task, timer thread or
authority to acknowledge work.

The first error, changed errors and repeat counts go to the existing rotated
local `scode.log` / `sudoclaw.log` sink (`SCODE_LOG_PATH` still overrides it).
These are local diagnostic events, not model messages or remote telemetry.
`RetryNotices` selects presentation without changing aggregation. Ordinary
receivers use `LogOnly` and retain their latest error in the existing receiver
status and tracing channel. They never write a terminal notice. Completion
persistence and hosts which explicitly own terminal presentation use `Terminal`:
the first failure prints a short notice with the log path, and ongoing failures
produce at most one reminder per minute across all scopes. In that mode an
unavailable log falls back to a bounded notice retaining the original cause.
Notices use fallible stderr writes: a closed terminal or a
descriptor handoff must not panic the observing service. The original operation
is recorded before its notice. A failed notice gets a separate local
`background_retry_terminal_unavailable` event with its IO error kind; it is
never retried, since a prefix may already have been displayed. Reporting remains
best effort and does not change the work's actual result. A log-only caller
continues to own its operation error even when the diagnostic file is unavailable.

A scope identifies an independently failing operation. A healthy conversation
does not clear a different conversation's failure. Recovery is reported only
after all observed failures clear and a healthy check follows a short quiet
period. The message describes receive checks recovering; it does not prove a
peer is online, a send was delivered or every message was acknowledged. Abort,
standby and process exit are not successful delivery.

`outage()` exposes the same rule read-only: when the unrecovered outage began,
and each scope still failing with its latest detail. It is `None` once recovery
is confirmed. The start is set when the first failure turns the outage on and
cleared when recovery turns it off, so a change of scope or error text never
moves it. The mailbox receiver registers its instance with its agent's receiver
state, and peer `health` derives from it. That way the footer and `/status`
apply this rule instead of a second one.

## Repository audit

The audit inspected tracked Rust, TypeScript/JavaScript and shell sources,
then followed error output and retry/reconnect/backoff matches into their
callers. Generated artifacts, vendored dependencies and test-only output were
excluded from production candidates. The initial scan preceded the TypeScript ACP extraction. After normally
merging main, its seven production modules were reviewed too: native failures
and per-notification callback failures remain explicit; there is no automatic
send retry or second background reconnect loop to aggregate.

| Area | Decision | Reason |
|---|---|---|
| `runtime/src/mailbox.rs`: receiver seat, conversation discovery, reader claim, poll and commit | Share one reporter per receiver and the existing status owner | One captured observation supplies status/tracing and aggregated local records. Ordinary receivers select log-only presentation. Original waits, locks, leases, cursor writes and delivery decisions are retained. |
| `engine-core/src/subagent_completion.rs` and `subagent_completion/control.rs`: terminal persistence | Share one reporter per completion state | The observation and delivery paths report the same retained terminal events. Scope uses the existing lifecycle source key (assistant-message identity plus tool-call ID), shared with the pending map; reporting does not change qualification, persistence, retries or receipts. |
| `api/src/http_transport.rs`, request scope and renderer retry events | Keep existing mechanism | Finite provider retries already have their own budget, lifecycle logs and UI notifier. Adding another reporter would duplicate progress and could obscure request failure. |
| `runtime/src/mcp_server_manager.rs`, MCP transports/lifecycle | Keep existing mechanism | Initialization limits, connection state, typed refusal and explicit reconnect already belong to the manager. There is no matching unbounded per-tick stderr loop to replace. |
| `sdk/typescript/src`: canonical ACP/MCP/callback host | Keep explicit protocol errors | The child stderr has its own configured local file. Per-command audit errors and per-notification callback errors are separate outcomes; no automatic send/reconnect retry exists. Adding a TypeScript copy of the Rust reporter would duplicate policy without a matching loop. |
| `nexus-vfs-client/src/lib.rs` | Keep error propagation | The transport returns errors to its caller and repairs its channel; the receiver is the reporting owner. Logging again here would duplicate the same incident. |
| `runtime/src/bash.rs`, tools, todo/config persistence, startup and ACP reply-send errors | Keep explicit errors | These report an individual operation or an uncertain result, rather than an owned background retry loop. In particular an uncertain send must never acquire automatic retry. |
| Telemetry sink write/rotation failures | Reuse a fallible writer | The regular sink keeps its fallback. The retry reporter calls the same serializer/rotation path through `try_record`. Terminal presentation owns a bounded fallback; log-only services retain their existing status/error owner. Neither recursively reports a log failure through that log. |
| CLI commands, examples, build/install and test harness scripts | Keep command output | Their failure output is the requested command result, not unsolicited background noise. |

## Delivery observation ownership

The landed `DeliveryEvent` is the domain observation at each claim, poll,
commit refusal and rejected batch. The event is captured once, then its inline
observer feeds the existing bounded delivery log, receiver status/tracing and,
for actual I/O errors, the shared retry reporter. These consumers use the same
captured cause, holder, optional offset/tail, stall, typed lease loss and repeat
count. Claim failures keep unknown
positions null. Consumer refusal and lease loss remain structured attribution,
not transport reconnect notices. This includes a commit refused because its
lease lapsed, moved to another holder or answered too late: the captured
`CommitRefusal.loss` determines that category. Register-write errors and renewal
errors returned by the commit's own confirmation retain that returned message.
If the background worker already lost the lease, the commit returns a generic
ownership-loss message; its typed loss retains the worker's error kind, site and
timing, not the original I/O error text. Reader/seat-loss causes remain visible
through the existing status and tracing channel; they are distinct from the
aggregated transport reconnect records. The real-node
composition fixture checks both a lapse without an I/O failure and a subsequent register
I/O refusal in the same original receiver. Retry timings, eligibility and acknowledgment
are unchanged. The receiver's existing state map owns the latest error and
retires it when its last receiver stops; no second status registry is added.

## Terminal ownership

A bounded notice still must not write over live input. Both terminal owners
use the same process stderr capture and streaming UTF-8 decoder. The iocraft
tick drains into its output handle. The synchronous LineEditor forwards into
its native external printer, which preserves the draft and cursor; peer-waiting
notices use that same handle. Its forwarding worker stops after the editor
disconnects the print queue, then restores stderr before joining.
The default receiver emits no display text. The sync capture is active only around readline;
turns and external terminal programs inherit the original stderr target.
Error exits also abort/join the receiver and clear the shared editor in normal
stack cleanup. Its capture worker is never left for thread-local destruction
just because a final transcript save failed.
Rearming waits for an in-flight stderr writer before taking the capture mutex;
the forwarder can still drain that writer's old pipe. Suspension does not wait
for stderr. A retained pipe handle keeps its target, but replacing fd 2 on
Darwin can cancel an in-flight global stderr write with `BrokenPipe`, even
while both pipe ends remain open. The retry reporter handles that error without
panicking; this capture does not promise lossless writes from arbitrary
third-party code which panics on stderr failure.
The forwarder parks only when idle. Windows' existing pipe reader unparks it
when a chunk arrives, so bounded batches do not impose a sleep between pending
chunks. The iocraft owner retains its existing tick; neither path adds a reader.
The completion reporter is boxed behind its lazy cell: healthy completion
states reserve only the cell/pointer, rather than the reporter's map and strings.
This reduces inline layout; it does not establish the rendering workload's
retained state count or explain its measured RSS by itself.

The classic path pins [rustyline 19.0.0](https://github.com/kkawakam/rustyline/releases/tag/v19.0.0),
which includes [the upstream buffered-input fix](https://github.com/kkawakam/rustyline/pull/977).
Versions 15, 17 and 18 check the raw input descriptor before already-buffered
keys when the external printer is enabled. That stalled fast bursts after the
first character and explains why the old classic path bypassed the printer.
The upstream fix keeps input ownership in rustyline; no local cursor painter,
fork or second decoding policy is introduced.

Windows uses std `OwnedHandle` for standard-stream ownership and cloning.
The existing Win32 wrapper crate provides the small standard-handle replacement
operation, preserving aliases to stdout/stdin and returning a displaced handle
only after a successful swap. No pipe/socket classification runs during this
handoff. The existing `filedescriptor` dependency still creates the pipe, before
it has pending IO. `GetNamedPipeInfo` waits behind synchronous IO and was
reachable through generic duplication, raw-handle construction and drop. Std
ownership avoids those queries. No dependency fork or new unsafe-code exception
is introduced; the existing isolated Win32 FFI surface grows only by the owned
standard-handle swap documented in the PR. Original five-second gates and fifteen-second parent budgets remain;
only new Windows execution qualifies the candidate.

Unix keeps its nonblocking-pipe implementation. Windows owns its real stderr
handle through std and the existing isolated Win32 wrapper; one pipe reader
feeds a bounded queue, and the UI tick drains without waiting for IO. The CLI
retains its absolute unsafe-code prohibition and its existing renderer seam.
Windows retains a separate restore handle, preserving stdout/stdin handles
when standard streams share a target. Terminal handoff restores the original
stderr target and resume reinstalls capture. Suspension retains the actual
displaced pipe handle, rather than closing a handle with a synchronous write
still in flight. Repeated suspend/resume calls are idempotent and keep only one
saved handle per target. Teardown closes its queue and pipe without waiting for a library's
retained stderr clone. Headless/ACP output ownership is unchanged.

## Validation

The receiver process fixture uses the existing fault-boundary backend with
real host locks, conversation discovery, two receiver tails, register writes
and natural commits. Only writes to two exact reader registers fail. It asserts
multiple actual retries, one aggregated notice, no early recovery when one peer
is still failing, detailed errors/repeat counts in the log and delivery of both
original messages after the fault clears. The same fixture also runs through
the default log-only receiver, requiring no terminal notice, both peers' actual
errors in the local log, the retained latest status error after recovery and
its removal after the receiver stops. This reaches the reader-claim error
branch; it is not a claim of a real TLS outage or a lease-renewal test.
Additional process cases refuse the log open and, on Linux, fail the actual
write through `/dev/full`, guarding against a second stderr spam loop.
A Linux process case holds the real register fault beyond one production
minute and reconciles the backend's exact failed-write count with the initial,
reminder and recovery records. The reminder's own logged attempt is not a
suppressed repeat. No production interval is shortened for this test.

The offline CLI PTY workflow additionally checks that the original node error
is retained in the diagnostic log while a real model turn, input and original
session resume remain usable. The existing receiver/cursor/completion tests
remain applicable. After the original startup and completed-turn readiness
assertions, the PTY keeps that process alive until its actual background IO
failure appears in the log; a fast Windows mock turn can otherwise exit before
the connection attempt reports a result. The original input/turn deadlines are
unchanged. The original Windows failure and the later Linux #1032 failure
both show raw stderr appended to the ready prompt. The initial capture fix
covered only iocraft; the classic path did not install it. Those failures are
retained, and test context now identifies the actual mode. The input assertion
remains intact.
The existing long UTF-8 diagnostic PTY case now runs in both modes on all three
platforms and requires a clean input line afterward. The sync peer PTY preserves
a Unicode draft and its middle cursor across the real waiting notice, then
processes a control-key burst before its original peer-turn assertions.
Separate subprocess cases gate the actual forwarder and fill the real capture
pipe with an 8 MiB write while holding Rust's stderr lock. One retains a library
handle to the old pipe, so rearm must wait until the gate is released and that
writer finishes. The other writes through global stderr: a short write may
retry against the restored target, or Darwin can report `BrokenPipe`. The
original requirement that this global write must succeed was contradicted by
the retained macOS CI result and an isolated OS-only descriptor probe; the
fixture now explicitly asserts the platform's actual IO outcome. The retained
handle case still requires successful IO and rearm waiting for the writer.
A third case drives the actual retry reporter through the full pipe: macOS
must record one real `BrokenPipe` notice failure, retain both original operation
errors and their scope recoveries, and continue to observe a second operation.
All cases retain the original deadlines and release the gate before reporting
assertions. Raw IO results and handoff stages survive a child timeout.
These isolate pipe/lock ordering with
a controlled printer; they do not replace the native-editor PTY assertions.
Exact-head CI and platform results are reported in the PR;
source preparation alone does not constitute runtime rollout or acceptance.

The retained Windows compaction-conflict failure also exposed a source-refresh
ownership boundary. Reloading the same live transcript must keep that context's
existing writer, which can still be held by a snapshot or completion journal.
Fresh source/checkpoint caches are retained; no second writer is opened. Record
history refreshes the existing continuation, never the latest sibling's view.
An identity/workspace/owner change or a closed/deleted writer remains an error.
The original conflict PTY still requires a successful normal exit. A rejected
identity refresh exercises normal error exit without a TLS teardown abort and
checks that the changed transcript is untouched. New and resumed sessions use
one renderer dispatcher and propagate returned errors to the existing top-level
error handler. A failed final save on resume must not print an error and then
return success. The rejection fixture changes both history and identity, and
checks the stale-history precondition before submitting compaction; a metadata
edit alone does not force this refresh branch. A real record-history runtime
case retains the original writer across refresh and a subsequent model turn
while excluding a sibling's committed private history. These fixtures do not
establish the cause of the original Windows TLS abort on their own.
