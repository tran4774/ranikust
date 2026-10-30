//! Tests of the layer through real `tracing` spans, of the installed
//! subscriber and of the collector's write-once rules.

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::thread;

use serde_json::{Value, json};
use tracing::field::Empty;
use tracing::{Level, Subscriber as _, debug_span, span, trace_span};

use super::*;

fn fake_host(dir: &Path) -> HostPaths {
    HostPaths {
        proc_self_cgroup: dir.join("missing"),
        cgroup_root: dir.join("missing"),
        proc_self_status: dir.join("missing"),
        proc_net_dev: dir.join("missing"),
    }
}

#[test]
fn config_reads_the_environment_values() {
    assert!(!Config::from_vars(None).is_enabled());
    assert!(!Config::from_vars(Some("0".into())).is_enabled());
    assert!(!Config::from_vars(Some("".into())).is_enabled());

    // The summary only: the report path never comes from the environment
    // (ADR-0037).
    let summary = Config::from_vars(Some("1".into()));
    assert_eq!((summary.file, summary.summary), (None, true));
}

#[test]
fn disabled_path_installs_nothing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (dispatch, collector) = build(&Config::default(), fake_host(tmp.path()));
    assert!(collector.is_none());
    assert!(dispatch.downcast_ref::<MetricsLayer>().is_none());
    // `build` returns `logs_only()` when disabled. Its level hint is the log
    // layer's: tracing rejects `debug_span!` with one comparison against it.
    assert_eq!(
        logs_only(LogOptions::default()).max_level_hint(),
        Some(LevelFilter::INFO)
    );
    // Whether a `debug_span!` is then disabled depends on the process-wide
    // callsite cache, which the other tests' subscribers share: checked in
    // `tests/init_disabled.rs`, a process of its own.
}

#[test]
fn the_log_level_is_the_log_layers_hint_in_every_format() {
    for format in [LogFormat::Text, LogFormat::Color, LogFormat::Json] {
        for (level, timestamp) in [(LevelFilter::DEBUG, true), (LevelFilter::ERROR, false)] {
            let options = LogOptions {
                level,
                format,
                timestamp,
            };
            assert_eq!(
                logs_only(options).max_level_hint(),
                Some(level),
                "{options:?}"
            );
        }
    }
}

/// `-v trace` shows ranikust's own events, never a dependency's below
/// `INFO` (T11: request internals); `-v error` hides everyone's warnings.
#[test]
fn dependencies_log_at_info_at_most() {
    use tracing::Level;
    let trace = log_filter(LevelFilter::TRACE);
    assert!(trace.would_enable("rnk_registry::client", &Level::TRACE));
    assert!(trace.would_enable("ranikust::build", &Level::DEBUG));
    assert!(!trace.would_enable("hyper::proto::h1", &Level::DEBUG));
    assert!(trace.would_enable("hyper::proto::h1", &Level::INFO));
    let error = log_filter(LevelFilter::ERROR);
    assert!(!error.would_enable("ranikust::main", &Level::WARN));
    assert!(!error.would_enable("hyper", &Level::WARN));
}

#[test]
fn log_level_never_hides_a_metric_span() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let config = Config {
        file: Some(tmp.path().join("m.json")),
        summary: false,
        ..Config::default()
    };
    let (dispatch, collector) = build(&config, fake_host(tmp.path()));
    let collector = collector.expect("collector");
    assert!(dispatch.downcast_ref::<MetricsLayer>().is_some());
    // Through the full subscriber, log layer at INFO included.
    tracing::dispatcher::with_default(&dispatch, || {
        drop(trace_span!("rnk_test.fine").entered());
        drop(debug_span!("rnk_test.coarse").entered());
    });
    let state = collector.shared.lock();
    assert_eq!(state.phases["rnk_test.fine"].agg.count, 1);
    assert_eq!(state.phases["rnk_test.coarse"].agg.count, 1);
}

