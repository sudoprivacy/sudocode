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
use std::time::{SystemTime, UNIX_EPOCH};

use runtime::cron_registry::{CronCreateParams, CronEntry, CronKind, CronRegistry};
use runtime::cron_schedule;

use super::args::CliOutputFormat;

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
    let (sub, rest) = args.split_first().map_or(("list", &[][..]), |(s, r)| (s.as_str(), r));
    let reg = open_registry();
    match sub {
        "list" | "ls" => list(&reg, output_format),
        "add" | "create" => add(&reg, rest, output_format),
        "remove" | "rm" | "delete" => remove(&reg, rest, output_format),
        "enable" => set_enabled(&reg, rest, true, output_format),
        "disable" => set_enabled(&reg, rest, false, output_format),
        "run" | "tick" | "daemon" => {
            Err("`cron run/tick/daemon` (firing) is added in the scheduler step".into())
        }
        other => Err(format!(
            "unknown cron subcommand {other:?}; expected: add | list | remove | enable | disable | run | tick | daemon"
        )
        .into()),
    }
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
            return Err("`cron add` requires one of --schedule <cron> | --every <secs> | --at <unix_ts>".into())
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
    if let Some(next) = cron_schedule::next_run_after(&entry, now_secs()) {
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
            if let Some(next) = cron_schedule::next_run_after(&entry, now_secs()) {
                let _ = reg.set_next_run(id, Some(next));
            }
        }
    }
    let entry = reg.get(id).ok_or_else(|| format!("cron not found: {id}"))?;
    emit_entry(if enabled { "enabled" } else { "disabled" }, &entry, output_format)
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
        self.map.get(key).map(String::as_str).filter(|s| !s.is_empty())
    }
}
