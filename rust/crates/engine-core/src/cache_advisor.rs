//! Local, advisory-only TTL selection at context boundaries. Estimates are
//! counterfactual scenarios, never proof of expiry or a subscription invoice.
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use api::CacheRequestObservation;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const MAX_BYTES: u64 = 512 * 1024;
const MAX_DECISIONS: usize = 16;
const MIN_INTERVALS: usize = 8;
const SWITCH_MARGIN: f64 = 0.10;

#[derive(Clone, Default, Deserialize)]
struct Row {
    #[serde(default)]
    gateway_request_id: Option<String>,
    #[serde(default)]
    provider_request_id: Option<String>,
    #[serde(default)]
    model: String,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    observation: Option<CacheRequestObservation>,
}

impl Row {
    fn cached(&self) -> u64 {
        self.cache_read_input_tokens
            .saturating_add(self.cache_creation_input_tokens)
    }

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
    let mut coverage = Coverage::default();
    let Ok(mut file) = File::open(directory.join("requests.jsonl")) else {
        return (Vec::new(), coverage);
    };
    let length = file.metadata().map_or(0, |m| m.len());
    let offset = length.saturating_sub(MAX_BYTES);
    coverage.sampled_tail = offset > 0;
    if file.seek(SeekFrom::Start(offset)).is_err() {
        return (Vec::new(), coverage);
    }
    let mut reader = BufReader::new(file.take(MAX_BYTES));
    if offset > 0 {
        let _ = reader.read_line(&mut String::new());
    }
    let mut rows = Vec::new();
    let mut final_positions = HashMap::new();
    for line in reader.lines().map_while(Result::ok) {
        let Ok(row) = serde_json::from_str::<Row>(&line) else {
            coverage.legacy_or_malformed_rows += 1;
            rows.push(Some(Row::default()));
            continue;
        };
        if row.observation.is_none() {
            coverage.legacy_or_malformed_rows += 1;
        }
        if let (Some(id), Some(o)) = (
            row.gateway_request_id
                .as_ref()
                .filter(|id| !id.is_empty())
                .or(row.provider_request_id.as_ref().filter(|id| !id.is_empty())),
            &row.observation,
        ) {
            let identity = (
                id.clone(),
                row.model.clone(),
                o.route_key.clone(),
                o.request_started_ms,
                o.attempts,
                o.context_segment,
            );
            if let Some(before) = final_positions.insert(identity, rows.len()) {
                // A final cumulative snapshot replaces earlier snapshots of
                // this attempt; different attempt start times remain separate.
                rows[before] = None;
                coverage.duplicate_snapshots_ignored += 1;
            }
        }
        rows.push(Some(row));
    }
    let rows: Vec<Row> = rows.into_iter().flatten().collect();
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

fn cost(row: &Row, previous: Option<&Row>, ttl: u32, switched: bool, price: &Price) -> f64 {
    let mut read = 0;
    if !switched {
        if let Some((gap, prefix)) = previous.and_then(|p| reusable(p, row)) {
            if gap < u64::from(ttl) * 1000 {
                read = prefix;
            }
            // Preserve a hit actually observed under this same requested TTL,
            // including late hits; a hard timing cutoff cannot erase evidence.
            if uniform_ttl(row.eligible().unwrap()) == Some(ttl) {
                read = read.max(row.cache_read_input_tokens);
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
        let switched = previous_policy.is_some_and(|p| p != policy);
        if switched {
            result.transition_writes += 1;
        }
        result.fixed_5m_usd += cost(row, previous, 300, false, price);
        result.fixed_1h_usd += cost(row, previous, 3600, false, price);
        result.boundary_policy_usd += cost(row, previous, policy, switched, price);
        result.rows += 1;
        previous = Some(row);
        previous_policy = Some(policy);
    }
    result
}

/// Called once for a new context, or after a successfully committed compact.
/// Rebuilding/resuming the same context preserves the already made decision.
pub fn context_boundary(directory: &Path, segment: u64, model: &str, route: &str, configured: u32) {
    let mut state = state(directory);
    if state.decisions.last().is_some_and(|d| {
        d.context_segment == segment
            && d.model == model
            && d.route_key == route
            && d.configured_ttl_seconds == configured
    }) {
        return;
    }
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
    });
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
}

/// Sanitized view consumed by the existing CLI report. No second cost formula
/// or public request/route/session identifiers are introduced.
#[must_use]
pub fn report(directory: &Path) -> Value {
    let state = state(directory);
    let (rows, coverage) = history(directory);
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
        "mode": "advisory_only", "applied": false,
        "timing_basis": "request_start", "upstream_ttl": "unknown",
        "cost_basis": table.basis, "price_version": table.version,
        "comparison_kind": "hypothetical_not_causal_or_billed_savings",
        "assumptions": "append-compatible client prefixes survive on the same upstream; no TTL-boundary jitter; first/context-change/switch charged as full cached-input write",
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
