//! Integration tests for the `<task-notification>` XML renderer that
//! coordinator mode uses to signal agent completion.
//!
//! Two things this file locks in:
//!
//! 1. **SSOT shape** — the emitted XML matches what the coordinator
//!    prompt (`coordinator_system_prompt`) teaches the model to
//!    expect. Any drift here silently breaks the coordinator's
//!    parsing.
//! 2. **Coord-on/off gate** — `render_task_notification_if_enabled`
//!    only emits when `SUDOCODE_COORDINATOR_MODE` is truthy. Non-coord
//!    sessions must keep their legacy JSON manifest shape (backwards
//!    compat with any pre-parity `TaskOutput` consumer).
//!
//! Env-touching tests share a process-wide mutex — parallel writers
//! would race like they do in `coordinator_gate.rs`.

use std::sync::{Mutex, MutexGuard, OnceLock};

use runtime::coordinator_mode::{
    is_coordinator_mode, normalize_task_notification_status, render_task_notification,
    render_task_notification_if_enabled, TaskNotificationView, COORDINATOR_ENV_VAR,
};

fn env_mutex() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

struct EnvGuard(&'static str, Option<String>);
impl EnvGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let prior = std::env::var(key).ok();
        std::env::set_var(key, value);
        Self(key, prior)
    }
    fn clear(key: &'static str) -> Self {
        let prior = std::env::var(key).ok();
        std::env::remove_var(key);
        Self(key, prior)
    }
}
impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.1 {
            Some(v) => std::env::set_var(self.0, v),
            None => std::env::remove_var(self.0),
        }
    }
}

fn base_view<'a>() -> TaskNotificationView<'a> {
    TaskNotificationView {
        agent_id: "agent-a1b",
        status: "completed",
        summary: "Agent \"Investigate auth bug\" completed",
        result: None,
        duration_ms: None,
        tool_uses: None,
        total_tokens: None,
    }
}

// ── SSOT shape ────────────────────────────────────────────────────

#[test]
fn shape_minimal_completed_no_result_no_usage() {
    let xml = render_task_notification(&base_view());

    // Every required tag present in required order. `"` in the summary
    // gets escaped to `&quot;` — see `shape_xml_escapes_special_chars`
    // for the full escape contract.
    let expected = "\
<task-notification>
<task-id>agent-a1b</task-id>
<status>completed</status>
<summary>Agent &quot;Investigate auth bug&quot; completed</summary>
</task-notification>";
    assert_eq!(xml, expected);
}

#[test]
fn shape_with_result_block() {
    let view = TaskNotificationView {
        result: Some("Found null pointer in src/auth/validate.ts:42"),
        ..base_view()
    };
    let xml = render_task_notification(&view);
    assert!(xml.contains("<result>Found null pointer in src/auth/validate.ts:42</result>"));
    // result comes after summary, before optional usage.
    let summary_at = xml.find("</summary>").expect("summary present");
    let result_at = xml.find("<result>").expect("result present");
    assert!(summary_at < result_at, "result must follow summary");
}

#[test]
fn shape_with_usage_all_three_counters() {
    let view = TaskNotificationView {
        result: Some("done"),
        duration_ms: Some(1234),
        tool_uses: Some(7),
        total_tokens: Some(4096),
        ..base_view()
    };
    let xml = render_task_notification(&view);
    // Match the flat one-line <usage> shape teased in the prompt:
    // <usage><total_tokens>N</total_tokens><tool_uses>N</tool_uses><duration_ms>N</duration_ms></usage>
    let expected_usage =
        "<usage><total_tokens>4096</total_tokens><tool_uses>7</tool_uses><duration_ms>1234</duration_ms></usage>";
    assert!(
        xml.contains(expected_usage),
        "expected usage sub-tags in the shape:\n  actual: {xml}"
    );
    // </usage> should sit before </task-notification>.
    let usage_close = xml.find("</usage>").expect("usage close tag");
    let outer_close = xml.find("</task-notification>").expect("outer close tag");
    assert!(usage_close < outer_close);
}

#[test]
fn shape_usage_omits_unset_sub_counters() {
    let view = TaskNotificationView {
        duration_ms: Some(500),
        tool_uses: None,
        total_tokens: None,
        ..base_view()
    };
    let xml = render_task_notification(&view);
    assert!(xml.contains("<usage><duration_ms>500</duration_ms></usage>"));
    assert!(!xml.contains("<tool_uses>"));
    assert!(!xml.contains("<total_tokens>"));
}

