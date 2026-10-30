//! Runs the stages the targets need, each once its dependencies are done.

use std::num::NonZeroUsize;
use std::pin::pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use futures_util::StreamExt as _;
use futures_util::future::{Either, join, select};
use futures_util::stream::FuturesUnordered;
use rnk_dockerfile::{Base, StageGraph, StageNode};
use tokio::sync::{Notify, oneshot, watch};
use tokio_util::sync::CancellationToken;
use tracing::field::Empty;
use tracing::{Instrument as _, Span, debug_span};

use crate::error::Error;
use crate::memory::{MemoryGate, SystemMemory};
use crate::runner::{LogTag, PrepareJob, StageJob, StageRunner};
use crate::slots::RunSlots;
use crate::unit::{Placement, Unit, own_root_units};

/// How often a stage held back by the memory gate asks again when no
/// stage has finished in the meantime.
const MEMORY_POLL: Duration = Duration::from_millis(250);

/// Runs a stage graph: `--jobs` `RUN` permits shared by all stages, a
/// memory gate in front of each stage.
pub struct Scheduler {
    jobs: NonZeroUsize,
    memory: Box<dyn MemoryGate>,
}

impl std::fmt::Debug for Scheduler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scheduler")
            .field("jobs", &self.jobs)
            .finish_non_exhaustive()
    }
}

/// What a finished build hands back: every stage's output.
#[derive(Debug)]
pub struct BuildSummary<O> {
    /// `(stage, output)` for every unit the build ran, in the order of the
    /// units (file order), whatever order they finished in.
    pub stages: Vec<(usize, Arc<O>)>,
    /// The stage whose image is the result.
    pub target: usize,
}

impl<O> BuildSummary<O> {
    /// The output of `stage`, if the build ran it.
    #[must_use]
    pub fn output(&self, stage: usize) -> Option<&Arc<O>> {
        self.stages
            .iter()
            .find(|(s, _)| *s == stage)
            .map(|(_, o)| o)
    }

    /// The output of the result stage.
    #[must_use]
    pub fn target_output(&self) -> Option<&Arc<O>> {
        self.output(self.target)
    }
}

impl Scheduler {
    /// `jobs` `RUN`s at once, by default `available_parallelism()`
    /// (ADR-0013), which honours the cgroup CPU quota and the affinity
    /// mask; the memory gate reads the host ([`SystemMemory`]).
    #[must_use]
    pub fn new(jobs: Option<NonZeroUsize>) -> Self {
        // `available_parallelism` fails only where the count is unknown;
        // one job is then the safe default.
        let jobs = jobs
            .unwrap_or_else(|| std::thread::available_parallelism().unwrap_or(NonZeroUsize::MIN));
        Self {
            jobs,
            memory: Box::new(SystemMemory::detect()),
        }
    }

    /// Replaces the memory gate.
    #[must_use]
    pub fn with_memory_gate(mut self, gate: impl MemoryGate + 'static) -> Self {
        self.memory = Box::new(gate);
        self
    }

    /// The `--jobs` value in effect.
    #[must_use]
    pub fn jobs(&self) -> NonZeroUsize {
        self.jobs
    }

    /// Builds every stage in `graph.needed` with `runner`: up-front work
    /// for all of them first, then each stage once its dependencies are
    /// done. The first failure cancels the others; the call returns only
    /// once every started `prepare` and `build` has returned.
    ///
    /// Must run inside a tokio runtime with its timer enabled.
    ///
    /// On failure the stages' outputs and any unbuilt `Prepared` values are
    /// dropped; the runner must not rely on that to clean up (see
    /// [`StageRunner`]).
    ///
    /// # Cancel safety
    ///
    /// Not cancel safe: dropping the future drops every stage's `prepare`
    /// and `build` mid-way, and nothing undoes what they started. To stop
    /// a build, cancel `cancel` and keep awaiting until `run` returns
    /// [`Error::Cancelled`].
    ///
    /// # Errors
    ///
    /// The first stage that failed ([`Error::Stage`], [`Error::Prepare`]),
    /// [`Error::Cancelled`] when `cancel` fired first, or [`Error::Graph`]
    /// for an inconsistent graph.
    pub async fn run<R: StageRunner>(
        &self,
        graph: &StageGraph,
        runner: &R,
        cancel: &CancellationToken,
    ) -> Result<BuildSummary<R::Output>, Error<R::Error>> {
        self.run_units(graph, &own_root_units(graph), runner, cancel)
            .await
    }

