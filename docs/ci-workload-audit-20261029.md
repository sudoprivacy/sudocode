# CI workload audit, 2026-10-29

## Decision

Reduce repeated suite executions before removing distinct user journeys.
Keep cache-prefix, compaction, resume, rendering and platform-specific native
terminal acceptance. No test is deleted merely because no failure was recorded.

For a ready PR, one queue group and the resulting main push:

| Work | Before | After |
| --- | --- | --- |
| Complete deterministic workspace suite | 2 PR + 3 queue + 3 main = 8 | 1 Linux PR + 3 queue = 4 |
| Native terminal resize suite | 2 PR + 3 queue + 3 main = 8 | 1 Linux PR + 3 queue = 4 |
| Release startup/streaming acceptance | 2 PR + 3 queue + 3 main = 8 | 1 Linux PR + 3 queue = 4 |
| Real daemon A2A acceptance | PR + queue + main = 3 | queue = 1 |
| Shared-service acceptance | 4 PR + 4 main = 8 | 1 Linux PR + 4 queue = 5 |
| Specialized PTY/stderr pre-run steps per workspace job | 8, then the same cases in the workspace suite | 0; cases remain in the workspace suite |
| Benchmark API compilation | Optimized build/link of workspace benchmarks and tests | Locked benchmark type-check using the workspace job's debug graph |

Manual dispatch retains full validation. Existing experimental branch pushes
retain the three-platform Rust jobs. Paid provider jobs continue on main and
manual dispatch; this change does not reduce their API calls. The manually
disabled paid code-review workflows remain disabled.

Required check names and branch protection remain unchanged. Windows/macOS jobs
report skipped on PRs without allocating runners and run in the merge queue.
The integrated commit must pass all three platforms before landing. Rendering
budgets, workloads, A/A calibration and sample counts are unchanged. Additional
source before/after viewport diagnostics require an explicit label.

Workspace tests and Clippy omit disposable debug-symbol output while retaining
debug assertions. Their shared environment keeps Cargo cache inputs aligned.
Release resource acceptance retains its normal release profile. The browser
workflow triggers on Rust, its terminal harness and its own workflow changes;
an unrelated workflow edit alone no longer starts two browser release builds.

## Dependency cache within the existing quota

Complementary native jobs use the same cache environment as the main publisher.
`rust-cache` always fingerprints `CARGO*` variables and adds `env-vars` to its
default prefix list; `env-vars: ImageOS` does not replace that list. Keep
`CARGO_TERM_COLOR=always` and the two debug settings aligned. The action itself
sets `CARGO_INCREMENTAL=0`; the trusted command runner applies its one-job limit
after restore so that scheduling does not split an otherwise identical cache.
This fixes a key mismatch, not a measured cache-hit-rate claim.

The boundary job no longer downloads and compares `vfs.proto` a second time.
The required pin job retains the exact pinned-revision byte comparison once per
event. This removes duplicate validation without removing the protocol guard.

Keep the repository's 10 GB cache limit. Only a main-branch publisher writes
the shared Rust dependency caches; PRs, merge groups, release and provider jobs
read them. A PR or merge-group cache is isolated from unrelated PRs, so those
writes previously consumed storage without supplying a shared default-branch
cache. Formatting does not need a Rust build cache.

Use one dependency pool per runner image and architecture, including `ImageOS`
in the environment key. The latest Linux, Windows and macOS workspace hosts
store dev/test and normal release dependencies together, sharing the Cargo
registry and Git sources. Service-only macOS 15 ARM64/Intel and distribution
hosts compile normally on a miss. Do not restore compiled artifacts across
macOS 26/15 or Windows 2025/2022 compiler/SDK boundaries.
Omit dev/test debug symbols while
retaining debug assertions; no release profile, LTO setting or acceptance
budget changes. The cache action excludes workspace artifacts and incremental
output as before. Do not discard all compiled dependencies merely to reduce
storage: cold compilation is itself a major CI cost.

