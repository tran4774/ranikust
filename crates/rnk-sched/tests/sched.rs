//! The scheduler with fake runners: order, limits, failure, determinism.

use std::collections::BTreeMap;
use std::fmt;
use std::num::NonZeroUsize;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use proptest::prelude::*;
use rnk_dockerfile::{GraphOptions, StageGraph, parse, stage_graph};
use rnk_sched::{
    CancellationToken, Error, MemoryCheck, MemoryGate, PrepareJob, Scheduler, StageJob,
    StageRunner, Unlimited,
};

fn graph(src: &str, targets: &[&str]) -> StageGraph {
    let opts = GraphOptions {
        targets: targets.iter().map(|t| (*t).to_owned()).collect(),
        ..GraphOptions::default()
    };
    stage_graph(&parse(src).unwrap(), &opts).unwrap()
}

fn sched(jobs: usize) -> Scheduler {
    Scheduler::new(NonZeroUsize::new(jobs)).with_memory_gate(Unlimited)
}

#[derive(Debug, PartialEq, Eq)]
enum FakeError {
    Failed(usize),
    Cancelled,
}

impl fmt::Display for FakeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for FakeError {}

/// A runner whose output names the stage and its dependencies' outputs, so
/// equal outputs mean the same inputs reached every stage.
#[derive(Default)]
struct Fake {
    events: Mutex<Vec<String>>,
    /// `RUN`s per stage.
    runs: usize,
    /// Yields inside each `RUN`, per stage (default 1): shuffles the
    /// interleaving without timers.
    yields: BTreeMap<usize, usize>,
    /// Stages whose build fails.
    fail: Vec<usize>,
    /// Stages whose preparation fails.
    fail_prepare: Vec<usize>,
    /// Stages that run until cancelled.
    hang: Vec<usize>,
    in_run: AtomicUsize,
    max_in_run: AtomicUsize,
    building: AtomicUsize,
    max_building: AtomicUsize,
}

impl Fake {
    fn event(&self, e: String) {
        self.events.lock().unwrap().push(e);
    }

    fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }

    async fn spin(&self, stage: usize) {
        for _ in 0..*self.yields.get(&stage).unwrap_or(&1) {
            tokio::task::yield_now().await;
        }
    }
}

impl StageRunner for Fake {
    type Prepared = String;
    type Output = String;
    type Error = FakeError;

    async fn prepare(&self, job: PrepareJob<'_>) -> Result<String, FakeError> {
        self.event(format!("prepare {}", job.stage));
        self.spin(job.stage).await;
        if self.fail_prepare.contains(&job.stage) {
            return Err(FakeError::Failed(job.stage));
        }
        Ok(format!("base-of-{}", job.node.display_name))
    }

