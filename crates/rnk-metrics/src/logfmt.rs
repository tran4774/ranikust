//! Log lines as kaniko writes them: logrus's text and JSON formatters, as
//! kaniko configures them (`kaniko@44aa7ddc0 pkg/logging/logging.go:48-60`,
//! `vendor/github.com/sirupsen/logrus/text_formatter.go`,
//! `json_formatter.go`). ADR-0036.
//!
//! - `color`: logrus's `ForceColors`: the level's first four letters in its
//!   color, then `[0005]`, the seconds since start, or with
//!   `--log-timestamp` the full time, then the message padded to 44
//!   columns and the fields as colored `key=value`.
//! - `text`: `time=… level=… msg=…` and the fields, sorted, always with the
//!   time, as logrus's plain formatter.
//! - `json`: one object per line with `level`, `msg`, `time` and the fields,
//!   keys sorted as Go's encoding of a map.
//!
//! Differences from logrus: times are UTC (`Z`), not the local zone; field
//! values are their `Debug` or `Display` text, not Go's `%v`.

use std::collections::BTreeMap;
use std::fmt;
use std::time::{Instant, SystemTime};

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::registry::LookupSpan;

use crate::LogFormat;
use crate::time::rfc3339_seconds;

/// The event formatter of the log layer.
pub(crate) struct Logrus {
    pub(crate) format: LogFormat,
    /// `--log-timestamp`: the full time instead of seconds since start, in
    /// the colored format; the text and JSON formats always have it.
    pub(crate) full_timestamp: bool,
    /// logrus's `baseTimestamp`: when logging was set up.
    pub(crate) start: Instant,
}

impl<S, N> FormatEvent<S, N> for Logrus
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        _ctx: &FmtContext<'_, S, N>,
        mut out: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let mut fields = Fields::default();
        event.record(&mut fields);
        let level = *event.metadata().level();
        let line = match self.format {
            LogFormat::Json => json(level, &fields),
            LogFormat::Text => text(level, &fields),
            LogFormat::Color => self.colored(level, &fields),
        };
        writeln!(out, "{line}")
    }
}

impl Logrus {
    fn colored(&self, level: Level, f: &Fields) -> String {
        let color = color(level);
        let when = if self.full_timestamp {
            rfc3339_seconds(SystemTime::now())
        } else {
            format!("{:04}", self.start.elapsed().as_secs())
        };
        let mut line = format!(
            "\x1b[{color}m{}\x1b[0m[{when}] {:<44} ",
            prefix(level),
            f.message
        );
        for (k, v) in &f.fields {
            line.push_str(&format!(" \x1b[{color}m{k}\x1b[0m={}", quoted(v)));
        }
        line.truncate(line.trim_end().len());
        line
    }
}

/// The message and the other fields of an event, in key order.
#[derive(Default)]
struct Fields {
    message: String,
    fields: BTreeMap<String, String>,
}

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            value.clone_into(&mut self.message);
        } else {
            self.fields
                .insert(field.name().to_owned(), value.to_owned());
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        } else {
            self.fields
                .insert(field.name().to_owned(), format!("{value:?}"));
        }
    }
}

/// logrus's `Level.String()`.
fn name(level: Level) -> &'static str {
    match level {
        Level::TRACE => "trace",
        Level::DEBUG => "debug",
        Level::INFO => "info",
        Level::WARN => "warning",
        Level::ERROR => "error",
    }
}

/// logrus's colored prefix: the level's name upper-cased, cut to four.
fn prefix(level: Level) -> &'static str {
    match level {
        Level::TRACE => "TRAC",
        Level::DEBUG => "DEBU",
        Level::INFO => "INFO",
        Level::WARN => "WARN",
        Level::ERROR => "ERRO",
    }
}

/// logrus's colors: gray, blue, yellow, red.
fn color(level: Level) -> u8 {
    match level {
        Level::TRACE | Level::DEBUG => 37,
        Level::INFO => 36,
        Level::WARN => 33,
        Level::ERROR => 31,
    }
}

/// logrus's `needsQuoting`: a value is quoted, Go style, unless every
/// character is a letter, a digit or one of `-._/@^+`.
fn quoted(value: &str) -> String {
    let plain = !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_alphanumeric() || "-._/@^+".contains(c));
    if plain {
        value.to_owned()
    } else {
        // JSON string escaping is Go's `%q` for printable text.
        serde_json::Value::String(value.to_owned()).to_string()
    }
}

fn text(level: Level, f: &Fields) -> String {
    let mut line = format!(
        "time={} level={}",
        quoted(&rfc3339_seconds(SystemTime::now())),
        name(level)
    );
    if !f.message.is_empty() {
        line.push_str(&format!(" msg={}", quoted(&f.message)));
    }
    for (k, v) in &f.fields {
        line.push_str(&format!(" {k}={}", quoted(v)));
    }
    line
}

fn json(level: Level, f: &Fields) -> String {
    let mut object = serde_json::Map::new();
    for (k, v) in &f.fields {
        object.insert(k.clone(), serde_json::Value::String(v.clone()));
    }
    object.insert("level".to_owned(), name(level).into());
    object.insert("msg".to_owned(), f.message.clone().into());
    object.insert("time".to_owned(), rfc3339_seconds(SystemTime::now()).into());
    serde_json::Value::Object(object).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(message: &str, extra: &[(&str, &str)]) -> Fields {
        Fields {
            message: message.to_owned(),
            fields: extra
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        }
    }

    #[test]
    fn text_is_logrus_plain() {
        let line = text(Level::WARN, &fields("a b", &[("stage", "x")]));
        assert!(line.starts_with("time="), "{line}");
        assert!(
            line.ends_with(r#" level=warning msg="a b" stage=x"#),
            "{line}"
        );
    }

    #[test]
    fn json_has_logrus_keys() {
        let line = json(Level::INFO, &fields("built", &[("n", "1")]));
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["level"], "info");
        assert_eq!(v["msg"], "built");
        assert_eq!(v["n"], "1");
        assert!(v["time"].as_str().unwrap().ends_with('Z'));
        assert!(line.starts_with(r#"{"level""#), "keys sorted: {line}");
    }

    #[test]
    fn color_is_forced_with_seconds_since_start() {
        let f = Logrus {
            format: LogFormat::Color,
            full_timestamp: false,
            start: Instant::now(),
        };
        let line = f.colored(Level::ERROR, &fields("failed", &[]));
        assert!(
            line.starts_with("\x1b[31mERRO\x1b[0m[0000] failed"),
            "{line:?}"
        );
        let f = Logrus {
            full_timestamp: true,
            ..f
        };
        let line = f.colored(Level::INFO, &fields("x", &[]));
        assert!(line.starts_with("\x1b[36mINFO\x1b[0m[2"), "{line:?}");
    }

    #[test]
    fn values_are_quoted_as_logrus_does() {
        assert_eq!(quoted("plain-1.2/x@y"), "plain-1.2/x@y");
        assert_eq!(quoted("two words"), r#""two words""#);
        assert_eq!(quoted(""), r#""""#);
    }
}
