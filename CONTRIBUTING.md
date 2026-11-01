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

The Linux queue reuses a fixed VFS checkout and its isolated Cargo inputs and
outputs. An independent main-push publisher saves only a missing exact key;
queue jobs only restore. The key binds the lockfile revision, compiler, runner
image, build recipe, canonical path and explicit codegen settings. Source bytes
are checked against the pinned Git tree before use, including same-mtime edits
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

One acceptance runs per host at a time. Every `check`, whether from the hook,
`contract-update` or a manual run, holds `~/.nexus/local-acceptance/host.lock`
while its profiles run; a second invocation prints the holder from
`host-lease.json` and starts when that holder exits, including an abrupt exit.
Concurrent suites on one machine contend for CPU, memory and temporary paths,
which turns timing budgets into failures that cost whole profile reruns.
Processes started under the holder inherit `SUDOCODE_ACCEPTANCE_HOST_LEASE`, so
fixture checks nested inside an acceptance run without waiting for their parent.
`attempt.json` records the wait as `host_lease_wait_seconds`; the receipt format
is unchanged.

On macOS, rendering/browser profiles and the release startup measurement require
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

Rendering A/A and A/B, painted browser performance and viewport resource checks
run automatically in local Docker when CLI, engine-core, shared PTY helpers,
measurement sources or Rust build/dependency inputs change. Other crates' source
changes keep their selected native functional checks but skip these statistical
performance comparisons. This deliberately reduces performance coverage for
indirect effects from API, runtime, tools, ACP and service implementations; it
does not establish that those effects are impossible. A change known to affect
the performance workloads must include the corresponding measurement scenario.
Selected measurements keep their baselines, per-run assertions and budgets.
Paired rounds are listed below.
Changes limited to `e2e/terminal-resize/benchmark.py`, `gui-benchmark.py`,
`performance-policy.json` and `gui-policy.json` select both statistical profiles
and the native benchmark-builder regression in CI. These files feed the explicit
statistical drivers; ordinary workspace and resize suites do not execute them.
Those unrelated native suites and their complementary CI hosts are therefore
omitted for these four inputs. Rust, shared terminal hosts, unknown external
inputs and mixed changes keep their applicable native suites. Real committed-Git
planner and PR/push/queue fixtures cover this boundary, including deletion and
renaming a shared host into a measurement-only path.
Rendering and browser use the same canonical candidate checkout and Cargo
outputs under the existing exclusive benchmark lease. Each baseline and the
trusted rendering driver keep separate outputs. Copied normal release binaries
stay fixed while dev-feature test drivers compile. Real Cargo acceptance checks
cross-profile candidate reuse, exact binary identity and unchanged observer stages.
When rendering and browser run in the same acceptance process for the same
candidate, protected policy and Docker context, browser reuses rendering's exact
immutable image and saved normal release. The retained image ID and binary hash
live in that process only; previous output directories and mutable image tags
cannot authorize reuse. The release enters browser through read-only inputs and
is checked against its source, policy, image and SHA256 before use. Changed
inputs or a fresh process use the ordinary build path; corrupted retained files
fail explicitly. Browser baseline builds, dev-feature drivers, paired GUI samples
and viewport observers still run. Real Cargo acceptance compiles another source
into the shared cache between profiles and verifies the original normal release
is preserved without another normal candidate build. Actual Docker acceptance
exercises the frozen image, invocation boundary and five tampered identities/bytes.
The host and container entrypoint share the canonical `container/<profile>`
output layout. The Docker fixture calls the actual entrypoint and dispatcher,
then retains the resulting native binary for browser reuse; a fixture workload
replaces statistical product measurements. Missing or mismatched output fails
without promoting old raw results into a signed profile.
This removes a duplicate build even when another workline churns the cache. It
does not shorten a successful measurement matrix or prove production speedup.
Performance drivers compile in the binary-free `scode-benchmark-drivers` package,
referencing the original CLI test sources. They launch only the explicitly
selected, copied release through `SCODE_TEST_BIN`. Distinct, opt-in target names
keep ordinary workspace acceptance from running those cases twice. Older author
snapshots use their original CLI targets; a present but broken standalone package
fails instead of falling back. Cargo artifact records and a deliberately broken
unused CLI prove the production builder omits that extra binary, while retaining
normal/dev feature separation, actual driver execution and all observers. These
infrastructure fixtures add no product journey coverage. Per-run assertions,
sampling policy, baselines and budgets stay unchanged.
The guard also reads actual Cargo metadata to require unique opt-in test targets
that reference the original sources. Changes inside the driver package select
this guard in CI; real Git event fixtures check that selection.
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
Rendering completes and judges one scenario at a time. Four scenarios keep
two warm-ups, per-run interaction counts and every metric budget. The short scenario keeps
five A/A and seven alternating A/B pairs, preserving the full check that caught
a real RSS regression. The other scenarios use three A/A and three A/B pairs.
A 12 KiB Markdown workload replaces the independent 8 KiB prose, 12 KiB prose
and 2,300-byte Markdown workloads, adopting the consolidation proposed in #1196.
Short prose, foreground tools and background tools remain separate. The required
core matrix runs 68 native samples instead of 96. This reduces workload coverage:
regressions specific to the removed plain-text sizes or short Markdown can escape
the combined workload. No metric is filtered from the remaining cases.
Browser comparison uses
three A/A and three A/B pairs plus two warm-ups: 14 runs instead of 26.
Headless and painted viewport comparisons inherit the same 3/3 rounds: each also
runs 14 instead of 26 samples. Repeated comparisons total 110 instead of the
original 234 (53.0% fewer); the separate 60-sample painted interaction check remains.
Per-run parsed/painted interactions and budgets stay intact.
This reduces repeat coverage and precision: intermittent regressions and
environmental noise have fewer chances to appear. A preserved actual viewport
A/A calibration refuses CPU noise with five pairs but not its first three;
that original record remains noisy. Equivalent statistical sensitivity and
whole-lifecycle savings are not established by these sample counts.
The repeated-turn stress scenario retains five A/A and seven A/B pairs.
A failed/noisy/invalid scenario stops the matrix and releases the lease; later
scenarios remain unrun. The mandatory command prints the declared pair counts,
active scenario and evidence path. `measurements/report.json` combines the original
case reports, with original policy and binary/observer hashes, completed scenarios
and remaining work. The actual benchmark CLI validates and describes the plan
before npm or Cargo starts; the host checks its policy hash and complete sample
count before accepting a passing report. Real Cargo/native-process acceptance exercises
full success, relative RSS regression, broken drivers and changed observers with
the unchanged paired evaluator, and rejects missing CPU metrics or passing
reports with missing samples. It does not claim product performance coverage.
Both changed performance recipes require new execution; earlier full-matrix
results cannot authorize the reduced sampling policy. Native profile recipes
remain unchanged. Actual native fixture processes verify the 68/14 schedules,
alternating sides, the full short check and rejection of relative RSS/browser
regressions and policies below their minimum rounds. These fixtures add no
product performance coverage credit.

