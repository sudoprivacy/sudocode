//! `scode cron` — manage scheduled tasks.
//!
//! CRUD + lifecycle over the persistent [`CronRegistry`]
//! (`<config_home>/crons.json`). Firing (`run`/`tick`/`daemon`) is added
//! in the scheduler step and reuses the one-shot prompt run path.
//!
//! Usage:
//! ```text
//! scode cron add --schedule "0 9 * * *" --prompt "daily standup" [--name n] [--tz Asia/Shanghai] [--cwd DIR]
//! scode cron add --every 3600 --prompt "hourly health check"
//! scode cron add --at 1767225600 --prompt "one-shot reminder"
//! scode cron list [--output-format json]
//! scode cron remove <cron_id>
//! scode cron enable <cron_id>
//! scode cron disable <cron_id>
//! ```

use std::error::Error;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use runtime::cron_registry::{CronCreateParams, CronEntry, CronKind, CronRegistry};
use runtime::cron_schedule;
use runtime::PermissionMode;

use super::args::{resolve_repl_model, CliOutputFormat};
use crate::{LiveCli, DEFAULT_MODEL};

/// Seconds between ticks in `scode cron daemon`.
const DAEMON_TICK_SECS: u64 = 60;

type CronResult = Result<(), Box<dyn Error>>;

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Open the persistent registry at `<config_home>/crons.json`.
pub(crate) fn open_registry() -> CronRegistry {
    CronRegistry::open(runtime::default_config_home().join("crons.json"))
}

pub(crate) fn run(args: &[String], output_format: CliOutputFormat) -> CronResult {
    let (sub, rest) = args
        .split_first()
        .map_or(("list", &[][..]), |(s, r)| (s.as_str(), r));
    let reg = open_registry();
    match sub {
        "list" | "ls" => list(&reg, output_format),
        "add" | "create" => add(&reg, rest, output_format),
        "remove" | "rm" | "delete" => remove(&reg, rest, output_format),
        "enable" => set_enabled(&reg, rest, true, output_format),
        "disable" => set_enabled(&reg, rest, false, output_format),
        "run" => run_now(&reg, rest),
        "tick" => {
            tick(&reg);
            Ok(())
        }
        "daemon" => daemon(&reg),
        other => Err(format!(
            "unknown cron subcommand {other:?}; expected: add | list | remove | enable | disable | run | tick | daemon"
        )
        .into()),
    }
}

/// REPL `/cron ...` — CRUD management, returns text for the REPL to show.
/// Firing (`run`/`tick`/`daemon`) is directed to the shell: a live REPL
/// session already owns a runtime, so firing an agent turn from inside it is
/// confusing; `scode cron run/tick/daemon` is the surface for that.
pub(crate) fn run_slash(args: Option<&str>) -> Result<String, String> {
    let tokens = tokenize(args.unwrap_or(""));
    let (sub, rest) = tokens
        .split_first()
        .map_or(("list", &[][..]), |(s, r)| (s.as_str(), r));
    let reg = open_registry();
    match sub {
        "" | "list" | "ls" => Ok(list_text(&reg)),
        "add" | "create" => add_text(&reg, rest),
        "remove" | "rm" | "delete" => {
            let id = slash_id(rest)?;
            reg.delete(id)?;
            Ok(format!("removed cron {id}"))
        }
        "enable" => {
            let id = slash_id(rest)?;
            reg.set_enabled(id, true)?;
            if let Some(e) = reg.get(id) {
                if let Some(n) = cron_schedule::first_run_at(&e, now_secs()) {
                    let _ = reg.set_next_run(id, Some(n));
                }
            }
            Ok(format!("enabled cron {id}"))
        }
        "disable" => {
            let id = slash_id(rest)?;
            reg.set_enabled(id, false)?;
            Ok(format!("disabled cron {id}"))
        }
        "run" | "tick" | "daemon" => Ok(format!(
            "run `scode cron {sub}` from the shell — firing from inside a REPL session isn't supported"
        )),
        other => Err(format!(
            "unknown /cron subcommand {other:?}; try: list | add | remove | enable | disable"
        )),
    }
}

fn list_text(reg: &CronRegistry) -> String {
    let entries = reg.list(false);
    if entries.is_empty() {
        return "No scheduled tasks. Add one: /cron add --schedule \"0 9 * * *\" --prompt \"…\""
            .to_owned();
    }
    entries
        .iter()
        .map(format_entry_line)
        .collect::<Vec<_>>()
        .join("\n")
}

