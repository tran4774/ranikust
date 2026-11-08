//! Red team of the user namespace probe's directories in a state directory
//! (#79, Q-111): what a `RUN` in kaniko mode on `/`, or another process,
//! plants under the probe's name, and a `prune --all` racing builds that
//! hold the directory from their check to their store. Its own binary: the
//! race takes `flock`s across threads.

use std::fs;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use rnk_store::{BuildPaths, Error, STATE_MARKER, Store, USERNS_PROBE_PREFIX};
use rustix::fs::{Mode, OFlags};

/// Every path below `dir`, with its kind, contents and mode, sorted.
fn tree(dir: &Path) -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            let m = fs::symlink_metadata(&p).unwrap();
            let kind = if m.file_type().is_symlink() {
                format!("link {}", fs::read_link(&p).unwrap().display())
            } else if m.is_dir() {
                stack.push(p.clone());
                "dir".to_owned()
            } else {
                format!("file {:?} {:o}", fs::read(&p).unwrap(), m.mode() & 0o7777)
            };
            out.push((p.strip_prefix(dir).unwrap().to_path_buf(), kind));
        }
    }
    out.sort();
    out
}

/// A probe directory a `RUN` planted with what a remover must neither
/// follow, open nor be stopped by: links out by every route, a hard link
/// to a file outside, a FIFO and a socket (which block or fail an `open`),
/// a tree 3000 levels deep, and directories without write or search
/// permission. `Store::open` takes the directory as a new store, removes
/// the planted one without blocking, and leaves everything outside as it
/// was (Q-111; T4, T21).
#[test]
fn red_team_a_planted_probe_directory_goes_without_following_or_blocking() {
    let top = tempfile::tempdir().unwrap();
    let outside = top.path().join("outside");
    fs::create_dir_all(outside.join("sub")).unwrap();
    fs::write(outside.join("sub/f"), b"keep").unwrap();
    fs::write(outside.join("own"), b"keep").unwrap();
    let state = top.path().join("state");
    fs::create_dir(&state).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
    let probe = state.join(format!("{USERNS_PROBE_PREFIX}1-1"));
    fs::create_dir(&probe).unwrap();
    let ln = |to: &Path, at: &str| std::os::unix::fs::symlink(to, probe.join(at)).unwrap();
    ln(&outside, "dir");
    ln(&outside.join("own"), "file");
    ln(Path::new("../../outside/sub"), "dotdot");
    ln(Path::new("/"), "root");
    fs::hard_link(outside.join("own"), probe.join("hard")).unwrap();
    rustix::fs::mknodat(
        rustix::fs::CWD,
        probe.join("fifo"),
        rustix::fs::FileType::Fifo,
        Mode::from_raw_mode(0o600),
        0,
    )
    .unwrap();
    drop(std::os::unix::net::UnixListener::bind(probe.join("sock")).unwrap());
    // Deeper than PATH_MAX: made through descriptors.
    let mut at = rustix::fs::open(&probe, OFlags::DIRECTORY, Mode::empty()).unwrap();
    for name in std::iter::once("deep").chain(std::iter::repeat_n("d", 3000)) {
        rustix::fs::mkdirat(&at, name, Mode::from_raw_mode(0o700)).unwrap();
        at = rustix::fs::openat(&at, name, OFlags::DIRECTORY, Mode::empty()).unwrap();
    }
    rustix::fs::symlinkat(&outside, &at, "out").unwrap();
    drop(at);
    for (name, mode) in [("no-write", 0o500), ("no-search", 0o600), ("none", 0o000)] {
        let d = probe.join(name);
        fs::create_dir(&d).unwrap();
        fs::write(d.join("f"), b"x").unwrap();
        std::os::unix::fs::symlink(&outside, d.join("out")).unwrap();
        fs::set_permissions(&d, fs::Permissions::from_mode(mode)).unwrap();
    }
    let before = tree(&outside);

    // A remover that opened the FIFO would block: give it a minute.
    let (tx, rx) = mpsc::channel();
    let at = state.clone();
    thread::spawn(move || {
        let opened = Store::open(&BuildPaths::new(&at, &at)).map(Store::close);
        tx.send(opened.map(|closed| closed.is_ok())).unwrap();
    });
    let opened = rx
        .recv_timeout(Duration::from_secs(60))
        .expect("Store::open blocked on the planted directory");
    assert!(opened.unwrap(), "the store opened and closed");
    assert!(state.join(STATE_MARKER).is_file());
    assert_eq!(tree(&outside), before, "nothing outside changed");
    let own_links = fs::metadata(outside.join("own")).unwrap().nlink();
    // Directories without search or write permission may keep it from a
    // user, which only warns: what it must never do is reach outside. The
    // removal stops at the first of them, so what it reached before depends
    // on the order the filesystem lists the directory in.
    if probe.exists() {
        assert!(!rustix::process::geteuid().is_root(), "root left {probe:?}");
        let hard_left = fs::symlink_metadata(probe.join("hard")).is_ok();
        assert_eq!(
            own_links,
            1 + u64::from(hard_left),
            "hard link left: {hard_left}"
        );
        eprintln!("measured: a user leaves the planted directory in part");
        for name in ["no-write", "no-search", "none"] {
            let _ = fs::set_permissions(probe.join(name), fs::Permissions::from_mode(0o700));
        }
    } else {
        assert_eq!(own_links, 1);
        eprintln!("measured: the planted directory is removed whole");
    }
}

