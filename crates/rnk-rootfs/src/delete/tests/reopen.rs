//! A deletion walk deeper than [`OPEN_DIRS`] closes its farthest ancestors
//! and reopens each through `..` of its child on the way back up. Each test
//! changes the tree while the walk is at the bottom of a chain (from the
//! walk's own callback, so at a known point), then checks what the walk did
//! and that nothing outside the root, or beside where a directory went,
//! changed (T1, T3, T4).

use std::cell::{Cell, RefCell};
use std::fs;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};

use rustix::io::Errno;

use crate::delete::{
    Counts, Keep, OPEN_DIRS, Pruned, delete_contents, open_dir, prune, remove_below, reopen_parent,
};
use crate::error::{Error, Result};
use crate::ignore::IgnoreList;
use crate::meta::FileType;
use crate::test_util::{Fixture, Snapshot, errno, snapshot};

/// Levels of the chain: the first ones' descriptors are closed while the
/// walk is at the bottom.
const DEPTH: usize = 3 * OPEN_DIRS;

/// Makes the chain `d/d/…/d` of [`DEPTH`] levels in `f`'s root, each level
/// holding a file `a` made before its `d` and a file `z` made after, so that
/// one of them is still to visit when the walk climbs back whatever order
/// the directory lists them in; the last level holds `leaf`. Outside, next
/// to `outside()`'s entries, lookalikes of what the walk would delete if it
/// took the outside for one of its directories: `a`, `z` and an empty `d`.
/// Returns the outside.
fn chain(f: &Fixture) -> PathBuf {
    let (out, _) = f.outside();
    for n in ["a", "z"] {
        fs::write(out.join(n), b"outside").unwrap();
    }
    fs::create_dir(out.join("d")).unwrap();
    let mut dir = f.host.clone();
    for _ in 0..DEPTH {
        fs::write(dir.join("a"), b"").unwrap();
        dir.push("d");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.parent().unwrap().join("z"), b"").unwrap();
    }
    fs::write(dir.join("leaf"), b"").unwrap();
    out
}

/// A directory's entries, and its own modification time.
fn state(dir: &Path) -> (Snapshot, (i64, i64)) {
    use std::os::unix::fs::MetadataExt as _;
    let m = fs::metadata(dir).unwrap();
    (snapshot(dir), (m.mtime(), m.mtime_nsec()))
}

/// Empties `f`'s root but `side` with [`remove_below`], running `act`
/// once, when the walk meets `leaf`. Returns what it returned and the paths
/// it decided on after `act`.
fn delete_with(f: &Fixture, act: impl FnOnce()) -> (Result<bool>, Vec<PathBuf>) {
    let mut act = Some(act);
    let mut later = Vec::new();
    let mut decide = |p: &Path, _: FileType| {
        if act.is_none() {
            later.push(p.to_owned());
        } else if p.file_name() == Some("leaf".as_ref())
            && let Some(act) = act.take()
        {
            act();
        }
        if p.starts_with("side") {
            Keep::Whole
        } else {
            Keep::Remove
        }
    };
    let start = open_dir(&f.root, Path::new("")).unwrap();
    let r = remove_below(
        &f.root,
        start,
        Path::new(""),
        &mut decide,
        &mut Counts::default(),
    );
    assert!(act.is_none(), "the walk never reached the leaf: {r:?}");
    (r, later)
}

/// Prunes the files of `f`'s root but those in `side`, keeping its
/// directories, running `act` once, when the walk meets `leaf`.
fn prune_with(f: &Fixture, act: impl FnOnce()) -> Result<Pruned> {
    let act = RefCell::new(Some(act));
    let remove = |p: &Path, t: FileType| {
        if p.file_name() == Some("leaf".as_ref())
            && let Some(act) = act.borrow_mut().take()
        {
            act();
        }
        t == FileType::Regular && !p.starts_with("side")
    };
    let r = prune(&f.root, &remove);
    assert!(
        act.borrow().is_none(),
        "the walk never reached the leaf: {r:?}"
    );
    r
}

