# Contributing to Sudo Code

Thanks for your interest in contributing to **Sudo Code** (`scode`).

This document is the canonical guide for working in this repo —
prerequisites, the design rules every PR is judged against, the
required checks, and the commit/PR workflow. **Human contributors
and AI agents follow the same rules in here.** Read it once before
your first contribution.

> **Before you write code, read [`README.md`](./README.md) and
> [`sudo-code-roadmap.html`](./sudo-code-roadmap.html).** Sudo Code has a small but
> bright-line set of design rules; PRs that contradict them are
> declined regardless of code quality. The fast summary lives in
> [Design principles you must respect](#design-principles-you-must-respect)
> below; the full statement is in `sudo-code-roadmap.html`.

## Table of Contents

- [Code of Conduct](#code-of-conduct)
- [Design principles you must respect](#design-principles-you-must-respect)
- [Test policy](#test-policy)
- [Parity work — standing rule](#parity-work--standing-rule)
- [Experimental features — standing rule](#experimental-features--standing-rule)
- [Project Layout](#project-layout)
- [Prerequisites](#prerequisites)
- [Building](#building)
- [Required checks](#required-checks)
- [Company model routing in CI](#company-model-routing-in-ci)
- [Measuring prompt-cache behaviour](#measuring-prompt-cache-behaviour)
- [Running the CLI locally](#running-the-cli-locally)
- [Working on a single crate](#working-on-a-single-crate)
- [Commit style](#commit-style)
- [Pull request guidelines](#pull-request-guidelines)
- [Reporting bugs & requesting features](#reporting-bugs--requesting-features)
- [License](#license)

## Code of Conduct

Be respectful, be concise, and assume good faith. Disagree about code,
not people. Maintainers may close or lock conversations that turn
personal or off-topic.

## Design principles you must respect

These are bright-line rules. PRs that introduce any of the items
below will be asked to remove them or are closed. Reopening any rule
requires a separate `sudo-code-roadmap.html` edit before the code lands — not
the code first.

**Hard NOs (the ones contributors trip on most):**

- **No `ratatui`, no alternate-screen mode, no `--tui` flag.**
  sudocode is inline-only by rule. Even a feature-flagged
  `ratatui` dependency is rejected — its presence in `Cargo.lock`
  is the slippery slope toward an alternate-screen path that
  breaks scrollback, attach/detach, and pipe composition. (See
  ROADMAP § Goal 3 → "No alternate-screen / full-screen TUI mode
  — by rule".)

- **No unit tests.** PTY (pty-expect) is the only required test
  layer; integration is an optional debug accelerator; unit tests
  are forbidden by rule. See [Test policy](#test-policy) below for
  the rationale. (See ROADMAP § Goal 1 → "Test layer policy".)

- **No in-CLI multi-agent dashboard.** sudocode is an agent unit,
  not a hub. Multi-agent orchestration UI belongs in sudowork /
  your tmux — not inside scode. (See ROADMAP § Goal 4.)

- **No CLA, no copyright assignment, no closed-source escape
  hatch.** Contributors retain their copyright; their code is
  MIT-licensed and that is the whole arrangement.

- **No "users might misclick" as a reason to hide a feature.**
  Either ship it or don't. Hiding correct behavior behind a config
  flag because the 99% might fumble = treating us like the 99%.
  We are not them.

- **No telemetry by default.** Opt-in is explicit; never buried.

- **One engine↔renderer seam — do not grow a second.** All
  communication between the engine (the model/tool loop) and any
  renderer (the REPL, ACP for moss/sudowork, a future frontend)
  crosses exactly one abstraction: the `engine-core` crate. A
  renderer *consumes* an `EngineHandle` (send `EngineCommand`, recv
  `EngineEvent`); an engine *implements* `EngineDelegate`. That is
  the whole cut. Do not reach past it (no new ad-hoc channel, no
  renderer touching `runtime::TurnSummary`/`RuntimeObserver`, no
  renderer depending on the `api` wire types). The SSOT + the map is
  the crate-root doc of
  [`rust/crates/engine-core/src/lib.rs`](./rust/crates/engine-core/src/lib.rs)
  and [`.../src/session.rs`](./rust/crates/engine-core/src/session.rs).
  A CI `boundary-gate` job enforces it. (See ROADMAP → ACP-cut / #100.)

The product principles live in [`README.md` § Design
principles](./README.md#design-principles). When in doubt, read
that section before opening the PR.

## Test policy

PTY workspaces must not inherit instruction files from outside the fixture.
The harness checks temporary-directory ancestors for `AGENTS.md`, including
`.nexus/sudocode/AGENTS.md`. On Windows it can fall back to a directory under
`PUBLIC`; set `SCODE_TEST_TEMP_DIR` to an absolute, instruction-free directory
to choose another location. If no safe location is available, setup fails
before starting scode or an external editor.

sudocode is deliberately opinionated about what to test and how.

| Layer | Status | When to write |
|---|---|---|
| **PTY (`pty-expect`)** | **Required acceptance gate** (local + complementary CI); the only layer counting toward the 90% e2e coverage metric | Every user-facing feature |
| Integration (mock harness) | Optional — debug accelerator, not a CI gate | When a PTY failure's blast radius is wide enough that shrinking bisection distance is worth the file |
| **Unit** | **Forbidden by rule** | Don't. PRs adding unit tests will be asked to remove them. |

**Why no unit tests.** Empirically, in a year of agent-driven
development on this codebase, unit tests have not caught a real bug.
Real bugs surface in the e2e harness or in human use of the product.
Unit tests here have functioned as pure restatements of the
implementation — they co-vary with every refactor without paying for
themselves in caught bugs. The PTY layer plus structured-log trace
quality is our substitute for fine-grained failure attribution.

**Reopening this rule** requires a concrete bug a unit test would
have caught and a PTY scenario would not, plus a `sudo-code-roadmap.html` edit
to change the rule before the unit test lands.

(See `sudo-code-roadmap.html` § Goal 1 → "Test layer policy" for the long form.)

### Running tests

Acceptance for new or changed behavior starts with the most realistic end-to-end
run available. Prefer live PTY tests with a real API key for scode, and a real
daemon or local Docker deployment for infrastructure. Exercise connected user
steps with fresh data and assert the resulting content, not only success text.
Commit the live tests after actually running them, or add the end-to-end tests
to CI. Record the command, result and any remaining coverage limits in the PR;
mock passes or skipped live tests alone do not complete acceptance.

CLI configuration checks such as `--model` precedence use a deterministic
transport and verify the launched CLI's actual request bodies on all three
platforms. They do not measure provider compatibility. Live headless file-tool
and resume workflows use `SCODE_LIVE_MODEL`; the model compatibility workflow
reports availability and protocol refusals for individual provider models.

Set `SCODE_TEST_BIN` to the absolute path of a downloaded release binary to run
the same PTY workflows against that artifact. Without it, the harness uses the
CLI built by Cargo. This also lets release acceptance leave build outputs intact.

```bash
cd rust/
cargo test --workspace                     # all tests (PTY tests included)
cargo test -p runtime                      # one crate
cargo test -p runtime -- session_resume    # one test by name
```

PTY tests are the `tests/pty_*.rs` files under
`crates/rusty-sudocode-cli/tests/`. They use the
`sudoprivacy/pty-expect` crate and run as part of `cargo test
--workspace` on **Linux, macOS, and Windows** — `pty-expect` drives a
real ConPTY on Windows, so the core conversation / streaming / render /
tool tests execute natively there too. A small number of tests that
depend on Unix-only mechanisms (SIGINT/ESC signal delivery, `sh -c` /
`python3` subprocesses, the ACP stdio subprocess handshake) are still
`#[cfg(unix)]`-gated.

### Mandatory local acceptance and complementary CI

Install the managed pre-push hook once per clone. All worktrees share it:

```bash
python -m pip install psutil jsonschema
ssh-keygen -t ed25519 -f ~/.nexus/sudocode-acceptance-key -N ""
# Add only the .pub key to spec/local_acceptance_signers.json through review.
python .github/scripts/local_acceptance.py install --signer YOUR_DEVICE_ID --key ~/.nexus/sudocode-acceptance-key
```

Enrolled device public keys are listed in `spec/local_acceptance_signers.json`;
their private keys stay outside the repository. Each new author device enrolls
its own platform key.
Never commit a private key. Installation preserves existing hooks by refusing to
overwrite them; compose an existing hook explicitly. Protected-main updates refresh
the installed runner automatically, including when pushing an older worktree.

The hook runs applicable real checks before a branch push. A Windows author runs
the Windows workspace, resize, release and service checks locally; CI runs Linux
and macOS. A Mac author runs macOS locally; CI runs Linux and Windows. A Linux
author runs Linux locally; CI runs Windows and macOS. Co-host build verification
follows the same host split when its inputs change. SDK changes additionally
require both Linux x64 and ARM64: a Linux author supplies only their own CPU
target, and CI supplies the other. Each supported OS remains covered; macOS and
Windows source acceptance is by OS, not every CPU architecture on every PR.
Actual tagged co-host releases retain their five-platform artifact builds.
Ordinary Windows Rust PTYs use the same checksum-pinned ConPTY distribution as
the visual and performance journeys. The shared installer provisions native
sidecars beside the dev-profile test drivers and reports their exact hashes;
the explicit system-host diagnostic retains its legacy compatibility findings.
See [the terminal host contract](e2e/terminal-resize/README.md) for scope and
architecture details.
Pre-merge co-host acceptance uses the workspace dev profile with the deployed
`daemon,driver-ai,egress-gate-presidio` features and runs the resulting binary to
verify its exact source revision stamp. This avoids a separate optimized build
graph on each PR. Optimized co-host builds and platform size budgets remain
required on release tags before publication. This reduces pre-merge coverage:
optimization-specific failures and footprint regressions are caught at release
time. The required command prints this split. A real Git/Cargo/native-process
guard verifies feature compilation, dev execution and refusal of a missing stamp;
it adds no product journey coverage.

The queue separates CLI daemon journeys from co-host/search acceptance. CLI
source changes retain the real pinned daemon, cross-node CLI and authenticated
sender journeys. The independent co-host duet, VFS search owner/restart and
driver feature checks run for shared Rust, manifest/lock, build and daemon
harness inputs. The selector excludes CLI source only when both committed
layouts prove a binary-only package without declared workspace dependents;
unknown layouts retain co-host checks. CLI tests remain outside co-host inputs.
Unknown build/dependency changes keep the conservative scope.
The required job prints its decision and refuses missing decisions. Real Git
PR/push/queue events and actual Cargo builds prove a deliberately broken CLI
does not enter the co-host build, while shared runtime changes rebuild it.
Actual repository metadata requires a binary-only CLI without workspace
dependents. This removes co-host execution on CLI-only revisions; CLI changes
are still exercised against real daemons. Scope guards add no journey coverage.
Changes confined to `.github/scripts/ci_input_scope.py` do not rerun the
unchanged daemon journeys or three-platform workspace Clippy. The selector
does not build or launch the product. Its real Git PR/push/queue matrix,
existing scoped executor controls and complete native/Docker contract guard
remain mandatory. CI reports `selector_only_product_checks_skipped` when this
reduction applies. Workflow edits, mixed Rust or harness inputs and moves into
or out of product source retain their original checks. This deliberately
removes general product smoke coverage from selector-only queue revisions;
no selected journey, assertion or deadline is shortened.
Author-scheduling edits can also omit unchanged daemon journeys. This requires
regular committed runner files with the same mode and identical module code
outside `host_acceptance_lease` and `perform_check` bodies and the plain
`profile_reservations(ordered, pending)` declaration. The required policy and
every non-guard reachable execution fingerprint must still match. Companion
edits are limited to dispatcher changes with those same non-guard fingerprints,
the host-lease enforcement fixture and documentation. Changed command runners,
imports, globals, runner declarations,
product code, workflows, unknown inputs or execution fingerprints retain the
original daemon scope. `acceptance_scheduling_scope` in the mandatory CI JSON
reports the proof or fallback reason. Real Git PR/push/queue controls, preflight
and the full native/Docker contract guard remain required. This removes general
product smoke coverage for proven author-scheduling-only revisions; every
selected journey keeps its cases, assertions and deadlines. These infrastructure
controls add no product coverage. Observed job costs are not measured savings.
Verifier-body-only edits can also omit the two generic queue daemon jobs.
The committed runner must keep its mode, declarations, module context and all
code outside the nine reported verifier/fingerprint bodies. Process execution
and the selector/dependency consumers must not observe those bodies, dynamic
namespaces or the managed runner code frame. The existing exact self-frame
capture remains bound for managed refresh. Imports, every declaration and complete
queue-consumer bodies must also match the reviewed source layout. This gate runs
before importing comparison-base code; equal but unknown module constructors,
default-value factories or decorators keep both daemon jobs. A changed layout
selects full checks automatically; it requires no personal review or waiver.
Only Python compilation CRLF normalization is applied to layout text.
Non-guard profiles are compared
with the comparison base's fingerprint implementation. The queue uses its
protected base; PRs use the target merge base, and push records name their prior
branch commit. The required JSON identifies that source commit and its kind;
final acceptance continues through the protected gate. An edited candidate
algorithm cannot bless a changed command. Companion paths are limited to
those metadata enforcement fixtures and protected pins. New declarations,
function signatures, command effects, policy, signer, workflow, product and
unknown mixed inputs keep the original daemon checks. The mandatory CI JSON
prints `acceptance_metadata_scope` with its proof or conservative fallback.
Preflight and the complete native/Docker contract guard still run. This removes
general daemon smoke coverage for qualifying verifier-body changes; selected
journeys keep every case, assertion and deadline. Real private Git PR/push/queue
controls retain the original 66 scheduling events and 28 bounded metadata events.
Three old-behavior mutation controls and six actual helper-launched children
prove that eager module/default/decorator observers can change command
environments, and that unknown layouts refuse before executing the callback.
They add no product coverage. Observed previous job times are not measured savings.
Fixed VFS cache helper, composite action and publisher changes select the
co-host/search checks without rerunning the independent CLI A2A, cross-node
and sender-identity journeys. Product Rust and A2A/shared harness changes
retain those journeys, including mixed edits and moves across that boundary.
The mandatory output reports both decisions and refuses a missing decision
or a daemon-job selection that differs from their union. No case, assertion,
lease wait or timeout is shortened within a selected journey.
Seven independent runtime reader/stall/receiver-seat fault cases share the
existing pinned nextest runner, with at most four workers and zero retries.
Every case keeps its original real daemon, isolated agent, lease clock,
assertions and deadlines. The batch checks its complete ignored-test inventory
before execution and requires seven successful JUnit results afterward.
The first failure cancels this independent batch, retaining failure and cancelled
results; the outer harness still closes its private daemon. Daemon
restart and SIGSTOP stay serial, outside the batch. Real native control
processes verify overlap, failure and missing/non-ignored-case refusal;
they add no product coverage. The same seven real daemon cases still run in CI.
Changes confined to the A2A shell/Python harness leave the native Rust workspace
unchanged. They skip its Cargo/PTY product reruns on the author and complementary
hosts; the Linux queue still executes the changed real-daemon harness. Author
acceptance still requires realistic testing of the changed behavior. The native
workspace does not launch these scripts or provision their daemon. Mixed Rust
changes and moves into Rust retain the full native workspace assignment. The
required hook and CI plan print `A2A_HARNESS_SCOPE` when this reduction applies.

The Linux queue reuses a fixed VFS checkout and its isolated Cargo inputs and
outputs. An independent main-push publisher saves only a missing exact key;
queue jobs only restore. The key binds the lockfile revision, compiler, runner
image, build recipe, canonical path and explicit codegen settings.
An exact key includes the fixed source revision. A compatible fallback excludes
only that revision and retains every compiler/environment binding. Before
switching a restored source pin, the helper verifies its old Git tree and saved
compiler identity, fetches the required revision with checkout hooks disabled,
cleans old workspace outputs, and verifies the new Git tree. Registry and Git
dependency artifacts remain available to normal Cargo. An old producer's exact
current-pin key is recognized for the one-time cache format migration; unrelated
legacy keys are excluded. A missing main key restores compatible inputs before
prewarming, even when the initial lookup did not download a cache.
The required interface reports the actual restored key. Refusal messages identify
that key for recovery rather than telling an agent to delete a missing new key.
Source bytes are checked against the pinned Git tree before use, including same-mtime edits
and unexpected files. Every invocation still asks Cargo to build normally and
runs the original search authorization/restart case against the current
checkout's co-host, outside this cache. The fixed driver and plugin retain their
original optimization profiles. Cache hits never substitute a previous test
result. The required command prints builds and stores timings and failure logs.
The main publisher alone performs explicit prewarming. Consumers invoke normal
`cargo test`, which compiles its driver and runs the original case's plugin build.
The required job refuses a redundant pre-test build directory; this avoids extra
switches between the plugin's semantic and default-off dependency variants.
Outer builds use the cluster package metadata that Cargo supplies to the test's
nested plugin build. This prevents native dependencies such as ring from
rebuilding solely because inherited Cargo package variables flip between unset
and test values. Each build script still receives its own package metadata;
compiler flags and optimization profiles remain unchanged.
Real Git/Cargo/tar/native fixtures cover restore, changed inputs and wrong-runtime
refusal; they add no product journey credit. The actual cache footprint and
cold/warm product timings must be measured before claiming a lifecycle saving.

The PR proof and completed-run publisher verify against their frozen checkout of
protected main. A PR API or event base SHA can lag that branch. The verified
receipt's actual merge base drives the complementary hosts as well; those jobs
do not independently select a base from the event. Queue verification retains
the merge group's exact base. Git/SSH/process acceptance covers stale API bases
and refuses a signed receipt for the wrong actual base.

CI compares complete published Nexus dependency tags, including prerelease and
build suffixes, with the lockfile's unique tag and commit. A prerelease such as
`v0.8.29-rc.5` must not be truncated to `v0.8.29`. The existing preflight job runs
the actual Bash workflow step against fresh valid, mismatched, ambiguous and
missing inputs. These infrastructure checks add no product journey coverage.

For a publisher rerun, queue verification checks the current attempt's actual
completed jobs and binds the success status's creation time to that execution.
GitHub's aggregate run conclusion can retain an earlier failed attempt. Older
jobs are excluded; missing, pending, failed or mismatched current jobs and old
success statuses still block the queue. The gate prints the run and recovery
command when publication is incomplete. Real Git/SSH/HTTP process fixtures
exercise both the stale aggregate and these refusals.

API, runtime and tools use opt-in acceptance wrappers to compile shared test
code in fewer drivers. Their original individual targets remain available to
Cargo and the daemon journeys. The canonical nextest path runs each case in its
own process. Fixed-name self-spawning tests, cache prefix/TTL and compaction
checks, and model-limit process-global checks retain their independent drivers.
No test body, assertion, ignored flag, worker count or deadline changes.

Checks that run tests, builds or containers share one host lease.
The reviewed compiler-priority migration preserves exact earlier recipes for
ten otherwise unchanged profiles through explicit protected hash pairs. It does
not approve the changed workspace or contract guard recipes. Original source,
SSH signatures, current required profiles and original log hashes still apply;
a changed executor, assertion, image or newly required profile needs validation.

Every `check`, whether from the hook, `contract-update` or a manual run,
executes metadata preflight before joining that queue. Revisions requiring
only preflight and runs restoring already verified complete results finish
without waiting for the host. Partial signed progress still waits and executes
every missing check. Signatures, source bindings, checkpoint validation and
the earliest recorded start time keep their existing requirements. The required
command prints `LOCAL_ACCEPTANCE_RESOURCE_SCOPE`, including the selected and
pending checks. Any pending test, build or container profile holds
`~/.nexus/local-acceptance/host.lock`; a later invocation prints the holder from
`host-lease.json`, its active profile and the queue position. Adjacent fresh
workspace and compile-only examples share one reservation, with a separate
checkpoint for each. Other fresh profiles rejoin the FIFO, so existing waiters
run before this invocation's next reservation. A running reservation keeps the
lease through its execution and checkpoints. Waiters start in arrival order
when it releases the host, including an abrupt exit; each holds a ticket under
`~/.nexus/local-acceptance/queue/`, and a waiter that exits leaves the queue.
`e2e/local-acceptance/run.py` fixture checks never take the real lease.
Concurrent suites on one machine contend for CPU, memory and temporary paths,
which turns timing budgets into failures that cost whole profile reruns.
Processes started under the holder inherit an acquisition-specific
`SUDOCODE_ACCEPTANCE_HOST_LEASE` claim. Nested checks validate that claim against
the existing OS-locked file, the owner's process creation time and actual
ancestry before proceeding without waiting for their parent. Invented, copied,
released and previous-acquisition claims fail before heavy execution. The record
inside `host.lock` is authoritative; `host-lease.json` is a diagnostic snapshot.
Standalone enforcement fixtures use private homes and real private locks, never
a fabricated claim. Existing older runners can finish naturally; their legacy
claims do not authorize execution under a newer validator.
The native author guard and complementary native jobs run the same real-process
inheritance controls, including Windows byte-range locks, before long compilation.
`LOCAL_ACCEPTANCE_RESOURCE_SCOPE` reports the reservation boundary. `attempt.json`
records the total wait across acquisitions as `host_lease_wait_seconds` and each
reservation's wait under `host_lease_acquisitions`; shared acquisitions list both
profiles and count their common wait once. The signed receipt format is unchanged.

A fresh `guard-contract` also validates its protected input pins, selection
digest, release recipe and native fixture inventory before joining the host queue. The required
`GUARD_STATIC_PREFLIGHT` output states that full execution remains required.
A mismatch reports the path and observed hash, retains signed preflight progress,
and exits without taking a queue ticket or starting Docker/native checks.
The same inventory builder checks the required protocol/native counts, duplicate
script/log entries and missing fixture files at admission and execution.
`GUARD_NATIVE_INVENTORY` reports the counts and keeps full acceptance required;
its argv placeholders do not prepare tools or execute tests. A correctly pinned
but malformed inventory still refuses while another private workflow holds the
host, without a ticket or completed guard credit. A valid inventory still waits
for that holder and executes its required profile.
Execution stages and validates a separate copy from the protected Git ref again;
the preliminary snapshot grants no acceptance or execution credit. Already
verified results and policies without this optional early validator keep their
existing paths.

Fresh Windows contract guards and native workspaces check their required
Windows PowerShell and PowerShell 7 paths before joining the queue. Missing
tools report how to fix this process's PATH and rerun the same command.
The native lint and release compiler checks share the same path validator and
check again at execution; readiness grants no completed-check credit.
Real Git/SSH/native-process controls remove PowerShell 7 only from a private
child's PATH while a private holder stays active. Guard and workspace source
must refuse without a queue ticket, while metadata-only source still completes.
This adds no product journey coverage and changes no test deadlines.

When both a fresh contract guard and native workspace are required, the workspace's
existing formatting, metadata and warning-denying lint commands run before the
contract matrix. `LOCAL_ACCEPTANCE_RESOURCE_SCOPE` reports
`workspace_lint_before_guard`; `NATIVE_LINT_PREPARATION` explains completion and
failure on the mandatory command path. Preparation takes the existing host lease,
then releases it and rejoins the FIFO for the guard. After the guard passes, the
workspace verifies and consumes those original command records/logs once before
executing every remaining test. The preparation binds clean HEAD, base, protected
executor, output and Cargo target; altered logs or mismatched bindings are refused.
It never signs a completed workspace. A later failure preserves only actually
completed profiles; a new process repeats unsigned preparation, while a fully
verified workspace keeps ordinary signed resume. Executors without this optional
preparation retain their original flow. Actual Git/SSH/child-process and native
Cargo/Clippy controls cover refusal, consumption, failure order and signed resume.

After the selected native workspace checks, service acceptance runs before
optimized release builds and rendering/browser measurements. Service bootstrap
failures therefore stop the attempt before those expensive profiles start.
`LOCAL_ACCEPTANCE_RESOURCE_SCOPE` prints this execution order. The declared
acceptance plan and every profile's commands remain required; completed signed
results retain their existing restore and carry rules. Real Git/SSH/child-process
checks cover failure before slow-profile entry, complete success and verified
restore with the original plan.
The complementary native CI entrypoint uses the same ordering helper and prints
`CI_NATIVE_EXECUTION_ORDER` with both the declared checks and actual order.
Its first failed service profile stops the job before release execution; jobs
selecting only release still run it. Real production-entrypoint child-process
controls cover failure, complete success and release-only selection.

When native services are selected, preparation resolves their existing grpcurl
client and actually runs `--version` before native compilation. Windows selects
Git for Windows Bash explicitly, so an earlier WSL launcher cannot take its place.
Download and executable failures stop at `NATIVE_SERVICE_TOOL_PREFLIGHT`.
The later service journey uses that exact resolved file and verifies its bytes
again; both original service journeys still run. Real Bash/curl proxy-504,
invalid-executable, offline-cache and unselected-profile controls protect this
ordering. These are infrastructure checks and add no product journey credit.

Contract validation requires the Docker Engine version API to answer within
30 seconds before container/native checks start. Its required output names the
deadline and prints Docker Desktop and `DOCKER_HOST`/context recovery steps on
failure. The version request avoids optional CLI plugin discovery. A native
guard uses the real Docker client against a stalled local HTTP endpoint to
verify the connection deadline. Its container image includes the official
Docker CLI pinned by image digest for the nested policy checks. The guard
preserves an ordinary client error as a
failure. Healthy Engine execution still requires the full container matrix.
The guard image provisions nextest before the matrix using the exact frozen
`nextest_binary` function, including its original URL and checksums. Image
preparation runs archive/executable corruption-refusal controls and restores the
verified files. Docker reuses that layer while its tool script and image inputs
stay unchanged. Each selected attempt rechecks the actual cached CLI with
`--network none` before running the full matrix and author-native guards.
`GUARD_TOOL_PREFLIGHT` names these stages. Image/tool preparation errors point
to the saved build log and installed retry command before tests begin.
This caches tool preparation; the full matrix and native checks retain their
original cases, assertions and deadlines.

On macOS, the rendering profile and the release startup measurement require
external AC power with low-power mode disabled. One shared native probe reads
the power source and Foundation's active low-power mode before and after the
timing window and once per second while it runs. Unknown settings, a detected
power change or a failed probe refuse
the profile, preserving `host-power.json` and any original command failure.
Shorter changes between observations may escape this periodic check; the probe
does not prove a fixed CPU clock or thermal state. It never changes system
settings, budgets, sample counts or automatic retry policy. Functional-only
profiles and Linux/Windows keep their existing execution. These environmental
observations are supplementary diagnostics; completed signed profiles still
require their actual commands and current protected recipe.

The timing context is created only when a profile calls it. Unused timing helper
bodies do not invalidate ordinary native execution. Metadata preflight stages only
its protected history checker; unrelated native fixture pins do not affect its
recipe. Both staging paths share the same path and SHA256 verification. Exact
protected migration pairs retain earlier successful evidence for this context
factory and staging refactor; changed selected commands, signatures, source or
required checks still need their normal validation.
For local Git/SSH debugging, `python e2e/local-acceptance/run.py --profile-compat-only`
uses the real hook and signed-note workflow and explicitly reports its limited
scope. The mandatory default still runs the complete enforcement matrix.

Typing feel is measured by the paired rendering matrix: keypress-to-echo P95
while reading history, waiting for the model and streaming, first visible output
and cancel on prose, paste and resize on a short reply, short-reply RSS peak/after
limits, and CPU and peak memory on a 12 KB Markdown stream. It drives the actual release binary through a headless
terminal, which isolates scode's own cost, and compares it with the fixed baseline
in `e2e/terminal-resize/performance-policy.json`. Each scenario names the metrics
it is judged on; the driver records them all. No scenario runs an unjudged
warm-up; the short reply keeps five A/A and seven alternating A/B pairs, the check
that caught a real RSS regression, and the heavy case uses three and three: 36 judged
observations. A failed, noisy
or invalid scenario stops the matrix. Drivers build in the binary-free
`scode-benchmark-drivers` package, so measuring compiles no extra CLI binary.
The matrix is mandatory on the author's local Docker before push for CLI,
engine-core, PTY harness/helper, rendering-driver and named native observer inputs.
Cargo/dependency metadata and unrelated crate source do not select it per PR.
The browser functional smoke remains separate. Required plan output and signed
evidence attribute both checks to the exact PR head. Foreground/background task
and repeated-turn comparisons run in the `stress` profile on every release
candidate, which also judges dependency-only changes; see
[Release acceptance coverage](#release-acceptance-coverage). Their functional
PTY cases keep the native suite's per-PR scope.

The repeated-turn stress scenario samples 20 typed keys per input phase per
turn, instead of 60. All 24 paired processes and 30 turns per process remain:
600 observations per phase per process, or 43,200 overall instead of 129,600.
Single-turn core scenarios retain 60 keys per phase and 40 resizes per turn.
Repeated turns retain one narrow/wide resize pair: 60 resizes per process,
1,440 overall instead of 28,800. Cancel (20 per process), paste, resource
measurements, budgets and the relative evaluator remain unchanged. This reduces
repeated typing and resize pressure; intermittent latency spikes and small
resize-dependent memory leaks may be missed. Equivalent sensitivity is not claimed.
`RENDERING_SAMPLING` prints each effective policy before measurement. Each sample
retains the exact resolved workload policy and its SHA256; the original judged
policy remains in the parent report. Policies without overrides retain 60.
The parent counts raw arrays using the evaluator's shared helper. The public
process controls refuse missing input, resize or turn observations, tampered
summaries and unsupported reductions. Fresh product stress execution is required before
claiming runtime savings; observation counts alone do not prove faster delivery.

Rendering's throwaway sessions use a private 512 MiB tmpfs in its local Docker
container. Reports, copied binaries and Cargo caches keep their existing storage.
The frozen historical baseline serialized response-archive disk flushes; real
debugger stacks show that backlog delaying exit, while a controlled native
disk/tmpfs comparison isolates the storage cost. Exit assertions, deadlines and
all performance budgets remain unchanged. This gate now isolates rendering from
host disk flush latency, reducing coverage of slow persistent storage. It does
not qualify filesystem durability. Other profiles, including the browser,
native persistence, response gzip, compaction and cache checks retain their
storage paths. A real Docker regression executes a native process from the
temporary directory and verifies the mount type, capacity, frozen sources and
durable evidence location.

The `browser` profile is a functional smoke. CLI sources, the PTY harness, shared
PTY helpers, the browser drivers and the browser terminal harness select it. Local
Docker runs the candidate in pinned
Chromium/xterm: Markdown links stay clickable, native selection survives
streaming, the prompt viewport scrolls and selects, and an idle terminal receives
no output. Measurement-only inputs (`benchmark.py`, `performance-policy.json`,
`gui-smoke.py`, `gui-policy.json` and the retired `gui-benchmark.py`) do not select
the native workspace or resize suites, which never execute them.
The GUI controller, its Python import and sampling policy travel with the runner
from the frozen protected policy. Older candidate snapshots do not need to contain
new acceptance scripts; the CLI and native browser drivers still come from the
candidate. No baseline or full trusted source checkout is added to browser runs.
When rendering precedes browser smoke in the same acceptance process, browser
reuses its immutable Docker image and copied normal release. The key binds the
output namespace, candidate, protected policy and image context; the consumer
checks source/policy/image identity and binary SHA256 before npm or Cargo starts.
Other candidates, policies and invocations use their own builds. Browser-only
acceptance keeps its normal dev build. Native browser drivers and every existing
interaction still run. When reused, GUI behaviour is checked on the production
release configuration; that run no longer adds a separate dev-GUI variant.
Paired browser drivers use the rendering driver's leased release Cargo cache.
Before Cargo runs, its mutable checkout is synchronized to the exact candidate,
replacing the earlier policy snapshot. Normal Cargo builds both browser drivers
and rebuilds changed inputs; no earlier test result is reused. Standalone browser
drivers keep their dev profile and separate cache. This changes only the paired
test driver's optimization profile; the product, steps and assertions stay fixed.
Both measurement harnesses verify the expected driver digest before execution.
The GUI report records its expected and actual candidate digest, and the wrapper
checks the candidate again after viewport interaction. Corrupt or mismatched
promotions fail without starting a replacement build. Reports explicitly record
whether rendering supplied the release; previous output folders cannot grant reuse.
Every Docker build keeps its own randomly named image tag alongside the exact
image ID, so another invocation building identical cached layers cannot replace
it with a different BuildKit attestation/index. Contract and performance images
use one tag allocator.

Active checkouts, npm trees, Cargo dependencies and targets live in the Linux
Docker volume `sudocode-acceptance-benchmark-lease-cache-v1`. The first invocation
copies the existing host cache's `cargo` and `leased-benchmarks-v1` directories
under the same lease, preserving canonical `/state` paths, modes and mtimes.
The host cache stays mounted read-only at `/legacy-state`; it is not deleted.
Completed migration is not repeated; interruption leaves no completion receipt.
The required command prints migration start/completion and includes that one-time
copy in local elapsed time. Logs and measurement evidence remain in the host
output directory. Keep both the data volume and the separate shared lease volume
when managing Docker storage. Removing the data volume requires migration again;
removing the preserved host cache as well causes a cold dependency/product build.
Real Cargo acceptance checks warm artifact identity, changed-source rebuild,
partial-copy recovery, and refusal of escaped roots or invalid completion records.
The production Docker launcher is also exercised with private labelled volumes.
This changes storage placement, not selected workloads, samples or budgets. Both
Docker execution recipes require new evidence; native profile recipes stay unchanged.

Completed guard and browser receipts for the same source head can survive the
private Docker image-tag ownership fix through exact protected recipe pairs.
The image reference change retains the caller's image; it changes no completed
assertion or statistical budget. Browser pairs also retain the already approved
equivalent wrapper changes. Rendering's changed warm-up conditions still require
fresh execution. Different heads, altered logs/signatures, missing profiles and
changed selected criteria keep their existing refusal behavior.
For a live check against genuine saved author evidence, use
`python e2e/local-acceptance/saved_profiles_acceptance.py --saved PATH --policy-ref origin/main --profiles browser --refuse-profiles rendering --output FRESH_PATH`
when testing this historical browser-only migration. The diagnostic exercises
production resume and tamper controls; it does not sign, publish or authorize a
source change. Its input must be a complete acceptance directory with original logs.

Default SSE response archives reuse at most one finished deflate state across
captures. Active captures retain separate states; a finished state releases its
file, session budget and attachment lease before reuse. Compression level, gzip
bytes, CRC, per-session limits and context separation stay unchanged. Real PTY
acceptance checks six consecutive distinct responses and earlier-file integrity.
The conditional PR codec check also writes 42 actual files, including six
concurrent captures, independently decodes them and compares quota failure
prefixes with the previous flate2 encoder. It adds no live-provider coverage.
These comparisons have no scheduled or manual cloud replacement.

Documentation-only changes run preflight. Local
preflight checks actual conflicts against its supplied base and fetched
`origin/main` before product builds and benchmark leases. It prints the exact
paths and recovery step without changing the checkout or index. Clean branches
may stay behind main. Preflight stages the exact hash-pinned checker from its
protected policy, so an older author branch does not need a source update to
receive these diagnostics. The Git-only `e2e/pr-history/run.py` fixture stays required
in CI preflight; editing it alone skips unrelated native product builds.
Development-only Cargo manifest/lock changes do not select normal release,
service, co-host or daemon builds. The shared selector reads committed Git/TOML:
resolver 2 and normal workspace settings must stay unchanged. For the standard
`crates/*` workspace, shared dev dependency revisions and versions may change
when every normal/build declaration and its reachable lock records and resolved
edges remain identical. Aliases, optional dependencies and target-specific
tables participate in that proof. Removed or replaced dev-only lock nodes are
allowed; changed normal resolutions, missing/ambiguous edges and unsupported
metadata retain the ordinary scope. Committed blobs are read in one Git batch,
without executing candidate code. Older/custom layouts retain the existing
conservative verifier for direct dev-only additions. Native workspace/lint,
provider, resize and statistical performance scopes stay unchanged. Other
source or assertion changes still select their checks. Required output prints
`BUILD_INPUT_SCOPE` and CI's `normal_product_ignored` paths. Python without the
standard TOML reader conservatively keeps all checks.
An earlier broader signed full receipt or checkpoint can retain unchanged
completed profiles for the same source/base/host. Its signed plan must still
match a canonical host assignment for known profiles. The current protected
selection drives required checks and CI hosts; new checks cannot inherit a pass.
Legacy receipts without plans retain known completed checks. Failed/unrun
required profiles still run, and old failures and first timing remain recorded.
Local compilers use one job and BelowNormal/nice priority while Cargo and tests stay
Normal. Set CARGO_TARGET_DIR to an existing owned build directory to reuse it.
Compiler observation reads executable paths instead of argv, avoiding macOS
process-argument permission races while preserving native command failures.
Hosted complementary builds use the available compiler CPUs. That hosted-only
setting is applied after cache restore; local author builds keep one worker and
all test commands, deadlines and compiler priorities stay unchanged.
Full logs and partial failures remain under ~/.nexus/local-acceptance/results/.
Deterministic workspace cases use the checksum-pinned nextest runner. Workspace,
resize and service dispatch verify its original pinned archive and
executable before compilation. A missing download or corrupt cache fails before
Cargo; cached verified tools remain usable offline. Real HTTP proxy 504 and
corrupt-archive process controls protect this ordering. Missing nextest/protoc
archives resolve their exact tag and filename through GitHub's public release
metadata and asset API. The existing nextest archive/executable digests and
protoc executable version still qualify the result. This avoids failing browser
download redirects; the canonical artifact URL in the tool receipt stays stable.
The Docker image provisions the same protected downloader. Tool preparation stays
inside the invocation's total elapsed time; test selection and retries are unchanged.
On Windows/macOS the native guard also prepares the original protoc 3.20.2
before its matrix and checks the real executable's version. The isolated cohost
canary copies only that tool cache into its private HOME and runs offline;
it retains private Git/lease state and the original feature/stamp refusals.
Real proxy 504 controls require protoc failure before cohost Cargo or any full
native guard command. Tool preparation and failures remain in total timing.
Tool controls keep their isolated HOME short on Windows and run from the real
nested policy/evidence layout, preserving raw logs without exceeding native
executable path limits.
Full grouped workspaces retain eight workers on hosts with at least eight logical
CPUs. Hosted runners from four CPUs also use eight for the measured, qualified
resume-test source fingerprint; macOS also qualifies from three CPUs after its
matched full-suite comparison. Old/unknown source retains four. Smaller/unknown hosts,
older ungrouped sources, isolated targets and other profiles retain four. `SCODE_ACCEPTANCE_TEST_THREADS=1..4` lowers the
pool explicitly. Local Cargo commands compile with half the logical CPUs, at most one
job per 3 GiB of memory (`SCODE_ACCEPTANCE_BUILD_JOBS` lowers it), with only compiler workers
at BelowNormal priority; test processes stay Normal. Hosted runners use every CPU. Before author workspace
execution and a plan preview for the current host, the mandatory
`LOCAL_ACCEPTANCE_CONCURRENCY` output reports the current invocation's compiler
jobs and requested test cap. An explicit cap of one also prints the shell
command to restore the protected test-pool default. Explicit caps remain honored;
the signed plan and test selection remain unchanged. Historical signature
verification omits this current-environment diagnostic, so a verifier's cap is
never presented as the original author's execution setting. A matched Windows run on
20 logical CPUs passed the same 1,089 integration cases and 69 ignored markers:
four workers took 424.5 seconds, eight took 244.7 seconds. Real ConPTY echo P95
was 0.58/0.74 ms. This warm, single-host execution comparison excludes compilation
and does not establish whole-PR or cross-hardware savings. A test that previously
left its ACP child holding output pipes now awaits its existing shutdown helper.
Native guards run actual case processes, observe overlap and enforce an explicit
four-worker limit; they add no product journey coverage.
The hosted increase checks the canonical resume-test source fingerprint,
normalizing CRLF. Older branches can retain four without merging main. A later
fixture edit conservatively restores four until separately qualified; required
CLI output reports this source decision. Existing eight-core behavior is retained.
Exact protected recipe pairs retain earlier signed native workspace results
with their recorded commands and worker counts. Explicit lower counts remain
allowed; local thresholds, cases, assertions and deadlines are unchanged.
Earlier execution is never relabelled as an eight-worker field. Changed
source, signatures, logs, forged recipe maps and later criteria still refuse
reuse. Pending unmerged origin policies retain strict refusal and print the
origin/current policy and reviewed-merge recovery on the required interface.
A single ordered four-then-eight comparison reused identical captured binaries
and complete case inventories on each hosted VM. Windows (four CPUs, 1,104 cases)
took 600.998/438.974 seconds; macOS (five CPUs, 1,157 cases) took
868.141/459.716 seconds. Ignored cases, assertions and deadlines were unchanged.
These test-stage observations exclude setup/build/queue time and do not establish
population or whole-PR savings. Separate full eight-worker fields also passed on
Linux, Windows and macOS. The cold-resume test uses the existing acknowledged-input
helper before recognizing a new turn; its actual-provider-request and persisted
result byte assertions remain required. Native controls execute real cases for
hosted/local CPU thresholds, unknown CPU counts and explicit lower concurrency.
A standard three-core macOS runner separately passed an ordered eight-then-four
comparison on clean protected source `cffe7dad`: the same 1,161 cases and 134
captured binaries took 649.779/1,000.493 seconds, 35.1% less test time at eight.
Both phases retained 76 ignored cases and the original assertions/deadlines.
This single-VM comparison excludes setup, compilation and queues and proves no
population or whole-PR saving. Only qualified hosted macOS gains the three-core
threshold; three-core Windows/Linux, local hosts and explicit limits retain
their existing selection. Real native controls execute every threshold branch.
The runner lives in a private acceptance cache; it does not
replace a contributor's installed tools. Retries are disabled, the first failure
stops new work while active cases clean up, and an empty selection fails. Cargo
still runs doctests separately. The existing live-provider commands remain on
Cargo; no provider coverage is inferred from deterministic fixture results.
Lint now denies warnings before long native builds and PTY execution. Selected
CLI-only changes lint those targets; a full workspace change lints all targets
with the native mailbox feature graph. The queue additionally checks all features.
The isolated-test selector compares the common ancestor with the candidate HEAD.
Complementary CI can supply a newer main revision; files changed only on main do
not widen that test-only PR into a full workspace run. Actual branch changes to
production code, shared helpers, unknown inputs, deletions and renames retain the
full scope. Real divergent Git branches drive selected and full Cargo/nextest
commands and verify the native processes that actually ran.
This moves ordinary source/test lint failures earlier without another full test run.
The full native workspace opts into eight declarative integration wrappers for
128 CLI targets, selecting 91 instead of 211 integration targets. The selector
contract was qualified before this Rust activation so installed verifiers agree
on its required checks. A Windows prototype passed all 2,505 active cases with
70 ignored markers; activation still requires fresh exact-head author and
complementary native acceptance. Original per-file Cargo commands stay available,
and new ungrouped targets are selected automatically.
Grouped native workspace execution now selects integration targets explicitly,
retiring legacy library/binary unit harnesses in line with the test-layer policy.
An audited Windows run contained 1,423 such unit cases and 1,089 integration
cases; the unit cases contributed 73 summed worker seconds, so their 56.6%
share of case count is not a 56.6% time saving. Every selected integration/PTY
case and ignored marker remains; queue all-feature Clippy,
vendored Mermaid checks and workspace doctests remain. Legacy unit assertions
are no longer runtime regression gates and gain no e2e coverage credit.
Real Cargo/nextest controls poison both unit harnesses, require the former full
selection to fail, then execute the production dispatcher and verify the complete
integration inventory without either harness. Older ungrouped sources retain
Cargo's complete original default suite, including legacy unit harnesses and
disabled/feature-gated target exclusions. An explicit glob would enable manual
targets; a real deliberately failing disabled target protects that boundary.
Native workspace Clippy uses that same validated group inventory, retaining
library and binary test configurations, examples, benches and ungrouped enabled
tests. It avoids linting the 128 covered modules as separate crates on every
author/complementary host: 91 instead of 211 integration targets in the audited
workspace, 56.9% fewer. This is an inventory reduction, not a measured time saving.
Ungrouped sources keep `--all-targets`; disabled
standalone test targets keep their original exclusion. Queue all-feature lint
retains `--all-targets` for ordinary/shared changes, checking original standalone
crate identities. Only committed isolated CLI test edits use the same native
scope predicate: changed original targets and their canonical containing groups.
Unchanged unrelated integration, library/bin test, example and bench roots are
not linted again for that test-only class. The full workspace all-feature
dependency graph remains; a real cross-member feature counterexample rejects
isolated package selection that drops a required dependency feature. Missing base
history, dirty/untracked inputs, unknown or unregistered targets retain full lint.
`QUEUE_CLIPPY_SCOPE` reports the exact mode/reason, counts and command.
Real Cargo/Clippy controls inject
errors into ordinary and test-only library/bin code, grouped/ignored tests,
shared helpers, examples and benches, and verify disabled-target exclusion.
No product test case, assertion or deadline changes.
The selector checks real Cargo metadata and exact declarative wrapper bytes;
missing, duplicated, redirected, unregistered or feature-hidden members fail.
Wrappers retain the original helper namespaces and explicitly allow only
Clippy's `duplicate_mod` diagnostic there. Other lint rules and test assertions
remain required. Self-spawning cases and named release, performance and terminal
drivers stay standalone. Nextest still isolates every case in a fresh process.
Real Cargo/nextest regression compares inventories and ignored markers, proves
process-state isolation, runs the original developer/driver commands and checks
that the normal product stays Cargo-fresh with identical bytes.
New `test=false` targets skip unchanged product/performance builds only when
their names and wrapper paths are new, their fields are exactly name/path/test,
and workspace/existing manifest inputs are unchanged. Existing target edits,
extra fields, missing files and unknown metadata retain ordinary checks. This
exception does not cover changed dev dependencies or measurement sources.
An exact protected migration pair can retain an earlier full workspace receipt
for a committed source without group files or declarations. The verifier reads
Git/TOML only; escaped declarations, missing/invalid metadata, outside workspace
members and grouped sources require fresh validation. This conditional pair
cannot authorize the new grouped execution from an old original-target pass.
Grouped or unknown sources also select `native-examples`: a separate normal
`cargo build --workspace --locked --features engine-acp/mailbox --examples` on
the author and complementary hosts. It compiles and links examples without
running their entry points. Explicit nextest target selection otherwise omits
this original default Cargo/nextest compile-only coverage. Known ungrouped
sources retain their existing selection without adding a duplicate example
build. The public plan prints `EXAMPLE_BUILD_SCOPE` for this decision. An actual
native guard proves normal linking, no implicit example execution and refusal
of a deliberately broken example. These checks add no product PTY coverage.
On the author's machine, a fresh `native-workspace` and its immediately
following fresh `native-examples` share one existing FIFO reservation. Their
order, commands, separate signed results and original command fingerprints
remain unchanged. Workspace success is checkpointed before examples begin;
an example failure preserves it for verified resume. All other fresh profiles
rejoin the FIFO, and reused profiles need no reservation. The mandatory
`LOCAL_ACCEPTANCE_RESOURCE_SCOPE` output lists the actual reservation groups;
acquisition timing lists both profiles and counts their common wait once.
This removes an additional queue admission, not the example build. Hosted
complementary profiles and all native cases remain unchanged. Real private
Git/SSH/process controls verify the shared acquisition, the existing waiter's
subsequent turn, failure propagation and full/partial reuse without replaying
completed work. Population time savings remain unmeasured.
Changes confined to ordinary individual CLI test files can omit the separate
example build on both the author and complementary hosts. README and Markdown
files under `docs/` may accompany them. The selector reads committed Git blobs;
deleted, redirected or unknown inputs, shared helpers, manifests and production
sources retain ordinary scope. `EXAMPLE_BUILD_SCOPE` reports `isolated_cli_tests`
on this path. Native test selection and execution stay required. Real Git/Cargo
guards execute the changed CLI test, then independently confirm that the example
artifact remains fresh with the same binary hash. Mixed/shared changes retain
example compilation and a broken example still fails. This removes unchanged
compile-only work; it does not add product journey coverage or prove a whole-PR
time reduction.
Contract validation finishes protected input staging and Docker/tool preflight
before overlapping the full Linux matrix with the full author-native guard.
The Linux matrix keeps its metadata-before-Cargo order. Both complete results
remain mandatory; the first failed branch cancels only its owned sibling.
Original commands still use the shared executor, one Cargo worker and reduced
compiler priority. The original Docker limits, workloads and assertions remain.
Seven required real Docker/native controls check success, either branch failure,
native interruption, wrong owner/image refusal and empty container-ID recovery.
Cleanup verifies the exact image/owner and removal; original child command logs
and hashes remain in the signed profile. The existing host FIFO stays exclusive.
An unchanged full-guard experiment completed in 501 seconds; the earlier serial
author run took 907 seconds. These different runs support further qualification,
not a fixed or population author-to-merge saving. No check coverage is removed.
Within the author-native guard, the original Git/SSH/HTTP publication controls
overlap the original serial compiler/test lane. The same supervisor requires
every original command log and cancels only its owned descendants on failure or
interrupt. Five additional real native controls check clean completion, either
failure direction, serial command order, exited-process history and parent/child
cleanup while preserving an unrelated sibling. Waiting checks each original
PID and creation time; inaccessible live identities still refuse cleanup. The existing
seven Docker controls remain mandatory. Same-source observations recorded 488
seconds serial and 344 seconds overlapping; this single field does not establish
a population delivery-time reduction. No compiler concurrency is added.
Every newly built release artifact must pass native acceptance before packaging
and publication. Both Linux builds, both Mac builds and both Windows builds use
their actual native executable. The gate verifies its Git revision, architecture
and SHA256, then runs the original stream/resume journey, 40-sample 100 ms startup
budget and compiler-context journey. It does not rebuild the release executable.
An independent test-profile driver uses the original stream/capture source and
launches the copied artifact through `SCODE_TEST_BIN`, without building an unused
debug CLI. Failed or changed artifacts stop their build job and prevent publication.
Evidence uploads are excluded from the packaged assets. Existing-asset publication
resumes retain their existing validation; they do not build a new artifact.

Ordinary product edits retain applicable native functional, resize/service and
selected Linux release-rendering checks. `native-context` reuses the current
workspace dev/mailbox binary for the connected compiler-context journey.
`native-release` remains mandatory locally for changes to optimized acceptance
drivers and harnesses. **Coverage reduction:** ordinary source acceptance can
miss native optimization-specific failures and startup regressions until release.
`RELEASE_ACCEPTANCE_SCOPE` and `NATIVE_CONTEXT_SCOPE` explain this at the required
interface. Real Git plans cover ordinary, retained, mixed, renamed and deleted
inputs; real Cargo/native controls check identity, failure propagation, changed
bytes and the publication ordering. See [Release acceptance coverage](#release-acceptance-coverage).
An actual Cargo/native-process fixture checks driver reuse, the release binary's
identity and rejection of a substituted debug binary. The test driver and its
in-process fixture helpers now execute in the test profile; the product process
still uses the normal release graph.
Ordinary Windows shells also receive the configured/discovered MSVC environment
when the startup measurement script compiles its native reference process.
The protected policy records exact pairs of equivalent execution hashes for the
runner migration. Existing full-Cargo evidence for the same candidate can remain
valid; the new runner-enforcement guard still runs. The verifier checks the old
recipe against signed source and the pair against the current recipe. A later
command change or newly required check cannot inherit that approval. Reuse is
reported directly by the hook and CI verifier.
The native recovery smoke and executor guard share an immutable protected-policy
snapshot with their matching helpers. Older candidate checkouts do not need
those newly added files. The protected profile pins every staged input's SHA256,
including helpers outside `.github/`; changed bytes fail before execution until
their protected pins are reviewed and updated. The existing profile fingerprints
bind those pins without changing the receipt format. The full Docker decision
matrix still exercises candidate sources.
Toolchain discovery still uses the candidate repository.
Compiler monitoring uses executable identity, including versioned and target-prefixed
compiler names, without reading command-line arguments. This avoids psutil 7.2.2's
macOS process-name fallback failure. One compiler worker, lowered compiler priority,
Normal test priority, commands and budgets remain unchanged; protected equivalence
pins retain successful earlier evidence for those unchanged commands. The new native
guard always needs its own actual run, including a refused process-name query.
On a Windows box with unregistered Build Tools, set
`git config localAcceptance.vsdevcmd C:/path/to/Common7/Tools/VsDevCmd.bat`;
otherwise the runner discovers Visual Studio or uses the existing Developer shell.
Inherited `NO_COLOR` is cleared after SDK initialization so the launcher's log
preference cannot suppress colored fixtures. A command's explicit `NO_COLOR`
value is still honored. The native guard runs actual Cargo/SDK processes to
verify inherited, explicit and empty values, including cached SDK setup.

A signed Git note binds the successful command list and log hashes to the exact
commit, tree, diff base, policy and native host. The hook publishes that note
before the branch. GitHub's read-only metadata job verifies the signature against
public keys enrolled on main and selects the missing hosts. The SDK profile runs
the actual `npm ci`, `npm test` and runtime packaging commands locally and in CI;
an older preflight-only receipt cannot satisfy it. Missing, failed,
stale or tampered receipts block the Local acceptance required check. A signature
authenticates the enrolled author's statement; it is not remote execution
attestation. Maintainers may attest a contributor's PR by actually running its
checks on an enrolled host; forks need not receive repository secrets.

After the protected commands change, `check --resume-from /path/to/previous/output`
can reuse unchanged profiles from a signed run of the same commit, tree, base,
native platform and architecture. The previous policy must belong to the current protected
history, its signature must still verify, and the original logs must match their
signed hashes. Changed profiles run again. The new receipt records which profiles
were reused and retains their original command records and completion time.
After each successful profile, the runner signs a separate checkpoint while the
source remains clean and unchanged, before it starts the next profile. Complete
receipt/signature/envelope files live in immutable numbered generations; an atomic
`checkpoint.json` pointer advertises only a verified complete generation. An
abrupt process exit can therefore retain earlier completed profiles without
running exception cleanup. Failed and unrun profiles remain required. Ordinary pushes
automatically discover compatible checkpoints for the exact head, base and native
host, including explicit `--output` directories. Each external output gets one
atomic discovery reference under `~/.nexus/local-acceptance/results/<head>/`;
the original receipt, signature and logs remain in their output directory. The
required command prints `LOCAL_ACCEPTANCE_OUTPUT_INDEX` with both paths. These
references only locate evidence: reuse still verifies its signature, source,
host, protected commands and original log hashes. Invalid references cannot
approve checks. An index write failure prints
`LOCAL_ACCEPTANCE_OUTPUT_INDEX_UNAVAILABLE` and preserves signed progress for
explicit `check --resume-from /path/to/output` recovery.
Older external outputs can be discovered without rerunning or re-signing them:

```bash
python .github/scripts/local_acceptance.py index-output --directory /path/to/old/output --policy-ref origin/main
```

This command verifies the original signed receipt/checkpoint and log bytes before
writing a discovery reference. It changes no original evidence and grants no
acceptance by itself. Deleted or moved outputs must be restored or indexed at
their new location; missing required results still run normally.
A later head of the same branch carries a signed profile from an earlier head of
that branch when nothing it changed selects the profile. Only the PR's own changes
count: a path changed since the earlier head counts when it differs from current
main or was part of the PR when the profile ran. Main's own commits keep their own
acceptance, and a branch behind main merges without rerunning its profiles, so a
clean merge of main costs nothing extra. Conflict resolutions and PR changes
replaced by main's version still count. Carrying requires the same native host,
unchanged profile commands and matching original logs; `preflight` always runs.
The receipt records each origin head with its signed envelope, and the verifier
re-checks that envelope, its ancestry, the change set and the copied command
records. The runner prints `CARRIED_PROFILES` and why a profile was not carried.
For a contract update, the managed child keeps the candidate command recipe and
protected signing authority separate. Carry admission, checkpoints, resume and
publication use the same protected authority; an earlier unmerged candidate
policy cannot qualify as its own protected ancestor. Ineligible carried profiles
run fresh. Exact-source recovery still checks compatibility with the candidate
recipe and verifies its signatures against the protected enrolled keys.
Older managed parents already name the protected base and exact candidate policy;
that child argument form keeps the same authority before the parent is upgraded.
Real Git/SSH/child-process controls retain the former local-pass/publication-refusal
counterexample and require the actual managed child to refuse that carry before
executing and publishing fresh checks. They add no product journey coverage.
When CI refuses an obsolete execution recipe, its `LOCAL_ACCEPTANCE_RECOVERY`
record includes preparation commands and the author-host runner arguments.
Run preparation first: mainline recovery fetches protected main before resolving
its current policy, so an old CI diagnostic cannot pin an obsolete recipe.
Explicit experimental policy snapshots remain pinned. The check freezes the
resolved commits at startup and still requires every newly selected profile.
Checkpoint signatures use a different SSH namespace and cannot satisfy the required
GitHub check or authorize a branch push. Original logs must still match their hashes.
Earlier `checkpoint/` records remain readable. An incomplete unadvertised generation
does not replace earlier signed progress. The active profile still needs to finish
before its result can be saved; this does not recover a partial profile or claim
protection against host power loss. The required command prints progress and the
resume command directly. Real Git/SSH acceptance uses an actual abrupt CLI exit,
explicit and automatically discovered resume, a legacy record, an unsigned partial
generation and an escaped pointer; the earliest recorded local time is retained.
The required interface prints `LOCAL_ACCEPTANCE_TIMING` and retains
`attempt.json` for each started check, including failures. Signed receipts and
checkpoints retain the first recorded start across verified resumes. Outer push
and contract operations also print `LOCAL_ACCEPTANCE_OPERATION_TIMING`; explicit
output directories retain `operation.json`. Measure local setup, builds, lease
waiting, retries and publication through CI failure or merge. Use the earliest
applicable start, without adding overlapping intervals. Older/manual history can
be missing; these diagnostics neither authorize a push nor prove complete history.
For acceptance-contract changes, the full Git/SSH decision matrix runs automatically
in a private local Docker container. Native guards retain the actual failing
process, checkpoint, SSH purpose, damaged-log and automatic-resume workflow; the
native nextest/Cargo enforcement fixture runs on the author OS. Windows author
acceptance also verifies native color initialization. Docker is required on
the author's machine, not by complementary native jobs. The full
negative decision matrix is no longer repeated natively on Windows or macOS, so
OS-specific negative hook paths have less coverage. The container uses its own
temporary source/fixtures and does not take the shared performance lease or build
the product. Product native acceptance and complementary CI hosts are unchanged.
Edits limited to the independent Cargo benchmark, Docker input or Clippy
executor fixtures skip the unchanged local enforcement matrix. Their actual
changed fixture remains required in CI preflight. Shared executors, policy,
signers, enforcement fixtures and unknown new acceptance fixtures still select
the full guard; mixed changes and renames in either direction retain it.
The public planner and mandatory command output state that CI preflight runs
changed executor fixtures. Real Git regressions exercise isolated, mixed,
unknown and renamed inputs through the public planner, and verify the matching
PR/push/queue selector outputs. Product checks and selected fixture assertions
are unchanged; this avoids repeating unrelated enforcement tests locally.
The policy-guard fixture reconstructs exact pinned LF bytes after a Windows
archive converts line endings. It accepts the reconstruction only when SHA256
matches the original protected pin; changed content still fails. Its private Git
repository disables automatic line-ending conversion without changing author settings.

For shared nextest execution, the CI scope response includes
`nextest_dependency_scope`: the decision and bound function names, or the reason
an unknown dependency requires full A2A. It binds transitive module functions
including the GitHub API/download helpers. Unrelated function bodies omit this
journey; changed imported context, reachable code, unknown data, rebinding or
reflection retain it. Real Git/CLI cases exercise actual Windows-helper,
authorization and asset-identity changes alongside the existing scope matrix.

A failed native matrix job cancels the other pending/running platforms. The
trusted publisher checks the actual completed jobs directly; no second hosted
summary job repeats those checks. Scheduling-only edits keep valid completed
local evidence when its executor/profile/image are unchanged and it covers the
current required checks. New commits or changed commands still need acceptance.
Per-profile fingerprints retain the selected literal dispatch branch and all
shared code, process execution and image inputs. Adding an unselected branch
preserves that evidence. Legacy receipts require the exact attested executor to
be recoverable from their immutable signed source or base; their signatures stay unchanged.
The versioned semantic AST encoding ignores empty optional fields so author and
CI Python versions compute the same execution fingerprint.
Modern receipt verification validates both recorded maps against the attested
original source, then compares its reachable map with the current policy. It
does not first compute a whole-policy hash that this decision would overwrite.
Within one verification, identical immutable source/policy references share the
computed maps; no global cache changes later reads. Legacy whole-hash/bootstrap
receipts retain their original verification path. Signatures, source/base,
current required checks, equivalence rules and refusal conditions remain.
Both fingerprint formats share one AST projection helper. It copies the module
shell and the dispatcher that projection mutates; other nodes remain read-only.
Each profile and later invocation keeps its own dispatch projection. Fingerprint
bytes and conservative reflection/import behavior remain unchanged, so this
computation change requires no new equivalence rule or repeated product run.
Provider tests require explicit local execution or manual provider dispatch;
PR, queue and main events never authorize paid calls. Real-daemon queue tests
still run for changed daemon inputs without a model credential. Documentation/CI-only
changes skip those unchanged daemon journeys.
The acceptance policy and signer registry are CI authorization metadata; changing
them selects their acceptance checks without selecting unchanged provider calls.
Paid provider checks follow the explicit on-demand policy below; changing
authorization metadata does not authorize provider calls.
CLI integration-test-only edits retain native PTYs, but skip the daemon suite
whose binaries and shell inputs are unchanged.
CLI manifests, production source and daemon harness edits still select it.
When a change contains only standalone CLI integration-test modules (optionally
with root README or Markdown documentation edits), workspace acceptance runs
those test targets with the same mailbox feature. Unchanged metadata, Mermaid,
benchmark and production Clippy commands are reused. Shared helpers, fixtures,
specs, source, manifests, target deletion and moves across those boundaries
keep full workspace acceptance. All other selected native/local profiles remain.
Shared PTY helper sources also select native terminal resize acceptance, whether
they live under CLI `tests/common` or the test-only `rust/crates/pty-harness` path.

Public `spec/` inputs selected by the native workspace policy also select the
queue's all-feature Clippy and real CLI/co-host daemon journeys. The CI selector
reads that same committed policy, including its acceptance-metadata exclusions;
mixed changes and moves retain both paths. Paid model calls remain on demand.

The required `CI preflight` job shares one checkout and tool setup for format,
generated cache schemas, docs, history, boundary, release-mirror and provider
probe checks. Its first failing step blocks its daemon/provider dependents.
PRs always start this job, including SDK-only changes; path filters cannot leave
a required status pending. The queue's protected acceptance gate still checks
the combined Git history. No unique product test is removed by this consolidation.
After merge, main reuses those completed integrity checks and does not start
another Rust CI preflight. Independent cache/package publishers retain their
own main-push triggers. PR, queue, legacy development-branch pushes and explicit
runs retain the full preflight; no product assertion is removed.
Signature verification and status publication use Windows runners, with an
explicit Python interpreter, to avoid two extra admissions to the busy Linux
pool. This metadata placement does not replace native Linux or macOS tests.

When Windows terminal resize acceptance is selected, both the author hook and
complementary CI install its checksum-pinned terminal host before long native
compilation. A failed download stops before workspace builds. The same actual
installation command/log is retained in the later resize receipt. Completed
resize checkpoints are reused without another preparation step.

Resize drivers use the workspace dev/mailbox feature graph, so Cargo can reuse
the same current executable and ignored-test binaries. All selected real
terminal cases, assertions and deadlines remain. These journeys no longer
cover a separate standalone default-feature debug graph. Artificial installer
HTTP/cache corruption checks run when committed non-Rust inputs change,
excluding the known README/contribution/roadmap and Markdown documentation
paths. Installer/helper/executor changes, mixed changes, renames/deletions of
those inputs and unresolved Git scope retain those checks. Ordinary Rust-only edits keep actual
checksum-pinned installation and all real terminal cases, but skip the unchanged
installer fault matrix. Negative installer paths therefore have less coverage
on ordinary source PRs. `WINDOWS_TERMINAL_INSTALLER_SCOPE` and the retained
`installer-scope.json` explain the decision. Real Git/Cargo/native-process guards
check input transitions, inherited Git bindings, warm reuse and ignored-only
target selection; their controlled dependency-acquisition boundaries add no
product journey coverage.

Resize, service and release-driver tests share the native workspace's package
selection as well as its mailbox feature. Full acceptance uses `--workspace`;
an isolated CLI-test scope retains `-p rusty-sudocode-cli`. Other selected
workspace members can enable additional dependency features, so matching the
mailbox flag alone does not preserve the graph. `NATIVE_CLI_TEST_GRAPH` reports
the chosen packages. The shared committed-input reader ignores inherited Git
bindings and keeps unresolved scope conservative. The normal release product
build remains separately selected and uses its original package graph.

Native service journeys use the workspace's dev/mailbox feature graph. Cargo
verifies or builds the `pty_model_stream_close` driver with `--no-run`, then
the two original Python service harnesses launch the resulting debug `scode`.
This avoids switching back to a separate normal dependency graph after workspace
acceptance. It also builds correctly when run alone and refuses stale binaries
when current source fails compilation. No driver tests execute twice. Service
cases, RPCs, deadlines and cleanup stay unchanged; these journeys no longer
cover the standalone default-feature debug graph. Selected normal release
acceptance retains its separate product graph. The mandatory executor output
prints this scope, and its real Cargo/native-process fixture checks warm reuse,
standalone construction and refusal before services when current source fails.
Both service harnesses use the executor's explicit Python interpreter, including
the Git Bash call; an unversioned `python` alias is not required on Linux.

`spec/local_acceptance.json` selects required targets, and the protected runner's
`acceptance_plan` drives both the hook and CI. Push output and CI summaries print
the local/CI responsibilities and an `ACCEPTANCE_PLAN` JSON record. The signed
receipt retains that plan; GitHub's required `Local acceptance` status description
also names the local host and complementary CI targets, so `gh`/API clients see
the split directly. To preview it without executing or attesting tests, run:

```bash
python .github/scripts/local_acceptance.py plan
```

The ACP package workflow no longer repeats PR integration tests on the author's
platform. Ready PRs use the mandatory shared acceptance gate; drafts do not claim
those skipped jobs passed. Main still builds and packages its exact merged
revision on all four hosts. Explicit package dispatch remains diagnostic and
does not replace a signed receipt. A newer head cancels obsolete acceptance runs.
Unrelated workflow edits do not start three Rust cache
publishers; Rust changes and changes to the publisher itself still do.

These receipts establish source-test coverage. They do not prove installation,
authenticated enrollment, frontend lifecycle or deployed cross-machine delivery.
Do not request another author's manual platform rerun to duplicate CI; separately
scoped deployment acceptance still exercises the actual installed system.

The merge queue verifies each included PR's current receipt and completed
cross-platform workflow, then checks the combined Git history. It does not repeat
the native suites on the combined merge-group tree. Native acceptance retains
its per-host lint. Strict all-feature Clippy runs once on Linux,
Windows and macOS against the combined queue tree, without a duplicate Linux
PR job. All-feature-only findings can therefore arrive later in the queue;
this removes early feedback, not the final three-platform lint requirement.
The required daemon
gate waits for those checks and rejects failed or unexpectedly skipped jobs.
The lint command stops scheduling additional crates after the first error;
compiler jobs already running may finish before matrix cancellation. Successful
runs retain every feature and `-D warnings` check; ordinary source changes retain
all target categories, and isolated test edits
select changed originals plus their groups. An actual three-package
Cargo/Clippy fixture compares the old continuing command with the current CI
command, verifies all packages/targets/features after fixing the warnings, and
records the actual compiler priorities. Additional real compiler controls reject
errors in either grouped module, examples, benches and library/bin test
configurations, and refuse noncanonical wrappers. Old/new compiler artifacts
also prove manual-driver exclusion with deliberately broken manual code. Required output prints
`QUEUE_CLIPPY_SCOPE`, including all-feature target counts and the actual command.
The fixture runs once on a PR only when the lint workflow, entry, fixture or scope
selector changes; queue/main reuse its result.
Changing the queue entry itself also selects the real all-feature workspace
lint on all three queue platforms. Real committed PR/push/queue event controls
verify this trigger and the exclusion of documentation-only changes.
New integration grouping files declared with `test=false` also reuse the
committed Git/TOML proof for their manifests. Only previously absent regular
files under `tests/acceptance_groups`, with fresh target names and unchanged
remaining manifest/workspace settings, can skip unchanged daemon/A2A/cohost
journeys. Existing or ordinary test edits, enabled/unknown declarations,
missing/indirect sources and mixed production/build changes retain those gates.
Required preflight JSON prints `daemon_disabled_test_sources_ignored`; full
native cases, examples, all-feature Clippy and provider/live scope remain.
Real Git event, Cargo product/hash, nextest inventory and native-process
controls run in the required Linux Docker and author-native contract guards.
Draft snapshots use a separate concurrency group and explicit run title. They
cannot cancel or supersede applicable ready/queue acceptance. Skipped draft
workflows allocate no publisher runner. A newer applicable ready run and its
current attempt still own the result, including failures; required jobs and
SSH signatures remain verified. Publisher output explains an ignored event
on the mandatory execution path. Real child-process/HTTP/Git/SSH controls cover
both event orders, queued drafts, failed ready runs, signature corruption and
raw UTF-8 event metadata with UTF-8 mode disabled and the POSIX C locale. The
controls record the child process's actual default file encoding. The gate
and publisher share one event-file decoder;
PR text and workflow metadata do not depend on the runner's locale.
In the queue, independent failure-only callers stop unfinished jobs in the same
Rust CI run after Clippy, CLI daemon or cohost acceptance fails. Each caller
waits only for its own failing dependency. The shared cancellation job has the
Actions write permission and runs no checkout or candidate executable. Successful
runs allocate no cancellation runner. The original failed step remains visible;
the command prints how to inspect it and rerun failed work after repair. A live
GitHub fixture exercises all three failure directions and the uncancelled success
path on a disposable branch. These are CI execution checks and add no product
journey coverage.
CLI A2A and cohost journeys use independent fresh-node jobs after the same pin
preflight. Each selected job must pass; an unselected job must be skipped.
Their original shell commands, features, cases and fault assertions remain.
The managed scode and remote-context journeys run once on the queue tree:
CLI A2A's `run.sh` owns them when selected; independently selected cohost changes
run the same script in the cohost job. Missing selection fails before the daemon
starts. The integrated gate still requires each selected job to pass. Real
workflow-shell controls retain cohost-only execution and child failure propagation
and refuse both an omitted journey and duplicate dispatch. This removes a repeated
invocation and retains each original journey, assertion and deadline.
An isolated matched GitHub comparison with identical source and full cache hits
took 825 seconds sequentially and 506 seconds in parallel. Total runner time
was 825 versus 837 seconds. This one run demonstrates a shorter daemon path,
with a small compute increase; it does not establish population or full
author-to-merge savings. Required output identifies the separate cohost job,
and native Bash guards reject missing, failed, cancelled or unexpected results.
A runtime interaction that exists only in the combination of independently verified PRs is therefore outside
that pre-merge runtime coverage. Main pushes do not repeat deterministic suites.
Real provider/API assertions remain available through explicit acceptance;
mock transport results do not
establish live cache hits. Cache-prefix, compaction and all existing workspace
cases remain, including standalone vendored Mermaid and benchmark type checks.

Existing PRs do not need a cosmetic merge from main to acquire the hook files:
the managed hook loads protected-main command definitions. Registering another
public device key does not invalidate other devices' current receipts.

#### Native tool API credentials

Complementary native CI prepares selected pinned tools in a separate trusted
step with the read-only GitHub token. Both release metadata and binary API
requests authenticate only at `https://api.github.com`; credentials are omitted
from redirected requests, including release-asset storage. Public downloads
without a token remain supported. Tool tags, asset selection, hashes, versions,
caches and request timeouts retain their existing checks.

The trusted preparation step receives the read-only API credential. Tool
subprocesses and the candidate step receive neither `GH_TOKEN` nor `GITHUB_TOKEN`.
The native command
executor also strips both after SDK discovery and explicit environment overrides,
including mixed-case keys that Windows treats as the same variable names.
The native contract guard exercises real private HTTPS requests, redirect refusals,
candidate subprocess isolation and refusal of a case-sensitive cleanup that leaks
explicit lowercase or mixed-case overrides. These controls
establish credential transport behavior; they do not measure live GitHub quota.

#### Acceptance contract changes

For ordinary frozen source with complementary native hosts, the protected
runner also supports `start-pr --remote origin`. The normal pre-push output
prints `LOCAL_ACCEPTANCE_PARALLEL_AVAILABLE` with the executable entry.
Both installed entries refresh atomically from protected main. The required
`LOCAL_ACCEPTANCE_RUNNER_REFRESHED` output names that commit and the managed
entry, then continues the frozen source with all acceptance still required.
The parallel entry retains the original start/output across the refresh and
executes the new protected code even if another workline already updated its
file. Installation uses the same atomic writer, including self-install; it
preserves the enrolled signer/key and existing managed hook. Candidate scripts
outside the managed entry are refused without overwriting their source.
It publishes the exact branch under a separate signed execution plan, readies
the frozen PR for tests and runs every mandatory author check while native CI
executes. It never treats the scheduling plan as completed acceptance. The
required GitHub Actions `Local acceptance` status stays pending until both
the full author signature and original native jobs pass. Finalization on main
rechecks the source and reuses those jobs; retries reuse exact-source author
proof rather than rerunning completed work. Its signed timing includes setup
and publication before the author checks. Rule changes still use the complete
`contract-update` route below; ordinary `git push` retains serial acceptance.
Protected metadata can advance while a plan is running. Its original SSH
signature and contract hash must still authenticate an ancestor of current
main, and the source contract must remain unchanged. The current responsibility
plan, every selected command identity, native workflow and dispatcher must match.
The required output prints `LOCAL_ACCEPTANCE_PLAN_REUSED` when these conditions
permit reuse. Changed commands/assertions, images, native dispatch/workflow,
required profiles or revoked signers still reject the old plan. Real Git/SSH/HTTP
controls verify both completion orders, unchanged notes/logs and native jobs
executed once. A scheduling plan still cannot satisfy completed acceptance.

Compiler-budget or command-completion waiting changes can preserve an older signed execution plan and its
completed native profiles. The verifier derives this proof from both committed
sources: only the known serial/adaptive compiler-budget calculations and matching
native forwarding forms, or the exact 150 ms sleep/completion-notification forms,
may differ. The command supervisor waits for actual process exit instead of
sleeping the full interval after a short command finishes. Running commands keep
the same compiler-priority scan interval; tests stay Normal. Original signed fingerprints must first
match their original source. All remaining selected commands, assertions, test
pools, retries, deadlines, environment, Docker images, workflow and current
host responsibilities stay bound. Unknown changes refuse reuse. The required
proof also binds reviewed module initialization and the actual command/selector
consumer functions after resource normalization. Equal unknown callbacks or
helper bodies can observe a changed executor and therefore refuse automatically.
Complete normalized helper-program layouts also bind outer callers, including
`main` and the check producer; only the literal reviewed-source set is removed
from its own checksum. Unknown callers keep all current checks.
`LOCAL_ACCEPTANCE_RESOURCE_LAYOUT_REFUSED` reports both layouts and the supported
original-author route; it does not request a personal review or waiver. The required
interfaces print `LOCAL_ACCEPTANCE_RESOURCE_EQUIVALENCE` and
`LOCAL_ACCEPTANCE_PLAN_RESOURCE_EQUIVALENCE`; a refusal prints
`LOCAL_ACCEPTANCE_PLAN_RECOVERY`, including that blindly renewing the same-head
plan is unsupported. Old plans, signatures, logs and the first timing record are
preserved. This proof executes no product tests and cannot complete missing
profiles or complementary-host jobs. Real private Git/SSH/CLI controls exercise
original execution, plan/full verification, resume without repeated commands,
and eighteen independent refusals. Real short-lived children retain exact output,
normal priority and failure propagation under both wait forms. Three real
Git/native paired observer controls require changed child environments to refuse
reuse at module initialization, a command consumer and an outer producer, while
the prior proof mutant exposes the unsafe acceptance. All fixture Git/runtime
and proof children use the existing isolated command environment, including
when outer hooks export Git repository/index selectors. They add
no product journey coverage. Measured individual command costs do not establish
a whole-lifecycle speedup.

The local hook and CI verifier reject a candidate that changes the protected
acceptance contract relative to trusted main. This includes adding checks and
editing diagnostics, not only removing checks or lowering thresholds. The purpose
is to prevent a candidate from authorizing the rules used to accept itself;
the rejection does not establish that its product tests failed.

The diagnostic identifies the changed protected files and trusted policy commit.
The rejection prints the executable next step, using the installed protected
runner so the command also works in an older worktree:

```sh
python "$(git rev-parse --path-format=absolute --git-common-dir)/local-acceptance-hooks/local_acceptance.py" contract-update --remote origin
```

This requires GitHub repository maintain/admin authority and an already enrolled
signing device. It runs the exact candidate executor and every selected check,
verifies its real signed receipt against keys on protected main, and pushes only
the checked-out non-main branch through an exact-head verifying hook. It then
publishes the separate `Acceptance contract authorization` status, bound to the
full candidate and trusted contract hashes. That status authorizes the rule
update; it is not the `Local acceptance` test result. The read-only gate checks
the actor's current role and uses the approved executor for complementary hosts.
Failed tests, invalid signatures and missing hosts still fail normal acceptance.

Contract hashing reads one committed tree and batches its immutable Git blobs.
The byte hashes, signed payloads and required tests remain the same; no mutable
ref result is cached. The required Docker and author-native contract guards
exercise binary/empty inputs, Unicode paths, link text, moved refs, missing
objects and the original per-file hash reference through real Git processes.

The command requires intervening protected contract changes to be integrated.
Unrelated main updates retain the checked-out head and its existing build outputs.
A changed head, changed trusted rules, revoked status or lost maintainer role requires a fresh update.
If authorization publication fails after the push, rerun the same command: a
valid exact-head signed result is reused. Neither the installed global hook nor
branch protection is disabled. Signer registry edits also require this route;
an existing enrolled maintainer attests new-device enrollment.

"Maintainer-reviewed" describes repository responsibility, not a requirement for
the end user personally to read the patch or for a particular chat session to
approve it. Human maintainers and authorized AI agents follow the same checks.
GitHub's normal bypass protections stay active. The CI migration's explicitly
authorized temporary admin route was specific to that migration; it is not a
general approval route for later contract changes.

Rust caches keep the existing 10 GB quota and three shared main publishers.
The publisher and complementary native jobs derive their cache paths from
`.github/local-acceptance/cache_layout.py`. Sources stay in `candidate/rust`,
while native Cargo outputs use the publisher's `rust/target` path. Identical keys
with different paths have different cache versions and cannot share an entry.
`CARGO_TARGET_DIR` is bound after restore so it does not change the publisher's
environment hash. Required output prints `CI_CACHE_LAYOUT` and `CI_CACHE_BINDING`.
Real Cargo/native-process acceptance checks both layouts, dependency reuse,
fresh candidate execution and refusal of a conflicting target. It adds no product
journey coverage; real hosted restores establish remote cache hits and timings.
Native Rust profiles also use checksum-pinned sccache 0.19.0 with a local
2 GiB compiler-object cache. Cargo outputs remain in their original target;
the object cache never supplies a previous test result or a source-freshness
attestation. Original Cargo arguments and environment settings, including target
inheritance in nested Cargo and compile-time environment reads, are retained.
Explicit caller wrappers and sccache settings remain authoritative.
Each profile owns a private server and stops it on success or failure. A retained
ownership record prevents reuse of storage after an interrupted parent while
its server is still alive; the tool's 10-minute idle timeout bounds orphaned
servers. Client-side compilation keeps rustc under the original priority observer.
Compilers stay BelowNormal/nice while actual tests stay Normal. Busy disk-cache
storage uses the original Cargo command immediately, without another wait.
The early workspace-lint handoff keeps its existing exact command recipe.
`compiler_objects_acceptance.py` runs actual Cargo, nextest, nested Cargo and PTY
processes with independent outputs, changed features/compile-time environment,
an original failing source, repaired source and explicit-wrapper controls. It
runs inside the existing native-cache guard. Cache code changes additionally
select the actual native workspace on author and complementary hosts. These
checks add no product coverage credit. Hosted object persistence and whole-PR
savings are not established by the private fixture's cache-hit counts.
The guard image fetches the fixture's exact shared Cargo manifest and lockfile
while building. A network-disabled Cargo fetch verifies those dependencies
before either full matrix starts; missing offline inputs fail at preparation.
The same image provisions the pinned compiler tool and rejects corrupted archive
and executable bytes before retaining the intact tool. Both tools are rechecked
with networking disabled before the matrices; isolated homes copy verified public
tool bytes, never compiler objects or lease claims.
The guard image declares the executor's `SCODE_ACCEPTANCE_BUILD_JOBS=1` cap
explicitly and checks it at entry. Its existing two-CPU, 2 GiB and 256-PID limits
remain unchanged. The compiled canary also checks the effective Cargo job count
against an explicit executor cap. A plain Cargo environment default alone does
not constrain the executor's automatic host-sized budget.
Protected profile modules and their compiler-cache companion are materialized
from the same frozen Git policy. A candidate's working-tree companion is never
an execution source. The original cache tests also exercise this managed loader.
Scheduling plans and completed receipts share the existing exact recipe approval
lookup in protected policy, including the source-without-test-groups condition.
Original signatures, source/tree/base, author/platform responsibility, required
checks, native workflow and dispatcher checks remain mandatory. This preserves
an authentic plan when only an explicitly approved equivalent recipe advances;
it records no completed check. `LOCAL_ACCEPTANCE_PLAN_DECLARED_EQUIVALENCE`
reports the exact accepted profile set. Actual Git/SSH/HTTP/finalizer controls
retain original notes, logs and process counts; undeclared, malformed and
cross-profile approvals still refuse. Cache migration approves only the prior
native recipes whose unchanged core commands were source-verified. The changed
guard remains fresh and cache implementation changes select the real workspace.
Queue maintenance retains current runs and cancels obsolete merge groups only
when both their Git ref and live queue membership are gone. See the
[CI workload audit](docs/ci-workload-audit-20261029.md) for measurements and limits.

### Mock parity harness — optional

The deterministic mock at `mock-anthropic-service` and the harness
at [`rust/scripts/run_mock_parity_harness.sh`](./rust/scripts/run_mock_parity_harness.sh)
remain available for debug-bisection of complex agent-loop
failures. **Not a required pre-PR check.** Reach for it when a PTY
failure's root cause sits behind transport / runtime / API surface
noise and you want to shrink the search. The reference doc is at
[`docs/mock-parity-harness.md`](./docs/mock-parity-harness.md).

## Parity work — standing rule

When making a parity decision against `anthropics/claude-code`,
**always** also check `claude-code-best/claude-code` (CCB) — the
TypeScript behavioral reference — before settling the resolution.
CHANGELOG entries are usually too coarse on their own; CCB
converts them into readable source. CCB is **not** a cherry-pick
source for our Rust tree; we read it for understanding only. The
full triage flow, the Tier 1 source list (public CC surfaces,
the private `sudoprivacy/claude-code` snapshot, the runtime-
observation combo), the sync markers, and the resolution
taxonomy now live in [`sudo-code-roadmap.html`](./sudo-code-roadmap.html) under
Goal 2.

Every design write-up for a feature with parity intent **leads**
with a CCB validation section: which CCB files were read, what
behavior was confirmed, what surprises were found, and what
decisions follow. Design write-ups live in
[`sudo-code-roadmap.html`](./sudo-code-roadmap.html) under the goal they belong to.
When a plan ships or is superseded, remove its content from
`sudo-code-roadmap.html` in the same PR; ROADMAP tracks the live state, not
history.

## Experimental features — standing rule

Every feature that is still experimental — not yet a committed part
of the product surface, still being validated, or built ahead of the
host that will use it — ships behind a **feature flag**, OFF by
default. No hidden env-var-only switches, no "it's harmless because
nothing triggers it": if it's experimental, it's flagged.

The mechanism is `runtime::experiments`:

- **Register** the flag as an `Experiment` variant (one enum, the
  single registry) and mirror its key in `config_schema`'s
  `EXPERIMENTAL_CHILDREN` so `settings.json` validation knows it.
- **Enable** via `"experimental": {"<flagKey>": true}` in any
  settings scope, or `SUDOCODE_EXPERIMENT_<UPPER_SNAKE>=1` in the
  environment (env wins; an explicit `0`/`false` force-disables).
  Unknown keys under `experimental` are a load error — a typo must
  fail loud, not silently leave the experiment off.
- **Gate** every consumer through
  `experiments::is_enabled(Experiment::<Name>)` — never a raw
  `std::env::var` check.
- **Graduate** by deleting the flag and the dead branch in one PR —
  never by flipping the default to `true`. If the experiment fails,
  the flag's removal deletes the feature with it.

This does not conflict with the "no hiding shipped features behind
config" bright line above: that rule is about *finished* behavior;
this rule is about features that are explicitly not finished. The
flag is the boundary between the two — and it must be temporary.

Current experiments are enumerated in
[`rust/crates/runtime/src/experiments.rs`](./rust/crates/runtime/src/experiments.rs);
that file is the SSOT, not this doc.

## Project Layout

The repository is a Cargo workspace rooted at
[`rust/`](./rust). Everything else (`README.md`, `docs/`, `assets/`,
`scripts/`) is documentation or tooling around that workspace. The
crate map and per-crate responsibilities live in
[`rust/README.md`](./rust/README.md).

Working defaults live in `.scode.json` (committed). Machine-local
overrides live in `.nexus/sudocode/settings.local.json` (gitignored).
Prefer editing existing files intentionally over replacing them
wholesale — small, reviewable diffs land faster than large rewrites.

All `cargo` commands in this guide assume your working directory is
`rust/` unless stated otherwise.

## Prerequisites

- **Rust (stable)** — install via [rustup](https://rustup.rs). The
  workspace pins `edition = "2021"` and tracks the current stable
  toolchain used by CI.
- **`rustfmt`** and **`clippy`** components (installed by default
  with `rustup`; otherwise `rustup component add rustfmt clippy`).
- A POSIX-like shell for the helper scripts under `scripts/` and
  `rust/scripts/`.
- (Optional) `python3` if you plan to run
  `.github/scripts/check_doc_source_of_truth.py` locally.

Credentials are only needed when invoking a live model; the build,
tests, and lints run without them.

## Building

```bash
cd rust/

# Debug build of the whole workspace
cargo build --workspace

# Release build (produces ./target/release/scode)
cargo build --release
```

## Release channels

Three channels, one build path (`release.yml` builds whatever tag lands):

- **nightly** — `nightly.yml` moves the rolling `nightly` tag to main's
  HEAD every day at midnight CST (16:00 UTC) when main advanced; the tag
  push rebuilds the full platform matrix (Linux x64/arm64, macOS
  arm64/x64, Windows x64/arm64) and overwrites the single `nightly`
  prerelease on GitHub.
- **release candidate** — promote a soaked nightly by tagging **the same
  commit** it was built from: `git tag v0.2.0-rc.1 <nightly-commit> &&
  git push origin v0.2.0-rc.1`. Any tag containing `-` publishes as a
  prerelease. Promotion is by commit, not by copying artifacts — the
  rebuild keeps one publish path and the commit is what soaked.
- **stable** — tag `vX.Y.Z` on the RC's commit once it has soaked.
  Stable tags also mirror to immutable Tencent COS `release/vX.Y.Z/`
  directories when COS credentials are configured; nightly and RC stay on
  GitHub only. Publication uploads the GitHub assets as a draft, stages every
  mirror artifact and reads it back to verify SHA-256, then publishes GitHub
  and switches `release/latest/version.txt`. Installers resolve that pointer
  once, keeping the previous complete version usable during recovery. Without
  COS credentials the release is GitHub-only; partial credentials fail the job.

Rerun a failed **publish-release job** with its original build artifacts to
recover mirror delivery; do not move the tag. Matching immutable objects are
verified and skipped, differing objects fail without replacement, and an older
retry cannot move the mirror pointer backward. A public stable release is never
made draft or overwritten on retry. If GitHub finalization succeeds but pointer
promotion fails, the previous mirror remains usable until that retry completes.
If recovery needs a publisher fix from main, run
`gh workflow run release.yml --ref main -f resume_tag=vX.Y.Z` after merging it.
This skips every build and reuses the existing GitHub assets, original checksum
manifest and tagged installer. It never moves the stable tag or replaces its
binaries; the same immutable mirror verification and promotion rules apply.
`python3 e2e/release-mirror/run.py` exercises interrupted uploads, retry, version
pinning, optional bundle delivery and checksum rejection with the real publisher
and installer through a local HTTP mirror; it is also a CI gate.

### Release acceptance coverage

Accept every release candidate with one dispatch on its commit:

```bash
gh workflow run rust-ci.yml --ref <tag-or-branch> -f live_only=true
```

Per-PR acceptance is scoped to each PR's inputs. Newly built artifacts have a
mandatory gate in their release build jobs. The explicit full-candidate dispatch
retains its existing rendering and provider comparisons against the previous tag:

| Coverage | Per PR (local + complementary CI) | Release acceptance |
|---|---|---|
| Rendering matrix against the fixed baseline | `core`: short reply and 12 KiB Markdown, 36 judged observations, for CLI, engine-core, PTY and rendering-driver inputs | `stress`: `core` plus foreground task, background task and repeated turns, 84 judged observations (`release-rendering`, Linux Docker) |
| Cargo/dependency metadata and unrelated crate source | Native functional checks | Indirect performance effects judged by the same `stress` matrix |
| Optimized stream/resume, 100 ms startup and compiler context | Functional native suite and dev/mailbox compiler context; optimized checks for their harness/driver edits | Mandatory on each newly built artifact, on all six native platform jobs, before packaging/publication |
| Real provider and model journeys | Scripted model transport | Live API smoke and Linux/Windows PTY suites |

The rendering and native jobs use no credentials. Their evidence is uploaded as
`release-rendering` and `release-native-<os>` artifacts for 30 days. The same
stress matrix runs locally with
`SCODE_RENDERING_PROFILE=stress python .github/local-acceptance/checks.py --native-checks rendering --base <previous-tag>`.

Sudo Code forbids `unsafe_code` workspace-wide (`unsafe_code = "forbid"`
in `rust/Cargo.toml`). Relaxing this for a single crate goes through
the PR description first.

## Required checks

Prompt cache observation defaults, field semantics and synthetic scenarios live
in [`spec/prompt_cache_observations.json`](./spec/prompt_cache_observations.json).
The native ledger reads that specification directly. Endpoint probes can pin
the same public file by commit and SHA-256. Cache efficiency formulas remain
in [`spec/cache_metrics.json`](./spec/cache_metrics.json). Synthetic usage
snapshots with expected totals live in
[`spec/cache_usage_fixtures.json`](./spec/cache_usage_fixtures.json), with a
validated [schema](./spec/cache_usage_fixtures.schema.json) beside it. Report
and scanner adapters should reproduce those totals.

Completion intervals and TTL advisor coverage share the bounded ledger reader
in `engine-core::cache_report`. With attempt timing, the final cumulative
snapshot replaces earlier snapshots of the same request identity. Different
attempt starts, attempt counts, routes, models and context segments stay
separate. Legacy rows without attempt timing deduplicate only exact repeats;
missing correlation IDs remain separate. Malformed rows interrupt adjacency.
Missing usage fields remain unknown and do not match an explicit zero.
The JSON stats report reads the ledger once for both views.

The fixture PTYs exercise that reader, reporting and shared metric formulas.
Their `stats.json` rollup is prefilled from expected totals, so they do not
establish the runtime's accumulation of those totals, provider cache hits or
measured TTL savings. The growing-snapshot fixture also retains two attempts
that reuse a correlation ID. Reuse these public fixtures when adding scanner
or runtime adapters, rather than maintaining another copy of the observations.
Validate the schema with `python scripts/check_cache_usage_fixtures.py`
(`jsonschema==4.26.0`); Rust CI runs this check, including invalid variants.

The contract also defines endpoint input-accounting paths and synthetic usage
vectors: Anthropic input excludes cache reads/writes; OpenAI and Gemini input
includes cache tokens. Missing counters remain unknown. Pair-total comparisons
are scoped heuristics; their results describe reported usage and require
separate evidence for billing or credential identity.

`input_accounting` owns provider usage paths and the nonnegative int64 domain
for both counters and computed totals. `provider_semantics` retains the primary
read/write field names as a generated compatibility projection; aliases belong
to the accounting paths. After changing those paths, run
`python scripts/generate_cache_semantics.py`. The existing public-fixture check
also verifies that this projection has not drifted, without rewriting the file.
Public product specs select native workspace acceptance because Rust embeds
them as runtime inputs. Acceptance configuration and signer registration keep
their separate infrastructure guards.

### Startup latency

`SCODE_TRACE_STARTUP=1 scode version` prints local phase durations to stderr.
`--version` reports console setup and clap parsing; `version` also reports
permission defaults, argument conversion, legacy config migration and output.
Agent startup additionally reports prompt construction, auth configuration,
model capabilities and engine construction (including plugin/MCP discovery).
The `arguments` phase includes its nested `clap` and `permission_config` phases;
do not add their durations together. Tracing is disabled by default and sends
no telemetry. Cumulative time begins at `main`, excluding OS process creation
and executable loading.

Measure that complete process cost with
`python3 e2e/startup/run.py --bin rust/target/release/scode`. It warms the page
cache, samples each version command 40 times with an isolated config/home,
checks that tracing preserves JSON stdout, and enforces a 100 ms median budget.
The harness requires `rustc` and builds a minimal Rust reference outside the
timed region. It alternates paired invocations with identical arguments,
environment, output bytes and pipe capture. `reference_median_ms` measures
that process-launch floor; `paired_difference_median_ms` is scode's added cost
on the same host. This difference includes executable loading and application
work, so it is not a measurement of config parsing alone. `traced_main_ms`
covers only the instrumented portion inside `main`; `traced_wall_ms` also
includes process creation, loading, tracing output and shutdown. The single
traced sample is diagnostic, not the untraced median used for the budget.
Pass `--report path.json` to retain all paired samples and phase timings; Local acceptance retains the report; complementary CI uploads its native logs.
The author-local and complementary jobs together run it on Linux, macOS and Windows. The initial
100 ms budget is a coarse regression guard, not a claim of equivalent hardware
performance: local Intel macOS medians were 22.6 ms (`--version`) and 23.1 ms
(`version --output-format json`), with about 1.3 ms measured inside `main`.
Windows' previously reported 27–29 ms needs its own phase measurements before
selecting an optimization; the macOS measurements do not establish that cause.

### Other acceptance checks

Terminal palette policy has one owner: `render::ColorTheme`. It selects semantic
Markdown scopes, code/metadata colors, status fallbacks, and contrast on painted
surfaces. `code_theme` reads syntax assets; Markdown and diff renderers consume
resolved styles. Preserve this boundary when adding presentation roles. External
tool ANSI remains source data decoded at the terminal adapter boundary.

`pty_syntax_theme` compares terminal cells with a pinned, independently generated
Codex reference, including nested Markdown, live preview, resize, history handoff
and resume. Its fixture records the upstream commit and helper digest. Refresh
it with `e2e/codex-style/capture.py` against the intended upstream checkout when
updating parity; retain the upstream style/contrast helpers rather than copying
scode's expected colors into the reference. Brand amber remains a scode role.

Renderer changes pass the browser smoke in the required `Local acceptance`
status, and scoped mandatory local rendering judges their typing latency. See the shared
[release performance gate](e2e/terminal-resize/README.md#release-performance-gate).
Discuss budget, baseline or measurement changes with a maintainer using the
retained A/A and A/B evidence; get explicit approval before relaxing the gate.

When explicitly dispatched under the [provider acceptance policy](#company-model-routing-in-ci),
the `Model Compatibility` workflow discovers gateway models and runs live PTYs
in batches of at most eight, with three batches running concurrently.
Each process has one 90-second deadline. A pass requires exit 0 and the expected
persisted assistant answer; model names and error text cannot satisfy it.
If retry backoff outlasts the deadline, the report uses the last HTTP attempt's
recorded error to identify unavailable providers. A later HTTP success clears
that evidence; unexplained timeouts and empty or incorrect answers still fail.
`SCODE_COMPAT_REPORT=/absolute/path/report.json` keeps a report outside the
temporary test workspace and updates it after every model. CI retains partial
reports and rejects incomplete sweeps or a sweep with no passing models.
An explicit protocol refusal is reported separately as `REFUSED`, with the
provider's category and explanation when supplied. It does not establish
compatibility and is never retried. An all-refused sweep still fails the
aggregate's requirement for a real pass. Reports include request parameters
and HTTP results for diagnosis, without prompts, tool definitions or headers.

Ordinary PRs gate source checks selected by the protected acceptance plan.
For Rust changes, run the applicable local checks before pushing:

```bash
cd rust/
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
```

`scripts/fmt.sh` from the repo root wraps `cd rust && cargo fmt`:

```bash
./scripts/fmt.sh           # auto-format
./scripts/fmt.sh --check   # CI-equivalent check
```

The managed native acceptance runner checks the same Cargo target roots and
recursive local path dependencies with rustfmt batches bounded to 24,000 UTF-16
command units. This avoids Windows' process argument limit as the workspace
grows. Every edition, target kind, module traversal and package-local rustfmt
configuration remains selected. Real Cargo/rustfmt controls compare formatting
failures and exercise an oversized workspace, including failure in a later batch.
Private enforcement homes copy the prepared nextest cache, which every reader
still verifies against the pinned archive. The real old-checkout guard requires
offline tool reuse and rejects a corrupted private executable while proving
the enclosing tool cache retains its original bytes.

The workspace enables `clippy::all` and `clippy::pedantic` at
`warn` level. A few pedantic lints are allowed globally
(`module_name_repetitions`, `missing_panics_doc`,
`missing_errors_doc`); see `[workspace.lints.clippy]` in
`rust/Cargo.toml`. Prefer fixing the lint over `#[allow(...)]`.
Allow attributes should be scoped tightly and explained in a
comment.

`cargo test --workspace` runs PTY tests as part of the regular
suite (not behind `#[ignore]`). Tests that need network or a live
API gate behind `#[ignore]` or a dedicated `--test` integration
target (see `rusty-sudocode-cli/tests/acp_live_smoke.rs` for the
live-API smoke pattern, run explicitly with local credentials or manual dispatch).

### Targeted commands

```bash
# A single crate's tests
cargo test -p runtime

# A single test by name
cargo test -p runtime -- session_resume

# Clippy a single crate
cargo clippy -p rusty-sudocode-cli

# Build a single crate
cargo build -p mock-anthropic-service

# Documentation build (catches broken intra-doc links)
cargo doc --workspace --no-deps
```

## Company model routing in CI

Every PR runs real CLI/PTY regressions with a scripted model transport, including
keyboard input, rendering, resize, cancellation, descendant process cleanup and
recovery in the next turn. The process checks inspect actual child PIDs and fresh
files. They do not require paid credentials.

Paid model acceptance is **on demand**, normally on the developer's machine.
Ordinary PR, queue and main workflows never authorize paid provider calls.
The real-daemon queue gate uses a local model fixture and needs no API key;
it remains required. Model compatibility has no scheduled catalog fan-out.

The shared command registry is
`.github/scripts/live_model_acceptance.py`. Without `--execute` it only prints
the selected commands. Local PTYs use the developer's existing model configuration;
ACP suites additionally require `PROXY_AUTH_TOKEN`, and the mailbox suite requires
`ANTHROPIC_API_KEY`. Do not place credentials on a command line or commit them.

```bash
# Preview; no model calls
python .github/scripts/live_model_acceptance.py --suite smoke

# Explicitly use the local configured model/key; retains logs and result.json
python .github/scripts/live_model_acceptance.py --suite smoke --execute
```

Run relevant suites when changing provider protocols, credentials, model routing,
tool-result compatibility or model-dependent behavior, and accept the actual
release candidate before publication. Record the tested commit, commands,
result and remaining provider/platform limits. A local run does not prove
untested provider channels or platforms. This is an explicit release acceptance
step, not a new automatic paid job on every tag. The default smoke suite covers
actual file contents through resumed and headless CLI turns. The fuller `pty`,
`acp`, `native-acp`, `mailbox`, `offline-nexus`, `subagent` and `compat` suites
retain their existing assertions. `--tests` selects registered test targets;
unknown targets are rejected. The full PTY suite requires its separately
configured Azure profile and `SCODE_AZURE_MODEL`.

When cloud execution is specifically needed, a maintainer can explicitly run
the existing company-funded ACP/cohost and Linux/Windows PTY workflow on a
chosen branch. Normal manual cache/preflight runs do not authorize it:

```bash
gh workflow run rust-ci.yml --ref <branch> -f live_only=true
```

The same dispatch runs the credential-free release coverage jobs; see
[Release acceptance coverage](#release-acceptance-coverage).
There is no scheduled paid PTY/model compatibility run. Select one local or
manual execution for the required candidate coverage rather than repeating it
before and after every merge. A deterministic native pass is not evidence of
real provider compatibility, and an unexecuted live suite is not a live pass.
The runner records one attempt per command, a bounded suite duration and failures
without retrying to green. Quota/authentication/rate-limit/service failures
remain failed with their category; they are not reported as product regressions
or silently converted into success. Finite company key quotas remain external
spend limits; the runner does not replenish them or claim to reserve money.

Provider preflight and traces identify quota, authentication, rate-limit and
service failures separately. A failed preflight keeps the job failed and retains
its evidence; it never counts as model acceptance. Successful small probes do
not reserve quota for the full workflow.

CI uses the company `SUDOROUTER_CI_API_KEY` secret. Personal developer accounts
belong only in local configuration and must never be copied into CI secrets.
`SUDOROUTER_NATIVE_CHANNEL` optionally selects an existing company-accessible
native Anthropic channel without replacing the credential. Both native ACP
acceptance and model review read this repository variable. They also share
`SUDOROUTER_NATIVE_MODEL` (default `claude-sonnet-4-6`). GPT protocol acceptance
keeps its separate baseline and normal company routing. File/tool PTY journeys
use a faster model verified with the original approval, report-read and replay
assertions; changing a route or model does not raise their turn budgets.

`SUDOROUTER_AZURE_CHANNEL` selects the existing Azure channel for the DeepSeek
reasoning/file-tool journey. That journey uses its own copied auth profile with
the same company credential and a channel suffix. Other PTYs keep normal routing.
Validate this route with the complete file-read scenario on Linux and Windows.
The provider probe and CLI profile share `AZURE_PROXY_AUTH_TOKEN`. Evidence names
the auth profile separately from the model, so the same model can be checked on
different provider channels without silently reusing another credential.

Verify a changed native route with the original real subagent/cache scenario,
retaining its output/thinking budgets and assertions. A small successful request
alone is insufficient. Channel selection does not prove another model or the
rest of the catalog is usable.

Review runs follow the current ready PR head and cancel superseded runs. They
use the job-scoped read-only GitHub token so workflow edits can receive a real
review. The workflow checks the live PR and checkout before and after execution;
the repository-owned [review instructions](.github/prompts/code-review.md) define
the independent reviews to run. Prior comments never substitute for reviewing
the current commit. Missing execution records, skipped fan-out and incomplete
model runs fail the job, with the sanitized execution evidence retained.

## Measuring prompt-cache behaviour

Anthropic's prompt cache matches a **byte-exact prefix**, so anything that
rewrites an earlier part of a request throws away everything cached after it.
That failure is invisible in normal output — the run still succeeds, it just
costs several times more input. Two places report it:

- **Live, per turn** — `⚡NN%` and `✎NN%` are cache reads and writes divided
  by all prompt input, including uncached input. `Σ⚡NN%` is the session's
  cumulative read share. Write shares of at least 25% appear red.
- **After the fact, across sessions** — `scode cache stats`
  (`--output-format json` for a machine-readable version):

```bash
scode cache stats
```

It reads per-session records under
`~/.nexus/sudocode/cache/prompt-cache/<session-id>/stats.json` and reports
reads, writes, cache reuse and cache breaks. Reuse divides reads by reads plus
writes, so it differs from the status line's prompt denominator. JSON includes
both metrics and per-session completion-interval histograms. Each histogram
reads at most the last 512 KiB and reports sampling, duplicates and malformed
rows. These are response-completion intervals; they do not establish expiry.
Legacy rollups without uncached-input coverage report prompt shares as unknown.
A break is classified: `system
prompt changed` / `tool definitions changed` / `model changed` / `message
history rewritten at index N`, or **`unexpected`** when reads dropped while
the request fingerprint held steady — that last one means the prefix went
cold for a reason the request does not explain, and is worth chasing.

Two limits worth knowing before reading the numbers:

- **Only the Anthropic provider records.** A session on an OpenAI-compatible,
  Gemini or Codex model leaves nothing here, so empty stats mean "not
  measured", not "no problems".
- **Appending is not rewriting.** Every turn appends messages, so a reason
  naming a *rewritten* index is the one that matters; it is the signature of
  code mutating history that the provider has already cached.

If you change anything that touches the system prompt, the tool list, or the
message history, take a reading before and after — the cost of getting this
wrong does not show up as a failure.

`spec/cache_metrics.json` is the calculation source for the StatusSlot, offline
reports and Ladder monitoring. Update it, run
`python scripts/generate_cache_metrics.py`, and commit both generated outputs.
CI runs `--check` to prevent drift. Ladder vendors `scripts/cache_metrics.py`
at a recorded commit and SHA-256; sync it with Ladder's
`tools/sync_cache_metrics.py --source-root <scode-checkout>` after committing
the canonical changes. Keep calculations in this shared contract instead of
adding separate percentages or TTL price formulas to a renderer or monitor.
Changes to measurement do not enable telemetry or automatic TTL selection.

### TTL advice

An unpinned session's outgoing TTL follows this advice; `cache_ttl_1h` in
`sudocode.json` pins it instead (`true` 1h, `false` 5m), and a pinned session
only gets the recommendation. Before a session has evidence of its own, it
starts from the TTL this user's sessions last decided from evidence, kept in
`cache/prompt-cache/ttl-default.json`, or 5m before anything is learned: one
user's agents tend to work alike, so that is the best guess. Deleting the file
resets it. The advisor sends no keepalive or extra model calls. Use
`scode cache stats --output-format json` to inspect `ttl_advisor` per session
and `ttl_default` for the learned start.

The existing request ledger gains request-start, response-start and logical
completion times, final client cache-marker TTLs (including the default 5m),
context segment, opaque route/settings keys, HTTP attempt count and completion
coverage. Compaction requests are distinguished. Upstream TTL remains unknown:
client markers do not prove a gateway's effective policy. Interrupted streams
retain consumed usage with `completed=false`; retried, restarted, mixed-TTL and
legacy samples are excluded from the advisor, rather than counted as clean
successes. Prefix compatibility reuses the existing local fingerprints.
An empty-response fallback opens a separate request scope. Its consumed partial
usage is recorded before the replacement, and incomplete responses never advance
the successful prefix state. Missing or legacy rows break reuse adjacency.

One recommendation is frozen at session creation/resume and each committed
compaction. Resume of the same model/route/configuration and segment reuses the
decision. An empty new session keeps its starting TTL (the learned default, or 5m) until
there is enough history at a later boundary. This does not reset cumulative session usage.
Training reads at most a 512 KiB ledger tail and only completed history available
before that boundary; derived state retains at most 16 decisions. No full prompt,
tool payload, URL or credential is added. Public advice omits correlation keys.

Recommendations compare two explicit scenarios using request-start intervals
and token-weighted reusable prefixes. Within each scenario, matching client
prefixes are assumed to survive on the same upstream until that TTL; actual
hits under that same requested TTL are preserved even beyond a timing cutoff.
Unknown routing, eviction and boundary jitter mean these are **hypothetical
costs, not measured savings or expiry attribution**. The first request, context
changes and policy transitions are conservatively charged a full cached-input
write. Uncached input and output are excluded because their cost is common to
the compared policies. Reference model prices and their date/source live in
`spec/cache_ttl_prices.json`; unknown models keep the configured TTL. These
prices do not establish gateway billing or subscription-quota economics.

The first version requires eight eligible intervals and a 10% scenario-cost
margin to suggest changing TTL; otherwise it preserves the configured choice.
Those are heuristic decision thresholds, not statistical confidence bounds.
`forward_comparison` scores only later requests with a previously frozen
decision, against both fixed baselines using the same scenario assumptions and
the shared generated cost function. It does not fit the policy to those later
requests.

Main turns, subagent turns, and compaction share
`api::session_message_request`. Add session-level request fields there rather
than assembling another request in a caller. `SystemPrompt` uses copy-on-write
sections and cached rendering; session snapshots survive compact/resume/fork.
Do not refresh file-derived context as a side effect of compaction.

Run `SCODE_TEST_BACKEND=mock cargo test --workspace --test pty_cache_prefix --test pty_compaction_safety`
from `rust/` after changing these paths. The native workspace profile includes
both suites: the author runs their own platform and complementary CI supplies
the remaining hosts according to the protected acceptance plan. The merge queue
carries those exact-head results instead of repeating the workspace suites.
These PTYs capture HTTP bodies after
provider conversion and compare the stable prefix through retries, discovery,
compaction, forks, and restart. They also exercise explicit prompt replacement.
The scoped-context cases verify config roots, CLI package discovery, layered
memory, and memory write permissions through the real CLI.
Mock usage numbers are fixtures, not evidence of real provider cache hits.

`SUDOCODE_CACHE_DIAGNOSTICS=1` opts into the provider's diagnosis beta. Its
previous-message ID belongs to one client/session and is restored from
`cache/prompt-cache/<session-id>/cache-diagnostics.json` on restart. The existing
SSE decoder captures diagnostic metadata across HTTP chunks, JSON whitespace and
whole-JSON responses; it does not run a second parser. With diagnostics disabled,
no diagnostic fields, beta or diagnostic-state I/O are added. State persistence
is best effort and cannot fail or retry a provider request. Set
`SUDOCODE_DUMP_REQUESTS` to keep diagnoses next to request dumps; these opt-in
dumps can contain conversation data and must not be published as raw evidence.
The PTY prefix suite tests correlation and unchanged wire prefixes, not real
cache availability. Neither a correct diagnosis chain nor identical client
prefixes establish a live cache hit or an upstream account/cache-shard identity.

## Running the CLI locally

For the canonical CLI surface, run `cargo run --bin scode -- --help`.
For day-to-day workflows, [`docs/usage.md`](./docs/usage.md) is the
SSOT.

## Working on a single crate

Each crate under `rust/crates/` has its own `Cargo.toml` and (where
relevant) `README.md`. When adding a new crate:

1. Create the crate under `rust/crates/<name>/`
   (`cargo new --lib crates/<name>` from inside `rust/`).
2. Inherit shared metadata from the workspace where possible:
   ```toml
   [package]
   name              = "<name>"
   version.workspace = true
   edition.workspace = true
   license.workspace = true
   ```
3. Add it to `rust/Cargo.toml` workspace dependencies if other
   crates will consume it.
4. Run the [Required checks](#required-checks) before committing.

## Commit style

- **Many small commits, all on one PR.** Each commit is one logical
  change with a clear subject. Ten focused commits beat one monster
  commit — easier to review, easier to revert, easier for an AI
  agent or human to read a year later.
- Short, imperative subject:
  `runtime: parse session.close in resume mode`.
- One logical change per commit. Reviewers split mixed commits.
- Reference issues in the body, not the subject: `Fixes #123` on
  its own line at the bottom.
- **No squash.** We use `--merge` (not `--squash`) when landing PRs
  so feature-branch history reaches `main` intact. Squashing has
  historically lost attribution across our multi-agent workflow.
- Commits omit generated artifacts (`target/`, IDE folders,
  `.DS_Store`).
- **No AI signatures** (`Co-Authored-By: <AI tool>`,
  "Generated with …", any AI footer). They pollute `git log`
  without adding signal.

## Pull request guidelines

Before opening a PR:

- [ ] Branch off the latest `main`.
- [ ] The managed hook ran every check selected by the protected acceptance
      plan on the author's platform and produced a valid exact-head receipt.
      Use its printed commands and local/CI responsibilities; do not infer a
      full Rust build from this checklist for documentation or SDK-only edits.
- [ ] Rust-changing PRs pass `cargo clippy --workspace --all-targets --all-features -- -D warnings`; the queue runs it on all three native platforms.
- [ ] The change does **not** add unit tests (see
      [Test policy](#test-policy)).
- [ ] The change does **not** introduce `ratatui`, alternate-screen
      mode, a `--tui` flag, or any in-CLI multi-agent UI (see
      [Design principles](#design-principles-you-must-respect)).
- [ ] If the change is parity work, the PR description leads with a
      CCB validation section (see
      [Parity work — standing rule](#parity-work--standing-rule)).
- [ ] If the change touches user-facing surfaces, the relevant
      `docs/*.md` SSOT is updated.
- [ ] If the change touches the agent loop, tools, or API surface,
      the mock parity harness still passes (run it locally and
      mention the result in the PR description).
- [ ] Commits are small and focused; no squashing in the local
      history.

When opening the PR:

- Use the provided
  [pull request template](./.github/pull_request_template.md) — fill
  in each section.
- **Title:** short, imperative, optionally prefixed with the affected
  area (`runtime:`, `cli:`, `docs:`).
- **Summary:** explain *why* the change is needed, not just *what*
  it does. Link the issue it closes.
- **Testing:** list the exact commands run (`cargo test -p runtime`,
  PTY scenario name, `./scripts/run_mock_parity_harness.sh` if
  invoked, manual REPL run, etc.).
- **Scope:** keep PRs focused on one repo area at a time. Bug fixes
  and unrelated refactors land in separate PRs.
- **Draft early:** open the PR as a draft for directional feedback
  before polishing. Draft PRs run only the fast checks; the full
  cross-platform matrix starts when you mark the PR ready (and runs
  again in the merge queue), so iterate as a draft and mark it ready once.

Reviewers may push small fixups directly to your branch (formatting,
typos) unless the PR description opts out. Larger changes go through
review comments.

Once approved, land it with **`gh pr merge <n> --merge`** (not squash,
not `--admin`). That adds the PR to the merge queue: GitHub merges it onto
the latest `main` (plus any PRs queued ahead), runs the required checks on
that result, and merges it into `main` as a merge commit only when they
pass. A PR does not need to be up to date with `main` to enter the queue,
so don't merge `main` into your branch just to catch up — merge `main` only
to resolve a real conflict. Preserve the existing commits and their
authors; do not rebase or squash already-published history.

When you do merge `main`, the merge commit must not quietly change files
your PR does not otherwise edit. Agents resolving merge-from-main conflicts
have silently reverted other PRs' work this way. If you need to adapt code
to something new on `main`, make that edit in its own commit after the
merge. If a merge that already went out did adapt something, declare it on
any commit with a `Merge-adapts: <path>` trailer (for example,
`git commit --allow-empty -m 'Declare merge adaptation' -m 'Merge-adapts: <path>'`).

The "Verify PR history" check enforces this on the PR and again on the
merge-queue group. For each merge commit it recomputes Git's own merge of
the two parents; a file may differ from that result only if it conflicted
or is declared. It also rejects new conflict markers. (nexi-lab/nexus's
`check_merge_history.py` enforces the same rule.) It
runs for every PR regardless of changed paths. Its implementation is
[`check_pr_history.py`](.github/scripts/check_pr_history.py). The required
CI checks must pass before merging, including for admins.

## Reporting bugs & requesting features

- **Bugs:** use the
  [bug report template](./.github/ISSUE_TEMPLATE/bug_report.md).
  Include `scode --version`, the exact command run, the auth mode,
  and the full error output. If the bug touches a design principle
  (see above), call that out explicitly — it helps triage.
- **Features:** use the
  [feature request template](./.github/ISSUE_TEMPLATE/feature_request.md).
  Describe the user-visible behavior and the motivating use case
  before jumping to implementation ideas. If your feature request
  contradicts a Design principle (e.g. proposes a `--tui` flag,
  proposes unit tests for a new module), the request will be closed
  without code review.
- **Security issues:** email the maintainers listed in the
  repository's security contact (see `SECURITY.md` if present) or
  use GitHub's private vulnerability reporting.

## Publishing the roadmap to ShareOne (interim, manual)

`sudo-code-roadmap.html` is the SSOT plan file. Mirroring it to ShareOne for
at-a-glance external viewing is currently a manual maintainer step
— both human and AI contributors can run it. The long-term plan
exposes `publish_to_shareone` as an LLM tool that any `scode` agent
can call, tracked as a Goal 3 candidate inside `sudo-code-roadmap.html`; it
ships when a real user asks for it.

Until that tool exists, the documented manual recipes are:

**Create a new share** (each run yields a fresh URL):

```bash
curl -s -X POST https://shareone.app/api/v1/pages \
  -H "X-API-Key: $SHAREONE_API_KEY" \
  -H "Content-Type: application/json" \
  -d "{\"filename\":\"sudo-code-roadmap.html\",\"html_content\":$(jq -Rs . < sudo-code-roadmap.html),\"allow_comments\":true}"
```

The response includes `share_url` — that is the page to share.

**Update an existing share** (stable URL — pass the `share_id` you
got back from a prior POST):

```bash
curl -s -X PUT "https://shareone.app/api/v1/pages/<share_id>" \
  -H "X-API-Key: $SHAREONE_API_KEY" \
  -H "Content-Type: application/json" \
  -d "{\"filename\":\"sudo-code-roadmap.html\",\"html_content\":$(jq -Rs . < sudo-code-roadmap.html),\"allow_comments\":true}"
```

Get a `SHAREONE_API_KEY` from <https://shareone.app>. URL stability
across re-publishes is optional; for a one-off shareable link the
POST form is enough.

## License

By contributing, your contributions are licensed under the
[MIT License](./LICENSE.md) that covers this project.

**No CLA. No copyright assignment.** You keep your copyright; we
receive a permissive license. That is the whole arrangement — no
contributor agreement to sign, no maintainer-administered waiver,
no future ability for the maintainers to re-license your code
without your consent. This is a deliberate "Always" principle of
sudocode — see
[`README.md` § Design principles](./README.md#design-principles).

---

Happy hacking on `scode`.
