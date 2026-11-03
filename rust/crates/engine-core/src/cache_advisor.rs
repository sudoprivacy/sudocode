//! Local, advisory-only TTL selection at context boundaries. Estimates are
//! counterfactual scenarios, never proof of expiry or a subscription invoice.
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use api::CacheRequestObservation;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::cache_report::{
    request_history, RequestHistory, RequestRow as Row, MAX_LEDGER_BYTES as MAX_BYTES,
};

const MAX_DECISIONS: usize = 16;
/// Reuse intervals the replay needs before it suggests anything.
pub const MIN_INTERVALS: usize = 8;
const SWITCH_MARGIN: f64 = 0.10;

impl Row {
    fn eligible(&self) -> Option<&CacheRequestObservation> {
        let o = self.observation.as_ref()?;
        (o.schema_version == 1
            && o.completed
            && o.attempts == 1
            && o.message_starts == 1
            && o.request_kind == "session_request"
            && o.request_started_ms > 0
            && o.request_started_ms <= o.response_started_ms
            && o.response_started_ms <= o.response_completed_ms
            && self.cached() > 0
            && uniform_ttl(o).is_some())
        .then_some(o)
    }
}

fn uniform_ttl(o: &CacheRequestObservation) -> Option<u32> {
    let ttl = *o.requested_ttls_seconds.first()?.as_ref()?;
    (matches!(ttl, 300 | 3600)
        && o.requested_ttls_seconds
            .iter()
            .all(|value| *value == Some(ttl)))
    .then_some(ttl)
}

#[derive(Default, Serialize)]
struct Coverage {
    rows: usize,
    legacy_or_malformed_rows: usize,
    duplicate_snapshots_ignored: usize,
    ineligible_rows: usize,
    sampled_tail: bool,
}

fn history(directory: &Path) -> (Vec<Row>, Coverage) {
    advisor_history(&request_history(&directory.join("requests.jsonl")))
}

fn advisor_history(history: &RequestHistory) -> (Vec<Row>, Coverage) {
    let mut coverage = Coverage {
        sampled_tail: history.sampled_tail,
        duplicate_snapshots_ignored: history.duplicate_snapshots_ignored,
        ..Coverage::default()
    };
    let mut rows = Vec::with_capacity(history.rows.len());
    for row in &history.rows {
        let row = row.clone().unwrap_or_default();
        if row.observation.is_none() {
            coverage.legacy_or_malformed_rows += 1;
        }
        rows.push(row);
    }
    coverage.rows = rows.len();
    coverage.ineligible_rows = rows.iter().filter(|r| r.eligible().is_none()).count();
    (rows, coverage)
}

#[derive(Clone, Deserialize)]
struct Price {
    write_5m: f64,
    write_1h: f64,
    read: f64,
}

#[derive(Deserialize)]
struct Prices {
    version: String,
    basis: String,
    models: HashMap<String, Price>,
}

fn prices() -> &'static Prices {
    static TABLE: OnceLock<Prices> = OnceLock::new();
    TABLE.get_or_init(|| {
        serde_json::from_str(include_str!("../../../../spec/cache_ttl_prices.json"))
            .expect("committed TTL reference price table")
    })
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Scenario {
    rows: usize,
    eligible_intervals: usize,
    fixed_5m_usd: f64,
    fixed_1h_usd: f64,
    boundary_policy_usd: f64,
    /// Switches that paid a full rewrite: 5m to 1h while the prefix was warm.
    transition_writes: usize,
}

#[derive(Clone, Serialize, Deserialize)]
struct Decision {
    made_at_ms: u64,
    context_segment: u64,
    model: String,
    route_key: String,
    configured_ttl_seconds: u32,
    suggested_ttl_seconds: u32,
    reason: String,
    training: Scenario,
    /// What prompted the decision; empty in files from older builds, which
    /// only decided at context boundaries.
    #[serde(default)]
    trigger: String,
    /// Whether this decision chose the TTL on the wire, because none was
    /// pinned. False for advice only, and in files from older builds.
    #[serde(default)]
    applied: bool,
}

