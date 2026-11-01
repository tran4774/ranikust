use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::ffi::OsStringExt as _;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt, symlink};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use rustix::fs::{CWD, Mode};

use super::*;
use crate::test_util::{Fixture, errno, snapshot, tree};

#[test]
fn deletes_everything_but_the_root() {
    let f = Fixture::new();
    let (out, before) = f.outside();
    fs::create_dir_all(f.host("a/b/c")).unwrap();
    fs::create_dir(f.host("empty")).unwrap();
    fs::write(f.host("a/b/c/f"), b"1").unwrap();
    fs::write(f.host("a/f"), b"2").unwrap();
    fs::set_permissions(f.host("a/f"), fs::Permissions::from_mode(0o000)).unwrap();
    fs::hard_link(f.host("a/f"), f.host("hl")).unwrap();
    symlink(&out, f.host("to-outside")).unwrap();
    symlink("/", f.host("a/to-slash")).unwrap();
    symlink("../../outside", f.host("a/b/up")).unwrap();
    rustix::fs::mkfifoat(CWD, f.host("fifo"), Mode::from_raw_mode(0o600)).unwrap();

    delete_contents(&f.root, &IgnoreList::new()).unwrap();
    assert!(f.host.is_dir());
    assert_eq!(tree(&f.host), BTreeSet::new());
    assert_eq!(snapshot(&out), before);
}

#[test]
fn ignore_list_keeps_entries_and_their_ancestors() {
    let f = Fixture::new();
    fs::create_dir_all(f.host("a/b/deep")).unwrap();
    fs::write(f.host("a/b/deep/f"), b"").unwrap();
    fs::create_dir_all(f.host("a/bc")).unwrap();
    fs::write(f.host("a/other"), b"").unwrap();
    fs::write(f.host("c"), b"").unwrap();
    fs::create_dir_all(f.host("x/y")).unwrap();
    fs::write(f.host("x/z"), b"").unwrap();
    let mut keep = IgnoreList::new();
    keep.insert("a/b").unwrap();
    keep.insert("/c").unwrap();
    keep.insert("/x/./y/").unwrap();
    keep.insert("missing/path").unwrap();
    assert_eq!(keep.len(), 4);

    delete_contents(&f.root, &keep).unwrap();
    let left: BTreeSet<PathBuf> = ["a", "a/b", "a/b/deep", "a/b/deep/f", "c", "x", "x/y"]
        .into_iter()
        .map(PathBuf::from)
        .collect();
    assert_eq!(tree(&f.host), left);
}

#[test]
fn ignore_list_entries_are_cleaned_and_confined() {
    let mut keep = IgnoreList::new();
    assert!(keep.is_empty());
    for bad in ["", "/", ".", "..", "a/../..", "/../etc"] {
        let e = keep.insert(bad).unwrap_err();
        assert!(matches!(e, Error::InvalidPath { .. }), "{bad:?}: {e}");
    }
    keep.insert("/proc").unwrap();
    keep.insert("ranikust/state/").unwrap();
    assert!(keep.contains("proc"));
    assert!(keep.contains("/./proc/"));
    assert!(keep.contains("/ranikust/x/../state"));
    assert!(!keep.contains("/ranikust"));
    assert!(!keep.contains("/proc/self"));
}

#[test]
fn entries_on_another_mount_are_kept() {
    let f = Fixture::new();
    fs::create_dir_all(f.host("a/b")).unwrap();
    fs::write(f.host("f"), b"").unwrap();
    // Every entry now looks as if it were on another mount.
    let root = RootPath::open(&f.host)
        .unwrap()
        .with_mnt_id(f.root.mnt_id() + 1);
    let mut counts = Counts::default();
    let start = open_dir(&root, Path::new("a")).unwrap();
    let all_gone = remove_below(
        &root,
        start,
        Path::new("a"),
        &mut |_, _| Keep::Remove,
        &mut counts,
    )
    .unwrap();
    assert!(!all_gone);
    delete_contents(&root, &IgnoreList::new()).unwrap();
    assert_eq!(tree(&f.host).len(), 3);
}

