# Contributing to Sudo Code

Thanks for your interest in contributing to **Sudo Code** (`scode`).

This document is the canonical guide for working in this repo —
prerequisites, the design rules every PR is judged against, the
required checks, and the commit/PR workflow. **Human contributors
and AI agents follow the same rules in here.** Read it once before
your first contribution.

> **Before you write code, read [`README.md`](./README.md) and
> [`ROADMAP.html`](./ROADMAP.html).** Sudo Code has a small but
> bright-line set of design rules; PRs that contradict them are
> declined regardless of code quality. The fast summary lives in
> [Design principles you must respect](#design-principles-you-must-respect)
> below; the full statement is in `ROADMAP.html`.

## Table of Contents

- [Code of Conduct](#code-of-conduct)
- [Design principles you must respect](#design-principles-you-must-respect)
- [Test policy](#test-policy)
- [Parity work — standing rule](#parity-work--standing-rule)
- [Project Layout](#project-layout)
- [Prerequisites](#prerequisites)
- [Building](#building)
- [Required checks](#required-checks)
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
requires a separate `ROADMAP.html` edit before the code lands — not
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
  hydra / your tmux — not inside scode. (See ROADMAP § Goal 4.)

- **No CLA, no copyright assignment, no closed-source escape
  hatch.** Contributors retain their copyright; their code is
  MIT-licensed and that is the whole arrangement.

- **No "users might misclick" as a reason to hide a feature.**
  Either ship it or don't. Hiding correct behavior behind a config
  flag because the 99% might fumble = treating us like the 99%.
  We are not them.

- **No silent / forced updates.** Semver. Breaking change = major
  bump + release notes. You pin a version, that version stays.

- **No telemetry by default.** Opt-in is explicit; never buried.

The full 11 Always / 10 Never list lives in [`README.md` § Design
principles](./README.md#design-principles). When in doubt, read
that section before opening the PR.

## Test policy

sudocode is deliberately opinionated about what to test and how.

| Layer | Status | When to write |
|---|---|---|
| **PTY (`pty-expect`)** | **Required CI gate**; the only layer counting toward the 90% e2e coverage metric | Every user-facing feature |
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
have caught and a PTY scenario would not, plus a `ROADMAP.html` edit
to change the rule before the unit test lands.

(See `ROADMAP.html` § Goal 1 → "Test layer policy" for the long form.)

### Running tests

```bash
cd rust/
cargo test --workspace                     # all tests (PTY tests included)
cargo test -p runtime                      # one crate
cargo test -p runtime -- session_resume    # one test by name
```

PTY tests live in `crates/rusty-sudocode-cli/tests/pty/`. They use
the `sudoprivacy/pty-expect` crate and run as part of
`cargo test --workspace` on Linux and macOS. Windows runtime
support is deferred to `pty-expect` v0.2; on Windows CI the PTY
tests compile but skip-execute for now.

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
taxonomy now live in [`ROADMAP.html`](./ROADMAP.html) under
Goal 2.

Every design write-up for a feature with parity intent **leads**
with a CCB validation section: which CCB files were read, what
behavior was confirmed, what surprises were found, and what
decisions follow. Design write-ups live in
[`ROADMAP.html`](./ROADMAP.html) under the goal they belong to.
When a plan ships or is superseded, remove its content from
`ROADMAP.html` in the same PR; ROADMAP tracks the live state, not
history.

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

Sudo Code forbids `unsafe_code` workspace-wide (`unsafe_code = "forbid"`
in `rust/Cargo.toml`). Relaxing this for a single crate goes through
the PR description first.

## Required checks

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
live-API smoke pattern, which CI only runs on `main`).

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
  before polishing.

Reviewers may push small fixups directly to your branch (formatting,
typos) unless the PR description opts out. Larger changes go through
review comments.

Once approved, a maintainer merges with **`--merge`** (not squash)
to preserve the feature-branch commit history. The "Block Merge
Commits in PR" CI check requires you to **rebase** (not merge) when
syncing your feature branch onto the latest `main` —
`git rebase main` + `git push --force-with-lease` is the safe form.

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

`ROADMAP.html` is the SSOT plan file. Mirroring it to ShareOne for
at-a-glance external viewing is currently a manual maintainer step
— both human and AI contributors can run it. The long-term plan
exposes `publish_to_shareone` as an LLM tool that any `scode` agent
can call, tracked as a Goal 3 candidate inside `ROADMAP.html`; it
ships when a real user asks for it.

Until that tool exists, the documented manual recipes are:

**Create a new share** (each run yields a fresh URL):

```bash
curl -s -X POST https://shareone.app/api/v1/pages \
  -H "X-API-Key: $SHAREONE_API_KEY" \
  -H "Content-Type: application/json" \
  -d "{\"filename\":\"ROADMAP.html\",\"html_content\":$(jq -Rs . < ROADMAP.html),\"allow_comments\":true}"
```

The response includes `share_url` — that is the page to share.

**Update an existing share** (stable URL — pass the `share_id` you
got back from a prior POST):

```bash
curl -s -X PUT "https://shareone.app/api/v1/pages/<share_id>" \
  -H "X-API-Key: $SHAREONE_API_KEY" \
  -H "Content-Type: application/json" \
  -d "{\"filename\":\"ROADMAP.html\",\"html_content\":$(jq -Rs . < ROADMAP.html),\"allow_comments\":true}"
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
