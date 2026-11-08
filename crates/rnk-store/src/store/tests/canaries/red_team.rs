//! Red team of the canary of an id map (ADR-0128, T13): links out of the
//! state directory, special files, loops and unreadable directories where
//! the canary, its directories or its lock would be, and stores racing to
//! make it. The maps are given to the stores; nothing enters a user
//! namespace or gives a file another owner.

use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use rustix::fs::{CWD, Mode, mknodat};

use super::*;

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

/// The check before a store opens, with `maps` as the build's namespace's
/// (from its parent), then the store's open with `maps` as its own: each
/// on a thread of its own, each `Ok` or the error's text, prefixed
/// `changed: ` for [`Error::IdMapChanged`].
fn both_checks(
    case: &str,
    paths: &BuildPaths,
    maps: &IdMaps,
) -> [std::result::Result<(), String>; 2] {
    let text = |e: Error| match e {
        Error::IdMapChanged { .. } => format!("changed: {e}"),
        e => e.to_string(),
    };
    let (state, m) = (paths.state.clone(), maps.clone());
    let outside = unblocked(case, move || {
        give(None);
        Store::check_state_dir_for(&state, &[m])
            .map(drop)
            .map_err(text)
    });
    let (p, m) = (paths.clone(), maps.clone());
    let inside = unblocked(case, move || {
        give(Some(m));
        Store::open(&p).and_then(Store::close).map_err(text)
    });
    [outside, inside]
}

/// Makes a special file at a path.
type Make = fn(&Path) -> std::io::Result<()>;

/// A case: what it plants, and how in the state directory.
type Planted<'a, F> = (&'a str, F);

/// Plants in a state directory.
type Plant<'a> = &'a dyn Fn(&Path);

/// Plants a canary for the maps at its path.
type PlantCanary = fn(&Path, &IdMaps);

/// The kinds of special file a user may make: `(name, make at path)`.
/// A character device `0:0`, a whiteout, is the one device node a user
/// may make; where the kernel refuses it, it is left out.
fn special_files() -> Vec<Planted<'static, Make>> {
    let fifo: Make = |p| {
        mknodat(
            CWD,
            p,
            rustix::fs::FileType::Fifo,
            Mode::from_raw_mode(0o644),
            0,
        )
        .map_err(Into::into)
    };
    // `bind` takes 108 bytes at most: bound at a short name, then renamed.
    let socket: Make = |p| {
        let short = p.parent().unwrap().parent().unwrap().join("s");
        drop(std::os::unix::net::UnixListener::bind(&short)?);
        fs::rename(&short, p)
    };
    let whiteout: Make = |p| {
        mknodat(
            CWD,
            p,
            rustix::fs::FileType::CharacterDevice,
            Mode::from_raw_mode(0o644),
            0,
        )
        .map_err(Into::into)
    };
    let mut kinds = vec![("a FIFO", fifo), ("a socket", socket)];
    let probe = tempfile::tempdir().unwrap();
    match whiteout(&probe.path().join("w")) {
        Ok(()) => kinds.push(("a whiteout", whiteout)),
        Err(e) => eprintln!("skipped a character device: {e}"),
    }
    kinds
}

/// A fresh state directory a store opened once with no maps, and its paths.
fn fresh() -> (tempfile::TempDir, BuildPaths) {
    let tmp = tempfile::tempdir().unwrap();
    let paths = BuildPaths::new(tmp.path(), tmp.path().join("state"));
    give(None);
    Store::open(&paths).unwrap().close().unwrap();
    (tmp, paths)
}

