//! Real scode -> Bash -> embedded sudohand CLI -> Chrome -> local page.
//! Only model replies are scripted; browser actions and DOM assertions are real.
mod common;

use common::{scode_bin, spawn_scode_in_dir_with_env, HarnessWorkspace};
use serde_json::{json, Value};
use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread;
use std::time::Duration;

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

struct Fixture {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    stopped: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Fixture {
    fn start(port: u16, shot: String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let page = format!("{url}/form");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let stopped = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&stopped);
        let worker = thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((socket, _)) => serve(socket, &captured, port, &page, &shot),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(e) => panic!("accept: {e}"),
                }
            }
        });
        Self {
            url,
            requests,
            stopped,
            worker: Some(worker),
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn serve(mut socket: TcpStream, captured: &Mutex<Vec<Value>>, port: u16, page: &str, shot: &str) {
    socket.set_nonblocking(false).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut reader = BufReader::new(socket.try_clone().unwrap());
    let mut first = String::new();
    if reader.read_line(&mut first).unwrap_or(0) == 0 {
        return;
    }
    let mut len = 0;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        if line == "\r\n" {
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            if key.eq_ignore_ascii_case("content-length") {
                len = value.trim().parse().unwrap();
            }
        }
    }
    let mut bytes = vec![0; len];
    reader.read_exact(&mut bytes).unwrap();
    let (kind, body) = if first.starts_with("GET /form") {
        ("text/html", r#"<!doctype html><title>Browser E2E</title><label>Name <input id="name" aria-label="Name"></label><button onclick="document.getElementById('result').textContent='Saved '+document.getElementById('name').value">Save</button><h1 id="result">Pending</h1>"#.into())
    } else if first.contains("count_tokens") {
        ("application/json", json!({"input_tokens":1000}).to_string())
    } else if first.starts_with("POST ") {
        let request: Value = serde_json::from_slice(&bytes).unwrap();
        let mut requests = captured.lock().unwrap();
        let step = requests.len();
        let block = next_step(step, &request, port, page, shot);
        requests.push(request);
        ("text/event-stream", stream(&block))
    } else {
        ("application/json", "{}".into())
    };
    let _ = write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
}

fn last_output(request: &Value) -> String {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .flat_map(|m| m["content"].as_array().unwrap().iter().rev())
        .find(|b| b["type"] == "tool_result")
        .unwrap()["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|b| b["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn element_ref(request: &Value, name: &str) -> String {
    let text = last_output(request);
    let result: Value = serde_json::from_str(&text).expect("Bash result JSON");
    let elements: Value = serde_json::from_str(result["stdout"].as_str().expect("Bash stdout"))
        .unwrap_or_else(|error| panic!("snapshot JSON: {error}; result: {text}"));
    elements
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == name)
        .unwrap()["ref"]
        .as_str()
        .unwrap()
        .into()
}

fn next_step(step: usize, request: &Value, port: u16, page: &str, shot: &str) -> Value {
    let bin = quote(&scode_bin().to_string_lossy().replace('\\', "/"));
    let base = format!("{bin} browser");
    let command = match step {
        0 => {
            let binary = scode_bin().to_string_lossy().replace('\\', "/");
            assert!(
                request["system"].as_array().unwrap().iter().any(|block| {
                    block["text"]
                        .as_str()
                        .is_some_and(|text| text.contains(&binary))
                }),
                "agent must receive this CLI's path, not a potentially older PATH entry"
            );
            // GitHub's Chrome-for-Testing binary has no installed AppArmor
            // sandbox profile on Ubuntu. This is only the isolated test browser.
            let sandbox = if cfg!(target_os = "linux") && std::env::var_os("CI").is_some() {
                " --override-default-args '{\"--no-sandbox\":\"\"}'"
            } else {
                ""
            };
            format!(
                "{base} browser_start --port {port} --headless --silent-stderr --url {}{sandbox}",
                quote(page)
            )
        }
        1 | 3 => format!("{base} page_discover --port {port} --no-include-coordinates"),
        2 => format!(
            "{base} type_by_ref --port {port} --ref {} --text 'Ada 浏览器' --clear",
            quote(&element_ref(request, "Name"))
        ),
        4 => format!(
            "{base} click_by_ref --port {port} --ref {} --no-human-like",
            quote(&element_ref(request, "Save"))
        ),
        5 => format!(
            "{base} page_discover --port {port} --no-interactable-only --no-include-coordinates"
        ),
        6 => {
            assert!(
                last_output(request).contains("Saved Ada 浏览器"),
                "real DOM must reflect typing and clicking"
            );
            format!(
                "{base} js_evaluate --port {port} --expression {} && {base} page_screenshot --port {port} --path {}",
                quote("({error: 'page data'})"),
                quote(shot)
            )
        }
        7 => {
            assert!(last_output(request).contains("page data"));
            format!("{base} browser_stop --port {port}")
        }
        8 => return json!({"type":"text","text":"BROWSER_E2E_DONE"}),
        _ => panic!("unexpected extra model request"),
    };
    json!({"type":"tool_use","id":format!("browser-{step}"),"name":"Bash","input":{"command":command,"timeout":60000}})
}

fn stream(block: &Value) -> String {
    let tool = block["type"] == "tool_use";
    let mut start = block.clone();
    let delta = if tool {
        start["input"] = json!({});
        json!({"type":"input_json_delta","partial_json":block["input"].to_string()})
    } else {
        start["text"] = json!("");
        json!({"type":"text_delta","text":block["text"]})
    };
    [
        ("message_start", json!({"type":"message_start","message":{"id":"browser-reply","type":"message","role":"assistant","model":"claude-sonnet-4-6","content":[],"usage":{"input_tokens":1000,"output_tokens":0}}})),
        ("content_block_start", json!({"type":"content_block_start","index":0,"content_block":start})),
        ("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":delta})),
        ("content_block_stop", json!({"type":"content_block_stop","index":0})),
        ("message_delta", json!({"type":"message_delta","delta":{"stop_reason":if tool {"tool_use"} else {"end_turn"}},"usage":{"input_tokens":1000,"output_tokens":10}})),
        ("message_stop", json!({"type":"message_stop"})),
    ].into_iter().fold(String::new(), |mut output, (event, data)| {
        write!(output, "event: {event}\ndata: {data}\n\n").unwrap();
        output
    })
}

