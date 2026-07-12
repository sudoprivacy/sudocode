//! Behavioral PTY (ConPTY) tests for `scode cron` — real user journeys a
//! human runs at a terminal: schedule a task, see it listed + persisted,
//! toggle it, fire it and watch the agent actually run, expire a one-shot.
//! 3+ causal steps per test.
//!
//! Two layers:
//!   * CRUD/persistence — no model, so they run everywhere incl. CI.
//!   * Firing (`run`/`tick`) — a real autonomous agent turn; gated to
//!     `SCODE_TEST_BACKEND=live` and driven against the real
//!     `~/.nexus/sudocode` proxy config. This is the "run it live with a
//!     real key before you commit" coverage; CI's mock run keeps the CRUD
//!     surface honest.
//!
//! ```bash
//! cargo test -p rusty-sudocode-cli --test pty_cron                          # CRUD (CI-safe)
//! SCODE_TEST_BACKEND=live cargo test -p rusty-sudocode-cli --test pty_cron  # + real firing
//! ```
//!
//! Each test gets an ISOLATED config home (temp dir) so it never touches
//! the user's real crons and tests can't cross-talk. Serial by design
//! (`--test-threads=1`) because the spawn helper sets process env.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use pty_expect::PtySession;

static COUNTER: AtomicU64 = AtomicU64::new(0);
const TIMEOUT: Duration = Duration::from_secs(45);

/// The spawn helper sets process-global env (SUDO_CODE_CONFIG_HOME, cwd),
/// so tests must not overlap. Each `CronEnv` holds this lock for its whole
/// lifetime, serialising tests even under a parallel `cargo test`.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn is_live() -> bool {
    std::env::var("SCODE_TEST_BACKEND").as_deref() == Ok("live")
}

fn unique_dir(label: &str) -> PathBuf {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_millis();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "scode-cron-{label}-{}-{millis}-{n}",
        std::process::id()
    ))
}

/// Real `~/.nexus/sudocode` on this machine (for live auth seeding).
fn real_config_home() -> PathBuf {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .expect("no HOME/USERPROFILE");
    home.join(".nexus").join("sudocode")
}

struct CronEnv {
    config_home: PathBuf,
    home: PathBuf,
    workspace: PathBuf,
    _guard: std::sync::MutexGuard<'static, ()>,
}

impl CronEnv {
    fn new(label: &str) -> Self {
        let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = unique_dir(label);
        let config_home = root.join("config-home");
        let home = root.join("home");
        let workspace = root.join("workspace");
        for d in [&config_home, &home, &workspace] {
            fs::create_dir_all(d).expect("mkdir");
        }
        if is_live() {
            // Seed auth from the real config so a fired cron reaches the API.
            let src = real_config_home();
            for name in ["sudocode.json", "scode.json"] {
                let s = src.join(name);
                if s.exists() {
                    let _ = fs::copy(&s, config_home.join(name));
                }
            }
            assert!(
                config_home.join("sudocode.json").exists(),
                "live mode needs {}/sudocode.json",
                src.display()
            );
        }
        Self {
            config_home,
            home,
            workspace,
            _guard: guard,
        }
    }

    /// Spawn `scode [--auth --model] cron <args...>` under a PTY with this
    /// env's isolated config home. Env is set on the process (tests run
    /// serially), and the binary is spawned directly — no `sh` needed.
    fn cron(&self, args: &[&str]) -> PtySession {
        let scode = env!("CARGO_BIN_EXE_scode");
        std::env::set_var("SUDO_CODE_CONFIG_HOME", &self.config_home);
        std::env::set_var("HOME", &self.home);
        std::env::set_var("NO_COLOR", "1");
        let _ = std::env::set_current_dir(&self.workspace);

        let mut full: Vec<String> = Vec::new();
        // model/auth flags only matter when a cron fires; harmless for CRUD.
        if is_live() {
            full.extend(["--auth", "proxy", "--model", "auto"].map(String::from));
        }
        full.push("cron".to_string());
        full.extend(args.iter().map(|s| (*s).to_string()));
        let refs: Vec<&str> = full.iter().map(String::as_str).collect();

        let mut sess = PtySession::spawn(scode, &refs).expect("spawn scode cron");
        sess.set_default_timeout(TIMEOUT);
        sess
    }

