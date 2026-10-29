use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use rustix::fs::{CWD, Mode};

use super::*;
use crate::test_util::{Fixture, errno};

/// Every path below `dir`, relative to it.
fn tree(dir: &Path) -> BTreeSet<PathBuf> {
    let mut out = BTreeSet::new();
    let mut stack = vec![dir.to_owned()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            if fs::symlink_metadata(&p).unwrap().is_dir() {
                stack.push(p.clone());
            }
            out.insert(p.strip_prefix(dir).unwrap().to_owned());
        }
    }
    out
}

/// A directory outside the root with a few entries, and its listing.
fn outside(f: &Fixture) -> (PathBuf, BTreeSet<PathBuf>) {
    let out = f.host.with_file_name("outside");
    fs::create_dir_all(out.join("sub")).unwrap();
    fs::write(out.join("sentinel"), b"x").unwrap();
    fs::write(out.join("sub/file"), b"y").unwrap();
    let t = tree(&out);
    (out, t)
}

#[test]
fn deletes_everything_but_the_root() {
    let f = Fixture::new();
    let (out, before) = outside(&f);
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
    assert_eq!(tree(&out), before);
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
    let all_gone =
        remove_below(&root, Path::new("a"), &mut |_, _| Keep::Remove, &mut counts).unwrap();
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
    remove_below(
        &f.root,
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
        let (out, before) = outside(&f);
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
        assert_eq!(tree(&out), before);
    }
}

/// Run by the T1 harness inside a mount namespace, which prepares
/// `$RNK_T1_ROOT` with `bind/` (a bind mount of a directory on the same
/// filesystem) and `tmpfs/` (a tmpfs), each holding a file `keep`.
#[test]
#[ignore = "T1: needs a bind mount and a tmpfs inside the root (mount namespace)"]
fn mounts_inside_the_root_survive() {
    let dir = std::env::var_os("RNK_T1_ROOT").expect("RNK_T1_ROOT is set by the T1 harness");
    let root = RootPath::open(&dir).unwrap();
    delete_contents(&root, &IgnoreList::new()).unwrap();
    let dir = PathBuf::from(dir);
    assert!(
        dir.join("bind/keep").exists(),
        "same-filesystem bind mount emptied"
    );
    assert!(dir.join("tmpfs/keep").exists(), "tmpfs emptied");
}
