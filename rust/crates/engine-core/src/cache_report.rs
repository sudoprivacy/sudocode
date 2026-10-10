//! Bounded, read-only diagnostics over the local request ledger.
//! Completion intervals are observed timings, not idle time or proof of expiry.
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use serde::Serialize;

const MAX_LEDGER_BYTES: u64 = 512 * 1024;

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
    let mut report = CompletionIntervals {
        timing_basis: "response_completion",
        expiry_attribution: "unconfirmed",
        ..CompletionIntervals::default()
    };
    let Ok(mut file) = File::open(path) else {
        return report;
    };
    let Ok(metadata) = file.metadata() else {
        return report;
    };
    let offset = metadata.len().saturating_sub(MAX_LEDGER_BYTES);
    if file.seek(SeekFrom::Start(offset)).is_err() {
        return report;
    }
    report.available = true;
    report.sampled_tail = offset > 0;
    let mut reader = BufReader::new(file.take(MAX_LEDGER_BYTES));
    if offset > 0 && reader.read_line(&mut String::new()).is_err() {
        report.available = false;
        return report;
    }
    let mut previous: Option<u64> = None;
    let mut seen = HashSet::new();
    for line in reader.lines() {
        let Ok(line) = line else {
            report.available = false;
            break;
        };
        let Ok(row) = serde_json::from_str::<serde_json::Value>(&line) else {
            report.malformed_rows += 1;
            previous = None;
            continue;
        };
        let Some(at) = row.get("at_unix_secs").and_then(serde_json::Value::as_u64) else {
            report.missing_timestamps += 1;
            previous = None;
            continue;
        };
        // Deduplicate only rows with a correlation ID AND identical timestamp
        // and usage. Distinct attempts must not collapse into one logical turn.
        let id = row
            .get("gateway_request_id")
            .and_then(|v| v.as_str())
            .filter(|v| !v.is_empty())
            .or_else(|| {
                row.get("provider_request_id")
                    .and_then(|v| v.as_str())
                    .filter(|v| !v.is_empty())
            });
        if let Some(id) = id {
            let identity = (
                id.to_owned(),
                at,
                row.get("model").cloned(),
                row.get("provider_request_id").cloned(),
                row.get("input_tokens").cloned(),
                row.get("cache_read_input_tokens").cloned(),
                row.get("cache_creation_input_tokens").cloned(),
            );
            if !seen.insert(identity) {
                report.duplicate_rows_ignored += 1;
                continue;
            }
        }
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
