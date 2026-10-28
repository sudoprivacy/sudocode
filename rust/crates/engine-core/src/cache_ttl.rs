//! When a chosen prompt-cache TTL reaches this session's requests.
//!
//! Measured against the API: a request that grows the conversation and asks
//! for 1h on a prefix cached at 5m rewrites the whole prefix at the 1h write
//! price, because a 1h entry cannot extend a 5m one. Going from 1h to 5m keeps
//! the cached prefix. So a switch to 1h waits for the next cold point, where
//! the prefix is rewritten anyway, unless it is asked for now; a switch to 5m
//! applies to the next request.
//!
//! Without a pin, and with the cache TTL advisor on, the TTL follows this
//! session's own history: the advisor decides at context boundaries, at cold
//! points and every [`DECIDE_EVERY`] requests, and the same rule applies the
//! result. Switching to 1h early pays only if a 5-60 minute gap is nearly
//! certain before the session ends, so an automatic switch never does.
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::{Mutex, MutexGuard, OnceLock};

use serde::Deserialize;

use crate::cache_advisor::{Trigger, Verdict};

/// Bytes read from the end of the request ledger to find the last request.
const LEDGER_TAIL_BYTES: u64 = 16 * 1024;

/// Requests between an automatic TTL's decisions inside one context, so a
/// long session that never compacts still adapts.
pub const DECIDE_EVERY: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TtlChoice {
    OneHour,
    FiveMinutes,
    /// No pin. With the cache TTL advisor on, this session's own history
    /// picks the TTL; otherwise the auth mode's default: 1h on a
    /// subscription, 5m on a key.
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
    /// No TTL is pinned: this session's history picks it.
    pub auto: bool,
    /// Automatic switches applied so far in this process.
    pub switches: u64,
}

/// Where the advisor reads this session's history, for automatic decisions
/// and for the last request after a restart.
pub(crate) struct AdvisorInput<'a> {
    pub directory: &'a Path,
    pub segment: u64,
    pub model: &'a str,
    pub route: &'a str,
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
    /// `Some(false)`: the TTL follows this session's history. Set from whether
    /// a setting pinned the client, and by a choice.
    pinned: Option<bool>,
    /// The latest decision, while automatic, and whether a boundary made it
    /// since the last request, so the cold request after it need not repeat it.
    verdict: Option<Verdict>,
    fresh: bool,
    since_decision: usize,
    /// Apply the next automatic switch now rather than at a cold point.
    force_next: bool,
    switches: u64,
    /// Switches already reported by [`take_switch_notice`].
    announced: u64,
}

fn sessions() -> MutexGuard<'static, HashMap<String, Entry>> {
    static SESSIONS: OnceLock<Mutex<HashMap<String, Entry>>> = OnceLock::new();
    SESSIONS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Whether an unpinned TTL follows the session's history. The history is
/// recorded only under this experiment.
fn automatic_enabled() -> bool {
    runtime::experiments::is_enabled(runtime::experiments::Experiment::CacheTtlAdvisor)
}

/// Record a choice for this session's next suitable request. `Auto` with the
/// advisor on unpins the TTL and decides afresh at the next request.
pub fn choose(session_id: &str, choice: TtlChoice, force_now: bool) {
    let mut map = sessions();
    let entry = map.entry(session_id.to_owned()).or_default();
    if choice == TtlChoice::Auto && automatic_enabled() {
        entry.pinned = Some(false);
        entry.pending = None;
        entry.force_next = force_now;
        entry.fresh = false;
        entry.since_decision = DECIDE_EVERY;
    } else {
        entry.pinned = Some(true);
        entry.pending = Some(PendingTtl { choice, force_now });
    }
}

#[must_use]
pub fn status(session_id: &str) -> TtlStatus {
    sessions()
        .get(session_id)
        .map_or_else(TtlStatus::default, |entry| TtlStatus {
            effective_seconds: entry.last.map(|last| last.ttl_seconds),
            pending: entry.pending,
            last_request_ms: entry.last.map(|last| last.started_ms),
            auto: entry.pinned == Some(false),
            switches: entry.switches,
        })
}

/// Whether an automatic switch happened since this was last asked: the first
/// tip after it says what changed, later ones don't repeat it.
#[must_use]
pub fn take_switch_notice(session_id: &str) -> bool {
    sessions().get_mut(session_id).is_some_and(|entry| {
        let new = entry.switches > entry.announced;
        entry.announced = entry.switches;
        new
    })
}