#[test]
fn deletes_a_tree_deeper_than_path_max() {
    const DEPTH: usize = 3004;
    // One fd per level is held; raise the soft limit like ranikust does at
    // start-up (only ever upwards, and only in this test process).
    let lim = rustix::process::getrlimit(rustix::process::Resource::Nofile);
    let need = u64::try_from(DEPTH).unwrap() + 64;
    if lim.current.is_some_and(|c| c < need) {
        assert!(
            lim.maximum.is_none_or(|m| m >= need),
            "RLIMIT_NOFILE hard limit below {need}"
        );
        rustix::process::setrlimit(
            rustix::process::Resource::Nofile,
            rustix::process::Rlimit {
                current: lim.maximum,
                maximum: lim.maximum,
            },
        )
        .unwrap();
    }
    let f = Fixture::new();
    let mut cur =
        rustix::fs::open(&f.host, OFlags::PATH | OFlags::DIRECTORY, Mode::empty()).unwrap();
    for _ in 0..DEPTH {
        rustix::fs::mkdirat(&cur, "dddddddddd", Mode::from_raw_mode(0o755)).unwrap();
        cur = rustix::fs::openat(
            &cur,
            "dddddddddd",
            OFlags::PATH | OFlags::DIRECTORY,
            Mode::empty(),
        )
        .unwrap();
    }
    let leaf = rustix::fs::openat(
        &cur,
        "leaf",
        OFlags::WRONLY | OFlags::CREATE,
        Mode::from_raw_mode(0o644),
    )
    .unwrap();
    drop((leaf, cur));
    let mut counts = Counts::default();
    let start = open_dir(&f.root, Path::new("")).unwrap();
    remove_below(
        &f.root,
        start,
        Path::new(""),
        &mut |_, _| Keep::Remove,
        &mut counts,
    )
    .unwrap();
    assert_eq!(
        (counts.files, counts.dirs),
        (1, u64::try_from(DEPTH).unwrap())
    );
    assert_eq!(fs::read_dir(&f.host).unwrap().count(), 0);
}

/// A thread keeps exchanging a directory in the root with a symlink to a
/// directory outside while the root is emptied: the outside must survive.
#[test]
fn concurrent_swap_never_deletes_outside() {
    for _ in 0..20 {
        let f = Fixture::new();
        let (out, before) = f.outside();
        fs::create_dir(f.host("d")).unwrap();
        for i in 0..50 {
            fs::write(f.host(&format!("d/f{i}")), b"").unwrap();
        }
        symlink(&out, f.host("swap")).unwrap();
        let host_dir = fs::File::open(&f.host).unwrap();
        let stop = AtomicBool::new(false);
        let result = std::thread::scope(|s| {
            s.spawn(|| {
                while !stop.load(Ordering::Relaxed) {
                    let r = rustix::fs::renameat_with(
                        &host_dir,
                        "d",
                        &host_dir,
                        "swap",
                        rustix::fs::RenameFlags::EXCHANGE,
                    );
                    if r.is_err() {
                        // One side was deleted: the race is over.
                        break;
                    }
                }
            });
            let r = delete_contents(&f.root, &IgnoreList::new());
            stop.store(true, Ordering::Relaxed);
            r
        });
        match result {
            Ok(()) | Err(Error::Changed { .. }) => {}
            Err(e) => assert_eq!(errno(&e), Some(Errno::NOENT), "{e}"),
        }
        assert_eq!(snapshot(&out), before);
    }
}

/// Detaches its mounts, last first, when dropped (also on a panic), before
/// the temporary directory holding them is deleted; aborts if one stays,
/// so `remove_dir_all` never walks into a mount.
struct Detach(Vec<PathBuf>);

impl Drop for Detach {
    fn drop(&mut self) {
        for m in self.0.iter().rev() {
            if rustix::mount::unmount(m, rustix::mount::UnmountFlags::DETACH).is_err() {
                eprintln!(
                    "could not detach {}; aborting before any deletion",
                    m.display()
                );
                std::process::abort();
            }
        }
    }
}

