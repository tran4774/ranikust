use std::fs;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::time::{Duration, SystemTime};

use super::*;

struct Fixture {
    _tmp: tempfile::TempDir,
    host: PathBuf,
    root: RootPath,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let host = tmp.path().join("root");
        fs::create_dir(&host).unwrap();
        let root = RootPath::open(fs::canonicalize(&host).unwrap()).unwrap();
        Self {
            _tmp: tmp,
            host,
            root,
        }
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.host.join(rel)
    }

    fn write(&self, rel: &str, data: &[u8]) {
        fs::write(self.p(rel), data).unwrap();
    }

    fn mkdir(&self, rel: &str) {
        fs::create_dir_all(self.p(rel)).unwrap();
    }
}

fn walker(mode: SnapshotMode, workers: usize) -> Walker {
    Walker::new(WalkOptions {
        workers: NonZeroUsize::new(workers).unwrap(),
        mode,
    })
    .unwrap()
}

fn none() -> IgnoreList {
    IgnoreList::new()
}

fn paths(d: &WalkDiff) -> Vec<(String, Change)> {
    d.changed
        .iter()
        .map(|c| (c.path.display().to_string(), c.change))
        .collect()
}

fn set_mtime(p: &Path, t: SystemTime) {
    fs::File::options()
        .write(true)
        .open(p)
        .unwrap()
        .set_modified(t)
        .unwrap();
}

/// A tree of `dirs` directories of `files` files each, two levels deep.
fn tree(f: &Fixture, dirs: usize, files: usize) {
    for d in 0..dirs {
        for e in 0..files {
            let dir = format!("d{d}/sub{}", e % 3);
            f.mkdir(&dir);
            f.write(&format!("{dir}/f{e}"), format!("{d}-{e}").as_bytes());
        }
    }
}

#[test]
fn the_first_diff_adds_everything_in_path_order() {
    let f = Fixture::new();
    f.mkdir("a/b");
    f.write("a/b/c", b"x");
    f.write("a-b", b"y");
    f.write("z", b"z");
    let mut w = walker(SnapshotMode::Full, 4);
    let d = w.diff(&f.root, &none()).unwrap();
    assert!(d.root.is_some());
    assert_eq!(
        paths(&d),
        [
            ("a".to_owned(), Change::Added),
            ("a/b".to_owned(), Change::Added),
            ("a/b/c".to_owned(), Change::Added),
            ("a-b".to_owned(), Change::Added),
            ("z".to_owned(), Change::Added),
        ]
    );
    assert!(d.deleted.is_empty());
    assert_eq!(d.stats.entries, 5);
    assert_eq!(d.stats.files_hashed, 3);
    assert_eq!(w.entries(), 5);

    let again = w.diff(&f.root, &none()).unwrap();
    assert_eq!(again.root, None);
    assert!(again.changed.is_empty() && again.deleted.is_empty());
}

#[test]
fn baseline_keeps_the_walk_without_a_diff() {
    let f = Fixture::new();
    f.write("a", b"x");
    let mut w = walker(SnapshotMode::Full, 2);
    let stats = w.baseline(&f.root, &none()).unwrap();
    assert_eq!(stats.entries, 1);
    f.write("b", b"y");
    let d = w.diff(&f.root, &none()).unwrap();
    assert_eq!(paths(&d), [("b".to_owned(), Change::Added)]);
    // The root's mtime changed with the new entry.
    assert!(d.root.is_some());
    w.forget();
    assert_eq!(w.diff(&f.root, &none()).unwrap().changed.len(), 2);
}

