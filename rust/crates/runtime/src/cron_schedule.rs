//! Pure schedule math for the cron scheduler.
//!
//! Turns a [`CronEntry`]'s `schedule` + `kind` into a concrete "next fire
//! time" and decides whether an entry is due. No I/O and no firing —
//! those live in the scheduler/CLI. Kept pure so it is exhaustively
//! unit-testable with fixed timestamps.

use std::str::FromStr;

use chrono::{TimeZone, Utc};
use chrono_tz::Tz;

use crate::cron_registry::{CronEntry, CronKind};

/// Validate a `(kind, schedule, tz)` triple at create time so the CLI /
/// tool can reject bad input up front rather than silently never firing.
pub fn validate(kind: CronKind, schedule: &str, tz: Option<&str>) -> Result<(), String> {
    match kind {
        CronKind::Cron => {
            let six = to_six_field(schedule)
                .ok_or_else(|| format!("cron schedule must have 5 fields, got: {schedule:?}"))?;
            cron::Schedule::from_str(&six)
                .map_err(|e| format!("invalid cron expression {schedule:?}: {e}"))?;
            if let Some(tzname) = tz {
                tzname
                    .parse::<Tz>()
                    .map_err(|_| format!("unknown timezone: {tzname:?}"))?;
            }
            Ok(())
        }
        CronKind::Every => {
            let secs = schedule
                .trim()
                .parse::<u64>()
                .map_err(|_| format!("`every` schedule must be integer seconds, got: {schedule:?}"))?;
            if secs == 0 {
                return Err("`every` interval must be > 0 seconds".to_owned());
            }
            Ok(())
        }
        CronKind::At => {
            schedule
                .trim()
                .parse::<u64>()
                .map_err(|_| format!("`at` schedule must be a unix timestamp, got: {schedule:?}"))?;
            Ok(())
        }
    }
}

/// Next fire time (unix secs) strictly AFTER `after_secs`, or `None` if
/// the entry can never fire again (e.g. a one-shot already in the past,
/// or an unparseable schedule).
#[must_use]
pub fn next_run_after(entry: &CronEntry, after_secs: u64) -> Option<u64> {
    match entry.kind {
        CronKind::Cron => next_cron(&entry.schedule, entry.tz.as_deref(), after_secs),
        CronKind::Every => {
            let interval = entry.schedule.trim().parse::<u64>().ok().filter(|n| *n > 0)?;
            Some(after_secs.saturating_add(interval))
        }
        CronKind::At => {
            let at = entry.schedule.trim().parse::<u64>().ok()?;
            (at > after_secs).then_some(at)
        }
    }
}

/// Is this entry due to fire at `now_secs`? Enabled and its scheduler-
/// maintained `next_run_at` has arrived. Entries whose `next_run_at` is
/// unset are NOT due until the scheduler computes it.
#[must_use]
pub fn is_due(entry: &CronEntry, now_secs: u64) -> bool {
    entry.enabled && entry.next_run_at.is_some_and(|n| n <= now_secs)
}

/// A one-shot (`At`) entry disables itself once fired.
#[must_use]
pub fn is_one_shot(entry: &CronEntry) -> bool {
    entry.kind == CronKind::At
}

fn next_cron(expr5: &str, tz: Option<&str>, after_secs: u64) -> Option<u64> {
    let six = to_six_field(expr5)?;
    let schedule = cron::Schedule::from_str(&six).ok()?;
    let after_utc = Utc.timestamp_opt(after_secs as i64, 0).single()?;
    match tz {
        Some(tzname) => {
            let tz: Tz = tzname.parse().ok()?;
            let after_tz = after_utc.with_timezone(&tz);
            let next = schedule.after(&after_tz).next()?;
            Some(next.with_timezone(&Utc).timestamp().max(0) as u64)
        }
        None => {
            // No explicit tz → local machine time (matches CC's default).
            let after_local = after_utc.with_timezone(&chrono::Local);
            let next = schedule.after(&after_local).next()?;
            Some(next.with_timezone(&Utc).timestamp().max(0) as u64)
        }
    }
}

