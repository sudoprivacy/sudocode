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

Browser release comparisons also restore the candidate's normal release
dependency graph after compiling its test driver. Cargo may otherwise replace
the candidate CLI with a dev-dependency graph while the baseline uses the
production graph. No build occurs between freezing the final pair and measuring
them. That correction adds a build step to preserve a valid comparison; it is
not claimed as a workload saving.

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
| Workspace test steps, accumulated runner time | 63,507 minutes |
| Optimized benchmark compilation steps | 40,259 minutes |
| Separate cache-prefix/compaction pre-run steps | 13,350 minutes |
| Native resize steps | 13,745 minutes |
| Rust-cache post steps | 13,226 minutes |

The snapshot of all 91 organization repositories at 2026-10-29 04:43:43 UTC
showed two assigned in-progress jobs. That snapshot does not establish the
cause of prolonged runner provisioning delays; assuming every queued job was
waiting for a saturated standard concurrency limit would be unsupported.

## Interpreting zero recorded failures

The audit did not collect a complete per-case execution denominator from every
successful run. Cancelled attempts without failed-step conclusions were not
all parsed. A case with no matching failure in this collection therefore has
no recorded failure here; it is not proven to have never failed.

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

## Validation and follow-up

Workflow lint, required check names, draft/ready PR, merge-group, main-push and
manual-dispatch behavior must be checked before landing. Observe fresh runs
after landing and compare runner time and queue wait separately. Keep original
failed/noisy performance evidence; scheduling noise is inconclusive, not a pass.

The canonical roadmap Goal 1 and contributor guide describe this cadence.
Refresh the existing ShareOne roadmap only after its Git-backed source lands,
then download it and verify that the served content matches the source.
