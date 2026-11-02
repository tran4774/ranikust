//! `--limit <name>=<value>`: bounds on what a registry or an image can make
//! ranikust hold in memory or wait for (ADR-0079, threat model T15). One
//! table, [`LIMITS`]: a new limit is one more row.

use std::time::Duration;

use rnk_registry::ClientOptions;
use serde_json::{Map, Value, json};

/// What a limit counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Unit {
    /// Bytes: a number, or with `KiB`, `MiB` or `GiB`.
    Size,
    /// Time: a number with `s`, `m` or `h`; kept in milliseconds.
    Duration,
}

/// One limit: its `--limit` name, what it counts, its default, what it
/// bounds and where it goes.
struct Row {
    name: &'static str,
    unit: Unit,
    /// In bytes, or milliseconds.
    default: u64,
    about: &'static str,
    set: fn(&mut Limits, u64),
}

/// The largest size any limit takes: every one bounds what is held in
/// memory, and the extraction reserves nothing near it up front.
const MAX_SIZE: u64 = 1 << 30;
/// The longest `registry-idle`, in milliseconds: a day.
const MAX_DURATION: u64 = 24 * 3_600_000;

/// Every limit, in the order `--help` and errors list them.
const LIMITS: [Row; 5] = [
    Row {
        name: "tar-header",
        unit: Unit::Size,
        default: rnk_rootfs::DEFAULT_EXTENSION_HEADER_LIMIT,
        about: "each GNU long name, GNU long link or PAX record set of a base layer",
        set: |l, v| l.tar_header = v,
    },
    Row {
        name: "manifest",
        unit: Unit::Size,
        default: ClientOptions::MAX_MANIFEST_BYTES,
        about: "a manifest or index, refused while it arrives",
        set: |l, v| l.manifest = v,
    },
    Row {
        name: "registry-body",
        unit: Unit::Size,
        default: ClientOptions::MAX_BODY_BYTES,
        about: "a token response; error bodies are read no further",
        set: |l, v| l.registry_body = v,
    },
    Row {
        name: "config",
        unit: Unit::Size,
        default: ClientOptions::MAX_CONFIG_BYTES,
        about: "an image config, checked before it is pulled",
        set: |l, v| l.config = v,
    },
    Row {
        name: "registry-idle",
        unit: Unit::Duration,
        // `ClientOptions::READ_TIMEOUT`, checked by a test.
        default: 120_000,
        about: "how long a registry may send nothing before the request fails",
        set: |l, v| l.registry_idle = Duration::from_millis(v),
    },
];

/// One `--limit` value, parsed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LimitArg {
    name: &'static str,
    /// In bytes, or milliseconds.
    value: u64,
}

/// The limits in force.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Limits {
    pub(crate) tar_header: u64,
    pub(crate) manifest: u64,
    pub(crate) registry_body: u64,
    pub(crate) config: u64,
    pub(crate) registry_idle: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        let mut limits = Self {
            tar_header: 0,
            manifest: 0,
            registry_body: 0,
            config: 0,
            registry_idle: Duration::ZERO,
        };
        for row in &LIMITS {
            (row.set)(&mut limits, row.default);
        }
        limits
    }
}

impl Limits {
    /// The defaults with each `--limit` applied in order: the last of one
    /// name wins.
    pub(crate) fn from_args(args: &[LimitArg]) -> Self {
        let mut limits = Self::default();
        for arg in args {
            if let Some(row) = LIMITS.iter().find(|r| r.name == arg.name) {
                (row.set)(&mut limits, arg.value);
            }
        }
        limits
    }

    /// Hands the registry limits to the registry client.
    pub(crate) fn apply(&self, options: &mut ClientOptions) {
        options.max_manifest_bytes = self.manifest;
        options.max_body_bytes = self.registry_body;
        options.max_config_bytes = self.config;
        options.read_timeout = Some(self.registry_idle);
    }

    /// Sets the process-wide extraction limit; once, before any build.
    pub(crate) fn set_process_wide(&self) {
        rnk_rootfs::set_extension_header_limit(self.tar_header);
    }

    /// The limits for the metrics report: sizes in bytes, durations in
    /// milliseconds.
    pub(crate) fn meta(&self) -> Value {
        let mut out = Map::new();
        for row in &LIMITS {
            out.insert(row.name.to_owned(), json!(self.value(row.name)));
        }
        Value::Object(out)
    }