#[test]
fn what_each_snapshot_mode_compares() {
    let old = SystemTime::now() - Duration::from_secs(3600);
    for (mode, content, chmod, touch) in [
        (SnapshotMode::Full, true, true, true),
        (SnapshotMode::Redo, false, true, true),
        (SnapshotMode::Time, false, false, true),
    ] {
        let f = Fixture::new();
        for n in ["content", "chmod", "touch"] {
            f.write(n, b"aaaa");
            set_mtime(&f.p(n), old);
        }
        let mut w = walker(mode, 2);
        w.baseline(&f.root, &none()).unwrap();
        // Same size, same mtime, other bytes: only content tells.
        f.write("content", b"bbbb");
        set_mtime(&f.p("content"), old);
        fs::set_permissions(f.p("chmod"), fs::Permissions::from_mode(0o600)).unwrap();
        set_mtime(&f.p("touch"), old + Duration::from_secs(1));
        let d = w.diff(&f.root, &none()).unwrap();
        let got: Vec<String> = d
            .changed
            .iter()
            .map(|c| c.path.display().to_string())
            .collect();
        let want: Vec<&str> = [("chmod", chmod), ("content", content), ("touch", touch)]
            .into_iter()
            .filter_map(|(n, on)| on.then_some(n))
            .collect();
        assert_eq!(got, want, "{mode:?}");
        assert!(d.changed.iter().all(|c| c.change == Change::Modified));
    }
}

#[test]
fn a_type_change_counts_in_every_mode() {
    let old = SystemTime::now() - Duration::from_secs(3600);
    let f = Fixture::new();
    f.write("x", b"a");
    set_mtime(&f.p("x"), old);
    let mut w = walker(SnapshotMode::Time, 1);
    w.baseline(&f.root, &none()).unwrap();
    fs::remove_file(f.p("x")).unwrap();
    f.mkdir("x");
    std::fs::File::open(f.p("x"))
        .unwrap()
        .set_modified(old)
        .unwrap();
    let d = w.diff(&f.root, &none()).unwrap();
    assert_eq!(paths(&d), [("x".to_owned(), Change::Added)]);
    assert!(d.deleted.is_empty());
}

#[test]
fn deletions_are_reported_at_the_topmost_path_only() {
    let f = Fixture::new();
    f.mkdir("gone/deep/er");
    f.write("gone/deep/er/f", b"x");
    f.write("gone/g", b"x");
    f.mkdir("keep");
    f.write("keep/lost", b"x");
    f.write("keep/kept", b"x");
    f.mkdir("replaced/inner");
    f.write("replaced/inner/f", b"x");
    let mut w = walker(SnapshotMode::Full, 3);
    w.baseline(&f.root, &none()).unwrap();
    fs::remove_dir_all(f.p("gone")).unwrap();
    fs::remove_file(f.p("keep/lost")).unwrap();
    fs::remove_dir_all(f.p("replaced")).unwrap();
    f.write("replaced", b"now a file");
    let d = w.diff(&f.root, &none()).unwrap();
    assert_eq!(
        d.deleted,
        [PathBuf::from("gone"), PathBuf::from("keep/lost")]
    );
    assert!(paths(&d).contains(&("replaced".to_owned(), Change::Added)));
}

#[test]
fn symlinks_are_hashed_by_target_and_never_followed() {
    let f = Fixture::new();
    let outside = f._tmp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("secret"), b"s").unwrap();
    symlink(&outside, f.p("out")).unwrap();
    symlink("a", f.p("rel")).unwrap();
    let mut w = walker(SnapshotMode::Full, 2);
    let d = w.diff(&f.root, &none()).unwrap();
    assert_eq!(d.stats.entries, 2, "nothing below the symlink is walked");
    assert!(d.changed.iter().all(|c| c.meta.kind == FileType::Symlink));
    assert_eq!(d.stats.files_hashed, 0);

    // Same length, other target, mtime put back: only the target tells.
    let mtime = fs::symlink_metadata(f.p("rel"))
        .unwrap()
        .modified()
        .unwrap();
    fs::remove_file(f.p("rel")).unwrap();
    symlink("b", f.p("rel")).unwrap();
    rustix::fs::utimensat(
        rustix::fs::CWD,
        f.p("rel"),
        &rustix::fs::Timestamps {
            last_access: rustix::fs::Timespec::default(),
            last_modification: to_timespec(mtime),
        },
        AtFlags::SYMLINK_NOFOLLOW,
    )
    .unwrap();
    let d = w.diff(&f.root, &none()).unwrap();
    assert_eq!(paths(&d), [("rel".to_owned(), Change::Modified)]);
}

