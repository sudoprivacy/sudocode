//! Shared status / model / config / doctor report rendering.
//!
//! These are the human-readable report builders behind `/status`, `/model`,
//! `/compact`, `/config` and `/doctor`. They live in `commands` — the
//! renderer-support crate both consumers already depend on — so the in-process
//! REPL (`rusty-sudocode-cli`) and the ACP renderer (`engine-acp`) render the
//! *same* reports from ONE definition instead of each owning a copy. Everything
//! here reads only `runtime` config/session data (+ spawned `git`/`tmux`); no
//! renderer- or engine-specific types cross into this module.
//!
//! The build-metadata a doctor report needs (`version` / `git_sha` /
//! `build_target`) is CLI-crate build info, so it is threaded in as
//! [`BuildInfo`] rather than read from a compiled-in constant here.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use runtime::{
    load_oauth_credentials, resolve_sandbox_status, ConfigLoader, ConfigSource, ProjectContext,
    SudoCodeConfig, TokenUsage,
};
use serde_json::{json, Map, Value};

/// Official distribution source of truth (doctor `Install source` check).
pub const OFFICIAL_REPO_URL: &str = "https://github.com/sudoprivacy/sudocode";
/// Official repo slug (`owner/name`).
pub const OFFICIAL_REPO_SLUG: &str = "sudoprivacy/sudocode";
/// The deprecated crate install command doctor warns against.
pub const DEPRECATED_INSTALL_COMMAND: &str = "cargo install sudocode";

/// Build metadata a doctor report surfaces. Threaded in by the caller (the CLI
/// passes its compiled-in `VERSION` / `GIT_SHA` / `BUILD_TARGET`; the ACP path
/// passes the same values carried on its config) so this crate needs no
/// build.rs of its own.
#[derive(Debug, Clone, Copy)]
pub struct BuildInfo<'a> {
    pub version: &'a str,
    pub git_sha: Option<&'a str>,
    pub build_target: Option<&'a str>,
}

/// A borrowed, renderer-neutral view of a resolved model's provenance, for the
/// `/status` `Model source` line. The CLI builds one from its `ModelProvenance`
/// (whose constructors are engine-host-coupled); the ACP path passes `None`.
#[derive(Debug, Clone, Copy)]
pub struct ProvenanceView<'a> {
    /// Resolved model string (after alias expansion).
    pub resolved: &'a str,
    /// Raw user input before alias resolution; `None` when the source is the
    /// compiled-in default.
    pub raw: Option<&'a str>,
    /// Where the resolved model came from (`flag` / `env` / `config` / `default`).
    pub source: &'a str,
}

// ===========================================================================
// Git workspace summary
// ===========================================================================

#[allow(clippy::struct_field_names)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GitWorkspaceSummary {
    pub changed_files: usize,
    pub staged_files: usize,
    pub unstaged_files: usize,
    pub untracked_files: usize,
    pub conflicted_files: usize,
}

impl GitWorkspaceSummary {
    #[must_use]
    pub fn is_clean(self) -> bool {
        self.changed_files == 0
    }

    #[must_use]
    pub fn headline(self) -> String {
        if self.is_clean() {
            "clean".to_string()
        } else {
            let mut details = Vec::new();
            if self.staged_files > 0 {
                details.push(format!("{} staged", self.staged_files));
            }
            if self.unstaged_files > 0 {
                details.push(format!("{} unstaged", self.unstaged_files));
            }
            if self.untracked_files > 0 {
                details.push(format!("{} untracked", self.untracked_files));
            }
            if self.conflicted_files > 0 {
                details.push(format!("{} conflicted", self.conflicted_files));
            }
            format!(
                "dirty · {} files · {}",
                self.changed_files,
                details.join(", ")
            )
        }
    }
}

#[must_use]
pub fn parse_git_status_metadata(status: Option<&str>) -> (Option<PathBuf>, Option<String>) {
    parse_git_status_metadata_for(
        &runtime::current_workspace_root().unwrap_or_else(|_| PathBuf::from(".")),
        status,
    )
}

#[must_use]
pub fn parse_git_status_branch(status: Option<&str>) -> Option<String> {
    let status = status?;
    let first_line = status.lines().next()?;
    let line = first_line.strip_prefix("## ")?;
    if line.starts_with("HEAD") {
        return Some("detached HEAD".to_string());
    }
    let branch = line.split(['.', ' ']).next().unwrap_or_default().trim();
    if branch.is_empty() {
        None
    } else {
        Some(branch.to_string())
    }
}

#[must_use]
pub fn parse_git_workspace_summary(status: Option<&str>) -> GitWorkspaceSummary {
    let mut summary = GitWorkspaceSummary::default();
    let Some(status) = status else {
        return summary;
    };

    for line in status.lines() {
        if line.starts_with("## ") || line.trim().is_empty() {
            continue;
        }

        summary.changed_files += 1;
        let mut chars = line.chars();
        let index_status = chars.next().unwrap_or(' ');
        let worktree_status = chars.next().unwrap_or(' ');

        if index_status == '?' && worktree_status == '?' {
            summary.untracked_files += 1;
            continue;
        }

        if index_status != ' ' {
            summary.staged_files += 1;
        }
        if worktree_status != ' ' {
            summary.unstaged_files += 1;
        }
        if (matches!(index_status, 'U' | 'A') && matches!(worktree_status, 'U' | 'A'))
            || index_status == 'U'
            || worktree_status == 'U'
        {
            summary.conflicted_files += 1;
        }
    }

    summary
}