/// D2 of experiment 09, T2 (root, the VM only): emptying a root keeps a
/// bind mount of a directory on the **same filesystem** (same `st_dev`,
/// so only the mount id tells it apart) and a tmpfs, with what they hold,
/// and a symlink and a hardlink pointing at them change nothing. The test
/// runs on a thread of its own in a new mount namespace, every mount
/// private there, so nothing it mounts reaches another namespace.
#[test]
#[ignore = "T2: mounts as real root in a private mount namespace; run in the test VM only"]
fn mounts_inside_the_root_survive() {
    std::thread::spawn(|| {
        use rustix::mount::{MountFlags, MountPropagationFlags, mount, mount_bind, mount_change};
        use rustix::thread::UnshareFlags;
        // SAFETY: this thread shares nothing that the unshared filesystem
        // attributes or mount namespace would break; it ends with the test.
        unsafe { rustix::thread::unshare_unsafe(UnshareFlags::FS | UnshareFlags::NEWNS) }
            .expect("unshare(CLONE_FS | CLONE_NEWNS)");
        mount_change(
            "/",
            MountPropagationFlags::PRIVATE | MountPropagationFlags::REC,
        )
        .expect("make every mount private");
        let f = Fixture::new();
        let (out, before) = f.outside();
        let source = f.host.parent().unwrap().join("same-fs-source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("keep"), b"bind").unwrap();
        fs::create_dir_all(f.host("bind")).unwrap();
        fs::create_dir_all(f.host("tmpfs")).unwrap();
        let mut guard = Detach(Vec::new());
        mount_bind(&source, f.host("bind")).expect("bind mount");
        guard.0.push(f.host("bind"));
        mount("tmpfs", f.host("tmpfs"), "tmpfs", MountFlags::empty(), None).expect("tmpfs");
        guard.0.push(f.host("tmpfs"));
        fs::write(f.host("tmpfs/keep"), b"tmpfs").unwrap();
        assert_eq!(
            fs::metadata(f.host("bind")).unwrap().dev(),
            fs::metadata(&f.host).unwrap().dev(),
            "the bind mount is on the root's filesystem"
        );
        symlink(f.host("bind"), f.host("to-bind")).unwrap();
        symlink("bind/keep", f.host("to-keep")).unwrap();
        fs::write(f.host("file"), b"gone").unwrap();
        delete_contents(&f.root, &IgnoreList::new()).unwrap();
        assert_eq!(
            fs::read(source.join("keep")).unwrap(),
            b"bind",
            "bind emptied"
        );
        assert_eq!(
            fs::read(f.host("tmpfs/keep")).unwrap(),
            b"tmpfs",
            "tmpfs emptied"
        );
        assert!(
            fs::symlink_metadata(f.host("file")).is_err(),
            "the root not emptied"
        );
        assert!(fs::symlink_metadata(f.host("to-bind")).is_err());
        assert_eq!(snapshot(&out), before);
        drop(guard);
    })
    .join()
    .unwrap();
}

/// An entry as a layer takes it: mode, owner, size, modification time,
/// link count, content or link target, and the first path of its hardlink
/// group. A directory's size is left out: a copy builds compact
/// directories, while one emptied in place keeps its size on ext4 and xfs,
/// which is why the hand-over is decided from the stage graph (ADR-0062).
type Entry = (u32, u32, u32, u64, (i64, i64), u64, Vec<u8>, PathBuf);

fn listing(dir: &Path) -> BTreeMap<PathBuf, Entry> {
    let mut all = Vec::new();
    let mut stack = vec![dir.to_owned()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            let md = fs::symlink_metadata(&p).unwrap();
            if md.is_dir() {
                stack.push(p.clone());
            }
            all.push((p, md));
        }
    }
    all.sort_by(|a, b| a.0.cmp(&b.0));
    let mut groups: BTreeMap<(u64, u64), PathBuf> = BTreeMap::new();
    let mut out = BTreeMap::new();
    for (p, md) in all {
        let rel = p.strip_prefix(dir).unwrap().to_owned();
        let body = if md.file_type().is_symlink() {
            fs::read_link(&p).unwrap().into_os_string().into_vec()
        } else if md.is_file() {
            fs::read(&p).unwrap()
        } else {
            Vec::new()
        };
        let group = groups
            .entry((md.dev(), md.ino()))
            .or_insert_with(|| rel.clone())
            .clone();
        let mtime = (md.mtime(), md.mtime_nsec());
        out.insert(
            rel,
            (
                md.mode(),
                md.uid(),
                md.gid(),
                if md.is_dir() { 0 } else { md.len() },
                mtime,
                md.nlink(),
                body,
                group,
            ),
        );
    }
    out
}

