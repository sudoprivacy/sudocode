use crate::session::Session;

const DEFAULT_INPUT_COST_PER_MILLION: f64 = 15.0;
const DEFAULT_OUTPUT_COST_PER_MILLION: f64 = 75.0;
const DEFAULT_CACHE_CREATION_COST_PER_MILLION: f64 = 18.75;
const DEFAULT_CACHE_READ_COST_PER_MILLION: f64 = 1.5;

/// Per-million-token pricing used for cost estimation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelPricing {
    pub input_cost_per_million: f64,
    pub output_cost_per_million: f64,
    pub cache_creation_cost_per_million: f64,
    pub cache_read_cost_per_million: f64,
}

impl ModelPricing {
    #[must_use]
    pub const fn default_sonnet_tier() -> Self {
        Self {
            input_cost_per_million: DEFAULT_INPUT_COST_PER_MILLION,
            output_cost_per_million: DEFAULT_OUTPUT_COST_PER_MILLION,
            cache_creation_cost_per_million: DEFAULT_CACHE_CREATION_COST_PER_MILLION,
            cache_read_cost_per_million: DEFAULT_CACHE_READ_COST_PER_MILLION,
        }
    }
}

/// Token counters accumulated for a conversation turn or session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_creation_input_tokens: u32,
    pub cache_read_input_tokens: u32,
    pub cost_units: Option<u64>,
    pub cost_currency: Option<UsageCostCurrency>,
}

/// Controlled set of cost currencies accepted from provider usage payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageCostCurrency {
    SudoPoint,
}

impl UsageCostCurrency {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SudoPoint => "sudo_point",
        }
    }
}

