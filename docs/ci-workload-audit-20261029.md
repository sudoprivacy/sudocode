# CI workload audit, 2026-10-29

## Decision

Reduce repeated suite executions before removing distinct user journeys.
Keep cache-prefix, compaction, resume, rendering and platform-specific native
terminal acceptance. No test is deleted merely because no failure was recorded.

For a ready PR, one queue group and the resulting main push:

| Work | Before | After |
| --- | --- | --- |
| Complete deterministic workspace suite | 2 PR + 3 queue + 3 main = 8 | 2 complementary CI hosts + 1 author-local host |
| Native terminal resize suite | 2 PR + 3 queue + 3 main = 8 | 2 complementary CI hosts + 1 author-local host, for changed inputs |
| Release startup/streaming acceptance | 2 PR + 3 queue + 3 main = 8 | 2 complementary CI hosts + 1 author-local host, for changed inputs |
| Real daemon A2A acceptance | PR + queue + main = 3 | queue = 1, for changed daemon inputs |
| Shared-service acceptance | 4 PR + 4 main = 8 | 2 complementary CI hosts + 1 author-local host, for changed inputs |
| PR co-host build/size verification | 5 hosted platforms | 2 complementary CI hosts + 1 author-local host; actual tagged releases retain 5 platforms |
| Short metadata and integrity jobs | 7 hosted jobs per event | 1 shared preflight job per event |
| Specialized PTY/stderr pre-run steps per workspace job | 8, then the same cases in the workspace suite | 0; cases remain in the workspace suite |
| Benchmark API compilation | Optimized build/link of workspace benchmarks and tests | Locked benchmark type-check using the workspace job's debug graph |
| ACP package integration | 4 PR hosts + 4 main hosts | 4 PR hosts; main keeps exact-merged-revision builds and package artifacts |

Provider assertions remain on main for changed provider inputs and explicit
dispatch. Rendering/browser checks are mandatory locally, with their original
budgets and sample counts. The manually disabled paid review stays disabled.

Required contexts are `CI preflight`, `Local acceptance` and the real-daemon
guard. The queue carries the exact PR heads' completed native acceptance and
verifies combined Git history; it does not repeat native suites on that combined
tree. Runtime interactions unique to the combination are outside that pre-merge
coverage. Admin enforcement and the queue ruleset remain enabled after the
explicitly authorized temporary migration route.

Workspace tests and Clippy omit disposable debug-symbol output while retaining
debug assertions. Their shared environment keeps Cargo cache inputs aligned.
Release resource acceptance retains its normal release profile. Browser
measurements run through the changed-input local Docker profile.

## End-to-end acceptance metric

The primary clock starts at the first recorded local validation/acceptance
operation and ends at the first local or hosted failure, or at merge. Include
local setup, compilation, test execution, benchmark-lock waiting, publication,
hosted queueing, retries and merge queue. Keep both whole-PR and final-head
series. Moving work onto the author's box is not a reduction in total time.
Runner-minutes and execution counts are supporting measurements.

`check` writes `attempt.json` for running, successful and failed attempts and
prints `LOCAL_ACCEPTANCE_TIMING` on the mandatory interface. Signed full receipts
and completed-profile checkpoints retain the local start and earliest recorded
start from a verified resume chain. `LOCAL_ACCEPTANCE_OPERATION_TIMING` also
covers the outer push/contract operation, including authority checks and Git
publication; explicit output directories retain it as `operation.json`.
Use the earliest applicable timestamp when joining local records to hosted PR
outcomes. Do not add overlapping operation/check intervals together.
Abrupt termination can leave a running record without an end; keep it censored.
Older or manual attempts can be unknown. Missing local history must not be
replaced with zero or silently included in a complete end-to-end average.

The provisional September 29–October 29 Rust source/build cohort contains 419
failed candidate heads averaging 32.252 minutes to first failure and 200 merged
PRs averaging 185.261 minutes from their final candidate's CI trigger to merge.
The corresponding hosted-clock reference targets are 3.225 and 18.526 minutes.
Historical local timing was not collected, so these are conservative reference
thresholds, not a measured historical total-time baseline. Scope comes from final
PR diffs; open outcomes are censored and same-head human idle time remains in
merge elapsed time. These are elapsed means, not the older completion medians.

Early post-rollout observations through October 30 00:29 UTC contain 17 merged
CI/test/documentation heads averaging 4.215 minutes and seven failed heads
averaging 1.488 minutes. There is no comparable completed production Rust sample
in that cohort. This does not establish the overall target. One subsequent
local acceptance waited 6,913.983 seconds for the benchmark lease and eventually
failed a real terminal exit check; that incomplete run is not a fast pass.

