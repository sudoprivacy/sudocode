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

/// An owned reply future lets a prompt wait without borrowing the dispatcher.
pub type PromptReply<T> = futures::future::BoxFuture<'static, T>;

/// One input owner per turn, shared by permissions and questions across hosts.
/// Queueing a prompt never holds a tool or provider polling thread.
#[derive(Clone, Default)]
pub struct PromptQueue(std::sync::Arc<tokio::sync::Mutex<()>>);

impl PromptQueue {
    pub fn enqueue<T, F, R>(&self, request: F) -> PromptReply<T>
    where
        T: Send + 'static,
        F: FnOnce() -> R + Send + 'static,
        R: std::future::Future<Output = T> + Send + 'static,
    {
        let queue = self.0.clone();
        Box::pin(async move {
            let _input = queue.lock_owned().await;
            request().await
        })
    }
}

/// Prompting interface used when policy requires interactive approval.
pub trait PermissionPrompter {
    fn decide(&mut self, request: &PermissionRequest) -> PermissionPromptDecision;

    fn begin_decision(
        &mut self,
        request: &PermissionRequest,
    ) -> PromptReply<PermissionPromptDecision> {
        let decision = self.decide(request);
        Box::pin(std::future::ready(decision))
    }
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

/// Human-facing prompt content. Formatting is explicit; shell commands and
/// paths must never be guessed to be Markdown. Clones share the immutable source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptText {
    Plain(std::sync::Arc<str>),
    Markdown(std::sync::Arc<str>),
}

impl PromptText {
    #[must_use]
    pub fn format_name(&self) -> &'static str {
        match self {
            Self::Plain(_) => "plain",
            Self::Markdown(_) => "markdown",
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Plain(text) | Self::Markdown(text) => text,
        }
    }

    /// Constant-time identity for derived presentation caches. An edited source
    /// gets a new allocation; rendering never mutates the source itself.
    #[must_use]
    pub fn same_source(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Plain(a), Self::Plain(b)) | (Self::Markdown(a), Self::Markdown(b)) => {
                std::sync::Arc::ptr_eq(a, b)
            }
            _ => false,
        }
    }
}

impl From<String> for PromptText {
    fn from(text: String) -> Self {
        Self::Plain(text.into())
    }
}