/// Red team (T13): a link out of the state directory at `layers/v3`, at
/// the canaries' directory or at their locks', absolute or climbing with
/// `..`, never lets the check read a canary outside it, nor the first
/// layer under the maps make one or its lock there: resolved in the state
/// directory, it reaches only what is there.
#[test]
fn red_team_a_link_out_never_reads_or_makes_a_canary_outside_the_state_directory() {
    let maps = own_maps();
    let hex = canary::path(&maps).file_name().unwrap().to_owned();
    // Where the link is, and the directories below its target that hold
    // the canary (or the canary's lock).
    let links: [(&str, &[&str]); 3] = [
        ("layers/v3", &["canary"]),
        ("layers/v3/canary", &[]),
        ("locks/layers/v3/canary", &[]),
    ];
    // How the link names `outside`, and whether the state directory holds
    // a directory where that name leads once kept in it.
    let targets = [
        ("absolute", false),
        ("..'s", false),
        ("..'s, there too", true),
    ];
    for (at, below) in links {
        for (how, mirrored) in targets {
            let case = format!("{at} -> outside ({how})");
            let (tmp, paths) = fresh();
            let outside = tmp.path().join("outside");
            let reached = below.iter().fold(outside.clone(), |p, c| p.join(c));
            fs::create_dir_all(&reached).unwrap();
            let target = if how == "absolute" {
                outside.clone()
            } else {
                // From the link's directory up to the state directory's parent.
                let up = Path::new(at).components().count();
                PathBuf::from("../".repeat(up)).join("outside")
            };
            let mirror = below
                .iter()
                .fold(paths.state.join("outside"), |p, c| p.join(c));
            if mirrored {
                fs::create_dir_all(&mirror).unwrap();
            }
            let link = paths.state.join(at);
            fs::create_dir_all(link.parent().unwrap()).unwrap();
            symlink(&target, &link).unwrap();
            // The control: followed on the host, the link leads outside.
            assert_eq!(
                fs::canonicalize(&link).unwrap(),
                fs::canonicalize(&outside).unwrap(),
                "{case}"
            );
            if !at.starts_with("locks/") {
                // A canary of no entries there: refused, were it read.
                fs::create_dir(reached.join(&hex)).unwrap();
                for r in both_checks(&case, &paths, &maps) {
                    assert_eq!(r, Ok(()), "{case}: the canary outside was read");
                }
                fs::remove_dir(reached.join(&hex)).unwrap();
            }
            let before = tree(&outside);
            let (p, m) = (paths.clone(), maps.clone());
            let published = unblocked(&case, move || {
                give(Some(m));
                let store = Store::open(&p).unwrap();
                let r = layer(&store, 1).map(drop).map_err(|e| e.to_string());
                store.close().unwrap();
                r
            });
            assert_eq!(tree(&outside), before, "{case}: {published:?}");
            if mirrored {
                // Made where the link leads in the state directory.
                assert_eq!(published, Ok(()), "{case}");
                assert!(mirror.join(&hex).exists(), "{case}");
                let root = RootPath::open(&paths.state).unwrap();
                let found = canary::check(&root, &paths.state, &maps).unwrap();
                assert_eq!(found, canary::Found::Matches, "{case}");
            } else {
                let e = published.unwrap_err();
                assert!(!paths.state.join(key(1).path()).exists(), "{case}: {e}");
            }
        }
    }
}

/// Red team (T13): a FIFO, a socket or a whiteout as the canary, as each
/// of its entries, or as a directory above it is refused from outside and
/// inside without blocking, and changes nothing; in place of the canary's
/// lock it fails the layer, never blocking, and publishes nothing.
#[test]
fn red_team_a_special_file_as_a_canary_its_entries_or_its_lock_is_refused_unblocked() {
    let maps = own_maps();
    let rel = canary::path(&maps);
    for (what, make) in special_files() {
        let places: [Planted<'_, Plant<'_>>; 3] = [
            ("the canary", &|state| {
                fs::create_dir_all(state.join(&rel).parent().unwrap()).unwrap();
                make(&state.join(&rel)).unwrap();
            }),
            ("each entry", &|state| {
                fs::create_dir_all(state.join(&rel)).unwrap();
                for e in canary::entries(&maps) {
                    make(&state.join(&rel).join(&e.name)).unwrap();
                }
            }),
            ("the canaries' directory", &|state| {
                fs::create_dir_all(state.join("layers/v3")).unwrap();
                make(&state.join("layers/v3/canary")).unwrap();
            }),
        ];
        for (place, plant) in places {
            let case = format!("{what} as {place}");
            let (_tmp, paths) = fresh();
            plant(&paths.state);
            let before = tree(&paths.state);
            for r in both_checks(&case, &paths, &maps) {
                let e = r.unwrap_err();
                assert!(e.starts_with("changed: "), "{case}: {e}");
                assert!(e.contains("is not a"), "{case}: {e}");
                assert!(e.contains("ranikust prune --all"), "{case}: {e}");
            }
            assert_eq!(tree(&paths.state), before, "{case}");
        }
        let case = format!("{what} as the canary's lock");
        let (_tmp, paths) = fresh();
        let lock = paths.state.join("locks").join(&rel);
        fs::create_dir_all(lock.parent().unwrap()).unwrap();
        make(&lock).unwrap();
        let (p, m) = (paths.clone(), maps.clone());
        let e = unblocked(&case, move || {
            give(Some(m));
            let store = Store::open(&p).unwrap();
            let r = layer(&store, 1).map(drop);
            store.close().unwrap();
            r
        })
        .unwrap_err();
        assert!(!matches!(e, Error::IdMapChanged { .. }), "{case}: {e}");
        assert!(!paths.state.join(key(1).path()).exists(), "{case}: {e}");
        assert!(!paths.state.join(&rel).exists(), "{case}: {e}");
        let kind = fs::symlink_metadata(&lock).unwrap().file_type();
        assert!(!kind.is_file() && !kind.is_dir(), "{case}: {e}");
    }
}