## Browser build reuse and selected execution evidence

Rendering and browser profiles share the candidate's canonical source checkout
and Cargo output directory. Browser drivers use that same candidate tree too;
the independent baselines and rendering's trusted driver stay separate. Copy
production binaries before test builds, because dev dependency features can
change the CLI binary in Cargo's output directory. This reduces the original
seven target trees across both profiles to four (from five after the first reuse
change). The existing exclusive lease still covers mutation, compilation and
measurement, so another head cannot replace inputs mid-run. Full-profile savings still need
measurement. No workloads, sample counts, budgets or product cases are removed.

`benchmark_targets_acceptance.py` executes both production builders with
real Cargo, npm and native fixture drivers in private state. It checks distinct
baseline/candidate results, dev feature unification, immutable measured binaries,
both observer stages and Cargo-fresh viewport dependencies. The complete
rendering-to-browser sequence must keep the same normal candidate bytes and
reuse the candidate build without compilation; restoring separate candidate
paths/targets makes the real Cargo regression fail. CI runs it only for changed executor,
image, fixture or workflow inputs on the PR; queue/main reuse that execution.
These are infrastructure checks and earn no product journey coverage credit.

Selected execution fingerprints retain module import effects, process execution,
images and the helpers reached by the selected dispatcher. Reflection or a
rewritten profile parameter retains the complete module conservatively. A
browser-only helper edit can therefore preserve unchanged native evidence.
New receipts bind both fingerprint recipes to an immutable `commands_ref` and
the complete signed command hash. Existing recipes are recovered only from their
actual attested source. Real Git pushes and SSH notes exercise old/new receipt
formats, unused and transitive helpers, changed shared/executor/image inputs,
required profiles, reflection, malformed hashes and invalid source references.

## Measured timing and duplicate-test review

The month contains 540 successful PR heads with all three original native
workspace jobs actually passing. Rust CI alone has a 52.5-minute median elapsed
time. Of these, 449 heads have every latest applicable PR acceptance workflow
successful: their completion envelope has a 50.35-minute median and 191.13-minute
P90. The smaller second cohort explains its lower median. Both exclude local
pre-push time and merge queue, and the second excludes paid Claude review;
neither is a complete submit-to-merge baseline. Do not compare a CI-only PR to
these production cohorts and call the 10% end-to-end goal achieved.

A syntax-tree scan covered 552 Rust files and 2,795 explicit test functions,
preserving literal and helper names. It found no identical PTY test bodies.
Three small `env_guard_recovers_after_poisoning` bodies match textually but
exercise three separate crate-owned guards. Keep them; their deletion offers
negligible runtime savings. Macro-generated cases and semantic overlap require
separate review. The same ACP subagent scenario also runs against different
GPT and Claude routes, so a repeated test name alone does not establish redundant
provider coverage.

Preflight runs while a PR is draft and does not depend on draft state. Promoting
the same commit to ready reuses that result instead of starting it again.
The separate native acceptance workflow retains `ready_for_review`, because its
real complementary hosts deliberately wait for readiness.

An isolated Linux experiment kept the same release CLI, exact 64 test identities,
one CPU, non-root user, test priority and existing assertions/deadlines:

| Runner | Two concurrent tests | Four concurrent tests |
| --- | --- | --- |
| Cargo | 490.7 s | 329.6 s |
| nextest 0.9.150 | 483.4 s | 274.2 s |

These are 63 controlled-provider PTY cases plus one live-only early return under
mock, not 64 real API successes. Each setting has one trial. Cargo's four-thread
run was 32.8% shorter than its two-thread control; nextest was 16.8% shorter than
Cargo at four threads. The similar two-thread timings do not support attributing
all improvement to a new runner. No nextest dependency or global test-concurrency
change is adopted from this experiment. Hosted production latency is unproven.

The new narrow CLI scope was also exercised through the real acceptance executor
against a test-only Git commit and unchanged production Rust. Its mailbox-enabled
compaction target reported 34 passes (33 controlled-provider PTY cases plus one
live-only early return). The release-profile experiment reused the owned build
output; it does not claim the dev native profile or real provider acceptance.
All observed compiler/linker workers had nice 10 and actual Cargo/PTys had nice 0.
The owned container was stopped and its original launcher restored afterward.

