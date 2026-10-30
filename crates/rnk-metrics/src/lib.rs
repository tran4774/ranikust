//! Opt-in build metrics, and the process's `tracing` subscriber.
//!
//! ranikust's libraries describe their work as `tracing` spans named
//! `<crate>.<operation>` (`rnk_layer.encode`), with numeric fields that carry
//! their unit (`bytes_in`, `files`, `user_ns`), recorded once per span. This
//! crate installs the subscriber that turns them into logs and, when
//! `RANIKUST_METRICS_FILE` is set, into one JSON report written when the
//! process ends: totals per span name, one record per stage, instruction and
//! snapshot, bytes per registry host, `getrusage` and cgroup v2 counters. The
//! report follows kaniko's `KANIKO_METRICS_FILE` report where the meaning
//! matches. Which span each crate emits is in `docs/design/metrics.md`; the
//! decision is ADR-0025.
//!
//! ```no_run
//! let telemetry = rnk_metrics::init(rnk_metrics::Config::from_env());
//! let result: Result<(), String> = tracing::debug_span!("ranikust.run").in_scope(|| Ok(()));
//! telemetry.finish(&match &result {
//!     Ok(()) => rnk_metrics::Outcome::success(),
//!     Err(e) => rnk_metrics::Outcome::error(1, e),
//! });
//! ```
//!
//! # Boundaries
//!
//! - Used by the `ranikust` binary only. Libraries emit spans and never
//!   depend on this crate, except as a dev-dependency to check their spans
//!   with [`Capture`].
//! - Never influences a build: nothing here reaches a layer, a config, a
//!   cache key or a digest. The only file it writes is the report, at exit.
//! - Off means off: without `RANIKUST_METRICS_FILE` or
//!   `RANIKUST_METRICS_SUMMARY`, neither the metrics layer nor the panic hook
//!   is installed, and spans cost tracing's disabled-callsite check.
//! - Aggregates only: no state per file or tar entry, no timer finer than a
//!   span, no thread of its own (the process must stay single-threaded until
//!   its namespaces are set up).
//! - Logs go to stderr at the level and in the format of
//!   [`Config::logs`]: kaniko's `--verbosity`, `--log-format` and
//!   `--log-timestamp`, which `ranikust` parses (ADR-0036).

mod collector;
mod layer;
mod logfmt;
mod report;
mod resources;
mod state;
mod time;

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use serde_json::Value;
use tracing::Dispatch;
use tracing_subscriber::filter::{LevelFilter, Targets, filter_fn};
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::{Layer as _, Registry};

use crate::collector::Collector;
use crate::layer::{MetricsLayer, Shared, is_metric_span};
pub use crate::report::{Outcome, SCHEMA};
use crate::resources::HostPaths;

/// Path of the JSON report; unset or empty, metrics are off.
pub const FILE_ENV: &str = "RANIKUST_METRICS_FILE";

/// Set to anything but empty or `0`, prints a short summary to stderr at
/// exit (and turns metrics on even without [`FILE_ENV`]).
pub const SUMMARY_ENV: &str = "RANIKUST_METRICS_SUMMARY";

/// What to collect and where to write it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Config {
    /// Where to write the JSON report; `None` writes no file.
    pub file: Option<PathBuf>,
    /// Print a short summary to stderr at exit.
    pub summary: bool,
    /// How logs are written.
    pub logs: LogOptions,
}

/// The log layer's settings: kaniko's `--verbosity`, `--log-format` and
/// `--log-timestamp` (ADR-0036). They never change what the metrics layer
/// sees.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogOptions {
    /// The most detailed level written; kaniko's default is `info`.
    pub level: LevelFilter,
    /// The format.
    pub format: LogFormat,
    /// kaniko's `--log-timestamp`: in the colored format, the full time
    /// instead of the seconds since start (`INFO[0005]`); the text and JSON
    /// formats always carry the time, as logrus's.
    pub timestamp: bool,
}

impl Default for LogOptions {
    fn default() -> Self {
        Self {
            level: LevelFilter::INFO,
            format: LogFormat::Color,
            timestamp: false,
        }
    }
}

