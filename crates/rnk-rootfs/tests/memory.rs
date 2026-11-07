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

use rnk_rootfs::{
    DeviceNodes, IgnoreList, RootPath, Whiteouts, delete_contents, extract, extract_live,
    extract_overlay, prune,
};

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

/// Device nodes a user namespace leaves out take no room on disk, so what
/// extraction keeps of their names, and of hardlinks to them, is bounded on
/// its own: long names, thousands of each, cost a bounded heap.
#[test]
fn devices_left_out_cost_a_bounded_heap_whatever_their_names() {
    let _one = one_at_a_time();
    let (_tmp, _host, root) = root();
    let mut b = tar::Builder::new(Vec::new());
    // Names of about 4000 bytes the kernel would take, as a hardlink's name
    // must be (#179); a left-out device's name never reaches it.
    let long = format!(
        "{}{}",
        format!("{}/", "n".repeat(250)).repeat(15),
        "n".repeat(200)
    );
    for i in 0..4000 {
        let name = if i == 0 {
            "dev".to_owned()
        } else {
            format!("{long}{i}")
        };
        let mut h = header(tar::EntryType::Char);
        h.set_device_major(1).unwrap();
        h.set_device_minor(3).unwrap();
        b.append_data(&mut h, &name, &b""[..]).unwrap();
    }
    for i in 0..4000 {
        let name = format!("{long}-link{i}");
        b.append_link(&mut header(tar::EntryType::Link), &name, "dev")
            .unwrap();
    }
    let tar = b.into_inner().unwrap();
    let peak = peak_of(|| {
        extract_overlay(&root, tar.as_slice(), &[], DeviceNodes::Skip).unwrap();
    });
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

/// A chain of `depth` directories with 200-byte names below `host`, made by
/// descriptor: its path is far past `PATH_MAX`, as a `RUN` can make one.
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

/// Deleting a deep chain holds one name per level, not the path to it:
/// memory linear in depth.
#[test]
fn deleting_a_deep_chain_takes_memory_linear_in_its_depth() {
    let _one = one_at_a_time();
    let (_tmp, host, root) = root();
    deep_chain(&host, 400);
    let keep = IgnoreList::new();
    let peak = peak_of(|| delete_contents(&root, &keep).unwrap());
    eprintln!("peak: {peak} bytes");
    assert!(peak < 4 * MIB, "deletion took {peak} bytes");
    assert!(root.read_names("").unwrap().is_empty());
}

/// Copying a deep chain (a per-stage base copied from its parent stage,
/// `COPY --from`, a kept stage copied out) holds one name per level and
/// one path buffer, not every level's full path: memory linear in depth.
/// Before, each frame kept its source and destination paths, 33 MB at
/// this depth (T15).
#[test]
fn copying_a_deep_chain_takes_memory_linear_in_its_depth() {
    let _one = one_at_a_time();
    let (_tmp, host, src) = root();
    deep_chain(&host, 400);
    let dst_tmp = tempfile::tempdir().unwrap();
    let dst = RootPath::open(dst_tmp.path()).unwrap();
    let opts = rnk_rootfs::CopyOpts::default();
    let peak = peak_of(|| rnk_rootfs::copy(&src, "", &dst, "", &opts).unwrap());
    eprintln!("peak: {peak} bytes");
    assert!(peak < 4 * MIB, "copying took {peak} bytes");
    let mut cur = PathBuf::new();
    for _ in 0..3 {
        cur.push("n".repeat(200));
        assert_eq!(dst.read_names(&cur).unwrap().len(), 1);
    }
}

/// A file with other names at the bottom of a chain past `PATH_MAX` is
/// refused by the copy, before anything of it is written: its path would
/// otherwise be remembered whole for linking its other names, so many such
/// files under a deep enough chain would hold gigabytes (T15). The error
/// names only the entry.
#[test]
fn a_hardlinked_file_past_path_max_is_refused_by_the_copy() {
    use rustix::fs::{AtFlags, Mode, OFlags};
    let _one = one_at_a_time();
    let (_tmp, host, src) = root();
    deep_chain(&host, 30);
    let mut cur = rustix::fs::open(&host, OFlags::PATH | OFlags::DIRECTORY, Mode::empty()).unwrap();
    let name = "n".repeat(200);
    for _ in 0..30 {
        cur = rustix::fs::openat(
            &cur,
            name.as_str(),
            OFlags::PATH | OFlags::DIRECTORY,
            Mode::empty(),
        )
        .unwrap();
    }
    let f = rustix::fs::openat(
        &cur,
        "a",
        OFlags::WRONLY | OFlags::CREATE,
        Mode::from_raw_mode(0o644),
    )
    .unwrap();
    drop(f);
    rustix::fs::linkat(&cur, "a", &cur, "b", AtFlags::empty()).unwrap();
    let dst_tmp = tempfile::tempdir().unwrap();
    let dst = RootPath::open(dst_tmp.path()).unwrap();
    let opts = rnk_rootfs::CopyOpts::default();
    let e = rnk_rootfs::copy(&src, "", &dst, "", &opts).unwrap_err();
    let text = e.to_string();
    assert!(text.contains("PATH_MAX"), "{text}");
    assert!(text.len() < 200, "the error names only the entry: {text}");
}

/// So does pruning, which walks every directory it keeps.
#[test]
fn pruning_a_deep_chain_takes_memory_linear_in_its_depth() {
    let _one = one_at_a_time();
    let (_tmp, host, root) = root();
    deep_chain(&host, 400);
    let peak = peak_of(|| {
        prune(&root, &|_, _| false).unwrap();
    });
    eprintln!("peak: {peak} bytes");
    assert!(peak < 4 * MIB, "pruning took {peak} bytes");
}

/// Empties a root on drop, when a test failed before it did: the temp dir's
/// own removal recurses with a descriptor and a stack frame per level.
struct Emptied<'a>(&'a RootPath);

