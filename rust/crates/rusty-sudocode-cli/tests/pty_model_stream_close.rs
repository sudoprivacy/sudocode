//! Reproduce a live gateway dropping the HTTP body before transport EOF.
mod common;

use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use common::TestEnv;
use serde_json::{json, Value};

struct TruncatedProvider {
    url: String,
    stopped: Arc<AtomicBool>,
    requests: Arc<AtomicUsize>,
    worker: Option<thread::JoinHandle<()>>,
}

impl TruncatedProvider {
    fn new(terminal: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let stopped = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(AtomicUsize::new(0));
        let stop = stopped.clone();
        let count = requests.clone();
        let worker = thread::spawn(move || {
            'connections: while !stop.load(Ordering::SeqCst) {
                let Ok((mut socket, _)) = listener.accept() else {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                };
                socket.set_nonblocking(false).unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut input = BufReader::new(socket.try_clone().unwrap());
                let mut first = String::new();
                if input.read_line(&mut first).unwrap_or(0) == 0 {
                    continue;
                }
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    if input.read_line(&mut line).unwrap_or(0) == 0 {
                        continue 'connections;
                    }
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((key, value)) = line.split_once(':') {
                        if key.eq_ignore_ascii_case("content-length") {
                            length = value.trim().parse().unwrap();
                        }
                    }
                }
                let mut body = vec![0; length];
                input.read_exact(&mut body).unwrap();
                if !first.starts_with("POST /v1/messages ") {
                    let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"data\":[]}");
                    continue;
                }
                count.fetch_add(1, Ordering::SeqCst);
                let request: Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(request["stream"], true);
                let body = response_body(terminal);
                // Promise bytes that never arrive: reqwest must raise a body
                // read error, instead of accepting ordinary clean HTTP EOF.
                let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len() + 128);
                let _ = socket.write_all(response.as_bytes());
            }
        });
        Self {
            url,
            stopped,
            requests,
            worker: Some(worker),
        }
    }
}

impl Drop for TruncatedProvider {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.worker.take().unwrap().join().unwrap();
    }
}

fn response_body(terminal: bool) -> String {
    let mut events = vec![
        json!({"type":"message_start","message":{"id":"body-close","type":"message","role":"assistant","model":"sonnet","content":[],"usage":{"input_tokens":12,"output_tokens":0}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"STREAM_BODY_VERIFIED"}}),
    ];
    if terminal {
        events.extend([
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":8}}),
            json!({"type":"message_stop"}),
        ]);
    }
    let mut body = String::new();
    for event in events {
        write!(
            &mut body,
            "event: {}\ndata: {event}\n\n",
            event["type"].as_str().unwrap()
        )
        .unwrap();
    }
    body
}

// render takes a closure over screens with different lifetimes.
#[allow(clippy::redundant_closure_for_method_calls)]
fn check_close(terminal: bool) {
    let env = TestEnv::new("stream-body-close");
    if env.is_live() {
        return;
    } // the peer must truncate at a specified byte
    let provider = TruncatedProvider::new(terminal);
    let path = env.config_home().join("sudocode.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    config["auth_modes"]["api-key"]["anthropic"]["baseUrl"] = json!(provider.url);
    std::fs::write(path, config.to_string()).unwrap();
    let mut cli = env.spawn(&[
        "--permission-mode",
        "read-only",
        "--print",
        "Report the stream result",
    ]);
    cli.set_default_timeout(common::at_least(Duration::from_secs(45)));
    let exit = cli.expect_eof().unwrap();
    let screen = cli.render(|s| s.contents());
    if terminal {
        assert_eq!(exit, 0, "{screen}");
        assert!(screen.contains("STREAM_BODY_VERIFIED"), "{screen}");
        assert_eq!(
            provider.requests.load(Ordering::SeqCst),
            1,
            "a completed response must not be regenerated"
        );
    } else {
        assert_ne!(exit, 0, "a partial body cannot complete a turn: {screen}");
        assert!(
            screen.contains("incomplete") || screen.contains("truncated"),
            "{screen}"
        );
    }
}

#[test]
fn terminal_response_survives_a_broken_http_body() {
    check_close(true);
}

#[test]
fn interrupted_content_is_not_a_successful_response() {
    check_close(false);
}