    /// [`Scheduler::run`] over explicit units: kaniko's squashed stages
    /// ([`crate::shared_root_units`]) or stages on roots of their own
    /// ([`crate::own_root_units`]). Units must come in an order where each
    /// one's dependencies are earlier. A [`Placement::Shared`] unit starts
    /// only once the shared unit before it in `units` has finished, so
    /// stages on the shared root never overlap and always run in the same
    /// order; `--jobs` still bounds `RUN`s, which on the shared root changes
    /// nothing, since one stage runs there at a time.
    ///
    /// # Errors
    ///
    /// As [`Scheduler::run`]; [`Error::Graph`] also for a unit listed twice,
    /// a stage missing from the graph or a dependency on a stage no
    /// earlier unit builds.
    pub async fn run_units<R: StageRunner>(
        &self,
        graph: &StageGraph,
        units: &[Unit],
        runner: &R,
        cancel: &CancellationToken,
    ) -> Result<BuildSummary<R::Output>, Error<R::Error>> {
        let span = debug_span!(
            "rnk_sched.run",
            stages = units.len() as u64,
            jobs = self.jobs.get() as u64,
            max_running = Empty,
        );
        self.run_in(graph, units, runner, cancel, &span)
            .instrument(span.clone())
            .await
    }

    async fn run_in<R: StageRunner>(
        &self,
        graph: &StageGraph,
        units: &[Unit],
        runner: &R,
        cancel: &CancellationToken,
        span: &Span,
    ) -> Result<BuildSummary<R::Output>, Error<R::Error>> {
        let plan = Plan::new(graph, units)?;
        let n = plan.stages.len();
        let token = cancel.child_token();
        let slots = RunSlots::new(self.jobs);
        let ctx = Ctx {
            sched: self,
            graph,
            runner,
            plan: &plan,
            token: &token,
            slots: &slots,
            failure: Mutex::new(None),
            running: AtomicUsize::new(0),
            max_running: AtomicUsize::new(0),
            finished: Notify::new(),
            done: (0..n).map(|_| watch::channel(Slot::Pending).0).collect(),
        };

        let (txs, rxs): (Vec<_>, Vec<_>) = (0..n).map(|_| oneshot::channel()).unzip();
        let mut preps: FuturesUnordered<_> = txs
            .into_iter()
            .enumerate()
            .map(|(k, tx)| ctx.prepare(k, tx))
            .collect();
        let mut stages: FuturesUnordered<_> = rxs
            .into_iter()
            .enumerate()
            .map(|(k, rx)| ctx.stage(k, rx))
            .collect();
        // `join` polls the preparations first, so they start before the
        // stages are built. Not a guarantee: `FuturesUnordered` yields
        // after 32 polls, so on a wider graph a build may start before the
        // last preparations do.
        join(async { while preps.next().await.is_some() {} }, async {
            while stages.next().await.is_some() {}
        })
        .await;

        span.record(
            "max_running",
            ctx.max_running.load(Ordering::Relaxed) as u64,
        );
        if let Some(e) = ctx
            .failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            return Err(e);
        }
        let mut out = Vec::with_capacity(n);
        for (st, tx) in plan.stages.iter().zip(&ctx.done) {
            match &*tx.borrow() {
                Slot::Done(o) => out.push((st.stage, o.clone())),
                // Without a recorded failure, a stage only stops early when
                // the caller cancelled.
                Slot::Pending | Slot::Failed => return Err(Error::Cancelled),
            }
        }
        Ok(BuildSummary {
            stages: out,
            target: graph.primary_target(),
        })
    }
}

/// A finished or pending stage, as its dependents see it.
enum Slot<O> {
    Pending,
    Done(Arc<O>),
    Failed,
}

/// One unit, with its dependencies resolved to positions.
struct Planned<'a> {
    stage: usize,
    node: &'a StageNode,
    folded: &'a [usize],
    placement: Placement,
    /// `(stage, position in Plan::stages)`: the dependencies of every stage
    /// of the chain, base first, without the chain's own stages.
    deps: Vec<(usize, usize)>,
    /// For a shared unit, the position of the shared unit before it.
    after: Option<usize>,
    tag: LogTag,
}

struct Plan<'a> {
    stages: Vec<Planned<'a>>,
}

