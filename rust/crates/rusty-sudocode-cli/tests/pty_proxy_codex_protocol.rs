//! A cold proxy configuration sends Codex to Responses through the real CLI.
mod common;

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

const MARKER: &str = "QA_PROXY_CODEX_RESPONSE";

struct Provider {
    base_url: String,
    requests: mpsc::Receiver<(String, Value)>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Provider {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let running = Arc::clone(&stop);
        let (sender, requests) = mpsc::channel();
        let worker = thread::spawn(move || {
            while !running.load(Ordering::Relaxed) {
                let mut socket = match listener.accept() {
                    Ok((socket, _)) => socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("fixture accept: {error}"),
                };
                socket.set_nonblocking(false).unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                let (header, body) = read_request(&mut socket);
                if header.starts_with("GET ") {
                    respond(&mut socket, "application/json", r#"{"data":[]}"#);
                    continue;
                }
                let is_responses = header.starts_with("POST /v1/responses ");
                sender
                    .send((header, serde_json::from_slice(&body).unwrap()))
                    .unwrap();
                let body = if is_responses {
                    format!(
                        "event: response.created\ndata: {{\"id\":\"fixture\",\"model\":\"gpt-5-codex\"}}\n\n\
                         event: response.content_part.added\ndata: {{\"output_index\":0}}\n\n\
                         event: response.output_text.delta\ndata: {{\"delta\":\"{MARKER}\"}}\n\n\
                         event: response.output_text.done\ndata: {{}}\n\n\
                         event: response.completed\ndata: {{\"response\":{{\"usage\":{{\"input_tokens\":1,\"output_tokens\":1}}}}}}\n\n"
                    )
                } else {
                    format!(
                        "data: {{\"id\":\"fixture\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-5-codex\",\"choices\":[{{\"index\":0,\"delta\":{{\"role\":\"assistant\",\"content\":\"{MARKER}\"}},\"finish_reason\":null}}]}}\n\n\
                         data: {{\"id\":\"fixture\",\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n\
                         data: [DONE]\n\n"
                    )
                };
                respond(&mut socket, "text/event-stream", &body);
            }
        });
        Self {
            base_url,
            requests,
            stop,
            worker: Some(worker),
        }
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}

fn read_request(socket: &mut TcpStream) -> (String, Vec<u8>) {
    let mut bytes = Vec::new();
    let (header, start, length) = loop {
        let mut buffer = [0; 4096];
        let size = socket.read(&mut buffer).unwrap();
        assert_ne!(size, 0, "fixture request ended before its headers");
        bytes.extend_from_slice(&buffer[..size]);
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let header = String::from_utf8(bytes[..end].to_vec()).unwrap();
            let length = header
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length: ")
                        .map(|value| value.parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            break (header, end + 4, length);
        }
    };
    while bytes.len() < start + length {
        let mut buffer = [0; 4096];
        let size = socket.read(&mut buffer).unwrap();
        assert_ne!(size, 0, "fixture request body was truncated");
        bytes.extend_from_slice(&buffer[..size]);
    }
    (header, bytes[start..start + length].to_vec())
}

fn respond(socket: &mut TcpStream, content_type: &str, body: &str) {
    write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
}

fn exercise_proxy_protocol(api_override: Option<&str>, expected_path: &str) {
    let env = common::TestEnv::new("proxy-codex-protocol");
    let provider = Provider::start();
    let mut configuration = json!({
        "auth_modes": {"proxy": {"fixture": {"baseUrl": provider.base_url, "apiKey": "fixture-owner-key"}}},
        "models": {"gpt-5-codex": {"providers": {"proxy": {"provider": "fixture", "model": "gpt-5-codex"}}}}
    });
    if let Some(api) = api_override {
        configuration["models"]["gpt-5-codex"]["providers"]["proxy"]["api"] = json!(api);
    }
    fs::write(
        env.config_home().join("sudocode.json"),
        configuration.to_string(),
    )
    .unwrap();
    fs::write(
        env.config_home().join("settings.json"),
        r#"{"thinking":false}"#,
    )
    .unwrap();
    let home = env.workspace_root().join("home");
    fs::create_dir_all(&home).unwrap();
    let services = env.workspace_root().join("services");
    let mut cli = common::spawn_scode_in_dir_with_env(
        env.workspace_root(),
        &[
            "--auth",
            "proxy",
            "--model",
            "gpt-5-codex",
            "--permission-mode",
            "read-only",
            "-p",
            "Reply exactly QA_PROXY_CODEX_RESPONSE. Use no tools.",
        ],
        Duration::from_secs(90),
        &[
            ("SUDO_CODE_CONFIG_HOME", env.config_home()),
            ("HOME", &home),
            ("USERPROFILE", &home),
            ("SCODE_SERVICE_ROOT", &services),
            ("SCODE_SKIP_CONFIG_MIGRATION", Path::new("1")),
        ],
    )
    .unwrap();
    let exit = cli.expect_eof().unwrap();
    let screen = common::screen_tail(&cli, 12000);
    assert_eq!(exit, 0, "{screen}");
    assert!(screen.contains(MARKER), "{screen}");
    let (header, body) = provider
        .requests
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    assert!(
        header.starts_with(&format!("POST {expected_path} ")),
        "wrong provider endpoint: {}",
        header.lines().next().unwrap()
    );
    assert!(header
        .to_ascii_lowercase()
        .contains("authorization: bearer fixture-owner-key"));
    assert_eq!(body["model"], "gpt-5-codex");
    let field = if expected_path.ends_with("responses") {
        "input"
    } else {
        "messages"
    };
    assert!(
        body[field].is_array(),
        "native request input must reach the provider"
    );
}

#[test]
fn cold_proxy_codex_uses_responses_without_an_api_override() {
    exercise_proxy_protocol(None, "/v1/responses");
}

#[test]
fn explicit_proxy_protocol_takes_precedence_over_the_cold_fallback() {
    exercise_proxy_protocol(Some("openai-completions"), "/v1/chat/completions");
}