fn add_text(reg: &CronRegistry, rest: &[String]) -> Result<String, String> {
    let opts = FlagMap::parse(rest);
    let prompt = opts
        .get("prompt")
        .ok_or("`/cron add` requires --prompt <text>")?
        .to_owned();
    let (kind, schedule) = match (opts.get("schedule"), opts.get("every"), opts.get("at")) {
        (Some(s), None, None) => (CronKind::Cron, s.to_owned()),
        (None, Some(s), None) => (CronKind::Every, s.to_owned()),
        (None, None, Some(s)) => (CronKind::At, s.to_owned()),
        (None, None, None) => {
            return Err("`/cron add` requires one of --schedule | --every | --at".to_owned())
        }
        _ => {
            return Err("`/cron add` accepts exactly one of --schedule | --every | --at".to_owned())
        }
    };
    let tz = opts.get("tz").map(str::to_owned);
    cron_schedule::validate(kind, &schedule, tz.as_deref())?;
    let entry = reg.create_full(CronCreateParams {
        schedule,
        kind,
        prompt,
        description: opts.get("description").map(str::to_owned),
        name: opts.get("name").map(str::to_owned),
        tz,
        cwd: opts.get("cwd").map(str::to_owned),
    });
    if let Some(n) = cron_schedule::first_run_at(&entry, now_secs()) {
        let _ = reg.set_next_run(&entry.cron_id, Some(n));
    }
    let entry = reg.get(&entry.cron_id).unwrap_or(entry);
    Ok(format!("created: {}", format_entry_line(&entry)))
}

fn slash_id(rest: &[String]) -> Result<&str, String> {
    rest.iter()
        .find(|a| !a.starts_with('-'))
        .map(String::as_str)
        .ok_or_else(|| "expected a <cron_id>".to_owned())
}

/// Quote-aware tokenizer so `/cron add --schedule "0 9 * * *" --prompt "do X"`
/// keeps multi-word quoted values intact.
fn tokenize(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut has = false;
    for c in s.chars() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                } else {
                    cur.push(c);
                }
            }
            None => {
                if c == '\'' || c == '"' {
                    quote = Some(c);
                    has = true;
                } else if c.is_whitespace() {
                    if has {
                        out.push(std::mem::take(&mut cur));
                        has = false;
                    }
                } else {
                    cur.push(c);
                    has = true;
                }
            }
        }
    }
    if has {
        out.push(cur);
    }
    out
}

fn list(reg: &CronRegistry, output_format: CliOutputFormat) -> CronResult {
    let entries = reg.list(false);
    match output_format {
        CliOutputFormat::Json => {
            let doc = serde_json::json!({ "crons": entries, "count": entries.len() });
            println!("{}", serde_json::to_string_pretty(&doc)?);
        }
        CliOutputFormat::Text => {
            if entries.is_empty() {
                println!("No scheduled tasks. Add one with: scode cron add --schedule \"0 9 * * *\" --prompt \"…\"");
                return Ok(());
            }
            for e in &entries {
                println!("{}", format_entry_line(e));
            }
        }
    }
    Ok(())
}

fn add(reg: &CronRegistry, rest: &[String], output_format: CliOutputFormat) -> CronResult {
    let opts = FlagMap::parse(rest);
    let prompt = opts
        .get("prompt")
        .ok_or("`cron add` requires --prompt <text>")?
        .to_owned();

    // Exactly one of --schedule (cron) / --every <secs> / --at <ts>.
    let (kind, schedule) = match (opts.get("schedule"), opts.get("every"), opts.get("at")) {
        (Some(s), None, None) => (CronKind::Cron, s.to_owned()),
        (None, Some(s), None) => (CronKind::Every, s.to_owned()),
        (None, None, Some(s)) => (CronKind::At, s.to_owned()),
        (None, None, None) => {
            return Err(
                "`cron add` requires one of --schedule <cron> | --every <secs> | --at <unix_ts>"
                    .into(),
            )
        }
        _ => return Err("`cron add` accepts exactly one of --schedule | --every | --at".into()),
    };
    let tz = opts.get("tz").map(str::to_owned);
    cron_schedule::validate(kind, &schedule, tz.as_deref())?;

    let entry = reg.create_full(CronCreateParams {
        schedule,
        kind,
        prompt,
        description: opts.get("description").map(str::to_owned),
        name: opts.get("name").map(str::to_owned),
        tz,
        cwd: opts.get("cwd").map(str::to_owned),
    });
    // Seed the first next-run so `list` shows it immediately and the
    // scheduler considers it without a create/tick race.
    if let Some(next) = cron_schedule::first_run_at(&entry, now_secs()) {
        let _ = reg.set_next_run(&entry.cron_id, Some(next));
    }
    let entry = reg.get(&entry.cron_id).unwrap_or(entry);
    emit_entry("created", &entry, output_format)
}

