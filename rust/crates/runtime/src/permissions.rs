use std::collections::BTreeMap;

use serde_json::Value;

use crate::config::RuntimePermissionRuleConfig;

/// Permission level assigned to a tool invocation or runtime session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PermissionMode {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
    Prompt,
    Allow,
}

impl PermissionMode {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::DangerFullAccess => "danger-full-access",
            Self::Prompt => "prompt",
            Self::Allow => "allow",
        }
    }
}

/// Hook-provided override applied before standard permission evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionOverride {
    Allow,
    Deny,
    Ask,
}

/// Additional permission context supplied by hooks or higher-level orchestration.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PermissionContext {
    override_decision: Option<PermissionOverride>,
    override_reason: Option<String>,
}

impl PermissionContext {
    #[must_use]
    pub fn new(
        override_decision: Option<PermissionOverride>,
        override_reason: Option<String>,
    ) -> Self {
        Self {
            override_decision,
            override_reason,
        }
    }

    #[must_use]
    pub fn override_decision(&self) -> Option<PermissionOverride> {
        self.override_decision
    }

    #[must_use]
    pub fn override_reason(&self) -> Option<&str> {
        self.override_reason.as_deref()
    }
}

/// Full authorization request presented to a permission prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionRequest {
    pub tool_name: String,
    pub input: String,
    pub current_mode: PermissionMode,
    pub required_mode: PermissionMode,
    pub reason: Option<String>,
}

/// User-facing decision returned by a [`PermissionPrompter`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionPromptDecision {
    Allow,
    Deny { reason: String },
}

/// Prompting interface used when policy requires interactive approval.
pub trait PermissionPrompter {
    fn decide(&mut self, request: &PermissionRequest) -> PermissionPromptDecision;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionOption {
    pub label: String,
    pub value: String,
    pub description: Option<String>,
    pub recommended: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionField {
    pub id: String,
    pub prompt: String,
    pub kind: QuestionKind,
    pub required: bool,
    pub allow_custom_input: bool,
    pub custom_input_hint: Option<String>,
    pub options: Vec<QuestionOption>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuestionKind {
    SingleSelect,
    MultiSelect,
    Text,
    Boolean,
}

impl QuestionKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            QuestionKind::SingleSelect => "single_select",
            QuestionKind::MultiSelect => "multi_select",
            QuestionKind::Text => "text",
            QuestionKind::Boolean => "boolean",
        }
    }