impl From<&str> for PromptText {
    fn from(text: &str) -> Self {
        Self::Plain(text.into())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionPromptRequest {
    pub title: Option<String>,
    pub description: Option<PromptText>,
    pub fields: Vec<QuestionField>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionPromptAnswer {
    pub id: String,
    pub value: String,
    pub label: Option<String>,
}

pub trait QuestionPrompter: Send {
    fn cancel_pending(&mut self) {}
    fn ask(&mut self, request: &QuestionPromptRequest)
        -> Result<Vec<QuestionPromptAnswer>, String>;

    fn begin_question(
        &mut self,
        request: &QuestionPromptRequest,
    ) -> PromptReply<Result<Vec<QuestionPromptAnswer>, String>> {
        let answer = self.ask(request);
        Box::pin(std::future::ready(answer))
    }
}

/// Final authorization result after evaluating static rules and prompts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionOutcome {
    Allow,
    Deny { reason: String },
}

enum PermissionEvaluation {
    Allow,
    Deny { reason: String },
    Prompt(PermissionRequest),
}

impl PermissionEvaluation {
    fn prompt(
        tool_name: &str,
        input: &str,
        current_mode: PermissionMode,
        required_mode: PermissionMode,
        reason: Option<String>,
    ) -> Self {
        Self::Prompt(PermissionRequest {
            tool_name: tool_name.into(),
            input: input.into(),
            current_mode,
            required_mode,
            reason,
        })
    }
}

fn resolve_permission(
    request: &PermissionRequest,
    decision: Option<PermissionPromptDecision>,
) -> PermissionOutcome {
    match decision {
        Some(PermissionPromptDecision::Allow) => PermissionOutcome::Allow,
        Some(PermissionPromptDecision::Deny { reason }) => PermissionOutcome::Deny { reason },
        None => PermissionOutcome::Deny {
            reason: request.reason.clone().unwrap_or_else(|| {
                format!(
                    "tool '{}' requires approval to run while mode is {}",
                    request.tool_name,
                    request.current_mode.as_str()
                )
            }),
        },
    }
}

/// Evaluates permission mode requirements plus allow/deny/ask rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionPolicy {
    active_mode: PermissionMode,
    tool_requirements: BTreeMap<String, PermissionMode>,
    allow_rules: Vec<PermissionRule>,
    memory_allow_rules: Vec<PermissionRule>,
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
            memory_allow_rules: Vec::new(),
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
    /// Writes to configured memory roots are auto-allowed regardless of
    /// permission mode (except `ReadOnly` and deny rules).
    #[must_use]
    pub fn with_memory_allow_rules(mut self, memory_dir: &std::path::Path) -> Self {
        let mut native = memory_dir.join("").to_string_lossy().into_owned();
        let portable = format!("{}/", memory_dir.to_string_lossy().trim_end_matches('/'));
        if native.is_empty() {
            native.clone_from(&portable);
        }
        for prefix in std::iter::once(&native).chain((native != portable).then_some(&portable)) {
            for tool in ["write_file", "edit_file"] {
                self.memory_allow_rules.push(PermissionRule {
                    raw: format!("{tool}({prefix}:*)"),
                    tool_name: tool.to_string(),
                    matcher: PermissionRuleMatcher::Prefix(prefix.clone()),
                });
            }
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

    /// The permission a tool needs, defaulting to the strictest when the tool
    /// is unknown.
    ///
    /// The table is keyed by SPEC name while `tool_name` is whatever the model
    /// spelled, so an unresolved CC spelling does not read as "unknown tool" —
    /// it reads as `DangerFullAccess`, and the call is refused under any mode
    /// short of full access. That is how a `--allowedTools <alias>` for a name
    /// that is no longer a spec of its own comes to dispatch nothing at all:
    /// the gate lets it through and the permission layer, one lookup later,
    /// silently requires more than the session had. Try the name as given
    /// first — a plugin may register a literal name that also happens to be an
    /// alias key — then the canonical tool it names.
    #[must_use]
    pub fn required_mode_for(&self, tool_name: &str) -> PermissionMode {
        self.tool_requirements
            .get(tool_name)
            .or_else(|| {
                self.tool_requirements
                    .get(&crate::tool_names::canonicalize_tool_name(tool_name))
            })
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
    pub fn authorize_with_context(
        &self,
        tool_name: &str,
        input: &str,
        context: &PermissionContext,
        prompter: Option<&mut dyn PermissionPrompter>,
    ) -> PermissionOutcome {
        match self.evaluate(tool_name, input, context) {
            PermissionEvaluation::Allow => PermissionOutcome::Allow,
            PermissionEvaluation::Deny { reason } => PermissionOutcome::Deny { reason },
            PermissionEvaluation::Prompt(request) => {
                resolve_permission(&request, prompter.map(|p| p.decide(&request)))
            }
        }
    }

    pub(crate) fn begin_authorization(
        &self,
        tool_name: &str,
        input: &str,
        context: &PermissionContext,
        prompter: Option<&mut dyn PermissionPrompter>,
    ) -> PromptReply<PermissionOutcome> {
        match self.evaluate(tool_name, input, context) {
            PermissionEvaluation::Allow => Box::pin(std::future::ready(PermissionOutcome::Allow)),
            PermissionEvaluation::Deny { reason } => {
                Box::pin(std::future::ready(PermissionOutcome::Deny { reason }))
            }
            PermissionEvaluation::Prompt(request) => {
                let reply = prompter.map(|p| p.begin_decision(&request));
                Box::pin(async move {
                    let decision = match reply {
                        Some(reply) => Some(reply.await),
                        None => None,
                    };
                    resolve_permission(&request, decision)
                })
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    fn evaluate(
        &self,
        tool_name: &str,
        input: &str,
        context: &PermissionContext,
    ) -> PermissionEvaluation {
        if let Some(rule) = Self::find_matching_rule(&self.deny_rules, tool_name, input) {
            return PermissionEvaluation::Deny {
                reason: format!(
                    "Permission to use {tool_name} has been denied by rule '{}'",
                    rule.raw
                ),
            };
        }

        let current_mode = self.active_mode();
        let required_mode = self.required_mode_for(tool_name);
        let ask_rule = Self::find_matching_rule(&self.ask_rules, tool_name, input);
        let allow_rule =
            Self::find_matching_rule(&self.allow_rules, tool_name, input).or_else(|| {
                // Synthetic memory access must follow live mode switches; user rules
                // retain their explicit semantics and deny/ask still take precedence.
                if current_mode == PermissionMode::ReadOnly {
                    None
                } else {
                    Self::find_matching_rule(&self.memory_allow_rules, tool_name, input)
                }
            });

        match context.override_decision() {
            Some(PermissionOverride::Deny) => {
                return PermissionEvaluation::Deny {
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
                return PermissionEvaluation::prompt(
                    tool_name,
                    input,
                    current_mode,
                    required_mode,
                    Some(reason),
                );
            }
            Some(PermissionOverride::Allow) => {
                if let Some(rule) = ask_rule {
                    let reason = format!(
                        "tool '{tool_name}' requires approval due to ask rule '{}'",
                        rule.raw
                    );
                    return PermissionEvaluation::prompt(
                        tool_name,
                        input,
                        current_mode,
                        required_mode,
                        Some(reason),
                    );
                }
                if allow_rule.is_some()
                    || current_mode == PermissionMode::Allow
                    || (current_mode != PermissionMode::Prompt && current_mode >= required_mode)
                {
                    return PermissionEvaluation::Allow;
                }
            }
            None => {}
        }

        if let Some(rule) = ask_rule {
            let reason = format!(
                "tool '{tool_name}' requires approval due to ask rule '{}'",
                rule.raw
            );
            return PermissionEvaluation::prompt(
                tool_name,
                input,
                current_mode,
                required_mode,
                Some(reason),
            );
        }

        if allow_rule.is_some()
            || current_mode == PermissionMode::Allow
            || (current_mode != PermissionMode::Prompt && current_mode >= required_mode)
        {
            return PermissionEvaluation::Allow;
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
            return PermissionEvaluation::prompt(
                tool_name,
                input,
                current_mode,
                required_mode,
                reason,
            );
        }

        PermissionEvaluation::Deny {
            reason: format!(
                "tool '{tool_name}' requires {} permission; current mode is {}",
                required_mode.as_str(),
                current_mode.as_str()
            ),
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
        if self.tool_name != tool_name {
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

    /// The requirement table is keyed by SPEC name; the model spells tools its
    /// own way. Resolving one against the other is not a nicety — an unmatched
    /// name falls through to `DangerFullAccess`, so a CC-spelled call is not
    /// "unknown", it is REFUSED under every mode below full access, and the
    /// refusal names a permission problem rather than a spelling one.
    #[test]
    fn cc_spelled_tools_resolve_to_their_canonical_requirement() {
        let policy = PermissionPolicy::new(PermissionMode::WorkspaceWrite)
            .with_tool_requirement("pid_status", PermissionMode::ReadOnly)
            .with_tool_requirement("send", PermissionMode::WorkspaceWrite)
            .with_tool_requirement("TodoWrite", PermissionMode::WorkspaceWrite);

        // TodoWrite is a write (whole-list replace of the to-do checklist).
        assert_eq!(
            policy.required_mode_for("TodoWrite"),
            PermissionMode::WorkspaceWrite,
            "`TodoWrite` should require WorkspaceWrite"
        );
        assert_eq!(
            policy.required_mode_for("pid_status"),
            PermissionMode::ReadOnly,
            "`pid_status` should be ReadOnly"
        );
        assert_eq!(
            policy.required_mode_for("SendMessage"),
            PermissionMode::WorkspaceWrite
        );
        // A genuinely unknown tool still gets the strictest default — the
        // fallback resolves spelling, it must not become a way in.
        assert_eq!(
            policy.required_mode_for("MadeUpTool"),
            PermissionMode::DangerFullAccess
        );
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
    fn prompt_mode_requires_a_decision_at_every_tool_level() {
        for required in [
            PermissionMode::ReadOnly,
            PermissionMode::WorkspaceWrite,
            PermissionMode::DangerFullAccess,
        ] {
            let policy = PermissionPolicy::new(PermissionMode::Prompt)
                .with_tool_requirement("test_tool", required);
            for hook in [None, Some(PermissionOverride::Allow)] {
                let context = PermissionContext::new(hook, None);
                for allow in [true, false] {
                    let mut prompter = RecordingPrompter {
                        seen: Vec::new(),
                        allow,
                    };
                    let outcome = policy.authorize_with_context(
                        "test_tool",
                        "{}",
                        &context,
                        Some(&mut prompter),
                    );
                    assert_eq!(prompter.seen.len(), 1, "{required:?}, hook={hook:?}");
                    assert_eq!(
                        outcome,
                        if allow {
                            PermissionOutcome::Allow
                        } else {
                            PermissionOutcome::Deny {
                                reason: "not now".into(),
                            }
                        }
                    );
                }
                assert!(
                    matches!(
                        policy.authorize_with_context("test_tool", "{}", &context, None),
                        PermissionOutcome::Deny { .. }
                    ),
                    "missing approver must fail closed"
                );
            }
        }
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
