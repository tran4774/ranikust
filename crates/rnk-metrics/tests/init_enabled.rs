//! `init` with metrics on, in a process of its own: spans reach the report
//! whatever the log level, dependencies' spans do not, and the panic hook
//! writes a report. One test only, since `init` and the panic hook are
//! process-wide.

use std::fs;

use serde_json::Value;
use tracing::level_filters::LevelFilter;
use tracing::{Level, span};

fn read(path: &std::path::Path) -> Value {
    serde_json::from_slice(&fs::read(path).expect("report")).expect("json")
}

#[test]
fn metrics_on_collects_spans_and_reports_panics() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("metrics.json");
    let telemetry = rnk_metrics::init(rnk_metrics::Config {
        file: Some(path.clone()),
        summary: false,
        ..rnk_metrics::Config::default()
    });
    assert!(telemetry.is_enabled());
    telemetry.set_meta("ranikust_version", "test");
    assert_eq!(LevelFilter::current(), LevelFilter::TRACE);

    let run = span!(target: "ranikust", Level::DEBUG, "ranikust.run");
    run.in_scope(|| {
        let fine =
            span!(target: "rnk_layer::encode", Level::TRACE, "rnk_layer.encode", bytes_out = 3u64);
        assert!(!fine.is_disabled());
        drop(fine);
        assert!(span!(target: "h2::codec", Level::TRACE, "frame").is_disabled());
    });

    // The hook writes a report before unwinding (or aborting) goes on.
    let caught = std::panic::catch_unwind(|| panic!("deliberate"));
    assert!(caught.is_err());
    let provisional = read(&path);
    assert_eq!(provisional["exit"]["status"], "panic");
    assert!(
        provisional["exit"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("deliberate"))
    );
    assert_eq!(provisional["phases"]["ranikust.run"]["open"], 1);

    drop(run);
    telemetry.finish(&rnk_metrics::Outcome::success());
    let report = read(&path);
    assert_eq!(report["schema"], rnk_metrics::SCHEMA);
    assert_eq!(report["exit"]["status"], "success");
    assert_eq!(report["exit"]["panics"], 1);
    assert_eq!(report["meta"]["ranikust_version"], "test");
    assert_eq!(report["phases"]["ranikust.run"]["count"], 1);
    assert_eq!(report["phases"]["rnk_layer.encode"]["bytes_out"], 3);
    assert!(report["phases"].get("frame").is_none());
}
