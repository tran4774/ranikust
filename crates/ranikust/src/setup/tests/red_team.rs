//! Red team of overlay mode's check of its state directory (#213, #184)
//! (T4).

use std::fs;
use std::os::unix::fs::{lchown, symlink};
use std::path::Path;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use rustix::fs::{CWD, FileType, Mode, OFlags, makedev, mknodat};

use super::*;
use crate::statedir::{Owner, SYSTEM_DIRS};
use crate::test_util::{Base, snapshot};

/// Runs `f` on a thread of its own, since a FIFO opened by mistake blocks.
fn unblocked<T: Send + 'static>(case: &str, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = mpsc::channel();
    // The receiver is gone only once the case has failed as blocked.
    let worker = std::thread::spawn(move || drop(tx.send(f())));
    match rx.recv_timeout(Duration::from_secs(30)) {
        Ok(r) => r,
        Err(RecvTimeoutError::Timeout) => panic!("{case}: blocked"),
        Err(RecvTimeoutError::Disconnected) => {
            std::panic::resume_unwind(worker.join().unwrap_err())
        }
    }
}

/// An inotify watch for opens of some paths; `O_PATH`, which looks without opening, is not seen,
/// nor is an open that fails, as a socket's or a `0:0` whiteout's does with `ENXIO`.
struct Opens(std::os::fd::OwnedFd);

impl Opens {
    fn watch(paths: &[&Path]) -> Self {
        use rustix::fs::inotify::{self, CreateFlags, WatchFlags};
        let fd = inotify::init(CreateFlags::CLOEXEC | CreateFlags::NONBLOCK).unwrap();
        for p in paths {
            inotify::add_watch(&fd, *p, WatchFlags::OPEN).unwrap();
        }
        Self(fd)
    }

    fn seen(&self) -> bool {
        let mut buf = [0u8; 256];
        match rustix::io::read(&self.0, &mut buf) {
            Ok(n) => n > 0,
            Err(rustix::io::Errno::AGAIN) => false,
            Err(e) => panic!("read the inotify events: {e}"),
        }
    }

    /// The control: an open of a FIFO in `dir` is seen.
    fn control(dir: &Path) {
        let fifo = dir.join("control-fifo");
        mknodat(CWD, &fifo, FileType::Fifo, Mode::from_raw_mode(0o600), 0).unwrap();
        let opens = Self::watch(&[&fifo]);
        drop(rustix::fs::open(&fifo, OFlags::RDONLY | OFlags::NONBLOCK, Mode::empty()).unwrap());
        assert!(opens.seen(), "the control: an open is seen");
        fs::remove_file(&fifo).unwrap();
    }
}

/// Root's overlay mode's set-up of `state`, on a thread.
fn set_up_on(case: &str, state: &Path) -> Result<(), Refusal> {
    let s = state.to_owned();
    unblocked(case, move || {
        set_up(&s, false, &Seccomp::Unset, &mut false, false, false).map(drop)
    })
}

/// `hold_state_dir` of `state` for `owner`, on a thread: rootless overlay mode's check with
/// `Owner::me()` in T0, root's in T2, and `--userns`'s with a range.
fn held(case: &str, state: &Path, owner: Owner) -> Result<(), Refusal> {
    let s = state.to_owned();
    unblocked(case, move || {
        let uid = rustix::process::geteuid().as_raw();
        let on_overlayfs = || Refusal {
            kind: Kind::OverlayUnusable,
            reason: "on overlayfs".into(),
            guidance: "".into(),
        };
        hold_state_dir(&s, uid, owner, on_overlayfs).map(drop)
    })
}

