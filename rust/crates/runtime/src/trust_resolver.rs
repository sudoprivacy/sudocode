use std::path::{Path, PathBuf};

const TRUST_PROMPT_CUES: &[&str] = &[
    "do you trust the files in this folder",
    "trust the files in this folder",
    "trust this folder",
    "allow and continue",
    "yes, proceed",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustPolicy {
    AutoTrust,
    RequireApproval,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustEvent {
    TrustRequired { cwd: String },
    TrustResolved { cwd: String, policy: TrustPolicy },
    TrustDenied { cwd: String, reason: String },
}

#[derive(Debug, Clone, Default)]
pub struct TrustConfig {
    allowlisted: Vec<PathBuf>,
    denied: Vec<PathBuf>,
}

impl TrustConfig {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_allowlisted(mut self, path: impl Into<PathBuf>) -> Self {
        self.allowlisted.push(path.into());
        self
    }

    #[must_use]
    pub fn with_denied(mut self, path: impl Into<PathBuf>) -> Self {
        self.denied.push(path.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustDecision {
    NotRequired,
    Required {
        policy: TrustPolicy,
        events: Vec<TrustEvent>,
    },
}

impl TrustDecision {
    #[must_use]
    pub fn policy(&self) -> Option<TrustPolicy> {
        match self {
            Self::NotRequired => None,
            Self::Required { policy, .. } => Some(*policy),
        }
    }

    #[must_use]
    pub fn events(&self) -> &[TrustEvent] {
        match self {
            Self::NotRequired => &[],
            Self::Required { events, .. } => events,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TrustResolver {
    config: TrustConfig,
}

impl TrustResolver {
    #[must_use]
    pub fn new(config: TrustConfig) -> Self {
        Self { config }
    }

    #[must_use]
    pub fn resolve(&self, cwd: &str, screen_text: &str) -> TrustDecision {
        if !detect_trust_prompt(screen_text) {
            return TrustDecision::NotRequired;
        }

        let mut events = vec![TrustEvent::TrustRequired {
            cwd: cwd.to_owned(),
        }];

        if let Some(matched_root) = self
            .config
            .denied
            .iter()
            .find(|root| path_matches(cwd, root))
        {
            let reason = format!("cwd matches denied trust root: {}", matched_root.display());
            events.push(TrustEvent::TrustDenied {
                cwd: cwd.to_owned(),
                reason,
            });
            return TrustDecision::Required {
                policy: TrustPolicy::Deny,
                events,
            };
        }

        if self
            .config
            .allowlisted
            .iter()
            .any(|root| path_matches(cwd, root))
        {
            events.push(TrustEvent::TrustResolved {
                cwd: cwd.to_owned(),
                policy: TrustPolicy::AutoTrust,
            });
            return TrustDecision::Required {
                policy: TrustPolicy::AutoTrust,
                events,
            };
        }

        TrustDecision::Required {
            policy: TrustPolicy::RequireApproval,
            events,
        }
    }

    #[must_use]
    pub fn trusts(&self, cwd: &str) -> bool {
        !self
            .config
            .denied
            .iter()
            .any(|root| path_matches(cwd, root))
            && self
                .config
                .allowlisted
                .iter()
                .any(|root| path_matches(cwd, root))
    }
}

#[must_use]
pub fn detect_trust_prompt(screen_text: &str) -> bool {
    let lowered = screen_text.to_ascii_lowercase();
    TRUST_PROMPT_CUES
        .iter()
        .any(|needle| lowered.contains(needle))
}

#[must_use]
pub fn path_matches_trusted_root(cwd: &str, trusted_root: &str) -> bool {
    path_matches(cwd, &normalize_path(Path::new(trusted_root)))
}

fn path_matches(candidate: &str, root: &Path) -> bool {
    let candidate = normalize_path(Path::new(candidate));
    let root = normalize_path(root);
    candidate == root || candidate.starts_with(&root)
}

fn normalize_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::{
        detect_trust_prompt, path_matches_trusted_root, TrustConfig, TrustDecision, TrustEvent,
        TrustPolicy, TrustResolver,
    };

    #[test]
    fn detects_known_trust_prompt_copy() {
        // given
        let screen_text = "Do you trust the files in this folder?\n1. Yes, proceed\n2. No";

        // when
        let detected = detect_trust_prompt(screen_text);

        // then
        assert!(detected);
    }

    #[test]
    fn does_not_emit_events_when_prompt_is_absent() {
        // given
        let resolver = TrustResolver::new(TrustConfig::new().with_allowlisted("/tmp/worktrees"));

        // when
        let decision = resolver.resolve("/tmp/worktrees/repo-a", "Ready for your input\n>");

        // then
        assert_eq!(decision, TrustDecision::NotRequired);
        assert_eq!(decision.events(), &[]);
        assert_eq!(decision.policy(), None);
    }

    #[test]
    fn auto_trusts_allowlisted_cwd_after_prompt_detection() {
        // given
        let resolver = TrustResolver::new(TrustConfig::new().with_allowlisted("/tmp/worktrees"));

        // when
        let decision = resolver.resolve(
            "/tmp/worktrees/repo-a",
            "Do you trust the files in this folder?\n1. Yes, proceed\n2. No",
        );

        // then
        assert_eq!(decision.policy(), Some(TrustPolicy::AutoTrust));
        assert_eq!(
            decision.events(),
            &[
                TrustEvent::TrustRequired {
                    cwd: "/tmp/worktrees/repo-a".to_string(),
                },
                TrustEvent::TrustResolved {
                    cwd: "/tmp/worktrees/repo-a".to_string(),
                    policy: TrustPolicy::AutoTrust,
                },
            ]
        );
    }

    #[test]
    fn requires_approval_for_unknown_cwd_after_prompt_detection() {
        // given
        let resolver = TrustResolver::new(TrustConfig::new().with_allowlisted("/tmp/worktrees"));

        // when
        let decision = resolver.resolve(
            "/tmp/other/repo-b",
            "Do you trust the files in this folder?\n1. Yes, proceed\n2. No",
        );

        // then
        assert_eq!(decision.policy(), Some(TrustPolicy::RequireApproval));
        assert_eq!(
            decision.events(),
            &[TrustEvent::TrustRequired {
                cwd: "/tmp/other/repo-b".to_string(),
            }]
        );
    }

    #[test]
    fn denied_root_takes_precedence_over_allowlist() {
        // given
        let resolver = TrustResolver::new(
            TrustConfig::new()
                .with_allowlisted("/tmp/worktrees")
                .with_denied("/tmp/worktrees/repo-c"),
        );

        // when
        let decision = resolver.resolve(
            "/tmp/worktrees/repo-c",
            "Do you trust the files in this folder?\n1. Yes, proceed\n2. No",
        );

        // then
        assert_eq!(decision.policy(), Some(TrustPolicy::Deny));
        assert_eq!(
            decision.events(),
            &[
                TrustEvent::TrustRequired {
                    cwd: "/tmp/worktrees/repo-c".to_string(),
                },
                TrustEvent::TrustDenied {
                    cwd: "/tmp/worktrees/repo-c".to_string(),
                    reason: "cwd matches denied trust root: /tmp/worktrees/repo-c".to_string(),
                },
            ]
        );
    }

    #[test]
    fn sibling_prefix_does_not_match_trusted_root() {
        // given
        let trusted_root = "/tmp/worktrees";
        let sibling_path = "/tmp/worktrees-other/repo-d";

        // when
        let matched = path_matches_trusted_root(sibling_path, trusted_root);

        // then
        assert!(!matched);
    }
}