#[must_use]
pub fn resolve_git_branch_for(cwd: &Path) -> Option<String> {
    let branch = run_git_capture_in(cwd, &["branch", "--show-current"])?;
    let branch = branch.trim();
    if !branch.is_empty() {
        return Some(branch.to_string());
    }

    let fallback = run_git_capture_in(cwd, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    let fallback = fallback.trim();
    if fallback.is_empty() {
        None
    } else if fallback == "HEAD" {
        Some("detached HEAD".to_string())
    } else {
        Some(fallback.to_string())
    }
}

#[must_use]
pub fn run_git_capture_in(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// # Errors
/// Returns an error when `cwd` is not inside a git repository.
pub fn find_git_root_in(cwd: &Path) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output()?;
    if !output.status.success() {
        return Err("not a git repository".into());
    }
    let path = String::from_utf8(output.stdout)?.trim().to_string();
    if path.is_empty() {
        return Err("empty git root".into());
    }
    Ok(PathBuf::from(path))
}

#[must_use]
pub fn parse_git_status_metadata_for(
    cwd: &Path,
    status: Option<&str>,
) -> (Option<PathBuf>, Option<String>) {
    let branch = resolve_git_branch_for(cwd).or_else(|| parse_git_status_branch(status));
    let project_root = find_git_root_in(cwd).ok();
    (project_root, branch)
}

// ===========================================================================
// Session lifecycle classification
// ===========================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionLifecycleKind {
    RunningProcess,
    IdleShell,
    SavedOnly,
}

