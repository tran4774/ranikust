//! The collector behind an enabled [`Telemetry`](crate::Telemetry): the span
//! state, the start sample, and writing the report exactly once.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Map, Value};

use crate::layer::Shared;
use crate::report::{self, Outcome, ReportInput};
use crate::resources::{self, HostPaths};
use crate::state::State;

/// How long the panic hook waits for another thread to release the state
/// lock before writing the report without the span sections.
const PANIC_LOCK_WAIT: Duration = Duration::from_millis(100);

pub(crate) struct Collector {
    file: Option<PathBuf>,
    summary: bool,
    paths: HostPaths,
    started: Instant,
    started_wall: SystemTime,
    start_sample: Value,
    pub(crate) shared: Arc<Shared>,
    meta: Mutex<Map<String, Value>>,
    /// A normal exit wrote the report; nothing writes after it.
    finished: AtomicBool,
    panics: AtomicU64,
}

impl Collector {
    pub(crate) fn new(file: Option<PathBuf>, summary: bool, paths: HostPaths) -> Self {
        let started = Instant::now();
        let started_wall = SystemTime::now();
        let start_sample = resources::sample(&paths);
        let mut meta = Map::new();
        meta.insert("os".to_owned(), Value::from(std::env::consts::OS));
        meta.insert("arch".to_owned(), Value::from(std::env::consts::ARCH));
        meta.insert(
            "num_cpu".to_owned(),
            Value::from(thread::available_parallelism().map_or(0, std::num::NonZero::get)),
        );
        meta.insert("pid".to_owned(), Value::from(std::process::id()));
        meta.insert(
            "file".to_owned(),
            Value::from(
                file.as_deref()
                    .map(|f| f.display().to_string())
                    .unwrap_or_default(),
            ),
        );
        Self {
            file,
            summary,
            paths,
            started,
            started_wall,
            start_sample,
            shared: Arc::new(Shared::default()),
            meta: Mutex::new(meta),
            finished: AtomicBool::new(false),
            panics: AtomicU64::new(0),
        }
    }

    pub(crate) fn set_meta(&self, key: &str, value: Value) {
        self.meta
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key.to_owned(), value);
    }

    /// Writes the report of a normal exit. Only the first call writes.
    pub(crate) fn finish(&self, outcome: &Outcome) {
        if self.finished.swap(true, Ordering::SeqCst) {
            return;
        }
        let state = self.shared.lock();
        self.write(Some(&state), outcome, self.panics.load(Ordering::SeqCst));
    }

    /// Writes the report from the panic hook. In a release build the process
    /// aborts right after; in an unwinding build a later [`finish`] still
    /// writes the final report over this one.
    ///
    /// [`finish`]: Self::finish
    pub(crate) fn finish_panic(&self, message: String) {
        let panics = self.panics.fetch_add(1, Ordering::SeqCst) + 1;
        if self.finished.load(Ordering::SeqCst) {
            return;
        }
        let deadline = Instant::now() + PANIC_LOCK_WAIT;
        let state = loop {
            // The panicking thread may hold the lock itself (a bug in the
            // layer): never block on it here.
            if let Some(state) = self.shared.try_lock() {
                break Some(state);
            }
            if Instant::now() >= deadline {
                break None;
            }
            thread::sleep(Duration::from_millis(1));
        };
        self.write(state.as_deref(), &Outcome::panic(message), panics - 1);
    }

    /// Builds the report and writes the file and the summary. `panics`
    /// counts the panic hooks that ran before this report.
    fn write(&self, state: Option<&State>, outcome: &Outcome, panics: u64) {
        let ended = Instant::now();
        let ended_wall = SystemTime::now();
        let end_sample = resources::sample(&self.paths);
        let resources = resources::report(&self.start_sample, &end_sample, &self.paths);
        let meta = match self.meta.try_lock() {
            Ok(meta) => meta.clone(),
            Err(std::sync::TryLockError::Poisoned(meta)) => meta.into_inner().clone(),
            Err(std::sync::TryLockError::WouldBlock) => Map::new(),
        };
        let input = ReportInput {
            started: self.started,
            ended,
            started_wall: self.started_wall,
            ended_wall,
            meta,
            outcome,
            panics,
            resources,
        };
        let report = report::build(state, &input);
        if let Some(file) = &self.file {
            let written = report::to_bytes(&report)
                .map_err(std::io::Error::from)
                .and_then(|bytes| report::write_file(file, &bytes));
            if let Err(err) = written {
                crate::warn(&format!("writing {}: {err}", file.display()));
            }
        }
        if self.summary {
            eprint!("{}", report::summary(&report, self.file.as_deref()));
        }
    }
}
