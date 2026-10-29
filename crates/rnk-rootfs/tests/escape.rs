//! Escape suite for `RootPath` (threats T1–T3): every path must stay inside
//! the root or fail. Each test checks afterwards that nothing outside the
//! root changed.
//!
//! T0: unprivileged; everything lives below a fresh temp dir, removed on drop
//! even when a test fails. The magic-link test only reads `/proc/<pid>`.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use rnk_rootfs::{Error, FileType, Meta, OpenHow, RootPath};
use rustix::io::Errno;

/// `tmp/root` is the root; `tmp/outside` holds a sentinel no test may touch.
struct Fixture {
    _tmp: tempfile::TempDir,
    base: PathBuf,
    host: PathBuf,
    root: RootPath,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(tmp.path()).unwrap();
        let host = base.join("root");
        fs::create_dir_all(host.join("etc")).unwrap();
        fs::write(host.join("etc/passwd"), b"inside").unwrap();
        fs::create_dir(base.join("outside")).unwrap();
        fs::write(base.join("outside/sentinel"), b"outside").unwrap();
        let root = RootPath::open(&host).unwrap();
        Self {
            _tmp: tmp,
            base,
            host,
            root,
        }
    }

    fn host(&self, p: &str) -> PathBuf {
        self.host.join(p)
    }

    /// The absolute host path of the outside directory.
    fn outside(&self) -> PathBuf {
        self.base.join("outside")
    }

    /// Everything outside the root: the base dir without `root/`, recursively.
    fn snapshot_outside(&self) -> BTreeMap<PathBuf, (u32, u64, i64, i64, u64)> {
        let mut out = BTreeMap::new();
        let mut stack = vec![self.base.clone()];
        while let Some(dir) = stack.pop() {
            for e in fs::read_dir(&dir).unwrap() {
                let p = e.unwrap().path();
                if p == self.host {
                    continue;
                }
                let md = fs::symlink_metadata(&p).unwrap();
                if md.is_dir() {
                    stack.push(p.clone());
                }
                out.insert(
                    p,
                    (md.mode(), md.len(), md.mtime(), md.mtime_nsec(), md.nlink()),
                );
            }
        }
        out
    }

    fn read(&self, p: &str) -> Result<String, Error> {
        let mut s = String::new();
        fs::File::from(self.root.open_at(p, &OpenHow::read())?)
            .read_to_string(&mut s)
            .unwrap();
        Ok(s)
    }
}

fn errno<T: std::fmt::Debug>(r: Result<T, Error>) -> Errno {
    let e = r.unwrap_err();
    Errno::from_raw_os_error(
        e.raw_os_error()
            .unwrap_or_else(|| panic!("no errno in {e}")),
    )
}

#[test]
fn dotdot_chains_stay_inside() {
    let f = Fixture::new();
    let before = f.snapshot_outside();
    for p in [
        "../../../../etc/passwd",
        "/../../etc/passwd",
        "etc/../../../etc/passwd",
        "./../etc/../../etc/passwd",
    ] {
        assert_eq!(f.read(p).unwrap(), "inside", "{p}");
    }
    assert_eq!(
        errno(f.read("../outside/sentinel")),
        Errno::NOENT,
        "../outside must be clamped to the root"
    );
    assert_eq!(
        errno(
            f.root
                .open_at("../../outside/new", &OpenHow::write().create(0o644))
        ),
        Errno::NOENT
    );
    // Clamped at the root: creates root/x, never base/x.
    f.root.create_dir_all("../../../x", 0o755).unwrap();
    assert!(f.host("x").is_dir());
    f.root.symlink("../../../../outside", "up").unwrap();
    f.root.hard_link("etc/passwd", "../../hl").unwrap();
    assert!(f.host("hl").is_file());
    f.root.rename("hl", "../../moved").unwrap();
    assert!(f.host("moved").is_file());
    assert_eq!(errno(f.read("up/sentinel")), Errno::NOENT);
    assert_eq!(f.snapshot_outside(), before);
}