/// What prompts a decision. Pinned sessions decide only at boundaries; an
/// automatic TTL also decides at cold points and every few requests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    /// A new context: runtime build or resume, compaction, a model or route change.
    Boundary,
    /// The next request follows an idle longer than the TTL in use.
    Cold,
    /// Enough requests since the last decision.
    Periodic,
}

impl Trigger {
    fn label(self) -> &'static str {
        match self {
            Self::Boundary => "boundary",
            Self::Cold => "cold",
            Self::Periodic => "periodic",
        }
    }
}

/// What a decision means for an automatic TTL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Too few reuse intervals, or a model without prices.
    NoEvidence,
    /// Both TTLs cost about the same; keep the one in use.
    Keep,
    /// This TTL is cheaper by more than the margin.
    Seconds(u32),
}

fn verdict(decision: &Decision) -> Verdict {
    match decision.reason.as_str() {
        "hypothetical_1h_saving" | "hypothetical_5m_saving" => {
            Verdict::Seconds(decision.suggested_ttl_seconds)
        }
        "within_uncertainty_margin" => Verdict::Keep,
        _ => Verdict::NoEvidence,
    }
}

#[derive(Default, Serialize, Deserialize)]
struct State {
    decisions: Vec<Decision>,
}

fn state(directory: &Path) -> State {
    let Ok(file) = File::open(directory.join("ttl-advice.json")) else {
        return State::default();
    };
    // Derived state is small and bounded, including when a local file is corrupt.
    serde_json::from_reader(file.take(64 * 1024)).unwrap_or_default()
}

fn write_cost(read: u64, write: u64, ttl: u32, price: &Price) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let comparison = crate::cache_metrics::ttl_cost_comparison(
        read as f64,
        write as f64,
        0.0,
        price.write_5m,
        price.write_1h,
        price.read,
    );
    comparison.map_or(0.0, |c| if ttl == 3600 { c.cost_1h } else { c.cost_5m }) / 1_000_000.0
}

fn reusable(previous: &Row, current: &Row) -> Option<(u64, u64)> {
    let a = previous.eligible()?;
    let b = current.eligible()?;
    if previous.model != current.model
        || a.route_key != b.route_key
        || a.settings_key != b.settings_key
        || a.context_segment != b.context_segment
        || b.client_prefix_compatible != Some(true)
    {
        return None;
    }
    let gap = b.request_started_ms.checked_sub(a.request_started_ms)?;
    if b.request_started_ms < a.response_started_ms {
        return None;
    }
    Some((gap, previous.cached().min(current.cached())))
}

/// Cost of `row` sent at `ttl`, when the previous request's prefix was held
/// for `held` seconds. `rewrite` charges the whole cached input as a write.
fn cost(
    row: &Row,
    previous: Option<&Row>,
    ttl: u32,
    held: u32,
    rewrite: bool,
    price: &Price,
) -> f64 {
    let mut read = 0;
    if !rewrite {
        if let Some((gap, prefix)) = previous.and_then(|p| reusable(p, row)) {
            if gap < u64::from(held) * 1000 {
                read = prefix;
            }
            // Preserve a hit actually observed under this same requested TTL,
            // including late hits; a hard timing cutoff cannot erase evidence.
            if uniform_ttl(row.eligible().unwrap()) == Some(ttl) {
                read = read.max(row.cache_read_input_tokens.unwrap_or_default());
            }
        }
    }
    write_cost(read, row.cached().saturating_sub(read), ttl, price)
}