    fn crons_json(&self) -> String {
        fs::read_to_string(self.config_home.join("crons.json")).unwrap_or_default()
    }

    /// Run a one-shot agent prompt (`scode "<text>"`) with tools enabled,
    /// optionally with the host-owns-scheduling gate set — to prove the agent
    /// can (or cannot) reach the cron tools.
    fn prompt_run(&self, text: &str, disable_cron_tools: bool) -> PtySession {
        let scode = env!("CARGO_BIN_EXE_scode");
        std::env::set_var("SUDO_CODE_CONFIG_HOME", &self.config_home);
        std::env::set_var("HOME", &self.home);
        std::env::set_var("NO_COLOR", "1");
        if disable_cron_tools {
            std::env::set_var("SUDOCODE_DISABLE_CRON_TOOLS", "1");
        } else {
            std::env::remove_var("SUDOCODE_DISABLE_CRON_TOOLS");
        }
        let _ = std::env::set_current_dir(&self.workspace);
        let args = [
            "--auth",
            "proxy",
            "--model",
            "auto",
            "--permission-mode",
            "danger-full-access",
            text,
        ];
        let mut sess = PtySession::spawn(scode, &args).expect("spawn scode prompt");
        sess.set_default_timeout(TIMEOUT);
        sess
    }

    /// Spawn the interactive REPL (`scode` with no subcommand) under a PTY
    /// with this env's isolated config home — for driving the `/cron`
    /// slash command.
    fn repl(&self) -> PtySession {
        let scode = env!("CARGO_BIN_EXE_scode");
        std::env::set_var("SUDO_CODE_CONFIG_HOME", &self.config_home);
        std::env::set_var("HOME", &self.home);
        std::env::set_var("NO_COLOR", "1");
        let _ = std::env::set_current_dir(&self.workspace);
        let args = ["--auth", "proxy", "--model", "auto"];
        let mut sess = PtySession::spawn(scode, &args).expect("spawn scode repl");
        sess.set_default_timeout(TIMEOUT);
        sess
    }
}

/// Pull the single cron's id straight from the persisted store.
fn only_id(env: &CronEnv) -> String {
    let json = env.crons_json();
    let key = "\"cron_id\": \"";
    let start = json.find(key).expect("cron_id in store") + key.len();
    let end = start + json[start..].find('"').expect("id end");
    json[start..end].to_owned()
}

// ── CRUD + persistence (no model — runs in CI too) ─────────────────────

#[test]
fn add_then_list_and_persist() {
    let env = CronEnv::new("add-list");
    let mut s = env.cron(&[
        "add",
        "--schedule",
        "0 9 * * *",
        "--prompt",
        "daily standup",
        "--name",
        "standup",
    ]);
    s.expect("created").unwrap();
    s.expect("standup").unwrap();
    drop(s);
    // a FRESH process lists it → persisted across invocations.
    let mut s = env.cron(&["list"]);
    s.expect("daily standup").unwrap();
    drop(s);
    let json = env.crons_json();
    assert!(
        json.contains("\"schedule\": \"0 9 * * *\""),
        "crons.json: {json}"
    );
    assert!(json.contains("\"kind\": \"cron\""));
    assert!(json.contains("\"next_run_at\":"), "next-run seeded: {json}");
}

