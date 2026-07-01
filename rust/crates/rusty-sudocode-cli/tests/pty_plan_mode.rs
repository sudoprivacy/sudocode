//! PTY tests for the plan-mode tool pair (EnterPlanMode / ExitPlanMode).
//!
//! Coverage target: roadmap §Feature-inventory row
//! "EnterPlanMode / ExitPlanMode" (must-have, P0 core differentiator).
//! Before this file: 0 PTY tests → row marked "Gap". After this file: the
//! full worktree-local state-file lifecycle is exercised in both modes.
//!
//! ## What plan mode actually does (source of truth: tools/src/lib.rs)
//!
//! `EnterPlanMode` writes `permissions.defaultMode = "plan"` into
//! `<cwd>/.nexus/sudocode/settings.local.json` AND writes a state file at
//! `<cwd>/.nexus/sudocode/tool-state/plan-mode.json` capturing whatever
//! was there before the switch. `ExitPlanMode` reads the state file and
//! restores — either wiping the field entirely (no prior override), or
//! writing back the exact previous value.
//!
//! Subtle branches worth exercising:
//!
//! 1. **Fresh workspace** — no settings.local.json, no state file.
//!    Enter must create both; Exit must delete the plan field AND the
//!    state file.
//! 2. **Pre-existing override** — user already has
//!    `permissions.defaultMode = "workspace-write"`. Enter must record
//!    that as `previous_local_mode` in the state file; Exit must restore
//!    to `"workspace-write"` (NOT wipe).
//! 3. **Exit without prior Enter** — state file missing; Exit is a
//!    no-op that returns success (no crash, no state pollution).
//!
//! Structural invariant across both modes: settings.local.json ends the
//! turn in the exact shape the design promises. Disk-level, not
//! response-text, because the response phrasing varies per model.
//!
//! ```bash
//! cargo test --test pty_plan_mode                          # mock (CI)
//! SCODE_TEST_BACKEND=live cargo test --test pty_plan_mode  # real API
//! ```

mod common;

use std::fs;
use std::path::PathBuf;

use common::TestEnv;
use serde_json::Value;

fn settings_local(env: &TestEnv) -> PathBuf {
    env.workspace_root()
        .join(".nexus")
        .join("sudocode")
        .join("settings.local.json")
}

fn plan_mode_state(env: &TestEnv) -> PathBuf {
    env.workspace_root()
        .join(".nexus")
        .join("sudocode")
        .join("tool-state")
        .join("plan-mode.json")
}

fn read_json_or_empty(path: &std::path::Path) -> Value {
    if !path.exists() {
        return Value::Object(Default::default());
    }
    let text = fs::read_to_string(path).unwrap_or_default();
    if text.trim().is_empty() {
        return Value::Object(Default::default());
    }
    serde_json::from_str::<Value>(&text).unwrap_or(Value::Object(Default::default()))
}

fn default_mode_of(settings: &Value) -> Option<&str> {
    settings
        .get("permissions")
        .and_then(|p| p.get("defaultMode"))
        .and_then(|v| v.as_str())
}

fn write_json(path: &std::path::Path, value: &Value) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent");
    }
    fs::write(
        path,
        serde_json::to_string_pretty(value).expect("serialize"),
    )
    .expect("write");
}

// ──────────────────────────────────────────────────────────────────────
// 1. Fresh workspace — Enter writes both files
// ──────────────────────────────────────────────────────────────────────

/// User asks the agent to enable plan mode; EnterPlanMode must write
/// `permissions.defaultMode = "plan"` to settings.local.json AND record
/// the pre-Enter state in `tool-state/plan-mode.json` for later restore.
///
/// Agent trigger: the model must pick EnterPlanMode from the prompt.
#[test]
fn enter_plan_mode_writes_settings_and_state_from_fresh_workspace() {
    let env = TestEnv::new("plan-mode-enter-fresh");

    // Sanity: neither file exists yet.
    assert!(
        !settings_local(&env).exists(),
        "fresh workspace should not have settings.local.json"
    );
    assert!(
        !plan_mode_state(&env).exists(),
        "fresh workspace should not have plan-mode state"
    );

    let prompt = env.prompt(
        "Please enable plan mode by calling the EnterPlanMode tool. Do not describe it; just call the tool.",
        "enter_plan_mode_roundtrip",
    );

    let mut sess = env.spawn(&[
        "--permission-mode",
        "workspace-write",
        "--allowedTools",
        "EnterPlanMode",
        &prompt,
    ]);

    sess.expect("EnterPlanMode")
        .expect("model must invoke EnterPlanMode (agent trigger)");
    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(exit, 0, "enter plan mode turn should exit 0; got {exit}");

    // Disk assertions — the strongest layer, works in both modes.
    let settings = read_json_or_empty(&settings_local(&env));
    assert_eq!(
        default_mode_of(&settings),
        Some("plan"),
        "settings.local.json must have permissions.defaultMode = \"plan\" after EnterPlanMode; got: {settings}"
    );

    assert!(
        plan_mode_state(&env).exists(),
        "tool-state/plan-mode.json must be created so ExitPlanMode can restore"
    );
    let state = read_json_or_empty(&plan_mode_state(&env));
    assert_eq!(
        state.get("had_local_override").and_then(|v| v.as_bool()),
        Some(false),
        "state file must record had_local_override = false when starting from a fresh workspace"
    );
}

