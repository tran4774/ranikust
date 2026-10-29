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
    assert!(!Config::from_vars(None, None).is_enabled());
    assert!(!Config::from_vars(Some("".into()), Some("0".into())).is_enabled());
    assert!(!Config::from_vars(None, Some("".into())).is_enabled());

    let summary = Config::from_vars(None, Some("1".into()));
    assert_eq!((summary.file, summary.summary), (None, true));

    let relative = Config::from_vars(Some("out/m.json".into()), None);
    let file = relative.file.expect("file");
    assert!(file.is_absolute());
    assert!(file.ends_with("out/m.json"));
}

#[test]
fn disabled_path_installs_nothing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (dispatch, collector) = build(&Config::default(), fake_host(tmp.path()));
    assert!(collector.is_none());
    assert!(dispatch.downcast_ref::<MetricsLayer>().is_none());
    // The level hint is the log layer's: tracing rejects `debug_span!` with
    // one comparison against it.
    let logs_only = Registry::default().with(logs());
    assert_eq!(logs_only.max_level_hint(), Some(LevelFilter::INFO));
    assert_eq!(fs::read_dir(tmp.path()).expect("read_dir").count(), 0);
    // Whether a `debug_span!` is then disabled depends on the process-wide
    // callsite cache, which the other tests' subscribers share: checked in
    // `tests/init_disabled.rs`, a process of its own.
}

#[test]
fn log_level_never_hides_a_metric_span() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let config = Config {
        file: Some(tmp.path().join("m.json")),
        summary: false,
    };
    let (dispatch, collector) = build(&config, fake_host(tmp.path()));
    assert!(collector.is_some());
    assert!(dispatch.downcast_ref::<MetricsLayer>().is_some());
    // The metrics layer takes spans at every level, whatever the log level;
    // the process-wide behaviour is checked in `tests/init_enabled.rs`.
    let capture = Capture::new();
    capture.run(|| drop(trace_span!("rnk_test.fine").entered()));
    assert_eq!(capture.report()["phases"]["rnk_test.fine"]["count"], 1);
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
fn unwritable_report_is_only_a_warning() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let blocker = tmp.path().join("file");
    fs::write(&blocker, b"").expect("write");
    let collector = Collector::new(Some(blocker.join("m.json")), false, fake_host(tmp.path()));
    // Returns normally; the warning goes to stderr.
    collector.finish(&Outcome::success());
    assert!(!blocker.join("m.json").exists());
}
