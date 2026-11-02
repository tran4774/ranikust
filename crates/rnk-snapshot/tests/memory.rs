//! Red team (T15): the heap the kaniko-mode walk takes for a deep tree a
//! `RUN` made, measured by a counting allocator (T0).
//!
//! A `RUN` makes a chain of directories as deep as it likes by `mkdir`
//! and `cd`, far past `PATH_MAX`; `rnk-rootfs`'s deletion and pruning hold
//! one name per level for it (`crates/rnk-rootfs/tests/memory.rs`). The
//! walk that diffs the root after the `RUN` should cost what the tree
//! holds, not the sum of every directory's path, which grows with the
//! square of the depth.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use rnk_rootfs::{IgnoreList, RootPath};
use rnk_snapshot::{WalkOptions, Walker};

struct Counting;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grew(by: usize) {
    let now = CURRENT.fetch_add(by, Ordering::Relaxed) + by;
    PEAK.fetch_max(now, Ordering::Relaxed);
}

// SAFETY: every call is forwarded to `System` unchanged; the counters only
// observe sizes.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's contract for `alloc` is passed on as is.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            grew(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
        // SAFETY: the caller's contract for `dealloc` is passed on as is.
        unsafe { System.dealloc(ptr, layout) };
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the caller's contract for `realloc` is passed on as is.
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
            grew(new_size);
        }
        p
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

const MIB: usize = 1 << 20;

/// The peak heap `f` adds above what was allocated when it started.
fn peak_of<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let base = CURRENT.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let out = f();
    (out, PEAK.load(Ordering::Relaxed).saturating_sub(base))
}

/// A chain of `depth` directories with 200-byte names below `host`, made by
/// descriptor as a `RUN` makes one with `mkdir` and `cd`: its path is far
/// past `PATH_MAX`.
fn deep_chain(host: &std::path::Path, depth: usize) {
    use rustix::fs::{Mode, OFlags};
    let name = "n".repeat(200);
    let dir = OFlags::PATH | OFlags::DIRECTORY;
    let mut cur = rustix::fs::open(host, dir, Mode::empty()).unwrap();
    for _ in 0..depth {
        rustix::fs::mkdirat(&cur, name.as_str(), Mode::from_raw_mode(0o755)).unwrap();
        cur = rustix::fs::openat(&cur, name.as_str(), dir, Mode::empty()).unwrap();
    }
}

/// Red team (T15): a `RUN` that leaves a chain of 400 directories with
/// 200-byte names (80 KB of path at the bottom, 400 entries in all) makes
/// the kaniko-mode walk that diffs the root hold about 4 MiB at most, as
/// deletion and pruning of the same chain do: a cost that follows the
/// tree, not the sum of its paths (about 16 MB for this chain, and for a
/// chain of depth `d` growing with `d²`).
#[test]
fn red_team_walking_a_deep_chain_takes_memory_linear_in_its_depth() {
    // (depth, diff peak, baseline peak): the smaller chain shows how the
    // cost grows with depth.
    let mut seen = Vec::new();
    for depth in [200, 400] {
        let tmp = tempfile::tempdir().unwrap();
        let host = tmp.path().join("root");
        std::fs::create_dir(&host).unwrap();
        let root = RootPath::open(&host).unwrap();
        let keep = IgnoreList::new();
        let mut walker = Walker::new(WalkOptions::default()).unwrap();
        walker.baseline(&root, &keep).unwrap();
        deep_chain(&host, depth);
        let (diff, peak) = peak_of(|| walker.diff(&root, &keep).unwrap());
        let longest = diff
            .changed
            .iter()
            .map(|c| c.path.as_os_str().len())
            .max()
            .unwrap_or(0);
        assert_eq!(diff.changed.len(), depth);
        drop(diff);
        let (_, rewalk) = peak_of(|| walker.baseline(&root, &keep).unwrap());
        println!(
            "depth {depth}: longest path {longest} bytes, diff peak {peak} bytes, \
             baseline peak {rewalk} bytes"
        );
        seen.push((depth, peak, rewalk));
    }
    let (_, peak, rewalk) = seen[1];
    assert!(
        peak < 4 * MIB && rewalk < 4 * MIB,
        "the walk of 400 directories held {peak} bytes (diff) and {rewalk} bytes \
         (baseline); by depth: {seen:?}"
    );
}
