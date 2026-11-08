//! Red team of `--userns`'s state directory (ADR-0104, ADR-0139): layouts
//! root refuses with nothing written, and what the range's uid plants in
//! a state directory given to it before it takes its name (T4, T21).

use std::fs;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _, chown, lchown, symlink};
use std::path::{Path, PathBuf};

use super::super::*;
use super::{BEFORE_RENAME, private_tempdir};
use crate::test_util::{Base, snapshot};

/// A range of the host's ids, as `--userns` gives its state directory to.
const RANGE: Owner = Owner::Mapped {
    uid: 310_000,
    gid: 410_000,
};
/// The range's first uid and gid.
const RANGE_IDS: (u32, u32) = (310_000, 410_000);
/// A third user's ids, neither root's nor the range's.
const FAR: (u32, u32) = (520_000, 520_000);

/// A layout `--userns` refuses, for any user: a missing chain below a
/// parent root does not own, a symlink on the way, an existing directory
/// with something in it, a `..`: nothing is made or changed. Where the
/// test runs as a user other than root, the chain itself refuses each.
#[test]
fn red_team_userns_refusals_make_nothing_whatever_the_layout() {
    let tmp = private_tempdir();
    let real = tmp.path().join("real");
    fs::create_dir(&real).unwrap();
    symlink(&real, tmp.path().join("link")).unwrap();
    let filled = tmp.path().join("filled");
    fs::create_dir(&filled).unwrap();
    fs::write(filled.join("keep"), "x").unwrap();
    let me = Owner::Mapped {
        uid: rustix::process::geteuid().as_raw(),
        gid: rustix::process::getegid().as_raw(),
    };
    let before = snapshot(tmp.path());
    for state in [
        "new/a/state",
        "link/state",
        "link",
        "filled",
        "real/../new",
        "real/./new",
    ] {
        let state = PathBuf::from(format!("{}/{state}", tmp.path().display()));
        for owner in [RANGE, me] {
            assert!(
                prepare_for(&state, owner).is_err(),
                "{} taken",
                state.display()
            );
            assert_eq!(snapshot(tmp.path()), before, "{}", state.display());
        }
    }
}

