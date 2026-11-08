//! Deletion and pruning hold a bounded number of directory descriptors,
//! whatever the depth of the tree, and take time linear in its depth, as
//! do a live root's whiteouts.
//!
//! A test binary of its own: each deletion and pruning test lowers this
//! process's soft `RLIMIT_NOFILE` around the call so that exactly the
//! descriptors the function's doc promises are free, far fewer than the
//! depth of the trees. No other test should run under such a limit.

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
        // `.` and `..` are no numbers.
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
        // A drop cannot fail the test, and may run while it unwinds.
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
        // A drop cannot fail the test; a tree left behind is at least shown.
        if let Err(e) = delete_contents(&self.root, &IgnoreList::new()) {
            eprintln!("left behind in {}: {e}", self.host.display());
        }
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
    chain_of(at, name, DEPTH, NAME, files)
}

/// [`chain`] of `depth` levels named `level`.
fn chain_of(at: &OwnedFd, name: &str, depth: usize, level: &str, files: &[&str]) -> OwnedFd {
    rustix::fs::mkdirat(at, name, Mode::from_raw_mode(0o755)).unwrap();
    let mut cur = dir_fd(at, name);
    for _ in 0..depth {
        for f in files {
            file(&cur, f);
        }
        rustix::fs::mkdirat(&cur, level, Mode::from_raw_mode(0o755)).unwrap();
        cur = dir_fd(&cur, level);
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

/// Levels of the deepest chain: a `RUN` makes one in seconds, its path some
/// 50 times `PATH_MAX`.
const DEEPEST: usize = 100_000;

/// Writes `measured: <line>` to stderr past libtest's capture.
fn measured(line: &str) {
    use std::io::Write as _;
    // The line is for the log only; the assertions decide.
    let _ = writeln!(std::io::stderr().lock(), "measured: {line}");
}

/// A chain of [`DEEPEST`] levels, each holding a file, is pruned of its
/// files, then removed whole, with only the descriptors [`prune`] promises
/// free, each pass in time linear in its depth.
#[test]
fn a_chain_of_100k_levels_is_pruned_within_the_descriptors_promised() {
    let _serial = serial();
    let r = Root::new();
    let top = r.top();
    let made = std::time::Instant::now();
    drop(chain_of(&top, "deep", DEEPEST, "d", &["f"]));
    let made = made.elapsed();

    let files = std::time::Instant::now();
    let pruned = {
        let _free = Free::only(PRUNE_FDS);
        prune(&r.root, &|_: &Path, t: FileType| t == FileType::Regular)
    };
    let files = files.elapsed();
    assert!(!pruned.unwrap().linked_outside);
    let mut cur = dir_fd(&top, "deep");
    for level in 0..DEEPEST {
        if level % 10_000 == 0 {
            assert_eq!(names(&cur), ["d"], "level {level}");
        }
        cur = dir_fd(&cur, "d");
    }
    assert!(names(&cur).is_empty());
    drop(cur);

    let whole = std::time::Instant::now();
    let pruned = {
        let _free = Free::only(PRUNE_FDS);
        prune(&r.root, &|p: &Path, _: FileType| p == Path::new("deep"))
    };
    let whole = whole.elapsed();
    assert!(!pruned.unwrap().linked_outside);
    assert!(names(&top).is_empty());
    measured(&format!(
        "{DEEPEST} levels: made in {made:.2?}, pruned of its files in {files:.2?}, \
         removed whole in {whole:.2?}"
    ));
    // A fixed amount of work per level, as making it: never quadratic in
    // depth, which would take minutes here.
    for (what, took) in [("pruning files", files), ("removing whole", whole)] {
        assert!(
            took < 20 * made + std::time::Duration::from_secs(5),
            "{what} took {took:?}, making {made:?}"
        );
    }
}

#[test]
fn delete_contents_empties_a_chain_of_100k_levels_in_time_linear_in_its_depth() {
    let _serial = serial();
    let r = Root::new();
    let top = r.top();
    let made = std::time::Instant::now();
    drop(chain_of(&top, "deep", DEEPEST, "d", &["f"]));
    let made = made.elapsed();
    let took = std::time::Instant::now();
    {
        let _free = Free::only(DELETE_FDS);
        delete_contents(&r.root, &IgnoreList::new()).unwrap();
    }
    let took = took.elapsed();
    assert!(names(&top).is_empty());
    measured(&format!(
        "{DEEPEST} levels: made in {made:.2?}, deleted in {took:.2?}"
    ));
    assert!(
        took < 20 * made + std::time::Duration::from_secs(10),
        "deleting took {took:?}, making {made:?}"
    );
}

/// A list of literal, below-only and glob entries: two inside the chain
/// `deep` (its files `deep/d/f` and `deep/d/d/f`), the deeper ones beside
/// it.
fn mixed_list() -> IgnoreList {
    let mut keep = IgnoreList::new();
    keep.insert("deep/d/f").unwrap();
    keep.insert("elsewhere/a/b/c/d/e/f/g").unwrap();
    keep.insert_below("elsewhere/x/y/z").unwrap();
    keep.insert_glob("deep/d/d/[ef]").unwrap();
    keep.insert_glob("elsewhere/*/*/*/*/*/*/*/[xy]*").unwrap();
    keep
}

/// [`delete_contents`] asks the list about every entry of the chain: in
/// time linear in its depth still, keeping exactly what the list names.
#[test]
fn delete_contents_with_an_ignore_list_empties_a_chain_of_100k_levels_in_time_linear_in_its_depth()
{
    let _serial = serial();
    let r = Root::new();
    let top = r.top();
    let made = std::time::Instant::now();
    drop(chain_of(&top, "deep", DEEPEST, "d", &["f"]));
    let made = made.elapsed();
    let keep = mixed_list();
    let took = std::time::Instant::now();
    {
        let _free = Free::only(DELETE_FDS);
        delete_contents(&r.root, &keep).unwrap();
    }
    let took = took.elapsed();
    assert_eq!(names(&top), ["deep"]);
    let deep = dir_fd(&top, "deep");
    assert_eq!(names(&deep), ["d"]);
    let d1 = dir_fd(&deep, "d");
    assert_eq!(names(&d1), ["d", "f"]);
    assert_eq!(names(&dir_fd(&d1, "d")), ["f"]);
    measured(&format!(
        "{DEEPEST} levels with a list: made in {made:.2?}, deleted in {took:.2?}"
    ));
    assert!(
        took < 20 * made + std::time::Duration::from_secs(10),
        "deleting took {took:?}, making {made:?}"
    );
}

/// A layer of empty regular files named `names`: whiteouts, here.
fn layer_of(names: &[&str]) -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    for n in names {
        let mut h = tar::Header::new_gnu();
        h.set_path(n).unwrap();
        h.set_entry_type(tar::EntryType::Regular);
        h.set_mode(0o644);
        h.set_size(0);
        h.set_cksum();
        b.append(&h, std::io::empty()).unwrap();
    }
    b.into_inner().unwrap()
}

/// A whiteout in a live root removes a chain of [`DEEPEST`] levels, asking
/// the list about each entry, in time linear in its depth.
#[test]
fn a_whiteout_in_a_live_root_removes_a_chain_of_100k_levels_in_time_linear_in_its_depth() {
    let _serial = serial();
    let r = Root::new();
    let top = r.top();
    let made = std::time::Instant::now();
    drop(chain_of(&top, "deep", DEEPEST, "d", &["f"]));
    let made = made.elapsed();
    // Nothing on it lies below the chain, which a whiteout would then skip.
    let mut keep = IgnoreList::new();
    keep.insert("proc").unwrap();
    keep.insert("elsewhere/a/b/c/d/e/f/g").unwrap();
    keep.insert_below("tmp/apt-key-gpghome").unwrap();
    keep.insert_glob("var/cache/*/[xy]*").unwrap();
    let layer = layer_of(&[".wh.deep"]);
    let took = std::time::Instant::now();
    rnk_rootfs::extract_live(&r.root, layer.as_slice(), &keep).unwrap();
    let took = took.elapsed();
    assert!(names(&top).is_empty());
    measured(&format!(
        "{DEEPEST} levels whited out: made in {made:.2?}, removed in {took:.2?}"
    ));
    assert!(
        took < 20 * made + std::time::Duration::from_secs(10),
        "removing took {took:?}, making {made:?}"
    );
}

/// An opaque marker in a live root empties a chain of [`DEEPEST`] levels,
/// asking the list about each entry and whether it holds a kept one, in
/// time linear in its depth, and keeps what the list names.
#[test]
fn an_opaque_marker_in_a_live_root_empties_a_chain_of_100k_levels_in_time_linear_in_its_depth() {
    let _serial = serial();
    let r = Root::new();
    let top = r.top();
    let made = std::time::Instant::now();
    drop(chain_of(&top, "deep", DEEPEST, "d", &["f"]));
    let made = made.elapsed();
    let keep = mixed_list();
    let layer = layer_of(&["deep/.wh..wh..opq"]);
    let took = std::time::Instant::now();
    rnk_rootfs::extract_live(&r.root, layer.as_slice(), &keep).unwrap();
    let took = took.elapsed();
    assert_eq!(names(&top), ["deep"]);
    let deep = dir_fd(&top, "deep");
    assert_eq!(names(&deep), ["d"]);
    let d1 = dir_fd(&deep, "d");
    assert_eq!(names(&d1), ["d", "f"]);
    assert_eq!(names(&dir_fd(&d1, "d")), ["f"]);
    measured(&format!(
        "{DEEPEST} levels below an opaque marker: made in {made:.2?}, emptied in {took:.2?}"
    ));
    assert!(
        took < 20 * made + std::time::Duration::from_secs(10),
        "emptying took {took:?}, making {made:?}"
    );
}

/// Levels of the comb's spine, and of each of its teeth: three times
/// [`DELETE_FDS`], and more than it.
const SPINE: usize = 96;
const TOOTH: usize = 40;
const _: () = assert!(SPINE as u64 == 3 * DELETE_FDS && TOOTH as u64 > DELETE_FDS);

/// A spine of [`SPINE`] levels, each holding a file `f` and a tooth: a
/// chain of [`TOOTH`] levels, each holding a file `f`. Every walk climbs out
/// of a tooth through `..` and goes down the next one from a directory it
/// reopened, again and again.
fn comb(top: &OwnedFd) {
    rustix::fs::mkdirat(top, "comb", Mode::from_raw_mode(0o755)).unwrap();
    let mut cur = dir_fd(top, "comb");
    for _ in 0..SPINE {
        file(&cur, "f");
        drop(chain_of(&cur, "tooth", TOOTH, NAME, &["f"]));
        rustix::fs::mkdirat(&cur, NAME, Mode::from_raw_mode(0o755)).unwrap();
        cur = dir_fd(&cur, NAME);
    }
}

/// The names of each level of the chain below `name` in `at`, down to its
/// end.
fn levels(at: &OwnedFd, name: &str) -> Vec<Vec<OsString>> {
    let mut out = Vec::new();
    let mut cur = dir_fd(at, name);
    loop {
        let n = names(&cur);
        let next = n.iter().any(|n| n == NAME);
        out.push(n);
        if !next {
            return out;
        }
        cur = dir_fd(&cur, NAME);
    }
}

/// The comb is deleted whole, and pruned of its files and of every other
/// tooth, with only the descriptors each function promises free: a walk
/// that went back down from a reopened directory keeps to the bound, and
/// `prune` leaves exactly what it should, its spine's times included.
#[test]
fn a_comb_of_deep_teeth_is_deleted_and_pruned_within_the_descriptors_promised() {
    let _serial = serial();
    let r = Root::new();
    let top = r.top();
    comb(&top);
    let times = Timestamps {
        last_access: TIMES,
        last_modification: TIMES,
    };
    let mut parent = r.top();
    let mut name = "comb";
    for _ in 0..=SPINE {
        rustix::fs::utimensat(&parent, name, &times, AtFlags::SYMLINK_NOFOLLOW).unwrap();
        parent = dir_fd(&parent, name);
        name = NAME;
    }
    drop(parent);

    // Every other tooth goes whole, from the spine's first level on; every
    // file goes.
    let pruned = {
        let _free = Free::only(PRUNE_FDS);
        prune(&r.root, &|p: &Path, t: FileType| match t {
            FileType::Regular => true,
            FileType::Directory if p.file_name() == Some("tooth".as_ref()) => {
                p.components().count().is_multiple_of(2)
            }
            _ => false,
        })
    };
    assert!(!pruned.unwrap().linked_outside);
    let mut cur = dir_fd(&top, "comb");
    for level in 0..SPINE {
        let want: &[&str] = if level.is_multiple_of(2) {
            &[NAME]
        } else {
            &[NAME, "tooth"]
        };
        assert_eq!(names(&cur), want, "spine level {level}");
        assert_eq!(mtime(&cur), TIMES, "spine level {level}");
        if !level.is_multiple_of(2) {
            let tooth = levels(&cur, "tooth");
            assert_eq!(tooth.len(), TOOTH + 1, "spine level {level}");
            assert!(
                tooth[..TOOTH].iter().all(|n| n == &[NAME]) && tooth[TOOTH].is_empty(),
                "spine level {level}: {tooth:?}"
            );
        }
        cur = dir_fd(&cur, NAME);
    }
    assert!(names(&cur).is_empty());
    drop(cur);

    rustix::fs::mkdirat(&top, "again", Mode::from_raw_mode(0o755)).unwrap();
    comb(&dir_fd(&top, "again"));
    let deleted = {
        let _free = Free::only(DELETE_FDS);
        delete_contents(&r.root, &IgnoreList::new())
    };
    deleted.unwrap();
    assert!(names(&top).is_empty());
}
