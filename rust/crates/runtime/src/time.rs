//! Date/time helpers used by prompt assembly and runtime turn dispatch.
//!
//! Centralised here so the system prompt and the conversation runtime agree
//! on what "today" means without duplicating chrono usage across crates.

use chrono::{DateTime, Local};

/// Returns today's local calendar date as `YYYY-MM-DD`.
///
/// Used both when freezing the session-start date into the cacheable system
/// prompt and when the conversation runtime checks for an inter-turn date
/// rollover.
#[must_use]
pub fn today_local() -> String {
    Local::now().format("%Y-%m-%d").to_string()
}

/// Converts a Unix epoch millisecond timestamp into the corresponding local
/// calendar date as `YYYY-MM-DD`.
///
/// Used to recover the session-start date from `Session::created_at_ms` when
/// a session is resumed days later — passing `today_local()` then would set
/// `prompt_known_date` to today and silently suppress the rollover reminder.
#[must_use]
pub fn local_date_from_millis(ms: u64) -> String {
    let secs = i64::try_from(ms / 1000).unwrap_or(i64::MAX);
    let nanos = u32::try_from((ms % 1000) * 1_000_000).unwrap_or(0);
    DateTime::from_timestamp(secs, nanos)
        .map(|dt| dt.with_timezone(&Local).format("%Y-%m-%d").to_string())
        .unwrap_or_else(today_local)
}

#[cfg(test)]
mod tests {
    use super::{local_date_from_millis, today_local};
    use chrono::{Local, TimeZone};

    #[test]
    fn today_local_returns_iso_date() {
        let date = today_local();
        assert_eq!(date.len(), 10, "expected YYYY-MM-DD format, got {date}");
        let bytes = date.as_bytes();
        assert!(bytes[4] == b'-' && bytes[7] == b'-', "got {date}");
        assert!(date[..4].chars().all(|c| c.is_ascii_digit()), "got {date}");
        assert!(date[5..7].chars().all(|c| c.is_ascii_digit()), "got {date}");
        assert!(date[8..].chars().all(|c| c.is_ascii_digit()), "got {date}");
    }

    #[test]
    fn local_date_from_millis_matches_local_calendar() {
        // Pick a timestamp far from a UTC midnight so DST/timezone shifts
        // don't cross the calendar boundary: 2026-05-15T12:00:00 in local TZ.
        let expected = Local
            .with_ymd_and_hms(2026, 5, 15, 12, 0, 0)
            .single()
            .expect("unambiguous local datetime");
        let millis = u64::try_from(expected.timestamp_millis()).expect("non-negative epoch ms");
        assert_eq!(local_date_from_millis(millis), "2026-05-15");
    }
}