struct BrowserCleanup(u16);
impl Drop for BrowserCleanup {
    fn drop(&mut self) {
        let _ = std::process::Command::new(scode_bin())
            .args(["browser", "browser_stop", "--port", &self.0.to_string()])
            .output();
    }
}

#[test]
fn agent_controls_real_browser_through_bash_cli() {
    assert!(
        sudohand_browser::chrome::find_chrome().is_some(),
        "install Chrome or set AI_DEV_BROWSER_CHROME for browser PTY tests"
    );
    let workspace = HarnessWorkspace::new("browser-cli-agent");
    let port = sudohand_browser::port::get_available_port((19350, 19450), &[]).unwrap();
    let _cleanup = BrowserCleanup(port);
    let shot = workspace.root.join("browser.png");
    let provider = Fixture::start(port, shot.to_string_lossy().into_owned());
    workspace.write_mock_config(&provider.url);
    let mut cli = spawn_scode_in_dir_with_env(&workspace.root,
        &["--auth","api-key","--model","sonnet","--permission-mode","danger-full-access","Use the browser CLI to fill Name, save, verify and screenshot the local form. Close your browser afterward."],
        Duration::from_secs(120), &[("HOME",&workspace.home),("SUDO_CODE_CONFIG_HOME",&workspace.config_home)]).unwrap();
    cli.expect("BROWSER_E2E_DONE").unwrap();
    assert_eq!(cli.expect_eof().unwrap(), 0);
    assert_eq!(provider.requests.lock().unwrap().len(), 9);
    assert!(std::fs::read(shot)
        .unwrap()
        .starts_with(b"\x89PNG\r\n\x1a\n"));
    assert!(
        TcpStream::connect(("127.0.0.1", port)).is_err(),
        "browser must be stopped"
    );
}

#[test]
fn browser_help_and_invalid_arguments_need_no_model_credentials() {
    let workspace = HarnessWorkspace::new("browser-cli-no-auth");
    let mut cli = spawn_scode_in_dir_with_env(
        &workspace.root,
        &["browser", "--help"],
        Duration::from_secs(15),
        &[
            ("HOME", &workspace.home),
            ("SUDO_CODE_CONFIG_HOME", &workspace.config_home),
        ],
    )
    .unwrap();
    cli.expect("page_discover").unwrap();
    assert_eq!(cli.expect_eof().unwrap(), 0);
    let output = std::process::Command::new(scode_bin())
        .args(["browser", "click_by_ref"])
        .env("HOME", &workspace.home)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["error"]["kind"], "invalid_input");
    let output = std::process::Command::new(scode_bin())
        .args(["browser", "browser_stop"])
        .env("HOME", &workspace.home)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(9));
    assert!(output.stdout.is_empty());
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert!(error["error"]["message"].as_str().unwrap().contains("port"));
}
