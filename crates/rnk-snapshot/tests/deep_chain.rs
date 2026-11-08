//! A kaniko-mode stage's copy and hand-over of a deep chain take time
//! linear in its depth, each asking the ignore list about every entry.
//!
//! A test binary of its own: the copy holds two descriptors a level, so the
//! test raises this process's soft `RLIMIT_NOFILE` to the hard one, as
//! kaniko mode does, which no other test should run under.

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use rnk_rootfs::IgnoreList;
use rnk_snapshot::{Base, KanikoStage, KanikoStages, Snapshotter, StageRootfs, WalkOptions};
use rnk_store::{BuildPaths, Store};
use rustix::fs::{Mode, OFlags};
use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};

/// Levels of a chain a `RUN` makes in seconds, as many as the copy can
/// hold descriptors for under the hard `RLIMIT_NOFILE`, the soft limit
/// raised to it: 100,000 at most, none below 30,000, too few levels for a
/// cost growing with the square of the depth to show.
fn deepest_copyable() -> Option<usize> {
    let was = getrlimit(Resource::Nofile);
    // Where raising fails, the chain is only shallower.
    let _ = setrlimit(
        Resource::Nofile,
        Rlimit {
            current: was.maximum,
            maximum: was.maximum,
        },
    );
    let soft = getrlimit(Resource::Nofile).current.unwrap_or(u64::MAX);
    // Room for the store's and the harness's own descriptors.
    let depth = usize::try_from(soft.saturating_sub(1024) / 2)
        .unwrap_or(usize::MAX)
        .min(100_000);
    (depth >= 30_000).then_some(depth)
}

/// A chain of `depth` directories named `d` in `p`, made by descriptor,
/// with a file at its end.
fn chain(p: &Path, depth: usize) {
    let mut fd = rustix::fs::open(p, OFlags::PATH, Mode::empty()).unwrap();
    for _ in 0..depth {
        rustix::fs::mkdirat(&fd, "d", Mode::from_raw_mode(0o755)).unwrap();
        fd = rustix::fs::openat(&fd, "d", OFlags::PATH | OFlags::DIRECTORY, Mode::empty()).unwrap();
    }
    let leaf = rustix::fs::openat(
        &fd,
        "leaf",
        OFlags::WRONLY | OFlags::CREATE,
        Mode::from_raw_mode(0o644),
    )
    .unwrap();
    rustix::io::write(&leaf, b"deep").unwrap();
}

/// A stage of `stages` holding a chain of `depth` levels, never walked.
fn parent(stages: &KanikoStages, depth: usize) -> KanikoStage {
    let mut s = stages.prepare(Base::Scratch).unwrap();
    s.set_layers_needed(false);
    s.begin_layer().unwrap();
    chain(s.mount_point(), depth);
    s.end_layer().unwrap();
    s.done();
    s
}

#[test]
fn a_deep_chain_is_copied_and_handed_over_in_time_linear_in_its_depth() {
    let Some(depth) = deepest_copyable() else {
        eprintln!("skipped: the hard RLIMIT_NOFILE holds no chain of 30,000 levels");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let top = fs::canonicalize(tmp.path()).unwrap();
    let store = Store::open(&BuildPaths::new(&top, top.join("state"))).unwrap();
    let mut ignore = IgnoreList::new();
    ignore.insert("/proc").unwrap();
    let stages = KanikoStages::new(&store, ignore, WalkOptions::default()).unwrap();
    let made = Instant::now();
    let parent = parent(&stages, depth);
    let made = made.elapsed();
    let t0 = Instant::now();
    let child = stages.prepare(Base::Stage(&parent)).unwrap();
    let copied = t0.elapsed();
    child.release().unwrap();
    let t0 = Instant::now();
    let last = stages.prepare_last(&parent).unwrap();
    let handed = t0.elapsed();
    last.release().unwrap();
    parent.release().unwrap();
    eprintln!("depth {depth}: made {made:.2?}, copied {copied:.2?}, handed over {handed:.2?}");
    // A fixed amount of work per level, as making it: never quadratic in
    // depth, which would take minutes here.
    for (what, took) in [("copying", copied), ("handing over", handed)] {
        assert!(
            took < 20 * made + Duration::from_secs(10),
            "{what} took {took:?}, making {made:?}"
        );
    }
}
