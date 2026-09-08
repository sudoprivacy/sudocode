//! System-prompt assembly — the engine-side prompt the turn loop starts from.
//!
//! Builds the process-default system prompt for a `cwd` (identity + workspace-
//! derived dynamic blocks), layers the process-wide `--system-prompt` /
//! `--append-system-prompt` CLI flags, and (for a session) the per-session
//! `_meta.sudocode.systemPrompt` / `appendSystemPrompt`. Which prompt the model
//! sees is an engine input, so both the REPL and `engine-acp` build it here.

use std::env;
use std::path::Path;
use std::sync::OnceLock;

use runtime::{load_system_prompt, SystemPrompt, SystemPromptOverrides};

/// Process-wide `--system-prompt` / `--append-system-prompt` flags, set once
/// from `run()` and applied by every prompt build in this process (REPL,
/// `--print`, `scode system-prompt`, and the ACP default a session starts
/// from before its own `_meta` adjustments).
static CLI_PROMPT_OVERRIDES: OnceLock<SystemPromptOverrides> = OnceLock::new();

/// Record the process-wide CLI prompt overrides. First-write-wins and
/// idempotent — the renderer calls this once at startup from
/// `parse_args_with_prompt_overrides`.
pub fn set_cli_prompt_overrides(overrides: SystemPromptOverrides) {
    let _ = CLI_PROMPT_OVERRIDES.set(overrides);
}

pub fn apply_cli_prompt_overrides(prompt: &mut SystemPrompt) {
    if let Some(overrides) = CLI_PROMPT_OVERRIDES.get() {
        overrides.apply(prompt);
    }
}

pub fn build_system_prompt_for(cwd: &Path) -> Result<SystemPrompt, Box<dyn std::error::Error>> {
    // Use the local date at session-start time (not the build date baked
    // into DEFAULT_DATE) so the cacheable system prompt reflects when the
    // user actually started talking. ConversationRuntime separately tracks
    // this date and emits a system-reminder if the date rolls over
    // mid-session, keeping the prompt cache prefix warm.
    let mut prompt = load_system_prompt(
        cwd.to_path_buf(),
        runtime::today_local(),
        env::consts::OS,
        "unknown",
    )?;
    // Coordinator mode: when the SUDOCODE_COORDINATOR_MODE env var is
    // set, prepend the ported CC-fork coordinator role prompt so it
    // takes primacy over the default identity. See
    // runtime::coordinator_mode for the full port.
    runtime::coordinator_mode::apply_coordinator_prompt_if_enabled(&mut prompt);
    apply_cli_prompt_overrides(&mut prompt);
    Ok(prompt)
}

/// ACP variant of [`build_system_prompt_for`]: builds the process-default
/// prompt for `cwd`/`model` (including any `--system-prompt` /
/// `--append-system-prompt` CLI flags), then layers the session's
/// `_meta.sudocode.systemPrompt` / `appendSystemPrompt` on top: the former
/// swaps the static blocks, the latter appends a trailing dynamic block.
/// Workspace-derived dynamic blocks (environment, `AGENTS.md`, memory,
/// plugins) stay, so the caller's prompt still knows where it is running.
///
/// Returns a plain error string; the renderer that owns the ACP wire wraps it
/// into an `AcpError` (engine-host stays below — and independent of — the ACP
/// serialization).
pub fn build_acp_system_prompt(
    cwd: &Path,
    prompt_overrides: &SystemPromptOverrides,
) -> Result<SystemPrompt, String> {
    let mut prompt =
        build_system_prompt_for(cwd).map_err(|e| format!("failed to build system prompt: {e}"))?;
    prompt_overrides.apply(&mut prompt);
    Ok(prompt)
}
