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
use tokio::sync::Notify;

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
    /// Stages whose preparation runs until cancelled.
    hang_prepare: Vec<usize>,
    /// Notified when a build starts.
    built: Notify,
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
        if self.hang_prepare.contains(&job.stage) {
            job.cancel.cancelled().await;
            self.event(format!("prepare {} cancelled", job.stage));
            return Err(FakeError::Cancelled);
        }
        Ok(format!("base-of-{}", job.node.display_name))
    }

    async fn build(&self, job: StageJob<'_, Self>) -> Result<String, FakeError> {
        let stage = job.stage;
        self.event(format!("build {stage}"));
        self.built.notify_one();
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
fn preparations_start_before_builds_on_a_small_graph() {
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
    let err = cancel_once_built(&sched(2), &g, &fake);
    assert!(matches!(err, Error::Cancelled), "{err:?}");
    assert!(fake.events().contains(&"done 0".to_owned()));
}

/// Runs `fake` and cancels the build once a stage has started building.
fn cancel_once_built(s: &Scheduler, g: &StageGraph, fake: &Fake) -> Error<FakeError> {
    let cancel = CancellationToken::new();
    rt().block_on(async {
        let canceller = async {
            fake.built.notified().await;
            // Let the other stages reach their waits before the cancel.
            for _ in 0..10 {
                tokio::task::yield_now().await;
            }
            cancel.cancel();
        };
        let (res, ()) = tokio::join!(s.run(g, fake, &cancel), canceller);
        res.unwrap_err()
    })
}

#[test]
fn cancelling_reaches_a_stage_waiting_in_the_memory_gate() {
    let g = graph(WIDE, &[]);
    let fake = Fake {
        hang: (0..6).collect(),
        ..Fake::default()
    };
    let s = Scheduler::new(NonZeroUsize::new(4)).with_memory_gate(AtMost(1));
    let err = cancel_once_built(&s, &g, &fake);
    assert!(matches!(err, Error::Cancelled), "{err:?}");
    let ev = fake.events();
    // One stage was building; the others waited in the gate and never
    // started.
    assert_eq!(
        ev.iter().filter(|e| e.starts_with("build")).count(),
        1,
        "{ev:?}"
    );
    assert_eq!(
        ev.iter().filter(|e| e.starts_with("done")).count(),
        1,
        "{ev:?}"
    );
}

#[test]
fn a_running_preparation_is_awaited_after_a_failure() {
    let g = graph(
        "FROM scratch AS bad\nFROM alpine AS slow\nFROM scratch AS t\n\
         COPY --from=bad / /\nCOPY --from=slow / /\n",
        &[],
    );
    let fake = Fake {
        fail: vec![0],
        hang_prepare: vec![1],
        ..Fake::default()
    };
    let err = rt()
        .block_on(sched(2).run(&g, &fake, &CancellationToken::new()))
        .unwrap_err();
    assert!(matches!(err, Error::Stage { stage: 0, .. }), "{err:?}");
    // `run` returned only after the hanging preparation saw the cancel.
    assert!(
        fake.events().contains(&"prepare 1 cancelled".to_owned()),
        "{:?}",
        fake.events()
    );
}

#[test]
fn an_inconsistent_graph_is_refused() {
    let mut g = graph(DIAMOND, &[]);
    let fake = Fake::default();
    let run = |g: &StageGraph| {
        rt().block_on(sched(1).run(g, &fake, &CancellationToken::new()))
            .unwrap_err()
    };
    // `d` (3) needs `b` (1), which is no longer listed before it.
    g.needed = vec![0, 2, 3, 1];
    let err = run(&g);
    assert!(matches!(err, Error::Graph { stage: 3, .. }), "{err}");
    assert!(err.to_string().contains("depends on stage 1"), "{err}");
    g.needed = vec![0, 0];
    assert!(matches!(run(&g), Error::Graph { stage: 0, .. }));
    g.needed = vec![7];
    assert!(matches!(run(&g), Error::Graph { stage: 7, .. }));
    assert!(fake.events().is_empty());
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