Default SSE response archives reuse at most one finished deflate state across
captures. Active captures retain separate states; a finished state releases its
file, session budget and attachment lease before reuse. Compression level, gzip
bytes, CRC, per-session limits and context separation stay unchanged. Real PTY
acceptance checks six consecutive distinct responses and earlier-file integrity.
The conditional PR codec check also writes 42 actual files, including six
concurrent captures, independently decodes them and compares quota failure
prefixes with the previous flate2 encoder. It adds no live-provider coverage.
These comparisons have no scheduled or manual cloud replacement. Documentation-only changes run preflight;
ordinary test-only changes do not rebuild an unchanged performance pair. Local
preflight checks actual conflicts against its supplied base and fetched
`origin/main` before product builds and benchmark leases. It prints the exact
paths and recovery step without changing the checkout or index. Clean branches
may stay behind main. Preflight stages the exact hash-pinned checker from its
protected policy, so an older author branch does not need a source update to
receive these diagnostics. The Git-only `e2e/pr-history/run.py` fixture stays required
in CI preflight; editing it alone skips unrelated native product builds.
Development-only Cargo manifest/lock changes do not select normal release,
service, co-host or daemon builds. The shared selector reads committed Git/TOML:
resolver 2 and workspace settings must stay unchanged, manifest edits must be
limited to dev dependencies, and existing lock packages/normal edges must stay
unchanged. New versions of an existing package name, deleted lock nodes and
unknown/malformed metadata keep the ordinary scope. Native workspace/lint,
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
Hosted complementary builds use the available compiler CPUs. That hosted-only
setting is applied after cache restore; local author builds keep one worker and
all test commands, deadlines and compiler priorities stay unchanged.
Full logs and partial failures remain under ~/.nexus/local-acceptance/results/.
Deterministic workspace cases use the checksum-pinned nextest runner with four
global test workers. The runner lives in a private acceptance cache; it does not
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
Contract validation checks the Docker Git/signature/authorization matrix before
Cargo fixtures and author-host native guards. Passing runs keep every existing
check; an early metadata failure leaves later compilation unrun.
Release acceptance builds the normal release `scode` once, copies it outside
Cargo outputs, and points the native workspace's test-profile driver at that
exact executable with `SCODE_TEST_BIN`. The driver keeps the workspace mailbox
feature graph; release-only dev-dependency compilation is no longer repeated.
The same stream/resume assertions and 40-sample, 100 ms startup budget remain.
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
host; custom output directories can use the printed `check --resume-from` command.
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
in a private local Docker container. A native host smoke retains the actual failing
process, checkpoint, SSH purpose, damaged-log and automatic-resume workflow; the
native nextest/Cargo enforcement fixture also remains on the author OS. The full
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

