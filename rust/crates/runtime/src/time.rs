//! Date/time helpers used by prompt assembly and runtime turn dispatch.
//!
//! Centralised here so the system prompt and the conversation runtime agree
//! on what "today" means without duplicating chrono usage across crates.

use chrono::Local;

/// Returns today's local calendar date as `YYYY-MM-DD`.
///
/// Used both when freezing the session-start date into the cacheable system
/// prompt and when the conversation runtime checks for an inter-turn date
/// rollover.
#[must_use]
pub fn today_local() -> String {
    Local::now().format("%Y-%m-%d").to_string()
}

#[cfg(test)]
mod tests {
    use super::today_local;

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
}