#[test]
fn shape_omits_usage_entirely_when_all_counters_missing() {
    let view = TaskNotificationView {
        result: Some("no counters"),
        duration_ms: None,
        tool_uses: None,
        total_tokens: None,
        ..base_view()
    };
    let xml = render_task_notification(&view);
    assert!(
        !xml.contains("<usage>"),
        "empty <usage> tag must be omitted"
    );
}

#[test]
fn shape_xml_escapes_special_chars_in_text_content() {
    let view = TaskNotificationView {
        agent_id: "agent-<x>&y",
        status: "failed",
        summary: "Agent \"weird & wild\" failed",
        result: Some("stdout: 1 < 2 && 3 > 2, quote: \"x\", apos: 'y'"),
        ..base_view()
    };
    let xml = render_task_notification(&view);
    // Raw special chars must be replaced with entity references so a
    // downstream XML parser (or the model's own regex) doesn't choke.
    assert!(xml.contains("agent-&lt;x&gt;&amp;y"), "task-id escapes");
    assert!(
        xml.contains("Agent &quot;weird &amp; wild&quot; failed"),
        "summary escapes"
    );
    assert!(
        xml.contains("1 &lt; 2 &amp;&amp; 3 &gt; 2"),
        "result &lt;/&gt;/&amp; escapes"
    );
    assert!(
        xml.contains("&quot;x&quot;") && xml.contains("&apos;y&apos;"),
        "quote / apostrophe escapes"
    );
    // No raw unescaped ampersand (except as part of an entity ref).
    for line in xml.lines() {
        for token in line
            .split("&amp;")
            .flat_map(|s| s.split("&lt;"))
            .flat_map(|s| s.split("&gt;"))
            .flat_map(|s| s.split("&quot;"))
            .flat_map(|s| s.split("&apos;"))
        {
            assert!(
                !token.contains('&'),
                "unescaped ampersand slipped through in token `{token}` (line: `{line}`)"
            );
        }
    }
}

// ── status normalization ─────────────────────────────────────────

#[test]
fn normalize_completed_synonyms() {
    for s in [
        "completed",
        "COMPLETED",
        "success",
        "succeeded",
        "done",
        "finished",
    ] {
        assert_eq!(
            normalize_task_notification_status(s),
            "completed",
            "{s} should normalize to completed"
        );
    }
}

#[test]
fn normalize_killed_synonyms() {
    for s in ["killed", "stopped", "aborted", "cancelled", "canceled"] {
        assert_eq!(
            normalize_task_notification_status(s),
            "killed",
            "{s} should normalize to killed"
        );
    }
}

#[test]
fn normalize_running_and_unknown_default_to_failed() {
    // Deliberate: unexpected/mid-flight statuses surface as failed
    // rather than sneaking a bogus <status> value into the XML.
    for s in [
        "failed", "error", "errored", "running", "working", "", "  ", "xyz",
    ] {
        assert_eq!(
            normalize_task_notification_status(s),
            "failed",
            "{s} should normalize to failed"
        );
    }
}

// ── coord-on/off toggle behavior ─────────────────────────────────

#[test]
fn gate_returns_none_when_coord_mode_off() {
    let _guard = env_mutex();
    let _clear = EnvGuard::clear(COORDINATOR_ENV_VAR);
    assert!(!is_coordinator_mode(), "sanity: env off");

    let out = render_task_notification_if_enabled(&base_view());
    assert!(out.is_none(), "gate must return None when coord mode off");
}

#[test]
fn gate_returns_some_xml_when_coord_mode_on() {
    let _guard = env_mutex();
    let _env = EnvGuard::set(COORDINATOR_ENV_VAR, "1");
    assert!(is_coordinator_mode(), "sanity: env on");

    let out = render_task_notification_if_enabled(&base_view())
        .expect("gate must return Some when coord mode on");
    assert!(out.starts_with("<task-notification>"));
    assert!(out.ends_with("</task-notification>"));
    assert!(out.contains("<task-id>agent-a1b</task-id>"));
}

#[test]
fn gate_returns_none_for_common_off_env_values() {
    let _guard = env_mutex();
    for value in ["0", "false", "off", "no", ""] {
        let _env = EnvGuard::set(COORDINATOR_ENV_VAR, value);
        assert!(
            !is_coordinator_mode(),
            "value `{value}` must NOT enable coord mode"
        );
        assert!(
            render_task_notification_if_enabled(&base_view()).is_none(),
            "value `{value}` must gate off task-notification rendering"
        );
    }
}
