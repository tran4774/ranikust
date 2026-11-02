//! Heap bounds of extraction and deletion on hostile trees (T15).
//!
//! A test binary of its own: a counting global allocator measures the peak
//! heap each call adds, and the tests take one lock, so only one measures at
//! a time. The bounds are far above the linear cost and far below what a
//! cost growing with the square of depth, or with every name ever
//! created, would take.

use std::alloc::{GlobalAlloc, Layout, System};
use std::fs;
use std::os::unix::fs::symlink;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use rnk_rootfs::{IgnoreList, RootPath, Whiteouts, extract, extract_live};

/// The system allocator, counting the bytes allocated now and their peak.
struct Counting;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grew(by: usize) {
    let now = CURRENT.fetch_add(by, Ordering::Relaxed) + by;
    PEAK.fetch_max(now, Ordering::Relaxed);
}

// SAFETY: every call goes to `System` with the caller's arguments; the
// counters only observe sizes.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's contract for `alloc`, passed on.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            grew(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from this allocator, that is from `System`.
        unsafe { System.dealloc(ptr, layout) };
        CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the caller's contract for `realloc`, passed on.
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

static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

/// The measuring lock; a test that failed while holding it does not fail
/// the others.
fn one_at_a_time() -> MutexGuard<'static, ()> {
    ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The peak heap `f` adds above what was allocated when it started.
fn peak_of(f: impl FnOnce()) -> usize {
    let base = CURRENT.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    f();
    PEAK.load(Ordering::Relaxed).saturating_sub(base)
}

const MIB: usize = 1 << 20;

/// A fresh root below a temp dir, removed on drop.
fn root() -> (tempfile::TempDir, PathBuf, RootPath) {
    let tmp = tempfile::tempdir().unwrap();
    let host = fs::canonicalize(tmp.path()).unwrap();
    let root = RootPath::open(&host).unwrap();
    (tmp, host, root)
}

fn header(t: tar::EntryType) -> tar::Header {
    let mut h = tar::Header::new_gnu();
    h.set_entry_type(t);
    h.set_mode(if t == tar::EntryType::Directory {
        0o755
    } else {
        0o644
    });
    h.set_uid(rustix::process::getuid().as_raw().into());
    h.set_gid(rustix::process::getgid().as_raw().into());
    h.set_mtime(0);
    h.set_size(0);
    h
}

/// A layer that creates a deep tree and deletes it again, many times, keeps
/// nothing of it: disk use stays flat, and so must memory.
#[test]
fn extraction_forgets_what_the_same_layer_deleted() {
    let _one = one_at_a_time();
    let (_tmp, _host, root) = root();
    let mut b = tar::Builder::new(Vec::new());
    let deep = vec!["x"; 40].join("/");
    for i in 0..2000 {
        // Its 40 missing parents are created for it, then the whiteout
        // removes the whole tree.
        let name = format!("d{i}/{deep}/f");
        b.append_data(&mut header(tar::EntryType::Regular), &name, &b""[..])
            .unwrap();
        let wh = format!(".wh.d{i}");
        b.append_data(&mut header(tar::EntryType::Regular), &wh, &b""[..])
            .unwrap();
    }
    let tar = b.into_inner().unwrap();
    let peak = peak_of(|| extract(&root, tar.as_slice(), Whiteouts::Apply).unwrap());
    eprintln!("peak: {peak} bytes");
    assert!(peak < 2 * MIB, "extraction took {peak} bytes");
    assert_eq!(
        root.read_names("").unwrap(),
        Vec::<std::ffi::OsString>::new()
    );
}

/// Entries reached through a symlink to a deep directory land at its long
/// physical path; each must cost its own name, not that path.
#[test]
fn a_live_extraction_keeps_each_created_name_once() {
    let _one = one_at_a_time();
    let (_tmp, host, root) = root();
    let chain: PathBuf = (0..30).map(|i| format!("{i:0>100}")).collect();
    fs::create_dir_all(host.join(&chain)).unwrap();
    symlink(&chain, host.join("s")).unwrap();
    let mut b = tar::Builder::new(Vec::new());
    for i in 0..2000 {
        let name = format!("s/f{i}");
        b.append_data(&mut header(tar::EntryType::Regular), &name, &b""[..])
            .unwrap();
    }
    let tar = b.into_inner().unwrap();
    let keep = IgnoreList::new();
    let peak = peak_of(|| extract_live(&root, tar.as_slice(), &keep).unwrap());
    eprintln!("peak: {peak} bytes");
    assert!(peak < 2 * MIB, "extraction took {peak} bytes");
    assert_eq!(root.read_names(&chain).unwrap().len(), 2000);
}