/// Red team: a planted or unfit state directory against overlay mode's check (T4).
#[test]
fn red_team_a_planted_state_directory_is_refused_unchanged_by_overlay_mode_s_check() {
    if as_root() {
        return;
    }
    let tmp = private_tempdir();
    if crate::statedir::on_overlayfs(tmp.path()) {
        eprintln!("skipped: the temporary directory is on overlayfs");
        return;
    }
    let t = tmp.path();
    Opens::control(t);
    let node = |p: &Path, kind: FileType| mknodat(CWD, p, kind, Mode::from_raw_mode(0o600), 0);
    let outside = t.join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("f"), "keep").unwrap();
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o755)).unwrap();
    let fifo_out = t.join("fifo-out");
    node(&fifo_out, FileType::Fifo).unwrap();
    fs::write(t.join("file"), "keep").unwrap();
    symlink("missing", t.join("dead")).unwrap();
    // 0:0, a whiteout: the only device node a user may make.
    let chardev = match node(&t.join("chardev"), FileType::CharacterDevice) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("skipped a character device: {e}");
            false
        }
    };
    drop(std::os::unix::net::UnixListener::bind(t.join("sock")).unwrap());
    let mut cases = 0;
    let mut check = |state: &Path, case: &str, says: &str, watched: Option<&Path>| {
        let before = snapshot(t);
        let opens = watched.map(|w| Opens::watch(&[w]));
        for (how, r) in [
            ("set_up", set_up_on(case, state)),
            ("hold_state_dir", held(case, state, Owner::me())),
        ] {
            let Err(refusal) = r else {
                panic!("{case} ({how}): taken");
            };
            assert!(refusal.reason.contains(says), "{case} ({how}): {refusal}");
        }
        assert_eq!(snapshot(t), before, "{case}");
        assert!(!opens.is_some_and(|o| o.seen()), "{case}: opened");
        cases += 1;
    };
    // A store as builds leave it, open to others as no set-up leaves it.
    let store = |name: &str| {
        let state = t.join(name);
        Store::open(&BuildPaths::new(&state, &state))
            .unwrap()
            .close()
            .unwrap();
        for p in [&state, &state.join("builds")] {
            fs::set_permissions(p, fs::Permissions::from_mode(0o755)).unwrap();
        }
        state
    };

    for d in ["blobs", "builds", "tmp"] {
        for plant in [
            "a FIFO",
            "a socket",
            "a character device",
            "a link out",
            "a dead link",
            "a link to a FIFO",
        ] {
            if plant == "a character device" && !chardev {
                continue;
            }
            let state = store(&format!("{d}-{}", plant.replace(' ', "-")));
            let at = state.join(d);
            fs::remove_dir_all(&at).unwrap();
            match plant {
                "a FIFO" => node(&at, FileType::Fifo).unwrap(),
                "a socket" => drop(std::os::unix::net::UnixListener::bind(&at).unwrap()),
                "a character device" => node(&at, FileType::CharacterDevice).unwrap(),
                "a link out" => symlink(&outside, &at).unwrap(),
                "a dead link" => symlink("missing", &at).unwrap(),
                _ => symlink(&fifo_out, &at).unwrap(),
            }
            let watched = match plant {
                "a FIFO" => Some(at.as_path()),
                "a link to a FIFO" => Some(fifo_out.as_path()),
                _ => None,
            };
            let says = format!("holds {d}, which is");
            check(&state, &format!("{d} as {plant}"), &says, watched);
        }
    }

    // Not a store: a directory as a system one is, and markers that are not files.
    let sys = t.join("sys");
    for d in ["bin", "etc", "usr/lib"] {
        fs::create_dir_all(sys.join(d)).unwrap();
    }
    fs::write(sys.join("etc/passwd"), "root:x:0:0::/root:/bin/sh\n").unwrap();
    fs::set_permissions(&sys, fs::Permissions::from_mode(0o755)).unwrap();
    let unmarked = "has no regular file .ranikust-state";
    check(&sys, "a system-like directory", unmarked, None);
    for plant in [
        "a directory",
        "a FIFO",
        "a link to a file out",
        "a dead link",
    ] {
        let state = t.join(format!("marker-{}", plant.replace(' ', "-")));
        fs::create_dir(&state).unwrap();
        fs::create_dir(state.join("blobs")).unwrap();
        let marker = state.join(rnk_store::STATE_MARKER);
        match plant {
            "a directory" => fs::create_dir(&marker).unwrap(),
            "a FIFO" => node(&marker, FileType::Fifo).unwrap(),
            "a link to a file out" => symlink(outside.join("f"), &marker).unwrap(),
            _ => symlink("missing", &marker).unwrap(),
        }
        fs::set_permissions(&state, fs::Permissions::from_mode(0o755)).unwrap();
        let watched = (plant == "a FIFO").then_some(marker.as_path());
        check(&state, &format!("the marker as {plant}"), unmarked, watched);
    }

    // At or below what is not a directory.
    let mut at_or_below = vec!["fifo-out", "file", "dead", "sock"];
    if chardev {
        at_or_below.push("chardev");
    }
    for name in at_or_below {
        let says = "as a directory without following a symlink";
        let watched = (name == "fifo-out").then_some(fifo_out.as_path());
        check(&t.join(name), &format!("at {name}"), says, watched);
        let below = t.join(name).join("state");
        check(&below, &format!("below {name}"), says, watched);
    }
    assert!(cases >= 3 * 5 + 5 + 8, "{cases} cases");

    // The control: a store as builds leave it is taken, and made private.
    let state = store("taken");
    held("the control", &state, Owner::me()).unwrap();
    assert_eq!(fs::metadata(&state).unwrap().mode() & 0o7777, 0o700);
}

