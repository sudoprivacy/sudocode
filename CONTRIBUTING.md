# Contributing to Sudo Code

Thanks for your interest in contributing to **Sudo Code** (`scode`) — a Rust-based AI coding agent engine with ACP (Agent Communication Protocol) support. This document explains how to get a local checkout building, how to run the required checks, and how to submit a pull request that lands smoothly.

Sudo Code is a community-driven project. Bug reports, documentation fixes, new crates, and protocol work are all welcome.

---

## Table of Contents

- [Code of Conduct](#code-of-conduct)
- [Project Layout](#project-layout)
- [Prerequisites](#prerequisites)
- [Building](#building)
- [Required Checks (run these before every PR)](#required-checks-run-these-before-every-pr)
  - [`cargo fmt`](#cargo-fmt)
  - [`cargo clippy`](#cargo-clippy)
  - [`cargo test`](#cargo-test)
- [Optional / Targeted Checks](#optional--targeted-checks)
- [Running the CLI Locally](#running-the-cli-locally)
- [Working on a Single Crate](#working-on-a-single-crate)
- [Mock Parity Harness](#mock-parity-harness)
- [Commit Style](#commit-style)
- [Pull Request Guidelines](#pull-request-guidelines)
- [Reporting Bugs & Requesting Features](#reporting-bugs--requesting-features)
- [License](#license)

---

## Code of Conduct

Be respectful, be concise, and assume good faith. Disagree about code, not people. Maintainers may close or lock conversations that turn personal or off-topic.

## Project Layout

The repository is a Cargo workspace rooted at [`rust/`](./rust). Everything else (top-level `README.md`, `USAGE.md`, `docs/`, `assets/`, `scripts/`) is documentation or tooling around that workspace.

```
.
├── README.md                  # Top-level overview & quick start
├── USAGE.md                   # Task-oriented usage guide
├── docs/                      # Long-form documentation
├── scripts/                   # Helper scripts (fmt, dogfood build, etc.)
├── rust/                      # ← Cargo workspace (run cargo here)
│   ├── Cargo.toml             # [workspace] members = ["crates/*"]
│   ├── Cargo.lock
│   ├── README.md              # Workspace-level docs
│   ├── crates/
│   │   ├── api/               # HTTP / ACP surface
│   │   ├── commands/          # Built-in slash commands
│   │   ├── compat-harness/    # Compatibility test harness
│   │   ├── mock-anthropic-service/  # Deterministic mock backend
│   │   ├── nexus-vfs-client/  # Virtual filesystem client
│   │   ├── plugins/           # Plugin runtime & tools
│   │   ├── rag/               # Retrieval / indexing
│   │   ├── runtime/           # Agent loop & session state
│   │   ├── rusty-sudocode-cli/  # `scode` binary
│   │   ├── telemetry/         # Tracing / metrics
│   │   └── tools/             # Built-in tool implementations
│   └── scripts/               # Workspace-scoped harnesses
└── .github/                   # CI workflows & issue/PR templates
```

**All `cargo` commands in this guide assume your working directory is `rust/`** unless stated otherwise.

## Prerequisites

- **Rust (stable)** — install via [rustup](https://rustup.rs). The workspace pins `edition = "2021"` and tracks the current stable toolchain used by CI.
- **`rustfmt`** and **`clippy`** components (installed by default with `rustup`; otherwise `rustup component add rustfmt clippy`).
- A POSIX-like shell for the helper scripts under `scripts/` and `rust/scripts/`.
- (Optional) `python3` if you plan to run `.github/scripts/check_doc_source_of_truth.py` locally.

Credentials are only needed when you actually invoke a model — the build, tests, and lints do not require them.

## Building

```bash
cd rust/

# Debug build of the whole workspace
cargo build --workspace

# Release build (produces ./target/release/scode)
cargo build --release
```

Sudo Code forbids `unsafe_code` workspace-wide (`unsafe_code = "forbid"` in `rust/Cargo.toml`). If you have a legitimate need to relax this for a single crate, raise it in your PR description first.

## Required Checks (run these before every PR)

CI runs three gating jobs against the `rust/` workspace: `cargo fmt --all --check`, `cargo test --workspace`, and `cargo clippy --workspace`. Run them locally before pushing — they take seconds and save a round trip.

### `cargo fmt`

```bash
cd rust/
cargo fmt --all            # auto-format
cargo fmt --all --check    # CI-equivalent check (no changes written)
```

A convenience wrapper lives at [`scripts/fmt.sh`](./scripts/fmt.sh) that `cd`s into `rust/` for you:

```bash
./scripts/fmt.sh           # from repo root
./scripts/fmt.sh --check   # pass-through args
```

### `cargo clippy`

```bash
cd rust/
cargo clippy --workspace
```

The workspace enables `clippy::all` and `clippy::pedantic` at `warn` level. A few pedantic lints are allowed globally (`module_name_repetitions`, `missing_panics_doc`, `missing_errors_doc`); see `[workspace.lints.clippy]` in `rust/Cargo.toml` for the full list. Prefer fixing the lint over `#[allow(...)]` — if you must allow, scope it as tightly as possible and explain why in a comment.

To treat warnings as errors locally (matches a stricter sweep):

```bash
cargo clippy --workspace --all-targets -- -D warnings
```

### `cargo test`

```bash
cd rust/
cargo test --workspace
```

Tests are expected to pass on a clean machine with no network access and no API credentials. If a test legitimately requires network or a live API, gate it behind an `#[ignore]` attribute or a dedicated `--test` integration target (see `rusty-sudocode-cli/tests/acp_live_smoke.rs` for the live-API smoke pattern, which CI only runs on `main`).

## Optional / Targeted Checks

```bash
# Run a single crate's tests
cargo test -p runtime

# Run a single test by name
cargo test -p runtime -- session_resume

# Clippy a single crate
cargo clippy -p rusty-sudocode-cli

# Build a single crate
cargo build -p mock-anthropic-service

# Documentation build (catches broken intra-doc links)
cargo doc --workspace --no-deps
```

## Running the CLI Locally

```bash
cd rust/

# From a debug build
cargo run --bin scode -- --help
cargo run --bin scode -- prompt "explain this codebase"

# From a release build
./target/release/scode
./target/release/scode doctor          # health check
```

See the top-level [`README.md`](./README.md) and [`USAGE.md`](./USAGE.md) for the full set of flags, auth modes (`api-key` / `subscription` / `proxy`), and model aliases.

## Working on a Single Crate

Each crate under `rust/crates/` has its own `Cargo.toml` and (where relevant) `README.md`. When adding a new crate:

1. Create the crate under `rust/crates/<name>/` (`cargo new --lib crates/<name>` from inside `rust/`).
2. Inherit shared metadata from the workspace where possible:
   ```toml
   [package]
   name    = "<name>"
   version.workspace = true
   edition.workspace = true
   license.workspace = true
   ```
3. Add it to `rust/Cargo.toml` workspace dependencies if other crates will consume it.
4. Run `cargo fmt --all`, `cargo clippy --workspace`, and `cargo test --workspace` before committing.

## Mock Parity Harness

Many of the integration tests depend on a deterministic, Anthropic-compatible mock service that ships in the workspace as the `mock-anthropic-service` crate. The harness lives at [`rust/scripts/run_mock_parity_harness.sh`](./rust/scripts/run_mock_parity_harness.sh):

```bash
cd rust/
./scripts/run_mock_parity_harness.sh
```

If you change anything in `runtime/`, `tools/`, `api/`, or the parity scenarios in `rust/mock_parity_scenarios.json`, please run the harness locally and mention the result in your PR.

## Commit Style

- Keep commit messages short and imperative: `Add ACP session.close support`, not `added support and fixed a few things`.
- One logical change per commit where practical. Reviewers will ask you to split mixed commits.
- Reference issues in the body, not the subject: `Fixes #123` on its own line at the bottom.
- Do not include generated artifacts (`target/`, IDE folders, `.DS_Store`) in commits.

## Pull Request Guidelines

Before opening a PR:

- [ ] Branch off the latest `main`.
- [ ] `cargo fmt --all --check` passes.
- [ ] `cargo clippy --workspace` is clean (or you have explained the remaining `#[allow]`s).
- [ ] `cargo test --workspace` passes locally.
- [ ] If you touched anything user-facing, the top-level `README.md` and/or `USAGE.md` are updated.
- [ ] If you touched the agent loop, tools, or API surface, the mock parity harness still passes.
- [ ] Your commits are reasonably squashed and have meaningful messages.

When opening the PR:

- Use the provided [pull request template](./.github/pull_request_template.md) — don't delete the sections, just fill them in.
- **Title:** short, imperative, optionally prefixed with the affected area (`runtime:`, `cli:`, `docs:`).
- **Summary:** explain *why* the change is needed, not just *what* it does. Link the issue it closes.
- **Testing:** list the exact commands you ran (`cargo test -p runtime`, `./scripts/run_mock_parity_harness.sh`, manual REPL run, etc.).
- **Scope:** keep PRs focused. Bug fixes and unrelated refactors belong in separate PRs.
- **Draft early:** if you want directional feedback before polishing, open the PR as a draft and say so.

Reviewers may push small fixups directly to your branch (formatting, typos) unless you opt out in the PR description. Larger changes will be requested via review comments.

Once approved, a maintainer will merge — usually as a squash commit. Please don't force-push after a review has started unless asked.

## Reporting Bugs & Requesting Features

- **Bugs:** use the [bug report template](./.github/ISSUE_TEMPLATE/bug_report.md). Include `scode --version`, the exact command you ran, the auth mode, and the full error output.
- **Features:** use the [feature request template](./.github/ISSUE_TEMPLATE/feature_request.md). Describe the user-visible behavior and the motivating use case before jumping to implementation ideas.
- **Security issues:** please **do not** open a public issue. Email the maintainers listed in the repository's security contact (see `SECURITY.md` if present) or use GitHub's private vulnerability reporting.

## License

By contributing, you agree that your contributions will be licensed under the [MIT License](./LICENSE.md) that covers this project.

---

Thanks again — happy hacking on `scode`.
