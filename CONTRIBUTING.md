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
author runs Linux locally; CI runs Windows and macOS. Co-host release verification
follows the same host split when its inputs change. Each supported OS remains
covered; this does not claim every CPU architecture is exercised on every PR.
Actual tagged co-host releases retain their five-platform artifact builds.

Rendering A/A and A/B, painted browser performance and viewport resource checks
run automatically in local Docker when production or measurement inputs change.
They keep their existing baselines, rounds, assertions and budgets. They have no
scheduled or manual cloud replacement. Documentation-only changes run preflight;
ordinary test-only changes do not rebuild an unchanged performance pair. Local
compilers use one job and BelowNormal/nice priority while Cargo and tests stay
Normal. Set CARGO_TARGET_DIR to an existing owned build directory to reuse it.
Full logs and partial failures remain under ~/.nexus/local-acceptance/results/.
On a Windows box with unregistered Build Tools, set
`git config localAcceptance.vsdevcmd C:/path/to/Common7/Tools/VsDevCmd.bat`;
otherwise the runner discovers Visual Studio or uses the existing Developer shell.

A signed Git note binds the successful command list and log hashes to the exact
commit, tree, diff base, policy and native host. The hook publishes that note
before the branch. GitHub's read-only metadata job verifies the signature against
public keys enrolled on main and selects the missing hosts. Missing, failed,
stale or tampered receipts block the Local acceptance required check. A signature
authenticates the enrolled author's statement; it is not remote execution
attestation. Maintainers may attest a contributor's PR by actually running its
checks on an enrolled host; forks need not receive repository secrets.

A failed native matrix job cancels the other pending/running platforms. The
trusted publisher checks the actual completed jobs directly; no second hosted
summary job repeats those checks. Scheduling-only edits keep valid completed
local evidence when its executor/profile/image are unchanged and it covers the
current required checks. New commits or changed commands still need acceptance.
Provider tests on main run only for changed provider inputs, and real-daemon
queue tests run only for changed daemon inputs. Documentation/CI-only changes
skip those unchanged expensive journeys. The explicit provider dispatch stays
available for gateway diagnosis.

The required `CI preflight` job shares one checkout and tool setup for format,
generated cache schemas, docs, history, boundary, release-mirror and provider
probe checks. Its first failing step blocks its daemon/provider dependents.
PRs always start this job, including SDK-only changes; path filters cannot leave
a required status pending. The queue's protected acceptance gate still checks
the combined Git history. No unique product test is removed by this consolidation.
Signature verification and status publication use Windows runners, with an
explicit Python interpreter, to avoid two extra admissions to the busy Linux
pool. This metadata placement does not replace native Linux or macOS tests.

The merge queue verifies each included PR's current receipt and completed
cross-platform workflow, then checks the combined Git history. It does not repeat
the native suites on the combined merge-group tree. A runtime interaction that
exists only in the combination of independently verified PRs is therefore outside
that pre-merge runtime coverage. Main pushes do not repeat deterministic suites.
Real provider/API assertions on main are unchanged; mock transport results do not
establish live cache hits. Cache-prefix, compaction and all existing workspace
cases remain, including standalone vendored Mermaid and benchmark type checks.

Existing PRs do not need a cosmetic merge from main to acquire the hook files:
the managed hook loads protected-main command definitions. Deliberate edits to the
acceptance contract need a maintainer-reviewed bootstrap so a PR cannot approve
its own weaker policy. Registering another public device key does not invalidate
other devices' current receipts. GitHub's normal bypass protections stay active;
the CI migration itself uses the explicitly authorized temporary admin route.

Rust caches keep the existing 10 GB quota and three shared main publishers.
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
cargo clippy --workspace --all-targets -- -D warnings
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
live-API smoke pattern, which CI runs on `main` and by manual dispatch).

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

Funded acceptance runs ACP and co-host model journeys, plus every live PTY
journey in the `live-model-pty` job on Linux and Windows. It runs on main pushes
and manual dispatch. Before merging a change to those paths, run it on the PR
branch:

```bash
gh workflow run rust-ci.yml --ref <branch> -f live_only=true
```

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
from `rust/` after changing these paths. CI includes both suites in the full
workspace run: Linux PR feedback, then Linux, macOS and Windows on the integrated
merge-queue commit. These PTYs capture HTTP bodies after
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
- [ ] `cargo fmt --all --check` passes.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` is clean.
- [ ] `cargo test --workspace` passes locally.
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
