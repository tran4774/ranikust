//! Units: kaniko's squashed stages and the shared root, with a fake runner.

use std::collections::BTreeMap;
use std::fmt;
use std::num::NonZeroUsize;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use rnk_dockerfile::{GraphOptions, StageGraph, parse, squash_for_shared_root, stage_graph};
use rnk_sched::{
    CancellationToken, Error, Placement, PrepareJob, Scheduler, StageJob, StageRunner, Unit,
    Unlimited, own_root_units, shared_root_units,
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

#[derive(Debug)]
struct Never;

impl fmt::Display for Never {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("never")
    }
}

impl std::error::Error for Never {}

/// What a unit's build was handed: folded stages, placement, dependencies.
type Handed = (Vec<usize>, Placement, Vec<usize>);

/// Records what each unit was handed and how many units ran at once,
/// overall and on the shared root.
#[derive(Default)]
struct Rec {
    events: Mutex<Vec<String>>,
    jobs: Mutex<BTreeMap<usize, Handed>>,
    /// Yields inside a build, per stage (default 1).
    yields: BTreeMap<usize, usize>,
    building: AtomicUsize,
    max_building: AtomicUsize,
    shared: AtomicUsize,
    max_shared: AtomicUsize,
}

impl Rec {
    fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }
}

impl StageRunner for Rec {
    type Prepared = Vec<usize>;
    type Output = String;
    type Error = Never;

    async fn prepare(&self, job: PrepareJob<'_>) -> Result<Vec<usize>, Never> {
        Ok(job.folded.to_vec())
    }

    async fn build(&self, job: StageJob<'_, Self>) -> Result<String, Never> {
        let stage = job.stage;
        assert_eq!(job.prepared, job.folded);
        let deps: Vec<usize> = job.deps.iter().map(|(s, _)| *s).collect();
        self.jobs
            .lock()
            .unwrap()
            .insert(stage, (job.folded.to_vec(), job.placement, deps));
        self.events.lock().unwrap().push(format!("build {stage}"));
        let now = self.building.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_building.fetch_max(now, Ordering::SeqCst);
        let shared = job.placement == Placement::Shared;
        if shared {
            let now = self.shared.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_shared.fetch_max(now, Ordering::SeqCst);
        }
        for _ in 0..*self.yields.get(&stage).unwrap_or(&1) {
            tokio::task::yield_now().await;
        }
        if shared {
            self.shared.fetch_sub(1, Ordering::SeqCst);
        }
        self.building.fetch_sub(1, Ordering::SeqCst);
        self.events.lock().unwrap().push(format!("done {stage}"));
        let outs: Vec<&str> = job.deps.iter().map(|(_, o)| o.as_str()).collect();
        Ok(format!("{}[{}]", job.node.display_name, outs.join(",")))
    }
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

const WIDE: &str = "FROM scratch AS s0\nFROM scratch AS s1\nFROM scratch AS s2\n\
                    FROM scratch AS s3\nFROM scratch AS all\n\
                    COPY --from=s0 / /\nCOPY --from=s1 / /\nCOPY --from=s2 / /\n\
                    COPY --from=s3 / /\n";

fn all_shared(g: &StageGraph) -> Vec<Unit> {
    own_root_units(g)
        .into_iter()
        .map(|u| Unit {
            placement: Placement::Shared,
            ..u
        })
        .collect()
}

#[test]
fn shared_units_run_one_at_a_time_in_unit_order() {
    let g = graph(WIDE, &[]);
    // Later stages finish their builds sooner, so readiness alone would
    // reorder them.
    let rec = Rec {
        yields: (0..4).map(|s| (s, 20 - 4 * s)).collect(),
        ..Rec::default()
    };
    rt().block_on(sched(8).run_units(&g, &all_shared(&g), &rec, &CancellationToken::new()))
        .unwrap();
    assert_eq!(rec.max_building.load(Ordering::SeqCst), 1);
    let builds: Vec<String> = rec
        .events()
        .into_iter()
        .filter(|e| e.starts_with("build"))
        .collect();
    assert_eq!(
        builds,
        ["build 0", "build 1", "build 2", "build 3", "build 4"]
    );
}

#[test]
fn own_roots_still_run_beside_the_shared_one() {
    let g = graph(WIDE, &[]);
    let mut units = own_root_units(&g);
    for u in units.iter_mut().filter(|u| u.stage % 2 == 0) {
        u.placement = Placement::Shared;
    }
    let rec = Rec {
        yields: (0..4).map(|s| (s, 10)).collect(),
        ..Rec::default()
    };
    rt().block_on(sched(8).run_units(&g, &units, &rec, &CancellationToken::new()))
        .unwrap();
    assert_eq!(rec.max_shared.load(Ordering::SeqCst), 1);
    assert!(rec.max_building.load(Ordering::SeqCst) > 1);
}

#[test]
fn a_squashed_chain_runs_as_one_unit() {
    let g = graph(
        "FROM alpine AS src\nRUN make\n\
         FROM alpine AS a\nRUN one\n\
         FROM a AS b\nCOPY --from=src /x /x\n\
         FROM b AS c\nRUN three\n",
        &[],
    );
    let plan = squash_for_shared_root(&g);
    let units = shared_root_units(&plan);
    assert_eq!(
        units,
        [
            Unit {
                stage: 0,
                folded: vec![],
                placement: Placement::Shared
            },
            Unit {
                stage: 3,
                folded: vec![1, 2],
                placement: Placement::Shared
            },
        ]
    );
    let rec = Rec::default();
    let out = rt()
        .block_on(sched(4).run_units(&g, &units, &rec, &CancellationToken::new()))
        .unwrap();
    // The folded stage's `COPY --from=src` becomes the unit's dependency.
    let jobs = rec.jobs.lock().unwrap().clone();
    assert_eq!(jobs[&3], (vec![1, 2], Placement::Shared, vec![0]));
    assert_eq!(out.target, 3);
    assert_eq!(out.target_output().unwrap().as_str(), "c[src[]]");
    assert_eq!(
        out.stages.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
        [0, 3]
    );
}

#[test]
fn units_out_of_order_are_refused() {
    let g = graph("FROM alpine AS a\nFROM a AS b\n", &[]);
    let mut units = own_root_units(&g);
    units.reverse();
    let e = rt()
        .block_on(sched(1).run_units(&g, &units, &Rec::default(), &CancellationToken::new()))
        .unwrap_err();
    assert!(
        matches!(&e, Error::Graph { stage: 1, reason } if reason.contains("no earlier unit")),
        "{e:?}"
    );
    let twice = vec![Unit::own(0), Unit::own(0)];
    let e = rt()
        .block_on(sched(1).run_units(&g, &twice, &Rec::default(), &CancellationToken::new()))
        .unwrap_err();
    assert!(matches!(e, Error::Graph { stage: 0, .. }), "{e:?}");
}

#[test]
fn shared_order_is_the_same_on_a_multi_thread_runtime() {
    let g = graph(WIDE, &[]);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    for _ in 0..20 {
        let rec = Rec::default();
        rt.block_on(sched(8).run_units(&g, &all_shared(&g), &rec, &CancellationToken::new()))
            .unwrap();
        let ev = rec.events();
        let want: Vec<String> = (0..5)
            .flat_map(|s| [format!("build {s}"), format!("done {s}")])
            .collect();
        assert_eq!(ev, want);
    }
}