/// The runtime's context segment; a change means the next request starts a
/// new context. `pinned` says whether a setting pinned the client's TTL; a
/// choice made in this process takes precedence.
pub(crate) fn context_boundary(session_id: &str, segment: u64, pinned: bool) {
    let mut map = sessions();
    let entry = map.entry(session_id.to_owned()).or_default();
    if entry.segment.is_some_and(|known| known != segment) {
        entry.new_context = true;
    }
    entry.segment = Some(segment);
    entry.pinned.get_or_insert(pinned || !automatic_enabled());
}

/// The advisor's verdict at a context boundary, for an automatic TTL, and
/// whether it was decided just now. A kept one (a rebuilt or resumed context)
/// does not excuse the next request from deciding if that request is cold.
pub(crate) fn advised(session_id: &str, verdict: Verdict, decided: bool) {
    let mut map = sessions();
    let entry = map.entry(session_id.to_owned()).or_default();
    if entry.pinned == Some(false) {
        entry.verdict = Some(verdict);
        entry.fresh = decided;
        entry.since_decision = 0;
    }
}

/// Decide, if automatic and due, apply a pending choice if this is the moment
/// for it, then record this request as the session's last one.
///
/// Cold means the previous request's prefix cannot be read any more: it was
/// sent longer ago than the TTL it was sent with, or a new context began (a
/// compaction rewrote the conversation), or there was none. The
/// first request of a process falls back to the ledger, so a quick restart is
/// not mistaken for a cold cache.
pub(crate) fn before_request(
    session_id: &str,
    ttl: &api::CacheTtl,
    advisor: Option<&AdvisorInput<'_>>,
    now_ms: u64,
) {
    let mut map = sessions();
    let entry = map.entry(session_id.to_owned()).or_default();
    let pinned = *entry
        .pinned
        .get_or_insert(ttl.pinned() || !automatic_enabled());
    let automatic = (!pinned).then_some(advisor).flatten();
    let last = entry
        .last
        .or_else(|| advisor.and_then(|advisor| last_from_ledger(advisor.directory)));
    // A client built from the saved setting (a restart, or a runtime rebuilt
    // for /model or /auth) may ask for a TTL the previous request did not use.
    // That is a switch too: it goes through the same rule rather than straight
    // to the wire, and a choice already pending stays the one that counts.
    // An automatic TTL ignores the client's starting value altogether.
    if let Some(last) = last {
        let wanted = ttl.one_hour();
        if (last.ttl_seconds == 3600) != wanted {
            ttl.set_one_hour(last.ttl_seconds == 3600);
            if automatic.is_none() {
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
    }
    let cold = entry.new_context
        || last.is_none_or(|last| {
            now_ms.saturating_sub(last.started_ms) >= u64::from(last.ttl_seconds) * 1000
        });
    if let Some(advisor) = automatic {
        let due = entry.verdict.is_none()
            || (cold && !entry.fresh)
            || entry.since_decision >= DECIDE_EVERY;
        if due {
            entry.verdict = Some(crate::cache_advisor::decide(
                advisor.directory,
                advisor.segment,
                advisor.model,
                advisor.route,
                ttl.seconds(),
                if cold {
                    Trigger::Cold
                } else {
                    Trigger::Periodic
                },
            ));
            entry.since_decision = 0;
        }
        let one_hour = match entry.verdict {
            Some(Verdict::Seconds(seconds)) => seconds == 3600,
            Some(Verdict::Keep) => ttl.one_hour(),
            Some(Verdict::NoEvidence) | None => false,
        };
        entry.pending = (one_hour != ttl.one_hour()).then_some(PendingTtl {
            choice: if one_hour {
                TtlChoice::OneHour
            } else {
                TtlChoice::FiveMinutes
            },
            force_now: entry.force_next,
        });
        entry.force_next = false;
    }
    entry.fresh = false;
    if let Some(pending) = entry.pending {
        let one_hour = match pending.choice {
            TtlChoice::OneHour => true,
            TtlChoice::FiveMinutes => false,
            TtlChoice::Auto => ttl.default_one_hour(),
        };
        if !one_hour || pending.force_now || cold || one_hour == ttl.one_hour() {
            ttl.set_one_hour(one_hour);
            entry.pending = None;
        }
    }
    if automatic.is_some() {
        // The first request ever sent is not a switch, whatever the client
        // started from.
        if last.is_some_and(|last| (last.ttl_seconds == 3600) != ttl.one_hour()) {
            entry.switches += 1;
        }
        entry.since_decision = entry.since_decision.saturating_add(1);
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