fn set_times(p: &Path, secs: i64) {
    let t = rustix::fs::Timespec {
        tv_sec: secs,
        tv_nsec: 7,
    };
    let times = rustix::fs::Timestamps {
        last_access: t,
        last_modification: t,
    };
    rustix::fs::utimensat(CWD, p, &times, AtFlags::SYMLINK_NOFOLLOW).unwrap();
}

/// A tree mixing what kaniko mode leaves out of a stage copy (ignored
/// paths, FIFOs, sockets) with what it keeps; every directory's times are
/// set last, to known values. The listener keeps the socket bound.
fn hand_over_tree(f: &Fixture) -> std::os::unix::net::UnixListener {
    let (out, _) = f.outside();
    for d in [
        "etc/sub",
        "var/run/app",
        "proc",
        "keep/deep/er",
        "ign-dir/x",
    ] {
        fs::create_dir_all(f.host(d)).unwrap();
    }
    fs::write(f.host("etc/passwd"), b"root:x:0:0\n").unwrap();
    fs::write(f.host("var/run/app/pid"), b"42").unwrap();
    fs::write(f.host("proc/stale"), b"x").unwrap();
    fs::write(f.host("ign-dir/x/f"), b"ignored").unwrap();
    fs::write(f.host("keep/deep/er/f"), b"kept").unwrap();
    fs::set_permissions(f.host("keep/deep/er/f"), fs::Permissions::from_mode(0o4755)).unwrap();
    fs::hard_link(f.host("keep/deep/er/f"), f.host("etc/hl")).unwrap();
    fs::hard_link(f.host("keep/deep/er/f"), f.host("ign-dir/hl")).unwrap();
    symlink(&out, f.host("etc/out")).unwrap();
    symlink("../../..", f.host("keep/up")).unwrap();
    rustix::fs::mkfifoat(CWD, f.host("keep/fifo"), Mode::from_raw_mode(0o600)).unwrap();
    rustix::fs::mkfifoat(CWD, f.host("fifo-top"), Mode::from_raw_mode(0o644)).unwrap();
    let sock = std::os::unix::net::UnixListener::bind(f.host("etc/sub/sock")).unwrap();
    let dirs = [
        "etc/sub",
        "etc",
        "var/run/app",
        "var/run",
        "var",
        "proc",
        "keep/deep/er",
        "keep/deep",
        "keep",
        "ign-dir/x",
        "ign-dir",
        "",
    ];
    for (secs, d) in (1_000_000..).zip(dirs) {
        set_times(&f.host(d), secs);
    }
    sock
}

fn hand_over_ignore() -> IgnoreList {
    let mut ignore = IgnoreList::new();
    ignore.insert_below("/proc").unwrap();
    ignore.insert("/var/run").unwrap();
    ignore.insert("/ign-dir").unwrap();
    ignore
}

fn special(t: FileType) -> bool {
    matches!(
        t,
        FileType::Fifo | FileType::CharDevice | FileType::BlockDevice | FileType::Socket
    )
}