fn to_timespec(t: SystemTime) -> rustix::fs::Timespec {
    let d = t.duration_since(UNIX_EPOCH).unwrap();
    rustix::fs::Timespec {
        tv_sec: i64::try_from(d.as_secs()).unwrap(),
        tv_nsec: i64::from(d.subsec_nanos()),
    }
}

#[test]
fn hard_links_share_device_and_inode() {
    let f = Fixture::new();
    f.write("a", b"x");
    fs::hard_link(f.p("a"), f.p("b")).unwrap();
    let mut w = walker(SnapshotMode::Full, 2);
    let d = w.diff(&f.root, &none()).unwrap();
    let [a, b] = &d.changed[..] else {
        panic!("{:?}", d.changed)
    };
    assert_eq!((a.meta.dev, a.meta.ino), (b.meta.dev, b.meta.ino));
    assert_eq!((a.meta.nlink, b.meta.nlink), (2, 2));
}

#[test]
fn ignored_paths_are_neither_walked_nor_deleted() {
    let f = Fixture::new();
    f.mkdir("state/blobs");
    f.write("state/blobs/x", b"x");
    f.mkdir("app");
    f.write("app/f", b"x");
    let mut w = walker(SnapshotMode::Full, 2);
    w.baseline(&f.root, &none()).unwrap();
    let mut ignore = IgnoreList::new();
    ignore.insert("/state").unwrap();
    ignore.insert("app/f").unwrap();
    f.write("state/new", b"y");
    let d = w.diff(&f.root, &ignore).unwrap();
    assert!(d.changed.is_empty(), "{:?}", d.changed);
    assert!(d.deleted.is_empty(), "{:?}", d.deleted);
    assert_eq!(d.stats.skipped, 2);
    // `state` and `app/f` are read, then skipped; `state/*` is not read.
    assert_eq!(d.stats.entries, 3);
}

#[test]
fn globs_and_below_only_entries_are_skipped() {
    let f = Fixture::new();
    f.mkdir("gpg/keys");
    f.mkdir("cache-a");
    f.mkdir("cache-b");
    f.mkdir("keep");
    let mut w = walker(SnapshotMode::Full, 2);
    w.baseline(&f.root, &none()).unwrap();
    let mut ignore = IgnoreList::new();
    ignore.insert_below("/gpg").unwrap();
    ignore.insert_glob("/cache-*").unwrap();
    f.write("gpg/keys/k", b"k");
    f.write("cache-a/x", b"x");
    f.write("cache-b/x", b"x");
    f.write("keep/x", b"x");
    let d = w.diff(&f.root, &ignore).unwrap();
    let changed: Vec<_> = paths(&d).into_iter().map(|(p, _)| p).collect();
    assert!(changed.iter().all(|p| p.starts_with("keep")), "{changed:?}");
    assert!(changed.iter().any(|p| p == "keep/x"), "{changed:?}");
    // `gpg` itself is walked; `gpg/keys`, `cache-a` and `cache-b` are skipped.
    assert_eq!(d.stats.skipped, 3);
}