/// Red team, as real root: system directories and device nodes against overlay mode's check (T4).
#[test]
#[ignore = "T2: real root's overlay check on system directories and device nodes; one taken would change the machine"]
fn red_team_root_refuses_system_directories_and_device_nodes_unchanged() {
    assert!(rustix::process::geteuid().is_root(), "run as root");
    let store_names = [
        rnk_store::STATE_MARKER,
        "blobs",
        "layers",
        "caches",
        "locks",
        "tmp",
        "builds",
    ];
    let present = |dir: &Path| -> Vec<bool> {
        store_names
            .iter()
            .map(|n| fs::symlink_metadata(dir.join(n)).is_ok())
            .collect()
    };
    let ids = |m: &fs::Metadata| (m.mode(), m.uid(), m.gid());
    let mut tried = 0;
    for sys in SYSTEM_DIRS.iter().copied().chain(["/"]) {
        let dir = Path::new(sys);
        let Ok(before) = fs::symlink_metadata(dir) else {
            continue;
        };
        // The first rule the directory breaks, in the order the check runs.
        let says: &[&str] = if sys == "/" {
            &["is /"]
        } else if before.file_type().is_symlink() {
            &["without following a symlink"]
        } else if before.uid() != 0 {
            &["is owned by uid"]
        } else if before.mode() & 0o022 != 0 {
            &["is writable by group or others"]
        } else {
            &["system directory", "not a state directory"]
        };
        let had = present(dir);
        let Err(refusal) = held(sys, dir, Owner::me()) else {
            panic!("{sys} taken");
        };
        assert!(
            says.iter().any(|s| refusal.reason.contains(s)),
            "{sys}: {refusal}"
        );
        let after = fs::symlink_metadata(dir).unwrap();
        assert_eq!(ids(&after), ids(&before), "{sys}");
        assert_eq!(present(dir), had, "{sys}");
        tried += 1;
    }
    assert!(tried >= 10, "{tried} system directories");

    let base = Base::new("set-up");
    let b = base.0.as_path();
    Opens::control(b);
    let node = |p: &Path, kind: FileType, dev| {
        mknodat(CWD, p, kind, Mode::from_raw_mode(0o600), dev).unwrap();
    };
    let plant = |at: &Path, what: &str| match what {
        "a FIFO" => node(at, FileType::Fifo, 0),
        "a socket" => drop(std::os::unix::net::UnixListener::bind(at).unwrap()),
        "a character device" => node(at, FileType::CharacterDevice, makedev(1, 3)),
        "a block device" => node(at, FileType::BlockDevice, makedev(7, 0)),
        "a link to /etc" => symlink("/etc", at).unwrap(),
        _ => symlink("/dev/zero", at).unwrap(),
    };
    let plants = [
        "a FIFO",
        "a socket",
        "a character device",
        "a block device",
        "a link to /etc",
        "a link to a device",
    ];
    // Links' targets are the machine's: their owner and mode are checked, not their opens.
    let watched = |at: &Path, what: &str| {
        matches!(what, "a FIFO" | "a character device" | "a block device")
            .then(|| Opens::watch(&[at]))
    };
    let targets = || -> Vec<_> {
        ["/etc", "/dev/zero"]
            .iter()
            .map(|t| ids(&fs::symlink_metadata(t).unwrap()))
            .collect()
    };
    let targets_before = targets();
    for d in ["blobs", "builds", "tmp"] {
        for what in plants {
            let case = format!("{d} as {what}");
            let state = b.join(case.replace(' ', "-").replace('/', ""));
            Store::open(&BuildPaths::new(&state, &state))
                .unwrap()
                .close()
                .unwrap();
            for p in [&state, &state.join("builds")] {
                fs::set_permissions(p, fs::Permissions::from_mode(0o755)).unwrap();
            }
            fs::remove_dir_all(state.join(d)).unwrap();
            plant(&state.join(d), what);
            let opens = watched(&state.join(d), what);
            let before = snapshot(b);
            let says = format!("holds {d}, which is");
            for (how, r) in [
                ("set_up", set_up_on(&case, &state)),
                ("hold_state_dir", held(&case, &state, Owner::me())),
            ] {
                let Err(refusal) = r else {
                    panic!("{case} ({how}): taken");
                };
                assert!(refusal.reason.contains(&says), "{case} ({how}): {refusal}");
            }
            assert_eq!(snapshot(b), before, "{case}");
            assert!(!opens.is_some_and(|o| o.seen()), "{case}: opened");
        }
    }

    // `--userns`: the state directory and what is planted in it are the range's.
    let (uid, gid) = (310_000, 410_000);
    let range = Owner::Mapped { uid, gid };
    let give = |p: &Path| lchown(p, Some(uid), Some(gid)).unwrap();
    let ranged = |name: &str| {
        let state = b.join(name);
        fs::create_dir(&state).unwrap();
        fs::write(state.join(rnk_store::STATE_MARKER), "").unwrap();
        give(&state.join(rnk_store::STATE_MARKER));
        fs::set_permissions(&state, fs::Permissions::from_mode(0o755)).unwrap();
        give(&state);
        state
    };
    for d in ["blobs", "builds", "tmp"] {
        for what in plants {
            let case = format!("the range's {d} as {what}");
            let state = ranged(&case.replace(' ', "-").replace(['/', '\''], ""));
            plant(&state.join(d), what);
            give(&state.join(d));
            let opens = watched(&state.join(d), what);
            let before = snapshot(b);
            // Root's overlay mode takes no directory of the range's: refused for its owner.
            let owner = format!("is owned by uid {uid}");
            let says = format!("holds {d}, which is");
            for (how, r, says) in [
                ("set_up", set_up_on(&case, &state), &owner),
                ("hold_state_dir", held(&case, &state, range), &says),
            ] {
                let Err(refusal) = r else {
                    panic!("{case} ({how}): taken");
                };
                assert!(refusal.reason.contains(says), "{case} ({how}): {refusal}");
            }
            assert_eq!(snapshot(b), before, "{case}");
            assert!(!opens.is_some_and(|o| o.seen()), "{case}: opened");
        }
    }
    assert_eq!(targets(), targets_before, "/etc and /dev/zero");

    // The controls: root's store and the range's are taken, made private.
    let state = b.join("root-s");
    Store::open(&BuildPaths::new(&state, &state))
        .unwrap()
        .close()
        .unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o755)).unwrap();
    held("root's control", &state, Owner::me()).unwrap();
    assert_eq!(fs::metadata(&state).unwrap().mode() & 0o7777, 0o700);
    let state = ranged("the-range-s");
    held("the range's control", &state, range).unwrap();
    assert_eq!(fs::metadata(&state).unwrap().mode() & 0o7777, 0o700);
}
