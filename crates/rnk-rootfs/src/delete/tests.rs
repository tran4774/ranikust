use std::collections::BTreeSet;
use std::fs;
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