impl<'a> Plan<'a> {
    fn new<E: std::error::Error>(
        graph: &'a StageGraph,
        units: &'a [Unit],
    ) -> Result<Self, Error<E>> {
        let n = units.len();
        let pos = |s: usize| units.iter().position(|u| u.stage == s);
        let bad = |stage: usize, reason: String| Error::Graph { stage, reason };
        let mut stages = Vec::with_capacity(n);
        let mut last_shared = None;
        for (k, unit) in units.iter().enumerate() {
            let stage = unit.stage;
            if pos(stage) != Some(k) {
                return Err(bad(stage, "listed twice in the units".to_owned()));
            }
            let node = graph
                .stages
                .get(stage)
                .ok_or_else(|| bad(stage, "not in the graph".to_owned()))?;
            let mut deps: Vec<(usize, usize)> = Vec::new();
            for c in unit.chain() {
                let member = graph
                    .stages
                    .get(c)
                    .ok_or_else(|| bad(c, "not in the graph".to_owned()))?;
                for d in member.dependencies() {
                    if unit.chain().any(|x| x == d) || deps.iter().any(|&(x, _)| x == d) {
                        continue;
                    }
                    // A dependency comes earlier in the units, so the plan
                    // has no cycle and every wait ends.
                    let p = pos(d).filter(|&p| p < k).ok_or_else(|| {
                        bad(
                            stage,
                            format!("depends on stage {d}, which no earlier unit builds"),
                        )
                    })?;
                    deps.push((d, p));
                }
            }
            let after = match unit.placement {
                Placement::Own => None,
                Placement::Shared => last_shared.replace(k),
            };
            stages.push(Planned {
                stage,
                node,
                folded: &unit.folded,
                placement: unit.placement,
                deps,
                after,
                tag: LogTag {
                    name: node.display_name.clone(),
                    k: k + 1,
                    n,
                },
            });
        }
        Ok(Self { stages })
    }
}

/// What every stage's future shares.
struct Ctx<'a, R: StageRunner> {
    sched: &'a Scheduler,
    graph: &'a StageGraph,
    runner: &'a R,
    plan: &'a Plan<'a>,
    token: &'a CancellationToken,
    slots: &'a RunSlots,
    failure: Mutex<Option<Error<R::Error>>>,
    running: AtomicUsize,
    max_running: AtomicUsize,
    finished: Notify,
    done: Vec<watch::Sender<Slot<R::Output>>>,
}