    /// The limits as `--limit` would spell them, for a log line.
    pub(crate) fn text(&self) -> String {
        LIMITS
            .iter()
            .map(|row| format!("{}={}", row.name, show(row.unit, self.value(row.name))))
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn value(&self, name: &str) -> u64 {
        match name {
            "tar-header" => self.tar_header,
            "manifest" => self.manifest,
            "registry-body" => self.registry_body,
            "config" => self.config,
            _ => u64::try_from(self.registry_idle.as_millis()).unwrap_or(u64::MAX),
        }
    }
}

/// Each limit with its default and what it bounds, for an unknown name.
fn help() -> String {
    LIMITS
        .iter()
        .map(|row| {
            format!(
                "{}={} ({})",
                row.name,
                show(row.unit, row.default),
                row.about
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn show(unit: Unit, value: u64) -> String {
    match unit {
        Unit::Size => {
            for (size, name) in [(1 << 30, "GiB"), (1 << 20, "MiB"), (1 << 10, "KiB")] {
                if value >= size && value.is_multiple_of(size) {
                    return format!("{}{name}", value / size);
                }
            }
            value.to_string()
        }
        Unit::Duration => {
            for (ms, name) in [(3_600_000, "h"), (60_000, "m"), (1000, "s")] {
                if value >= ms && value.is_multiple_of(ms) {
                    return format!("{}{name}", value / ms);
                }
            }
            format!("{value}ms")
        }
    }
}

fn names() -> String {
    LIMITS.iter().map(|r| r.name).collect::<Vec<_>>().join(", ")
}

/// A `--limit` value: `NAME=VALUE`, a known name, a value above 0.
pub(crate) fn parse_limit(arg: &str) -> Result<LimitArg, String> {
    let Some((name, value)) = arg.split_once('=') else {
        return Err(format!("{arg:?} is not NAME=VALUE (limits: {})", names()));
    };
    let Some(row) = LIMITS.iter().find(|r| r.name == name) else {
        return Err(format!("unknown limit {name:?} (limits: {})", help()));
    };
    let parsed = match row.unit {
        Unit::Size => parse_size(value),
        Unit::Duration => parse_duration(value),
    };
    let max = match row.unit {
        Unit::Size => MAX_SIZE,
        Unit::Duration => MAX_DURATION,
    };
    match parsed {
        Some(v) if v > max => Err(format!(
            "{name}={value:?}: at most {}; a bound past that would let one \
             input take the machine",
            show(row.unit, max)
        )),
        Some(v) if v > 0 => Ok(LimitArg {
            name: row.name,
            value: v,
        }),
        _ => Err(match row.unit {
            Unit::Size => format!(
                "{name}={value:?}: a size above 0 is a number of bytes, or a number with \
                 KiB, MiB or GiB"
            ),
            Unit::Duration => {
                format!("{name}={value:?}: a duration above 0 is a number with s, m or h")
            }
        }),
    }
}

/// A number, then a unit from `units`; `None` when either is wrong or the
/// product overflows.
fn number_with(value: &str, units: &[(&str, u64)]) -> Option<u64> {
    let digits = value.bytes().take_while(u8::is_ascii_digit).count();
    let (number, unit) = value.split_at(digits);
    let number: u64 = number.parse().ok()?;
    let (_, factor) = units.iter().find(|(u, _)| *u == unit)?;
    number.checked_mul(*factor)
}

fn parse_size(value: &str) -> Option<u64> {
    number_with(
        value,
        &[
            ("", 1),
            ("KiB", 1 << 10),
            ("MiB", 1 << 20),
            ("GiB", 1 << 30),
        ],
    )
}

fn parse_duration(value: &str) -> Option<u64> {
    number_with(value, &[("s", 1000), ("m", 60_000), ("h", 3_600_000)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_the_libraries_own() {
        let limits = Limits::default();
        assert_eq!(limits.tar_header, 1 << 20);
        assert_eq!(limits.manifest, 4 << 20);
        assert_eq!(limits.registry_body, 1 << 20);
        assert_eq!(limits.config, 8 << 20);
        assert_eq!(limits.registry_idle, ClientOptions::READ_TIMEOUT);
        assert_eq!(
            limits.text(),
            "tar-header=1MiB manifest=4MiB registry-body=1MiB config=8MiB registry-idle=2m"
        );
    }

    #[test]
    fn values_parse_with_their_units_and_the_last_one_wins() {
        let args: Vec<LimitArg> = [
            "manifest=8MiB",
            "tar-header=2097152",
            "registry-body=512KiB",
            "config=1GiB",
            "registry-idle=30s",
            "registry-idle=5m",
        ]
        .iter()
        .map(|a| parse_limit(a).unwrap())
        .collect();
        let limits = Limits::from_args(&args);
        assert_eq!(limits.manifest, 8 << 20);
        assert_eq!(limits.tar_header, 2 << 20);
        assert_eq!(limits.registry_body, 512 << 10);
        assert_eq!(limits.config, 1 << 30);
        assert_eq!(limits.registry_idle, Duration::from_secs(300));

        let mut options = ClientOptions::default();
        limits.apply(&mut options);
        assert_eq!(options.max_manifest_bytes, 8 << 20);
        assert_eq!(options.max_body_bytes, 512 << 10);
        assert_eq!(options.max_config_bytes, 1 << 30);
        assert_eq!(options.read_timeout, Some(Duration::from_secs(300)));
        assert_eq!(limits.meta()["registry-idle"], 300_000);
    }

    #[test]
    fn bad_names_and_values_are_refused_with_what_is_accepted() {
        let unknown = parse_limit("layer=1GiB").unwrap_err();
        assert!(unknown.contains("unknown limit \"layer\""), "{unknown}");
        assert!(
            unknown.contains("tar-header=1MiB (") && unknown.contains("registry-idle=2m ("),
            "{unknown}"
        );
        assert!(parse_limit("manifest").unwrap_err().contains("NAME=VALUE"));
        for bad in [
            "manifest=0",
            "manifest=",
            "manifest=4MB",
            "manifest=4 MiB",
            "manifest=-1",
            "manifest=unlimited",
            "manifest=99999999999GiB",
            "registry-idle=120",
            "registry-idle=0s",
            "registry-idle=1d",
            "tar-header=1.5MiB",
        ] {
            let err = parse_limit(bad).unwrap_err();
            assert!(err.contains("above 0"), "{bad}: {err}");
        }
        // Each limit has a ceiling: 1 GiB for sizes, a day for durations.
        for over in [
            "tar-header=1025MiB",
            "manifest=2GiB",
            "registry-body=1073741825",
            "config=8GiB",
            "registry-idle=25h",
        ] {
            let err = parse_limit(over).unwrap_err();
            assert!(err.contains("at most"), "{over}: {err}");
        }
        assert!(parse_limit("tar-header=1GiB").is_ok());
        assert!(parse_limit("registry-idle=24h").is_ok());
    }
}