/// ADR-0061: pruning a root of what a stage copy leaves out gives what the
/// copy gives, entry for entry (contents, modes, owners, modification
/// times, links, hardlinks between the names kept), and every directory it
/// deleted from keeps its times, the root's included.
#[test]
fn prune_gives_what_a_filtered_copy_gives() {
    let f = Fixture::new();
    let _sock = hand_over_tree(&f);
    let ignore = hand_over_ignore();
    let copy = Fixture::new();
    let opts = crate::CopyOpts {
        skip_special: true,
        ..crate::CopyOpts::default()
    };
    crate::copy_filtered(&f.root, "", &copy.root, "", &opts, &|p, _| {
        !ignore.covers(p)
    })
    .unwrap();
    let root_times = (
        fs::metadata(&f.host).unwrap().mtime(),
        fs::metadata(&f.host).unwrap().mtime_nsec(),
    );

    prune(&f.root, &|p, t| ignore.covers(p) || special(t)).unwrap();

    let (pruned, copied) = (listing(&f.host), listing(&copy.host));
    assert_eq!(pruned, copied);
    assert!(
        pruned.contains_key(Path::new("proc")),
        "below-only keeps its directory"
    );
    for gone in [
        "var/run",
        "ign-dir",
        "keep/fifo",
        "fifo-top",
        "etc/sub/sock",
        "proc/stale",
    ] {
        assert!(!pruned.contains_key(Path::new(gone)), "{gone}");
    }
    assert_eq!(fs::metadata(f.host("etc/sub")).unwrap().mtime(), 1_000_000);
    let now = fs::metadata(&f.host).unwrap();
    assert_eq!((now.mtime(), now.mtime_nsec()), root_times);
}

#[test]
fn prune_never_follows_a_link_out() {
    let f = Fixture::new();
    let (out, before) = f.outside();
    symlink(&out, f.host("to-outside")).unwrap();
    symlink("/", f.host("slash")).unwrap();
    fs::create_dir(f.host("d")).unwrap();
    symlink(out.join("sentinel"), f.host("d/sentinel")).unwrap();
    prune(&f.root, &|p, _| {
        p.starts_with("d") || p == Path::new("to-outside")
    })
    .unwrap();
    assert_eq!(tree(&f.host), BTreeSet::from([PathBuf::from("slash")]));
    assert_eq!(snapshot(&out), before);
}

#[test]
fn prune_refuses_a_root_on_another_mount() {
    let f = Fixture::new();
    fs::create_dir(f.host("a")).unwrap();
    // The root now looks as if it were on another mount than it claims.
    let root = RootPath::open(&f.host)
        .unwrap()
        .with_mnt_id(f.root.mnt_id() + 1);
    let e = prune(&root, &|_, _| true).unwrap_err();
    assert!(matches!(e, Error::Changed { .. }), "{e:?}");
    assert_eq!(tree(&f.host), BTreeSet::from([PathBuf::from("a")]));
}

/// ADR-0061: the machine's own `/` is refused before anything is walked
/// (the filter would delete nothing anyway, should the guard fail).
#[test]
fn prune_refuses_the_machines_own_root() {
    let slash = RootPath::open("/").unwrap();
    let e = prune(&slash, &|_, _| false).unwrap_err();
    assert!(
        matches!(
            e,
            Error::InvalidPath {
                reason: "the machine's own root directory",
                ..
            }
        ),
        "{e:?}"
    );
}

/// Links inside the root only: a pair, a kept name whose other name prune
/// deletes (a file at the top, `gone`), and one whose other names are in a
/// directory prune deletes whole (`ign/`). `top_first` creates the names
/// at the top before the directory `d`, which tmpfs lists in creation
/// order (ext4 in hash order), so the walk meets them in both orders.
fn links_only_inside(f: &Fixture, top_first: bool) {
    let top = |f: &Fixture| {
        fs::write(f.host("gone"), b"y").unwrap();
        fs::create_dir(f.host("ign")).unwrap();
        fs::write(f.host("ign/z"), b"z").unwrap();
    };
    if top_first {
        top(f);
    }
    fs::create_dir(f.host("d")).unwrap();
    fs::write(f.host("d/a"), b"x").unwrap();
    fs::hard_link(f.host("d/a"), f.host("d/b")).unwrap();
    if !top_first {
        top(f);
    }
    fs::hard_link(f.host("gone"), f.host("d/kept")).unwrap();
    fs::hard_link(f.host("ign/z"), f.host("d/kept-z")).unwrap();
    fs::hard_link(f.host("ign/z"), f.host("ign/z2")).unwrap();
}