#[must_use]
pub fn parse_usage_cost_currency(value: Option<&str>) -> Option<UsageCostCurrency> {
    match value {
        Some("sudo_point") => Some(UsageCostCurrency::SudoPoint),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct UsageCostAggregation {
    usage_count: usize,
    cost_count: usize,
    invalid_cost_count: usize,
    cost_units_sum: u64,
}

impl UsageCostAggregation {
    fn push(&mut self, usage: TokenUsage) {
        self.usage_count += 1;
        match (usage.cost_units, usage.cost_currency) {
            (Some(units), Some(UsageCostCurrency::SudoPoint)) => {
                self.cost_count += 1;
                self.cost_units_sum = self.cost_units_sum.saturating_add(units);
            }
            (None, None) => {}
            _ => {
                self.invalid_cost_count += 1;
            }
        }
    }

    fn apply_to(self, usage: &mut TokenUsage) {
        if self.usage_count > 0
            && self.cost_count == self.usage_count
            && self.invalid_cost_count == 0
        {
            usage.cost_units = Some(self.cost_units_sum);
            usage.cost_currency = Some(UsageCostCurrency::SudoPoint);
        } else {
            usage.cost_units = None;
            usage.cost_currency = None;
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct UsageAggregation {
    total: TokenUsage,
    cost: UsageCostAggregation,
}

impl UsageAggregation {
    pub(crate) fn push(&mut self, usage: TokenUsage) {
        self.total.add_assign_token_counts(usage);
        self.cost.push(usage);
    }

    #[must_use]
    pub(crate) fn finish(mut self) -> TokenUsage {
        self.cost.apply_to(&mut self.total);
        self.total
    }
}

/// Estimated dollar cost derived from a [`TokenUsage`] sample.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UsageCostEstimate {
    pub input_cost_usd: f64,
    pub output_cost_usd: f64,
    pub cache_creation_cost_usd: f64,
    pub cache_read_cost_usd: f64,
}

impl UsageCostEstimate {
    #[must_use]
    pub fn total_cost_usd(self) -> f64 {
        self.input_cost_usd
            + self.output_cost_usd
            + self.cache_creation_cost_usd
            + self.cache_read_cost_usd
    }
}

/// Returns pricing metadata for a known model alias or family.
#[must_use]
pub fn pricing_for_model(model: &str) -> Option<ModelPricing> {
    let normalized = model.to_ascii_lowercase();
    if normalized.contains("haiku") {
        return Some(ModelPricing {
            input_cost_per_million: 1.0,
            output_cost_per_million: 5.0,
            cache_creation_cost_per_million: 1.25,
            cache_read_cost_per_million: 0.1,
        });
    }
    if normalized.contains("opus") {
        return Some(ModelPricing {
            input_cost_per_million: 15.0,
            output_cost_per_million: 75.0,
            cache_creation_cost_per_million: 18.75,
            cache_read_cost_per_million: 1.5,
        });
    }
    if normalized.contains("sonnet") {
        return Some(ModelPricing::default_sonnet_tier());
    }
    None
}

impl TokenUsage {
    /// Aggregates another TokenUsage's token counters into this one using saturating_add.
    pub fn add_assign_usage(&mut self, other: Self) {
        self.add_assign_token_counts(other);
    }

    /// Aggregates another TokenUsage's token counters into this one using saturating_add.
    pub fn add_assign_token_counts(&mut self, other: Self) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.cache_creation_input_tokens = self
            .cache_creation_input_tokens
            .saturating_add(other.cache_creation_input_tokens);
        self.cache_read_input_tokens = self
            .cache_read_input_tokens
            .saturating_add(other.cache_read_input_tokens);
    }

    #[must_use]
    pub fn total_tokens(self) -> u32 {
        self.input_tokens
            + self.output_tokens
            + self.cache_creation_input_tokens
            + self.cache_read_input_tokens
    }

    #[must_use]
    pub fn estimate_cost_usd(self) -> UsageCostEstimate {
        self.estimate_cost_usd_with_pricing(ModelPricing::default_sonnet_tier())
    }

    #[must_use]
    pub fn estimate_cost_usd_with_pricing(self, pricing: ModelPricing) -> UsageCostEstimate {
        UsageCostEstimate {
            input_cost_usd: cost_for_tokens(self.input_tokens, pricing.input_cost_per_million),
            output_cost_usd: cost_for_tokens(self.output_tokens, pricing.output_cost_per_million),
            cache_creation_cost_usd: cost_for_tokens(
                self.cache_creation_input_tokens,
                pricing.cache_creation_cost_per_million,
            ),
            cache_read_cost_usd: cost_for_tokens(
                self.cache_read_input_tokens,
                pricing.cache_read_cost_per_million,
            ),
        }
    }

    #[must_use]
    pub fn summary_lines(self, label: &str) -> Vec<String> {
        self.summary_lines_for_model(label, None)
    }

    #[must_use]
    pub fn summary_lines_for_model(self, label: &str, model: Option<&str>) -> Vec<String> {
        let pricing = model.and_then(pricing_for_model);
        let cost = pricing.map_or_else(
            || self.estimate_cost_usd(),
            |pricing| self.estimate_cost_usd_with_pricing(pricing),
        );
        let model_suffix =
            model.map_or_else(String::new, |model_name| format!(" model={model_name}"));
        let pricing_suffix = if pricing.is_some() {
            ""
        } else if model.is_some() {
            " pricing=estimated-default"
        } else {
            ""
        };
        vec![
            format!(
                "{label}: total_tokens={} input={} output={} cache_write={} cache_read={} estimated_cost={}{}{}",
                self.total_tokens(),
                self.input_tokens,
                self.output_tokens,
                self.cache_creation_input_tokens,
                self.cache_read_input_tokens,
                format_usd(cost.total_cost_usd()),
                model_suffix,
                pricing_suffix,
            ),
            format!(
                "  cost breakdown: input={} output={} cache_write={} cache_read={}",
                format_usd(cost.input_cost_usd),
                format_usd(cost.output_cost_usd),
                format_usd(cost.cache_creation_cost_usd),
                format_usd(cost.cache_read_cost_usd),
            ),
        ]
    }
}

fn cost_for_tokens(tokens: u32, usd_per_million_tokens: f64) -> f64 {
    f64::from(tokens) / 1_000_000.0 * usd_per_million_tokens
}

#[must_use]
/// Formats a dollar-denominated value for CLI display.
pub fn format_usd(amount: f64) -> String {
    format!("${amount:.4}")
}

/// Aggregates token usage across a running session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageTracker {
    latest_turn: TokenUsage,
    cumulative: TokenUsage,
    cumulative_cost: UsageCostAggregation,
    turns: u32,
}

impl UsageTracker {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn from_session(session: &Session) -> Self {
        let mut tracker = Self::new();
        if let Some(usage) = session.compaction.as_ref().and_then(|value| value.usage) {
            tracker.record(usage);
        }
        for message in &session.messages {
            if let Some(usage) = message.usage {
                tracker.record(usage);
            }
        }
        tracker
    }

    pub fn record(&mut self, usage: TokenUsage) {
        self.latest_turn = usage;
        self.cumulative.add_assign_token_counts(usage);
        self.cumulative_cost.push(usage);
        self.cumulative_cost.apply_to(&mut self.cumulative);
        self.turns = self.turns.saturating_add(1);
    }

    #[must_use]
    pub fn current_turn_usage(&self) -> TokenUsage {
        self.latest_turn
    }

    /// Returns the usage for the turn that just completed, if any was recorded.
    /// Pass the turn count before the operation to detect if new usage was recorded.
    /// This prevents returning stale usage from previous turns.
    #[must_use]
    pub fn turn_usage_if_recorded(&self, turns_before: u32) -> Option<TokenUsage> {
        if self.turns > turns_before {
            Some(self.latest_turn)
        } else {
            None
        }
    }

    #[must_use]
    pub fn cumulative_usage(&self) -> TokenUsage {
        self.cumulative
    }

    #[must_use]
    pub fn turns(&self) -> u32 {
        self.turns
    }
}

#[cfg(test)]
mod tests {
    use super::{
        format_usd, parse_usage_cost_currency, pricing_for_model, TokenUsage, UsageCostCurrency,
        UsageTracker,
    };
    use crate::session::{
        ContentBlock, ConversationMessage, MessageRole, Session, SessionCompaction,
    };

    #[test]
    fn adds_token_usage_fields() {
        let mut total = TokenUsage {
            input_tokens: 10,
            output_tokens: 2,
            cache_creation_input_tokens: 1,
            cache_read_input_tokens: 3,
            ..TokenUsage::default()
        };
        total.add_assign_usage(TokenUsage {
            input_tokens: 20,
            output_tokens: 4,
            cache_creation_input_tokens: 2,
            cache_read_input_tokens: 6,
            ..TokenUsage::default()
        });

        assert_eq!(total.input_tokens, 30);
        assert_eq!(total.output_tokens, 6);
        assert_eq!(total.cache_creation_input_tokens, 3);
        assert_eq!(total.cache_read_input_tokens, 9);
        assert_eq!(total.total_tokens(), 48);
    }

    #[test]
    fn saturating_add_prevents_overflow() {
        let mut total = TokenUsage {
            input_tokens: u32::MAX - 1,
            output_tokens: u32::MAX - 1,
            cache_creation_input_tokens: u32::MAX - 1,
            cache_read_input_tokens: u32::MAX - 1,
            ..TokenUsage::default()
        };
        total.add_assign_usage(TokenUsage {
            input_tokens: 10,
            output_tokens: 10,
            cache_creation_input_tokens: 10,
            cache_read_input_tokens: 10,
            ..TokenUsage::default()
        });

        // Should saturate at u32::MAX instead of wrapping
        assert_eq!(total.input_tokens, u32::MAX);
        assert_eq!(total.output_tokens, u32::MAX);
        assert_eq!(total.cache_creation_input_tokens, u32::MAX);
        assert_eq!(total.cache_read_input_tokens, u32::MAX);
    }

    #[test]
    fn tracks_true_cumulative_usage() {
        let mut tracker = UsageTracker::new();
        tracker.record(TokenUsage {
            input_tokens: 10,
            output_tokens: 4,
            cache_creation_input_tokens: 2,
            cache_read_input_tokens: 1,
            cost_units: Some(100),
            cost_currency: Some(UsageCostCurrency::SudoPoint),
        });
        tracker.record(TokenUsage {
            input_tokens: 20,
            output_tokens: 6,
            cache_creation_input_tokens: 3,
            cache_read_input_tokens: 2,
            cost_units: Some(200),
            cost_currency: Some(UsageCostCurrency::SudoPoint),
        });

        assert_eq!(tracker.turns(), 2);
        assert_eq!(tracker.current_turn_usage().input_tokens, 20);
        assert_eq!(tracker.current_turn_usage().output_tokens, 6);
        assert_eq!(tracker.cumulative_usage().output_tokens, 10);
        assert_eq!(tracker.cumulative_usage().input_tokens, 30);
        assert_eq!(tracker.cumulative_usage().total_tokens(), 48);
        assert_eq!(tracker.cumulative_usage().cost_units, Some(300));
        assert_eq!(
            tracker.cumulative_usage().cost_currency,
            Some(UsageCostCurrency::SudoPoint)
        );
    }

    #[test]
    fn tracker_omits_cumulative_cost_after_missing_cost() {
        let mut tracker = UsageTracker::new();
        tracker.record(TokenUsage {
            input_tokens: 10,
            output_tokens: 4,
            cache_creation_input_tokens: 2,
            cache_read_input_tokens: 1,
            cost_units: Some(100),
            cost_currency: Some(UsageCostCurrency::SudoPoint),
        });
        tracker.record(TokenUsage {
            input_tokens: 20,
            output_tokens: 6,
            cache_creation_input_tokens: 3,
            cache_read_input_tokens: 2,
            ..TokenUsage::default()
        });

        assert_eq!(tracker.cumulative_usage().total_tokens(), 48);
        assert_eq!(tracker.cumulative_usage().cost_units, None);
        assert_eq!(tracker.cumulative_usage().cost_currency, None);
    }

    #[test]
    fn tracker_preserves_zero_cost() {
        let mut tracker = UsageTracker::new();
        tracker.record(TokenUsage {
            input_tokens: 10,
            output_tokens: 4,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            cost_units: Some(0),
            cost_currency: Some(UsageCostCurrency::SudoPoint),
        });

        assert_eq!(tracker.cumulative_usage().cost_units, Some(0));
        assert_eq!(
            tracker.cumulative_usage().cost_currency,
            Some(UsageCostCurrency::SudoPoint)
        );
    }

    #[test]
    fn parses_only_supported_cost_currency() {
        assert_eq!(
            parse_usage_cost_currency(Some("sudo_point")),
            Some(UsageCostCurrency::SudoPoint)
        );
        assert_eq!(parse_usage_cost_currency(Some("usd")), None);
        assert_eq!(parse_usage_cost_currency(None), None);
    }

    #[test]
    fn computes_cost_summary_lines() {
        let usage = TokenUsage {
            input_tokens: 1_000_000,
            output_tokens: 500_000,
            cache_creation_input_tokens: 100_000,
            cache_read_input_tokens: 200_000,
            ..TokenUsage::default()
        };

        let cost = usage.estimate_cost_usd();
        assert_eq!(format_usd(cost.input_cost_usd), "$15.0000");
        assert_eq!(format_usd(cost.output_cost_usd), "$37.5000");
        let lines = usage.summary_lines_for_model("usage", Some("claude-sonnet-4-20250514"));
        assert!(lines[0].contains("estimated_cost=$54.6750"));
        assert!(lines[0].contains("model=claude-sonnet-4-20250514"));
        assert!(lines[1].contains("cache_read=$0.3000"));
    }

    #[test]
    fn supports_model_specific_pricing() {
        let usage = TokenUsage {
            input_tokens: 1_000_000,
            output_tokens: 500_000,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            ..TokenUsage::default()
        };

        let haiku = pricing_for_model("claude-haiku-4-5-20251001").expect("haiku pricing");
        let opus = pricing_for_model("claude-opus-4-6").expect("opus pricing");
        let haiku_cost = usage.estimate_cost_usd_with_pricing(haiku);
        let opus_cost = usage.estimate_cost_usd_with_pricing(opus);
        assert_eq!(format_usd(haiku_cost.total_cost_usd()), "$3.5000");
        assert_eq!(format_usd(opus_cost.total_cost_usd()), "$52.5000");
    }

    #[test]
    fn marks_unknown_model_pricing_as_fallback() {
        let usage = TokenUsage {
            input_tokens: 100,
            output_tokens: 100,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            ..TokenUsage::default()
        };
        let lines = usage.summary_lines_for_model("usage", Some("custom-model"));
        assert!(lines[0].contains("pricing=estimated-default"));
    }

    #[test]
    fn reconstructs_usage_from_session_messages() {
        let mut session = Session::new();
        session.messages = vec![ConversationMessage {
            role: MessageRole::Assistant,
            blocks: vec![ContentBlock::Text {
                text: "done".to_string(),
            }],
            usage: Some(TokenUsage {
                input_tokens: 5,
                output_tokens: 2,
                cache_creation_input_tokens: 1,
                cache_read_input_tokens: 0,
                cost_units: Some(500),
                cost_currency: Some(UsageCostCurrency::SudoPoint),
            }),
            model: None,
        }];

        let tracker = UsageTracker::from_session(&session);
        assert_eq!(tracker.turns(), 1);
        assert_eq!(tracker.cumulative_usage().total_tokens(), 8);
    }

    #[test]
    fn reconstructs_usage_from_compaction_and_session_messages() {
        let mut session = Session::new();
        session.compaction = Some(SessionCompaction {
            count: 1,
            removed_message_count: 2,
            summary: "summarized earlier work".to_string(),
            usage: Some(TokenUsage {
                input_tokens: 10,
                output_tokens: 4,
                cache_creation_input_tokens: 1,
                cache_read_input_tokens: 2,
                cost_units: Some(100),
                cost_currency: Some(UsageCostCurrency::SudoPoint),
            }),
        });
        session.messages = vec![ConversationMessage {
            role: MessageRole::Assistant,
            blocks: vec![ContentBlock::Text {
                text: "done".to_string(),
            }],
            usage: Some(TokenUsage {
                input_tokens: 5,
                output_tokens: 2,
                cache_creation_input_tokens: 1,
                cache_read_input_tokens: 0,
                cost_units: Some(50),
                cost_currency: Some(UsageCostCurrency::SudoPoint),
            }),
            model: None,
        }];

        let tracker = UsageTracker::from_session(&session);
        assert_eq!(tracker.turns(), 2);
        assert_eq!(tracker.current_turn_usage().total_tokens(), 8);
        assert_eq!(tracker.cumulative_usage().input_tokens, 15);
        assert_eq!(tracker.cumulative_usage().output_tokens, 6);
        assert_eq!(tracker.cumulative_usage().total_tokens(), 25);
        assert_eq!(tracker.cumulative_usage().cost_units, Some(150));
        assert_eq!(
            tracker.cumulative_usage().cost_currency,
            Some(UsageCostCurrency::SudoPoint)
        );
    }
}