#[test]
fn nested_spans_build_records_and_phases() {
    let capture = Capture::new();
    capture.run(|| {
        let stage = debug_span!(
            "rnk_sched.stage",
            stage = 1u64,
            name = "build",
            base = "alpine:3"
        );
        let _stage = stage.enter();
        drop(debug_span!("rnk_sched.wait_deps").entered());
        let instr = debug_span!(
            "rnk_builder.instruction",
            index = 2u64,
            line = 9u64,
            kind = "COPY",
            text = "COPY . /src",
            cache_hit = Empty
        );
        let _instr = instr.enter();
        {
            let diff = debug_span!("rnk_snapshot.diff", files_added = Empty, whiteouts = 0u64);
            let _diff = diff.enter();
            drop(debug_span!("rnk_snapshot.walk", entries = 50u64, max_threads = 4u64).entered());
            drop(debug_span!("rnk_snapshot.walk", entries = 70u64, max_threads = 8u64).entered());
            diff.record("files_added", 11u64);
        }
        instr.record("cache_hit", false);
    });
    let report = capture.report();

    let walk = &report["phases"]["rnk_snapshot.walk"];
    assert_eq!(walk["count"], 2);
    assert_eq!(walk["entries"], 120);
    assert_eq!(walk["max_threads"], 8);
    assert!(walk["max_ns"].as_u64() <= walk["total_ns"].as_u64());

    let stage = &report["stages"][0];
    assert_eq!(
        (stage["stage"].as_i64(), stage["name"].as_str()),
        (Some(1), Some("build"))
    );
    assert_eq!(stage["text"], "alpine:3");
    assert_eq!(stage["phases"]["rnk_sched.wait_deps"]["count"], 1);
    assert_eq!(stage["phases"]["rnk_snapshot.walk"]["entries"], 120);
    assert!(stage.get("incomplete").is_none());

    let command = &report["commands"][0];
    assert_eq!(
        (
            command["stage"].as_i64(),
            command["index"].as_i64(),
            command["line"].as_u64()
        ),
        (Some(1), Some(2), Some(9))
    );
    assert_eq!(command["kind"], "COPY");
    assert!(command.get("cache_hit").is_none());
    assert_eq!(command["phases"]["rnk_snapshot.diff"]["files_added"], 11);

    let snapshot = &report["snapshots"][0];
    assert_eq!(snapshot["kind"], "command");
    assert_eq!(
        snapshot["values"],
        json!({ "files_added": 11, "whiteouts": 0 })
    );
    assert_eq!(snapshot["phases"]["rnk_snapshot.walk"]["count"], 2);
}

#[test]
fn concurrent_spans_from_many_threads_add_up() {
    const THREADS: u64 = 8;
    const SPANS: u64 = 50;
    let capture = Arc::new(Capture::new());
    let stage = capture.run(|| debug_span!("rnk_sched.stage", stage = 0u64));
    let workers: Vec<_> = (0..THREADS)
        .map(|t| {
            let capture = Arc::clone(&capture);
            let stage = stage.clone();
            thread::spawn(move || {
                capture.run(|| {
                    for i in 0..SPANS {
                        let span = debug_span!(parent: &stage, "rnk_layer.encode", bytes_in = Empty, max_threads = t);
                        span.record("bytes_in", i);
                        drop(span);
                    }
                });
            })
        })
        .collect();
    for worker in workers {
        worker.join().expect("worker");
    }
    drop(stage);
    let report = capture.report();

    let encode = &report["phases"]["rnk_layer.encode"];
    assert_eq!(encode["count"], THREADS * SPANS);
    assert_eq!(encode["bytes_in"], THREADS * SPANS * (SPANS - 1) / 2);
    assert_eq!(encode["max_threads"], THREADS - 1);
    let total = encode["total_ns"].as_u64().expect("total");
    let wall = encode["wall_ns"].as_u64().expect("wall");
    assert!(wall <= total, "wall {wall} > total {total}");
    assert!(encode.get("open").is_none());

    let stage = &report["stages"][0];
    assert_eq!(
        stage["phases"]["rnk_layer.encode"]["count"],
        THREADS * SPANS
    );
    assert_eq!(report["phases"]["rnk_sched.stage"]["count"], 1);
}

#[test]
fn parallel_stages_overlap_in_time() {
    let capture = Arc::new(Capture::new());
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let workers: Vec<_> = (0..2u64)
        .map(|n| {
            let capture = Arc::clone(&capture);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                capture.run(|| {
                    let _stage = debug_span!("rnk_sched.stage", stage = n).entered();
                    barrier.wait();
                    barrier.wait();
                });
            })
        })
        .collect();
    for worker in workers {
        worker.join().expect("worker");
    }
    let report = capture.report();
    let stages = report["stages"].as_array().expect("stages");
    assert_eq!(stages.len(), 2);
    let span = |s: &Value| {
        let start = s["start_ns"].as_u64().expect("start");
        (start, start + s["wall_ns"].as_u64().expect("wall"))
    };
    let (a, b) = (span(&stages[0]), span(&stages[1]));
    assert!(
        a.0 < b.1 && b.0 < a.1,
        "stages {a:?} and {b:?} do not overlap"
    );
    // Both were open at once: the phase's wall time is below its total.
    let phase = &report["phases"]["rnk_sched.stage"];
    assert!(phase["wall_ns"].as_u64() < phase["total_ns"].as_u64());
}

#[test]
fn open_spans_are_incomplete_records() {
    let capture = Capture::new();
    let stage = capture.run(|| debug_span!("rnk_sched.stage", stage = 3u64));
    let report = capture.report();
    assert_eq!(report["stages"][0]["incomplete"], true);
    assert_eq!(report["phases"]["rnk_sched.stage"]["open"], 1);
    assert_eq!(report["phases"]["rnk_sched.stage"]["count"], 0);
    drop(stage);
}