#[test]
fn the_result_does_not_depend_on_the_worker_count() {
    let old = SystemTime::now() - Duration::from_secs(3600);
    let f = Fixture::new();
    tree(&f, 40, 30);
    let mut one = walker(SnapshotMode::Full, 1);
    let mut eight = walker(SnapshotMode::Full, 8);
    let (a, b) = (
        one.diff(&f.root, &none()).unwrap(),
        eight.diff(&f.root, &none()).unwrap(),
    );
    assert_eq!(a.changed, b.changed);
    assert_eq!(a.changed.len(), 40 + 40 * 3 + 40 * 30);
    for d in 0..40 {
        if d % 2 == 0 {
            fs::remove_dir_all(f.p(&format!("d{d}/sub1"))).unwrap();
        } else {
            f.write(&format!("d{d}/sub0/f0"), b"changed!");
            f.write(&format!("d{d}/new"), b"n");
        }
        set_mtime(&f.p(&format!("d{d}/sub2/f2")), old);
    }
    let (a, b) = (
        one.diff(&f.root, &none()).unwrap(),
        eight.diff(&f.root, &none()).unwrap(),
    );
    assert_eq!(a.changed, b.changed);
    assert_eq!(a.deleted, b.deleted);
    assert_eq!(a.root, b.root);
    assert_eq!(a.deleted.len(), 20);
    assert!(b.stats.max_threads >= 1 && a.stats.max_threads == 1);
}

#[test]
fn the_stat_cache_skips_unchanged_files_outside_the_racy_window() {
    let f = Fixture::new();
    tree(&f, 3, 5);
    let mut cached = walker(SnapshotMode::Full, 2).with_racy_window(0);
    cached.baseline(&f.root, &none()).unwrap();
    let d = cached.diff(&f.root, &none()).unwrap();
    assert_eq!((d.stats.stat_cache_hits, d.stats.files_hashed), (15, 0));

    // Rewritten with the same size and mtime: ctime moves, so it is read.
    let mtime = fs::metadata(f.p("d0/sub0/f0")).unwrap().modified().unwrap();
    f.write("d0/sub0/f0", b"X-0");
    set_mtime(&f.p("d0/sub0/f0"), mtime);
    let d = cached.diff(&f.root, &none()).unwrap();
    assert_eq!((d.stats.stat_cache_hits, d.stats.files_hashed), (14, 1));
    assert_eq!(paths(&d), [("d0/sub0/f0".to_owned(), Change::Modified)]);

    // Files changed within the window are never taken from the cache.
    let mut racy = walker(SnapshotMode::Full, 2);
    racy.baseline(&f.root, &none()).unwrap();
    let d = racy.diff(&f.root, &none()).unwrap();
    assert_eq!((d.stats.stat_cache_hits, d.stats.files_hashed), (0, 15));
}

#[test]
fn xattrs_count_except_overlays() {
    let f = Fixture::new();
    f.write("x", b"a");
    let set = |name: &str, value: &[u8]| {
        rustix::fs::setxattr(f.p("x"), name, value, rustix::fs::XattrFlags::empty())
    };
    if let Err(e) = set("user.a", b"1") {
        eprintln!("skipped: no user xattrs here ({e})");
        return;
    }
    let mut w = walker(SnapshotMode::Full, 1);
    w.baseline(&f.root, &none()).unwrap();
    // Keep ctime aside: the cache is off (racy window), so content is read.
    set("user.overlay.opaque", b"y").unwrap();
    let d = w.diff(&f.root, &none()).unwrap();
    assert!(d.changed.is_empty(), "{:?}", d.changed);
    set("user.a", b"2").unwrap();
    let d = w.diff(&f.root, &none()).unwrap();
    assert_eq!(paths(&d), [("x".to_owned(), Change::Modified)]);
}

/// A tree a `RUN` made past `PATH_MAX` fails the walk with the exporter's
/// refusal (T15): each directory and change would hold its full path. The
/// directory the error names is within the limit.
#[test]
fn a_tree_deeper_than_path_max_is_refused() {
    let f = Fixture::new();
    let name = "d".repeat(200);
    let mut dir =
        rustix::fs::open(&f.host, OFlags::DIRECTORY | OFlags::RDONLY, Mode::empty()).unwrap();
    for _ in 0..30 {
        rustix::fs::mkdirat(&dir, name.as_str(), Mode::from_raw_mode(0o755)).unwrap();
        dir = rustix::fs::openat(
            &dir,
            name.as_str(),
            OFlags::DIRECTORY | OFlags::RDONLY,
            Mode::empty(),
        )
        .unwrap();
    }
    let mut w = walker(SnapshotMode::Full, 4);
    let e = w.diff(&f.root, &none()).unwrap_err();
    match &e {
        Error::Export { path, reason } => {
            assert!(reason.contains("PATH_MAX"), "{e}");
            assert!(path.as_os_str().len() <= 4095, "{}", path.as_os_str().len());
        }
        other => panic!("{other}"),
    }
}

