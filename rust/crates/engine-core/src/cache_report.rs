//! Bounded, read-only diagnostics over the local request ledger.
//! Completion intervals are observed timings, not idle time or proof of expiry.
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use api::CacheRequestObservation;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub(crate) const MAX_LEDGER_BYTES: u64 = 512 * 1024;

/// The shared report input. Missing observations retain legacy coverage rather
/// than being interpreted as clean advisor samples.
#[derive(Clone, Default, Deserialize)]
pub(crate) struct RequestRow {
    #[serde(default)]
    pub at_unix_secs: Option<u64>,
    #[serde(default)]
    gateway_request_id: Option<String>,
    #[serde(default)]
    provider_request_id: Option<String>,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    pub cache_read_input_tokens: Option<u64>,
    #[serde(default)]
    pub cache_creation_input_tokens: Option<u64>,
    pub observation: Option<CacheRequestObservation>,
}

impl RequestRow {
    #[inline]
    pub(crate) fn cached(&self) -> u64 {
        self.cache_read_input_tokens
            .unwrap_or_default()
            .saturating_add(self.cache_creation_input_tokens.unwrap_or_default())
    }

    fn identity(&self) -> Option<RequestIdentity> {
        let id = self
            .gateway_request_id
            .as_ref()
            .filter(|id| !id.is_empty())
            .or(self
                .provider_request_id
                .as_ref()
                .filter(|id| !id.is_empty()))?
            .clone();
        if let Some(o) = self
            .observation
            .as_ref()
            .filter(|o| o.request_started_ms > 0)
        {
            return Some(RequestIdentity::Attempt {
                id,
                model: self.model.clone(),
                route: o.route_key.clone(),
                started_ms: o.request_started_ms,
                attempts: o.attempts,
                segment: o.context_segment,
            });
        }
        // Without attempt timing, only exact legacy repeats are safe to merge.
        // Preserve all known observation fields, even when timing is absent.
        Some(RequestIdentity::Legacy {
            id,
            at: self.at_unix_secs?,
            model: self.model.clone(),
            provider_id: self.provider_request_id.clone(),
            input: self.input_tokens,
            read: self.cache_read_input_tokens,
            write: self.cache_creation_input_tokens,
            observation: self
                .observation
                .as_ref()
                .map(serde_json::to_string)
                .transpose()
                .ok()?,
        })
    }
}

#[derive(Hash, PartialEq, Eq)]
enum RequestIdentity {
    Attempt {
        id: String,
        model: String,
        route: String,
        started_ms: u64,
        attempts: u32,
        segment: u64,
    },
    Legacy {
        id: String,
        at: u64,
        model: String,
        provider_id: Option<String>,
        input: Option<u64>,
        read: Option<u64>,
        write: Option<u64>,
        observation: Option<String>,
    },
}

#[derive(Default)]
pub(crate) struct RequestHistory {
    /// A malformed row remains a barrier to interval/reuse attribution.
    pub rows: Vec<Option<RequestRow>>,
    pub available: bool,
    pub sampled_tail: bool,
    pub duplicate_snapshots_ignored: usize,
}

/// Read one bounded ledger tail and retain the final cumulative snapshot per
/// attempt. Completion reports and advisor training use this same identity.
pub(crate) fn request_history(path: &Path) -> RequestHistory {
    let mut history = RequestHistory::default();
    let Ok(mut file) = File::open(path) else {
        return history;
    };
    let Ok(metadata) = file.metadata() else {
        return history;
    };
    let offset = metadata.len().saturating_sub(MAX_LEDGER_BYTES);
    if file.seek(SeekFrom::Start(offset)).is_err() {
        return history;
    }
    history.available = true;
    history.sampled_tail = offset > 0;
    let mut reader = BufReader::new(file.take(MAX_LEDGER_BYTES));
    if offset > 0 && reader.read_line(&mut String::new()).is_err() {
        history.available = false;
        return history;
    }
    let mut positions = HashMap::new();
    let mut rows = Vec::new();
    for line in reader.lines() {
        let Ok(line) = line else {
            history.available = false;
            rows.push(Some(None));
            break;
        };
        let Ok(row) = serde_json::from_str::<RequestRow>(&line) else {
            rows.push(Some(None));
            continue;
        };
        if let Some(identity) = row.identity() {
            let is_legacy = matches!(identity, RequestIdentity::Legacy { .. });
            if let Some(before) = positions.get(&identity).copied() {
                history.duplicate_snapshots_ignored += 1;
                if is_legacy {
                    continue;
                }
                rows[before] = None;
            }
            positions.insert(identity, rows.len());
        }
        rows.push(Some(Some(row)));
    }
    history.rows = rows.into_iter().flatten().collect();
    history
}

/// Both local statistics views share a single read and snapshot selection.
#[must_use]
pub fn session_report(directory: &Path, include_advisor: bool) -> Value {
    let history = request_history(&directory.join("requests.jsonl"));
    let mut report = json!({"completion_intervals": intervals(&history)});
    if include_advisor {
        report["ttl_advisor"] = crate::cache_advisor::report_from_history(directory, &history);
    }
    report
}

#[derive(Debug, Default, Serialize)]
pub struct CompletionIntervals {
    pub timing_basis: &'static str,
    pub expiry_attribution: &'static str,
    pub available: bool,
    pub sampled_tail: bool,
    pub rows: u64,
    pub duplicate_rows_ignored: u64,
    pub malformed_rows: u64,
    pub missing_timestamps: u64,
    pub backwards_timestamps: u64,
    pub under_5m: u64,
    pub from_5m_to_1h: u64,
    pub over_1h: u64,
}

#[must_use]
pub fn completion_intervals(path: &Path) -> CompletionIntervals {
    intervals(&request_history(path))
}

fn intervals(history: &RequestHistory) -> CompletionIntervals {
    let mut report = CompletionIntervals {
        timing_basis: "response_completion",
        expiry_attribution: "unconfirmed",
        available: history.available,
        sampled_tail: history.sampled_tail,
        duplicate_rows_ignored: history.duplicate_snapshots_ignored as u64,
        ..CompletionIntervals::default()
    };
    let mut previous: Option<u64> = None;
    for row in &history.rows {
        let Some(row) = row else {
            report.malformed_rows += 1;
            previous = None;
            continue;
        };
        let Some(at) = row.at_unix_secs else {
            report.missing_timestamps += 1;
            previous = None;
            continue;
        };
        report.rows += 1;
        if let Some(before) = previous {
            if at < before {
                report.backwards_timestamps += 1;
                previous = Some(at);
                continue;
            }
            match at - before {
                0..300 => report.under_5m += 1,
                300..=3600 => report.from_5m_to_1h += 1,
                _ => report.over_1h += 1,
            }
        }
        previous = Some(at);
    }
    report
}