#[test]
fn registry_bytes_add_up_per_host() {
    let capture = Capture::new();
    capture.run(|| {
        for bytes in [100u64, 250] {
            let blob = debug_span!(
                "rnk_registry.pull_blob",
                host = "registry:5000",
                bytes_in = bytes,
                requests = 1u64
            );
            let _blob = blob.enter();
            // A nested span naming the same host is not counted twice.
            drop(
                debug_span!(
                    "rnk_registry.request",
                    host = "registry:5000",
                    bytes_in = bytes
                )
                .entered(),
            );
        }
        drop(
            debug_span!("rnk_registry.push_blob", host = %"mirror.example", bytes_out = 7u64)
                .entered(),
        );
    });
    let hosts = &capture.report()["network"]["hosts"];
    assert_eq!(hosts["registry:5000"]["bytes_in"], 350);
    assert_eq!(hosts["registry:5000"]["requests"], 2);
    assert_eq!(hosts["registry:5000"]["count"], 2);
    assert_eq!(hosts["mirror.example"]["bytes_out"], 7);
}

#[test]
fn other_targets_and_events_are_ignored() {
    let capture = Capture::new();
    capture.run(|| {
        drop(span!(target: "hyper::client", Level::DEBUG, "rnk_fake.op").entered());
        tracing::info!(bytes = 5u64, "an event");
    });
    assert_eq!(capture.report()["phases"], json!({}));
}

fn read_report(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).expect("report")).expect("json")
}

#[test]
fn finish_writes_once_and_panic_reports_are_provisional() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("m.json");
    let collector = Collector::new(Some(path.clone()), false, fake_host(tmp.path()));
    collector.set_meta("ranikust_version", Value::from("0.0.0"));

    // An unwinding build may panic on a thread and carry on: the hook writes
    // a report, which the normal exit later replaces.
    collector.finish_panic("panic at x.rs:1: boom".to_owned());
    let report = read_report(&path);
    assert_eq!(report["exit"]["status"], "panic");
    assert_eq!(report["exit"]["message"], "panic at x.rs:1: boom");

    collector.finish(&Outcome::error(3, "failed"));
    let report = read_report(&path);
    assert_eq!(
        report["exit"],
        json!({ "status": "error", "code": 3, "message": "failed", "panics": 1 })
    );
    assert_eq!(report["meta"]["ranikust_version"], "0.0.0");
    assert_eq!(report["meta"]["file"], path.display().to_string());
    assert_eq!(report["schema"], SCHEMA);

    // Nothing writes after a normal exit.
    fs::remove_file(&path).expect("remove");
    collector.finish(&Outcome::success());
    collector.finish_panic("late".to_owned());
    assert!(!path.exists());
}

#[test]
fn panic_hook_does_not_wait_forever_for_the_lock() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("m.json");
    let collector = Collector::new(Some(path.clone()), false, fake_host(tmp.path()));
    let held = collector.shared.lock();
    collector.finish_panic("panic while the state was locked".to_owned());
    drop(held);
    let report = read_report(&path);
    assert_eq!(report["exit"]["spans_unavailable"], true);
}

#[test]
fn a_panic_report_never_races_another_write() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("m.json");
    let collector = Collector::new(Some(path.clone()), false, fake_host(tmp.path()));
    // Another report is being written: this panic's report is dropped.
    let writing = collector.writing.lock().expect("lock");
    collector.finish_panic("second panic".to_owned());
    drop(writing);
    assert!(!path.exists());
    collector.finish(&Outcome::success());
    assert_eq!(read_report(&path)["exit"]["panics"], 1);
}

#[test]
fn host_recorded_late_is_still_counted_once() {
    let capture = Capture::new();
    capture.run(|| {
        let blob = debug_span!("rnk_registry.pull_blob", host = Empty, bytes_in = 10u64);
        let _blob = blob.enter();
        // The child opens before the parent knows its host.
        let request = debug_span!("rnk_registry.request", host = "r:5000", bytes_in = 10u64);
        blob.record("host", "r:5000");
        drop(request);
    });
    let host = &capture.report()["network"]["hosts"]["r:5000"];
    assert_eq!(host["count"], 1);
    assert_eq!(host["bytes_in"], 10);
}

#[test]
fn a_record_lasts_until_its_last_child_closes() {
    // tracing keeps a span open while a child is: background work parented
    // to a record stretches it. design/metrics.md therefore parents such
    // work to `rnk_sched.run`.
    let capture = Capture::new();
    let child = capture.run(|| {
        let _stage = debug_span!("rnk_sched.stage", stage = 0u64).entered();
        debug_span!("rnk_cache.push")
    });
    assert_eq!(capture.report()["stages"][0]["incomplete"], true);
    // The registry closes the parent through the current subscriber.
    capture.run(|| drop(child));
    assert!(capture.report()["stages"][0].get("incomplete").is_none());
}

#[test]
fn unwritable_report_is_only_a_warning() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let blocker = tmp.path().join("file");
    fs::write(&blocker, b"").expect("write");
    let collector = Collector::new(Some(blocker.join("m.json")), false, fake_host(tmp.path()));
    // Returns normally; the warning goes to stderr.
    collector.finish(&Outcome::success());
    assert!(!blocker.join("m.json").exists());
}