// ──────────────────────────────────────────────────────────────────────
// 2. Pre-existing override — Enter records it; Exit restores it
// ──────────────────────────────────────────────────────────────────────

/// The full lifecycle: user had `workspace-write` already, Enter records
/// it, Exit restores it. Regression guard against the two most likely
/// bugs — (a) Exit clears the field instead of restoring, (b) Enter
/// forgets the prior value so Exit can't restore.
///
/// Pre-seed the settings before we spawn — the model would otherwise have
/// to write settings itself, which is out of scope for this test.
#[test]
fn plan_mode_roundtrip_preserves_prior_default_mode() {
    let env = TestEnv::new("plan-mode-roundtrip-preserve");

    // Pre-seed: user already has permissions.defaultMode = "workspace-write".
    let seed = serde_json::json!({
        "permissions": { "defaultMode": "workspace-write" }
    });
    write_json(&settings_local(&env), &seed);

    // Phase A: Enter.
    {
        let prompt_enter = env.prompt(
            "Please enable plan mode by calling the EnterPlanMode tool. Do not describe it; just call the tool.",
            "enter_plan_mode_roundtrip",
        );
        let mut sess = env.spawn(&[
            "--permission-mode",
            "workspace-write",
            "--allowedTools",
            "EnterPlanMode",
            &prompt_enter,
        ]);
        sess.expect("EnterPlanMode")
            .expect("model must invoke EnterPlanMode");
        let exit = sess.expect_eof().expect("scode should exit");
        assert_eq!(exit, 0);
    }

    // After Enter: settings switched to "plan", state file records
    // the previous value.
    let settings_after_enter = read_json_or_empty(&settings_local(&env));
    assert_eq!(
        default_mode_of(&settings_after_enter),
        Some("plan"),
        "after EnterPlanMode, defaultMode must be \"plan\""
    );
    let state = read_json_or_empty(&plan_mode_state(&env));
    assert_eq!(
        state.get("had_local_override").and_then(|v| v.as_bool()),
        Some(true),
        "state must record that a prior override existed"
    );
    assert_eq!(
        state.get("previous_local_mode").and_then(|v| v.as_str()),
        Some("workspace-write"),
        "state must remember the exact previous mode so Exit can restore it"
    );

    // Phase B: Exit.
    {
        let prompt_exit = env.prompt(
            "Please exit plan mode by calling the ExitPlanMode tool. Do not describe it; just call the tool.",
            "exit_plan_mode_roundtrip",
        );
        let mut sess = env.spawn(&[
            "--permission-mode",
            "workspace-write",
            "--allowedTools",
            "ExitPlanMode",
            &prompt_exit,
        ]);
        sess.expect("ExitPlanMode")
            .expect("model must invoke ExitPlanMode");
        let exit = sess.expect_eof().expect("scode should exit");
        assert_eq!(exit, 0);
    }

    // After Exit: settings restored, state file gone.
    let settings_after_exit = read_json_or_empty(&settings_local(&env));
    assert_eq!(
        default_mode_of(&settings_after_exit),
        Some("workspace-write"),
        "after ExitPlanMode, defaultMode must be restored to the pre-Enter value \"workspace-write\" \
         — NOT wiped, NOT left as \"plan\". Got: {settings_after_exit}"
    );
    assert!(
        !plan_mode_state(&env).exists(),
        "tool-state/plan-mode.json must be deleted after ExitPlanMode"
    );
}

// ──────────────────────────────────────────────────────────────────────
// 3. Exit without prior Enter — no-op, no crash
// ──────────────────────────────────────────────────────────────────────

/// Robustness: if a user (or a badly-behaved agent) calls ExitPlanMode
/// without a prior EnterPlanMode, the tool must succeed as a no-op
/// rather than crashing or corrupting settings.local.json.
#[test]
fn exit_plan_mode_without_prior_enter_is_a_noop() {
    let env = TestEnv::new("plan-mode-exit-no-prior");

    // Sanity: nothing exists.
    assert!(!settings_local(&env).exists());
    assert!(!plan_mode_state(&env).exists());

    let prompt = env.prompt(
        "Please exit plan mode by calling the ExitPlanMode tool. Do not describe it; just call the tool.",
        "exit_plan_mode_roundtrip",
    );

    let mut sess = env.spawn(&[
        "--permission-mode",
        "workspace-write",
        "--allowedTools",
        "ExitPlanMode",
        &prompt,
    ]);

    sess.expect("ExitPlanMode")
        .expect("model must invoke ExitPlanMode");
    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(
        exit, 0,
        "no-op exit-plan-mode must NOT crash the CLI; got {exit}"
    );

    // The tool must not create either file when there was nothing to
    // undo. It's fine if settings.local.json exists but has no
    // permissions.defaultMode; it must NOT exist as `"plan"`.
    let settings = read_json_or_empty(&settings_local(&env));
    assert_ne!(
        default_mode_of(&settings),
        Some("plan"),
        "no-op exit must not accidentally set defaultMode = \"plan\""
    );
    assert!(
        !plan_mode_state(&env).exists(),
        "no state file should have been created by a no-op exit"
    );
}
