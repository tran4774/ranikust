//! Deletion and pruning hold a bounded number of directory descriptors,
//! whatever the depth of the tree.
//!
//! A test binary of its own: each test lowers this process's soft
//! `RLIMIT_NOFILE` around the call so that exactly the descriptors the
//! function's doc promises are free, far fewer than the depth of the trees,
//! which no other test should run under.

use std::ffi::OsString;
use std::fs;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rnk_rootfs::{FileType, IgnoreList, RootPath, delete_contents, prune};
use rustix::fs::{AtFlags, Mode, OFlags, Timespec, Timestamps};
use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};

/// Directory levels of each chain: far more than the descriptors left free.
const DEPTH: usize = 2000;
/// Descriptors [`delete_contents`] holds at most, as its doc says.
const DELETE_FDS: u64 = 32;
/// Descriptors [`prune`] holds at most, as its doc says.
const PRUNE_FDS: u64 = 64;
/// A name long enough that the trees' paths are far past `PATH_MAX`.
const NAME: &str = "dddddddddddddddd";
/// The times every directory of the pruned tree is given.
const TIMES: Timespec = Timespec {
    tv_sec: 1_000_000,
    tv_nsec: 7,
};

/// The limit is the process's: one test at a time, so that no other test
/// opens descriptors while it is lowered.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// The descriptor numbers this process has open.
fn open_fds() -> Vec<u64> {
    let fd = rustix::fs::open(
        "/proc/self/fd",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .unwrap();
    let own = u64::try_from(fd.as_raw_fd()).unwrap();
    rustix::fs::Dir::new(fd)
        .unwrap()
        .filter_map(|e| e.unwrap().file_name().to_str().unwrap().parse().ok())
        .filter(|&n| n != own)
        .collect()
}

/// Lowers the soft `RLIMIT_NOFILE` until dropped so that exactly `free`
/// descriptor numbers are left for new descriptors (a new one takes the
/// lowest free number, and must be below the soft limit).
struct Free {
    was: Rlimit,
}

impl Free {
    fn only(free: u64) -> Self {
        let was = getrlimit(Resource::Nofile);
        let open = open_fds();
        let mut limit = free;
        loop {
            let below = open.iter().filter(|&&n| n < limit).count() as u64;
            if free + below == limit {
                break;
            }
            limit = free + below;
        }
        assert!(was.current.is_none_or(|c| c >= limit), "{was:?}");
        setrlimit(
            Resource::Nofile,
            Rlimit {
                current: Some(limit),
                maximum: was.maximum,
            },
        )
        .unwrap();
        Self { was }
    }
}

impl Drop for Free {
    fn drop(&mut self) {
        let _ = setrlimit(Resource::Nofile, self.was);
    }
}

/// A fresh root below a temp dir. Emptied by [`delete_contents`] on drop,
/// before the temp dir goes: its own removal recurses with a descriptor
/// per level, which a deep tree may not leave it.
struct Root {
    host: PathBuf,
    root: RootPath,
    _tmp: tempfile::TempDir,
}

impl Root {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        // The temp dir may sit below a symlink, which `RootPath::open` refuses.
        let host = fs::canonicalize(tmp.path()).unwrap();
        let root = RootPath::open(&host).unwrap();
        Self {
            host,
            root,
            _tmp: tmp,
        }
    }

    fn top(&self) -> OwnedFd {
        rustix::fs::open(
            &self.host,
            OFlags::RDONLY | OFlags::DIRECTORY,
            Mode::empty(),
        )
        .unwrap()
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = delete_contents(&self.root, &IgnoreList::new());
    }
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

/// Makes the directory `name` in `at` and a chain of [`DEPTH`] directories
/// named [`NAME`] below it, each level but the last also holding the files
/// `files` (made by descriptor, a couple open at a time). Returns the last,
/// empty level.
fn chain(at: &OwnedFd, name: &str, files: &[&str]) -> OwnedFd {
    rustix::fs::mkdirat(at, name, Mode::from_raw_mode(0o755)).unwrap();
    let mut cur = dir_fd(at, name);
    for _ in 0..DEPTH {
        for f in files {
            file(&cur, f);
        }
        rustix::fs::mkdirat(&cur, NAME, Mode::from_raw_mode(0o755)).unwrap();
        cur = dir_fd(&cur, NAME);
    }
    cur
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
    let x = rustix::fs::statx(fd, "", AtFlags::EMPTY_PATH, rustix::fs::StatxFlags::MTIME).unwrap();
    Timespec {
        tv_sec: x.stx_mtime.tv_sec,
        tv_nsec: x.stx_mtime.tv_nsec.into(),
    }
}

#[test]
fn delete_contents_empties_a_tree_far_deeper_than_the_descriptors_it_may_use() {
    let _serial = serial();
    let r = Root::new();
    let top = r.top();
    drop(chain(&top, "gone", &["f"]));
    file(&chain(&top, "kept", &[]), "leaf");
    let mut keep = IgnoreList::new();
    keep.insert("kept").unwrap();

    let deleted = {
        let _free = Free::only(DELETE_FDS);
        delete_contents(&r.root, &keep)
    };

    deleted.unwrap();
    assert_eq!(names(&top), ["kept"]);
    let mut cur = dir_fd(&top, "kept");
    for _ in 0..DEPTH {
        cur = dir_fd(&cur, NAME);
    }
    assert_eq!(names(&cur), ["leaf"]);
}

#[test]
fn prune_empties_a_tree_far_deeper_than_the_descriptors_it_may_use() {
    let _serial = serial();
    let r = Root::new();
    let top = r.top();
    // Each level of `kept` loses a file and keeps the next level; at its
    // bottom, a chain `gone` goes whole while its parent's walk is as deep;
    // so does the chain `gone` in the root.
    let bottom = chain(&top, "kept", &["rm"]);
    file(&bottom, "leaf");
    drop(chain(&bottom, "gone", &["f"]));
    drop(bottom);
    drop(chain(&top, "gone", &["f"]));
    // Every directory of `kept` and the root, once everything in them is
    // made.
    let times = Timestamps {
        last_access: TIMES,
        last_modification: TIMES,
    };
    let mut parent = dir_fd(&top, "kept");
    for _ in 0..DEPTH {
        rustix::fs::utimensat(&parent, NAME, &times, AtFlags::SYMLINK_NOFOLLOW).unwrap();
        parent = dir_fd(&parent, NAME);
    }
    drop(parent);
    rustix::fs::utimensat(&top, "kept", &times, AtFlags::SYMLINK_NOFOLLOW).unwrap();
    rustix::fs::futimens(&top, &times).unwrap();

    let pruned = {
        let _free = Free::only(PRUNE_FDS);
        prune(&r.root, &|p: &Path, t: FileType| {
            let name = p.file_name().unwrap_or_default();
            (t == FileType::Directory && name == "gone") || (t == FileType::Regular && name == "rm")
        })
    };

    assert!(!pruned.unwrap().linked_outside);
    assert_eq!(names(&top), ["kept"]);
    assert_eq!(mtime(&top), TIMES, "the root");
    let mut cur = dir_fd(&top, "kept");
    for level in 0..DEPTH {
        assert_eq!(names(&cur), [NAME], "level {level}");
        assert_eq!(mtime(&cur), TIMES, "level {level}");
        cur = dir_fd(&cur, NAME);
    }
    assert_eq!(names(&cur), ["leaf"]);
    assert_eq!(mtime(&cur), TIMES, "the last level");
}