/// `diff_below` walks only the paths asked for and the directories that
/// lead to them.
#[test]
fn a_diff_below_some_paths_walks_only_them() {
    let f = Fixture::new();
    fs::create_dir_all(f.host.join("app/dist")).unwrap();
    fs::create_dir_all(f.host.join("other")).unwrap();
    f.write("app/dist/main.js", b"js");
    f.write("app/src.ts", b"ts");
    f.write("other/x", b"x");
    let mut w = walker(SnapshotMode::Full, 2);
    let d = w
        .diff_below(&f.root, &none(), &[PathBuf::from("app/dist")])
        .unwrap();
    assert_eq!(
        paths(&d),
        [
            ("app".to_owned(), Change::Added),
            ("app/dist".to_owned(), Change::Added),
            ("app/dist/main.js".to_owned(), Change::Added),
        ]
    );
}

#[test]
fn the_tree_finds_children_by_name() {
    let f = Fixture::new();
    for n in ["b", "a", "c", "ab"] {
        f.write(n, b"");
    }
    let mut w = walker(SnapshotMode::Full, 2);
    w.baseline(&f.root, &none()).unwrap();
    let t = w.prev.as_ref().unwrap();
    for n in ["a", "ab", "b", "c"] {
        let id = t.child(ROOT, n.as_bytes()).unwrap();
        assert_eq!(t.path(id), PathBuf::from(n));
    }
    assert_eq!(t.child(ROOT, b"d"), None);
}

/// `statx` of `name` in `dir`, and `dir` opened.
fn stat_in(dir: &Path, name: &str) -> (OwnedFd, Statx) {
    let fd = rustix::fs::open(dir, OFlags::DIRECTORY | OFlags::RDONLY, Mode::empty()).unwrap();
    let st = statx_at(fd.as_fd(), name, AtFlags::SYMLINK_NOFOLLOW, Path::new(name)).unwrap();
    (fd, st)
}

fn is_changed<T: std::fmt::Debug>(r: Result<T>) -> bool {
    matches!(r, Err(Error::Changed { .. }))
}

#[test]
fn a_fifo_in_the_tree_is_listed_and_never_opened() {
    let f = Fixture::new();
    f.write("file", b"x");
    rustix::fs::mknodat(
        rustix::fs::CWD,
        f.p("fifo"),
        rustix::fs::FileType::Fifo,
        Mode::from_raw_mode(0o644),
        0,
    )
    .unwrap();
    // A FIFO without a writer blocks a reader's `open`: the walk returning
    // at all shows it was not opened.
    let mut w = walker(SnapshotMode::Full, 2);
    let d = w.diff(&f.root, &none()).unwrap();
    assert_eq!(d.stats.files_hashed, 1);
    let fifo = d
        .changed
        .iter()
        .find(|c| c.path == Path::new("fifo"))
        .unwrap();
    assert_eq!(fifo.meta.kind, FileType::Fifo);
}

#[test]
fn a_fifo_swapped_in_for_a_regular_file_is_never_opened() {
    let f = Fixture::new();
    f.write("x", b"data");
    let (dir, st) = stat_in(&f.host, "x");
    fs::remove_file(f.p("x")).unwrap();
    rustix::fs::mknodat(
        rustix::fs::CWD,
        f.p("x"),
        rustix::fs::FileType::Fifo,
        Mode::from_raw_mode(0o644),
        0,
    )
    .unwrap();
    let proc = proc_fd_dir().unwrap();
    let r = open_regular(proc.as_fd(), dir.as_fd(), b"x", &st, Path::new("x"));
    assert!(is_changed(r), "a FIFO must be refused on the pinned fd");
}