#[test]
fn add_every_and_at_kinds() {
    let env = CronEnv::new("kinds");
    env.cron(&["add", "--every", "3600", "--prompt", "hourly"])
        .expect("created")
        .unwrap();
    env.cron(&["add", "--at", "4102444800", "--prompt", "y2100"])
        .expect("created")
        .unwrap();
    let json = env.crons_json();
    assert!(json.contains("\"kind\": \"every\""), "{json}");
    assert!(json.contains("\"kind\": \"at\""), "{json}");
}

#[test]
fn invalid_schedule_rejected() {
    let env = CronEnv::new("invalid");
    // 5 fields but an out-of-range minute → the cron parser rejects it.
    let mut s = env.cron(&["add", "--schedule", "99 * * * *", "--prompt", "x"]);
    s.expect("invalid cron expression").unwrap();
    drop(s);
    assert!(env.crons_json().is_empty(), "bad schedule must not persist");
}

#[test]
fn disable_then_enable() {
    let env = CronEnv::new("toggle");
    env.cron(&["add", "--every", "60", "--prompt", "p", "--name", "tgl"])
        .expect("created")
        .unwrap();
    let id = only_id(&env);
    env.cron(&["disable", &id]).expect("disabled").unwrap();
    assert!(env.crons_json().contains("\"enabled\": false"));
    env.cron(&["enable", &id]).expect("enabled").unwrap();
    assert!(env.crons_json().contains("\"enabled\": true"));
}

#[test]
fn remove_deletes_entry() {
    let env = CronEnv::new("remove");
    env.cron(&["add", "--every", "60", "--prompt", "p"])
        .expect("created")
        .unwrap();
    let id = only_id(&env);
    env.cron(&["remove", &id]).expect("removed").unwrap();
    env.cron(&["list"]).expect("No scheduled tasks").unwrap();
}

// ── Firing — real autonomous agent turn (live only) ────────────────────

#[test]
fn run_now_fires_and_records_ok() {
    if !is_live() {
        eprintln!("skipping run_now_fires_and_records_ok (set SCODE_TEST_BACKEND=live)");
        return;
    }
    let env = CronEnv::new("run");
    // huge interval so it never auto-fires; we fire explicitly with `run`.
    env.cron(&[
        "add",
        "--every",
        "999999",
        "--prompt",
        "Reply with the single word ACK",
        "--name",
        "fire",
    ])
    .expect("created")
    .unwrap();
    let id = only_id(&env);
    let mut s = env.cron(&["run", &id]);
    s.expect(&format!("run {id}: ok")).unwrap(); // the agent turn completed
    drop(s);
    let json = env.crons_json();
    assert!(json.contains("\"run_count\": 1"), "{json}");
    assert!(json.contains("\"last_status\": \"ok\""), "{json}");
    // A recurring cron RE-ARMS after firing (stays enabled) — this is the
    // distinction from a one-shot `at`, which self-disables.
    assert!(
        json.contains("\"enabled\": true"),
        "recurring cron stays enabled after firing: {json}"
    );
}

#[test]
fn disabled_cron_does_not_fire_on_tick() {
    let env = CronEnv::new("disabled-notick");
    // Would be due right now (past `at`) — but disabled, so tick must skip it.
    let past = (SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        - 10)
        .to_string();
    env.cron(&[
        "add",
        "--at",
        &past,
        "--prompt",
        "should NOT run",
        "--name",
        "off",
    ])
    .expect("created")
    .unwrap();
    let id = only_id(&env);
    env.cron(&["disable", &id]).expect("disabled").unwrap();

    // tick with nothing due (disabled) prints nothing and exits — give it a
    // beat to run, then verify no fire happened. (No agent turn, so this is
    // fast and runs in mock/CI too.)
    let s = env.cron(&["tick"]);
    std::thread::sleep(Duration::from_millis(800));
    drop(s);
    let json = env.crons_json();
    assert!(
        json.contains("\"run_count\": 0"),
        "disabled cron must not fire: {json}"
    );
    assert!(
        !json.contains("\"last_status\": \"ok\""),
        "no successful fire: {json}"
    );
    assert!(
        json.contains("\"enabled\": false"),
        "stays disabled: {json}"
    );
}