    pub fn from_str(value: &str) -> Option<Self> {
        match value {
            "single_select" => Some(QuestionKind::SingleSelect),
            "multi_select" => Some(QuestionKind::MultiSelect),
            "text" => Some(QuestionKind::Text),
            "boolean" => Some(QuestionKind::Boolean),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionPromptRequest {
    pub title: Option<String>,
    pub description: Option<String>,
    pub fields: Vec<QuestionField>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionPromptAnswer {
    pub id: String,
    pub value: String,
    pub label: Option<String>,
}

pub trait QuestionPrompter: Send {
    fn ask(&mut self, request: &QuestionPromptRequest)
        -> Result<Vec<QuestionPromptAnswer>, String>;
}

/// Final authorization result after evaluating static rules and prompts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionOutcome {
    Allow,
    Deny { reason: String },
}

/// Evaluates permission mode requirements plus allow/deny/ask rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionPolicy {
    active_mode: PermissionMode,
    tool_requirements: BTreeMap<String, PermissionMode>,
    allow_rules: Vec<PermissionRule>,
    deny_rules: Vec<PermissionRule>,
    ask_rules: Vec<PermissionRule>,
}

impl PermissionPolicy {
    #[must_use]
    pub fn new(active_mode: PermissionMode) -> Self {
        Self {
            active_mode,
            tool_requirements: BTreeMap::new(),
            allow_rules: Vec::new(),
            deny_rules: Vec::new(),
            ask_rules: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_tool_requirement(
        mut self,
        tool_name: impl Into<String>,
        required_mode: PermissionMode,
    ) -> Self {
        self.tool_requirements
            .insert(tool_name.into(), required_mode);
        self
    }

    #[must_use]
    pub fn with_permission_rules(mut self, config: &RuntimePermissionRuleConfig) -> Self {
        self.allow_rules = config
            .allow()
            .iter()
            .map(|rule| PermissionRule::parse(rule))
            .collect();
        self.deny_rules = config
            .deny()
            .iter()
            .map(|rule| PermissionRule::parse(rule))
            .collect();
        self.ask_rules = config
            .ask()
            .iter()
            .map(|rule| PermissionRule::parse(rule))
            .collect();
        self
    }

    /// Inject synthetic allow rules for the auto-memory directory.
    /// CC carve-out: writes to `~/.scode/projects/<slug>/memory/` are auto-allowed
    /// regardless of permission mode (except ReadOnly and deny rules).
    #[must_use]
    pub fn with_memory_allow_rules(mut self, memory_dir: &std::path::Path) -> Self {
        let prefix = memory_dir.to_string_lossy();
        let dir_prefix = if prefix.ends_with('/') {
            prefix.to_string()
        } else {
            format!("{}/", prefix)
        };
        for tool in &["write_file", "edit_file"] {
            self.allow_rules.push(PermissionRule {
                raw: format!("{tool}({dir_prefix}:*)"),
                tool_name: tool.to_string(),
                matcher: PermissionRuleMatcher::Prefix(dir_prefix.clone()),
            });
        }
        self
    }

    #[must_use]
    pub fn active_mode(&self) -> PermissionMode {
        self.active_mode
    }

    /// Change the active permission mode at runtime.
    pub fn set_active_mode(&mut self, mode: PermissionMode) {
        self.active_mode = mode;
    }

    #[must_use]
    pub fn required_mode_for(&self, tool_name: &str) -> PermissionMode {
        self.tool_requirements
            .get(tool_name)
            .copied()
            .unwrap_or(PermissionMode::DangerFullAccess)
    }

    #[must_use]
    pub fn authorize(
        &self,
        tool_name: &str,
        input: &str,
        prompter: Option<&mut dyn PermissionPrompter>,
    ) -> PermissionOutcome {
        self.authorize_with_context(tool_name, input, &PermissionContext::default(), prompter)
    }

    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn authorize_with_context(
        &self,
        tool_name: &str,
        input: &str,
        context: &PermissionContext,
        prompter: Option<&mut dyn PermissionPrompter>,
    ) -> PermissionOutcome {
        if let Some(rule) = Self::find_matching_rule(&self.deny_rules, tool_name, input) {
            return PermissionOutcome::Deny {
                reason: format!(
                    "Permission to use {tool_name} has been denied by rule '{}'",
                    rule.raw
                ),
            };
        }

        let current_mode = self.active_mode();
        // A file tool targeting a path outside the workspace requires
        // danger-full-access, regardless of its static requirement — otherwise
        // read-only could read /etc/passwd and workspace-write could write any
        // absolute path. This is the CLI's only enforcement gate (it has no
        // dispatch-time enforcer), so it must run here where the input is known.
        let required_mode = escalate_required_mode_for_path_scope(
            tool_name,
            input,
            self.required_mode_for(tool_name),
        );
        let ask_rule = Self::find_matching_rule(&self.ask_rules, tool_name, input);
        let allow_rule = Self::find_matching_rule(&self.allow_rules, tool_name, input);

        match context.override_decision() {
            Some(PermissionOverride::Deny) => {
                return PermissionOutcome::Deny {
                    reason: context.override_reason().map_or_else(
                        || format!("tool '{tool_name}' denied by hook"),
                        ToOwned::to_owned,
                    ),
                };
            }
            Some(PermissionOverride::Ask) => {
                let reason = context.override_reason().map_or_else(
                    || format!("tool '{tool_name}' requires approval due to hook guidance"),
                    ToOwned::to_owned,
                );
                return Self::prompt_or_deny(
                    tool_name,
                    input,
                    current_mode,
                    required_mode,
                    Some(reason),
                    prompter,
                );
            }
            Some(PermissionOverride::Allow) => {
                if let Some(rule) = ask_rule {
                    let reason = format!(
                        "tool '{tool_name}' requires approval due to ask rule '{}'",
                        rule.raw
                    );
                    return Self::prompt_or_deny(
                        tool_name,
                        input,
                        current_mode,
                        required_mode,
                        Some(reason),
                        prompter,
                    );
                }
                if allow_rule.is_some()
                    || current_mode == PermissionMode::Allow
                    || current_mode >= required_mode
                {
                    return PermissionOutcome::Allow;
                }
            }
            None => {}
        }

        if let Some(rule) = ask_rule {
            let reason = format!(
                "tool '{tool_name}' requires approval due to ask rule '{}'",
                rule.raw
            );
            return Self::prompt_or_deny(
                tool_name,
                input,
                current_mode,
                required_mode,
                Some(reason),
                prompter,
            );
        }

        if allow_rule.is_some()
            || current_mode == PermissionMode::Allow
            || current_mode >= required_mode
        {
            return PermissionOutcome::Allow;
        }

        if current_mode == PermissionMode::Prompt
            || (current_mode == PermissionMode::WorkspaceWrite
                && required_mode == PermissionMode::DangerFullAccess)
        {
            let reason = Some(format!(
                "tool '{tool_name}' requires approval to escalate from {} to {}",
                current_mode.as_str(),
                required_mode.as_str()
            ));
            return Self::prompt_or_deny(
                tool_name,
                input,
                current_mode,
                required_mode,
                reason,
                prompter,
            );
        }

        PermissionOutcome::Deny {
            reason: format!(
                "tool '{tool_name}' requires {} permission; current mode is {}",
                required_mode.as_str(),
                current_mode.as_str()
            ),
        }
    }

    fn prompt_or_deny(
        tool_name: &str,
        input: &str,
        current_mode: PermissionMode,
        required_mode: PermissionMode,
        reason: Option<String>,
        mut prompter: Option<&mut dyn PermissionPrompter>,
    ) -> PermissionOutcome {
        let request = PermissionRequest {
            tool_name: tool_name.to_string(),
            input: input.to_string(),
            current_mode,
            required_mode,
            reason: reason.clone(),
        };

        match prompter.as_mut() {
            Some(prompter) => match prompter.decide(&request) {
                PermissionPromptDecision::Allow => PermissionOutcome::Allow,
                PermissionPromptDecision::Deny { reason } => PermissionOutcome::Deny { reason },
            },
            None => PermissionOutcome::Deny {
                reason: reason.unwrap_or_else(|| {
                    format!(
                        "tool '{tool_name}' requires approval to run while mode is {}",
                        current_mode.as_str()
                    )
                }),
            },
        }
    }

    fn find_matching_rule<'a>(
        rules: &'a [PermissionRule],
        tool_name: &str,
        input: &str,
    ) -> Option<&'a PermissionRule> {
        rules.iter().find(|rule| rule.matches(tool_name, input))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PermissionRule {
    pub(crate) raw: String,
    pub(crate) tool_name: String,
    pub(crate) matcher: PermissionRuleMatcher,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PermissionRuleMatcher {
    Any,
    Exact(String),
    Prefix(String),
}

impl PermissionRule {
    fn parse(raw: &str) -> Self {
        let trimmed = raw.trim();
        let open = find_first_unescaped(trimmed, '(');
        let close = find_last_unescaped(trimmed, ')');

        if let (Some(open), Some(close)) = (open, close) {
            if close == trimmed.len() - 1 && open < close {
                let tool_name = trimmed[..open].trim();
                let content = &trimmed[open + 1..close];
                if !tool_name.is_empty() {
                    let matcher = parse_rule_matcher(content);
                    return Self {
                        raw: trimmed.to_string(),
                        tool_name: tool_name.to_string(),
                        matcher,
                    };
                }
            }
        }

        Self {
            raw: trimmed.to_string(),
            tool_name: trimmed.to_string(),
            matcher: PermissionRuleMatcher::Any,
        }
    }

    fn matches(&self, tool_name: &str, input: &str) -> bool {
        // Case-insensitive so a `Bash(rm:*)` deny rule matches the runtime
        // `bash`. Not lower-cased at parse time: several tools are
        // PascalCase-native (`WebFetch`, `NotebookEdit`, …).
        if !self.tool_name.eq_ignore_ascii_case(tool_name) {
            return false;
        }

        match &self.matcher {
            PermissionRuleMatcher::Any => true,
            PermissionRuleMatcher::Exact(expected) => {
                extract_permission_subject(input).is_some_and(|candidate| candidate == *expected)
            }
            PermissionRuleMatcher::Prefix(prefix) => extract_permission_subject(input)
                .is_some_and(|candidate| candidate.starts_with(prefix)),
        }
    }
}

fn parse_rule_matcher(content: &str) -> PermissionRuleMatcher {
    let unescaped = unescape_rule_content(content.trim());
    if unescaped.is_empty() || unescaped == "*" {
        PermissionRuleMatcher::Any
    } else if let Some(prefix) = unescaped.strip_suffix(":*") {
        PermissionRuleMatcher::Prefix(prefix.to_string())
    } else {
        PermissionRuleMatcher::Exact(unescaped)
    }
}

fn unescape_rule_content(content: &str) -> String {
    content
        .replace(r"\(", "(")
        .replace(r"\)", ")")
        .replace(r"\\", r"\")
}

fn find_first_unescaped(value: &str, needle: char) -> Option<usize> {
    let mut escaped = false;
    for (idx, ch) in value.char_indices() {
        if ch == '\\' {
            escaped = !escaped;
            continue;
        }
        if ch == needle && !escaped {
            return Some(idx);
        }
        escaped = false;
    }
    None
}

fn find_last_unescaped(value: &str, needle: char) -> Option<usize> {
    let chars = value.char_indices().collect::<Vec<_>>();
    for (pos, (idx, ch)) in chars.iter().enumerate().rev() {
        if *ch != needle {
            continue;
        }
        let mut backslashes = 0;
        for (_, prev) in chars[..pos].iter().rev() {
            if *prev == '\\' {
                backslashes += 1;
            } else {
                break;
            }
        }
        if backslashes % 2 == 0 {
            return Some(*idx);
        }
    }
    None
}

/// Escalate a file tool's required mode to danger-full-access when its target
/// resolves outside the workspace. The root is the process CWD, correct for the
/// standalone CLI (`StdFsBackend`); VFS backends always run at `Allow` /
/// `DangerFullAccess`, where the escalation is a no-op.
fn escalate_required_mode_for_path_scope(
    tool_name: &str,
    input: &str,
    base: PermissionMode,
) -> PermissionMode {
    if !is_path_scoped_tool(tool_name) {
        return base;
    }
    let Some(path) = extract_path_subject(input) else {
        return base;
    };
    if path_resolves_outside_workspace(&path) {
        PermissionMode::DangerFullAccess
    } else {
        base
    }
}

/// Canonical names plus the CC-style aliases the model may emit (`Read`, …).
fn is_path_scoped_tool(tool_name: &str) -> bool {
    matches!(
        tool_name.to_ascii_lowercase().replace('-', "_").as_str(),
        "read_file"
            | "read"
            | "write_file"
            | "write"
            | "edit_file"
            | "edit"
            | "glob_search"
            | "glob"
            | "grep_search"
            | "grep"
    )
}

/// Pull the path field from a file tool's input. Only genuine path fields, and
/// no raw-input fallback, so a non-path argument yields `None` (no escalation).
fn extract_path_subject(input: &str) -> Option<String> {
    let Ok(Value::Object(object)) = serde_json::from_str::<Value>(input) else {
        return None;
    };
    for key in [
        "path",
        "file_path",
        "filePath",
        "notebook_path",
        "notebookPath",
        "pattern", // glob_search, when no explicit base path
    ] {
        if let Some(value) = object.get(key).and_then(Value::as_str) {
            return Some(value.to_string());
        }
    }
    None
}

/// True when `path` resolves outside the workspace (process CWD), resolving
/// symlinks and `..`. Fails closed (reports "outside") when the CWD is unknown.
fn path_resolves_outside_workspace(path: &str) -> bool {
    use std::path::{Component, Path};

    // Strip shell-ish wrapping punctuation a model might include.
    let trimmed = path.trim().trim_matches(|ch: char| {
        matches!(
            ch,
            '"' | '\'' | '`' | ',' | ';' | ')' | '(' | '[' | ']' | '{' | '}'
        )
    });
    if trimmed.is_empty() {
        return false;
    }

    let Ok(cwd_raw) = std::env::current_dir() else {
        return true; // fail closed
    };
    let cwd = cwd_raw.canonicalize().unwrap_or(cwd_raw);

    let candidate = Path::new(trimmed);
    let absolute = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        cwd.join(candidate)
    };

    let resolved = canonicalize_best_effort(&absolute);

    // If canonicalization fell back to a literal path (missing leaf), a lexical
    // `..` climbing above the workspace still escapes it.
    let has_parent_escape = {
        let mut depth: i32 = 0;
        let mut escapes = false;
        for comp in resolved.components() {
            match comp {
                Component::ParentDir => {
                    depth -= 1;
                    if depth < 0 {
                        escapes = true;
                    }
                }
                Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
                Component::Normal(_) => depth += 1,
            }
        }
        escapes
    };

    has_parent_escape || !resolved.starts_with(&cwd)
}

/// Canonicalize `path` if it exists; otherwise canonicalize its deepest
/// existing ancestor and re-attach the trailing (not-yet-created) components.
fn canonicalize_best_effort(path: &std::path::Path) -> std::path::PathBuf {
    if let Ok(resolved) = path.canonicalize() {
        return resolved;
    }
    let mut tail = Vec::new();
    let mut current = path;
    while let Some(parent) = current.parent() {
        if let Some(name) = current.file_name() {
            tail.push(name.to_owned());
        }
        if let Ok(resolved_parent) = parent.canonicalize() {
            let mut out = resolved_parent;
            for name in tail.iter().rev() {
                out.push(name);
            }
            return out;
        }
        current = parent;
    }
    path.to_path_buf()
}

fn extract_permission_subject(input: &str) -> Option<String> {
    let parsed = serde_json::from_str::<Value>(input).ok();
    if let Some(Value::Object(object)) = parsed {
        for key in [
            "command",
            "path",
            "file_path",
            "filePath",
            "notebook_path",
            "notebookPath",
            "url",
            "pattern",
            "code",
            "message",
        ] {
            if let Some(value) = object.get(key).and_then(Value::as_str) {
                return Some(value.to_string());
            }
        }
    }

    (!input.trim().is_empty()).then(|| input.to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        PermissionContext, PermissionMode, PermissionOutcome, PermissionOverride, PermissionPolicy,
        PermissionPromptDecision, PermissionPrompter, PermissionRequest,
    };
    use crate::config::RuntimePermissionRuleConfig;

    struct RecordingPrompter {
        seen: Vec<PermissionRequest>,
        allow: bool,
    }

    impl PermissionPrompter for RecordingPrompter {
        fn decide(&mut self, request: &PermissionRequest) -> PermissionPromptDecision {
            self.seen.push(request.clone());
            if self.allow {
                PermissionPromptDecision::Allow
            } else {
                PermissionPromptDecision::Deny {
                    reason: "not now".to_string(),
                }
            }
        }
    }

    #[test]
    fn allows_tools_when_active_mode_meets_requirement() {
        let policy = PermissionPolicy::new(PermissionMode::WorkspaceWrite)
            .with_tool_requirement("read_file", PermissionMode::ReadOnly)
            .with_tool_requirement("write_file", PermissionMode::WorkspaceWrite);

        assert_eq!(
            policy.authorize("read_file", "{}", None),
            PermissionOutcome::Allow
        );
        assert_eq!(
            policy.authorize("write_file", "{}", None),
            PermissionOutcome::Allow
        );
    }

    #[test]
    fn denies_read_only_escalations_without_prompt() {
        let policy = PermissionPolicy::new(PermissionMode::ReadOnly)
            .with_tool_requirement("write_file", PermissionMode::WorkspaceWrite)
            .with_tool_requirement("bash", PermissionMode::DangerFullAccess);

        assert!(matches!(
            policy.authorize("write_file", "{}", None),
            PermissionOutcome::Deny { reason } if reason.contains("requires workspace-write permission")
        ));
        assert!(matches!(
            policy.authorize("bash", "{}", None),
            PermissionOutcome::Deny { reason } if reason.contains("requires danger-full-access permission")
        ));
    }

    #[test]
    fn prompts_for_workspace_write_to_danger_full_access_escalation() {
        let policy = PermissionPolicy::new(PermissionMode::WorkspaceWrite)
            .with_tool_requirement("bash", PermissionMode::DangerFullAccess);
        let mut prompter = RecordingPrompter {
            seen: Vec::new(),
            allow: true,
        };

        let outcome = policy.authorize("bash", "echo hi", Some(&mut prompter));

        assert_eq!(outcome, PermissionOutcome::Allow);
        assert_eq!(prompter.seen.len(), 1);
        assert_eq!(prompter.seen[0].tool_name, "bash");
        assert_eq!(
            prompter.seen[0].current_mode,
            PermissionMode::WorkspaceWrite
        );
        assert_eq!(
            prompter.seen[0].required_mode,
            PermissionMode::DangerFullAccess
        );
    }

    #[test]
    fn honors_prompt_rejection_reason() {
        let policy = PermissionPolicy::new(PermissionMode::WorkspaceWrite)
            .with_tool_requirement("bash", PermissionMode::DangerFullAccess);
        let mut prompter = RecordingPrompter {
            seen: Vec::new(),
            allow: false,
        };

        assert!(matches!(
            policy.authorize("bash", "echo hi", Some(&mut prompter)),
            PermissionOutcome::Deny { reason } if reason == "not now"
        ));
    }

    #[test]
    fn applies_rule_based_denials_and_allows() {
        let rules = RuntimePermissionRuleConfig::new(
            vec!["bash(git:*)".to_string()],
            vec!["bash(rm -rf:*)".to_string()],
            Vec::new(),
        );
        let policy = PermissionPolicy::new(PermissionMode::ReadOnly)
            .with_tool_requirement("bash", PermissionMode::DangerFullAccess)
            .with_permission_rules(&rules);

        assert_eq!(
            policy.authorize("bash", r#"{"command":"git status"}"#, None),
            PermissionOutcome::Allow
        );
        assert!(matches!(
            policy.authorize("bash", r#"{"command":"rm -rf /tmp/x"}"#, None),
            PermissionOutcome::Deny { reason } if reason.contains("denied by rule")
        ));
    }

    #[test]
    fn ask_rules_force_prompt_even_when_mode_allows() {
        let rules = RuntimePermissionRuleConfig::new(
            Vec::new(),
            Vec::new(),
            vec!["bash(git:*)".to_string()],
        );
        let policy = PermissionPolicy::new(PermissionMode::DangerFullAccess)
            .with_tool_requirement("bash", PermissionMode::DangerFullAccess)
            .with_permission_rules(&rules);
        let mut prompter = RecordingPrompter {
            seen: Vec::new(),
            allow: true,
        };

        let outcome = policy.authorize("bash", r#"{"command":"git status"}"#, Some(&mut prompter));

        assert_eq!(outcome, PermissionOutcome::Allow);
        assert_eq!(prompter.seen.len(), 1);
        assert!(prompter.seen[0]
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("ask rule")));
    }

    #[test]
    fn hook_allow_still_respects_ask_rules() {
        let rules = RuntimePermissionRuleConfig::new(
            Vec::new(),
            Vec::new(),
            vec!["bash(git:*)".to_string()],
        );
        let policy = PermissionPolicy::new(PermissionMode::ReadOnly)
            .with_tool_requirement("bash", PermissionMode::DangerFullAccess)
            .with_permission_rules(&rules);
        let context = PermissionContext::new(
            Some(PermissionOverride::Allow),
            Some("hook approved".to_string()),
        );
        let mut prompter = RecordingPrompter {
            seen: Vec::new(),
            allow: true,
        };

        let outcome = policy.authorize_with_context(
            "bash",
            r#"{"command":"git status"}"#,
            &context,
            Some(&mut prompter),
        );

        assert_eq!(outcome, PermissionOutcome::Allow);
        assert_eq!(prompter.seen.len(), 1);
    }

    #[test]
    fn hook_deny_short_circuits_permission_flow() {
        let policy = PermissionPolicy::new(PermissionMode::DangerFullAccess)
            .with_tool_requirement("bash", PermissionMode::DangerFullAccess);
        let context = PermissionContext::new(
            Some(PermissionOverride::Deny),
            Some("blocked by hook".to_string()),
        );

        assert_eq!(
            policy.authorize_with_context("bash", "{}", &context, None),
            PermissionOutcome::Deny {
                reason: "blocked by hook".to_string(),
            }
        );
    }

    // --- Case-insensitive deny/allow rule matching ---

    #[test]
    fn deny_rule_matches_pascalcase_tool_name() {
        // `Bash(rm -rf:*)` deny rule must fire against the runtime `bash`.
        let rules = RuntimePermissionRuleConfig::new(
            Vec::new(),
            vec!["Bash(rm -rf:*)".to_string()],
            Vec::new(),
        );
        let policy = PermissionPolicy::new(PermissionMode::DangerFullAccess)
            .with_tool_requirement("bash", PermissionMode::DangerFullAccess)
            .with_permission_rules(&rules);

        assert!(matches!(
            policy.authorize("bash", r#"{"command":"rm -rf /tmp/x"}"#, None),
            PermissionOutcome::Deny { reason } if reason.contains("denied by rule")
        ));
    }

    #[test]
    fn deny_rule_lowercase_matches_pascalcase_call() {
        // Symmetric: lowercase rule, PascalCase incoming tool name.
        let rules = RuntimePermissionRuleConfig::new(
            Vec::new(),
            vec!["read_file(/etc/passwd)".to_string()],
            Vec::new(),
        );
        let policy = PermissionPolicy::new(PermissionMode::DangerFullAccess)
            .with_tool_requirement("read_file", PermissionMode::ReadOnly)
            .with_permission_rules(&rules);

        assert!(matches!(
            policy.authorize("Read_File", r#"{"path":"/etc/passwd"}"#, None),
            PermissionOutcome::Deny { reason } if reason.contains("denied by rule")
        ));
    }

    #[test]
    fn deny_rule_still_rejects_unrelated_tool() {
        // Case-insensitivity must not make a `Bash` rule match `write_file`.
        let rules = RuntimePermissionRuleConfig::new(
            Vec::new(),
            vec!["Bash(rm:*)".to_string()],
            Vec::new(),
        );
        let policy = PermissionPolicy::new(PermissionMode::DangerFullAccess)
            .with_tool_requirement("write_file", PermissionMode::WorkspaceWrite)
            .with_permission_rules(&rules);

        assert_eq!(
            policy.authorize("write_file", r#"{"path":"a.txt","content":"x"}"#, None),
            PermissionOutcome::Allow
        );
    }

    // --- File path-scope escalation ---

    fn in_workspace_path(leaf: &str) -> String {
        std::env::current_dir()
            .expect("cwd")
            .join(leaf)
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn path_scope_classifies_outside_and_inside() {
        assert!(super::path_resolves_outside_workspace("/etc/passwd"));
        assert!(super::path_resolves_outside_workspace("/tmp"));
        assert!(!super::path_resolves_outside_workspace(&in_workspace_path(
            "src/lib.rs"
        )));
        assert!(!super::path_resolves_outside_workspace("src/lib.rs"));
        // Relative traversal that climbs above the workspace escapes it.
        assert!(super::path_resolves_outside_workspace("../../etc/passwd"));
    }

    #[test]
    fn read_only_denies_out_of_workspace_read() {
        let policy = PermissionPolicy::new(PermissionMode::ReadOnly)
            .with_tool_requirement("read_file", PermissionMode::ReadOnly);

        assert!(matches!(
            policy.authorize("read_file", r#"{"path":"/etc/passwd"}"#, None),
            PermissionOutcome::Deny { reason } if reason.contains("danger-full-access")
        ));
    }

    #[test]
    fn read_only_allows_in_workspace_read() {
        let policy = PermissionPolicy::new(PermissionMode::ReadOnly)
            .with_tool_requirement("read_file", PermissionMode::ReadOnly);
        let input = format!(r#"{{"path":"{}"}}"#, in_workspace_path("Cargo.toml"));

        assert_eq!(
            policy.authorize("read_file", &input, None),
            PermissionOutcome::Allow
        );
    }

    #[test]
    fn workspace_write_denies_out_of_workspace_write() {
        // workspace-write must not write absolute paths outside the workspace.
        let policy = PermissionPolicy::new(PermissionMode::WorkspaceWrite)
            .with_tool_requirement("write_file", PermissionMode::WorkspaceWrite);

        assert!(matches!(
            policy.authorize(
                "write_file",
                r#"{"path":"/etc/cron.d/evil","content":"x"}"#,
                None
            ),
            PermissionOutcome::Deny { reason } if reason.contains("danger-full-access")
        ));
    }

    #[test]
    fn workspace_write_allows_in_workspace_write() {
        let policy = PermissionPolicy::new(PermissionMode::WorkspaceWrite)
            .with_tool_requirement("write_file", PermissionMode::WorkspaceWrite);
        let input = format!(
            r#"{{"path":"{}","content":"x"}}"#,
            in_workspace_path("scratch_out.txt")
        );

        assert_eq!(
            policy.authorize("write_file", &input, None),
            PermissionOutcome::Allow
        );
    }

    #[test]
    fn danger_full_access_reads_anything() {
        // Default CLI mode: escalation must be a no-op.
        let policy = PermissionPolicy::new(PermissionMode::DangerFullAccess)
            .with_tool_requirement("read_file", PermissionMode::ReadOnly);

        assert_eq!(
            policy.authorize("read_file", r#"{"path":"/etc/passwd"}"#, None),
            PermissionOutcome::Allow
        );
    }

    #[test]
    fn out_of_workspace_write_prompts_when_prompter_present() {
        let policy = PermissionPolicy::new(PermissionMode::WorkspaceWrite)
            .with_tool_requirement("write_file", PermissionMode::WorkspaceWrite);
        let mut prompter = RecordingPrompter {
            seen: Vec::new(),
            allow: true,
        };

        let outcome = policy.authorize(
            "write_file",
            r#"{"path":"/etc/cron.d/evil","content":"x"}"#,
            Some(&mut prompter),
        );

        assert_eq!(outcome, PermissionOutcome::Allow);
        assert_eq!(prompter.seen.len(), 1);
        assert_eq!(
            prompter.seen[0].required_mode,
            PermissionMode::DangerFullAccess
        );
    }

    #[test]
    fn hook_ask_forces_prompt() {
        let policy = PermissionPolicy::new(PermissionMode::DangerFullAccess)
            .with_tool_requirement("bash", PermissionMode::DangerFullAccess);
        let context = PermissionContext::new(
            Some(PermissionOverride::Ask),
            Some("hook requested confirmation".to_string()),
        );
        let mut prompter = RecordingPrompter {
            seen: Vec::new(),
            allow: true,
        };

        let outcome = policy.authorize_with_context("bash", "{}", &context, Some(&mut prompter));

        assert_eq!(outcome, PermissionOutcome::Allow);
        assert_eq!(prompter.seen.len(), 1);
        assert_eq!(
            prompter.seen[0].reason.as_deref(),
            Some("hook requested confirmation")
        );
    }
}