#[test]
fn absolute_symlinks_stay_inside() {
    let f = Fixture::new();
    let before = f.snapshot_outside();
    symlink("/etc", f.host("abs")).unwrap();
    symlink("/", f.host("slash")).unwrap();
    symlink(f.outside(), f.host("out")).unwrap();
    symlink(f.outside().join("sentinel"), f.host("sent")).unwrap();
    assert_eq!(f.read("abs/passwd").unwrap(), "inside");
    assert_eq!(f.read("slash/etc/passwd").unwrap(), "inside");
    assert_eq!(f.read("slash/../../etc/passwd").unwrap(), "inside");

    // Every operation through a symlink to the host path fails inside.
    assert_eq!(errno(f.read("out/sentinel")), Errno::NOENT);
    assert_eq!(errno(f.read("sent")), Errno::NOENT);
    let w = OpenHow::write().create(0o644);
    assert_eq!(errno(f.root.open_at("out/new", &w)), Errno::NOENT);
    // The final symlink is followed inside the root: the dangling target
    // is created inside, never outside.
    assert_eq!(
        errno(f.root.open_at("sent", &OpenHow::write())),
        Errno::NOENT
    );
    assert_eq!(errno(f.root.create_dir_all("out/new", 0o755)), Errno::NOENT);
    assert_eq!(errno(f.root.create_dir("out/new", 0o755)), Errno::NOENT);
    assert_eq!(errno(f.root.sub_root("out")), Errno::NOENT);
    assert_eq!(errno(f.root.statx("out/sentinel")), Errno::NOENT);
    assert_eq!(
        errno(f.root.set_meta(
            "out/sentinel",
            &Meta {
                mode: Some(0o777),
                ..Meta::default()
            }
        )),
        Errno::NOENT
    );
    assert_eq!(errno(f.root.hard_link("out/sentinel", "h")), Errno::NOENT);
    assert_eq!(errno(f.root.hard_link("etc/passwd", "out/h")), Errno::NOENT);
    assert_eq!(errno(f.root.rename("etc/passwd", "out/p")), Errno::NOENT);
    assert_eq!(errno(f.root.rename("out/sentinel", "s")), Errno::NOENT);
    assert_eq!(errno(f.root.remove_file("out/sentinel")), Errno::NOENT);
    assert_eq!(errno(f.root.symlink("x", "out/l")), Errno::NOENT);
    assert_eq!(errno(f.root.read_link("out/sentinel")), Errno::NOENT);

    // statx and set_meta never follow the final symlink.
    assert_eq!(f.root.statx("out").unwrap().file_type(), FileType::Symlink);
    let md = fs::symlink_metadata(f.host("out")).unwrap();
    f.root
        .set_meta(
            "out",
            &Meta {
                uid: Some(md.uid()),
                gid: Some(md.gid()),
                mode: Some(0o777),
                mtime: Some(rnk_rootfs::FileTime::from_secs(1)),
                ..Meta::default()
            },
        )
        .unwrap();
    // remove_file on the symlink removes the link only.
    f.root.remove_file("out").unwrap();
    assert_eq!(f.snapshot_outside(), before);
}

#[test]
fn symlink_chains_stay_inside() {
    let f = Fixture::new();
    let before = f.snapshot_outside();
    symlink("l2", f.host("l1")).unwrap();
    symlink("/l3", f.host("l2")).unwrap();
    symlink("../../../etc", f.host("l3")).unwrap();
    assert_eq!(f.read("l1/passwd").unwrap(), "inside");
    fs::create_dir(f.host("d")).unwrap();
    symlink("../../../../outside", f.host("d/up")).unwrap();
    symlink("d/up", f.host("chain")).unwrap();
    assert_eq!(errno(f.read("chain/sentinel")), Errno::NOENT);
    assert_eq!(errno(f.root.create_dir_all("chain/x", 0o755)), Errno::NOENT);
    assert_eq!(f.snapshot_outside(), before);
}

