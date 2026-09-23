//! Exercise Bocha through the CLI, model tool loop, HTTP request, and PTY output.
mod common;

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;
use std::time::{Duration, Instant};

use common::TestEnv;
use serde_json::{json, Value};

fn search_server(status: u16, body: Value) -> (String, thread::JoinHandle<Value>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1/web-search", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let handle = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(25);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "no Bocha request received");
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("accept failed: {error}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut request = Vec::new();
        let (header_end, length) = loop {
            let mut chunk = [0; 4096];
            let n = stream.read(&mut chunk).unwrap();
            assert!(n > 0, "request ended before body");
            request.extend_from_slice(&chunk[..n]);
            if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&request[..end]).to_lowercase();
                assert!(headers.starts_with("post /v1/web-search "));
                assert!(headers.contains("authorization: bearer bocha-fixture-key"));
                let length: usize = headers
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix("content-length:")
                            .map(|v| v.trim().parse().unwrap())
                    })
                    .unwrap();
                if request.len() >= end + 4 + length {
                    break (end + 4, length);
                }
            }
        };
        let request: Value =
            serde_json::from_slice(&request[header_end..header_end + length]).unwrap();
        let body = body.to_string();
        write!(stream, "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        request
    });
    (url, handle)
}

fn configure(env: &TestEnv, url: &str) {
    let path = env.config_home().join("sudocode.json");
    let mut config: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    config["web_search"] = json!({"provider":"bocha", "apiUrl":url, "apiKey":"bocha-fixture-key"});
    std::fs::write(path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();
}

#[test]
fn bocha_search_roundtrip() {
    let env = TestEnv::new("bocha-search");
    let server = if env.is_mock() {
        let (url, server) = search_server(
            200,
            json!({"code":200,"data":{"webPages":{"value":[
                {"name":"Rust","url":"https://rust-lang.org/","summary":"bocha-long-summary","snippet":"unused-short-snippet"},
                {"name":"Duplicate","url":"https://rust-lang.org/","summary":"duplicate-must-disappear"},
                {"name":"Book","url":"https://doc.rust-lang.org/book/","summary":" ","snippet":"bocha-fallback-snippet"},
                {"name":"Blocked","url":"https://blocked.rust-lang.org/","summary":"blocked-must-disappear"},
                {"name":"Outside","url":"https://example.com/","summary":"outside-must-disappear"},
                {"name":"No URL","url":"","summary":"empty-url-must-disappear"}
            ]}}}),
        );
        configure(&env, &url);
        Some(server)
    } else {
        let Ok(api_key) = std::env::var("BOCHA_API_KEY") else {
            eprintln!("skipping bocha_search_roundtrip: BOCHA_API_KEY is not set");
            return;
        };
        let path = env.config_home().join("sudocode.json");
        let mut config: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        config["web_search"] = json!({
            "provider":"bocha",
            "apiUrl":"https://api.bocha.cn/v1/web-search",
            "apiKey":api_key
        });
        std::fs::write(path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();
        None
    };
    let prompt = env.prompt(
        "Use WebSearch to search for the Rust official website. Include result URLs in your answer.",
        "web_search_roundtrip",
    );
    let mut session = env.spawn_with_env(
        &[
            "--allowedTools",
            "WebSearch",
            "--permission-mode",
            "read-only",
            &prompt,
        ],
        &[
            ("BOCHA_API_KEY", ""),
            ("SUDOCODE_BOCHA_API_URL", ""),
            ("SUDOCODE_WEB_SEARCH_PROVIDER", "bocha"),
        ],
    );
    session.expect("WebSearch").unwrap_or_else(|error| {
        panic!("{error}: {}", common::screen_tail(&session, 6000));
    });
    if env.is_mock() {
        session.expect("search roundtrip error=false").unwrap();
        session.expect("bocha-long-summary").unwrap();
        session.expect("bocha-fallback-snippet").unwrap();
    } else {
        session.expect("rust-lang").unwrap();
    }
    assert_eq!(session.expect_eof().unwrap(), 0);
    if let Some(server) = server {
        let request = server.join().unwrap();
        assert_eq!(request["query"], "Rust official website");
        assert_eq!(request["summary"], true);
        assert_eq!(request["count"], 8);
        assert_eq!(request["include"], "rust-lang.org");
        assert_eq!(request["exclude"], "blocked.rust-lang.org");
        let screen = session.render(|screen| screen.contents());
        for unwanted in ["must-disappear", "unused-short-snippet"] {
            assert!(
                !screen.contains(unwanted),
                "unexpected search result: {screen}"
            );
        }
        assert_eq!(env.captured_message_count(), 2);
    }
}

#[test]
fn bocha_errors_are_returned_to_model() {
    for (status, body, expected) in [
        (
            403,
            json!({"message":"secret-body-must-not-be-echoed"}),
            "HTTP 403",
        ),
        (200, json!({"code":403}), "unsuccessful or missing code"),
        (200, json!({"code":200}), "missing search data"),
        (
            200,
            json!({"code":200,"data":{"webPages":{}}}),
            "missing webPages.value",
        ),
    ] {
        let env = TestEnv::new("bocha-error");
        if !env.is_mock() {
            return;
        }
        let (url, server) = search_server(status, body);
        configure(&env, &url);
        let prompt = env.prompt("Search the web", "web_search_roundtrip");
        let mut session = env.spawn_with_env(
            &[
                "--allowedTools",
                "WebSearch",
                "--permission-mode",
                "read-only",
                &prompt,
            ],
            &[
                ("BOCHA_API_KEY", ""),
                ("SUDOCODE_BOCHA_API_URL", ""),
                ("SUDOCODE_WEB_SEARCH_PROVIDER", "bocha"),
            ],
        );
        session
            .expect("search roundtrip error=true")
            .unwrap_or_else(|error| {
                panic!("{error}: {}", common::screen_tail(&session, 6000));
            });
        session.expect(expected).unwrap();
        assert_eq!(session.expect_eof().unwrap(), 0);
        server.join().unwrap();
        assert!(!session
            .render(|screen| screen.contents())
            .contains("secret-body"));
    }
}

#[test]
fn bocha_does_not_reuse_tavily_credentials() {
    let env = TestEnv::new("bocha-missing-key");
    if !env.is_mock() {
        return;
    }
    let path = env.config_home().join("sudocode.json");
    let mut config: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    config["web_search"] = json!({"provider":"tavily", "apiKey":"tavily-secret-must-stay-local"});
    std::fs::write(path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();
    let prompt = env.prompt("Search the web", "web_search_roundtrip");
    let mut session = env.spawn_with_env(
        &[
            "--allowedTools",
            "WebSearch",
            "--permission-mode",
            "read-only",
            &prompt,
        ],
        &[
            ("SUDOCODE_WEB_SEARCH_PROVIDER", "bocha"),
            ("BOCHA_API_KEY", ""),
        ],
    );
    session.expect("search roundtrip error=true").unwrap();
    session
        .expect("Bocha search requires BOCHA_API_KEY")
        .unwrap();
    assert_eq!(session.expect_eof().unwrap(), 0);
    assert!(!session
        .render(|screen| screen.contents())
        .contains("tavily-secret"));
}