impl SessionLifecycleKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RunningProcess => "running_process",
            Self::IdleShell => "idle_shell",
            Self::SavedOnly => "saved_only",
        }
    }

    #[must_use]
    pub fn human_label(self) -> &'static str {
        match self {
            Self::RunningProcess => "running process",
            Self::IdleShell => "idle shell",
            Self::SavedOnly => "saved only",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionLifecycleSummary {
    pub kind: SessionLifecycleKind,
    pub pane_id: Option<String>,
    pub pane_command: Option<String>,
    pub pane_path: Option<PathBuf>,
    pub workspace_dirty: bool,
    pub abandoned: bool,
}

impl SessionLifecycleSummary {
    #[must_use]
    pub fn signal(&self) -> String {
        let mut parts = vec![self.kind.human_label().to_string()];
        if self.workspace_dirty {
            parts.push("dirty worktree".to_string());
        }
        if self.abandoned {
            parts.push("abandoned?".to_string());
        }
        if let Some(command) = self.pane_command.as_deref() {
            parts.push(format!("cmd={command}"));
        }
        parts.join(" · ")
    }

    #[must_use]
    pub fn json_value(&self) -> Value {
        json!({
            "kind": self.kind.as_str(),
            "pane_id": self.pane_id,
            "pane_command": self.pane_command,
            "pane_path": self.pane_path.as_ref().map(|path| path.display().to_string()),
            "workspace_dirty": self.workspace_dirty,
            "abandoned": self.abandoned,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxPaneSnapshot {
    pub pane_id: String,
    pub current_command: String,
    pub current_path: PathBuf,
}

#[must_use]
pub fn classify_session_lifecycle_for(workspace: &Path) -> SessionLifecycleSummary {
    classify_session_lifecycle_from_panes(workspace, discover_tmux_panes())
}

#[must_use]
pub fn classify_session_lifecycle_from_panes(
    workspace: &Path,
    panes: Vec<TmuxPaneSnapshot>,
) -> SessionLifecycleSummary {
    let workspace_dirty = git_worktree_is_dirty(workspace);
    let mut idle_shell = None;
    for pane in panes {
        if !pane_path_matches_workspace(&pane.current_path, workspace) {
            continue;
        }
        if is_idle_shell_command(&pane.current_command) {
            idle_shell.get_or_insert(pane);
        } else {
            return SessionLifecycleSummary {
                kind: SessionLifecycleKind::RunningProcess,
                pane_id: Some(pane.pane_id),
                pane_command: Some(pane.current_command),
                pane_path: Some(pane.current_path),
                workspace_dirty,
                abandoned: false,
            };
        }
    }

    if let Some(pane) = idle_shell {
        SessionLifecycleSummary {
            kind: SessionLifecycleKind::IdleShell,
            pane_id: Some(pane.pane_id),
            pane_command: Some(pane.current_command),
            pane_path: Some(pane.current_path),
            workspace_dirty,
            abandoned: workspace_dirty,
        }
    } else {
        SessionLifecycleSummary {
            kind: SessionLifecycleKind::SavedOnly,
            pane_id: None,
            pane_command: None,
            pane_path: None,
            workspace_dirty,
            abandoned: workspace_dirty,
        }
    }
}

fn discover_tmux_panes() -> Vec<TmuxPaneSnapshot> {
    let output = Command::new("tmux")
        .args([
            "list-panes",
            "-a",
            "-F",
            "#{pane_id}\t#{pane_current_command}\t#{pane_current_path}",
        ])
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_tmux_pane_snapshots(&stdout)
}

fn parse_tmux_pane_snapshots(output: &str) -> Vec<TmuxPaneSnapshot> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(3, '\t');
            let pane_id = fields.next()?.trim();
            let current_command = fields.next()?.trim();
            let current_path = fields.next()?.trim();
            if pane_id.is_empty() || current_path.is_empty() {
                return None;
            }
            Some(TmuxPaneSnapshot {
                pane_id: pane_id.to_string(),
                current_command: current_command.to_string(),
                current_path: PathBuf::from(current_path),
            })
        })
        .collect()
}

fn pane_path_matches_workspace(pane_path: &Path, workspace: &Path) -> bool {
    let pane_path = fs::canonicalize(pane_path).unwrap_or_else(|_| pane_path.to_path_buf());
    let workspace = fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    pane_path == workspace || pane_path.starts_with(&workspace)
}

fn is_idle_shell_command(command: &str) -> bool {
    let command = command.rsplit('/').next().unwrap_or(command);
    matches!(
        command,
        "bash" | "zsh" | "sh" | "fish" | "nu" | "pwsh" | "powershell" | "cmd"
    )
}

fn git_worktree_is_dirty(workspace: &Path) -> bool {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(["status", "--porcelain"])
        .output();
    output
        .ok()
        .filter(|output| output.status.success())
        .is_some_and(|output| !output.stdout.is_empty())
}

// ===========================================================================
// Status report
// ===========================================================================

#[derive(Debug, Clone)]
pub struct StatusContext {
    pub cwd: PathBuf,
    pub session_path: Option<PathBuf>,
    pub loaded_config_files: usize,
    pub discovered_config_files: usize,
    pub memory_file_count: usize,
    pub project_root: Option<PathBuf>,
    pub git_branch: Option<String>,
    pub git_summary: GitWorkspaceSummary,
    pub session_lifecycle: SessionLifecycleSummary,
    pub sandbox_status: runtime::SandboxStatus,
    /// #143: when a loaded config file fails to parse, the parse error is
    /// captured here and every field that doesn't depend on runtime config is
    /// still populated. Top-level JSON output then reports `status: "degraded"`.
    pub config_load_error: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct StatusUsage {
    pub message_count: usize,
    pub turns: u32,
    pub latest: TokenUsage,
    pub cumulative: TokenUsage,
    pub estimated_tokens: usize,
}

/// Gather the status context (config discovery, git, sandbox, lifecycle) for
/// `session_path`. Degrades gracefully on a config parse failure (records it in
/// `config_load_error` and still populates config-independent fields).
///
/// # Errors
/// Returns an error only when the workspace root or project context cannot be
/// resolved at all.
pub fn status_context(
    session_path: Option<&Path>,
) -> Result<StatusContext, Box<dyn std::error::Error>> {
    let cwd = runtime::current_workspace_root()?;
    let loader = ConfigLoader::default_for(&cwd);
    let discovered_config_files = loader.discover().len();
    let (loaded_config_files, sandbox_status, config_load_error) = match loader.load() {
        Ok(runtime_config) => (
            runtime_config.loaded_entries().len(),
            resolve_sandbox_status(runtime_config.sandbox(), &cwd),
            None,
        ),
        Err(err) => (
            0,
            resolve_sandbox_status(&runtime::SandboxConfig::default(), &cwd),
            Some(err.to_string()),
        ),
    };
    let project_context = ProjectContext::discover_with_git(&cwd, runtime::today_local())?;
    let (project_root, git_branch) =
        parse_git_status_metadata(project_context.git_status.as_deref());
    let git_summary = parse_git_workspace_summary(project_context.git_status.as_deref());
    Ok(StatusContext {
        cwd: cwd.clone(),
        session_path: session_path.map(Path::to_path_buf),
        loaded_config_files,
        discovered_config_files,
        memory_file_count: project_context.instruction_files.len(),
        project_root,
        git_branch,
        git_summary,
        session_lifecycle: classify_session_lifecycle_for(&cwd),
        sandbox_status,
        config_load_error,
    })
}

#[must_use]
pub fn format_status_report(
    model: &str,
    usage: StatusUsage,
    permission_mode: &str,
    context: &StatusContext,
    provenance: Option<&ProvenanceView>,
) -> String {
    let status_line = if context.config_load_error.is_some() {
        "Status (degraded)"
    } else {
        "Status"
    };
    let mut blocks: Vec<String> = Vec::new();
    if let Some(err) = context.config_load_error.as_deref() {
        blocks.push(format!(
            "Config load error\n  Status           fail\n  Summary          runtime config failed to load; reporting partial status\n  Details          {err}\n  Hint             `scode doctor` classifies config parse errors; fix the listed field and rerun"
        ));
    }
    let model_source_line = provenance
        .map(|p| match p.raw {
            Some(raw) if raw != model => {
                format!("\n  Model source     {} (raw: {raw})", p.source)
            }
            _ => format!("\n  Model source     {}", p.source),
        })
        .unwrap_or_default();
    blocks.extend([
        format!(
            "{status_line}
  Model            {model}{model_source_line}
  Permission mode  {permission_mode}
  Messages         {}
  Turns            {}
  Estimated tokens {}",
            usage.message_count, usage.turns, usage.estimated_tokens,
        ),
        format!(
            "Usage
  Latest total     {}
  Cumulative input {}
  Cumulative output {}
  Cumulative total {}",
            usage.latest.total_tokens(),
            usage.cumulative.input_tokens,
            usage.cumulative.output_tokens,
            usage.cumulative.total_tokens(),
        ),
        format!(
            "Workspace
  Cwd              {}
  Project root     {}
  Git branch       {}
  Git state        {}
  Changed files    {}
  Staged           {}
  Unstaged         {}
  Untracked        {}
  Session          {}
  Lifecycle        {}
  Config files     loaded {}/{}
  Memory files     {}
  Suggested flow   /status → /diff → /commit",
            context.cwd.display(),
            context
                .project_root
                .as_ref()
                .map_or_else(|| "unknown".to_string(), |path| path.display().to_string()),
            context.git_branch.as_deref().unwrap_or("unknown"),
            context.git_summary.headline(),
            context.git_summary.changed_files,
            context.git_summary.staged_files,
            context.git_summary.unstaged_files,
            context.git_summary.untracked_files,
            context.session_path.as_ref().map_or_else(
                || "live-repl".to_string(),
                |path| path.display().to_string()
            ),
            context.session_lifecycle.signal(),
            context.loaded_config_files,
            context.discovered_config_files,
            context.memory_file_count,
        ),
        format_sandbox_report(&context.sandbox_status),
    ]);
    blocks.join("\n\n")
}

// ===========================================================================
// Model / compact / sandbox reports
// ===========================================================================

/// Render the `/model` report. The available-model list is derived from
/// `config` (aliases with display names + provider modes) unioned with the
/// capability SSOT; the caller loads `config` (engine-host-side) so this stays
/// a pure formatter.
#[must_use]
pub fn format_model_report(
    model: &str,
    message_count: usize,
    turns: u32,
    config: &SudoCodeConfig,
) -> String {
    let model_lower = model.to_ascii_lowercase();

    let mut available_lines = String::new();
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for (alias, entry) in &config.models {
        let marker = if alias == &model_lower { " *" } else { "" };
        let provider_modes: Vec<&str> = entry.providers.keys().map(String::as_str).collect();
        write!(
            available_lines,
            "\n    {:<16} {} ({}){marker}",
            alias,
            entry.name,
            provider_modes.join(", ")
        )
        .expect("write to string");
        seen.insert(alias.to_ascii_lowercase());
    }

    for id in runtime::model_capabilities::all_model_ids() {
        if !seen.contains(&id.to_ascii_lowercase()) {
            let marker = if id.eq_ignore_ascii_case(model) {
                " *"
            } else {
                ""
            };
            write!(available_lines, "\n    {id}{marker}").expect("write to string");
        }
    }

    format!(
        "Model
  Current model    {model}
  Available models{available_lines}
  Session messages {message_count}
  Session turns    {turns}

Usage
  Switch models with /model <name>"
    )
}

#[must_use]
pub fn format_model_switch_report(previous: &str, next: &str, message_count: usize) -> String {
    format!(
        "Model updated
  Previous         {previous}
  Current          {next}
  Preserved msgs   {message_count}"
    )
}

/// `/compact` report under ACP: what happened, how, and the effect on the
/// transcript. `method` is `None` when nothing was removed.
#[must_use]
pub fn format_acp_compact_report(
    before_tokens: usize,
    after_tokens: usize,
    removed: usize,
    kept: usize,
    method: Option<(runtime::CompactionMethod, &runtime::CompactionSummarySource)>,
) -> String {
    match method {
        Some((method, summary_source)) => format!(
            "Compact
  Result           compacted
  Method           {}
  Summary          {summary_source}
  Messages removed {removed}
  Messages kept    {kept}
  Estimated tokens {before_tokens} before, {after_tokens} after",
            method.as_str()
        ),
        None => format!(
            "Compact
  Result           skipped
  Reason           nothing to compact beyond the preserved recent messages
  Messages kept    {kept}
  Estimated tokens {after_tokens}"
        ),
    }
}

#[must_use]
pub fn format_sandbox_report(status: &runtime::SandboxStatus) -> String {
    format!(
        "Sandbox
  Enabled           {}
  Active            {}
  Supported         {}
  In container      {}
  Requested ns      {}
  Active ns         {}
  Requested net     {}
  Active net        {}
  Filesystem mode   {}
  Filesystem active {}
  Allowed mounts    {}
  Markers           {}
  Fallback reason   {}",
        status.enabled,
        status.active,
        status.supported,
        status.in_container,
        status.requested.namespace_restrictions,
        status.namespace_active,
        status.requested.network_isolation,
        status.network_active,
        status.filesystem_mode.as_str(),
        status.filesystem_active,
        if status.allowed_mounts.is_empty() {
            "<none>".to_string()
        } else {
            status.allowed_mounts.join(", ")
        },
        if status.container_markers.is_empty() {
            "<none>".to_string()
        } else {
            status.container_markers.join(", ")
        },
        status
            .fallback_reason
            .clone()
            .unwrap_or_else(|| "<none>".to_string()),
    )
}

// ===========================================================================
// Config report
// ===========================================================================

/// Render the `/config` report for `section` (or the whole merged config).
///
/// # Errors
/// Returns an error when the workspace root or the runtime config cannot be
/// loaded.
pub fn render_config_report(section: Option<&str>) -> Result<String, Box<dyn std::error::Error>> {
    let cwd = runtime::current_workspace_root()?;
    let loader = ConfigLoader::default_for(&cwd);
    let discovered = loader.discover();
    let runtime_config = loader.load()?;

    let mut lines = vec![
        format!(
            "Config
  Working directory {}
  Loaded files      {}
  Merged keys       {}",
            cwd.display(),
            runtime_config.loaded_entries().len(),
            runtime_config.merged().len()
        ),
        "Discovered files".to_string(),
    ];
    for entry in discovered {
        let source = match entry.source {
            ConfigSource::User => "user",
            ConfigSource::Project => "project",
            ConfigSource::Local => "local",
        };
        let status = if runtime_config
            .loaded_entries()
            .iter()
            .any(|loaded_entry| loaded_entry.path == entry.path)
        {
            "loaded"
        } else {
            "missing"
        };
        lines.push(format!(
            "  {source:<7} {status:<7} {}",
            entry.path.display()
        ));
    }

    if let Some(section) = section {
        lines.push(format!("Merged section: {section}"));
        let value = match section {
            "env" => runtime_config.get("env"),
            "hooks" => runtime_config.get("hooks"),
            "model" => runtime_config.get("model"),
            "plugins" => runtime_config
                .get("plugins")
                .or_else(|| runtime_config.get("enabledPlugins")),
            other => {
                lines.push(format!(
                    "  Unsupported config section '{other}'. Use env, hooks, model, or plugins."
                ));
                return Ok(lines.join("\n"));
            }
        };
        lines.push(format!(
            "  {}",
            match value {
                Some(value) => value.render(),
                None => "<unset>".to_string(),
            }
        ));
        return Ok(lines.join("\n"));
    }

    lines.push("Merged JSON".to_string());
    lines.push(format!("  {}", runtime_config.as_json().render()));
    Ok(lines.join("\n"))
}

// ===========================================================================
// Doctor report
// ===========================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticLevel {
    Ok,
    Warn,
    Fail,
}

impl DiagnosticLevel {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Fail => "fail",
        }
    }

    #[must_use]
    pub fn is_failure(self) -> bool {
        matches!(self, Self::Fail)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticCheck {
    pub name: &'static str,
    pub level: DiagnosticLevel,
    pub summary: String,
    pub details: Vec<String>,
    pub data: Map<String, Value>,
}

impl DiagnosticCheck {
    #[must_use]
    pub fn new(name: &'static str, level: DiagnosticLevel, summary: impl Into<String>) -> Self {
        Self {
            name,
            level,
            summary: summary.into(),
            details: Vec::new(),
            data: Map::new(),
        }
    }

    #[must_use]
    pub fn with_details(mut self, details: Vec<String>) -> Self {
        self.details = details;
        self
    }

    #[must_use]
    pub fn with_data(mut self, data: Map<String, Value>) -> Self {
        self.data = data;
        self
    }

    #[must_use]
    pub fn json_value(&self) -> Value {
        let mut value = Map::from_iter([
            (
                "name".to_string(),
                Value::String(self.name.to_ascii_lowercase()),
            ),
            (
                "status".to_string(),
                Value::String(self.level.label().to_string()),
            ),
            ("summary".to_string(), Value::String(self.summary.clone())),
            (
                "details".to_string(),
                Value::Array(
                    self.details
                        .iter()
                        .cloned()
                        .map(Value::String)
                        .collect::<Vec<_>>(),
                ),
            ),
        ]);
        value.extend(self.data.clone());
        Value::Object(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorReport {
    pub checks: Vec<DiagnosticCheck>,
}

impl DoctorReport {
    #[must_use]
    pub fn counts(&self) -> (usize, usize, usize) {
        (
            self.checks
                .iter()
                .filter(|check| check.level == DiagnosticLevel::Ok)
                .count(),
            self.checks
                .iter()
                .filter(|check| check.level == DiagnosticLevel::Warn)
                .count(),
            self.checks
                .iter()
                .filter(|check| check.level == DiagnosticLevel::Fail)
                .count(),
        )
    }

    #[must_use]
    pub fn has_failures(&self) -> bool {
        self.checks.iter().any(|check| check.level.is_failure())
    }

    #[must_use]
    pub fn render(&self) -> String {
        let (ok_count, warn_count, fail_count) = self.counts();
        let mut lines = vec![
            "Doctor".to_string(),
            format!(
                "Summary\n  OK               {ok_count}\n  Warnings         {warn_count}\n  Failures         {fail_count}"
            ),
        ];
        lines.extend(self.checks.iter().map(render_diagnostic_check));
        if fail_count == 0 && warn_count == 0 {
            lines.push("Sudo Code is healthy.".to_string());
        }
        lines.join("\n\n")
    }

    #[must_use]
    pub fn json_value(&self) -> Value {
        let report = self.render();
        let (ok_count, warn_count, fail_count) = self.counts();
        let healthy = fail_count == 0 && warn_count == 0;
        json!({
            "kind": "doctor",
            "message": report,
            "report": report,
            "healthy": healthy,
            "has_failures": self.has_failures(),
            "summary": {
                "total": self.checks.len(),
                "ok": ok_count,
                "warnings": warn_count,
                "failures": fail_count,
            },
            "checks": self
                .checks
                .iter()
                .map(DiagnosticCheck::json_value)
                .collect::<Vec<_>>(),
        })
    }
}

#[must_use]
pub fn render_diagnostic_check(check: &DiagnosticCheck) -> String {
    let mut lines = vec![format!(
        "{}\n  Status           {}\n  Summary          {}",
        check.name,
        check.level.label(),
        check.summary
    )];
    if !check.details.is_empty() {
        lines.push("  Details".to_string());
        lines.extend(check.details.iter().map(|detail| format!("    - {detail}")));
    }
    lines.join("\n")
}

/// Gather a full doctor report (auth / config / install / workspace / sandbox /
/// system / account checks). `build` supplies the CLI-crate build metadata the
/// system check surfaces.
///
/// # Errors
/// Returns an error only when the workspace root or project context cannot be
/// resolved at all.
pub fn render_doctor_report(build: &BuildInfo) -> Result<DoctorReport, Box<dyn std::error::Error>> {
    let cwd = runtime::current_workspace_root()?;
    let config_loader = ConfigLoader::default_for(&cwd);
    let config = config_loader.load();
    let discovered_config = config_loader.discover();
    let project_context = ProjectContext::discover_with_git(&cwd, runtime::today_local())?;
    let (project_root, git_branch) =
        parse_git_status_metadata(project_context.git_status.as_deref());
    let git_summary = parse_git_workspace_summary(project_context.git_status.as_deref());
    let empty_config = runtime::RuntimeConfig::empty();
    let sandbox_config = config.as_ref().ok().unwrap_or(&empty_config);
    let context = StatusContext {
        cwd: cwd.clone(),
        session_path: None,
        loaded_config_files: config
            .as_ref()
            .ok()
            .map_or(0, |runtime_config| runtime_config.loaded_entries().len()),
        discovered_config_files: discovered_config.len(),
        memory_file_count: project_context.instruction_files.len(),
        project_root,
        git_branch,
        git_summary,
        session_lifecycle: classify_session_lifecycle_for(&cwd),
        sandbox_status: resolve_sandbox_status(sandbox_config.sandbox(), &cwd),
        config_load_error: config.as_ref().err().map(ToString::to_string),
    };
    Ok(DoctorReport {
        checks: vec![
            check_auth_health(),
            check_config_health(&config_loader, config.as_ref()),
            check_install_source_health(),
            check_workspace_health(&context),
            check_sandbox_health(&context.sandbox_status),
            check_system_health(&cwd, config.as_ref().ok(), build),
            check_account_health(&config_loader),
        ],
    })
}

fn check_account_health(config_loader: &ConfigLoader) -> DiagnosticCheck {
    let config = match config_loader.load_sudocode_config() {
        Ok(config) => config,
        Err(err) => {
            return DiagnosticCheck::new(
                "Account",
                DiagnosticLevel::Warn,
                "could not load config to resolve the proxy account",
            )
            .with_details(vec![err.to_string()]);
        }
    };
    let Some(accounts) = config
        .auth_modes
        .get("proxy")
        .filter(|accounts| !accounts.is_empty())
    else {
        return DiagnosticCheck::new(
            "Account",
            DiagnosticLevel::Ok,
            "no proxy accounts configured",
        );
    };
    let selected = config.selected_account.as_deref();
    let resolved = match selected {
        Some(name) => accounts.get_key_value(name),
        None => accounts.iter().next(),
    };
    match resolved {
        Some((name, connection)) => {
            DiagnosticCheck::new("Account", DiagnosticLevel::Ok, "resolved proxy account")
                .with_details(vec![format!(
                    "account={name} base_url={} auth_profile={}",
                    connection.base_url,
                    selected.unwrap_or("<default: first>")
                )])
        }
        None => DiagnosticCheck::new(
            "Account",
            DiagnosticLevel::Warn,
            "auth_profile does not match any configured proxy account",
        )
        .with_details(vec![format!(
            "auth_profile={} is not present in auth_modes.proxy",
            selected.unwrap_or("<none>")
        )]),
    }
}

#[allow(clippy::too_many_lines)]
fn check_auth_health() -> DiagnosticCheck {
    let env_present = |key: &str| {
        env::var(key)
            .ok()
            .is_some_and(|value| !value.trim().is_empty())
    };
    let anthropic_api_key_present = env_present("ANTHROPIC_API_KEY");
    let anthropic_auth_token_present = env_present("ANTHROPIC_AUTH_TOKEN");
    let api_key_present = anthropic_api_key_present || anthropic_auth_token_present;
    let proxy_token_present = env_present("PROXY_AUTH_TOKEN");
    let claude_code_oauth_token_present = env_present("CLAUDE_CODE_OAUTH_TOKEN");
    let supported_auth_env_present =
        api_key_present || proxy_token_present || claude_code_oauth_token_present;
    let state = |present: bool| if present { "present" } else { "absent" };
    let env_details = format!(
        "Environment       ANTHROPIC_API_KEY={api_key} ANTHROPIC_AUTH_TOKEN={auth_token} \
         PROXY_AUTH_TOKEN={proxy} CLAUDE_CODE_OAUTH_TOKEN={oauth}",
        api_key = state(anthropic_api_key_present),
        auth_token = state(anthropic_auth_token_present),
        proxy = state(proxy_token_present),
        oauth = state(claude_code_oauth_token_present),
    );

    match load_oauth_credentials() {
        Ok(Some(token_set)) => DiagnosticCheck::new(
            "Auth",
            if supported_auth_env_present {
                DiagnosticLevel::Ok
            } else {
                DiagnosticLevel::Warn
            },
            if supported_auth_env_present {
                "supported auth env vars are configured; legacy saved OAuth is ignored"
            } else {
                "legacy saved OAuth credentials are present but unsupported"
            },
        )
        .with_details(vec![
            env_details,
            format!(
                "Legacy OAuth      expires_at={} refresh_token={} scopes={}",
                token_set
                    .expires_at
                    .map_or_else(|| "<none>".to_string(), |value| value.to_string()),
                if token_set.refresh_token.is_some() {
                    "present"
                } else {
                    "absent"
                },
                if token_set.scopes.is_empty() {
                    "<none>".to_string()
                } else {
                    token_set.scopes.join(",")
                }
            ),
            "Suggested action  run `scode login` to refresh, or set ANTHROPIC_API_KEY".to_string(),
        ])
        .with_data(Map::from_iter([
            ("api_key_present".to_string(), json!(api_key_present)),
            (
                "anthropic_api_key_present".to_string(),
                json!(anthropic_api_key_present),
            ),
            (
                "anthropic_auth_token_present".to_string(),
                json!(anthropic_auth_token_present),
            ),
            (
                "proxy_token_present".to_string(),
                json!(proxy_token_present),
            ),
            (
                "claude_code_oauth_token_present".to_string(),
                json!(claude_code_oauth_token_present),
            ),
            ("legacy_saved_oauth_present".to_string(), json!(true)),
            (
                "legacy_saved_oauth_expires_at".to_string(),
                json!(token_set.expires_at),
            ),
            (
                "legacy_refresh_token_present".to_string(),
                json!(token_set.refresh_token.is_some()),
            ),
            ("legacy_scopes".to_string(), json!(token_set.scopes)),
        ])),
        Ok(None) => DiagnosticCheck::new(
            "Auth",
            if supported_auth_env_present {
                DiagnosticLevel::Ok
            } else {
                DiagnosticLevel::Warn
            },
            if supported_auth_env_present {
                "supported auth env vars are configured"
            } else {
                "no supported auth env vars were found"
            },
        )
        .with_details(vec![env_details])
        .with_data(Map::from_iter([
            ("api_key_present".to_string(), json!(api_key_present)),
            (
                "anthropic_api_key_present".to_string(),
                json!(anthropic_api_key_present),
            ),
            (
                "anthropic_auth_token_present".to_string(),
                json!(anthropic_auth_token_present),
            ),
            (
                "proxy_token_present".to_string(),
                json!(proxy_token_present),
            ),
            (
                "claude_code_oauth_token_present".to_string(),
                json!(claude_code_oauth_token_present),
            ),
            ("legacy_saved_oauth_present".to_string(), json!(false)),
            ("legacy_saved_oauth_expires_at".to_string(), Value::Null),
            ("legacy_refresh_token_present".to_string(), json!(false)),
            ("legacy_scopes".to_string(), json!(Vec::<String>::new())),
        ])),
        Err(error) => DiagnosticCheck::new(
            "Auth",
            DiagnosticLevel::Fail,
            format!("failed to inspect legacy saved credentials: {error}"),
        )
        .with_data(Map::from_iter([
            ("api_key_present".to_string(), json!(api_key_present)),
            (
                "anthropic_api_key_present".to_string(),
                json!(anthropic_api_key_present),
            ),
            (
                "anthropic_auth_token_present".to_string(),
                json!(anthropic_auth_token_present),
            ),
            (
                "proxy_token_present".to_string(),
                json!(proxy_token_present),
            ),
            (
                "claude_code_oauth_token_present".to_string(),
                json!(claude_code_oauth_token_present),
            ),
            ("legacy_saved_oauth_present".to_string(), Value::Null),
            ("legacy_saved_oauth_expires_at".to_string(), Value::Null),
            ("legacy_refresh_token_present".to_string(), Value::Null),
            ("legacy_scopes".to_string(), Value::Null),
            (
                "legacy_saved_oauth_error".to_string(),
                json!(error.to_string()),
            ),
        ])),
    }
}

fn check_config_health(
    config_loader: &ConfigLoader,
    config: Result<&runtime::RuntimeConfig, &runtime::ConfigError>,
) -> DiagnosticCheck {
    let discovered = config_loader.discover();
    let discovered_count = discovered.len();
    let present_paths: Vec<String> = discovered
        .iter()
        .filter(|e| e.path.exists())
        .map(|e| e.path.display().to_string())
        .collect();
    let discovered_paths = discovered
        .iter()
        .map(|entry| entry.path.display().to_string())
        .collect::<Vec<_>>();
    match config {
        Ok(runtime_config) => {
            let loaded_entries = runtime_config.loaded_entries();
            let loaded_count = loaded_entries.len();
            let present_count = present_paths.len();
            let mut details = vec![format!(
                "Config files      loaded {}/{}",
                loaded_count, present_count
            )];
            if let Some(model) = runtime_config.model() {
                details.push(format!("Resolved model    {model}"));
            }
            details.push(format!(
                "MCP servers       {}",
                runtime_config.mcp().servers().len()
            ));
            if present_paths.is_empty() {
                details.push("Discovered files  <none> (defaults active)".to_string());
            } else {
                details.extend(
                    present_paths
                        .iter()
                        .map(|path| format!("Discovered file   {path}")),
                );
            }
            DiagnosticCheck::new(
                "Config",
                DiagnosticLevel::Ok,
                if present_count == 0 {
                    "no config files present; defaults are active"
                } else {
                    "runtime config loaded successfully"
                },
            )
            .with_details(details)
            .with_data(Map::from_iter([
                ("discovered_files".to_string(), json!(present_paths)),
                ("discovered_files_count".to_string(), json!(present_count)),
                ("loaded_config_files".to_string(), json!(loaded_count)),
                ("resolved_model".to_string(), json!(runtime_config.model())),
                (
                    "mcp_servers".to_string(),
                    json!(runtime_config.mcp().servers().len()),
                ),
            ]))
        }
        Err(error) => DiagnosticCheck::new(
            "Config",
            DiagnosticLevel::Fail,
            format!("runtime config failed to load: {error}"),
        )
        .with_details(if discovered_paths.is_empty() {
            vec!["Discovered files  <none>".to_string()]
        } else {
            discovered_paths
                .iter()
                .map(|path| format!("Discovered file   {path}"))
                .collect()
        })
        .with_data(Map::from_iter([
            ("discovered_files".to_string(), json!(discovered_paths)),
            (
                "discovered_files_count".to_string(),
                json!(discovered_count),
            ),
            ("loaded_config_files".to_string(), json!(0)),
            ("resolved_model".to_string(), Value::Null),
            ("mcp_servers".to_string(), Value::Null),
            ("load_error".to_string(), json!(error.to_string())),
        ])),
    }
}

fn check_install_source_health() -> DiagnosticCheck {
    DiagnosticCheck::new(
        "Install source",
        DiagnosticLevel::Ok,
        format!(
            "official source of truth is {OFFICIAL_REPO_SLUG}; avoid `{DEPRECATED_INSTALL_COMMAND}`"
        ),
    )
    .with_details(vec![
        format!("Official repo     {OFFICIAL_REPO_URL}"),
        "Recommended path  build from this repo or use the upstream binary documented in README.md"
            .to_string(),
        format!(
            "Deprecated crate  `{DEPRECATED_INSTALL_COMMAND}` installs a deprecated stub and does not provide the `scode` binary"
        ),
    ])
    .with_data(Map::from_iter([
        ("official_repo".to_string(), json!(OFFICIAL_REPO_URL)),
        (
            "deprecated_install".to_string(),
            json!(DEPRECATED_INSTALL_COMMAND),
        ),
        (
            "recommended_install".to_string(),
            json!("build from source or follow the upstream binary instructions in README.md"),
        ),
    ]))
}

fn check_workspace_health(context: &StatusContext) -> DiagnosticCheck {
    let in_repo = context.project_root.is_some();
    DiagnosticCheck::new(
        "Workspace",
        if in_repo {
            DiagnosticLevel::Ok
        } else {
            DiagnosticLevel::Warn
        },
        if in_repo {
            format!(
                "project root detected on branch {}",
                context.git_branch.as_deref().unwrap_or("unknown")
            )
        } else {
            "current directory is not inside a git project".to_string()
        },
    )
    .with_details(vec![
        format!("Cwd              {}", context.cwd.display()),
        format!(
            "Project root     {}",
            context
                .project_root
                .as_ref()
                .map_or_else(|| "<none>".to_string(), |path| path.display().to_string())
        ),
        format!(
            "Git branch       {}",
            context.git_branch.as_deref().unwrap_or("unknown")
        ),
        format!("Git state        {}", context.git_summary.headline()),
        format!("Changed files    {}", context.git_summary.changed_files),
        format!(
            "Memory files     {} · config files loaded {}/{}",
            context.memory_file_count, context.loaded_config_files, context.discovered_config_files
        ),
    ])
    .with_data(Map::from_iter([
        ("cwd".to_string(), json!(context.cwd.display().to_string())),
        (
            "project_root".to_string(),
            json!(context
                .project_root
                .as_ref()
                .map(|path| path.display().to_string())),
        ),
        ("in_git_repo".to_string(), json!(in_repo)),
        ("git_branch".to_string(), json!(context.git_branch)),
        (
            "git_state".to_string(),
            json!(context.git_summary.headline()),
        ),
        (
            "changed_files".to_string(),
            json!(context.git_summary.changed_files),
        ),
        (
            "memory_file_count".to_string(),
            json!(context.memory_file_count),
        ),
        (
            "loaded_config_files".to_string(),
            json!(context.loaded_config_files),
        ),
        (
            "discovered_config_files".to_string(),
            json!(context.discovered_config_files),
        ),
    ]))
}

fn check_sandbox_health(status: &runtime::SandboxStatus) -> DiagnosticCheck {
    let degraded = status.enabled && !status.active;
    let mut details = vec![
        format!("Enabled          {}", status.enabled),
        format!("Active           {}", status.active),
        format!("Supported        {}", status.supported),
        format!("Filesystem mode  {}", status.filesystem_mode.as_str()),
        format!("Filesystem live  {}", status.filesystem_active),
    ];
    if let Some(reason) = &status.fallback_reason {
        details.push(format!("Fallback reason  {reason}"));
    }
    DiagnosticCheck::new(
        "Sandbox",
        if degraded {
            DiagnosticLevel::Warn
        } else {
            DiagnosticLevel::Ok
        },
        if degraded {
            "sandbox was requested but is not currently active"
        } else if status.active {
            "sandbox protections are active"
        } else {
            "sandbox is not active for this session"
        },
    )
    .with_details(details)
    .with_data(Map::from_iter([
        ("enabled".to_string(), json!(status.enabled)),
        ("active".to_string(), json!(status.active)),
        ("supported".to_string(), json!(status.supported)),
        (
            "namespace_supported".to_string(),
            json!(status.namespace_supported),
        ),
        (
            "namespace_active".to_string(),
            json!(status.namespace_active),
        ),
        (
            "network_supported".to_string(),
            json!(status.network_supported),
        ),
        ("network_active".to_string(), json!(status.network_active)),
        (
            "filesystem_mode".to_string(),
            json!(status.filesystem_mode.as_str()),
        ),
        (
            "filesystem_active".to_string(),
            json!(status.filesystem_active),
        ),
        ("allowed_mounts".to_string(), json!(status.allowed_mounts)),
        ("in_container".to_string(), json!(status.in_container)),
        (
            "container_markers".to_string(),
            json!(status.container_markers),
        ),
        ("fallback_reason".to_string(), json!(status.fallback_reason)),
    ]))
}

fn check_system_health(
    cwd: &Path,
    config: Option<&runtime::RuntimeConfig>,
    build: &BuildInfo,
) -> DiagnosticCheck {
    let default_model = config.and_then(runtime::RuntimeConfig::model);
    let mut details = vec![
        format!("OS               {} {}", env::consts::OS, env::consts::ARCH),
        format!("Working dir      {}", cwd.display()),
        format!("Version          {}", build.version),
        format!(
            "Build target     {}",
            build.build_target.unwrap_or("<unknown>")
        ),
        format!("Git SHA          {}", build.git_sha.unwrap_or("<unknown>")),
    ];
    if let Some(model) = default_model {
        details.push(format!("Default model    {model}"));
    }
    DiagnosticCheck::new(
        "System",
        DiagnosticLevel::Ok,
        "captured local runtime metadata",
    )
    .with_details(details)
    .with_data(Map::from_iter([
        ("os".to_string(), json!(env::consts::OS)),
        ("arch".to_string(), json!(env::consts::ARCH)),
        ("working_dir".to_string(), json!(cwd.display().to_string())),
        ("version".to_string(), json!(build.version)),
        ("build_target".to_string(), json!(build.build_target)),
        ("git_sha".to_string(), json!(build.git_sha)),
        ("default_model".to_string(), json!(default_model)),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_lifecycle_prefers_running_process_over_idle_shell() {
        let workspace = PathBuf::from("/tmp/project");
        let lifecycle = classify_session_lifecycle_from_panes(
            &workspace,
            vec![
                TmuxPaneSnapshot {
                    pane_id: "%1".to_string(),
                    current_command: "zsh".to_string(),
                    current_path: workspace.clone(),
                },
                TmuxPaneSnapshot {
                    pane_id: "%2".to_string(),
                    current_command: "scode".to_string(),
                    current_path: workspace.join("rust"),
                },
            ],
        );

        assert_eq!(lifecycle.kind, SessionLifecycleKind::RunningProcess);
        assert_eq!(lifecycle.pane_id.as_deref(), Some("%2"));
        assert_eq!(lifecycle.pane_command.as_deref(), Some("scode"));
        assert!(!lifecycle.abandoned);
    }

    #[test]
    fn session_lifecycle_marks_dirty_idle_shell_as_abandoned() {
        let workspace = std::env::temp_dir().join(format!(
            "scode-lifecycle-dirty-idle-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis()
        ));
        fs::create_dir_all(&workspace).expect("workspace should create");

        // Set up a git repo with a dirty working tree.
        let git = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(&workspace)
                .output()
                .expect("git should run");
        };
        git(&["init", "--quiet"]);
        git(&["config", "user.email", "tests@example.com"]);
        git(&["config", "user.name", "Sudocode Tests"]);
        fs::write(workspace.join("tracked.txt"), "hello\n").expect("write tracked");
        git(&["add", "tracked.txt"]);
        git(&["commit", "-m", "init", "--quiet"]);
        fs::write(workspace.join("tracked.txt"), "hello\nchanged\n").expect("dirty tracked");

        let lifecycle = classify_session_lifecycle_from_panes(
            &workspace,
            vec![TmuxPaneSnapshot {
                pane_id: "%3".to_string(),
                current_command: "bash".to_string(),
                current_path: workspace.clone(),
            }],
        );

        assert_eq!(lifecycle.kind, SessionLifecycleKind::IdleShell);
        assert!(lifecycle.workspace_dirty);
        assert!(lifecycle.abandoned);

        fs::remove_dir_all(workspace).expect("cleanup temp dir");
    }
}