#[test]
fn a_file_gone_since_it_was_listed_fails_the_walk() {
    let f = Fixture::new();
    f.write("x", b"data");
    let (dir, st) = stat_in(&f.host, "x");
    fs::remove_file(f.p("x")).unwrap();
    let proc = proc_fd_dir().unwrap();
    let r = open_regular(proc.as_fd(), dir.as_fd(), b"x", &st, Path::new("x"));
    assert!(is_changed(r), "gone is a change of the root, as in kaniko");
}

#[test]
fn a_regular_file_swapped_for_another_is_refused() {
    let f = Fixture::new();
    f.write("x", b"data");
    let (dir, st) = stat_in(&f.host, "x");
    f.write("y", b"other");
    fs::rename(f.p("y"), f.p("x")).unwrap();
    let proc = proc_fd_dir().unwrap();
    let r = open_regular(proc.as_fd(), dir.as_fd(), b"x", &st, Path::new("x"));
    assert!(is_changed(r));
    // Unchanged, it opens and reads.
    let (dir, st) = stat_in(&f.host, "x");
    let fd = open_regular(proc.as_fd(), dir.as_fd(), b"x", &st, Path::new("x")).unwrap();
    assert_eq!(hash_content(fd.as_fd(), 5, Path::new("x")).unwrap().1, 5);
}

#[test]
#[ignore = "T2c: default: mknod of /dev/zero's node needs CAP_MKNOD and a TMPDIR without nodev"]
fn a_device_node_swapped_in_for_a_regular_file_is_never_opened() {
    let f = Fixture::new();
    f.write("x", b"data");
    let (dir, st) = stat_in(&f.host, "x");
    fs::remove_file(f.p("x")).unwrap();
    // 1:5 is /dev/zero: read, it never ends.
    rustix::fs::mknodat(
        rustix::fs::CWD,
        f.p("x"),
        rustix::fs::FileType::CharacterDevice,
        Mode::from_raw_mode(0o644),
        rustix::fs::makedev(1, 5),
    )
    .unwrap();
    let proc = proc_fd_dir().unwrap();
    let r = open_regular(proc.as_fd(), dir.as_fd(), b"x", &st, Path::new("x"));
    assert!(is_changed(r));
    let mut w = walker(SnapshotMode::Full, 2);
    let d = w.diff(&f.root, &none()).unwrap();
    assert_eq!(d.stats.files_hashed, 0);
}

#[test]
fn hashing_reads_no_more_than_the_size_statx_gave() {
    let f = Fixture::new();
    f.write("x", b"12345");
    let open = || rustix::fs::open(f.p("x"), OFlags::RDONLY, Mode::empty()).unwrap();
    assert!(hash_content(open().as_fd(), 5, Path::new("x")).is_ok());
    assert!(is_changed(hash_content(open().as_fd(), 4, Path::new("x"))));
    assert!(is_changed(hash_content(open().as_fd(), 6, Path::new("x"))));
    // A file that never ends stops after size + 1 bytes.
    let zero = rustix::fs::open("/dev/zero", OFlags::RDONLY, Mode::empty()).unwrap();
    assert!(is_changed(hash_content(
        zero.as_fd(),
        10,
        Path::new("zero")
    )));
}

#[test]
fn a_wide_directory_is_walked() {
    let f = Fixture::new();
    for i in 0..3000 {
        f.mkdir(&format!("w/{i}"));
    }
    let mut w = walker(SnapshotMode::Full, 1);
    let d = w.diff(&f.root, &none()).unwrap();
    assert_eq!(d.changed.len(), 3001);
}

#[test]
fn arena_offsets_beyond_u32_are_errors() {
    assert!(matches!(
        super::super::tree::len_u32_for_tests(usize::MAX),
        Err(Error::WalkTooLarge)
    ));
}