On main, an exact cache-key lookup skips download and compilation entirely.
A miss restores the previous dependency key and compiles without executing
tests. This compile-only maintenance is additional main work and is included
in the tradeoff; removing duplicate main test runs does not remove all main
builds. The independent publisher keeps its current run when main advances,
so frequent merges cannot repeatedly cancel a cold cache build. GitHub retains
the latest pending publication. Manual cache-only dispatch validates publication without provider keys
or E2E execution. No paid cache-capacity or billing settings change.

The completed [five-image cold publication](https://github.com/sudoprivacy/sudocode/actions/runs/40858182147)
passed on every host and uploaded 9,768,340,446 compressed bytes. The three
workspace pools account for 6,543,799,447 bytes; removing the two service-only
publishers reduces this measured set by 33.0% and leaves space for key turnover.
That is archive footprint, not an equivalent reduction in runner time. macOS 15
service builds lose those warm dependencies, while the more frequently used
workspace pools gain headroom. Future archive sizes, sustained hits and net
build time still need hosted observation. Legacy writers continued to publish
isolated caches during the transition and evicted the new pools. Cache misses
must remain correct.

References: [GitHub cache scope and eviction](https://docs.github.com/en/actions/reference/workflows-and-actions/dependency-caching),
[rust-cache cleanup and shared keys](https://github.com/Swatinem/rust-cache/blob/master/README.md).

Browser release comparisons also restore the candidate's normal release
dependency graph after compiling its test driver. Cargo may otherwise replace
the candidate CLI with a dev-dependency graph while the baseline uses the
production graph. No build occurs between freezing the final pair and measuring
them. That correction adds a build step to preserve a valid comparison; it is
not claimed as a workload saving.

### Integration-only edits and obsolete queue runs

Ordinary `rust/crates/*/tests/**` changes do not change the production CLI, so
they no longer compile and measure a fresh release pair. The headless benchmark
still runs for production Rust, manifests, vendor code, the measurement harness,
shared CLI test helpers and `pty_render_performance`. The browser workflow keeps
its shared helpers, `pty_hyperlinks`, terminal harness and own workflow triggers.
Renames are checked at both old and new paths by the headless policy. Test
execution and all resource budgets are retained.

At 12:50 UTC, 12 active merge-group runs referred to removed queue branches and
commits absent from the current queue. All 12 cancellation requests succeeded:
six Rust CI, three rendering and three history workflows. Their old Git refs
were checked again immediately before cancellation. Current queue runs were
retained. Queue refs include a changing base SHA, so existing concurrency keys
do not cancel these obsolete runs when main advances.

Default-branch queue maintenance now previews/cancels only `merge_group` runs
older than two minutes whose queue ref is gone and whose head is absent from
the live queue, rechecking identities before mutation. It never executes PR
code with its Actions write token. API errors stop cleanup rather than treating
a missing lookup result as permission to cancel. Run pagination finishes before
cancellation so a shrinking result set cannot skip later pages. This adds a
small maintenance job per new queue group/main push; current runs and unique
test journeys remain unchanged.

The 50% figure describes full-suite execution copies. It is not a measured
50% reduction in wall time, billed usage, unique test cases or paid API calls.
For example, removing an optimized benchmark compilation moves some cold
dependency compilation into the release build that still has to run.

## Data and method

The fixed creation window is 2026-09-29 01:50:06 UTC through
2026-10-29 01:50:06 UTC, for `sudoprivacy/sudocode`:

- 8,323 workflow runs, enumerated by daily pagination rather than accepting a
  truncated month-wide search result.
- 8,436 attempts including retries, with metadata for 54,131 jobs. No missing
  attempt metadata remained after collection.
- 954 attempt log bundles with a failed or timed-out step, all downloaded and
  parsed. Earlier failed attempts remain counted even when a rerun succeeded.
- 240 distinct target-qualified failed-case identifiers, including 160 PTY
  identifiers. Provider quota, setup, teardown and other fixture failures are
  included; these identifiers are not a count of independent product bugs.

Runner duration is assigned-runner start to completion. Queue delay is job
creation to assigned-runner start; jobs that never started do not enter those
percentiles. Totals are unweighted across operating systems and are not a
billing statement. Jobs were fetched after their runs were created, so this is
a creation-window cohort rather than an exact invoice-period total.

| Observation | Result |
| --- | --- |
| Windows workspace PR job median | about 31 minutes |
| Windows workspace PR job P90 | about 42 minutes |
| Release rendering A/A and A/B job median | about 41 minutes |
| Windows optimized benchmark compilation job median | about 20 minutes |
| Linux merge-queue job median wait, depending on job | about 50–68 minutes |
| Workspace test steps, accumulated runner time | 64,640 minutes |
| Optimized benchmark compilation steps | 41,215 minutes |
| Separate cache-prefix/compaction pre-run steps | 13,772 minutes |
| Native resize steps | 14,160 minutes |
| Rust-cache post steps | 13,537 minutes |

The snapshot of all 91 organization repositories at 2026-10-29 04:43:43 UTC
showed two assigned in-progress jobs. That snapshot does not establish the
cause of prolonged runner provisioning delays; assuming every queued job was
waiting for a saturated standard concurrency limit would be unsupported.

## Interpreting zero recorded failures

The audit did not collect a complete per-case execution denominator from every
successful run. Cancelled attempts without failed-step conclusions were not
all parsed. A case with no matching failure in this collection therefore has
no recorded failure here; it is not proven to have never failed.

For a concrete sample, the completed Ubuntu workspace log in
[run 40788908391](https://github.com/sudoprivacy/sudocode/actions/runs/40788908391)
records 592 passing PTY cases. Of those, 449 have no matching parsed failure in
the month collection. That confirms execution in this attempt, not a complete
month-wide execution count or an argument that the cases are unnecessary.

Even with complete data, zero failures can mean a stable critical feature,
limited execution, weak assertions or obsolete behavior. Removal needs a
specific argument: identical assertions already covered by another executed
journey, a retired feature, or evidence that the case cannot detect its stated
regression. Repeated execution of the same case in the same job has no such
independent coverage value. Platform-specific journeys remain necessary.

The benchmark compilation step itself failed in 18 attempts during the window.
Its API-drift protection is retained as a type-check rather than discarded.
Cache-prefix and compaction targets had 37 and 20 failed-case observations
respectively in those bundles. Their cases remain in the workspace run;
removing their separate pre-run eliminates repetition, not those assertions.

One reviewed case also had avoidable fixed waits and a weak request-count
assertion: `pty_repl_async_batched_flush`. Keep that journey, but use a real
tool's fresh readiness/release files and rendered queue receipts instead of a
30-second sleep. Verify the successful tool result, exactly one combined
request containing each of three queued inputs once, and the persisted answer.
The controlled provider checks client queue semantics; it earns no live-model
coverage credit. The separate real lease-expiry journey keeps its timed wait.

## Validation and follow-up

The second cleanup passed actionlint, Python/JavaScript syntax checks, the
nine-event workflow truth table and all 12 required-context checks. Its live
policy contract made 32 authenticated, read-only GitHub API calls: test-only
PRs #1105/#1084 skipped release measurements, production-dependency PR #1116
retained them, and docs-only PR #1111 skipped them. All four rejected stale
heads. The cleanup script's live preview retained all four current queue runs
and found no remaining obsolete run after the 12 cancellations above.

Workflow lint, required check names, draft/ready PR, merge-group, main-push and
manual-dispatch behavior must be checked before landing. Observe fresh runs
after landing and compare runner time and queue wait separately. Keep original
failed/noisy performance evidence; scheduling noise is inconclusive, not a pass.

The canonical roadmap Goal 1 and contributor guide describe this cadence.
Refresh the existing ShareOne roadmap only after its Git-backed source lands,
then download it and verify that the served content matches the source.

## Mandatory local execution and complementary platforms

The user clarified the host split: a Windows author runs Windows locally and
CI supplies macOS; a Mac author runs macOS locally and CI supplies Windows.
One Linux native run is retained. No persistent Mac, SSH access, self-hosted
runner service, scheduled benchmark or optional manual replacement is required.

The managed pre-push hook runs changed-input profiles, signs commit/tree/base/
policy/host/command/log digests and publishes a Git note. Protected-main code
verifies evidence before complementary jobs run. All workspace cases, resize,
service journeys, startup and co-host size budgets stay in shared profiles.
Full rendering A/A/A-B and painted-browser/viewport measurements move to local
Docker without changing their existing sampling or budgets. Tagged production
artifact publishing retains its multi-platform builds.

Static cloud workspace cadence is now two executions per ready Windows/Mac PR
lifecycle, versus four after cleanup one and eight originally; a third execution
is native on the author's machine. Co-host PR builds use the author host plus
two complementary CI hosts instead of five cloud builds. These counts cannot
be added to historical runner-minute percentages. Observed latency and net
runner-minute savings remain unmeasured until fresh runs complete.

The merge queue carries each exact PR head's completed acceptance workflow and
checks its composed Git history. It does not rerun runtime suites on the
integrated group. Interaction regressions unique to that combination are an
explicit coverage tradeoff. A device signature authenticates its enrolled
operator's claim, not remote execution. New public keys require main review;
private keys and full local logs never enter Git. The protected main gate also
rejects self-approved policy changes.

The guard E2E uses actual temporary Git repositories, SSH signatures and
pre-push transports: a failing executable blocks push; a successful one
automatically publishes evidence; a new head, tampered receipt and dirty
source fail. The local Docker image additionally launched Chromium and
verified actual input. These are enforcement/environment checks, not claims
that every product profile or a macOS native run was executed on Windows.

## Fail-fast and unchanged-input follow-up

Native, SDK, installer and live-model matrices cancel peers after the first
failed job. Native commands already stop sequentially on their first nonzero
exit; original logs remain. The independent publisher checks actual jobs, so
the extra summary runner and scheduling hop are removed.

Signed local evidence now binds executor/profile/image commands separately
from scheduling/publication metadata. A scheduling-only update does not require
rerunning already completed commands; the current required set must still be a
subset of the signed passes. Changed commits/commands or added checks still
require real acceptance. A real bare-Git E2E verifies this reuse across a
protected scheduling update.

Changed Git inputs also gate main's paid provider acceptance and the queue's
real-daemon acceptance. Docs/CI-only edits do not change their test inputs.
Rust, cache specs and relevant protocol/daemon harness changes retain their
respective checks; explicit provider dispatch remains explicit. The input
selector is exercised against actual Git commits for docs, CI, Rust, daemon
harness and cache-spec changes, through both push and merge-group payloads.
No independent product case or budget is removed in this follow-up.

## One shared preflight instead of seven short runners

Combine format/schema, docs, architecture boundary, pinned protocol, release
mirror, provider probe and PR-history jobs into the required `CI preflight`
job. Keep their original commands and real Git/process scenarios. Share one
checkout, one Python/Node/Rust environment and one queue admission; stop the
job and its expensive daemon/provider dependents on its first failed step.
The protected acceptance gate checks integrated queue history, so preflight
does not repeat that same range. All PR paths trigger preflight, including
SDK-only edits. Cache-only dispatch remains compile-only.

This removes six hosted jobs per PR/queue event. It is a scheduling count,
not a 7x latency claim: commands now run sequentially, and runner queue delay
still needs measurement. No independent product assertion or deadline changed.
Required contexts migrate only after the new job has an actual hosted pass.

The first hosted consolidated preflight passed all steps in 29 seconds after
about 5m45s of queue wait (run 40906797832). Its actual success allowed the
required contexts to become `CI preflight`, `Local acceptance` and the unchanged
real-daemon guard. This is one CI-only PR, not a comparable production latency
sample. Two retired PR-specific dependency pools were also deleted by exact ID,
reclaiming 3,272,376,987 bytes while retaining all three main pools.

Metadata-only signature verification and trusted publication use Windows
runners instead of adding two more Linux admissions. Windows/macOS cache jobs
started while the Linux peer remained queued; this is a capacity experiment,
not a permanent guarantee that one host is faster. Pin Python 3.12 and use
`sys.executable` for the queue history subprocess. Platform/runtime acceptance
is unchanged. Compare the actual gate-to-publication path before claiming a
latency improvement.
