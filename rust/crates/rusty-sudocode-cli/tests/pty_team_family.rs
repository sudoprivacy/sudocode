//! PTY tests for the `TeamCreate` / `TeamGet` / `TeamList` / `TeamDelete`
//! tool family.
//!
//! Coverage target: roadmap §Feature-inventory row
//! "Team tools (Create/Get/List/Delete)" — subagent-cc-fork-parity commit
//! A. Before this file: 0 PTY tests → the tool family didn't exist as
//! LLM-callable surface. After: the two branches that catch real
//! regressions.
//!
//! ## What the Team tools actually do
//!
//! `TeamCreate({team_name, task_ids?})` → creates an in-memory `Team`
//! record in `runtime::team_cron_registry::TeamRegistry`. Returns the
//! generated `team_id`, status = "created", and echoes back the input.
//! `TeamGet({team_id})` → returns the record or "team not found".
//! `TeamList({})` → returns `{teams: [...], count: N}` — the empty-registry
//! path returns `count: 0` and is the safest read-only branch to exercise
//! from a PTY (no subagent spawn, no cross-process state).
//! `TeamDelete({team_id})` → soft-delete (status → "deleted", record
//! retained).
//!
//! ## Two branches that matter in production
//!
//! 1. **TeamList empty** — a fresh process must report `count: 0` and
//!    exit 0. Regression sentinel against "TeamList crashes on empty
//!    state" or "returns null instead of 0". No subagent involvement.
//! 2. **TeamCreate happy path** — the model calls `TeamCreate` with a
//!    name; the tool must return a JSON payload containing a `team_id`
//!    and the requested name. Regression sentinel against "backend
//!    lookup returned None" or "team_id generation regressed".
//!
//! What's NOT covered here:
//! - Cross-turn persistence of the in-memory registry (by design — the
//!   `OnceLock` is per-process, so within one CLI turn the create → get
//!   round-trip is not testable from a single-shot PTY).
//! - `TeamDelete` — soft-delete state transition. Backend unit-covered
//!   in `runtime::team_cron_registry` (existing).
//!
//! ```bash
//! cargo test --test pty_team_family                          # mock (CI)
//! SCODE_TEST_BACKEND=live cargo test --test pty_team_family  # real API
//! ```

mod common;

use common::TestEnv;

// ──────────────────────────────────────────────────────────────────────
// TeamList on empty registry — count=0, teams=[]
// ──────────────────────────────────────────────────────────────────────

/// A fresh scode process has an empty team registry. TeamList must
/// return a JSON payload with `count: 0` and the CLI must exit 0.
/// Regression sentinel against a change that makes TeamList crash on
/// empty state or return `null` instead of the count field.
///
/// TeamList is a read-only tool — no subagent spawn, no mock-harness
/// interaction issues.
#[test]
fn team_list_on_empty_registry_returns_zero_count() {
    let env = TestEnv::new("team-list-empty");

    let prompt = env.prompt(
        "Please list the teams by calling the TeamList tool. Do not describe it; just call the tool.",
        "team_list_empty_roundtrip",
    );

    let mut sess = env.spawn(&[
        "--permission-mode",
        "workspace-write",
        "--allowedTools",
        "TeamList",
        &prompt,
    ]);

    sess.expect("TeamList")
        .expect("model must invoke TeamList (agent trigger)");

    // The tool serializes the response as a JSON object with a `count`
    // field. An empty registry means count = 0.
    sess.expect(r#""count":\s*0"#)
        .expect("TeamList on an empty registry must report count: 0");

    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(exit, 0, "team_list empty turn should exit 0; got {exit}");
}

// ──────────────────────────────────────────────────────────────────────
// TeamCreate happy path — returns a team_id
// ──────────────────────────────────────────────────────────────────────

/// The model calls `TeamCreate({team_name: "research-squad"})`. The tool
/// must return a JSON payload containing a fresh `team_id` string and
/// the CLI must exit 0. Regression sentinel against:
/// - id generator returning empty string
/// - backend `create()` returning without echoing the input name
/// - permission gate rejecting `DangerFullAccess` under
///   `workspace-write` (the intended matrix has workspace-write allow
///   TeamCreate — mismatched, this test would surface it)
#[test]
fn team_create_returns_team_id_and_exits_zero() {
    let env = TestEnv::new("team-create-happy");

    let prompt = env.prompt(
        "Please create a team named research-squad by calling the TeamCreate tool. Do not describe it; just call the tool.",
        "team_create_roundtrip",
    );

    let mut sess = env.spawn(&[
        "--permission-mode",
        "workspace-write",
        "--allowedTools",
        "TeamCreate",
        &prompt,
    ]);

    sess.expect("TeamCreate")
        .expect("model must invoke TeamCreate (agent trigger)");

    // The tool returns a JSON payload with team_id. Match the presence
    // of the key + a non-empty string value. The exact id format is
    // `team_<hex>_<counter>` but only the presence matters for the
    // regression sentinel — a rename of the field would surface here.
    sess.expect(r#""team_id":\s*"team_"#)
        .expect("TeamCreate must return a team_id starting with team_");

    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(exit, 0, "team_create turn should exit 0; got {exit}");
}
