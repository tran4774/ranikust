//! The RFC 3339 timestamps of image configs (`created`), written as Go's
//! `RFC3339Nano` writes them and parsed from untrusted documents.

use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::{Error, excerpt};

/// A point in time as an image config's `created` holds it.
///
/// An input, never read from the clock here: a caller makes one from a
/// [`SystemTime`] it read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp {
    /// Seconds since the Unix epoch, negative before it.
    seconds: i64,
    /// Nanoseconds within the second, below 1e9.
    nanos: u32,
}

/// Seconds of 0001-01-01T00:00:00Z and 9999-12-31T23:59:59Z, the range a
/// four-digit year spells.
const MIN_SECONDS: i64 = -62_135_596_800;
const MAX_SECONDS: i64 = 253_402_300_799;

impl Timestamp {
    /// `time`, clamped to the years 1 to 9999.
    #[must_use]
    pub fn from_system_time(time: SystemTime) -> Self {
        let (seconds, nanos) = match time.duration_since(UNIX_EPOCH) {
            Ok(after) => (
                i64::try_from(after.as_secs()).unwrap_or(i64::MAX),
                after.subsec_nanos(),
            ),
            Err(before) => {
                let before = before.duration();
                let seconds = i64::try_from(before.as_secs()).unwrap_or(i64::MAX);
                match before.subsec_nanos() {
                    0 => (-seconds, 0),
                    n => (-seconds - 1, 1_000_000_000 - n),
                }
            }
        };
        if seconds < MIN_SECONDS {
            Self {
                seconds: MIN_SECONDS,
                nanos: 0,
            }
        } else if seconds > MAX_SECONDS {
            Self {
                seconds: MAX_SECONDS,
                nanos: 999_999_999,
            }
        } else {
            Self { seconds, nanos }
        }
    }

    /// The same point as a [`SystemTime`].
    #[must_use]
    pub fn to_system_time(self) -> SystemTime {
        let nanos = Duration::from_nanos(u64::from(self.nanos));
        let whole = Duration::from_secs(self.seconds.unsigned_abs());
        // Within years 1 to 9999, which `SystemTime` holds on every target
        // ranikust builds for.
        let at = if self.seconds >= 0 {
            UNIX_EPOCH.checked_add(whole)
        } else {
            UNIX_EPOCH.checked_sub(whole)
        };
        at.and_then(|t| t.checked_add(nanos)).unwrap_or(UNIX_EPOCH)
    }

    /// Seconds since the Unix epoch, negative before it.
    #[must_use]
    pub fn unix_seconds(self) -> i64 {
        self.seconds
    }

    /// Parses an RFC 3339 date-time: `YYYY-MM-DDTHH:MM:SS`, an optional
    /// fraction of up to nine digits, then `Z` or an offset `±HH:MM`, as
    /// Go's `time.RFC3339` reads it. A leap second (`:60`) is refused, as
    /// Go refuses it.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidTimestamp`] for anything else.
    pub fn parse_rfc3339(text: &str) -> Result<Self, Error> {
        let bad = |reason| Error::InvalidTimestamp {
            input: excerpt(text),
            reason,
        };
        let b = text.as_bytes();
        if b.len() < 20 {
            return Err(bad("too short for YYYY-MM-DDTHH:MM:SSZ"));
        }
        if b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' {
            return Err(bad("not YYYY-MM-DDTHH:MM:SS"));
        }
        let year = digits(&b[0..4]).ok_or_else(|| bad("bad year"))?;
        let month = digits(&b[5..7]).ok_or_else(|| bad("bad month"))?;
        let day = digits(&b[8..10]).ok_or_else(|| bad("bad day"))?;
        let hour = digits(&b[11..13]).ok_or_else(|| bad("bad hour"))?;
        let minute = digits(&b[14..16]).ok_or_else(|| bad("bad minute"))?;
        let second = digits(&b[17..19]).ok_or_else(|| bad("bad second"))?;
        if year == 0 || !(1..=12).contains(&month) {
            return Err(bad("date out of range"));
        }
        if day == 0 || day > days_in_month(year, month) {
            return Err(bad("date out of range"));
        }
        if hour > 23 || minute > 59 || second > 59 {
            return Err(bad("time out of range"));
        }
        let mut rest = &b[19..];
        let mut nanos = 0u32;
        if let Some(fraction) = rest.strip_prefix(b".") {
            let n = fraction.iter().take_while(|c| c.is_ascii_digit()).count();
            if n == 0 || n > 9 {
                return Err(bad("a fraction of 1 to 9 digits"));
            }
            let value = digits(&fraction[..n]).ok_or_else(|| bad("bad fraction"))?;
            let scale = 10u32.pow(u32::try_from(9 - n).unwrap_or(0));
            nanos = u32::try_from(value).unwrap_or(0) * scale;
            rest = &fraction[n..];
        }
        let offset = match rest {
            b"Z" => 0,
            [sign @ (b'+' | b'-'), h1, h2, b':', m1, m2] => {
                let hours = digits(&[*h1, *h2]).ok_or_else(|| bad("bad offset"))?;
                let minutes = digits(&[*m1, *m2]).ok_or_else(|| bad("bad offset"))?;
                if hours > 23 || minutes > 59 {
                    return Err(bad("offset out of range"));
                }
                let offset = hours * 3600 + minutes * 60;
                if *sign == b'-' { -offset } else { offset }
            }
            _ => return Err(bad("not Z or an offset ±HH:MM")),
        };
        let days = days_from_civil(year, month, day);
        let seconds = days * 86_400 + hour * 3600 + minute * 60 + second - offset;
        if !(MIN_SECONDS..=MAX_SECONDS).contains(&seconds) {
            return Err(bad("date out of range"));
        }
        Ok(Self { seconds, nanos })
    }
}