fn remove(reg: &CronRegistry, rest: &[String], output_format: CliOutputFormat) -> CronResult {
    let id = require_id(rest, "remove")?;
    let removed = reg.delete(id)?;
    emit_entry("removed", &removed, output_format)
}

fn set_enabled(
    reg: &CronRegistry,
    rest: &[String],
    enabled: bool,
    output_format: CliOutputFormat,
) -> CronResult {
    let id = require_id(rest, if enabled { "enable" } else { "disable" })?;
    reg.set_enabled(id, enabled)?;
    // Re-seed next-run when (re-)enabling so it fires on schedule again.
    if enabled {
        if let Some(entry) = reg.get(id) {
            if let Some(next) = cron_schedule::first_run_at(&entry, now_secs()) {
                let _ = reg.set_next_run(id, Some(next));
            }
        }
    }
    let entry = reg.get(id).ok_or_else(|| format!("cron not found: {id}"))?;
    emit_entry(
        if enabled { "enabled" } else { "disabled" },
        &entry,
        output_format,
    )
}

/// `scode cron run <id>` — fire one entry immediately (regardless of its
/// schedule), record the outcome, advance next-run.
fn run_now(reg: &CronRegistry, rest: &[String]) -> CronResult {
    let id = require_id(rest, "run")?;
    let entry = reg.get(id).ok_or_else(|| format!("cron not found: {id}"))?;
    let status = fire_and_record(reg, &entry);
    println!("run {id}: {status}");
    Ok(())
}

/// `scode cron tick` — fire every entry that is due right now (the host /
/// OS-cron entrypoint). Also the unit the daemon loop calls.
fn tick(reg: &CronRegistry) {
    let now = now_secs();
    let due: Vec<CronEntry> = reg
        .list(false)
        .into_iter()
        .filter(|e| cron_schedule::is_due(e, now))
        .collect();
    if due.is_empty() {
        return;
    }
    for entry in &due {
        let status = fire_and_record(reg, entry);
        println!("[cron] fired {}: {status}", entry.cron_id);
    }
}

/// `scode cron daemon` — tick on a fixed interval forever. sudocode stays
/// daemonless-by-default; this is the opt-in "set and forget" wrapper (the
/// host may instead drive `scode cron tick` on its own heartbeat).
fn daemon(reg: &CronRegistry) -> CronResult {
    println!("[cron] daemon started (tick every {DAEMON_TICK_SECS}s); Ctrl-C to stop");
    loop {
        // Re-open each tick so externally-added/removed crons (another
        // process editing crons.json) are picked up without a restart.
        let fresh = open_registry();
        tick(&fresh);
        // Keep the caller's handle warm too (tests / single-shot callers).
        let _ = reg.len();
        std::thread::sleep(Duration::from_secs(DAEMON_TICK_SECS));
    }
}

/// Fire an entry's prompt as one autonomous (yolo) agent turn — the SAME
/// path as `scode prompt`, in-process — then record the outcome and the
/// recomputed next-run. One-shot `at` entries self-disable after firing.
/// Returns the status string (`ok` / `error: …`).
fn fire_and_record(reg: &CronRegistry, entry: &CronEntry) -> String {
    let outcome = fire_entry(entry);
    let now = now_secs();
    let next = if cron_schedule::is_one_shot(entry) {
        None
    } else {
        cron_schedule::next_run_after(entry, now)
    };
    let status = match &outcome {
        Ok(()) => {
            let _ = reg.record_result(&entry.cron_id, "ok", None, next);
            "ok".to_owned()
        }
        Err(msg) => {
            let _ = reg.record_result(&entry.cron_id, "error", Some(msg), next);
            format!("error: {msg}")
        }
    };
    if cron_schedule::is_one_shot(entry) {
        let _ = reg.disable(&entry.cron_id);
    }
    status
}

