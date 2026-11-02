//! Red team (T15): the heap the kaniko-mode walk takes for a deep tree a
//! `RUN` made, measured by a counting allocator (T0).
//!
//! A `RUN` makes a chain of directories as deep as it likes by `mkdir`
//! and `cd`, far past `PATH_MAX`; `rnk-rootfs`'s deletion and pruning hold
//! one name per level for it (`crates/rnk-rootfs/tests/memory.rs`). The
//! walk that diffs the root after the `RUN` holds each directory's and each
//! change's full path, so it refuses an entry past `PATH_MAX`, as the
//! overlay exporter does: what it holds stays bounded by what the tree
//! holds, never the sum of every directory's path, which grows with the
//! square of the depth. Such a tree could not be exported anyway.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use rnk_rootfs::{IgnoreList, RootPath};
use rnk_snapshot::{Error, WalkOptions, Walker};

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

/// A chain of `depth` directories with names of `len` bytes below `host`,
/// made by descriptor as a `RUN` makes one with `mkdir` and `cd`, so its
/// path may be far past `PATH_MAX`.
fn deep_chain(host: &std::path::Path, depth: usize, len: usize) {
    use rustix::fs::{Mode, OFlags};
    let name = "n".repeat(len);
    let dir = OFlags::PATH | OFlags::DIRECTORY;
    let mut cur = rustix::fs::open(host, dir, Mode::empty()).unwrap();
    for _ in 0..depth {
        rustix::fs::mkdirat(&cur, name.as_str(), Mode::from_raw_mode(0o755)).unwrap();
        cur = rustix::fs::openat(&cur, name.as_str(), dir, Mode::empty()).unwrap();
    }
}

/// Whether `e` is the walk's refusal of an entry past `PATH_MAX`.
fn past_path_max(e: &Error) -> bool {
    matches!(e, Error::Export { reason, .. } if reason.contains("PATH_MAX"))
}

/// Red team (T15): a `RUN` that leaves a chain of 200 or 400 directories
/// with 200-byte names (40 or 80 KB of path at the bottom) makes the
/// kaniko-mode walk that diffs the root, and the walk that takes the next
/// baseline, fail cleanly with the `PATH_MAX` refusal while holding under
/// 4 MiB. Before the refusal the diff of the 400-deep chain held 33 MB and
/// the baseline 17 MB, growing with the square of the depth.
#[test]
fn red_team_walking_a_deep_chain_takes_memory_linear_in_its_depth() {
    let mut seen = Vec::new();
    for depth in [200, 400] {
        let tmp = tempfile::tempdir().unwrap();
        let host = tmp.path().join("root");
        std::fs::create_dir(&host).unwrap();
        let root = RootPath::open(&host).unwrap();
        let keep = IgnoreList::new();
        let mut walker = Walker::new(WalkOptions::default()).unwrap();
        walker.baseline(&root, &keep).unwrap();
        deep_chain(&host, depth, 200);
        let (diff, peak) = peak_of(|| walker.diff(&root, &keep));
        let e = diff.expect_err("a chain past PATH_MAX is refused");
        assert!(past_path_max(&e), "{e}");
        assert!(e.to_string().len() <= 4096 + 128, "the error stays short");
        let (rewalk, again) = peak_of(|| walker.baseline(&root, &keep));
        let e = rewalk.expect_err("the baseline refuses it too");
        assert!(past_path_max(&e), "{e}");
        println!("depth {depth}: diff peak {peak} bytes, baseline peak {again} bytes");
        seen.push((depth, peak, again));
    }
    for &(_, peak, again) in &seen {
        assert!(
            peak < 4 * MIB && again < 4 * MIB,
            "a walk refused past PATH_MAX held too much; by depth: {seen:?}"
        );
    }
}

/// The refusal is only past `PATH_MAX`: a chain whose deepest path stays
/// within it (19 levels of 200-byte names, 3,819 bytes) is walked and
/// reported entry by entry, as before.
#[test]
fn a_chain_within_path_max_is_still_walked() {
    let tmp = tempfile::tempdir().unwrap();
    let host = tmp.path().join("root");
    std::fs::create_dir(&host).unwrap();
    let root = RootPath::open(&host).unwrap();
    let keep = IgnoreList::new();
    let mut walker = Walker::new(WalkOptions::default()).unwrap();
    walker.baseline(&root, &keep).unwrap();
    deep_chain(&host, 19, 200);
    let diff = walker.diff(&root, &keep).unwrap();
    assert_eq!(diff.changed.len(), 19);
    let longest = diff
        .changed
        .iter()
        .map(|c| c.path.as_os_str().len())
        .max()
        .unwrap_or(0);
    assert_eq!(longest, 19 * 200 + 18);
    walker.baseline(&root, &keep).unwrap();
}