The required `CI preflight` job shares one checkout and tool setup for format,
generated cache schemas, docs, history, boundary, release-mirror and provider
probe checks. Its first failing step blocks its daemon/provider dependents.
PRs always start this job, including SDK-only changes; path filters cannot leave
a required status pending. The queue's protected acceptance gate still checks
the combined Git history. No unique product test is removed by this consolidation.
After merge, main reuses those completed integrity checks. Its lightweight
preflight still resolves the pinned protocol and changed-input scope needed by
the existing daemon/provider jobs. PR, queue and explicit runs retain the full
preflight; no product assertion is removed.
Signature verification and status publication use Windows runners, with an
explicit Python interpreter, to avoid two extra admissions to the busy Linux
pool. This metadata placement does not replace native Linux or macOS tests.

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
the native suites on the combined merge-group tree. Rust-changing PRs also run
strict all-target/all-feature Clippy on Linux; the queue runs that same strict
check on Linux, Windows and macOS against the combined tree. The required daemon
gate waits for those checks and rejects failed or unexpectedly skipped jobs.
The lint command stops scheduling additional crates after the first error;
compiler jobs already running may finish before matrix cancellation. Successful
runs retain every target, feature and `-D warnings` check. An actual three-package
Cargo/Clippy fixture compares the old continuing command with the current CI
command, verifies all packages/targets/features after fixing the warnings, and
records the actual compiler priorities. It runs once on a PR only when the lint
workflow, fixture or scope selector changes; queue/main reuse its result.
In the queue, independent failure-only callers stop unfinished jobs in the same
Rust CI run after either Clippy or real daemon acceptance fails. Each caller
waits only for its own failing dependency. The shared cancellation job has the
Actions write permission and runs no checkout or candidate executable. Successful
runs allocate no cancellation runner. The original failed step remains visible;
the command prints how to inspect it and rerun failed work after repair. A live
GitHub fixture exercises both failure directions and the uncancelled success
path on a disposable branch. These are CI execution checks and add no product
journey coverage.
A runtime interaction that exists only in the combination of independently verified PRs is therefore outside
that pre-merge runtime coverage. Main pushes do not repeat deterministic suites.
Real provider/API assertions remain available through explicit acceptance;
mock transport results do not
establish live cache hits. Cache-prefix, compaction and all existing workspace
cases remain, including standalone vendored Mermaid and benchmark type checks.

Existing PRs do not need a cosmetic merge from main to acquire the hook files:
the managed hook loads protected-main command definitions. Registering another
public device key does not invalidate other devices' current receipts.

#### Acceptance contract changes

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

Renderer changes must pass the mandatory local rendering/browser profiles and
the required `Local acceptance` status. See the shared
[release performance gate](e2e/terminal-resize/README.md#release-performance-gate).
Discuss budget, baseline or measurement changes with a maintainer using the
retained A/A and A/B evidence; get explicit approval before relaxing the gate.

The daily `Model Compatibility` workflow discovers gateway models and runs
live PTYs in batches of at most eight, with three batches running concurrently.
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

CI gates these on every PR. Run them locally before pushing:

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

### Experimental TTL advice

Enable `"experimental": {"cacheTtlAdvisor": true}` in settings, or set
`SUDOCODE_EXPERIMENT_CACHE_TTL_ADVISOR=1` for the process. This experiment
collects local metadata and **never changes the outgoing TTL**, including
explicit `cache_ttl_1h` overrides. It sends no keepalive or extra model calls.
Use `scode cache stats --output-format json` to inspect `ttl_advisor` per
session. The text report points to this view without adding terminal warnings.

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
decision. An empty new session keeps its configured TTL until there is enough
history at a later boundary. This does not reset cumulative session usage.
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
Those are experimental decision thresholds, not statistical confidence bounds.
`forward_comparison` scores only later requests with a previously frozen
decision, against both fixed baselines using the same scenario assumptions and
the shared generated cost function. It does not fit the policy to those later
requests. Auto-application needs a separate decision after credible validation.

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