fn replay(
    rows: &[Row],
    model: &str,
    route: &str,
    price: &Price,
    decisions: Option<&[Decision]>,
) -> Scenario {
    let mut result = Scenario::default();
    let mut previous: Option<&Row> = None;
    let mut previous_policy = None;
    for row in rows {
        let Some(o) = row.eligible() else {
            previous = None;
            previous_policy = None;
            continue;
        };
        if row.model != model || o.route_key != route {
            previous = None;
            previous_policy = None;
            continue;
        }
        let policy = if let Some(decisions) = decisions {
            let Some(d) = decisions.iter().rev().find(|d| {
                d.model == model
                    && d.route_key == route
                    && d.context_segment == o.context_segment
                    && d.made_at_ms < o.request_started_ms
            }) else {
                previous = None;
                previous_policy = None;
                continue;
            };
            d.suggested_ttl_seconds
        } else {
            300
        };
        let interval = previous.and_then(|p| reusable(p, row));
        if interval.is_some() {
            result.eligible_intervals += 1;
        }
        // Measured against the API: a growing request that asks for 1h on a
        // prefix still warm at 5m rewrites the whole prefix at the 1h price;
        // one asking for 5m on a 1h prefix keeps reading it. A switch on a
        // prefix already gone costs nothing extra, it is rewritten anyway.
        let (held, rewrite) = match previous_policy {
            Some(before) if before != policy => {
                let warm = interval.is_some_and(|(gap, _)| gap < u64::from(before) * 1000);
                let rewrite = policy > before && warm;
                if rewrite {
                    result.transition_writes += 1;
                }
                (before, rewrite)
            }
            _ => (policy, false),
        };
        result.fixed_5m_usd += cost(row, previous, 300, 300, false, price);
        result.fixed_1h_usd += cost(row, previous, 3600, 3600, false, price);
        result.boundary_policy_usd += cost(row, previous, policy, held, rewrite, price);
        result.rows += 1;
        previous = Some(row);
        previous_policy = Some(policy);
    }
    result
}

/// Called once for a new context, or after a successfully committed compact.
/// Rebuilding/resuming the same context keeps the latest decision, which may
/// be an automatic one from the previous process. Returns its verdict and
/// whether it was made just now.
#[must_use]
pub fn context_boundary(
    directory: &Path,
    segment: u64,
    model: &str,
    route: &str,
    configured: u32,
    applied: bool,
) -> (Verdict, bool) {
    let state = state(directory);
    if let Some(last) = state.decisions.last().filter(|d| {
        d.context_segment == segment
            && d.model == model
            && d.route_key == route
            && d.configured_ttl_seconds == configured
    }) {
        return (verdict(last), false);
    }
    let verdict = decide_into(
        state,
        directory,
        (segment, model, route),
        configured,
        Trigger::Boundary,
        applied,
    );
    (verdict, true)
}

/// Decide now, whatever was decided before: an automatic TTL does so at a
/// cold point and every few requests, so a long context still adapts. The
/// margin is around `current`, the TTL in use, so it does not flip-flop.
#[must_use]
pub fn decide(
    directory: &Path,
    segment: u64,
    model: &str,
    route: &str,
    current: u32,
    trigger: Trigger,
) -> Verdict {
    decide_into(
        state(directory),
        directory,
        (segment, model, route),
        current,
        trigger,
        true,
    )
}