/// kaniko's `--log-format`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LogFormat {
    /// logrus's plain text: `time=… level=… msg=…`, never colored.
    Text,
    /// logrus's forced colors, kaniko's default: `INFO[0005] message`.
    #[default]
    Color,
    /// logrus's JSON: one object per line, with `level`, `msg` and `time`.
    Json,
}

impl Config {
    /// Reads [`FILE_ENV`] and [`SUMMARY_ENV`]. A relative report path is made
    /// absolute now, against the current directory at start-up.
    pub fn from_env() -> Self {
        Self::from_vars(std::env::var_os(FILE_ENV), std::env::var_os(SUMMARY_ENV))
    }

    fn from_vars(file: Option<OsString>, summary: Option<OsString>) -> Self {
        let file = file.filter(|f| !f.is_empty()).map(|f| {
            let path = PathBuf::from(f);
            // `absolute` only fails without a current directory; the
            // relative path is then the best there is.
            std::path::absolute(&path).unwrap_or(path)
        });
        let summary = summary.is_some_and(|s| !s.is_empty() && s != "0");
        Self {
            file,
            summary,
            logs: LogOptions::default(),
        }
    }

    /// Whether anything is collected.
    pub fn is_enabled(&self) -> bool {
        self.file.is_some() || self.summary
    }
}

/// The installed subscriber's handle: write the report through it at exit.
///
/// Dropping it writes nothing; call [`Telemetry::finish`] on every way out
/// that returns. The panic hook covers panics.
#[must_use = "call `finish` at exit, or the report is never written"]
pub struct Telemetry {
    collector: Option<Arc<Collector>>,
}

impl Telemetry {
    /// Whether metrics are collected.
    pub fn is_enabled(&self) -> bool {
        self.collector.is_some()
    }

    /// Adds an entry to the report's `meta` section (the version, the mode,
    /// the options that change what a build costs). Never pass build
    /// arguments, secrets or credentials.
    pub fn set_meta(&self, key: &str, value: impl Into<Value>) {
        if let Some(collector) = &self.collector {
            collector.set_meta(key, value.into());
        }
    }

    /// Writes the report and, if asked, the summary. Only the first call
    /// writes. A failure is a warning on stderr: the exit code never depends
    /// on the report.
    pub fn finish(&self, outcome: &Outcome) {
        if let Some(collector) = &self.collector {
            collector.finish(outcome);
        }
    }
}

/// Installs the process's subscriber: logs to stderr at `INFO`, plus the
/// metrics layer and a panic hook that writes the report when `config`
/// enables metrics. Call it once, first thing in `main`, before any thread
/// starts. It starts no thread itself.
pub fn init(config: Config) -> Telemetry {
    let (dispatch, collector) = build(&config, HostPaths::default());
    if let Err(err) = tracing::dispatcher::set_global_default(dispatch) {
        warn(&format!("not installed: {err}"));
        return Telemetry { collector: None };
    }
    if let Some(collector) = &collector {
        install_panic_hook(Arc::clone(collector));
    }
    Telemetry { collector }
}

/// ranikust's own crates: their events log at `options.level`; anything
/// else (dependencies such as `hyper`, `rustls`) at `INFO` at most, so that
/// `--verbosity=trace` cannot print a dependency's internals, request
/// headers included (threat model T11).
const OWN_TARGETS: &[&str] = &[
    "ranikust",
    "rnk_builder",
    "rnk_cache",
    "rnk_context",
    "rnk_dockerfile",
    "rnk_ignore",
    "rnk_layer",
    "rnk_metrics",
    "rnk_oci",
    "rnk_registry",
    "rnk_rootfs",
    "rnk_sandbox",
    "rnk_sched",
    "rnk_snapshot",
    "rnk_store",
];

/// `level` for [`OWN_TARGETS`], at most `INFO` for the rest.
fn log_filter(level: LevelFilter) -> Targets {
    OWN_TARGETS.iter().fold(
        Targets::new().with_default(level.min(LevelFilter::INFO)),
        |t, own| t.with_target(*own, level),
    )
}