Only 7 of 231 merged PR file scopes in the month window fit the narrow standalone
CLI-test pattern before checking deletions/target existence. The optimization
helps that iteration class; it is not a global 10% latency result. Existing
production-source acceptance remains complete.

## Dependency cache within the existing quota

Workflow-only changes unrelated to `ci-dependency-cache.yml` do not start its
three host jobs. Rust pushes still check for a new dependency/toolchain/image
key, and changes to the publisher still exercise it. Cache-hit correctness and
source validation are unchanged.

Local benchmark checkouts preserve the mtimes of identical source bytes and
executable modes, so Cargo can reuse valid artifacts at their existing canonical
source/target paths. Changed bytes receive fresh mtimes even when their Git archive
is old. Unexpected cached sources are removed and cached entry links are replaced
without traversing them. The existing exclusive Linux lease and separate legacy
namespace continue to protect shared state. Blocked callers now print the lock
path and report their acquisition wait in the execution log.

The mandatory real Git/Cargo fixture verifies unchanged-input freshness, changed
old archives, same-size/mtime tampering, and an injected-source compiler failure
followed by successful cleanup. Linux also checks executable permissions, links
outside the cached checkout and two processes competing for the actual lease.
These are infrastructure acceptance checks, not additional product journey credit.
Actual multi-PR waiting-time savings still require observation.

On a cache miss, the publisher compiles library/bin test targets with the same
mailbox feature, retaining normal and dev dependencies. It omits standalone
integration executables: the default cache action removes workspace artifacts
before saving them, so those links do not warm an additional dependency. Native
acceptance still builds and runs the complete selected journeys. Production
release dependency warming, all three host pools and the 10 GB quota remain.
See the action's [cache contract](https://github.com/Swatinem/rust-cache#cache-details).

A real Windows Cargo fixture built both selections from separate empty targets.
The reduced selection retained an integration-only dev dependency outside the
workspace and the enabled library feature, while eliminating five integration
executables. The fixture validates selection, not a percentage of cloud savings.
Previously measured 139 CLI integration compilations consumed 302.26 compiler
seconds in one single-worker desktop build; this is an opportunity estimate,
not a hosted latency reduction. Existing cache hits skip compilation already.

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

Each invocation resolves its candidate, PR base and authorized policy to fixed
Git commits before running profiles. Docker inputs use that policy commit for
the observer, image context and rendering baseline selection. A maintainer's
authorized contract update therefore executes its selected candidate tools;
ordinary acceptance continues to use protected-main tools. Later fetches cannot
silently change these inputs. Docker runs use the image ID written by the actual
build, rather than resolving a mutable tag again. `container-provenance.json`
records the candidate, baseline, policy, context digest and image identity.
The dispatcher keeps its existing API. Comparing the actual Git-tracked command
recipes preserves all eight non-container profiles and invalidates only
`rendering` and `browser`; unrelated native evidence need not be repeated for
this container implementation change.

The existing Git/SSH guard exercises a real signed check while its policy/base
refs change. The container infrastructure E2E executes real Docker builds and
runs, moves Git refs and replaces a tag, and requires a wrong-policy run to fail.
It uses private fixture storage and a labelled lease; no production benchmark
state is mounted. Relevant executor changes select this fixture once in the PR
preflight, with no queue/main repeat or extra job. These fixtures add no product
coverage and remove no workload, measurement, assertion or deadline.

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

CLI integration-test modules under `rust/crates/rusty-sudocode-cli/tests/` do
not enter the daemon/cohost binaries or their shell journeys. Test-only edits
there retain author/complementary workspace acceptance and the existing main
provider scope, but skip unchanged daemon queue work. Manifest/source changes,
daemon/service harness edits and renames across the production boundary retain
the daemon check. Real Git fixtures exercise push, PR and queue events, plus
the source rename and explicit dispatch. No daemon scenario is deleted.

For an isolated change to standalone CLI `tests/*.rs` modules, native workspace
acceptance selects those actual Cargo integration targets with the existing
mailbox feature. It keeps formatting and reuses unchanged schema/build-metadata,
Mermaid, benchmark and production Clippy checks. Only root README and Markdown
docs may accompany this narrow case. Shared helper/fixture/spec changes, target
deletion, production inputs and cross-boundary moves fall back to the complete
workspace. Other selected resize/release/services/cohost and local measurement
profiles are unchanged. Real Git scenarios cover one and multiple targets,
docs, common helpers, specs, manifests, deletion and a move into production.
No test target is deleted; unchanged target executions are omitted for this
specific input class. Its hosted latency benefit still requires observation.

