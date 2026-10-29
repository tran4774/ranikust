//! The walk's metrics spans.
//!
//! Alone in its test binary: `tracing` caches whether a callsite is enabled,
//! so a test without a subscriber hitting the same span concurrently could
//! disable it for the capture.

use std::fs;
use std::path::PathBuf;

use rnk_context::{ContextSource as _, DirSource, IgnoreFile, IgnoreSource};

#[test]
fn walk_records_its_totals_on_the_load_span() {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir(tmp.path().join("d")).unwrap();
    fs::write(tmp.path().join("d/x"), b"abc").unwrap();
    fs::write(tmp.path().join("y.log"), b"zz").unwrap();
    let ctx = DirSource::new(tmp.path()).open().unwrap();
    let capture = rnk_metrics::Capture::new();
    capture.run(|| {
        let ignore = IgnoreFile {
            source: IgnoreSource::ContextRoot,
            contents: b"*.log\n".to_vec(),
        };
        let m = ctx.matcher(&ignore).unwrap();
        let paths: Vec<PathBuf> = ctx.walk(Some(&m)).map(|e| e.unwrap().path).collect();
        assert_eq!(paths, [PathBuf::from("d"), PathBuf::from("d/x")]);
    });
    let report = capture.report();
    let load = &report["phases"]["rnk_context.load"];
    assert_eq!(load["count"], 1, "{report}");
    assert_eq!(load["files"], 1);
    assert_eq!(load["dirs"], 1);
    assert_eq!(load["bytes"], 3);
    assert_eq!(load["files_ignored"], 1);
    assert_eq!(report["phases"]["rnk_context.ignore"]["patterns"], 1);
}