#[test]
fn symlink_loops_fail_with_eloop() {
    let f = Fixture::new();
    symlink("self", f.host("self")).unwrap();
    symlink("b", f.host("a")).unwrap();
    symlink("/a", f.host("b")).unwrap();
    // A chain longer than the kernel's 40 hops.
    for i in 0..50 {
        symlink(format!("c{}", i + 1), f.host(&format!("c{i}"))).unwrap();
    }
    fs::write(f.host("c50"), b"end").unwrap();
    assert_eq!(errno(f.read("self")), Errno::LOOP);
    assert_eq!(errno(f.read("a/x")), Errno::LOOP);
    assert_eq!(errno(f.read("c0")), Errno::LOOP);
    assert_eq!(f.read("c45").unwrap(), "end");
    let w = OpenHow::write().create(0o644);
    assert_eq!(errno(f.root.open_at("a/new", &w)), Errno::LOOP);
    assert_eq!(errno(f.root.create_dir_all("self/x", 0o755)), Errno::LOOP);
    assert_eq!(errno(f.root.sub_root("b")), Errno::LOOP);
    // The loop itself can still be inspected and removed: no follow.
    assert_eq!(f.root.statx("a").unwrap().file_type(), FileType::Symlink);
    assert_eq!(f.root.read_link("self").unwrap(), Path::new("self"));
    f.root.remove_file("self").unwrap();
}

#[test]
fn proc_symlinks_inside_a_root_stay_inside() {
    let f = Fixture::new();
    let before = f.snapshot_outside();
    let target = Path::new("/proc/self/root").join(f.outside().strip_prefix("/").unwrap());
    symlink(&target, f.host("viaproc")).unwrap();
    symlink("/proc/self/fd/0", f.host("fd0")).unwrap();
    symlink("../../../../proc/self/cwd", f.host("cwd")).unwrap();
    // The root has no /proc: the targets resolve inside and do not exist.
    assert_eq!(errno(f.read("viaproc/sentinel")), Errno::NOENT);
    assert_eq!(errno(f.read("fd0")), Errno::NOENT);
    assert_eq!(errno(f.read("cwd")), Errno::NOENT);
    let w = OpenHow::write().create(0o644);
    assert_eq!(errno(f.root.open_at("viaproc/new", &w)), Errno::NOENT);
    assert_eq!(f.snapshot_outside(), before);
}

/// With a real procfs below the root, magic links are refused
/// (`RESOLVE_NO_MAGICLINKS`): this is what a kaniko-mode root `/` in a
/// container looks like. Read-only: `/proc/<pid>` is only opened and stat'ed.
#[test]
fn magic_links_are_refused() {
    let f = Fixture::new();
    let outside = fs::File::open(f.outside()).unwrap();
    let proc = RootPath::open(format!("/proc/{}", std::process::id())).unwrap();
    for p in ["root", "root/etc/passwd", "cwd", "exe", "cwd/."] {
        assert_eq!(errno(proc.open_at(p, &OpenHow::read())), Errno::LOOP, "{p}");
    }
    let via_fd = format!("fd/{}/sentinel", outside.as_raw_fd());
    assert_eq!(errno(proc.open_at(&via_fd, &OpenHow::read())), Errno::LOOP);
    assert_eq!(errno(proc.sub_root("root")), Errno::LOOP);
    assert_eq!(
        errno(proc.open_at("root/tmp/x", &OpenHow::write().create(0o644))),
        Errno::LOOP
    );
    // Not following the last component is fine: the link itself.
    assert_eq!(proc.statx("root").unwrap().file_type(), FileType::Symlink);
    assert!(proc.open_at("root", &OpenHow::path().no_follow()).is_ok());
}

#[test]
fn hardlinks_out_fail() {
    let f = Fixture::new();
    let before = f.snapshot_outside();
    let abs = f.outside().join("sentinel");
    assert_eq!(errno(f.root.hard_link(&abs, "h1")), Errno::NOENT);
    assert_eq!(
        errno(f.root.hard_link("../../outside/sentinel", "h2")),
        Errno::NOENT
    );
    assert_eq!(f.snapshot_outside(), before);
    assert_eq!(fs::metadata(&abs).unwrap().nlink(), 1);
}

