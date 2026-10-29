//! The `rnk_sandbox.*` spans the metrics report reads (ADR-0025).
//!
//! A test binary of its own, and the [`Capture`] is created before any span
//! is reached: tracing caches whether a span is wanted once per process.
//!
//! The `RUN` is `Isolation::Direct` with a missing `WORKDIR`: a real child
//! through the spawner thread that fails before executing anything, with no
//! namespace and no mount, whatever the privileges of the test.

use rnk_metrics::Capture;
use rnk_sandbox::{Error, Isolation, RunSpec, Sandbox, Step};

#[test]
fn a_failed_run_records_setup_and_run() {
    let capture = Capture::new();
    let dir = tempfile::tempdir().unwrap();
    let sandbox = Sandbox::start(None, dir.path()).unwrap();
    let mut spec = RunSpec::new("/", vec!["/nonexistent/ranikust-test".into()]);
    spec.isolation = Isolation::Direct;
    spec.workdir = "/nonexistent/ranikust-test".into();

    let r = capture.run(|| sandbox.run(spec));
    assert!(
        matches!(
            r,
            Err(Error::Setup {
                step: Step::Workdir,
                ..
            })
        ),
        "{r:?}"
    );
    let report = capture.report();
    assert_eq!(
        report["phases"]["rnk_sandbox.setup"]["count"], 1,
        "{report}"
    );
    let run = &report["phases"]["rnk_sandbox.run"];
    assert_eq!(run["count"], 1, "{report}");
    assert_eq!(run["failed"], 1, "{report}");
}