/// The log layer: stderr, at `options.level`, written as kaniko's logrus
/// writes it (`logfmt`; ADR-0036; per-stage grouping stays apart from the
/// format, Q-031). Its per-layer filter keeps its level from hiding spans
/// from the metrics layer.
fn logs<S>(options: LogOptions) -> Box<dyn tracing_subscriber::Layer<S> + Send + Sync>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    let filter = log_filter(options.level);
    tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .event_format(logfmt::Logrus {
            format: options.format,
            full_timestamp: options.timestamp,
            start: Instant::now(),
        })
        .with_filter(filter)
        .boxed()
}

/// The subscriber with metrics off: the log layer alone, whose level hint
/// lets tracing reject `debug_span!` with one comparison at `INFO`.
fn logs_only(options: LogOptions) -> impl tracing::Subscriber + Send + Sync {
    Registry::default().with(logs(options))
}

/// Builds the subscriber without installing it.
fn build(config: &Config, paths: HostPaths) -> (Dispatch, Option<Arc<Collector>>) {
    if !config.is_enabled() {
        return (Dispatch::new(logs_only(config.logs)), None);
    }
    let collector = Arc::new(Collector::new(config.file.clone(), config.summary, paths));
    let metrics =
        MetricsLayer::new(Arc::clone(&collector.shared)).with_filter(filter_fn(is_metric_span));
    let subscriber = Registry::default().with(logs(config.logs)).with(metrics);
    (Dispatch::new(subscriber), Some(collector))
}

/// Writes the report from the panic hook, after the previous hook printed
/// the panic. With `panic = "abort"` (release builds) the hook is the last
/// code that runs.
fn install_panic_hook(collector: Arc<Collector>) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        previous(info);
        let message = info.payload_as_str().unwrap_or("non-string payload");
        let message = match info.location() {
            Some(at) => format!("panic at {}:{}: {message}", at.file(), at.line()),
            None => format!("panic: {message}"),
        };
        collector.finish_panic(message);
    }));
}

/// A warning about metrics themselves: straight to stderr, since it may come
/// from the panic hook or after the subscriber is gone.
pub(crate) fn warn(message: &str) {
    to_stderr(&format!("ranikust: warning: metrics: {message}\n"));
}

/// Writes to stderr without panicking: `eprint!` panics when stderr is a
/// closed pipe or a full disk, and in a release build that panic would abort
/// and change the exit code.
pub(crate) fn to_stderr(text: &str) {
    use std::io::Write as _;
    // Nowhere left to report a failing stderr: the text is dropped.
    let _ = std::io::stderr().write_all(text.as_bytes());
}

/// Collects spans in a test, with the same aggregation as the report.
///
/// For library tests that check their spans: run the code under
/// [`Capture::run`] (or install [`Capture::dispatch`] on each thread) and
/// read the span sections from [`Capture::report`].
pub struct Capture {
    dispatch: Dispatch,
    shared: Arc<Shared>,
    started: Instant,
}

impl Capture {
    /// A collector that sees ranikust's spans (targets `rnk_*`, `ranikust`)
    /// at every level, and nothing else.
    pub fn new() -> Self {
        let shared = Arc::new(Shared::default());
        let layer = MetricsLayer::new(Arc::clone(&shared)).with_filter(filter_fn(is_metric_span));
        Self {
            dispatch: Dispatch::new(Registry::default().with(layer)),
            shared,
            started: Instant::now(),
        }
    }

    /// The subscriber, to install with `tracing::dispatcher::with_default`
    /// on threads the code under test starts.
    pub fn dispatch(&self) -> &Dispatch {
        &self.dispatch
    }

    /// Runs `f` with this collector as the current thread's subscriber.
    pub fn run<R>(&self, f: impl FnOnce() -> R) -> R {
        tracing::dispatcher::with_default(&self.dispatch, f)
    }

    /// The report's span sections so far: `phases`, `stages`, `commands`,
    /// `snapshots` and `network`, with sorted keys.
    pub fn report(&self) -> Value {
        let state = self.shared.lock();
        report::canonical(Value::Object(report::spans(
            &state,
            self.started,
            Instant::now(),
        )))
    }
}

impl Default for Capture {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;