impl Drop for Emptied<'_> {
    fn drop(&mut self) {
        // A drop cannot fail the test; a tree left behind is at least shown.
        if let Err(e) = delete_contents(self.0, &IgnoreList::new()) {
            eprintln!("left behind: {e}");
        }
    }
}

/// Pruning a chain of 100,000 levels of its files, then removing it whole,
/// takes heap linear in its depth: a few hundred bytes a level, where a path
/// a level would take gigabytes.
#[test]
fn pruning_a_chain_of_100k_levels_takes_memory_linear_in_its_depth() {
    use rnk_rootfs::FileType;
    use rustix::fs::{Mode, OFlags};
    const DEPTH: usize = 100_000;
    let _one = one_at_a_time();
    let (_tmp, host, root) = root();
    let _emptied = Emptied(&root);
    let dir = OFlags::PATH | OFlags::DIRECTORY;
    let mut cur = rustix::fs::open(&host, dir, Mode::empty()).unwrap();
    rustix::fs::mkdirat(&cur, "deep", Mode::from_raw_mode(0o755)).unwrap();
    cur = rustix::fs::openat(&cur, "deep", dir, Mode::empty()).unwrap();
    for _ in 0..DEPTH {
        let f = rustix::fs::openat(
            &cur,
            "f",
            OFlags::WRONLY | OFlags::CREATE,
            Mode::from_raw_mode(0o644),
        );
        drop(f.unwrap());
        rustix::fs::mkdirat(&cur, "d", Mode::from_raw_mode(0o755)).unwrap();
        cur = rustix::fs::openat(&cur, "d", dir, Mode::empty()).unwrap();
    }
    drop(cur);
    let files = peak_of(|| {
        prune(&root, &|_, t| t == FileType::Regular).unwrap();
    });
    let whole = peak_of(|| {
        prune(&root, &|p, _| p == std::path::Path::new("deep")).unwrap();
    });
    eprintln!("peak: {files} bytes pruning files, {whole} bytes removing whole");
    assert!(root.read_names("").unwrap().is_empty());
    for (what, peak) in [("pruning files", files), ("removing whole", whole)] {
        assert!(peak < 64 * MIB, "{what} took {peak} bytes");
    }
}
