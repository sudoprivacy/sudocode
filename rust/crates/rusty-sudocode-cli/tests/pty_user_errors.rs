//! Provider diagnostics must not become terminal recovery advice.
mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::time::Duration;

#[test]
fn provider_auth_failure_shows_recovery_without_raw_diagnostics() {
    let env = common::TestEnv::new("user-errors");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let config = runtime::SAMPLE_SUDOCODE_JSON
        .replace("https://api.anthropic.com", &url)
        .replace("<YOUR_ANTHROPIC_API_KEY>", "test-pty-key");
    std::fs::write(env.config_home().join("sudocode.json"), config).unwrap();
    let stopped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop = stopped.clone();
    let provider = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(45);
        while !stop.load(std::sync::atomic::Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut socket, _)) => {
                    socket.set_nonblocking(false).unwrap();
                    socket
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let mut input = BufReader::new(socket.try_clone().unwrap());
                    let mut length = 0;
                    loop {
                        let mut line = String::new();
                        assert!(input.read_line(&mut line).unwrap() > 0);
                        if line == "\r\n" {
                            break;
                        }
                        if let Some((key, value)) = line.split_once(':') {
                            if key.eq_ignore_ascii_case("content-length") {
                                length = value.trim().parse().unwrap();
                            }
                        }
                    }
                    input.read_exact(&mut vec![0; length]).unwrap();
                    let body = r#"{"type":"error","error":{"type":"authentication_error","message":"PRIVATE_DIAGNOSTIC /internal/auth.rs:42"}}"#;
                    write!(socket, "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "provider was never called"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("provider accept: {error}"),
            }
        }
    });
    let mut session = env.spawn(&["Say hello"]);
    session
        .expect("authorization or configuration")
        .unwrap_or_else(|error| panic!("{error}\n{}", session.render(|screen| screen.contents())));
    session.expect_eof().expect("failed turn exits");
    let screen = session.render(|screen| screen.contents());
    assert!(!screen.contains("PRIVATE_DIAGNOSTIC"), "{screen}");
    assert!(!screen.contains("/internal/auth.rs"), "{screen}");
    stopped.store(true, std::sync::atomic::Ordering::Relaxed);
    provider.join().unwrap();
}
