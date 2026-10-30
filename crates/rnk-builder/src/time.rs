//! The one time a build stamps on what it writes.
//!
//! kaniko stamps the pushed image with the current time
//! (`kaniko@44aa7ddc0 pkg/executor/build.go:2100`) and, under
//! `--reproducible`, rewrites every timestamp to a fixed one through
//! `mutate.Canonical` (`:2104`). ranikust reads the clock once per build:
//! the caller passes it in, so this crate holds no global state and reads no
//! environment. Under `--reproducible` the time is `SOURCE_DATE_EPOCH` when
//! set, else the Unix epoch, which equals BuildKit with `SOURCE_DATE_EPOCH=0`
//! (Q-161, `ADR-0032`); layer header mtimes later than it are clamped to it,
//! as BuildKit's `rewrite-timestamp` does.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{Error, Result};

/// When a build happened, as written into image configs, history entries
/// and, under `--reproducible`, layer headers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BuildTime {
    /// Seconds since the Unix epoch; never negative.
    seconds: i64,
    /// Nanoseconds within the second; always 0 when reproducible.
    nanos: u32,
    reproducible: bool,
}

impl BuildTime {
    /// The time of a normal build: `now`, the clock read once when the build
    /// started. A time before the Unix epoch is taken as the epoch.
    pub fn at(now: SystemTime) -> Self {
        let since = now.duration_since(UNIX_EPOCH).unwrap_or_default();
        Self {
            seconds: i64::try_from(since.as_secs()).unwrap_or(i64::MAX),
            nanos: since.subsec_nanos(),
            reproducible: false,
        }
    }

    /// The fixed time of a `--reproducible` build: `source_date_epoch`, the
    /// value of `SOURCE_DATE_EPOCH` if set, else the Unix epoch.
    ///
    /// # Errors
    ///
    /// [`Error::SourceDateEpoch`] when the value is not a non-negative
    /// decimal number of seconds.
    pub fn reproducible(source_date_epoch: Option<&str>) -> Result<Self> {
        let seconds = match source_date_epoch {
            None => 0,
            Some(value) => value
                .parse::<i64>()
                .ok()
                .filter(|s| *s >= 0 && value.bytes().all(|b| b.is_ascii_digit()))
                .ok_or_else(|| Error::SourceDateEpoch {
                    value: value.to_owned(),
                })?,
        };
        Ok(Self {
            seconds,
            nanos: 0,
            reproducible: true,
        })
    }

    /// Whether this is a `--reproducible` build.
    pub fn is_reproducible(&self) -> bool {
        self.reproducible
    }

    /// The latest mtime a layer header may carry: the fixed time under
    /// `--reproducible`, else `None` (each file keeps its own), as
    /// [`rnk_layer::EncodeOptions::mtime`] takes it.
    pub fn layer_mtime(&self) -> Option<i64> {
        self.reproducible.then_some(self.seconds)
    }

    /// RFC 3339 in UTC, with the fractional seconds Go's `RFC3339Nano`
    /// writes (trailing zeros dropped, none when zero), as the `created`
    /// fields of kaniko's configs.
    pub fn rfc3339(&self) -> String {
        let days = self.seconds.div_euclid(86_400);
        let secs = self.seconds.rem_euclid(86_400);
        let (year, month, day) = civil_from_days(days);
        let mut out = format!(
            "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}",
            secs / 3600,
            secs % 3600 / 60,
            secs % 60
        );
        if self.nanos != 0 {
            let fraction = format!("{:09}", self.nanos);
            out.push('.');
            out.push_str(fraction.trim_end_matches('0'));
        }
        out.push('Z');
        out
    }
}

/// The proleptic Gregorian date of a day count since 1970-01-01 (Howard
/// Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    // Both are small by construction: day 1..=31, month 1..=12.
    (
        year,
        u32::try_from(month).unwrap_or(1),
        u32::try_from(day).unwrap_or(1),
    )
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn formats_like_go_rfc3339_nano() {
        let at = |secs, nanos| BuildTime::at(UNIX_EPOCH + Duration::new(secs, nanos));
        assert_eq!(at(0, 0).rfc3339(), "1970-01-01T00:00:00Z");
        assert_eq!(at(951_782_400, 0).rfc3339(), "2000-02-29T00:00:00Z");
        assert_eq!(
            at(1_793_312_675, 120_000_000).rfc3339(),
            "2026-10-29T22:24:35.12Z"
        );
        assert_eq!(
            at(4_102_444_799, 999_999_999).rfc3339(),
            "2099-12-31T23:59:59.999999999Z"
        );
    }

    #[test]
    fn a_normal_build_keeps_file_times() {
        let time = BuildTime::at(UNIX_EPOCH + Duration::from_secs(5));
        assert!(!time.is_reproducible());
        assert_eq!(time.layer_mtime(), None);
    }

    #[test]
    fn reproducible_is_the_epoch_or_source_date_epoch() {
        let epoch = BuildTime::reproducible(None).unwrap();
        assert_eq!(epoch.rfc3339(), "1970-01-01T00:00:00Z");
        assert_eq!(epoch.layer_mtime(), Some(0));

        let sde = BuildTime::reproducible(Some("1700000000")).unwrap();
        assert_eq!(sde.rfc3339(), "2023-11-14T22:13:20Z");
        assert_eq!(sde.layer_mtime(), Some(1_700_000_000));
    }

    #[test]
    fn a_bad_source_date_epoch_is_refused() {
        for bad in ["", "-1", "+5", "1.5", "now", "99999999999999999999"] {
            assert!(
                matches!(
                    BuildTime::reproducible(Some(bad)),
                    Err(Error::SourceDateEpoch { .. })
                ),
                "{bad:?}"
            );
        }
    }
}
