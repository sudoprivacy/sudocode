//! The shipped scode executable hosted by the real managed-agent service.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nexus_vfs_client::NexusVfsClient;
use serde_json::{json, Value};

use super::{cohost_rpc, dial, fresh, Controller};

struct Workspace(PathBuf);

impl Workspace {
    fn new() -> Self {
        let parent = std::env::var_os("SCODE_TEST_TEMP_DIR")
            .map_or_else(std::env::temp_dir, PathBuf::from)
            .canonicalize()
            .unwrap();
        for ancestor in parent.ancestors() {
            for relative in ["AGENTS.md", ".nexus/sudocode/AGENTS.md"] {
                assert!(
                    !ancestor.join(relative).exists(),
                    "managed subprocess workspace would inherit {}; set SCODE_TEST_TEMP_DIR to an instruction-free parent",
                    ancestor.join(relative).display()
                );
            }
        }
        let root = parent.join(format!("scode-managed-process-{}", fresh()));
        std::fs::create_dir(&root).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        for name in ["home", "config", "workspace"] {
            std::fs::create_dir(root.join(name)).unwrap();
        }
        Self(root)
    }

    fn cwd(&self) -> PathBuf {
        self.0.join("workspace")
    }

    fn configure(&self, model: &str) -> HashMap<String, String> {
        let base = std::env::var("NEXUS_A2A_MODEL_URL").expect("provider base URL");
        let key =
            std::env::var("NEXUS_A2A_MODEL_KEY").unwrap_or_else(|_| "managed-test-key".into());
        let config = json!({
            "auth_modes":{"api-key":{"anthropic":{"baseUrl":base,"apiKey":key}}},
            "models":{model:{"alias":model,"name":"managed acceptance","input":["text"],
                "providers":{"api-key":{"provider":"anthropic","model":model}}}}
        });
        std::fs::write(
            self.0.join("config/sudocode.json"),
            serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();
        let mut env = HashMap::new();
        // Raw spawning clears the environment. Supply only the child inputs,
        // never copy unrelated credentials or the developer's Nexus node record.
        for name in [
            "PATH",
            "LANG",
            "LC_ALL",
            "TMPDIR",
            "TMP",
            "TEMP",
            "SystemRoot",
            "WINDIR",
            "COMSPEC",
        ] {
            if let Ok(value) = std::env::var(name) {
                env.insert(name.into(), value);
            }
        }
        env.insert("HOME".into(), self.0.join("home").display().to_string());
        env.insert(
            "USERPROFILE".into(),
            self.0.join("home").display().to_string(),
        );
        env.insert(
            runtime::config::GLOBAL_CONFIG_DIR_ENV.into(),
            self.0.join("config").display().to_string(),
        );
        env.insert(
            "SUDOCODE_DUMP_REQUESTS".into(),
            self.0.join("requests").display().to_string(),
        );
        env
    }

    fn assert_restored_request(&self, previous_answer: &str, current_prompt: &str) {
        // Observe the real provider boundary for both scripted and live models.
        // session/load restores engine history; it does not replay chat updates.
        let restored = std::fs::read_dir(self.0.join("requests"))
            .unwrap()
            .any(|entry| {
                let path = entry.unwrap().path();
                if !path.to_string_lossy().ends_with("-messages.json") {
                    return false;
                }
                let body: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
                let messages = body["messages"].as_array().unwrap();
                let contains = |role: &str, text: &str| {
                    messages.iter().any(|message| {
                        message["role"] == role && message["content"].to_string().contains(text)
                    })
                };
                contains("assistant", previous_answer) && contains("user", current_prompt)
            });
        assert!(
            restored,
            "the resumed provider request omitted the original assistant answer"
        );
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!(
                "managed subprocess evidence retained at {}",
                self.0.display()
            );
        } else {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }
}

#[cfg(unix)]
fn running(pid: i32) -> bool {
    use nix::errno::Errno;
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    match kill(Pid::from_raw(pid), None) {
        Ok(()) | Err(Errno::EPERM) => true,
        Err(Errno::ESRCH) => false,
        Err(error) => panic!("inspect managed OS PID {pid}: {error}"),
    }
}

#[cfg(windows)]
fn running(pid: i32) -> bool {
    let status = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command"])
        .arg(format!(
            "if (Get-Process -Id {pid} -ErrorAction SilentlyContinue) {{ exit 0 }}; exit 3"
        ))
        .status()
        .expect("query the managed Windows process");
    match status.code() {
        Some(0) => true,
        Some(3) => false,
        code => panic!("process query failed for {pid}: {code:?}"),
    }
}

fn start(
    operator: &Arc<NexusVfsClient>,
    binary: &Path,
    workspace: &Workspace,
    env: &HashMap<String, String>,
    agent: &str,
    model: &str,
) -> (Value, i32) {
    let started = cohost_rpc(
        operator,
        "managed_agent.start_session_v1",
        &json!({"agent_id":agent,"model":model,"spawn_spec":{
            "cmd":binary,"args":["--auth","api-key","--model",model,"acp"],
            "env":env,"cwd":workspace.cwd()}}),
    )
    .unwrap();
    let pid = i32::try_from(started["os_pid"].as_u64().expect("real child OS PID")).unwrap();
    assert_ne!(pid, i32::try_from(std::process::id()).unwrap());
    assert!(running(pid), "reported child PID is not running: {started}");
    assert_eq!(
        contracts::agent_pid::decode_agent_pid(started["session_id"].as_str().unwrap()),
        (Some(i64::from(pid)), None),
        "registry identity must name the actual subprocess"
    );
    (started, pid)
}

fn stop(operator: &NexusVfsClient, started: &Value, controller: Controller, pid: i32) {
    let cancelled = cohost_rpc(
        operator,
        "managed_agent.cancel_v1",
        &json!({"session_id":started["session_id"],"mode":"session"}),
    )
    .unwrap();
    assert_eq!(cancelled["cancelled"], true, "{cancelled}");
    controller.wait_closed();
    let deadline = Instant::now() + Duration::from_secs(5);
    while running(pid) {
        assert!(
            Instant::now() < deadline,
            "managed child {pid} was not reaped"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let retired = cohost_rpc(
        operator,
        "managed_agent.get_session_v1",
        &json!({"session_id":started["session_id"]}),
    )
    .expect_err("a reaped managed process must not leave an active registry record");
    assert!(
        retired.contains("unknown session_id")
            && retired.contains(started["session_id"].as_str().unwrap()),
        "unexpected lookup result for the retired process: {retired}"
    );
}

fn prompt(controller: &Controller, natural: &str, scenario: &str) -> String {
    let text = if std::env::var("NEXUS_A2A_MODEL_LIVE").as_deref() == Ok("1") {
        natural.to_string()
    } else {
        format!("{natural} PARITY_SCENARIO:{scenario}")
    };
    let result = controller.request(
        "session/prompt",
        json!({"sessionId":controller.session_id(),"prompt":[{"type":"text","text":text}]}),
    );
    assert!(result.get("error").is_none(), "{result}");
    assert_eq!(result["result"]["stopReason"], "end_turn", "{result}");
    assistant_text(&controller.drain_notifications())
}

fn assistant_text(messages: &[Value]) -> String {
    messages
        .iter()
        .filter(|message| message["params"]["update"]["sessionUpdate"] == "agent_message_chunk")
        .filter_map(|message| message["params"]["update"]["content"]["text"].as_str())
        .collect()
}

#[test]
#[ignore = "requires the authenticated daemon from run-managed-subprocess.sh"]
fn managed_scode_reads_cancels_exits_and_resumes_in_a_new_process() {
    let binary = PathBuf::from(std::env::var("NEXUS_A2A_SCODE_BIN").expect("current scode binary"))
        .canonicalize()
        .unwrap();
    let endpoint = std::env::var("NEXUS_A2A_TEST_ENDPOINT").unwrap();
    let model = std::env::var("NEXUS_A2A_TEST_MODEL").unwrap();
    let operator = dial(&endpoint);
    let workspace = Workspace::new();
    let env = workspace.configure(&model);
    let cwd = workspace.cwd();
    let agent = format!("managed-process-{}", fresh());
    let nonce = format!("MANAGED_FILE_{}", fresh());
    std::fs::write(cwd.join("fixture.txt"), &nonce).unwrap();
    let (first, pid) = start(&operator, &binary, &workspace, &env, &agent, &model);
    let controller = Controller::attach_session(&operator, &first, "", cwd.to_str().unwrap(), None);
    let sid = controller.session_id().to_string();
    assert_ne!(first["session_id"], sid);
    let text = prompt(
        &controller,
        "Read fixture.txt and report its exact contents.",
        "read_file_roundtrip",
    );
    assert!(
        text.contains(&nonce),
        "missing actual file contents: {text}"
    );

    let task = "Use Bash to run exactly: printf 'ready' > cancel-ready; sleep 30. Keep this tool in the foreground (run_in_background=false). Then report completion.";
    let task = if std::env::var("NEXUS_A2A_MODEL_LIVE").as_deref() == Ok("1") {
        task.to_string()
    } else {
        format!("{task} PARITY_SCENARIO:bash_interrupt_long_running")
    };
    let turn = controller.request_async(
        "session/prompt",
        json!({"sessionId":sid,"prompt":[{"type":"text","text":task}]}),
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    while !cwd.join("cancel-ready").exists() {
        assert!(
            Instant::now() < deadline,
            "managed child's actual shell did not start"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let status = controller.request("_sudocode/session/status", json!({"sessionId":sid}));
    assert_eq!(
        status["result"]["foreground"]["state"], "running",
        "{status}"
    );
    controller.notify("session/cancel", json!({"sessionId":sid}));
    let cancelled = turn.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(
        cancelled["result"]["stopReason"], "cancelled",
        "{cancelled}"
    );
    assert!(running(pid), "turn cancellation killed the managed process");
    stop(&operator, &first, controller, pid);

    let (second, new_pid) = start(&operator, &binary, &workspace, &env, &agent, &model);
    assert_ne!(new_pid, pid);
    let controller =
        Controller::attach_session(&operator, &second, "", cwd.to_str().unwrap(), Some(&sid));
    assert_eq!(controller.session_id(), sid);
    let updated = format!("MANAGED_UPDATED_{}", fresh());
    std::fs::write(cwd.join("fixture.txt"), &updated).unwrap();
    let followup = "The file changed. Read fixture.txt again and report its current contents.";
    let text = prompt(&controller, followup, "read_file_roundtrip");
    assert!(
        text.contains(&updated),
        "restored process did not read changed contents: {text}"
    );
    workspace.assert_restored_request(&nonce, followup);
    stop(&operator, &second, controller, new_pid);
    println!(
        "MANAGED SCODE PROCESS OK: old_pid={pid}, new_pid={new_pid}, restored ACP session={sid}"
    );
}
