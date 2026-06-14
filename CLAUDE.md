# CLAUDE.md

Guidance for agents working in this repository.

## Working agreement

- Prefer small, reviewable changes. Group related edits in a single
  commit; split unrelated edits across commits.
- Shared defaults live in `.scode.json`. Machine-local overrides live in
  `.nexus/sudocode/settings.local.json`.
- Update existing files intentionally; edit content rather than replace
  whole files unless the file is being restructured.

## Parity work: standing rule

When making a parity decision against `anthropics/claude-code`, **always**
also check `claude-code-best/claude-code` (CCB) — the TypeScript
behavioral reference — before settling the resolution. CHANGELOG entries
are usually too coarse on their own; CCB converts them into readable
source. CCB is not a cherry-pick source for our Rust tree; we read it
for understanding only. The full triage flow, sync markers, and
resolution taxonomy live in [`docs/parity.md`](./docs/parity.md).

Every design write-up for a feature with parity intent **leads** with a
CCB validation section: which CCB files were read, what behavior was
confirmed, what surprises were found, and what decisions follow.
Design write-ups live in [`ROADMAP.html`](./ROADMAP.html) under the goal
they belong to (the `!` bash mode section under Goal 3 is the shape
future design write-ups follow). When a plan ships or is superseded,
remove its content from ROADMAP.html in the same PR; ROADMAP tracks the
live state, not history.

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

## Publishing ROADMAP.html to ShareOne (interim, manual)

`ROADMAP.html` is the SSOT plan file in this repo. Mirroring it to
ShareOne for at-a-glance external viewing is currently a manual
maintainer action. The long-term plan — exposing
`publish_to_shareone` as an LLM tool that any `scode` agent can call
— is tracked as a Goal 3 candidate inside ROADMAP.html itself and
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
across re-publishes is optional; if you only want a one-off shareable
link, the POST form is enough.

## Documentation map

- [`README.md`](./README.md) — project entry, install, quick start.
- [`ROADMAP.html`](./ROADMAP.html) — goals.
- [`CONTRIBUTING.md`](./CONTRIBUTING.md) — contributor setup and PR
  workflow.
- [`docs/`](./docs/) — topic-scoped SSOTs (usage, authentication,
  permissions, ACP, models, plugins, parity, mock harness, container).
- [`rust/README.md`](./rust/README.md) — Cargo workspace map.

When the repository workflow changes, update this file along with the
change.