/// `prune --all` racing builds that each check the state directory, write
/// a probe directory there under its own `flock`, open their store and use
/// it: no prune ever empties a store a build holds, from its check to its
/// close, nor removes a probe directory in use (Q-111, (a); ADR-0105; T4).
/// Each side must win some rounds, or the race proved nothing.
#[test]
fn red_team_a_prune_racing_builds_never_empties_what_a_build_holds() {
    let top = tempfile::tempdir().unwrap();
    let state = top.path().join("state");
    fs::create_dir(&state).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let pruned = Arc::new(AtomicUsize::new(0));
    let built = Arc::new(AtomicUsize::new(0));
    let pruner = {
        let (state, stop, pruned) = (state.clone(), stop.clone(), pruned.clone());
        thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match Store::prune_all(&state) {
                    Ok(true) => {
                        pruned.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(false) | Err(Error::InUse { .. } | Error::Busy { .. }) => {}
                    Err(e) => panic!("prune: {e}"),
                }
                thread::yield_now();
            }
        })
    };
    let builders: Vec<_> = (0..2)
        .map(|n| {
            let (state, built) = (state.clone(), built.clone());
            thread::spawn(move || {
                for round in 0..150 {
                    let hold = match Store::check_state_dir(&state) {
                        Ok(hold) => hold,
                        Err(Error::Busy { .. }) => continue,
                        Err(e) => panic!("check {n}/{round}: {e}"),
                    };
                    let probe = state.join(format!("{USERNS_PROBE_PREFIX}{n}-{round}"));
                    fs::create_dir(&probe).unwrap();
                    let lock = fs::File::open(&probe).unwrap();
                    rustix::fs::flock(&lock, rustix::fs::FlockOperation::LockExclusive).unwrap();
                    fs::write(probe.join("f"), b"probe").unwrap();
                    thread::yield_now();
                    assert!(
                        probe.join("f").is_file(),
                        "{n}/{round}: the probe's file went"
                    );
                    fs::remove_file(probe.join("f")).unwrap();
                    fs::remove_dir(&probe).unwrap();
                    drop(lock);
                    let store = match Store::open(&BuildPaths::new(&state, &state)) {
                        Ok(store) => store,
                        Err(e) => panic!("open {n}/{round} while held: {e}"),
                    };
                    drop(hold);
                    let alive = store.build_dir().join("alive");
                    fs::write(&alive, b"x").unwrap();
                    thread::yield_now();
                    assert!(alive.is_file(), "{n}/{round}: a prune emptied a held store");
                    assert!(
                        state.join(STATE_MARKER).is_file(),
                        "{n}/{round}: the marker went"
                    );
                    store.close().unwrap();
                    built.fetch_add(1, Ordering::Relaxed);
                    // A window for the prune between builds.
                    thread::sleep(Duration::from_micros(300));
                }
            })
        })
        .collect();
    for b in builders {
        b.join().unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    pruner.join().unwrap();
    let (p, b) = (
        pruned.load(Ordering::Relaxed),
        built.load(Ordering::Relaxed),
    );
    eprintln!("measured: {b} builds and {p} prunes");
    assert!(p > 0 && b > 0, "{b} builds, {p} prunes: the race never ran");
}