impl<R: StageRunner> Ctx<'_, R> {
    /// Records `e` if it is the first failure, and cancels the build. A
    /// failure after the cancellation is its consequence, not reported.
    fn fail(&self, e: Error<R::Error>) {
        let mut f = self.failure.lock().unwrap_or_else(PoisonError::into_inner);
        if f.is_none() && !self.token.is_cancelled() {
            *f = Some(e);
        }
        self.token.cancel();
    }

    async fn prepare(&self, k: usize, tx: oneshot::Sender<Option<R::Prepared>>) {
        let Some(st) = self.plan.stages.get(k) else {
            return;
        };
        let job = PrepareJob {
            stage: st.stage,
            node: st.node,
            folded: st.folded,
            placement: st.placement,
            graph: self.graph,
            tag: st.tag.clone(),
            cancel: self.token.clone(),
        };
        let prepared = match self.runner.prepare(job).await {
            Ok(p) => Some(p),
            Err(source) => {
                self.fail(Error::Prepare {
                    stage: st.stage,
                    name: st.node.display_name.clone(),
                    source,
                });
                None
            }
        };
        // The stage's future may be gone only if the build ended; then
        // nobody needs the value.
        let _ = tx.send(prepared);
    }

    async fn stage(&self, k: usize, prepared: oneshot::Receiver<Option<R::Prepared>>) {
        let Some(st) = self.plan.stages.get(k) else {
            return;
        };
        let span = debug_span!(
            "rnk_sched.stage",
            stage = st.stage as u64,
            name = st.node.display_name.as_str(),
            base = base_text(self.graph, st.node),
            max_memory_bytes = Empty,
        );
        let ok = self.stage_in(k, st, prepared).instrument(span).await;
        if !ok && let Some(slot) = self.done.get(k) {
            slot.send_replace(Slot::Failed);
        }
    }

    /// Builds stage `k`; false if it did not produce an output.
    async fn stage_in(
        &self,
        k: usize,
        st: &Planned<'_>,
        prepared: oneshot::Receiver<Option<R::Prepared>>,
    ) -> bool {
        let deps = async {
            let deps = self.wait_deps(&st.deps).await?;
            // The shared root is free once the shared unit before this one
            // has ended; if it failed, the build is over.
            match st.after {
                Some(p) => self.wait_done(p).await.map(|_| deps),
                None => Some(deps),
            }
        }
        .instrument(debug_span!("rnk_sched.wait_deps"));
        let (prepared, deps) = join(prepared, deps).await;
        let (Ok(Some(prepared)), Some(deps)) = (prepared, deps) else {
            return false;
        };
        let Some(current) = self.admit().await else {
            return false;
        };
        if let Some(bytes) = current {
            Span::current().record("max_memory_bytes", bytes);
        }

        let job = StageJob {
            stage: st.stage,
            node: st.node,
            folded: st.folded,
            placement: st.placement,
            graph: self.graph,
            deps,
            prepared,
            tag: st.tag.clone(),
            cancel: self.token.clone(),
            slots: self.slots,
        };
        let res = self.runner.build(job).await;
        self.running.fetch_sub(1, Ordering::Relaxed);
        self.finished.notify_waiters();
        match res {
            Ok(out) => {
                if let Some(slot) = self.done.get(k) {
                    slot.send_replace(Slot::Done(Arc::new(out)));
                }
                true
            }
            Err(source) => {
                self.fail(Error::Stage {
                    stage: st.stage,
                    name: st.node.display_name.clone(),
                    source,
                });
                false
            }
        }
    }

    /// The outputs of `deps`, or `None` if one failed or the build was
    /// cancelled.
    async fn wait_deps(&self, deps: &[(usize, usize)]) -> Option<Vec<(usize, Arc<R::Output>)>> {
        let mut out = Vec::with_capacity(deps.len());
        for &(stage, p) in deps {
            out.push((stage, self.wait_done(p).await?));
        }
        Some(out)
    }

    /// The output of the unit at position `p`, or `None` if it failed or
    /// the build was cancelled.
    async fn wait_done(&self, p: usize) -> Option<Arc<R::Output>> {
        let mut rx = self.done.get(p)?.subscribe();
        let cancelled = pin!(self.token.cancelled());
        let finished = pin!(rx.wait_for(|s| !matches!(s, Slot::Pending)));
        match select(cancelled, finished).await {
            Either::Left(((), _)) => None,
            Either::Right((Ok(slot), _)) => match &*slot {
                Slot::Done(o) => Some(o.clone()),
                Slot::Pending | Slot::Failed => None,
            },
            // The senders live in `Ctx` until every stage has ended.
            Either::Right((Err(_), _)) => None,
        }
    }

    /// Waits until the memory gate lets one more stage start, and counts
    /// it as running. Returns the highest `memory.current` seen, or `None`
    /// if the build was cancelled first.
    async fn admit(&self) -> Option<Option<u64>> {
        let mut seen: Option<u64> = None;
        let mut waiting: Option<Span> = None;
        loop {
            let mut finished = pin!(self.finished.notified());
            finished.as_mut().enable();
            if self.token.is_cancelled() {
                return None;
            }
            let running = self.running.load(Ordering::Relaxed);
            let check = self.sched.memory.check(running);
            seen = seen.max(check.current_bytes);
            // With nothing running, the gate cannot help: a stage that
            // waited here would wait forever.
            if running == 0 || check.admit {
                let now = self.running.fetch_add(1, Ordering::Relaxed) + 1;
                self.max_running.fetch_max(now, Ordering::Relaxed);
                return Some(seen);
            }
            let span = waiting
                .get_or_insert_with(|| debug_span!("rnk_sched.wait_memory"))
                .clone();
            let cancelled = pin!(self.token.cancelled());
            let poll = pin!(tokio::time::sleep(MEMORY_POLL));
            select(finished, select(cancelled, poll))
                .instrument(span)
                .await;
        }
    }
}

/// The base as the stage record shows it: an image, `scratch`, or the base
/// stage's name.
fn base_text<'a>(graph: &'a StageGraph, node: &'a StageNode) -> &'a str {
    match &node.base {
        Base::Scratch => "scratch",
        Base::Image(i) => i,
        Base::Stage(s) => graph.stages.get(*s).map_or("", |b| b.display_name.as_str()),
    }
}

/// Compile-time check: `run`'s future is `Send` for every runner, so a
/// caller may spawn it on a multi-thread runtime.
fn _run_is_send<R: StageRunner>(
    sched: &Scheduler,
    graph: &StageGraph,
    runner: &R,
    cancel: &CancellationToken,
) {
    fn send<T: Send>(_: T) {}
    send(sched.run(graph, runner, cancel));
}