/// The `cron` crate wants a 6-field expression (with a leading seconds
/// field); CC-standard input is 5-field. Prepend `0` seconds. Accept an
/// already-6-field expression too, for forward flexibility.
fn to_six_field(expr: &str) -> Option<String> {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    match fields.len() {
        5 => Some(format!("0 {}", fields.join(" "))),
        6 => Some(fields.join(" ")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cron_registry::CronRegistry;

    fn entry(schedule: &str, kind: CronKind, tz: Option<&str>) -> CronEntry {
        let reg = CronRegistry::new();
        reg.create_full(crate::cron_registry::CronCreateParams {
            schedule: schedule.to_owned(),
            kind,
            prompt: "p".to_owned(),
            tz: tz.map(str::to_owned),
            ..Default::default()
        })
    }

    // 2026-01-01 00:00:00 UTC = 1767225600
    const T_2026_NEW_YEAR_UTC: u64 = 1_767_225_600;

    #[test]
    fn cron_top_of_hour_utc() {
        let e = entry("0 * * * *", CronKind::Cron, Some("UTC"));
        // 00:00:30 → next top-of-hour is 01:00:00
        let next = next_run_after(&e, T_2026_NEW_YEAR_UTC + 30).unwrap();
        assert_eq!(next, T_2026_NEW_YEAR_UTC + 3600);
    }

    #[test]
    fn cron_daily_9am_in_tz() {
        // "0 9 * * *" in Asia/Shanghai (UTC+8) fires at 01:00 UTC.
        let e = entry("0 9 * * *", CronKind::Cron, Some("Asia/Shanghai"));
        let next = next_run_after(&e, T_2026_NEW_YEAR_UTC).unwrap();
        // 2026-01-01 09:00 +08:00 == 2026-01-01 01:00:00 UTC
        assert_eq!(next, T_2026_NEW_YEAR_UTC + 3600);
    }

    #[test]
    fn every_interval_from_after() {
        let e = entry("300", CronKind::Every, None);
        let next = next_run_after(&e, 1_000).unwrap();
        assert_eq!(next, 1_300);
    }

    #[test]
    fn at_future_then_past() {
        let e = entry("2000", CronKind::At, None);
        assert_eq!(next_run_after(&e, 1_000), Some(2_000));
        // once we're past the timestamp it never fires again
        assert_eq!(next_run_after(&e, 2_000), None);
        assert_eq!(next_run_after(&e, 2_001), None);
    }

    #[test]
    fn is_due_respects_enabled_and_next_run() {
        let mut e = entry("0 * * * *", CronKind::Cron, Some("UTC"));
        e.next_run_at = Some(1_000);
        assert!(!is_due(&e, 999));
        assert!(is_due(&e, 1_000));
        assert!(is_due(&e, 1_001));
        e.enabled = false;
        assert!(!is_due(&e, 1_001));
        e.enabled = true;
        e.next_run_at = None;
        assert!(!is_due(&e, 1_001)); // never computed → not due
    }

    #[test]
    fn validation() {
        assert!(validate(CronKind::Cron, "0 * * * *", None).is_ok());
        assert!(validate(CronKind::Cron, "*/15 9-17 * * 1-5", Some("UTC")).is_ok());
        assert!(validate(CronKind::Cron, "not a cron", None).is_err());
        assert!(validate(CronKind::Cron, "0 * * *", None).is_err()); // 4 fields
        assert!(validate(CronKind::Cron, "0 * * * *", Some("Mars/Phobos")).is_err());
        assert!(validate(CronKind::Every, "60", None).is_ok());
        assert!(validate(CronKind::Every, "0", None).is_err());
        assert!(validate(CronKind::Every, "abc", None).is_err());
        assert!(validate(CronKind::At, "1767225600", None).is_ok());
        assert!(validate(CronKind::At, "soon", None).is_err());
    }

    #[test]
    fn is_one_shot_only_for_at() {
        assert!(is_one_shot(&entry("100", CronKind::At, None)));
        assert!(!is_one_shot(&entry("0 * * * *", CronKind::Cron, None)));
        assert!(!is_one_shot(&entry("60", CronKind::Every, None)));
    }
}
