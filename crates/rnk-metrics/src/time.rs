//! RFC 3339 timestamps for the report's `meta.start` and `meta.end`, without
//! a date library.

use std::time::{SystemTime, UNIX_EPOCH};

const SECS_PER_DAY: u64 = 86_400;

/// Formats a time as RFC 3339 in UTC with nanoseconds, for example
/// `2026-10-30T12:00:00.000000001Z`. Times before 1970 print as the epoch.
pub(crate) fn rfc3339(t: SystemTime) -> String {
    let since_epoch = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = since_epoch.as_secs();
    let (year, month, day) = civil_from_days(secs / SECS_PER_DAY);
    let rem = secs % SECS_PER_DAY;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:09}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        since_epoch.subsec_nanos(),
    )
}

/// Year, month and day of a count of days since 1970-01-01 (Howard Hinnant's
/// `civil_from_days`, restricted to non-negative days).
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn at(secs: u64, nanos: u32) -> String {
        rfc3339(UNIX_EPOCH + Duration::new(secs, nanos))
    }

    #[test]
    fn matches_date_u() {
        // Expected values from `date -u -d @<secs> +%FT%TZ`.
        assert_eq!(at(0, 0), "1970-01-01T00:00:00.000000000Z");
        assert_eq!(at(951_782_400, 0), "2000-02-29T00:00:00.000000000Z");
        assert_eq!(at(1_709_251_199, 1), "2024-02-29T23:59:59.000000001Z");
        assert_eq!(at(1_793_318_400, 500), "2026-10-30T00:00:00.000000500Z");
        assert_eq!(at(4_107_542_399, 0), "2100-02-28T23:59:59.000000000Z");
    }

    #[test]
    fn before_the_epoch_is_the_epoch() {
        let before = UNIX_EPOCH - Duration::from_secs(10);
        assert_eq!(rfc3339(before), "1970-01-01T00:00:00.000000000Z");
    }
}