/// What happens to `d/d`, a directory whose descriptor the walk closed,
/// while the walk is at the bottom of the chain. Every change stays inside
/// the root.
#[derive(Clone, Copy, Debug)]
enum Inside {
    /// Renamed into `side`, a directory of the root holding lookalikes of
    /// what is left in `d`: `..` of it leads to `side`.
    Elsewhere,
    /// Renamed to `d/e`: `..` of it leads to `d` again, which no longer
    /// holds it as `d`.
    Beside,
    /// Renamed to `d/old`, with a symlink to the outside in its place.
    SymlinkInPlace,
    /// Renamed to `d/old`, with a directory holding a file in its place.
    DirInPlace,
    /// Renamed to `d/old`, with an empty directory in its place.
    EmptyDirInPlace,
}

impl Inside {
    const ALL: [Self; 5] = [
        Self::Elsewhere,
        Self::Beside,
        Self::SymlinkInPlace,
        Self::DirInPlace,
        Self::EmptyDirInPlace,
    ];

    /// Makes `side` and its lookalikes.
    fn prepare(self, f: &Fixture) {
        fs::create_dir(f.host("side")).unwrap();
        for n in ["a", "z", "keep"] {
            fs::write(f.host(&format!("side/{n}")), b"side").unwrap();
        }
    }

    fn act(self, f: &Fixture, out: &Path) {
        let to = match self {
            Self::Elsewhere => "side/d",
            Self::Beside => "d/e",
            _ => "d/old",
        };
        fs::rename(f.host("d/d"), f.host(to)).unwrap();
        match self {
            Self::SymlinkInPlace => symlink(out, f.host("d/d")).unwrap(),
            Self::DirInPlace => {
                fs::create_dir(f.host("d/d")).unwrap();
                fs::write(f.host("d/d/keep"), b"in place").unwrap();
            }
            Self::EmptyDirInPlace => fs::create_dir(f.host("d/d")).unwrap(),
            Self::Elsewhere | Self::Beside => {}
        }
    }