#[test]
fn repl_cron_slash_add_and_list() {
    // The `/cron` slash command needs the REPL to boot (real config), so
    // it is live-gated. Quoted multi-word values exercise the slash
    // tokenizer end-to-end through the REPL → run_slash path.
    if !is_live() {
        eprintln!("skipping repl_cron_slash_add_and_list (set SCODE_TEST_BACKEND=live)");
        return;
    }
    let env = CronEnv::new("repl-slash");
    let mut s = env.repl();
    s.expect("❯").expect("REPL prompt");

    s.send("/cron add --every 3600 --prompt \"repl scheduled task\" --name viaslash\r")
        .expect("send /cron add");
    s.expect("created").expect("cron created via /cron");
    s.expect("❯").expect("prompt after add");
    s.send("/exit\r").expect("send /exit");
    drop(s);
    // The store proves the full slash path end-to-end: parse+dispatch reached
    // run_slash, the quote-aware tokenizer kept the multi-word prompt intact,
    // FlagMap parsed --name/--every, and it wrote through to crons.json.
    let json = env.crons_json();
    assert!(
        json.contains("repl scheduled task"),
        "quoted prompt tokenized + persisted: {json}"
    );
    assert!(
        json.contains("\"name\": \"viaslash\""),
        "name persisted: {json}"
    );
    assert!(json.contains("\"kind\": \"every\""), "kind parsed: {json}");
}

/// A host that owns scheduling (sudowork) sets SUDOCODE_DISABLE_CRON_TOOLS so
/// the agent can't "schedule" a task that would persist but never fire — an
/// orphan. Contrast proves the gate is load-bearing: same prompt, tool absent
/// vs present.
#[test]
fn cron_tools_gated_when_host_owns_scheduling() {
    if !is_live() {
        eprintln!("skipping cron_tools_gated_when_host_owns_scheduling (set SCODE_TEST_BACKEND=live)");
        return;
    }
    let env = CronEnv::new("tool-gate");
    let ask = "Use the CronCreate tool now to schedule a task with schedule \"0 9 * * *\" and prompt \"daily report\". Do it, then stop.";

    // 1. Gated (what an embedding host sets): the tool isn't advertised, so the
    //    agent cannot create a cron — nothing is persisted.
    let mut s = env.prompt_run(ask, true);
    let _ = s.expect_eof();
    drop(s);
    std::env::remove_var("SUDOCODE_DISABLE_CRON_TOOLS");
    let gated = env.crons_json();
    assert!(
        !gated.contains("daily report"),
        "gated host: agent must not be able to schedule an orphan cron: {gated}"
    );

    // 2. Ungated (standalone scode): the tool IS available, so the agent schedules it.
    let mut s = env.prompt_run(ask, false);
    let _ = s.expect_eof();
    drop(s);
    let ungated = env.crons_json();
    assert!(
        ungated.contains("daily report"),
        "standalone: agent should schedule via CronCreate: {ungated}"
    );
}

#[test]
fn one_shot_at_past_fires_on_tick_then_self_disables() {
    if !is_live() {
        eprintln!("skipping one_shot_at_past_fires_on_tick_then_self_disables (set SCODE_TEST_BACKEND=live)");
        return;
    }
    let env = CronEnv::new("oneshot");
    let past = (SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        - 10)
        .to_string();
    env.cron(&[
        "add",
        "--at",
        &past,
        "--prompt",
        "Reply with the single word DONE",
        "--name",
        "once",
    ])
    .expect("created")
    .unwrap();
    let mut s = env.cron(&["tick"]);
    s.expect("fired").unwrap();
    drop(s);
    let json = env.crons_json();
    assert!(
        json.contains("\"enabled\": false"),
        "one-shot self-disables: {json}"
    );
    assert!(json.contains("\"run_count\": 1"), "{json}");
}