fn decide_into(
    mut state: State,
    directory: &Path,
    (segment, model, route): (u64, &str, &str),
    configured: u32,
    trigger: Trigger,
    applied: bool,
) -> Verdict {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX);
    let (rows, _) = history(directory);
    // Never train on a later response or a still-running request.
    let rows: Vec<_> = rows
        .into_iter()
        .map(|r| {
            if r.observation
                .as_ref()
                .is_some_and(|o| o.response_completed_ms < now)
            {
                r
            } else {
                // Unknown or future rows break adjacency; dropping them would
                // invent reuse intervals across missing observations.
                Row::default()
            }
        })
        .collect();
    let table = prices();
    let (training, suggestion, reason) = if let Some(price) = table.models.get(model) {
        let training = replay(&rows, model, route, price, None);
        let sufficient = training.eligible_intervals >= MIN_INTERVALS;
        let (suggestion, reason) = if !sufficient {
            (configured, "insufficient_evidence")
        } else if training.fixed_1h_usd < training.fixed_5m_usd * (1.0 - SWITCH_MARGIN) {
            (3600, "hypothetical_1h_saving")
        } else if training.fixed_5m_usd < training.fixed_1h_usd * (1.0 - SWITCH_MARGIN) {
            (300, "hypothetical_5m_saving")
        } else {
            (configured, "within_uncertainty_margin")
        };
        (training, suggestion, reason)
    } else {
        (Scenario::default(), configured, "unknown_model_price")
    };
    state.decisions.push(Decision {
        made_at_ms: now,
        context_segment: segment,
        model: model.to_owned(),
        route_key: route.to_owned(),
        configured_ttl_seconds: configured,
        suggested_ttl_seconds: suggestion,
        reason: reason.to_owned(),
        training,
        trigger: trigger.label().to_owned(),
        applied,
    });
    let result = state.decisions.last().map_or(Verdict::NoEvidence, verdict);
    if state.decisions.len() > MAX_DECISIONS {
        state
            .decisions
            .drain(..state.decisions.len() - MAX_DECISIONS);
    }
    // Boundary-only I/O, not a scan or rewrite on each streamed event.
    if fs::create_dir_all(directory).is_ok() {
        let temporary = directory.join(format!("ttl-advice-{}.tmp", std::process::id()));
        if let Ok(bytes) = serde_json::to_vec(&state) {
            if fs::write(&temporary, bytes).is_ok() {
                let _ = fs::rename(&temporary, directory.join("ttl-advice.json"));
            }
        }
    }
    result
}

/// Sanitized view consumed by the existing CLI report. No second cost formula
/// or public request/route/session identifiers are introduced.
#[must_use]
pub fn report(directory: &Path) -> Value {
    report_from_history(
        directory,
        &request_history(&directory.join("requests.jsonl")),
    )
}

pub(crate) fn report_from_history(directory: &Path, history: &RequestHistory) -> Value {
    let state = state(directory);
    // Unpinned sessions follow their latest decision; pinned ones only advise.
    let applied = state.decisions.last().is_some_and(|d| d.applied);
    let (rows, coverage) = advisor_history(history);
    let table = prices();
    let mut seen = HashSet::new();
    let mut forward = Vec::new();
    for decision in state.decisions.iter().rev() {
        if !seen.insert((&decision.model, &decision.route_key)) {
            continue;
        }
        if let Some(price) = table.models.get(&decision.model) {
            forward.push(json!({"model": decision.model,
                "comparison": replay(&rows, &decision.model, &decision.route_key, price,
                    Some(&state.decisions))}));
        }
    }
    json!({
        "mode": if applied { "automatic" } else { "advisory_only" }, "applied": applied,
        "timing_basis": "request_start", "upstream_ttl": "unknown",
        "cost_basis": table.basis, "price_version": table.version,
        "comparison_kind": "hypothetical_not_causal_or_billed_savings",
        "assumptions": "append-compatible client prefixes survive on the same upstream; no TTL-boundary jitter; first request and context change charged as a full cached-input write; a switch to 1h on a warm prefix rewrites it at the 1h price, a switch to 5m keeps it",
        "coverage": coverage, "max_ledger_bytes": MAX_BYTES, "min_intervals": MIN_INTERVALS,
        "switch_margin": SWITCH_MARGIN,
        "decisions": state.decisions.iter().map(|d| json!({
            "context_segment": d.context_segment, "model": d.model,
            "configured_ttl_seconds": d.configured_ttl_seconds,
            "suggested_ttl_seconds": d.suggested_ttl_seconds,
            "reason": d.reason, "training": d.training,
        })).collect::<Vec<_>>(),
        "forward_comparison": forward,
    })
}