/// RFC 3339 in UTC with the fraction Go's `RFC3339Nano` writes: trailing
/// zeros dropped, none when zero.
impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let days = self.seconds.div_euclid(86_400);
        let secs = self.seconds.rem_euclid(86_400);
        let (year, month, day) = civil_from_days(days);
        write!(
            f,
            "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}",
            secs / 3600,
            secs % 3600 / 60,
            secs % 60
        )?;
        if self.nanos != 0 {
            let fraction = format!("{:09}", self.nanos);
            write!(f, ".{}", fraction.trim_end_matches('0'))?;
        }
        f.write_str("Z")
    }
}

/// A run of ASCII digits as a number; `None` for anything else.
fn digits(b: &[u8]) -> Option<i64> {
    if b.is_empty() || b.len() > 9 || !b.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(b.iter().fold(0i64, |n, &d| n * 10 + i64::from(d - b'0')))
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The proleptic Gregorian date of a day count since 1970-01-01 (Howard
/// Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(month <= 2), month, day)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn at(seconds: i64, nanos: u32) -> Timestamp {
        Timestamp { seconds, nanos }
    }

    #[test]
    fn formats_like_go_rfc3339_nano() {
        assert_eq!(at(0, 0).to_string(), "1970-01-01T00:00:00Z");
        assert_eq!(at(951_782_400, 0).to_string(), "2000-02-29T00:00:00Z");
        assert_eq!(
            at(1_793_312_675, 120_000_000).to_string(),
            "2026-10-29T22:24:35.12Z"
        );
        assert_eq!(
            at(4_102_444_799, 999_999_999).to_string(),
            "2099-12-31T23:59:59.999999999Z"
        );
        assert_eq!(at(-1, 500_000_000).to_string(), "1969-12-31T23:59:59.5Z");
    }

    #[test]
    fn parses_what_it_writes_and_offsets() {
        for (text, seconds, nanos) in [
            ("1970-01-01T00:00:00Z", 0, 0),
            ("2026-10-29T22:24:35.12Z", 1_793_312_675, 120_000_000),
            ("2000-02-29T00:00:00+01:00", 951_778_800, 0),
            ("1999-12-31T23:30:00-00:30", 946_684_800, 0),
            ("0001-01-01T00:00:00Z", MIN_SECONDS, 0),
            ("9999-12-31T23:59:59.999999999Z", MAX_SECONDS, 999_999_999),
            ("2023-11-14T22:13:20.000000001Z", 1_700_000_000, 1),
        ] {
            assert_eq!(
                Timestamp::parse_rfc3339(text).unwrap(),
                at(seconds, nanos),
                "{text}"
            );
        }
    }

    #[test]
    fn refuses_what_rfc3339_does_not_allow() {
        for text in [
            "",
            "2026-10-29",
            "2026-10-29 22:24:35Z",
            "2026-10-29T22:24:35",
            "2026-13-01T00:00:00Z",
            "2026-02-29T00:00:00Z",
            "1900-02-29T00:00:00Z",
            "2026-04-31T00:00:00Z",
            "0000-01-01T00:00:00Z",
            "2026-01-01T24:00:00Z",
            "2026-01-01T00:60:00Z",
            "2026-01-01T00:00:60Z",
            "2026-01-01T00:00:00.Z",
            "2026-01-01T00:00:00.1234567890Z",
            "2026-01-01T00:00:00+24:00",
            "2026-01-01T00:00:00+0100",
            "2026-01-01T00:00:00Zjunk",
            "+026-01-01T00:00:00Z",
            "2026-01-01T00:00:00z",
            "２０２６-01-01T00:00:00Z",
        ] {
            assert!(
                matches!(
                    Timestamp::parse_rfc3339(text),
                    Err(Error::InvalidTimestamp { .. })
                ),
                "{text:?}"
            );
        }
        let long = format!("2026-01-01T00:00:00Z{}", "x".repeat(10_000));
        let message = Timestamp::parse_rfc3339(&long).unwrap_err().to_string();
        assert!(message.len() < 200, "{}", message.len());
    }

    #[test]
    fn system_times_round_trip_and_clamp() {
        let t = UNIX_EPOCH + Duration::new(1_700_000_000, 123);
        assert_eq!(Timestamp::from_system_time(t).to_system_time(), t);
        let before = UNIX_EPOCH - Duration::new(10, 250_000_000);
        let stamp = Timestamp::from_system_time(before);
        assert_eq!(stamp, at(-11, 750_000_000));
        assert_eq!(stamp.to_system_time(), before);
        let far = UNIX_EPOCH + Duration::from_secs(1 << 40);
        assert_eq!(
            Timestamp::from_system_time(far).to_string(),
            "9999-12-31T23:59:59.999999999Z"
        );
    }

    proptest! {
        #[test]
        fn any_time_in_range_round_trips(
            seconds in MIN_SECONDS..=MAX_SECONDS,
            nanos in 0u32..1_000_000_000,
        ) {
            let stamp = at(seconds, nanos);
            prop_assert_eq!(Timestamp::parse_rfc3339(&stamp.to_string()).unwrap(), stamp);
        }

        #[test]
        fn any_text_parses_or_fails_without_panic(text in "\\PC{0,40}") {
            let _ = Timestamp::parse_rfc3339(&text);
        }
    }
}