/// ADR-0061: a kept file with a hardlink outside the root is reported; links
/// whose names are all inside it are not, whether prune keeps those names,
/// deletes them itself or inside a directory it deletes whole, and in
/// whatever order it meets them.
#[test]
fn prune_reports_a_hardlink_from_outside_the_root_in_any_order() {
    for top_first in [true, false] {
        let f = Fixture::new();
        links_only_inside(&f, top_first);
        let remove = |p: &Path, _: FileType| p == Path::new("gone") || p == Path::new("ign");
        let inside = prune(&f.root, &remove).unwrap();
        assert!(!inside.linked_outside, "top first {top_first}: {inside:?}");
        assert_eq!(
            tree(&f.host),
            ["d", "d/a", "d/b", "d/kept", "d/kept-z"]
                .map(PathBuf::from)
                .into(),
        );

        let (out, _) = f.outside();
        fs::hard_link(out.join("sentinel"), f.host("d/from-outside")).unwrap();
        let outside = prune(&f.root, &|_, _| false).unwrap();
        assert!(outside.linked_outside, "top first {top_first}: {outside:?}");
        assert_eq!(fs::read(out.join("sentinel")).unwrap(), b"x");
    }
}

/// ADR-0061: a link to a file outside the root whose every name in the
/// root prune deletes leaves nothing behind that reaches outside: not
/// reported, in a directory deleted whole or at the top.
#[test]
fn prune_does_not_report_an_outside_link_it_deletes() {
    let f = Fixture::new();
    let (out, _) = f.outside();
    fs::create_dir(f.host("ign")).unwrap();
    fs::hard_link(out.join("sentinel"), f.host("ign/outside")).unwrap();
    fs::hard_link(out.join("sub/file"), f.host("top-outside")).unwrap();
    let remove = |p: &Path, _: FileType| p == Path::new("ign") || p == Path::new("top-outside");
    let r = prune(&f.root, &remove).unwrap();
    assert!(!r.linked_outside, "{r:?}");
    assert!(tree(&f.host).is_empty());
    assert_eq!(fs::read(out.join("sentinel")).unwrap(), b"x");
}

/// ADR-0061: a mount below the root is refused, never pruned across, and
/// nothing on it is touched; a copy refuses one too.
#[test]
#[ignore = "T2: mounts as real root in a private mount namespace; run in the test VM only"]
fn prune_refuses_a_mount_below_the_root() {
    std::thread::spawn(|| {
        use rustix::mount::{MountPropagationFlags, mount_bind, mount_change};
        use rustix::thread::UnshareFlags;
        // SAFETY: this thread shares nothing that the unshared filesystem
        // attributes or mount namespace would break; it ends with the test.
        unsafe { rustix::thread::unshare_unsafe(UnshareFlags::FS | UnshareFlags::NEWNS) }
            .expect("unshare(CLONE_FS | CLONE_NEWNS)");
        mount_change(
            "/",
            MountPropagationFlags::PRIVATE | MountPropagationFlags::REC,
        )
        .expect("make every mount private");
        let f = Fixture::new();
        let source = f.host.parent().unwrap().join("bind-source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("keep"), b"bind").unwrap();
        rustix::fs::mkfifoat(CWD, source.join("fifo"), Mode::from_raw_mode(0o600)).unwrap();
        fs::create_dir_all(f.host("a/bind")).unwrap();
        let mut guard = Detach(Vec::new());
        mount_bind(&source, f.host("a/bind")).expect("bind mount");
        guard.0.push(f.host("a/bind"));
        let e = prune(&f.root, &|_, t| special(t)).unwrap_err();
        assert!(
            matches!(
                e,
                Error::InvalidPath {
                    reason: "a mount below the root",
                    ..
                }
            ),
            "{e:?}"
        );
        assert_eq!(fs::read(source.join("keep")).unwrap(), b"bind");
        assert!(fs::symlink_metadata(source.join("fifo")).is_ok());
        drop(guard);
    })
    .join()
    .unwrap();
}