    async fn build(&self, job: StageJob<'_, Self>) -> Result<String, FakeError> {
        let stage = job.stage;
        self.event(format!("build {stage}"));
        let now = self.building.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_building.fetch_max(now, Ordering::SeqCst);
        let res = async {
            for _ in 0..self.runs {
                let _permit = job.run_slot().await.ok_or(FakeError::Cancelled)?;
                let now = self.in_run.fetch_add(1, Ordering::SeqCst) + 1;
                self.max_in_run.fetch_max(now, Ordering::SeqCst);
                self.spin(stage).await;
                self.in_run.fetch_sub(1, Ordering::SeqCst);
            }
            if self.fail.contains(&stage) {
                return Err(FakeError::Failed(stage));
            }
            if self.hang.contains(&stage) {
                job.cancel.cancelled().await;
                return Err(FakeError::Cancelled);
            }
            let deps: Vec<&str> = job.deps.iter().map(|(_, o)| o.as_str()).collect();
            Ok(format!(
                "{}<{}>[{}]",
                job.node.display_name,
                job.prepared,
                deps.join(",")
            ))
        }
        .await;
        self.building.fetch_sub(1, Ordering::SeqCst);
        self.event(format!("done {stage}"));
        res
    }
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

const DIAMOND: &str = "FROM scratch AS a\n\
                       FROM a AS b\n\
                       FROM a AS c\n\
                       FROM scratch AS d\n\
                       COPY --from=b /x /x\n\
                       COPY --from=c /y /y\n";

#[test]
fn a_diamond_runs_each_stage_after_its_dependencies() {
    let g = graph(DIAMOND, &[]);
    let fake = Fake::default();
    let out = rt()
        .block_on(sched(4).run(&g, &fake, &CancellationToken::new()))
        .unwrap();
    let ev = fake.events();
    let at = |e: &str| ev.iter().position(|x| x == e).unwrap();
    for (dep, stage) in [(0, 1), (0, 2), (1, 3), (2, 3)] {
        assert!(
            at(&format!("done {dep}")) < at(&format!("build {stage}")),
            "{ev:?}"
        );
    }
    assert_eq!(out.target, 3);
    assert_eq!(
        out.target_output().unwrap().as_str(),
        "d<base-of-d>[b<base-of-b>[a<base-of-a>[]],c<base-of-c>[a<base-of-a>[]]]"
    );
    assert_eq!(
        out.stages.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
        [0, 1, 2, 3]
    );
}

#[test]
fn every_preparation_starts_before_any_build() {
    let g = graph(DIAMOND, &[]);
    let fake = Fake::default();
    rt().block_on(sched(4).run(&g, &fake, &CancellationToken::new()))
        .unwrap();
    let ev = fake.events();
    let first_build = ev.iter().position(|e| e.starts_with("build")).unwrap();
    let last_prepare = ev.iter().rposition(|e| e.starts_with("prepare")).unwrap();
    assert!(last_prepare < first_build, "{ev:?}");
}

#[test]
fn stages_off_the_target_path_never_run() {
    let g = graph(
        "FROM scratch AS a\nFROM scratch AS unused\nFROM a AS t\nFROM scratch AS last\n",
        &["t"],
    );
    let fake = Fake::default();
    let out = rt()
        .block_on(sched(2).run(&g, &fake, &CancellationToken::new()))
        .unwrap();
    assert_eq!(out.target, 2);
    let mut ev = fake.events();
    ev.sort();
    assert_eq!(
        ev,
        [
            "build 0",
            "build 2",
            "done 0",
            "done 2",
            "prepare 0",
            "prepare 2"
        ]
    );
}

const WIDE: &str = "FROM scratch AS s0\nFROM scratch AS s1\nFROM scratch AS s2\n\
                    FROM scratch AS s3\nFROM scratch AS s4\nFROM scratch AS s5\n\
                    FROM scratch AS all\n\
                    COPY --from=s0 / /\nCOPY --from=s1 / /\nCOPY --from=s2 / /\n\
                    COPY --from=s3 / /\nCOPY --from=s4 / /\nCOPY --from=s5 / /\n";

#[test]
fn run_permits_never_exceed_jobs() {
    for jobs in [1, 2, 3] {
        let g = graph(WIDE, &[]);
        let fake = Fake {
            runs: 3,
            yields: (0..6).map(|s| (s, 5)).collect(),
            ..Fake::default()
        };
        rt().block_on(sched(jobs).run(&g, &fake, &CancellationToken::new()))
            .unwrap();
        assert_eq!(fake.max_in_run.load(Ordering::SeqCst), jobs, "jobs {jobs}");
    }
}

#[test]
fn jobs_bound_runs_not_stages() {
    let g = graph(WIDE, &[]);
    let fake = Fake {
        runs: 2,
        yields: (0..6).map(|s| (s, 3)).collect(),
        ..Fake::default()
    };
    rt().block_on(sched(1).run(&g, &fake, &CancellationToken::new()))
        .unwrap();
    assert_eq!(fake.max_in_run.load(Ordering::SeqCst), 1);
    assert!(fake.max_building.load(Ordering::SeqCst) > 1);
}

/// Admits a stage only while fewer than `max` run.
struct AtMost(usize);

impl MemoryGate for AtMost {
    fn check(&self, running: usize) -> MemoryCheck {
        MemoryCheck {
            admit: running < self.0,
            current_bytes: Some(1000 + running as u64),
        }
    }
}

#[test]
fn the_memory_gate_bounds_running_stages() {
    let g = graph(WIDE, &[]);
    let fake = Fake {
        runs: 2,
        yields: (0..6).map(|s| (s, 3)).collect(),
        ..Fake::default()
    };
    let s = Scheduler::new(NonZeroUsize::new(8)).with_memory_gate(AtMost(2));
    rt().block_on(s.run(&g, &fake, &CancellationToken::new()))
        .unwrap();
    assert_eq!(fake.max_building.load(Ordering::SeqCst), 2);
}

#[test]
fn a_gate_that_never_admits_still_runs_one_stage_at_a_time() {
    let g = graph(WIDE, &[]);
    let fake = Fake {
        runs: 1,
        ..Fake::default()
    };
    let s = Scheduler::new(NonZeroUsize::new(4)).with_memory_gate(AtMost(0));
    let out = rt()
        .block_on(s.run(&g, &fake, &CancellationToken::new()))
        .unwrap();
    assert_eq!(fake.max_building.load(Ordering::SeqCst), 1);
    assert_eq!(out.stages.len(), 7);
}

#[test]
fn the_first_failure_cancels_the_rest() {
    let g = graph(
        "FROM scratch AS bad\nFROM scratch AS slow\nFROM bad AS after\n\
         FROM scratch AS t\nCOPY --from=slow / /\nCOPY --from=after / /\n",
        &[],
    );
    let fake = Fake {
        runs: 1,
        fail: vec![0],
        hang: vec![1],
        ..Fake::default()
    };
    let err = rt()
        .block_on(sched(4).run(&g, &fake, &CancellationToken::new()))
        .unwrap_err();
    match err {
        Error::Stage {
            stage: 0,
            name,
            source: FakeError::Failed(0),
        } => assert_eq!(name, "bad"),
        e => panic!("{e:?}"),
    }
    let ev = fake.events();
    // The hanging stage was cancelled and returned; the dependents of the
    // failed stage never started.
    assert!(ev.contains(&"done 1".to_owned()), "{ev:?}");
    assert!(!ev.contains(&"build 2".to_owned()), "{ev:?}");
    assert!(!ev.contains(&"build 3".to_owned()), "{ev:?}");
}

#[test]
fn a_failed_preparation_stops_the_build() {
    let g = graph(DIAMOND, &[]);
    let fake = Fake {
        fail_prepare: vec![2],
        ..Fake::default()
    };
    let err = rt()
        .block_on(sched(4).run(&g, &fake, &CancellationToken::new()))
        .unwrap_err();
    assert!(
        matches!(
            err,
            Error::Prepare {
                stage: 2,
                source: FakeError::Failed(2),
                ..
            }
        ),
        "{err:?}"
    );
    assert!(!fake.events().contains(&"build 3".to_owned()));
}

#[test]
fn the_callers_token_cancels_the_build() {
    let g = graph(DIAMOND, &[]);
    let fake = Fake {
        hang: vec![0],
        ..Fake::default()
    };
    let cancel = CancellationToken::new();
    let rt = rt();
    let err = rt
        .block_on(async {
            let c = cancel.clone();
            let canceller = async move {
                for _ in 0..20 {
                    tokio::task::yield_now().await;
                }
                c.cancel();
            };
            let (res, ()) = futures_join(sched(2).run(&g, &fake, &cancel), canceller).await;
            res
        })
        .unwrap_err();
    assert!(matches!(err, Error::Cancelled), "{err:?}");
    assert!(fake.events().contains(&"done 0".to_owned()));
}

async fn futures_join<A: Future, B: Future>(a: A, b: B) -> (A::Output, B::Output) {
    tokio::join!(a, b)
}

#[test]
fn log_tags_count_the_stages_the_build_runs() {
    let g = graph("FROM scratch AS a\nFROM scratch\nFROM a AS t\n", &["t"]);
    struct Tags(Mutex<Vec<String>>);
    impl StageRunner for Tags {
        type Prepared = ();
        type Output = ();
        type Error = FakeError;
        async fn prepare(&self, _job: PrepareJob<'_>) -> Result<(), FakeError> {
            Ok(())
        }
        async fn build(&self, job: StageJob<'_, Self>) -> Result<(), FakeError> {
            self.0.lock().unwrap().push(job.tag.to_string());
            Ok(())
        }
    }
    let tags = Tags(Mutex::default());
    rt().block_on(sched(1).run(&g, &tags, &CancellationToken::new()))
        .unwrap();
    assert_eq!(*tags.0.lock().unwrap(), ["[a 1/2]", "[t 2/2]"]);
}

#[test]
fn a_multi_thread_runtime_gives_the_same_outputs() {
    let g = graph(DIAMOND, &[]);
    let fake = Fake {
        runs: 2,
        ..Fake::default()
    };
    let multi = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    let out = multi
        .block_on(sched(3).run(&g, &fake, &CancellationToken::new()))
        .unwrap();
    let reference = rt()
        .block_on(sched(1).run(&g, &Fake::default(), &CancellationToken::new()))
        .unwrap();
    let flat = |s: &rnk_sched::BuildSummary<String>| {
        s.stages
            .iter()
            .map(|(i, o)| (*i, o.as_str().to_owned()))
            .collect::<Vec<_>>()
    };
    assert_eq!(flat(&out), flat(&reference));
}

/// A random DAG: stage `i` builds on scratch or an earlier stage and copies
/// from some earlier stages.
fn dockerfile(bases: &[Option<usize>], copies: &[Vec<usize>]) -> String {
    let mut s = String::new();
    for (i, (base, from)) in bases.iter().zip(copies).enumerate() {
        match base {
            Some(b) if *b < i => s.push_str(&format!("FROM s{b} AS s{i}\n")),
            _ => s.push_str(&format!("FROM scratch AS s{i}\n")),
        }
        for f in from.iter().filter(|f| **f < i) {
            s.push_str(&format!("COPY --from=s{f} / /\n"));
        }
    }
    s
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn outputs_do_not_depend_on_completion_order_or_jobs(
        bases in prop::collection::vec(prop::option::of(0usize..8), 1..8),
        copies in prop::collection::vec(prop::collection::vec(0usize..8, 0..3), 8),
        yields in prop::collection::vec(0usize..12, 8),
        jobs in 1usize..5,
    ) {
        let g = graph(&dockerfile(&bases, &copies), &[]);
        let reference = rt()
            .block_on(sched(1).run(&g, &Fake { runs: 1, ..Fake::default() }, &CancellationToken::new()))
            .unwrap();
        let shuffled = Fake {
            runs: 2,
            yields: yields.iter().copied().enumerate().collect(),
            ..Fake::default()
        };
        let out = rt()
            .block_on(sched(jobs).run(&g, &shuffled, &CancellationToken::new()))
            .unwrap();
        prop_assert_eq!(&out.stages.iter().map(|(s, o)| (*s, (**o).clone())).collect::<Vec<_>>(),
            &reference.stages.iter().map(|(s, o)| (*s, (**o).clone())).collect::<Vec<_>>());
        prop_assert!(shuffled.max_in_run.load(Ordering::SeqCst) <= jobs);
    }
}
