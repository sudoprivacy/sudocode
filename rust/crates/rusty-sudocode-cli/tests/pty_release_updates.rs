//! Release discovery through the real CLI and an isolated HTTP mirror.
mod common;

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn screen_contents(screen: &pty_expect::Screen<'_>) -> String {
    screen.contents()
}

fn wait_for_repl(child: &mut pty_expect::PtySession) {
    // The update picker also uses the prompt glyph. The engine banner marks
    // the transition from startup choices to a session ready for commands.
    child.expect("Auth mode").unwrap_or_else(|error| {
        panic!("session banner: {error}\n{}", child.render(screen_contents))
    });
    child.expect("❯").unwrap();
}

struct Mirror {
    base: String,
    release: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
    requests: Arc<AtomicUsize>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Mirror {
    fn new(tag: &'static str, is_released: bool) -> Self {
        Self::with_files(tag, is_released, HashMap::new())
    }

    fn with_files(tag: &'static str, is_released: bool, files: HashMap<String, Vec<u8>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}/release/latest", listener.local_addr().unwrap());
        let release = Arc::new(AtomicBool::new(is_released));
        let stopped = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(AtomicUsize::new(0));
        let thread_release = Arc::clone(&release);
        let thread_stop = Arc::clone(&stopped);
        let thread_requests = Arc::clone(&requests);
        let thread = std::thread::spawn(move || {
            while !thread_stop.load(Ordering::SeqCst) {
                let Ok((mut stream, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = String::new();
                if std::io::BufReader::new(&mut stream)
                    .read_line(&mut request)
                    .unwrap()
                    == 0
                {
                    continue;
                }
                let path = request.split_whitespace().nth(1).unwrap();
                thread_requests.fetch_add(1, Ordering::SeqCst);
                while !thread_release.load(Ordering::SeqCst) && !thread_stop.load(Ordering::SeqCst)
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
                let body = if path == "/release/latest/version.txt" {
                    tag.as_bytes()
                } else {
                    files
                        .get(path)
                        .unwrap_or_else(|| panic!("unexpected release URL: {path}"))
                        .as_slice()
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.write_all(body);
            }
        });
        Self {
            base,
            release,
            stopped,
            requests,
            thread: Some(thread),
        }
    }
}

fn payload_archive(bytes: &[u8]) -> (String, Vec<u8>) {
    let os = match std::env::consts::OS {
        "macos" => "macos",
        "windows" => "windows",
        _ => "linux",
    };
    let arch = if std::env::consts::ARCH == "aarch64" {
        "arm64"
    } else {
        "x64"
    };
    let target = format!("scode-{os}-{arch}");
    #[cfg(windows)]
    {
        let mut archive = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        archive
            .start_file(
                format!("{target}/scode.exe"),
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
        archive.write_all(bytes).unwrap();
        (
            format!("{target}.zip"),
            archive.finish().unwrap().into_inner(),
        )
    }
    #[cfg(not(windows))]
    {
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut archive = tar::Builder::new(gz);
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        archive
            .append_data(&mut header, format!("{target}/scode"), bytes)
            .unwrap();
        (
            format!("{target}.tar.gz"),
            archive.into_inner().unwrap().finish().unwrap(),
        )
    }
}

fn spawn_installed(
    env: &common::TestEnv,
    installed: &std::path::Path,
    mirror: &str,
    args: &[&str],
) -> pty_expect::PtySession {
    let record = env.workspace_root().join("restart arguments.txt");
    env.spawn_executable_with_env(
        installed,
        args,
        &[
            ("SCODE_MIRROR", mirror),
            ("SCODE_CHECK_UPDATES", "1"),
            ("SCODE_UPDATE_RECORD", &record.to_string_lossy()),
        ],
    )
}

fn prepare_installed(env: &common::TestEnv, installed: &std::path::Path) {
    std::fs::copy(common::scode_bin(), installed).unwrap();
    // Validate each staged executable once before the interactive workflow.
    // macOS can spend >10s loading a fresh debug copy before main (sampled at
    // _dyld_start), even though --version itself takes milliseconds. Process
    // launch has a separate e2e/startup benchmark; this journey checks updating
    // a working installation, with unchanged prompt/download/restart budgets.
    let version = std::process::Command::new(installed)
        .arg("--version")
        .current_dir(env.workspace_root())
        .env("SUDO_CODE_CONFIG_HOME", env.config_home())
        .env_remove("SCODE_GLOBAL_CONFIG_DIR")
        .env_remove("SCODE_PROJECT_CONFIG_DIR")
        .output()
        .expect("run the staged executable's version command");
    assert!(
        version.status.success() && version.stdout.starts_with(b"scode "),
        "staged executable did not report its version: {version:?}"
    );
}

#[test]
fn selecting_update_installs_the_checked_archive_and_restarts_with_original_arguments() {
    use sha2::{Digest, Sha256};
    let env = common::TestEnv::new("release-install");
    let root = env.workspace_root();
    let exe_name = if cfg!(windows) {
        "installed scode.exe"
    } else {
        "installed scode"
    };
    let installed = root.join(exe_name);
    prepare_installed(&env, &installed);
    let source = root.join("replacement.rs");
    std::fs::write(
        &source,
        r#"fn main() {
        let args = std::env::args().skip(1).collect::<Vec<_>>().join("|");
        let record = std::path::PathBuf::from(std::env::var("SCODE_UPDATE_RECORD").unwrap());
        std::fs::write(&record, args).unwrap();
        std::fs::write(record.with_extension("cwd"), std::env::current_dir().unwrap().to_string_lossy().as_bytes()).unwrap();
        println!("updated scode fixture");
    }"#,
    )
    .unwrap();
    let replacement = root.join(if cfg!(windows) {
        "replacement.exe"
    } else {
        "replacement"
    });
    assert!(std::process::Command::new("rustc")
        .arg(&source)
        .arg("-o")
        .arg(&replacement)
        .status()
        .unwrap()
        .success());
    let payload = std::fs::read(&replacement).unwrap();
    let (name, archive) = payload_archive(&payload);
    let checksum = format!("{:x}  {name}\n", Sha256::digest(&archive));
    let mirror = Mirror::with_files(
        "v999.0.0",
        true,
        HashMap::from([
            (format!("/release/v999.0.0/{name}"), archive),
            (
                "/release/v999.0.0/SHA256SUMS.txt".into(),
                checksum.into_bytes(),
            ),
        ]),
    );
    let cache = env.config_home().join("cache/release.json");
    std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    std::fs::write(&cache, serde_json::to_vec(&serde_json::json!({
        "checked_at": now, "source": mirror.base,
        "release": {"tag": "v999.0.0", "download_base": mirror.base.replace("/latest", "/v999.0.0")}
    })).unwrap()).unwrap();
    let record = root.join("restart arguments.txt");
    let mut child = spawn_installed(&env, &installed, &mirror.base, &["--model", "sonnet"]);
    child.expect("Update available:").unwrap_or_else(|error| {
        panic!(
            "update selection: {error}; requests={}; cache={:?}\n{}",
            mirror.requests.load(Ordering::SeqCst),
            std::fs::read_to_string(&cache),
            child.render(screen_contents)
        )
    });
    child.send("\r").unwrap();
    child.expect("checksum verified").unwrap();
    child
        .expect("updated scode fixture")
        .unwrap_or_else(|error| panic!("replacement: {error}\n{}", child.render(screen_contents)));
    assert_eq!(child.expect_eof().unwrap(), 0);
    assert_eq!(std::fs::read(&installed).unwrap(), payload);
    assert_eq!(std::fs::read_to_string(&record).unwrap(), "--model|sonnet");
    assert_eq!(
        std::path::PathBuf::from(std::fs::read_to_string(record.with_extension("cwd")).unwrap())
            .canonicalize()
            .unwrap(),
        root.canonicalize().unwrap(),
        "the installed CLI and its replacement must run in the fixture workspace"
    );
    assert_eq!(mirror.requests.load(Ordering::SeqCst), 2);
}

#[test]
fn a_bad_checksum_preserves_the_installed_executable() {
    let env = common::TestEnv::new("release-tampered");
    let installed = env
        .workspace_root()
        .join(if cfg!(windows) { "scode.exe" } else { "scode" });
    prepare_installed(&env, &installed);
    let original = std::fs::read(&installed).unwrap();
    let (name, archive) = payload_archive(b"tampered release bytes");
    let mirror = Mirror::with_files(
        "v999.0.0",
        true,
        HashMap::from([
            (format!("/release/v999.0.0/{name}"), archive),
            (
                "/release/v999.0.0/SHA256SUMS.txt".into(),
                format!("{}  {name}\n", "0".repeat(64)).into_bytes(),
            ),
        ]),
    );
    let mut child = spawn_installed(&env, &installed, &mirror.base, &["update", "--yes"]);
    child.expect("checksum mismatch").unwrap();
    assert_ne!(child.expect_eof().unwrap(), 0);
    assert_eq!(std::fs::read(&installed).unwrap(), original);
    assert!(!env.workspace_root().join("restart arguments.txt").exists());
}

impl Drop for Mirror {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let result = thread.join();
            if !std::thread::panicking() {
                result.unwrap();
            }
        }
    }
}

#[test]
fn startup_is_available_before_network_reply_and_next_launch_uses_cache() {
    for mode in ["off", "queue"] {
        let env = common::TestEnv::new("release-startup");
        let mirror = Mirror::new("v999.0.0", false);
        let options = [
            ("SCODE_MIRROR", mirror.base.as_str()),
            ("SCODE_CHECK_UPDATES", "1"),
            ("SUDOCODE_INTERRUPT_QUEUE_MODE", mode),
        ];
        let mut child = env.spawn_with_env(&[], &options);
        child.expect("❯").unwrap_or_else(|error| {
            panic!(
                "terminal startup: {error}\n{}",
                child.render(screen_contents)
            )
        });
        // The server withholds its reply until the real terminal is usable.
        // A synchronous startup check would block this workflow.
        mirror.release.store(true, Ordering::SeqCst);
        let cache = env.config_home().join("cache/release.json");
        let deadline = Instant::now() + common::DEFAULT_TIMEOUT;
        while !cache.exists() {
            assert!(Instant::now() < deadline, "metadata cache was not written");
            std::thread::sleep(Duration::from_millis(20));
        }
        let record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&cache).unwrap()).unwrap();
        assert_eq!(record["release"]["tag"], "v999.0.0");
        assert!(record["release"]["download_base"]
            .as_str()
            .unwrap()
            .ends_with("/release/v999.0.0"));
        child.send("/exit\r").unwrap();
        assert_eq!(child.expect_eof().unwrap(), 0);

        let mut restored = env.spawn_with_env(&[], &options);
        restored.expect("Update available:").unwrap();
        restored.expect("v999.0.0").unwrap();
        restored.send("\x1b[B\r").unwrap();
        wait_for_repl(&mut restored);
        restored.send("/exit\r").unwrap();
        assert_eq!(restored.expect_eof().unwrap(), 0);
        assert_eq!(mirror.requests.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn dismissing_a_release_keeps_sessions_available_without_another_prompt() {
    let env = common::TestEnv::new("release-dismiss");
    let mirror = Mirror::new("v999.0.0", true);
    let options = [
        ("SCODE_MIRROR", mirror.base.as_str()),
        ("SCODE_CHECK_UPDATES", "1"),
    ];
    let cache = env.config_home().join("cache/release.json");
    std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    std::fs::write(&cache, serde_json::to_vec(&serde_json::json!({
        "checked_at": now,
        "source": mirror.base,
        "release": {"tag": "v999.0.0", "download_base": mirror.base.replace("/latest", "/v999.0.0")}
    })).unwrap()).unwrap();
    let mut child = env.spawn_with_env(&[], &options);
    child.expect("Update available:").unwrap();
    child.send("\x1b[B\x1b[B\r").unwrap();
    wait_for_repl(&mut child);
    child.send("/exit\r").unwrap();
    assert_eq!(child.expect_eof().unwrap(), 0);
    let record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&cache).unwrap()).unwrap();
    assert_eq!(record["dismissed_version"], "v999.0.0");
    let mut restored = env.spawn_with_env(&[], &options);
    restored.expect("❯").unwrap();
    assert!(!restored
        .render(screen_contents)
        .contains("Update available:"));
    restored.send("/exit\r").unwrap();
    assert_eq!(restored.expect_eof().unwrap(), 0);
    assert_eq!(mirror.requests.load(Ordering::SeqCst), 0);
}

#[test]
fn opt_out_skips_startup_network_but_explicit_check_works_on_every_platform() {
    let env = common::TestEnv::new("release-check");
    let mirror = Mirror::new("v999.0.0", true);
    let options = [
        ("SCODE_MIRROR", mirror.base.as_str()),
        ("SCODE_CHECK_UPDATES", "0"),
    ];
    let mut child = env.spawn_with_env(&[], &options);
    child.expect("❯").unwrap_or_else(|error| {
        panic!(
            "terminal startup: {error}\n{}",
            child.render(screen_contents)
        )
    });
    child.send("/exit\r").unwrap();
    assert_eq!(child.expect_eof().unwrap(), 0);
    assert_eq!(mirror.requests.load(Ordering::SeqCst), 0);
    let mut check = env.spawn_with_env(&["update", "--check"], &options);
    check.expect("v999.0.0").unwrap_or_else(|error| {
        panic!(
            "explicit check: {error}; requests={}\n{}",
            mirror.requests.load(Ordering::SeqCst),
            check.render(screen_contents)
        )
    });
    check.expect("update available").unwrap();
    assert_eq!(check.expect_eof().unwrap(), 0);
    assert_eq!(mirror.requests.load(Ordering::SeqCst), 1);
}

#[test]
fn invalid_mirror_pointer_fails_and_static_mirror_pins_its_own_version() {
    let env = common::TestEnv::new("release-invalid");
    let mirror = Mirror::new("nexusd-cohost-v999.0.0", true);
    let mut invalid = env.spawn_with_env(
        &["update", "--check"],
        &[("SCODE_MIRROR", mirror.base.as_str())],
    );
    invalid
        .expect("mirror must name a stable vX.Y.Z release")
        .unwrap_or_else(|error| {
            panic!(
                "version check: {error}\n{}",
                invalid.render(screen_contents)
            )
        });
    assert_ne!(invalid.expect_eof().unwrap(), 0);

    let pinned = mirror.base.replace("/latest", "/v999.0.0");
    let mut check =
        env.spawn_with_env(&["update", "--check"], &[("SCODE_MIRROR", pinned.as_str())]);
    check.expect("v999.0.0").unwrap();
    check.expect("update available").unwrap();
    assert_eq!(check.expect_eof().unwrap(), 0);
    assert_eq!(mirror.requests.load(Ordering::SeqCst), 1);
}