/// Red team (T13): a symlink loop, a chain of links past the kernel's
/// limit, or a regular file at `layers/v3` or at the canaries' directory
/// refuses the state directory from outside and inside, without blocking,
/// and changes nothing.
#[test]
fn red_team_a_loop_or_a_file_above_the_canary_is_refused_unchanged() {
    let maps = own_maps();
    let cases: [Planted<'_, fn(&Path)>; 4] = [
        ("layers/v3 a link to itself", |state| {
            fs::create_dir_all(state.join("layers")).unwrap();
            symlink("v3", state.join("layers/v3")).unwrap();
        }),
        ("layers/v3 a regular file", |state| {
            fs::create_dir_all(state.join("layers")).unwrap();
            fs::write(state.join("layers/v3"), b"").unwrap();
        }),
        ("the canaries' directory a chain of 64 links", |state| {
            let v3 = state.join("layers/v3");
            fs::create_dir_all(v3.join("real")).unwrap();
            symlink("l0", v3.join("canary")).unwrap();
            for n in 0..64 {
                let next = if n == 63 {
                    "real".to_owned()
                } else {
                    format!("l{}", n + 1)
                };
                symlink(next, v3.join(format!("l{n}"))).unwrap();
            }
        }),
        ("the canary a link to itself", |state| {
            let canary = state.join(canary::path(&own_maps()));
            fs::create_dir_all(canary.parent().unwrap()).unwrap();
            symlink(canary.file_name().unwrap(), &canary).unwrap();
        }),
    ];
    for (case, plant) in cases {
        let (_tmp, paths) = fresh();
        plant(&paths.state);
        let before = tree(&paths.state);
        for r in both_checks(case, &paths, &maps) {
            let e = r.unwrap_err();
            assert!(e.starts_with("changed: "), "{case}: {e}");
            assert!(e.contains("is not a directory"), "{case}: {e}");
        }
        assert_eq!(tree(&paths.state), before, "{case}");
    }
}

/// Red team (T13): a canary, or the canaries' directory, that cannot be
/// searched refuses the state directory from outside and inside as a
/// mismatch, and changes nothing.
#[test]
fn red_team_an_unreadable_canary_is_refused_unchanged() {
    if rustix::process::geteuid().is_root() {
        eprintln!("skipped: root searches any directory");
        return;
    }
    let maps = own_maps();
    for locked in [canary::path(&maps), PathBuf::from("layers/v3/canary")] {
        let case = locked.display().to_string();
        let (_tmp, paths) = fresh();
        let store = {
            give(Some(maps.clone()));
            Store::open(&paths).unwrap()
        };
        layer(&store, 1).unwrap();
        store.close().unwrap();
        let before = tree(&paths.state);
        let dir = paths.state.join(&locked);
        let was = fs::symlink_metadata(&dir).unwrap().permissions();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o000)).unwrap();
        let checks = both_checks(&case, &paths, &maps);
        let mode = fs::symlink_metadata(&dir).unwrap().permissions().mode() & 0o7777;
        fs::set_permissions(&dir, was).unwrap();
        for r in checks {
            let e = r.unwrap_err();
            assert!(e.starts_with("changed: "), "{case}: {e}");
            assert!(e.contains("cannot be read"), "{case}: {e}");
        }
        assert_eq!(mode, 0, "{case}");
        assert_eq!(tree(&paths.state), before, "{case}");
    }
}

