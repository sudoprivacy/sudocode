//! When a chosen prompt-cache TTL reaches this session's requests.
//!
//! Measured against the API: a request that grows the conversation and asks
//! for 1h on a prefix cached at 5m rewrites the whole prefix at the 1h write
//! price, because a 1h entry cannot extend a 5m one. Going from 1h to 5m keeps
//! the cached prefix. So a switch to 1h waits for the next cold point, where
//! the prefix is rewritten anyway, unless it is asked for now; a switch to 5m
//! applies to the next request.
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::{Mutex, MutexGuard, OnceLock};

use serde::Deserialize;

/// Bytes read from the end of the request ledger to find the last request.
const LEDGER_TAIL_BYTES: u64 = 16 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TtlChoice {
    OneHour,
    FiveMinutes,
    /// Whatever the auth mode picks: 1h on a subscription, 5m on a key.
    Auto,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PendingTtl {
    pub choice: TtlChoice,
    pub force_now: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TtlStatus {
    /// The TTL the last request of this session was sent with.
    pub effective_seconds: Option<u32>,
    /// A choice not yet applied to a request.
    pub pending: Option<PendingTtl>,
    /// When that last request started, in unix milliseconds. Its prefix is
    /// cold once this is older than `effective_seconds`.
    pub last_request_ms: Option<u64>,
}

#[derive(Clone, Copy)]
struct LastRequest {
    started_ms: u64,
    ttl_seconds: u32,
}

#[derive(Default)]
struct Entry {
    pending: Option<PendingTtl>,
    last: Option<LastRequest>,
    /// The context segment the runtime last reported, and whether it moved:
    /// a committed compaction rewrites the prefix whatever the TTL. Rebuilding
    /// or resuming the same context reports the same segment.
    segment: Option<u64>,
    new_context: bool,
}

fn sessions() -> MutexGuard<'static, HashMap<String, Entry>> {
    static SESSIONS: OnceLock<Mutex<HashMap<String, Entry>>> = OnceLock::new();
    SESSIONS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Record a choice for this session's next suitable request.
pub fn choose(session_id: &str, choice: TtlChoice, force_now: bool) {
    sessions().entry(session_id.to_owned()).or_default().pending =
        Some(PendingTtl { choice, force_now });
}

#[must_use]
pub fn status(session_id: &str) -> TtlStatus {
    sessions()
        .get(session_id)
        .map_or_else(TtlStatus::default, |entry| TtlStatus {
            effective_seconds: entry.last.map(|last| last.ttl_seconds),
            pending: entry.pending,
            last_request_ms: entry.last.map(|last| last.started_ms),
        })
}

/// The runtime's context segment; a change means the next request starts a
/// new context.
pub(crate) fn context_boundary(session_id: &str, segment: u64) {
    let mut map = sessions();
    let entry = map.entry(session_id.to_owned()).or_default();
    if entry.segment.is_some_and(|known| known != segment) {
        entry.new_context = true;
    }
    entry.segment = Some(segment);
}

/// Apply a pending choice if this is the moment for it, then record this
/// request as the session's last one.
///
/// Cold means the previous request's prefix cannot be read any more: it was
/// sent longer ago than the TTL it was sent with, or a new context began (a
/// compaction rewrote the conversation), or there was none. The
/// first request of a process falls back to the ledger, so a quick restart is
/// not mistaken for a cold cache.
pub(crate) fn before_request(
    session_id: &str,
    ttl: &api::CacheTtl,
    ledger_dir: Option<&Path>,
    now_ms: u64,
) {
    let mut map = sessions();
    let entry = map.entry(session_id.to_owned()).or_default();
    let last = entry.last.or_else(|| ledger_dir.and_then(last_from_ledger));
    // A client built from the saved setting (a restart, or a runtime rebuilt
    // for /model or /auth) may ask for a TTL the previous request did not use.
    // That is a switch too: it goes through the same rule rather than straight
    // to the wire, and a choice already pending stays the one that counts.
    if let Some(last) = last {
        let wanted = ttl.one_hour();
        if (last.ttl_seconds == 3600) != wanted {
            ttl.set_one_hour(last.ttl_seconds == 3600);
            entry.pending.get_or_insert(PendingTtl {
                choice: if wanted {
                    TtlChoice::OneHour
                } else {
                    TtlChoice::FiveMinutes
                },
                force_now: false,
            });
        }
    }
    if let Some(pending) = entry.pending {
        let one_hour = match pending.choice {
            TtlChoice::OneHour => true,
            TtlChoice::FiveMinutes => false,
            TtlChoice::Auto => ttl.default_one_hour(),
        };
        let cold = entry.new_context
            || last.is_none_or(|last| {
                now_ms.saturating_sub(last.started_ms) >= u64::from(last.ttl_seconds) * 1000
            });
        if !one_hour || pending.force_now || cold || one_hour == ttl.one_hour() {
            ttl.set_one_hour(one_hour);
            entry.pending = None;
        }
    }
    entry.new_context = false;
    entry.last = Some(LastRequest {
        started_ms: now_ms,
        ttl_seconds: ttl.seconds(),
    });
}

#[derive(Deserialize)]
struct LedgerRow {
    observation: Option<LedgerObservation>,
}

#[derive(Deserialize)]
struct LedgerObservation {
    request_started_ms: u64,
    #[serde(default)]
    requested_ttls_seconds: Vec<Option<u32>>,
}

fn last_from_ledger(directory: &Path) -> Option<LastRequest> {
    let mut file = File::open(directory.join("requests.jsonl")).ok()?;
    let length = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(length.saturating_sub(LEDGER_TAIL_BYTES)))
        .ok()?;
    let mut bytes = Vec::new();
    file.take(LEDGER_TAIL_BYTES).read_to_end(&mut bytes).ok()?;
    // The tail may start inside a character; only whole lines are parsed.
    String::from_utf8_lossy(&bytes)
        .lines()
        .rev()
        .find_map(|line| {
            let observation = serde_json::from_str::<LedgerRow>(line).ok()?.observation?;
            (observation.request_started_ms > 0).then(|| LastRequest {
                started_ms: observation.request_started_ms,
                ttl_seconds: observation
                    .requested_ttls_seconds
                    .iter()
                    .flatten()
                    .copied()
                    .next()
                    .unwrap_or(300),
            })
        })
}