/// What the footer and the TTL panel need to offer a switch: the saved
/// boundary decision, the TTL this session's requests use, and what moving
/// the cached prefix would cost. No ledger scan beyond the last request.
#[derive(Clone, Debug, PartialEq)]
pub struct TtlAdvice {
    pub suggested_seconds: u32,
    pub effective_seconds: u32,
    pub pending: Option<crate::cache_ttl::PendingTtl>,
    /// Whether the decision found a saving, rather than lacking evidence.
    pub has_saving: bool,
    pub rows: usize,
    pub fixed_5m_usd: f64,
    pub fixed_1h_usd: f64,
    /// Cached input of the session's last request: the prefix a switch to 1h
    /// rewrites.
    pub cached_tokens: u64,
    /// One-time cost of that rewrite now, while warm, and at a cold point,
    /// where the prefix is rewritten anyway. Reference prices; `None` for a
    /// model the price table does not know.
    pub switch_now_usd: Option<f64>,
    pub switch_cold_usd: Option<f64>,
    /// No TTL is pinned: the session's own history picks it.
    pub auto: bool,
    /// Whether the decision had enough reuse intervals to compare at all.
    pub has_evidence: bool,
    /// Reuse intervals the decision replayed; [`MIN_INTERVALS`] are needed.
    pub intervals: usize,
}

impl TtlAdvice {
    /// The TTL an automatic session is heading for: the cheaper one when the
    /// history shows a saving, the one in use within the margin, and 5m
    /// without evidence. Its first full write is the cheaper one, and moving
    /// to 1h later at a cold point costs nothing extra.
    #[must_use]
    pub fn auto_target(&self) -> u32 {
        if self.has_saving {
            self.suggested_seconds
        } else if self.has_evidence {
            self.effective_seconds
        } else {
            300
        }
    }

    /// The suggested TTL's saving over the other one, as a percentage.
    #[must_use]
    pub fn saving_pct(&self) -> f64 {
        let (chosen, other) = if self.suggested_seconds == 3600 {
            (self.fixed_1h_usd, self.fixed_5m_usd)
        } else {
            (self.fixed_5m_usd, self.fixed_1h_usd)
        };
        if other > 0.0 {
            100.0 * (1.0 - chosen / other)
        } else {
            0.0
        }
    }
}

/// `"1h"` or `"5m"`.
#[must_use]
pub fn ttl_label(seconds: u32) -> &'static str {
    if seconds == 3600 {
        "1h"
    } else {
        "5m"
    }
}

fn valid_session_id(session_id: &str) -> bool {
    matches!(
        Path::new(session_id).components().next(),
        Some(std::path::Component::Normal(_))
    ) && Path::new(session_id).components().count() == 1
}

/// Cached input of the last request in the ledger, read from its tail.
fn last_cached_tokens(directory: &Path) -> u64 {
    let Ok(mut file) = File::open(directory.join("requests.jsonl")) else {
        return 0;
    };
    let length = file.metadata().map_or(0, |m| m.len());
    if file
        .seek(SeekFrom::Start(length.saturating_sub(16 * 1024)))
        .is_err()
    {
        return 0;
    }
    let mut bytes = Vec::new();
    if file.take(16 * 1024).read_to_end(&mut bytes).is_err() {
        return 0;
    }
    String::from_utf8_lossy(&bytes)
        .lines()
        .rev()
        .find_map(|line| {
            let row = serde_json::from_str::<Row>(line).ok()?;
            (row.observation.is_some() && row.cached() > 0).then(|| row.cached())
        })
        .unwrap_or(0)
}

