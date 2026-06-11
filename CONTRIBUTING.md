# Contributing to Sudo Code

Thanks for your interest in contributing to **Sudo Code** (`scode`).
This document explains how to set up a local checkout, run the required
checks, and submit a pull request that lands smoothly.

## Table of Contents

- [Code of Conduct](#code-of-conduct)
- [Project Layout](#project-layout)
- [Prerequisites](#prerequisites)
- [Building](#building)
- [Required checks](#required-checks)
- [Optional / targeted checks](#optional--targeted-checks)
- [Running the CLI locally](#running-the-cli-locally)
- [Working on a single crate](#working-on-a-single-crate)
- [Mock parity harness](#mock-parity-harness)
- [Commit style](#commit-style)
- [Pull request guidelines](#pull-request-guidelines)
- [Reporting bugs & requesting features](#reporting-bugs--requesting-features)
- [License](#license)

## Code of Conduct

Be respectful, be concise, and assume good faith. Disagree about code,
not people. Maintainers may close or lock conversations that turn
personal or off-topic.

## Project Layout

The repository is a Cargo workspace rooted at
[`rust/`](./rust). Everything else (`README.md`, `docs/`, `assets/`,
`scripts/`) is documentation or tooling around that workspace. The
crate map and per-crate responsibilities live in
[`rust/README.md`](./rust/README.md).

All `cargo` commands in this guide assume your working directory is
`rust/` unless stated otherwise.

## Prerequisites

- **Rust (stable)** — install via [rustup](https://rustup.rs). The
  workspace pins `edition = "2021"` and tracks the current stable
  toolchain used by CI.
- **`rustfmt`** and **`clippy`** components (installed by default with
  `rustup`; otherwise `rustup component add rustfmt clippy`).
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

CI runs three gating jobs against the `rust/` workspace. Run them
locally before pushing.

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

The workspace enables `clippy::all` and `clippy::pedantic` at `warn`
level. A few pedantic lints are allowed globally
(`module_name_repetitions`, `missing_panics_doc`,
`missing_errors_doc`); see `[workspace.lints.clippy]` in
`rust/Cargo.toml`. Prefer fixing the lint over `#[allow(...)]`. Allow
attributes should be scoped tightly and explained in a comment.

Tests run on a clean machine with no network access and no API
credentials. Tests that need network or a live API gate behind
`#[ignore]` or a dedicated `--test` integration target (see
`rusty-sudocode-cli/tests/acp_live_smoke.rs` for the live-API smoke
pattern, which CI only runs on `main`).

## Optional / targeted checks

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
   name    = "<name>"
   version.workspace = true
   edition.workspace = true
   license.workspace = true
   ```
3. Add it to `rust/Cargo.toml` workspace dependencies if other crates
   will consume it.
4. Run the [Required checks](#required-checks) before committing.

## Mock parity harness

The deterministic Anthropic-compatible mock service ships as the
`mock-anthropic-service` crate. The harness lives at
[`rust/scripts/run_mock_parity_harness.sh`](./rust/scripts/run_mock_parity_harness.sh).

```bash
cd rust/
./scripts/run_mock_parity_harness.sh
```

Changes to `runtime/`, `tools/`, `api/`, or
`rust/mock_parity_scenarios.json` run the harness locally and mention
the result in the PR description. The full reference is at
[`docs/mock-parity-harness.md`](./docs/mock-parity-harness.md).

## Commit style

- Short, imperative subject: `Add ACP session.close support`.
- One logical change per commit where practical. Reviewers split mixed
  commits.
- Reference issues in the body, not the subject: `Fixes #123` on its
  own line at the bottom.
- Commits omit generated artifacts (`target/`, IDE folders, `.DS_Store`).

## Pull request guidelines

Before opening a PR:

- [ ] Branch off the latest `main`.
- [ ] `cargo fmt --all --check` passes.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` is clean.
- [ ] `cargo test --workspace` passes locally.
- [ ] If the change touches user-facing surfaces, the relevant
      `docs/*.md` SSOT is updated.
- [ ] If the change touches the agent loop, tools, or API surface, the
      mock parity harness still passes.
- [ ] Commits are reasonably squashed and carry meaningful messages.

When opening the PR:

- Use the provided
  [pull request template](./.github/pull_request_template.md) — fill
  in each section.
- **Title:** short, imperative, optionally prefixed with the affected
  area (`runtime:`, `cli:`, `docs:`).
- **Summary:** explain *why* the change is needed, not just *what* it
  does. Link the issue it closes.
- **Testing:** list the exact commands run (`cargo test -p runtime`,
  `./scripts/run_mock_parity_harness.sh`, manual REPL run, etc.).
- **Scope:** keep PRs focused. Bug fixes and unrelated refactors land
  in separate PRs.
- **Draft early:** open the PR as a draft for directional feedback
  before polishing.

Reviewers may push small fixups directly to your branch (formatting,
typos) unless the PR description opts out. Larger changes go through
review comments.

Once approved, a maintainer merges — usually as a squash commit.
Force-pushing after a review has started goes through a request.

## Reporting bugs & requesting features

- **Bugs:** use the
  [bug report template](./.github/ISSUE_TEMPLATE/bug_report.md).
  Include `scode --version`, the exact command run, the auth mode, and
  the full error output.
- **Features:** use the
  [feature request template](./.github/ISSUE_TEMPLATE/feature_request.md).
  Describe the user-visible behavior and the motivating use case before
  jumping to implementation ideas.
- **Security issues:** email the maintainers listed in the repository's
  security contact (see `SECURITY.md` if present) or use GitHub's
  private vulnerability reporting.

## License

By contributing, you agree that your contributions will be licensed
under the [MIT License](./LICENSE.md) that covers this project.

---

Happy hacking on `scode`.