Before extracting the repeatedly compiled PTY helpers, enroll their shared
test-crate path as a native resize input. Existing broad Rust source rules
already retain workspace, rendering/browser and release acceptance. A real
Git change at the shared helper path verifies the selected native profiles.
This policy preparation does not claim the helper extraction has landed, and
does not remove an existing scenario or input guard.

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

Main pushes reuse pre-merge integrity checks instead of repeating formatting,
schemas, docs, architecture, release-mirror fixtures and provider-probe fixtures.
The protocol pin and changed-input scope remain unconditional because downstream
daemon/provider jobs consume their outputs. PR, queue, experimental pushes and
explicit runs retain their existing preflight checks. This saves repeated setup
and commands after each merge; it does not itself shorten the PR critical path.

The real Git enforcement fixture clears repository-local environment variables
exported by a caller's hook before creating its temporary repositories. Its
acceptance run deliberately inherits another temporary repository's Git directory,
worktree, index and injected config, then verifies that caller's HEAD, index,
config and source remain unchanged. This protects ordinary managed pushes as
well as direct fixture execution.

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

The shared executor's one-worker default protects the author's foreground
desktop. The complementary hosted wrapper overrides only the compiler worker
count to the available hosted CPUs after restoring dependencies. This removes an
unnecessary hosted build throttle without changing cache keys, source profiles,
test concurrency, assertions or deadlines. Local/manual execution remains one
worker. Actual hosted build-time benefit requires a comparable native run.
GitHub's public standard Linux/Windows runners currently provide four CPUs,
and ARM macOS provides three. The earlier two-worker hosted cap left that
capacity unused. See [GitHub-hosted runner specifications](https://docs.github.com/en/actions/reference/runners/github-hosted-runners).

## ACP package runs

The initial consolidation kept integration and portable installed-package
journeys on all four PR hosts. This duplicated the author's local platform and
left SDK execution outside the signed local profile. The subsequent SDK
integration fixes that gap: `native-sdk` is mandatory for SDK inputs, its actual
commands run before push, and the shared acceptance plan schedules only missing
targets in CI. Linux x64 and ARM64 remain distinct required targets; macOS and
Windows use OS-level source coverage. SDK-only jobs need Node, not a Rust setup.
Main still builds and packs its exact merged revision on each host, retaining
the commit-bound manifest, archive integrity receipt and uploaded artifacts.
It does not repeat the same integration journeys. As with native queue reuse,
an interaction unique to merged changes is outside that repeated runtime
coverage. Explicit runs retain integration; a new PR/ref head cancels obsolete
acceptance runs. No scenario or distribution platform is deleted.

The plan is printed by the hook (including when reusing a valid receipt), signed
with the command results, and reused by both the read-only CI gate and status
publisher. CI summaries show each check/target assignment; `Local acceptance`'s
status description exposes the host split to `gh` and API clients directly.
Missing SDK commands and mismatched signed plans are refusals. A passing source
receipt explicitly leaves installation and deployed-system acceptance unproven.

## Detect real conflicts before local builds

The Nexus workline's `5e191b85` SudoCode acceptance passed its eight profiles in
9,043.535 seconds, then discovered three merge conflicts. Those same paths already
conflicted with `b885db9f`, the protected main revision at the start of that run.
The old history checker accepts this pair; the new actual-Git preflight reports
the conflicts in 1.038 seconds. This is a replay of the known inputs, not a measured
production rollout saving. The full run and its timing remain in the record.

History preflight now checks both its supplied target and fetched `origin/main`,
because local acceptance supplies an ancestral diff base. Git's merge-tree leaves
the checkout and index untouched. Its required output lists exact paths and the
next step, including machine-readable `LOCAL_ACCEPTANCE_MERGE_CONFLICTS`. Clean
branches behind main remain valid; changes landing after this check can still
introduce conflicts. Provenance and conflict-marker checks remain.
The protected dispatcher stages the hash-pinned checker from its frozen policy
instead of executing the older copy in an author branch. Real native-process/Git
acceptance exercises that old checkout with a deliberately unusable local checker;
the protected checker runs and the author HEAD, index and source remain unchanged.

The Git-only `e2e/pr-history/run.py` regression remains mandatory in CI preflight.
Editing that exact fixture alone no longer selects unrelated native product
builds on the author host and both complementary CI hosts. Public-planner Git
regressions retain native validation for mixed source, unknown fixtures and
renames crossing this boundary. Product scenarios and their assertions stay.