/// `path` given to `ids` with `mode`.
fn own(path: &Path, ids: (u32, u32), mode: u32) {
    chown(path, Some(ids.0), Some(ids.1)).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

/// A directory `name` of `base`, owned by `ids` with `mode`.
fn dir_of(base: &Path, name: &str, ids: (u32, u32), mode: u32) -> PathBuf {
    let p = base.join(name);
    fs::create_dir(&p).unwrap();
    own(&p, ids, mode);
    p
}

/// `mkfifo` of `path`.
fn fifo(path: &Path) {
    rustix::fs::mknodat(
        rustix::fs::CWD,
        path,
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::from_raw_mode(0o600),
        0,
    )
    .unwrap();
}

/// Ends the process with `SIGALRM` after `secs`: a FIFO opened by
/// mistake would otherwise hang the suite; 0 cancels it.
fn deadline(secs: u32) {
    // SAFETY: `alarm` only arms or cancels this process's timer.
    unsafe { libc::alarm(secs) };
}

/// T2, as real root: each state directory `--userns` must refuse is
/// refused by its check, before anything is made or changed (ADR-0139):
/// below a parent writable by group or others (sticky or not), the
/// range's, a third user's or one the range may not search; through a
/// symlink, or a symlink itself; an existing one writable by others,
/// holding what is not a store, root's or a third user's; a FIFO or a
/// file where it should be; a missing chain below a refused parent.
#[test]
#[ignore = "T2: real root checks --userns state directories below /var/lib"]
fn red_team_root_refuses_each_unfit_userns_state_directory_writing_nothing() {
    assert!(rustix::process::geteuid().is_root(), "run as root");
    let base = Base::new("statedir");
    let b = base.0.as_path();
    dir_of(b, "gw", (0, 0), 0o775);
    dir_of(b, "sticky", (0, 0), 0o1777);
    dir_of(b, "ranges", RANGE_IDS, 0o755);
    dir_of(b, "fars", FAR, 0o755);
    dir_of(b, "closed", (0, 0), 0o700);
    dir_of(b, "group-only", (0, 0), 0o750);
    let real = dir_of(b, "real", (0, 0), 0o755);
    symlink(&real, b.join("link")).unwrap();
    let real2 = dir_of(b, "real2", (0, 0), 0o755);
    symlink(&real2, real.join("sl")).unwrap();
    lchown(real.join("sl"), Some(RANGE_IDS.0), Some(RANGE_IDS.1)).unwrap();
    dir_of(b, "open", RANGE_IDS, 0o777);
    let foreign = dir_of(b, "foreign", RANGE_IDS, 0o700);
    fs::write(foreign.join("keep"), "x").unwrap();
    own(&foreign.join("keep"), RANGE_IDS, 0o644);
    dir_of(b, "roots", (0, 0), 0o700);
    dir_of(b, "far-state", FAR, 0o700);
    fifo(&b.join("fifo"));
    fs::write(b.join("file"), "x").unwrap();
    let before = snapshot(b);
    deadline(60);
    for state in [
        "gw/state",
        "sticky/state",
        "ranges/state",
        "fars/state",
        "closed/state",
        "group-only/state",
        "link/state",
        "real/sl",
        "open",
        "foreign",
        "roots",
        "far-state",
        "fifo",
        "file",
        "gw/a/b/state",
        "ranges/a/state",
    ] {
        let state = b.join(state);
        let refused = prepare_for(&state, RANGE);
        assert!(refused.is_err(), "{} taken", state.display());
        assert_eq!(snapshot(b), before, "{}: {refused:?}", state.display());
    }
    deadline(0);
}

/// One way the range's uid plants in its state directory before it is
/// moved into place: what `builds` and the store's marker are, and
/// leftovers of the probe's name (a link out, a hard link to root's
/// file, a FIFO).
#[derive(Clone, Copy, Debug)]
enum Plant {
    BuildsLinkOut,
    BuildsHardLink,
    BuildsFifo,
    BuildsOpenDirWithHardLink,
    MarkerLinkOut,
    MarkerFifo,
}

/// What `plant` makes in `dir` (the state directory under its own name),
/// with root's `victim` file and `vdir` directory as targets, each of
/// the range's ids as the range would make it; a hard link keeps its
/// file's owner.
fn plant_in(dir: &Path, plant: Plant, victim: &Path, vdir: &Path) {
    let marker = dir.join(rnk_store::STATE_MARKER);
    let builds = dir.join("builds");
    let give = |p: &Path| lchown(p, Some(RANGE_IDS.0), Some(RANGE_IDS.1)).unwrap();
    match plant {
        Plant::MarkerLinkOut => {
            symlink(victim, &marker).unwrap();
            give(&marker);
        }
        Plant::MarkerFifo => {
            fifo(&marker);
            give(&marker);
        }
        _ => {
            fs::write(&marker, "").unwrap();
            give(&marker);
        }
    }
    match plant {
        Plant::BuildsLinkOut | Plant::MarkerLinkOut | Plant::MarkerFifo => {
            symlink(vdir, &builds).unwrap();
            give(&builds);
        }
        Plant::BuildsHardLink => fs::hard_link(victim, &builds).unwrap(),
        Plant::BuildsFifo => {
            fifo(&builds);
            give(&builds);
        }
        Plant::BuildsOpenDirWithHardLink => {
            fs::create_dir(&builds).unwrap();
            fs::hard_link(victim, builds.join("x")).unwrap();
            own(&builds, RANGE_IDS, 0o777);
        }
    }
    let probe = |s: &str| dir.join(format!("{}{s}", rnk_store::USERNS_PROBE_PREFIX));
    symlink(vdir, probe("a")).unwrap();
    give(&probe("a"));
    fs::hard_link(victim, probe("b")).unwrap();
    fifo(&probe("c"));
    give(&probe("c"));
    fs::create_dir(probe("d")).unwrap();
    fs::hard_link(victim, probe("d").join("x")).unwrap();
    own(&probe("d"), RANGE_IDS, 0o700);
}

/// T2, as real root: whatever the range's uid plants in the state
/// directory given to it before the move into place (ADR-0139) — `builds`
/// as a link out, a hard link to root's file, a FIFO or a directory open to
/// all; the store's marker as a link out or a FIFO; probe leftovers of
/// each kind — root's set-up before the namespace (the check, the store's
/// check, the mode, the leftovers' removal) neither blocks nor changes
/// root's files, follows no link, and deletes no file of root's.
#[test]
#[ignore = "T2: real root makes --userns state directories while the range plants in them"]
fn red_team_what_the_range_plants_before_the_rename_reaches_nothing_of_root_s() {
    assert!(rustix::process::geteuid().is_root(), "run as root");
    let base = Base::new("plant");
    let b = base.0.clone();
    let victim = b.join("victim");
    fs::write(&victim, "root").unwrap();
    own(&victim, (0, 0), 0o600);
    let vdir = dir_of(&b, "vdir", (0, 0), 0o755);
    fs::write(vdir.join("f"), "root").unwrap();
    own(&vdir.join("f"), (0, 0), 0o644);
    let far = b.join("far");
    fs::write(&far, "far").unwrap();
    own(&far, FAR, 0o644);
    let watched = |b: &Path| -> Vec<String> {
        ["victim", "vdir", "far"]
            .iter()
            .flat_map(|n| snapshot(&b.join(n)))
            // A hard link the range made changes the count and the ctime only.
            .map(|l| {
                let mut f: Vec<&str> = l.split(' ').collect();
                (f[3], f[6]) = ("-", "-");
                f.join(" ")
            })
            .collect()
    };
    let before = watched(&b);
    let mut outcomes = Vec::new();
    deadline(120);
    for (i, plant) in [
        Plant::BuildsLinkOut,
        Plant::BuildsHardLink,
        Plant::BuildsFifo,
        Plant::BuildsOpenDirWithHardLink,
        Plant::MarkerLinkOut,
        Plant::MarkerFifo,
    ]
    .into_iter()
    .enumerate()
    {
        let state = b.join(format!("s{i}"));
        let (b2, v2, d2) = (b.clone(), victim.clone(), vdir.clone());
        let planted = std::rc::Rc::new(std::cell::Cell::new(false));
        let seen = std::rc::Rc::clone(&planted);
        BEFORE_RENAME.with(|h| {
            *h.borrow_mut() = Some(Box::new(move || {
                let new = fs::read_dir(&b2)
                    .unwrap()
                    .map(|e| e.unwrap().path())
                    .find(|p| {
                        p.file_name()
                            .is_some_and(|n| n.to_string_lossy().starts_with(NEW_PREFIX))
                    })
                    .expect("the state directory under its own name");
                plant_in(&new, plant, &v2, &d2);
                seen.set(true);
            }));
        });
        let r = stage(&state, RANGE).and_then(|staged| {
            let held = rnk_store::Store::check_state_dir(&state)
                .map_err(|e| Refused(format!("the store's check: {e}")))?;
            staged.finish()?;
            drop(held);
            Ok(())
        });
        crate::setup::remove_own_probe_leftovers(&state);
        assert!(planted.get(), "{plant:?}: the hook never ran");
        outcomes.push(format!(
            "{plant:?}: {}",
            r.map_or_else(|e| e.0, |()| "taken".to_owned())
        ));
        assert_eq!(watched(&b), before, "{plant:?}: {outcomes:?}");
        let st = fs::symlink_metadata(&state).unwrap();
        assert_eq!((st.uid(), st.gid()), RANGE_IDS, "{plant:?}");
    }
    deadline(0);
    // Nothing root owns outside the state directories changed.
    assert_eq!(watched(&b), before, "{outcomes:?}");
    // Past libtest's capture: the lanes keep `measured:` lines.
    let mut err = std::io::stderr().lock();
    for o in &outcomes {
        let _ = std::io::Write::write_fmt(&mut err, format_args!("measured: {o}\n"));
    }
}