/// Q-420's measurement: what `--single-snapshot` would save in kaniko mode;
/// kept as the record of ADR-0065, which refused the flag for good.
/// A light synthetic workload, not W1–W3 (those run only as the ultra
/// benchmark): a base of `RANIKUST_BENCH_FILES` small files (default
/// 80 000) in 400 directories, then 10 steps that each rewrite 200 files and
/// add 200. Per-step snapshots walk after every step; a single snapshot
/// walks once at the end. The tree sits in `RANIKUST_BENCH_DIR` (a disk
/// directory; default a temp dir). Prints the medians of 3 runs; run with
/// `cargo test -p rnk-snapshot --release --lib single_snapshot_poc --
/// --ignored --nocapture`.
#[test]
#[ignore = "bench (Q-420): writes ~100k files and takes tens of seconds"]
fn single_snapshot_poc() {
    const DIRS: usize = 400;
    const STEPS: usize = 10;
    const PER_STEP: usize = 200;
    let files: usize = std::env::var("RANIKUST_BENCH_FILES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(80_000);
    let parent = std::env::var_os("RANIKUST_BENCH_DIR").map(PathBuf::from);
    let make = || {
        let tmp = match &parent {
            Some(p) => tempfile::tempdir_in(p).unwrap(),
            None => tempfile::tempdir().unwrap(),
        };
        let host = tmp.path().join("root");
        for d in 0..DIRS {
            fs::create_dir_all(host.join(format!("d{d}"))).unwrap();
        }
        for i in 0..files {
            fs::write(host.join(format!("d{}/f{i}", i % DIRS)), [b'x'; 64]).unwrap();
        }
        let root = RootPath::open(fs::canonicalize(&host).unwrap()).unwrap();
        (tmp, host, root)
    };
    let step = |host: &Path, s: usize| {
        for k in 0..PER_STEP {
            let i = (s * PER_STEP + k) * 7 % files;
            fs::write(
                host.join(format!("d{}/f{i}", i % DIRS)),
                format!("step {s} {k}"),
            )
            .unwrap();
            fs::write(host.join(format!("d{}/new-{s}-{k}", k % DIRS)), [b'y'; 64]).unwrap();
        }
    };
    let median = |mut v: Vec<f64>| {
        v.sort_by(f64::total_cmp);
        v[v.len() / 2]
    };
    let (mut per_step, mut single, mut base) = (Vec::new(), Vec::new(), Vec::new());
    let (mut changed_per_step, mut changed_single) = (0, 0);
    for _ in 0..3 {
        let (_a, host, root) = make();
        let mut w = walker(SnapshotMode::Full, 8);
        let t = std::time::Instant::now();
        w.baseline(&root, &none()).unwrap();
        base.push(t.elapsed().as_secs_f64());
        let mut total = 0.0;
        changed_per_step = 0;
        for s in 0..STEPS {
            step(&host, s);
            let t = std::time::Instant::now();
            changed_per_step += w.diff(&root, &none()).unwrap().changed.len();
            total += t.elapsed().as_secs_f64();
        }
        per_step.push(total);
        let (_b, host, root) = make();
        let mut w = walker(SnapshotMode::Full, 8);
        w.baseline(&root, &none()).unwrap();
        for s in 0..STEPS {
            step(&host, s);
        }
        let t = std::time::Instant::now();
        changed_single = w.diff(&root, &none()).unwrap().changed.len();
        single.push(t.elapsed().as_secs_f64());
    }
    let (b, p, s) = (median(base), median(per_step), median(single));
    eprintln!(
        "single_snapshot_poc: files={files} steps={STEPS} baseline={b:.3}s \
         per-step walks total={p:.3}s ({:.3}s each, {changed_per_step} changes) \
         single walk={s:.3}s ({changed_single} changes) saved={:.3}s",
        p / STEPS as f64,
        p - s
    );
    assert!(changed_single > 0 && changed_per_step >= changed_single);
}