    /// What must be there once the walk is done, whatever it did.
    fn check_left(self, f: &Fixture, out: &Path) {
        let mut names: Vec<_> = fs::read_dir(f.host("side"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        names.sort();
        let mut want = vec!["a", "keep", "z"];
        if let Self::Elsewhere = self {
            want.insert(1, "d");
        }
        assert_eq!(names, want, "{self:?}: side");
        for n in ["a", "z", "keep"] {
            assert_eq!(
                fs::read(f.host(&format!("side/{n}"))).unwrap(),
                b"side",
                "{self:?}: side/{n}"
            );
        }
        match self {
            Self::SymlinkInPlace => {
                assert_eq!(fs::read_link(f.host("d/d")).unwrap(), out, "{self:?}");
            }
            Self::DirInPlace => {
                assert_eq!(fs::read(f.host("d/d/keep")).unwrap(), b"in place");
            }
            _ => {}
        }
    }
}

/// A closed ancestor renamed or replaced inside the root, from a symlink to
/// a directory: the walk stops with [`Error::Changed`] where what it reaches
/// or the name it removes is not what it left, never deleting the
/// lookalikes beside where the directory went, nor through the symlink,
/// nor what the directory put in its place holds. The empty directory put
/// in its place is the one thing it may remove there, as a name of the
/// root. Nothing outside changes.
#[test]
fn a_closed_ancestor_renamed_or_replaced_inside_the_root_is_never_mistaken() {
    for how in Inside::ALL {
        let f = Fixture::new();
        let out = chain(&f);
        how.prepare(&f);
        let before = state(&out);
        let (r, _) = delete_with(&f, || how.act(&f, &out));
        assert!(matches!(r, Err(Error::Changed { .. })), "{how:?}: {r:?}");
        assert_eq!(state(&out), before, "{how:?}: the outside");
        how.check_left(&f, &out);
        assert!(f.host("d").is_dir(), "{how:?}: the chain's first directory");
    }
}

/// So does `prune`, which removes no directory it keeps: a closed ancestor
/// renamed out of its parent's reach stops it with [`Error::Changed`];
/// renamed within its parent, or replaced there, `..` still leads to the
/// parent it left, and the walk goes on there, inside the root, never
/// visiting what was put in its place.
#[test]
fn prune_never_mistakes_a_closed_ancestor_renamed_or_replaced_inside_the_root() {
    for how in Inside::ALL {
        let f = Fixture::new();
        let out = chain(&f);
        how.prepare(&f);
        let before = state(&out);
        let r = prune_with(&f, || how.act(&f, &out));
        match how {
            Inside::Elsewhere => {
                assert!(matches!(r, Err(Error::Changed { .. })), "{how:?}: {r:?}");
            }
            _ => assert!(r.is_ok(), "{how:?}: {r:?}"),
        }
        assert_eq!(state(&out), before, "{how:?}: the outside");
        how.check_left(&f, &out);
    }
}

/// A closed ancestor moved out of the root, on the same filesystem, while
/// the walk is at the bottom: `..` leads back into it at its new place,
/// which the walk takes for what it was, so it empties it of the names it
/// read there while it was in the root; then `..` of it leads outside and
/// the walk stops with [`Error::Changed`]. What was put in the moved
/// directory under new names stays, as does everything else outside and
/// what the walk had not reached in the root.
#[test]
fn an_ancestor_moved_out_loses_only_names_the_walk_read_in_the_root() {
    for op in ["delete", "prune"] {
        let f = Fixture::new();
        let out = chain(&f);
        let after = Cell::new(None);
        let act = || {
            fs::rename(f.host("d/d"), out.join("moved")).unwrap();
            fs::write(out.join("moved/new"), b"outside").unwrap();
            fs::create_dir(out.join("moved/new-dir")).unwrap();
            fs::write(out.join("moved/new-dir/f"), b"outside").unwrap();
            let rest: Snapshot = snapshot(&out)
                .into_iter()
                .filter(|(p, _)| !p.starts_with("moved"))
                .collect();
            after.set(Some((rest, state(&out).1)));
        };
        let r = match op {
            "delete" => delete_with(&f, act).0.map(drop),
            _ => prune_with(&f, act).map(drop),
        };
        assert!(matches!(r, Err(Error::Changed { .. })), "{op}: {r:?}");
        let rest: Snapshot = snapshot(&out)
            .into_iter()
            .filter(|(p, _)| !p.starts_with("moved"))
            .collect();
        assert_eq!(
            Some((rest, state(&out).1)),
            after.take(),
            "{op}: the outside"
        );
        assert_eq!(fs::read(out.join("moved/new")).unwrap(), b"outside", "{op}");
        assert_eq!(
            fs::read(out.join("moved/new-dir/f")).unwrap(),
            b"outside",
            "{op}"
        );
        for n in ["a", "z"] {
            assert!(
                fs::symlink_metadata(out.join("moved").join(n)).is_err(),
                "{op}: {n}, read in the root, goes"
            );
        }
        assert!(f.host("d").is_dir(), "{op}: the chain's first directory");
    }
}

/// The root itself moved, or renamed, while the walk is at the bottom, and
/// a new directory made at its old path: the walk holds no descriptor of
/// the root by then, and reopens it through `..` of its first level,
/// wherever it is. It empties the root it was given, at its new place, and
/// leaves the new directory and the outside as they were.
#[test]
fn the_root_moved_during_the_walk_is_still_the_one_emptied() {
    for into_outside in [false, true] {
        for op in ["delete", "prune"] {
            let f = Fixture::new();
            let out = chain(&f);
            let moved = if into_outside {
                out.join("root-moved")
            } else {
                f.host.with_file_name("root-renamed")
            };
            let after = Cell::new(None);
            let act = || {
                fs::rename(&f.host, &moved).unwrap();
                fs::create_dir(&f.host).unwrap();
                fs::write(f.host("a"), b"new").unwrap();
                fs::create_dir(f.host("d")).unwrap();
                fs::write(f.host("d/z"), b"new").unwrap();
                let rest: Snapshot = snapshot(&out)
                    .into_iter()
                    .filter(|(p, _)| !p.starts_with("root-moved"))
                    .collect();
                after.set(Some(rest));
            };
            let r = match op {
                "delete" => delete_with(&f, act).0.map(drop),
                _ => prune_with(&f, act).map(drop),
            };
            assert!(r.is_ok(), "{op}, into the outside {into_outside}: {r:?}");
            let rest: Snapshot = snapshot(&out)
                .into_iter()
                .filter(|(p, _)| !p.starts_with("root-moved"))
                .collect();
            assert_eq!(Some(rest), after.take(), "{op}: the outside");
            assert_eq!(fs::read(f.host("a")).unwrap(), b"new", "{op}");
            assert_eq!(fs::read(f.host("d/z")).unwrap(), b"new", "{op}");
            let left = crate::test_util::tree(&moved);
            match op {
                "delete" => assert!(left.is_empty(), "{op}: {left:?}"),
                // Only the chain's directories stay.
                _ => assert!(
                    left.iter()
                        .all(|p| p.components().all(|c| c.as_os_str() == "d")),
                    "{op}: {left:?}"
                ),
            }
        }
    }
}

/// A closed ancestor the walk may no longer open (mode 000, 300 or 600 set
/// while it is at the bottom; not for root, whom no mode stops): the walk
/// stops with `EACCES` where it reopens it or works in it, deleting nothing
/// above it, and there is no path-based fallback.
#[test]
fn a_closed_ancestor_the_caller_may_no_longer_enter_stops_the_walk() {
    if rustix::process::geteuid().is_root() {
        eprintln!("skipped: modes do not stop root");
        return;
    }
    for mode in [0o000, 0o300, 0o600] {
        let f = Fixture::new();
        let out = chain(&f);
        let before = state(&out);
        let (r, later) = delete_with(&f, || {
            fs::set_permissions(f.host("d/d/d"), fs::Permissions::from_mode(mode)).unwrap();
        });
        let e = r.unwrap_err();
        assert_eq!(errno(&e), Some(Errno::ACCESS), "mode {mode:o}: {e:?}");
        fs::set_permissions(f.host("d/d/d"), fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(state(&out), before, "mode {mode:o}: the outside");
        for p in ["d", "d/d", "d/d/d"] {
            assert!(f.host(p).is_dir(), "mode {mode:o}: {p}");
        }
        // Nothing in `d/d/d` or above was decided on once it changed.
        let above: Vec<_> = later
            .iter()
            .filter(|p| p.components().count() <= 4)
            .collect();
        assert!(above.is_empty(), "mode {mode:o}: {above:?}");
    }
}

/// The parent of a mount root lies on another mount: reopening it through
/// `..` is refused (`EXDEV`), never followed, and the walk reports a
/// change. A plain `openat(..)` from the same descriptor shows the control:
/// it does reach the other mount.
#[test]
fn the_parent_of_a_mount_root_is_never_reopened() {
    use rustix::fs::{Mode, OFlags};
    let found = ["/proc", "/sys", "/dev", "/run", "/dev/shm", "/tmp"]
        .into_iter()
        .find_map(|p| {
            let fd = rustix::fs::open(
                p,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .ok()?;
            let st =
                crate::meta::statx_at(std::os::fd::AsFd::as_fd(&fd), "".as_ref(), Path::new(p))
                    .ok()?;
            st.mount_root.then_some((Path::new(p), fd, st))
        });
    let (path, fd, st) = found.expect("a mount root besides /");
    let up = rustix::fs::openat(&fd, "..", OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty())
        .expect("control: a plain open of ..");
    let up_st =
        crate::meta::statx_at(std::os::fd::AsFd::as_fd(&up), "".as_ref(), Path::new("..")).unwrap();
    assert_ne!(
        up_st.mnt_id, st.mnt_id,
        "control: .. of {path:?} is on another mount"
    );

    let e = crate::root::open_parent(std::os::fd::AsFd::as_fd(&fd), path).unwrap_err();
    assert_eq!(errno(&e), Some(Errno::XDEV), "{path:?}: {e:?}");
    let e = reopen_parent(std::os::fd::AsFd::as_fd(&fd), &up_st, path).unwrap_err();
    assert!(matches!(e, Error::Changed { .. }), "{path:?}: {e:?}");
}

/// Runs `f` on a thread of its own in a new mount namespace, every mount
/// private there, so that nothing it mounts reaches another namespace.
fn in_private_mounts(f: impl FnOnce() + Send + 'static) {
    std::thread::spawn(|| {
        use rustix::mount::{MountPropagationFlags, mount_change};
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
        f();
    })
    .join()
    .unwrap();
}

/// What becomes a mount point while the walk is at the bottom of the chain.
#[derive(Clone, Copy, Debug)]
enum Mounted {
    /// A tmpfs on `d/d`, a closed ancestor, holding lookalikes of what the
    /// walk left there.
    TmpfsOnAncestor,
    /// `d/d` bound onto itself: the same directory, now a mount's root.
    AncestorOnItself,
    /// The outside bound onto `d/d/d`.
    OutsideOnAncestor,
    /// A tmpfs on the root itself, which the walk reopens last.
    TmpfsOnRoot,
}

impl Mounted {
    const ALL: [Self; 4] = [
        Self::TmpfsOnAncestor,
        Self::AncestorOnItself,
        Self::OutsideOnAncestor,
        Self::TmpfsOnRoot,
    ];

    /// Mounts, and fills a tmpfs; returns the mount point.
    fn mount(self, f: &Fixture, out: &Path) -> PathBuf {
        use rustix::mount::{MountFlags, mount, mount_bind};
        let at = match self {
            Self::TmpfsOnAncestor | Self::AncestorOnItself => f.host("d/d"),
            Self::OutsideOnAncestor => f.host("d/d/d"),
            Self::TmpfsOnRoot => f.host.clone(),
        };
        match self {
            Self::TmpfsOnAncestor | Self::TmpfsOnRoot => {
                mount("tmpfs", &at, "tmpfs", MountFlags::empty(), None).expect("tmpfs");
                for n in ["a", "z", "keep"] {
                    fs::write(at.join(n), b"tmpfs").unwrap();
                }
                fs::create_dir(at.join("d")).unwrap();
            }
            Self::AncestorOnItself => mount_bind(&at, &at).expect("bind onto itself"),
            Self::OutsideOnAncestor => mount_bind(out, &at).expect("bind the outside"),
        }
        at
    }

    /// The tmpfs holds what it was given.
    fn check(self, at: &Path) {
        if let Self::TmpfsOnAncestor | Self::TmpfsOnRoot = self {
            let mut names: Vec<_> = fs::read_dir(at)
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            names.sort();
            assert_eq!(names, ["a", "d", "keep", "z"], "{self:?}");
            for n in ["a", "z", "keep"] {
                assert_eq!(fs::read(at.join(n)).unwrap(), b"tmpfs", "{self:?}: {n}");
            }
        }
    }
}

/// T1: a closed ancestor, or the root, that became a mount point while the
/// walk was at the bottom: `..` of its child would step onto the mount, so
/// reopening it is refused (`EXDEV`) and the walk reports a change, never
/// deleting on the mount, nor in the outside bound there.
#[test]
#[ignore = "T1: privileged: mounts in a private mount namespace"]
fn a_closed_ancestor_that_became_a_mount_point_is_never_reopened() {
    in_private_mounts(|| {
        for how in Mounted::ALL {
            for op in ["delete", "prune"] {
                let f = Fixture::new();
                let out = chain(&f);
                let before = snapshot(&out);
                // Dropped before the fixture: detaches what was mounted.
                let mounts = RefCell::new(super::Detach(Vec::new()));
                let at = RefCell::new(None);
                let act = || {
                    let p = how.mount(&f, &out);
                    mounts.borrow_mut().0.push(p.clone());
                    *at.borrow_mut() = Some(p);
                };
                let r = match op {
                    "delete" => delete_with(&f, act).0.map(drop),
                    _ => prune_with(&f, act).map(drop),
                };
                assert!(
                    matches!(r, Err(Error::Changed { .. })),
                    "{how:?}, {op}: {r:?}"
                );
                how.check(at.borrow().as_ref().unwrap());
                assert_eq!(snapshot(&out), before, "{how:?}, {op}: the outside");
                drop(mounts);
            }
        }
    });
}

/// Levels of the chain with mounts deep in it, past the descriptors a walk
/// keeps.
const MOUNTED_DEPTH: usize = 6 * OPEN_DIRS;
/// The levels holding a bind mount of the outside, and a tmpfs.
const BIND_AT: usize = OPEN_DIRS + 8;
const TMPFS_AT: usize = 4 * OPEN_DIRS + 22;

/// The path of level `k` of the chain `d/d/…`.
fn level(k: usize) -> String {
    vec!["d"; k].join("/")
}

/// T1: a chain of [`MOUNTED_DEPTH`] levels holding the outside bound at
/// level [`BIND_AT`] and a tmpfs at [`TMPFS_AT`]: emptying the root keeps
/// both mounts and every level above them, and deletes everything else,
/// the levels below the tmpfs' level included; the walk climbs through
/// directories it reopened to get there. `prune` refuses the mounts, with
/// nothing on them touched.
#[test]
#[ignore = "T1: privileged: mounts in a private mount namespace"]
fn mounts_deep_in_a_chain_are_kept_with_every_level_above_them() {
    in_private_mounts(|| {
        use rustix::mount::{MountFlags, mount, mount_bind};
        for op in ["delete", "prune"] {
            let f = Fixture::new();
            let (out, before) = f.outside();
            for k in 1..=MOUNTED_DEPTH {
                let d = f.host(&level(k));
                fs::create_dir(&d).unwrap();
                fs::write(d.join("a"), b"").unwrap();
                fs::create_dir(d.join("s")).unwrap();
                fs::write(d.join("s/f"), b"").unwrap();
            }
            fs::create_dir(f.host(&format!("{}/b", level(BIND_AT)))).unwrap();
            fs::create_dir(f.host(&format!("{}/m", level(TMPFS_AT)))).unwrap();
            // Dropped before the fixture: detaches what was mounted.
            let mut mounts = super::Detach(Vec::new());
            let bind = f.host(&format!("{}/b", level(BIND_AT)));
            mount_bind(&out, &bind).expect("bind the outside");
            mounts.0.push(bind);
            let tmpfs = f.host(&format!("{}/m", level(TMPFS_AT)));
            mount("tmpfs", &tmpfs, "tmpfs", MountFlags::empty(), None).expect("tmpfs");
            mounts.0.push(tmpfs.clone());
            fs::write(tmpfs.join("keep"), b"tmpfs").unwrap();
            fs::create_dir(tmpfs.join("d")).unwrap();

            if op == "delete" {
                delete_contents(&f.root, &IgnoreList::new()).unwrap();
                for k in 0..=TMPFS_AT {
                    let mut names: Vec<_> = fs::read_dir(f.host(&level(k)))
                        .unwrap()
                        .map(|e| e.unwrap().file_name().into_string().unwrap())
                        .collect();
                    names.sort();
                    let want: &[&str] = match k {
                        BIND_AT => &["b", "d"],
                        TMPFS_AT => &["m"],
                        _ => &["d"],
                    };
                    assert_eq!(names, want, "level {k}");
                }
            } else {
                let e = prune(&f.root, &|_, t| t == FileType::Regular).unwrap_err();
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
            }
            assert_eq!(fs::read(tmpfs.join("keep")).unwrap(), b"tmpfs", "{op}");
            assert!(tmpfs.join("d").is_dir(), "{op}");
            assert_eq!(snapshot(&out), before, "{op}: the outside");
            drop(mounts);
        }
    });
}