/// The boundary recommendation against the TTL actually in use, using the same
/// saved decision as stats. No price calculation beyond the switch cost.
#[must_use]
pub fn ttl_advice(session_id: &str, model: &str) -> Option<TtlAdvice> {
    if !runtime::experiments::is_enabled(runtime::experiments::Experiment::CacheTtlAdvisor)
        || !valid_session_id(session_id)
    {
        return None;
    }
    let directory = crate::cache_root().join(session_id);
    let decision = state(&directory).decisions.pop()?;
    if decision.model != model {
        return None;
    }
    let status = crate::cache_ttl::status(session_id);
    let cached_tokens = last_cached_tokens(&directory);
    let price = prices().models.get(model);
    #[allow(clippy::cast_precision_loss)]
    let tokens = cached_tokens as f64;
    Some(TtlAdvice {
        suggested_seconds: decision.suggested_ttl_seconds,
        effective_seconds: status
            .effective_seconds
            .unwrap_or(decision.configured_ttl_seconds),
        pending: status.pending,
        has_saving: decision.reason.starts_with("hypothetical_"),
        rows: decision.training.rows,
        fixed_5m_usd: decision.training.fixed_5m_usd,
        fixed_1h_usd: decision.training.fixed_1h_usd,
        cached_tokens,
        switch_now_usd: price.map(|p| tokens * (p.write_1h - p.read) / 1_000_000.0),
        switch_cold_usd: price.map(|p| tokens * (p.write_1h - p.write_5m) / 1_000_000.0),
        auto: status.auto,
        has_evidence: verdict(&decision) != Verdict::NoEvidence,
        intervals: decision.training.eligible_intervals,
    })
}

/// The footer tip for the current recommendation. Pinned: a pending switch,
/// or a suggestion that differs from the TTL in use. Automatic: what it is
/// about to do, or what it just did when `just_switched`. `None` otherwise.
#[must_use]
pub fn session_tip(session_id: &str, model: &str, just_switched: bool) -> Option<crate::Tip> {
    let advice = ttl_advice(session_id, model)?;
    if advice.auto {
        let saving = if advice.has_saving {
            format!(" (−{:.0}%)", advice.saving_pct())
        } else {
            String::new()
        };
        let target = advice.auto_target();
        // Waiting for a cold point can be cut short or pinned instead, so that
        // tip is a footer stop; a done switch is only news.
        let (priority, message) = if just_switched {
            (
                crate::TipPriority::Normal,
                format!(
                    "Cache TTL auto: now {}{saving}",
                    ttl_label(advice.effective_seconds)
                ),
            )
        } else if target != advice.effective_seconds {
            // A move to 5m reaches the next request; one to 1h waits for a
            // cold point, where the prefix is rewritten anyway.
            let when = if target == 3600 {
                "when cold"
            } else {
                "next request"
            };
            (
                crate::TipPriority::Important,
                format!("Cache TTL auto: {} {when}{saving}", ttl_label(target)),
            )
        } else {
            return None;
        };
        return Some(crate::Tip { priority, message });
    }
    if let Some(pending) = advice.pending {
        let message = match pending.choice {
            crate::cache_ttl::TtlChoice::OneHour if !pending.force_now => {
                "Cache TTL: 1h at the next cold start".to_owned()
            }
            crate::cache_ttl::TtlChoice::OneHour => {
                "Cache TTL: 1h from the next request".to_owned()
            }
            crate::cache_ttl::TtlChoice::FiveMinutes => {
                "Cache TTL: 5m from the next request".to_owned()
            }
            crate::cache_ttl::TtlChoice::Auto => "Cache TTL: auto from the next request".to_owned(),
        };
        return Some(crate::Tip {
            priority: crate::TipPriority::Normal,
            message,
        });
    }
    if !advice.has_saving || advice.suggested_seconds == advice.effective_seconds {
        return None;
    }
    Some(crate::Tip {
        priority: crate::TipPriority::Important,
        message: format!(
            "Cache TTL: {} ≈{:.0}% cheaper (now {})",
            ttl_label(advice.suggested_seconds),
            advice.saving_pct(),
            ttl_label(advice.effective_seconds)
        ),
    })
}
