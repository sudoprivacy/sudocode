//! Entry point for the builtin `vault` plugin's tools: `scode vault tool list`
//! and `scode vault tool use`.
//!
//! The tool input JSON arrives on stdin and exactly one JSON object leaves on
//! stdout; nothing is ever written to stderr, because the plugin host shows a
//! failing tool's stderr to the model. A refusal is a well-formed result,
//! `{"error": {"code", "message"}}`, so the process exits 0 even then. Nothing
//! here reads, logs or returns a secret value: `list` asks the backend for
//! version metadata only, and `use` stops before any credential or network
//! access.

mod credential;
mod invoke;
mod list;
mod settings;

use std::io::{self, Read as _, Write as _};

use serde_json::{json, Map, Value};

#[derive(Clone, Copy)]
pub(crate) enum Tool {
    List,
    Use,
}

/// The tool named by exactly `vault tool <list|use>`.
pub(crate) fn requested_tool(args: &[String]) -> Option<Tool> {
    match args {
        [vault, tool, name] if vault == "vault" && tool == "tool" => match name.as_str() {
            "list" => Some(Tool::List),
            "use" => Some(Tool::Use),
            _ => None,
        },
        _ => None,
    }
}

/// A refusal returned to the model as the tool's result.
struct Refusal {
    code: &'static str,
    message: String,
}

impl Refusal {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn invalid_input(message: impl Into<String>) -> Self {
        Self::new("invalid_input", message)
    }

    fn into_json(self) -> Value {
        json!({"error": {"code": self.code, "message": self.message}})
    }
}

/// Run one tool call from this directory and print its single JSON result.
/// A panic anywhere, the connection's worker thread included, becomes an
/// `internal` refusal that carries none of the panic's payload.
pub(crate) fn run(tool: Tool) {
    std::panic::set_hook(Box::new(|_| {}));
    let output = std::panic::catch_unwind(|| answer(tool)).unwrap_or_else(|_| {
        Refusal::new("internal", "the vault tool failed internally").into_json()
    });
    let mut stdout = io::stdout().lock();
    // With stdout gone there is nowhere left to report to.
    let _ = writeln!(stdout, "{output}").and_then(|()| stdout.flush());
}

fn answer(tool: Tool) -> Value {
    let input = read_input();
    let Ok(cwd) = std::env::current_dir() else {
        return Refusal::new("internal", "the working directory is unavailable").into_json();
    };
    match tool {
        Tool::List => list::run(&cwd, input).unwrap_or_else(Refusal::into_json),
        Tool::Use => invoke::run(&cwd, input).into_json(),
    }
}

/// The tool input object. Read before anything else so the host's pipe always
/// drains; an empty stdin is an empty object.
fn read_input() -> Result<Map<String, Value>, Refusal> {
    let mut raw = String::new();
    io::stdin()
        .read_to_string(&mut raw)
        .map_err(|error| Refusal::invalid_input(format!("cannot read the tool input: {error}")))?;
    if raw.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str(&raw) {
        Ok(Value::Object(input)) => Ok(input),
        _ => Err(Refusal::invalid_input(
            "the tool input must be a JSON object",
        )),
    }
}