#[test]
fn sub_root_confines_to_itself() {
    let f = Fixture::new();
    fs::create_dir_all(f.host("stage/etc")).unwrap();
    fs::write(f.host("stage/etc/passwd"), b"stage").unwrap();
    symlink("/etc/passwd", f.host("stage/abs")).unwrap();
    let sub = f.root.sub_root("stage").unwrap();
    let mut s = String::new();
    fs::File::from(sub.open_at("../etc/passwd", &OpenHow::read()).unwrap())
        .read_to_string(&mut s)
        .unwrap();
    assert_eq!(s, "stage");
    s.clear();
    fs::File::from(sub.open_at("abs", &OpenHow::read()).unwrap())
        .read_to_string(&mut s)
        .unwrap();
    assert_eq!(s, "stage");
}

/// A directory swapped for a symlink between two calls: the second call
/// resolves the symlink again, inside the root.
#[test]
fn swap_between_calls_stays_inside() {
    let f = Fixture::new();
    let before = f.snapshot_outside();
    fs::create_dir(f.host("d")).unwrap();
    assert_eq!(f.root.statx("d").unwrap().file_type(), FileType::Directory);
    // The swap.
    fs::remove_dir(f.host("d")).unwrap();
    symlink(f.outside(), f.host("d")).unwrap();
    let w = OpenHow::write().create(0o644);
    assert_eq!(errno(f.root.open_at("d/f", &w)), Errno::NOENT);
    assert_eq!(errno(f.root.create_dir_all("d/x", 0o755)), Errno::NOENT);
    // `set_meta` on the swapped name changes the symlink, not its target.
    f.root
        .set_meta(
            "d",
            &Meta {
                mode: Some(0o700),
                mtime: Some(rnk_rootfs::FileTime::from_secs(1)),
                ..Meta::default()
            },
        )
        .unwrap();
    assert_eq!(f.snapshot_outside(), before);
}

/// A racing thread keeps exchanging `d` (a directory) with a symlink to the
/// outside directory while the main thread works through `d`. Whatever
/// interleaving happens, nothing outside may change.
#[test]
fn concurrent_swaps_stay_inside() {
    for target in ["abs", "rel", "proc"] {
        let f = Fixture::new();
        let before = f.snapshot_outside();
        fs::create_dir(f.host("d")).unwrap();
        fs::write(f.host("d/f"), b"x").unwrap();
        let link: PathBuf = match target {
            "abs" => f.outside(),
            "rel" => PathBuf::from("../../../outside"),
            _ => Path::new("/proc/self/root").join(f.outside().strip_prefix("/").unwrap()),
        };
        symlink(&link, f.host("swap")).unwrap();
        let host_dir = fs::File::open(&f.host).unwrap();
        let stop = AtomicBool::new(false);
        std::thread::scope(|s| {
            s.spawn(|| {
                while !stop.load(Ordering::Relaxed) {
                    rustix::fs::renameat_with(
                        &host_dir,
                        "d",
                        &host_dir,
                        "swap",
                        rustix::fs::RenameFlags::EXCHANGE,
                    )
                    .unwrap();
                }
            });
            let w = OpenHow::write().create(0o644);
            let m = Meta {
                mode: Some(0o777),
                mtime: Some(rnk_rootfs::FileTime::from_secs(1)),
                ..Meta::default()
            };
            for i in 0..2000 {
                // Errors are expected while `d` is the symlink: only the
                // effect outside matters.
                let _ = f.root.open_at(format!("d/n{i}"), &w);
                let _ = f.root.create_dir_all(format!("d/x{i}/y"), 0o755);
                let _ = f.root.set_meta("d/f", &m);
                let _ = f.root.set_meta("d", &m);
                let _ = f.root.symlink("/", format!("d/s{i}"));
                let _ = f.root.hard_link("etc/passwd", format!("d/h{i}"));
                let _ = f.root.rename(format!("d/n{i}"), format!("d/r{i}"));
                let _ = f.root.remove_file(format!("d/r{i}"));
                let _ = f.root.remove_file("d/sentinel");
                let _ = f.root.sub_root("d");
            }
            stop.store(true, Ordering::Relaxed);
        });
        assert_eq!(f.snapshot_outside(), before, "symlink kind {target}");
        assert_eq!(fs::read(f.outside().join("sentinel")).unwrap(), b"outside");
    }
}
