# CLAUDE.md

Guidance for agents working in this repository.

## Working agreement

- Prefer small, reviewable changes. Group related edits in a single
  commit; split unrelated edits across commits.
- Shared defaults live in `.scode.json`. Machine-local overrides live in
  `.nexus/sudocode/settings.local.json`.
- Update existing files intentionally; edit content rather than replace
  whole files unless the file is being restructured.

## Verification

The Rust workspace lives in `rust/`. From the repo root the standard
checks are:

```bash
cd rust
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

`scripts/fmt.sh` from the repo root wraps `cd rust && cargo fmt` and
forwards flags.

## Documentation map

- [`README.md`](./README.md) — project entry, install, quick start.
- [`ROADMAP.md`](./ROADMAP.md) — goals.
- [`CONTRIBUTING.md`](./CONTRIBUTING.md) — contributor setup and PR
  workflow.
- [`docs/`](./docs/) — topic-scoped SSOTs (usage, authentication,
  permissions, ACP, models, plugins, parity, mock harness, container).
- [`docs/plans/active/`](./docs/plans/active/) — in-flight design plans.
- [`docs/plans/archive/`](./docs/plans/archive/) — landed and superseded
  plans.
- [`rust/README.md`](./rust/README.md) — Cargo workspace map.

When the repository workflow changes, update this file along with the
change.