/// Build a fresh yolo `LiveCli` in the entry's cwd and run its prompt as
/// one turn — identical machinery to `scode prompt`, so cron reuses the
/// one agent-run primitive rather than duplicating it.
fn fire_entry(entry: &CronEntry) -> Result<(), String> {
    let prev_cwd = std::env::current_dir().ok();
    if let Some(cwd) = &entry.cwd {
        std::env::set_current_dir(cwd).map_err(|e| format!("cwd {cwd:?}: {e}"))?;
    }
    let model = resolve_repl_model(DEFAULT_MODEL.to_owned());
    let result = (|| -> Result<(), Box<dyn Error>> {
        let mut cli = LiveCli::new(model, true, None, PermissionMode::DangerFullAccess, None)?;
        cli.run_turn_with_output(&entry.prompt, CliOutputFormat::Text, false)
    })();
    if let Some(prev) = prev_cwd {
        let _ = std::env::set_current_dir(prev);
    }
    result.map_err(|e| e.to_string())
}

fn require_id<'a>(rest: &'a [String], op: &str) -> Result<&'a str, Box<dyn Error>> {
    rest.iter()
        .find(|a| !a.starts_with('-'))
        .map(String::as_str)
        .ok_or_else(|| format!("`cron {op}` requires a <cron_id>").into())
}

fn emit_entry(action: &str, entry: &CronEntry, output_format: CliOutputFormat) -> CronResult {
    match output_format {
        CliOutputFormat::Json => {
            let doc = serde_json::json!({ "action": action, "cron": entry });
            println!("{}", serde_json::to_string_pretty(&doc)?);
        }
        CliOutputFormat::Text => println!("{action}: {}", format_entry_line(entry)),
    }
    Ok(())
}

fn format_entry_line(e: &CronEntry) -> String {
    let name = e.name.as_deref().unwrap_or("-");
    let kind = match e.kind {
        CronKind::Cron => "cron",
        CronKind::Every => "every",
        CronKind::At => "at",
    };
    let state = if e.enabled { "enabled" } else { "disabled" };
    let next = e
        .next_run_at
        .map_or_else(|| "-".to_owned(), |n| n.to_string());
    let status = e.last_status.as_deref().unwrap_or("-");
    format!(
        "{id}  [{state}] {kind}({schedule}) next={next} runs={runs} last={status}  name={name}  prompt={prompt:?}",
        id = e.cron_id,
        schedule = e.schedule,
        runs = e.run_count,
        prompt = truncate(&e.prompt, 60),
    )
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}…")
    }
}

/// Minimal `--key value` / `--key=value` / bare-flag parser for the cron
/// subcommands. Values that don't follow a `--key` are ignored here (the
/// positional `<cron_id>` is pulled separately by [`require_id`]).
struct FlagMap {
    map: std::collections::HashMap<String, String>,
}

impl FlagMap {
    fn parse(args: &[String]) -> Self {
        let mut map = std::collections::HashMap::new();
        let mut i = 0;
        while i < args.len() {
            let arg = &args[i];
            if let Some(key) = arg.strip_prefix("--") {
                if let Some((k, v)) = key.split_once('=') {
                    map.insert(k.to_owned(), v.to_owned());
                    i += 1;
                } else if i + 1 < args.len() && !args[i + 1].starts_with("--") {
                    map.insert(key.to_owned(), args[i + 1].clone());
                    i += 2;
                } else {
                    map.insert(key.to_owned(), String::new());
                    i += 1;
                }
            } else {
                i += 1;
            }
        }
        Self { map }
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.map
            .get(key)
            .map(String::as_str)
            .filter(|s| !s.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::tokenize;

    #[test]
    fn tokenize_keeps_quoted_multiword_values() {
        // The exact `/cron add` shape a user types in the REPL.
        let t = tokenize("add --schedule \"0 9 * * *\" --prompt 'daily standup' --name s");
        assert_eq!(
            t,
            vec![
                "add",
                "--schedule",
                "0 9 * * *",
                "--prompt",
                "daily standup",
                "--name",
                "s"
            ]
        );
    }

    #[test]
    fn tokenize_plain_and_empty() {
        assert_eq!(tokenize("list"), vec!["list"]);
        assert_eq!(tokenize("   "), Vec::<String>::new());
        assert!(tokenize("").is_empty());
        // an explicitly empty quoted value is preserved as an empty arg.
        assert_eq!(tokenize("--prompt \"\""), vec!["--prompt", ""]);
    }
}
