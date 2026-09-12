//! The one place that knows a tool can be *named* two ways.
//!
//! Every capability is advertised under exactly ONE name (see
//! `tools::mvp_tool_specs`). What this module owns is narrower and
//! permanent: a model trained on Claude Code's tool set reaches for `Bash`,
//! `Agent`, `TaskStop`, `SendMessage` from habit even when it was handed
//! `bash`, `agent_spawn`, `pid_kill`, `send`, and a call refused for
//! spelling reads to the user as the model declining to act.
//!
//! It lives in `runtime`, below `tools`, because name matching is not only a
//! dispatch concern. Sites in this crate — the coordinator gate, the
//! concurrency classifier — match on tool names too, and while
//! `canonicalize_tool_name` sat in `tools` they could not call it, so each
//! hand-listed the spellings it happened to know. They drifted, exactly as
//! duplicated knowledge does: after the `Task*` → `pid_*` rename,
//! `is_concurrency_safe_tool` still listed only the old names, silently
//! dropping `pid_status`/`pid_output` from concurrent batches. Any site that
//! matches a model-supplied tool name canonicalises through here first.

/// Superseded tool name → this codebase's canonical tool name.
///
/// The canonical names are the Unified Agent/PID design's: `{layer}_{verb}`
/// for `agent_*` and `pid_*` (underscores, not the design's conceptual dots,
/// because a `.` in a JSON tool name breaks some providers), with `send` as
/// the deliberate exception — one messaging tool, auto-routed by target.
///
/// This is NOT a deprecation shim. Nothing here is reachable as a second
/// entry in the tool list; advertising a capability twice is what let a model
/// pick the wrong destination for a cross-machine message, so the alias table
/// and the spec list are kept strictly apart. What it buys is that a model
/// trained on Claude Code's tool set — which reaches for `Bash`, `Agent`,
/// `TaskStop`, `SendMessage` from habit — is not refused over spelling, since
/// a refusal reads to the user as the model declining to act.
///
/// Keys are the NORMALIZED (lower-cased, `-` → `_`) form. Note that
/// `SendMessage` and `send_message` normalize to DIFFERENT keys, so both are
/// listed. Tools whose canonical name IS the CC name (`Skill`, `WebFetch`,
/// `TaskCreate`, `AskUserQuestion`, …) need no entry —
/// [`canonicalize_tool_name`] passes them through unchanged.
const TOOL_ALIASES: &[(&str, &str)] = &[
    ("bash", "bash"),
    ("read", "read_file"),
    ("write", "write_file"),
    ("edit", "edit_file"),
    ("glob", "glob_search"),
    ("grep", "grep_search"),
    ("sendmessage", "send"),      // CC: SendMessage
    ("send_message", "send"),     // the A2A tool this replaced
    ("agent", "agent_spawn"),     // CC: Agent
    ("taskstop", "pid_kill"),     // CC: TaskStop
    ("taskget", "pid_status"),    // CC: TaskGet
    ("tasklist", "pid_status"),   // CC: TaskList — both map to pid_status
    ("taskoutput", "pid_output"), // CC: TaskOutput
];

/// Lower-case and fold `-` to `_` so `Web-Fetch`, `web_fetch` and `WebFetch`
/// all compare equal.
fn normalize_tool_name(value: &str) -> String {
    value.trim().replace('-', "_").to_ascii_lowercase()
}

/// Canonicalize a tool name from the model into the name the dispatcher and
/// every name-matching site use.
///
/// Unknown names pass through unchanged: this resolves spelling, it does not
/// validate. Dispatch rejects a name it has no arm for, and that error names
/// what the model actually asked for.
#[must_use]
pub fn canonicalize_tool_name(name: &str) -> String {
    let normalized = normalize_tool_name(name);
    for &(alias, canonical) in TOOL_ALIASES {
        if normalized == alias {
            return canonical.to_string();
        }
    }
    name.to_string()
}

/// The alias pairs, for callers that must enumerate them (the
/// `--allowedTools` parser accepts either spelling of a tool).
#[must_use]
pub fn tool_aliases() -> &'static [(&'static str, &'static str)] {
    TOOL_ALIASES
}