/// Red team (T13): a canary that is not as the maps give it, put there
/// after a store found none at its open (as a build under the same maps
/// from another enclosing range makes it), is found under the canary's
/// lock: a layer published meanwhile is not reused, the first layer of the
/// store is not published and its extraction is removed, and the canary is
/// left as it was.
#[test]
fn red_team_a_wrong_canary_made_after_the_open_refuses_reuse_and_publication() {
    let maps = own_maps();
    let rel = canary::path(&maps);
    let wrong: [Planted<'_, PlantCanary>; 2] = [
        ("with no entries", |c, _| fs::create_dir_all(c).unwrap()),
        ("with directories for entries", |c, maps| {
            for e in canary::entries(maps) {
                fs::create_dir_all(c.join(&e.name)).unwrap();
            }
        }),
    ];
    for (case, plant) in wrong {
        let (_tmp, paths) = fresh();
        give(Some(maps.clone()));
        let store = Store::open(&paths).unwrap();
        plant(&paths.state.join(&rel), &maps);
        // Published by another build, which took no canary's lock.
        give(None);
        let other = Store::open(&paths).unwrap();
        layer(&other, 2).unwrap();
        other.close().unwrap();
        let canary_before = tree(&paths.state.join(&rel));
        for e in [
            store.get(&key(2)).unwrap_err(),
            layer(&store, 1).unwrap_err(),
        ] {
            assert!(matches!(e, Error::IdMapChanged { .. }), "{case}: {e}");
        }
        assert!(!paths.state.join(key(1).path()).exists(), "{case}");
        let tmp = paths.state.join("tmp").join(store.build_id());
        let left = fs::read_dir(&tmp).map_or(0, Iterator::count);
        assert_eq!(left, 0, "{case}");
        assert_eq!(tree(&paths.state.join(&rel)), canary_before, "{case}");
        store.close().unwrap();
    }
}

/// Red team (T13): stores of one map that each opened with no canary and
/// publish their first layers at once, some the same layer, make exactly
/// one canary between them, which matches, and leave nothing half-made.
#[test]
fn red_team_stores_racing_for_the_first_layer_make_one_canary() {
    const STORES: u8 = 8;
    let maps = own_maps();
    for round in 0..10 {
        let (_tmp, paths) = fresh();
        let barrier = Arc::new(std::sync::Barrier::new(STORES.into()));
        let racers: Vec<_> = (0..STORES)
            .map(|n| {
                let (paths, maps, barrier) = (paths.clone(), maps.clone(), Arc::clone(&barrier));
                std::thread::spawn(move || {
                    give(Some(maps));
                    let store = Store::open(&paths).unwrap();
                    barrier.wait();
                    let r = layer(&store, n % 3 + 1)
                        .map(drop)
                        .map_err(|e| e.to_string());
                    store.close().unwrap();
                    r
                })
            })
            .collect();
        for r in racers {
            assert_eq!(r.join().unwrap(), Ok(()), "round {round}");
        }
        let canaries: Vec<_> = fs::read_dir(paths.state.join("layers/v3/canary"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            canaries,
            [canary::path(&maps).file_name().unwrap()],
            "round {round}"
        );
        let root = RootPath::open(&paths.state).unwrap();
        let found = canary::check(&root, &paths.state, &maps).unwrap();
        assert_eq!(found, canary::Found::Matches, "round {round}");
        for n in 1..=3 {
            assert!(paths.state.join(key(n).path()).is_dir(), "round {round}");
        }
        let tmp = paths.state.join("tmp");
        let left: Vec<_> = fs::read_dir(&tmp)
            .unwrap()
            .flat_map(|d| fs::read_dir(d.unwrap().path()).unwrap())
            .map(|e| e.unwrap().path())
            .collect();
        assert!(left.is_empty(), "round {round}: {left:?}");
    }
}
