//! Integration tests for `runtime::spawn_task`. Lives outside the
//! lib's `#[cfg(test)] mod` so it compiles as its own test binary
//! and stays decoupled from unrelated lib-test fixtures.

use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use kernel::core::agents::registry::{AgentDescriptor, AgentKind};
use kernel::kernel::{Kernel, OperationContext};
use runtime::spawn_task::spawn_task;

const DT_STREAM: i32 = 4;
const STREAM_CAPACITY: usize = 65_536;
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Add the `/proc` mount to the kernel's VFSRouter. Without this,
/// `sys_read` / `sys_write` against `/proc/*` paths errors at
/// `vfs_router.route()` before consulting the metastore. Mirrors
/// `mount_proc` in the nexus-side managed_agent integration tests.
fn mount_proc(kernel: &Kernel) {
    kernel
        .vfs_router_arc()
        .add_mount("/proc", "root", None, false);
}

fn plant_chat_stream(kernel: &Kernel, pid: &str) {
    let path = format!("/proc/{pid}/chat-with-me");
    kernel
        .sys_setattr(
            &path,
            DT_STREAM,
            /* backend_name */ "",
            /* backend */ None,
            /* metastore */ None,
            /* raft_backend */ None,
            /* io_profile */ "memory",
            /* zone_id */ "root",
            /* is_external */ false,
            STREAM_CAPACITY,
            /* read_fd */ None,
            /* write_fd */ None,
            /* mime_type */ None,
            /* modified_at_ms */ None,
            /* link_target */ None,
            /* source */ None,
            /* remote_metastore */ None,
        )
        .expect("plant /proc/{pid}/chat-with-me DT_STREAM");
}

fn make_desc(pid: &str, name: &str) -> AgentDescriptor {
    AgentDescriptor {
        pid: pid.to_string(),
        name: name.to_string(),
        kind: AgentKind::Managed,
        owner_id: "test-owner".to_string(),
        zone_id: "root".to_string(),
        ..Default::default()
    }
}

fn user_ctx() -> OperationContext {
    OperationContext::new(
        "test-user",
        "root",
        /* is_admin */ false,
        Some("user-test"),
        /* is_system */ true,
    )
}

fn read_envelopes(
    kernel: &Kernel,
    path: &str,
    ctx: &OperationContext,
    from_offset: u64,
) -> (Vec<serde_json::Value>, u64) {
    let mut offset = from_offset;
    let mut out = Vec::new();
    loop {
        match kernel.sys_read(path, ctx, 0, offset) {
            Ok(result) => {
                if let Some(bytes) = result.data.as_ref() {
                    if !bytes.is_empty() {
                        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(bytes) {
                            out.push(v);
                        }
                    }
                }
                let next = result.stream_next_offset.map_or(offset, |o| o as u64);
                if next == offset {
                    break;
                }
                offset = next;
            }
            Err(_) => break,
        }
    }
    (out, offset)
}

fn wait_for_envelope_with_body(
    kernel: &Kernel,
    path: &str,
    ctx: &OperationContext,
    body_eq: &str,
    timeout: Duration,
) -> Option<serde_json::Value> {
    let deadline = Instant::now() + timeout;
    let mut offset = 0u64;
    while Instant::now() < deadline {
        let (envelopes, next) = read_envelopes(kernel, path, ctx, offset);
        offset = next;
        for env in envelopes {
            if env.get("body").and_then(|v| v.as_str()) == Some(body_eq) {
                return Some(env);
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
    None
}

#[test]
fn echo_round_trip_through_proc_pid_chat_with_me() {
    let kernel = Arc::new(Kernel::new());
    mount_proc(&kernel);
    plant_chat_stream(&kernel, "v0-echo-1");
    let desc = make_desc("v0-echo-1", "agent-v0");
    let handle = spawn_task(Arc::clone(&kernel), desc);

    let ctx = user_ctx();
    let cwm_path = "/proc/v0-echo-1/chat-with-me";
    let prompt = serde_json::json!({
        "from": "user-test",
        "to": "agent-v0",
        "body": "hello",
    });
    kernel
        .sys_write(cwm_path, &ctx, &serde_json::to_vec(&prompt).unwrap(), 0)
        .expect("user write to chat-with-me");

    let echo = wait_for_envelope_with_body(
        &kernel,
        cwm_path,
        &ctx,
        "echo: hello",
        Duration::from_secs(2),
    );
    handle.abort_signal.abort();
    let _ = handle.join.join();
    let echo = echo.expect("agent echo response did not arrive within 2s");

    assert_eq!(echo.get("from").and_then(|v| v.as_str()), Some("agent-v0"));
    assert_eq!(echo.get("to").and_then(|v| v.as_str()), Some("user-test"));
}

#[test]
fn loop_exits_on_abort_signal() {
    let kernel = Arc::new(Kernel::new());
    mount_proc(&kernel);
    plant_chat_stream(&kernel, "v0-abort-1");
    let desc = make_desc("v0-abort-1", "agent-abort");

    let handle = spawn_task(Arc::clone(&kernel), desc);
    // No prompt sent — the loop is sitting in the poll sleep.
    // abort() must wake it within one POLL_INTERVAL + a sys_read
    // round trip.
    handle.abort_signal.abort();

    let watcher = thread::Builder::new()
        .spawn(move || handle.join.join())
        .expect("watcher thread");
    let deadline = Instant::now() + Duration::from_secs(2);
    while !watcher.is_finished() {
        if Instant::now() >= deadline {
            panic!("spawn_task thread did not exit within 2s of abort()");
        }
        thread::sleep(Duration::from_millis(20));
    }
    let _ = watcher.join();
}

#[test]
fn skips_own_writes_to_avoid_echo_loop() {
    // The agent's own echo writes carry from=self_agent_id; the
    // loop must filter these out so the mailbox does not
    // exponentially explode. Walks one round trip and asserts the
    // stream contains exactly one agent echo (no echo-of-echo).
    let kernel = Arc::new(Kernel::new());
    mount_proc(&kernel);
    plant_chat_stream(&kernel, "v0-loop-1");
    let desc = make_desc("v0-loop-1", "agent-loop");
    let handle = spawn_task(Arc::clone(&kernel), desc);

    let ctx = user_ctx();
    let cwm_path = "/proc/v0-loop-1/chat-with-me";
    let prompt = serde_json::json!({
        "from": "user-test",
        "to": "agent-loop",
        "body": "ping",
    });
    kernel
        .sys_write(cwm_path, &ctx, &serde_json::to_vec(&prompt).unwrap(), 0)
        .unwrap();

    let _ = wait_for_envelope_with_body(
        &kernel,
        cwm_path,
        &ctx,
        "echo: ping",
        Duration::from_secs(2),
    )
    .expect("first echo did not arrive");
    // Settle for several poll intervals so any bug-induced
    // echo-of-echo would have written by now.
    thread::sleep(POLL_INTERVAL * 4);
    handle.abort_signal.abort();
    let _ = handle.join.join();

    let (envelopes, _) = read_envelopes(&kernel, cwm_path, &ctx, 0);
    let agent_echoes = envelopes
        .iter()
        .filter(|v| v.get("from").and_then(|f| f.as_str()) == Some("agent-loop"))
        .count();
    assert_eq!(
        agent_echoes, 1,
        "expected exactly one agent echo, got {agent_echoes} — \
         loop is echoing its own writes"
    );
}
