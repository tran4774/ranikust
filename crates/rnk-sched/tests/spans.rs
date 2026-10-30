//! The `rnk_sched.*` spans the metrics report reads (ADR-0025).
//!
//! A test binary of its own, and every test creates its [`Capture`] before
//! it reaches a span: tracing caches whether a span is wanted once per
//! process, and a test reaching it with no subscriber alive could race a
//! `Capture` into caching "never".

use std::fmt;
use std::num::NonZeroUsize;

use rnk_dockerfile::{GraphOptions, parse, stage_graph};
use rnk_metrics::Capture;
use rnk_sched::{
    CancellationToken, MemoryCheck, MemoryGate, PrepareJob, Scheduler, StageJob, StageRunner,
};

#[derive(Debug)]
struct Never;

impl fmt::Display for Never {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("never")
    }
}

impl std::error::Error for Never {}

/// Two `RUN`s per stage, a yield inside each.
struct TwoRuns;

impl StageRunner for TwoRuns {
    type Prepared = ();
    type Output = ();
    type Error = Never;

    async fn prepare(&self, _job: PrepareJob<'_>) -> Result<(), Never> {
        Ok(())
    }

    async fn build(&self, job: StageJob<'_, Self>) -> Result<(), Never> {
        for _ in 0..2 {
            let _permit = job.run_slot().await;
            tokio::task::yield_now().await;
        }
        Ok(())
    }
}

/// One stage at a time, reporting a fixed `memory.current`.
struct OneAtATime;

impl MemoryGate for OneAtATime {
    fn check(&self, running: usize) -> MemoryCheck {
        MemoryCheck {
            admit: running == 0,
            current_bytes: Some(4096),
        }
    }
}

#[test]
fn run_stage_and_wait_spans() {
    let capture = Capture::new();
    let g = stage_graph(
        &parse("FROM alpine:3 AS a\nFROM scratch AS b\nFROM a AS c\nCOPY --from=b / /\n").unwrap(),
        &GraphOptions::default(),
    )
    .unwrap();
    let sched = Scheduler::new(NonZeroUsize::new(3)).with_memory_gate(OneAtATime);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    capture.run(|| {
        rt.block_on(sched.run(&g, &TwoRuns, &CancellationToken::new()))
            .unwrap()
    });
    let report = capture.report();

    let run = &report["phases"]["rnk_sched.run"];
    assert_eq!(run["count"], 1);
    assert_eq!(run["stages"], 3);
    assert_eq!(run["jobs"], 3);
    assert_eq!(run["max_running"], 1);
    assert_eq!(report["phases"]["rnk_sched.wait_deps"]["count"], 3);
    assert_eq!(report["phases"]["rnk_sched.wait_jobs"]["count"], 6);
    // `a` and `b` are ready together; the gate holds one of them back.
    assert!(report["phases"]["rnk_sched.wait_memory"]["count"].as_u64() >= Some(1));

    let stages = report["stages"].as_array().unwrap();
    let mut seen: Vec<_> = stages
        .iter()
        .map(|s| {
            (
                s["stage"].as_i64().unwrap(),
                s["name"].as_str().unwrap().to_owned(),
                s["text"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    seen.sort();
    assert_eq!(
        seen,
        [
            (0, "a".to_owned(), "alpine:3".to_owned()),
            (1, "b".to_owned(), "scratch".to_owned()),
            (2, "c".to_owned(), "a".to_owned()),
        ]
    );
    for s in stages {
        assert_eq!(s["phases"]["rnk_sched.wait_jobs"]["count"], 2, "{s}");
        assert_eq!(s["values"]["max_memory_bytes"], 4096, "{s}");
    }
}
