//! The same real ACP binary over a PTY for hosted input and interaction tests.
#![allow(dead_code)]
use crate::common::{self, TestEnv};
use pty_expect::PtySession;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;
const METHOD: &str = "_sudocode/session/input";

pub(crate) struct Host {
    pub(crate) terminal: PtySession,
    pub(crate) session: String,
    id: u64,
    /// Raw bytes consumed with the latest reply, including preceding events.
    pub(crate) last_read: String,
    trace: PathBuf,
    responses: std::collections::HashMap<u64, Value>,
}

impl Host {
    pub(crate) fn new(env: &TestEnv) -> Self {
        Self::with_permissions(env, true)
    }

    pub(crate) fn with_permissions(env: &TestEnv, allow_tools: bool) -> Self {
        Self::with_options(env, allow_tools, &[])
    }

    pub(crate) fn with_options(
        env: &TestEnv,
        allow_tools: bool,
        extra_env: &[(&str, &str)],
    ) -> Self {
        let args: &[&str] = if allow_tools {
            &["--dangerously-skip-permissions", "acp"]
        } else {
            &["--permission-mode", "workspace-write", "acp"]
        };
        let trace = extra_env
            .iter()
            .find_map(|(key, value)| (*key == "SCODE_LOG_PATH").then(|| PathBuf::from(value)))
            .or_else(|| std::env::var_os("SCODE_LOG_PATH").map(PathBuf::from))
            .unwrap_or_else(|| env.workspace_root().join("acp-requests.jsonl"));
        let trace_text = trace.to_string_lossy().into_owned();
        let mut variables = extra_env.to_vec();
        if !variables.iter().any(|(key, _)| *key == "SCODE_LOG_PATH") {
            variables.push(("SCODE_LOG_PATH", trace_text.as_ref()));
        }
        let mut terminal = env.spawn_with_env(args, &variables);
        terminal.resize(40, 4096).unwrap();
        terminal.set_default_timeout(common::LIVE_TURN_BUDGET);
        let mut host = Self {
            terminal,
            session: String::new(),
            id: 0,
            last_read: String::new(),
            trace,
            responses: std::collections::HashMap::new(),
        };
        let init = host.request("initialize", &json!({"protocolVersion":1}));
        assert_eq!(init["_meta"]["sudocode"]["inputDelivery"]["version"], 1);
        assert_eq!(init["_meta"]["sudocode"]["runtimeStatus"]["version"], 1);
        let created = host.request(
            "session/new",
            &json!({"cwd":env.workspace_root(),"mcpServers":[]}),
        );
        created["sessionId"]
            .as_str()
            .unwrap()
            .clone_into(&mut host.session);
        assert_eq!(host.runtime_status()["foreground"], json!({"state":"idle"}));
        host
    }

    pub(crate) fn request(&mut self, method: &str, params: &Value) -> Value {
        let response = self.response(method, params);
        assert!(response.get("error").is_none(), "{response}");
        response["result"].clone()
    }

    pub(crate) fn response(&mut self, method: &str, params: &Value) -> Value {
        let id = self.send_request(method, params);
        self.receive(id)
    }

    pub(crate) fn send_request(&mut self, method: &str, params: &Value) -> u64 {
        self.id += 1;
        let newline = if cfg!(windows) { "\r" } else { "\n" };
        self.terminal
            .send(&format!(
                "{}{newline}",
                json!({"jsonrpc":"2.0","id":self.id,"method":method,"params":params})
            ))
            .unwrap();
        self.id
    }

    pub(crate) fn receive(&mut self, id: u64) -> Value {
        loop {
            if let Some(response) = self.responses.remove(&id) {
                return response;
            }
            let response = self.receive_next();
            if response["id"] == id {
                return response;
            }
            self.responses
                .insert(response["id"].as_u64().unwrap(), response);
        }
    }

    pub(crate) fn receive_next(&mut self) -> Value {
        let response = self
            .terminal
            .expect(r#"\{"jsonrpc":"2.0","(?:result|error)":[^\r\n]*,"id":[0-9]+\}"#)
            .unwrap_or_else(|error| {
                panic!(
                    "ACP reply: {error}; {}",
                    self.terminal.render(common::screen_contents)
                )
            });
        self.last_read.clone_from(&response);
        let response: Value = response
            .lines()
            .rev()
            .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
            .find(|value| {
                value["id"].is_u64()
                    && (value.get("result").is_some() || value.get("error").is_some())
            })
            .unwrap_or_else(|| panic!("response line missing: {response}"));
        response
    }

    pub(crate) fn input(&mut self, mut action: Value) -> Value {
        action["sessionId"] = json!(self.session);
        self.request(METHOD, &action)
    }

    pub(crate) fn incoming_request(&mut self, method: &str) -> Value {
        let message = self
            .terminal
            .expect(&format!(
                r#"\{{"jsonrpc":"2.0","method":"{method}",[^\r\n]*\}}"#
            ))
            .unwrap();
        message
            .lines()
            .rev()
            .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
            .find(|value| value["method"] == method)
            .unwrap_or_else(|| panic!("missing {method} request: {message}"))
    }

    pub(crate) fn respond(&mut self, request: &Value, result: &Value) {
        let newline = if cfg!(windows) { "\r" } else { "\n" };
        self.terminal
            .send(&format!(
                "{}{newline}",
                json!({"jsonrpc":"2.0","id":request["id"],"result":result})
            ))
            .unwrap();
    }

    pub(crate) fn submit(&mut self, client: &str, text: &str, delivery: &str) -> Value {
        self.input(json!({"action":"submit","clientId":client,"delivery":delivery,"content":[{"type":"text","text":text}]}))
    }

    pub(crate) fn snapshot(&mut self) -> Value {
        self.input(json!({"action":"snapshot"}))["state"].clone()
    }

    pub(crate) fn runtime_status(&mut self) -> Value {
        self.request(
            "_sudocode/session/status",
            &json!({"sessionId":self.session}),
        )
    }

    pub(crate) fn idle(&mut self) -> Value {
        let deadline = std::time::Instant::now() + common::LIVE_TURN_BUDGET;
        loop {
            let state = self.snapshot();
            if state["running"] == false {
                return state;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "session did not stop: {state}"
            );
            std::thread::sleep(Duration::from_millis(30));
        }
    }

    pub(crate) fn close(&mut self) {
        self.request("session/close", &json!({"sessionId":self.session}));
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            return;
        }
        // The fixture deletes its workspace after this guard drops. Retain
        // transport facts on failure without printing credentials or prompts.
        let trace = std::fs::read_to_string(&self.trace).unwrap_or_default();
        let mut events: Vec<_> = trace
            .lines()
            .rev()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|event| {
                event["event"]
                    .as_str()
                    .is_some_and(|name| name.starts_with("request_") || name.contains("retry"))
            })
            .take(24)
            .map(|event| {
                let attributes = &event["attributes"];
                json!({
                    "event": event["event"],
                    "timestamp": event["timestamp"],
                    "status": attributes["status"],
                    "attempt": attributes["attempt"],
                    "model": attributes["body"]["model"],
                })
            })
            .collect();
        events.reverse();
        eprintln!("ACP failure transport evidence: {}", json!(events));
    }
}
