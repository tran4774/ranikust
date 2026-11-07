//! Deletion and pruning hold a bounded number of directory descriptors,
//! whatever the depth of the tree.
//!
//! A test binary of its own: it lowers this process's soft `RLIMIT_NOFILE`
//! far below the depth of the trees, which no other test should run under.

use std::ffi::OsString;
use std::fs;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::Once;

use rnk_rootfs::{FileType, IgnoreList, RootPath, delete_contents, prune};
use rustix::fs::{AtFlags, Mode, OFlags, Timespec, Timestamps};

/// Directory levels of each tree: far more than [`LIMIT`].
const DEPTH: usize = 2000;
/// The soft `RLIMIT_NOFILE` the tests run under.
const LIMIT: u64 = 256;
/// A name long enough that the trees' paths are far past `PATH_MAX`.
const NAME: &str = "dddddddddddddddd";
/// The times every directory of the pruned tree is given.
const TIMES: Timespec = Timespec {
    tv_sec: 1_000_000,
    tv_nsec: 7,
};

/// Lowers the soft limit, once for the whole binary, keeping the hard one.
fn lowered_limit() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
        let lim = getrlimit(Resource::Nofile);
        if lim.current.is_none_or(|c| c > LIMIT) {
            setrlimit(
                Resource::Nofile,
                Rlimit {
                    current: Some(LIMIT),
                    maximum: lim.maximum,
                },
            )
            .unwrap();
        }
    });
    let now = rustix::process::getrlimit(rustix::process::Resource::Nofile);
    assert!(now.current.is_some_and(|c| c <= LIMIT), "{now:?}");
}

/// A fresh root below a temp dir, removed on drop.
fn root() -> (tempfile::TempDir, PathBuf, RootPath) {
    let tmp = tempfile::tempdir().unwrap();
    // The temp dir may sit below a symlink, which `RootPath::open` refuses.
    let host = fs::canonicalize(tmp.path()).unwrap();
    let root = RootPath::open(&host).unwrap();
    (tmp, host, root)
}

fn dir_fd(at: &OwnedFd, name: &str) -> OwnedFd {
    rustix::fs::openat(at, name, OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty()).unwrap()
}

fn file(at: &OwnedFd, name: &str) {
    drop(
        rustix::fs::openat(
            at,
            name,
            OFlags::WRONLY | OFlags::CREATE,
            Mode::from_raw_mode(0o644),
        )
        .unwrap(),
    );
}

/// A chain of [`DEPTH`] directories named [`NAME`] below `name` in `host`
/// (made by descriptor, holding one at a time), each level also holding
/// the files `files`, the last one the file `leaf`.
fn chain(host: &Path, name: &str, files: &[&str], leaf: &str) {
    let mut cur =
        rustix::fs::open(host, OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty()).unwrap();
    rustix::fs::mkdirat(&cur, name, Mode::from_raw_mode(0o755)).unwrap();
    cur = dir_fd(&cur, name);
    for _ in 0..DEPTH {
        for f in files {
            file(&cur, f);
        }
        rustix::fs::mkdirat(&cur, NAME, Mode::from_raw_mode(0o755)).unwrap();
        cur = dir_fd(&cur, NAME);
    }
    file(&cur, leaf);
}

/// The names in the directory `fd`, sorted.
fn names(fd: &OwnedFd) -> Vec<OsString> {
    let mut names: Vec<OsString> = rustix::fs::Dir::read_from(fd)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_bytes().to_vec())
        .filter(|n| n != b"." && n != b"..")
        .map(|n| String::from_utf8(n).unwrap().into())
        .collect();
    names.sort();
    names
}

/// The modification time of the directory `fd`, as [`TIMES`] holds it.
fn mtime(fd: &OwnedFd) -> Timespec {
    let x = rustix::fs::statx(
        fd,
        "",
        AtFlags::EMPTY_PATH,
        rustix::fs::StatxFlags::MTIME,
    )
    .unwrap();
    Timespec {
        tv_sec: x.stx_mtime.tv_sec,
        tv_nsec: x.stx_mtime.tv_nsec.into(),
    }
}

#[test]
fn delete_contents_empties_a_tree_far_deeper_than_the_descriptor_limit() {
    lowered_limit();
    let (_tmp, host, root) = root();
    chain(&host, "gone", &["f"], "leaf");
    chain(&host, "kept", &[], "leaf");
    let mut keep = IgnoreList::new();
    keep.insert("kept").unwrap();

    delete_contents(&root, &keep).unwrap();

    let top = rustix::fs::open(&host, OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty()).unwrap();
    assert_eq!(names(&top), ["kept"]);
    let mut cur = dir_fd(&top, "kept");
    for _ in 0..DEPTH {
        cur = dir_fd(&cur, NAME);
    }
    assert_eq!(names(&cur), ["leaf"]);
}

#[test]
fn prune_empties_a_tree_far_deeper_than_the_descriptor_limit() {
    lowered_limit();
    let (_tmp, host, root) = root();
    // Each level of `kept` loses a file and keeps the next level; `gone`
    // goes whole.
    chain(&host, "kept", &["rm"], "leaf");
    chain(&host, "gone", &["f"], "leaf");
    // Every directory of `kept` and the root, once everything in them is
    // made.
    let times = Timestamps {
        last_access: TIMES,
        last_modification: TIMES,
    };
    let top = rustix::fs::open(&host, OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty()).unwrap();
    let mut parent = dir_fd(&top, "kept");
    for _ in 0..DEPTH {
        rustix::fs::utimensat(&parent, NAME, &times, AtFlags::SYMLINK_NOFOLLOW).unwrap();
        parent = dir_fd(&parent, NAME);
    }
    drop(parent);
    rustix::fs::utimensat(&top, "kept", &times, AtFlags::SYMLINK_NOFOLLOW).unwrap();
    rustix::fs::futimens(&top, &times).unwrap();

    let pruned = prune(&root, &|p: &Path, t: FileType| {
        p == Path::new("gone") || (t == FileType::Regular && p.ends_with("rm"))
    })
    .unwrap();

    assert!(!pruned.linked_outside);
    assert_eq!(names(&top), ["kept"]);
    assert_eq!(mtime(&top), TIMES, "the root");
    let mut cur = dir_fd(&top, "kept");
    for level in 0..DEPTH {
        assert_eq!(names(&cur), [NAME], "level {level}");
        assert_eq!(mtime(&cur), TIMES, "level {level}");
        cur = dir_fd(&cur, NAME);
    }
    assert_eq!(names(&cur), ["leaf"]);
}
