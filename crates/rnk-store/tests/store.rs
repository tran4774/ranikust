//! The store at tier T0: layout, marker, publication, per-digest locks
//! between threads and between processes, and start-up clean-up.

use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

use rnk_oci::Digest;
use rnk_store::{BuildPaths, CacheSharing, EntryKey, Error, Store, TmpEntry};

fn digest(n: u8) -> Digest {
    Digest::from_sha256_bytes(&[n; 32])
}

fn paths(dir: &Path) -> BuildPaths {
    BuildPaths::new(dir, dir.join(".ranikust"))
}

/// Writes `data` into a blob entry and returns its length.
fn write_blob(e: &TmpEntry, data: &[u8]) -> io::Result<u64> {
    let mut f = e.file().expect("a blob entry is a file");
    f.write_all(data)?;
    Ok(u64::try_from(data.len()).unwrap())
}

fn names(dir: impl AsRef<Path>) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    v.sort();
    v
}

#[test]
fn open_creates_the_layout_and_the_marker() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&paths(dir.path())).unwrap();
    let state = dir.path().join(".ranikust");
    assert_eq!(store.state_dir(), fs::canonicalize(&state).unwrap());
    assert!(state.join(".ranikust-state").is_file());
    for d in ["blobs", "layers", "caches", "tmp", "builds", "locks"] {
        assert!(state.join(d).is_dir(), "{d}");
    }
    assert!(store.build_dir().is_dir());
    assert_eq!(store.build_dir().parent().unwrap(), state.join("builds"));
}

/// ADR-0099: a setgid state directory, as a shared directory makes it,
/// gives its gid and the bit to no directory of the store, so what the
/// store makes has the gid of the process that makes it; nor does a store
/// directory of another gid keep it. The gid is another group of this
/// user's, or any other for root; for a user of one group only the bit is
/// checked. One already the process's is left unchanged.
#[test]
fn a_setgid_state_directory_passes_its_gid_to_no_store_directory() {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    let egid = rustix::process::getegid().as_raw();
    // Another group the user is in; root may take any.
    let group = rustix::process::getgroups()
        .unwrap()
        .into_iter()
        .map(rustix::process::Gid::as_raw)
        .find(|&g| g != egid)
        .or_else(|| rustix::process::geteuid().is_root().then_some(egid ^ 1))
        .unwrap_or(egid);
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join(".ranikust");
    fs::create_dir(&state).unwrap();
    match std::os::unix::fs::chown(&state, None, Some(group)) {
        Err(e) if e.raw_os_error() == Some(rustix::io::Errno::INVAL.raw_os_error()) => {
            eprintln!("skipped: that group is not mapped here");
            return;
        }
        r => r.unwrap(),
    }
    fs::set_permissions(&state, fs::Permissions::from_mode(0o2700)).unwrap();
    // The control: the kernel gives a new directory there the gid and bit.
    fs::create_dir(state.join("probe")).unwrap();
    let probe = fs::metadata(state.join("probe")).unwrap();
    assert_eq!((probe.gid(), probe.mode() & 0o2000), (group, 0o2000));
    fs::remove_dir(state.join("probe")).unwrap();

    let store = Store::open(&paths(dir.path())).unwrap();
    let layer = store
        .fill(
            &EntryKey::Layer(digest(1)),
            |e| -> rnk_rootfs::Result<u64> {
                e.root().unwrap().create_dir_all("etc", 0o755)?;
                Ok(0)
            },
        )
        .unwrap();
    let mut made: Vec<PathBuf> = ["blobs", "layers", "caches", "locks", "tmp", "builds"]
        .iter()
        .map(|d| state.join(d))
        .collect();
    made.extend([
        store.build_dir().to_owned(),
        layer.parent().unwrap().to_owned(),
        layer.clone(),
        layer.join("etc"),
    ]);
    for d in made {
        let m = fs::metadata(&d).unwrap();
        assert_eq!((m.gid(), m.mode() & 0o2000), (egid, 0), "{}", d.display());
    }
    // The state directory itself is left as it is.
    let m = fs::metadata(&state).unwrap();
    assert_eq!((m.gid(), m.mode() & 0o7777), (group, 0o2700));
    // A store directory already the process's is not changed again: its
    // ctime stays, once a change would get a later one.
    drop(store);
    let ctime = |p: &Path| {
        let m = fs::metadata(p).unwrap();
        (m.ctime(), m.ctime_nsec())
    };
    let before = ctime(&state.join("layers"));
    let control = dir.path().join("control");
    fs::write(&control, b"").unwrap();
    for mode in [0o600, 0o644].into_iter().cycle().take(1000) {
        if ctime(&control) > before {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
        fs::set_permissions(&control, fs::Permissions::from_mode(mode)).unwrap();
    }
    assert!(ctime(&control) > before, "the ctime clock never moved");
    let store = Store::open(&paths(dir.path())).unwrap();
    assert_eq!(ctime(&state.join("layers")), before);

    // A store directory of another gid without the bit, as a `grpid`
    // mount gives one, takes the effective gid at the next open.
    if group == egid {
        eprintln!("skipped: the gid checks (the user has one group)");
        return;
    }
    drop(store);
    let tmp = state.join("tmp");
    std::os::unix::fs::chown(&tmp, None, Some(group)).unwrap();
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o700)).unwrap();
    let _store = Store::open(&paths(dir.path())).unwrap();
    let m = fs::metadata(&tmp).unwrap();
    assert_eq!((m.gid(), m.mode() & 0o7777), (egid, 0o700));
}

#[test]
fn open_refuses_a_non_empty_directory_without_the_marker() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("precious"), "x").unwrap();
    let err = Store::open(&BuildPaths::new(dir.path(), dir.path())).unwrap_err();
    assert!(matches!(err, Error::NotAStateDir { .. }), "{err}");
    assert_eq!(names(dir.path()), ["precious"]);
}

/// The check before a write in the state directory decides as `open` does,
/// and writes nothing.
#[test]
fn check_state_dir_decides_as_open_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let left = format!("{}1-2", rnk_store::USERNS_PROBE_PREFIX);
    let case = |name: &str| {
        let d = dir.path().join(name);
        fs::create_dir(&d).unwrap();
        d
    };
    let empty = case("empty");
    drop(Store::check_state_dir(&empty).unwrap());
    assert!(names(&empty).is_empty());
    let leftovers = case("leftovers");
    fs::create_dir(leftovers.join(&left)).unwrap();
    drop(Store::check_state_dir(&leftovers).unwrap());
    assert_eq!(names(&leftovers), [left.as_str()]);
    let marked = case("marked");
    fs::write(marked.join(rnk_store::STATE_MARKER), "").unwrap();
    fs::write(marked.join("kept"), "x").unwrap();
    drop(Store::check_state_dir(&marked).unwrap());
    assert_eq!(names(&marked), [rnk_store::STATE_MARKER, "kept"]);
    let foreign = case("foreign");
    fs::write(foreign.join("precious"), "x").unwrap();
    let err = Store::check_state_dir(&foreign).unwrap_err();
    assert!(matches!(err, Error::NotAStateDir { .. }), "{err}");
    assert_eq!(names(&foreign), ["precious"]);
    let not_a_file = case("not-a-file");
    fs::create_dir(not_a_file.join(rnk_store::STATE_MARKER)).unwrap();
    let err = Store::check_state_dir(&not_a_file).unwrap_err();
    assert!(matches!(err, Error::NotAStateDir { .. }), "{err}");
    let slash = case("slash");
    fs::write(slash.join(rnk_store::BUILDING_MARKER), "").unwrap();
    let err = Store::check_state_dir(&slash).unwrap_err();
    assert!(matches!(err, Error::UsedOnSlash { .. }), "{err}");
    assert_eq!(names(&slash), [rnk_store::BUILDING_MARKER]);
    // An anonymous store holds its state directory alone.
    let held = case("held");
    let store = Store::open_anonymous(&held).unwrap();
    let err = Store::check_state_dir(&held).unwrap_err();
    assert!(matches!(err, Error::Busy { .. }), "{err}");
    store.close().unwrap();
    let err = Store::check_state_dir(&dir.path().join("missing")).unwrap_err();
    assert!(matches!(err, Error::Io { .. }), "{err}");
}

/// What `check_state_dir` returns holds the directory until it is dropped:
/// a store opens beside it, and nothing that needs the directory alone (a
/// prune, an anonymous store) runs until then.
#[test]
fn check_state_dir_holds_the_directory_until_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path();
    let hold = Store::check_state_dir(state).unwrap();
    let err = Store::prune_all(state).unwrap_err();
    assert!(matches!(err, Error::InUse { .. }), "{err}");
    let err = Store::open_anonymous(state).unwrap_err();
    assert!(matches!(err, Error::Busy { .. }), "{err}");
    let store = Store::open(&BuildPaths::new(state, state)).unwrap();
    drop(hold);
    let err = Store::prune_all(state).unwrap_err();
    assert!(matches!(err, Error::InUse { .. }), "{err}");
    store.close().unwrap();
    assert!(Store::prune_all(state).unwrap());
}

/// Each of the store's directories, replaced by a symlink that stays in
/// the state directory or by a file, is refused at open, named, with
/// nothing changed; `check_state_dir` refuses it too. The control: the
/// layout as the store made it opens.
#[test]
fn a_store_directory_that_is_not_a_real_directory_is_refused_unchanged() {
    for d in ["blobs", "layers", "caches", "locks", "tmp", "builds"] {
        for plant in ["a symlink", "a regular file"] {
            let dir = tempfile::tempdir().unwrap();
            let state = dir.path().join(".ranikust");
            Store::open(&paths(dir.path())).unwrap().close().unwrap();
            let moved = format!("{d}.moved");
            fs::rename(state.join(d), state.join(&moved)).unwrap();
            if plant == "a symlink" {
                std::os::unix::fs::symlink(&moved, state.join(d)).unwrap();
            } else {
                fs::write(state.join(d), b"").unwrap();
            }
            let case = format!("{d} as {plant}");
            let before = tree(&state);
            let refused = |e: Error| {
                let shown = e.to_string();
                assert!(matches!(e, Error::NotAStateDir { .. }), "{case}: {shown}");
                assert!(
                    shown.contains(&format!("holds {d}, which is {plant}")),
                    "{case}: {shown}"
                );
            };
            refused(Store::open(&paths(dir.path())).expect_err(&case));
            assert_eq!(tree(&state), before, "{case}");
            refused(Store::check_state_dir(&state).expect_err(&case));
            fs::remove_file(state.join(d)).unwrap();
            fs::rename(state.join(&moved), state.join(d)).unwrap();
            Store::open(&paths(dir.path())).unwrap().close().unwrap();
        }
    }
}

/// A probe directory of a build cut short before its store opened is
/// ranikust's own: the directory is still a new store, and the store
/// removes it once open.
#[test]
fn open_marks_a_directory_holding_only_ranikusts_leftovers() {
    let dir = tempfile::tempdir().unwrap();
    let at = |d: &Path| BuildPaths::new(d, d);
    let left = format!("{}1-2", rnk_store::USERNS_PROBE_PREFIX);
    fs::create_dir_all(dir.path().join(&left).join("overlay")).unwrap();
    fs::write(dir.path().join(&left).join("overlay/f"), "x").unwrap();
    Store::open(&at(dir.path())).unwrap().close().unwrap();
    assert!(dir.path().join(rnk_store::STATE_MARKER).is_file());
    assert!(!dir.path().join(&left).exists());
    // Anything else still makes it foreign.
    let other = tempfile::tempdir().unwrap();
    fs::create_dir(other.path().join(&left)).unwrap();
    fs::write(other.path().join("precious"), "x").unwrap();
    let err = Store::open(&at(other.path())).unwrap_err();
    assert!(matches!(err, Error::NotAStateDir { .. }), "{err}");
}

/// Only directories by the probe's name stand in for the marker: another
/// name of ranikust's, or the probe's name on a file or on a symlink to a
/// directory, makes a directory without the marker foreign, with nothing
/// written there or where the symlink points.
#[test]
fn only_the_probes_own_directories_stand_in_for_the_marker() {
    use std::os::unix::fs::symlink;
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("f"), "x").unwrap();
    let probe = format!("{}1-2", rnk_store::USERNS_PROBE_PREFIX);
    let other = format!("{}00ff", rnk_store::ANON_PREFIX);
    for what in ["another name", "a file", "a symlink"] {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        match what {
            "another name" => fs::create_dir(d.join(&other)).unwrap(),
            "a file" => fs::write(d.join(&probe), "x").unwrap(),
            _ => symlink(outside.path(), d.join(&probe)).unwrap(),
        }
        let before = names(d);
        let err = Store::check_state_dir(d).unwrap_err();
        assert!(matches!(err, Error::NotAStateDir { .. }), "{what}: {err}");
        let err = Store::open(&BuildPaths::new(d, d)).unwrap_err();
        assert!(matches!(err, Error::NotAStateDir { .. }), "{what}: {err}");
        assert_eq!(names(d), before, "{what}");
    }
    assert_eq!(names(outside.path()), ["f"]);
}

/// A store removes a probe directory only where it takes the directory's
/// lock, which a probe holds while it uses it; in a marked directory, the
/// probe's name on a symlink is left, and what it points to untouched.
#[test]
fn open_removes_only_the_probe_directories_no_probe_holds() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let outside = dir.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("f"), "x").unwrap();
    let at = BuildPaths::new(&state, &state);
    Store::open(&at).unwrap().close().unwrap();
    let name = |n: &str| format!("{}{n}", rnk_store::USERNS_PROBE_PREFIX);
    let (used, left, link) = (name("1-1"), name("1-2"), name("1-3"));
    fs::create_dir(state.join(&used)).unwrap();
    fs::create_dir(state.join(&left)).unwrap();
    symlink("../../outside/f", state.join(&left).join("out")).unwrap();
    symlink(&outside, state.join(&link)).unwrap();
    let held = fs::File::open(state.join(&used)).unwrap();
    rustix::fs::flock(&held, rustix::fs::FlockOperation::NonBlockingLockExclusive).unwrap();
    let store = Store::open(&at).unwrap();
    assert!(state.join(&used).is_dir(), "a probe's directory in use");
    assert!(
        !state.join(&left).exists(),
        "a probe's directory no probe holds"
    );
    assert!(state.join(&link).is_symlink());
    assert_eq!(names(&outside), ["f"]);
    store.close().unwrap();
    drop(held);
    Store::open(&at).unwrap().close().unwrap();
    assert!(!state.join(&used).exists(), "let go");
}

#[test]
fn open_refuses_a_directory_with_the_old_workdir_marker_only() {
    // An old kaniko-mode workdir (or an image's `/`) marked `.ranikust-root`
    // is not a store: its `tmp/` and `builds/` are never cleaned up.
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join(".ranikust-root"), "").unwrap();
    fs::create_dir(dir.path().join("builds")).unwrap();
    fs::write(dir.path().join("builds/keep"), "x").unwrap();
    let err = Store::open(&BuildPaths::new(dir.path(), dir.path())).unwrap_err();
    assert!(matches!(err, Error::NotAStateDir { .. }), "{err}");
    assert!(err.to_string().contains(".ranikust-root"), "{err}");
    assert!(dir.path().join("builds/keep").is_file());
}

#[test]
fn open_refuses_a_marker_that_is_not_a_regular_file() {
    let dir = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink("/etc/hostname", dir.path().join(".ranikust-state")).unwrap();
    let err = Store::open(&BuildPaths::new(dir.path(), dir.path())).unwrap_err();
    assert!(matches!(err, Error::NotAStateDir { .. }), "{err}");
}

#[test]
fn open_accepts_an_existing_store() {
    let dir = tempfile::tempdir().unwrap();
    Store::open(&paths(dir.path())).unwrap().close().unwrap();
    Store::open(&paths(dir.path())).unwrap();
}

#[test]
fn a_blob_is_published_once() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&paths(dir.path())).unwrap();
    let key = EntryKey::Blob(digest(1));
    assert_eq!(store.get(&key).unwrap(), None);

    let p = store.fill(&key, |e| write_blob(e, b"hello")).unwrap();
    assert_eq!(fs::read(&p).unwrap(), b"hello");
    assert_eq!(
        p,
        store
            .state_dir()
            .join("blobs/sha256")
            .join(digest(1).encoded())
    );
    assert_eq!(store.get(&key).unwrap(), Some(p.clone()));

    let again = store
        .fill(&key, |_| -> io::Result<u64> { panic!("filled twice") })
        .unwrap();
    assert_eq!(again, p);
    assert!(names(store.state_dir().join("tmp").join(store.build_id())).is_empty());
}

#[test]
fn a_layer_is_published_as_a_directory() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&paths(dir.path())).unwrap();
    let key = EntryKey::Layer(digest(2));
    let p = store
        .fill(&key, |e| -> rnk_rootfs::Result<u64> {
            let root = e.root().expect("a layer entry is a directory");
            root.create_dir_all("etc", 0o755)?;
            root.symlink("../outside", "etc/link")?;
            Ok(0)
        })
        .unwrap();
    assert_eq!(
        p,
        store
            .state_dir()
            .join("layers/v3/sha256")
            .join(digest(2).encoded())
    );
    assert!(p.join("etc").is_dir());
    assert_eq!(
        fs::read_link(p.join("etc/link")).unwrap(),
        Path::new("../outside")
    );
}

#[test]
fn layers_of_the_first_layout_are_not_reused() {
    // Before chain IDs (ADR-0032, Q-150) an upper layer was extracted under
    // its diff ID in `layers/<algorithm>/`; that extraction may hold
    // content copied from another stack.
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&paths(dir.path())).unwrap();
    let old = store
        .state_dir()
        .join("layers/sha256")
        .join(digest(7).encoded());
    fs::create_dir_all(old.join("etc")).unwrap();
    let key = EntryKey::Layer(digest(7));
    assert_eq!(store.get(&key).unwrap(), None);
    let filled = AtomicUsize::new(0);
    let p = store
        .fill(&key, |_| -> rnk_rootfs::Result<u64> {
            filled.fetch_add(1, Ordering::SeqCst);
            Ok(0)
        })
        .unwrap();
    assert_eq!(filled.load(Ordering::SeqCst), 1);
    assert_ne!(p, old);
    assert!(!p.join("etc").exists());
}

#[test]
fn a_failing_fill_publishes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&paths(dir.path())).unwrap();
    for key in [EntryKey::Blob(digest(3)), EntryKey::Layer(digest(3))] {
        let err = store
            .fill(&key, |e| {
                if let Some(mut f) = e.file() {
                    f.write_all(b"partial")?;
                } else {
                    e.root()
                        .unwrap()
                        .create_dir("half", 0o755)
                        .map_err(io::Error::other)?;
                }
                Err::<u64, _>(io::Error::other("digest mismatch"))
            })
            .unwrap_err();
        assert!(matches!(err, Error::Fill { .. }), "{err}");
        assert!(err.to_string().contains("digest mismatch"), "{err}");
        assert_eq!(store.get(&key).unwrap(), None);
        assert!(names(store.state_dir().join("tmp").join(store.build_id())).is_empty());
    }
}

#[test]
fn concurrent_fills_from_threads_run_the_filler_once() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(&paths(dir.path())).unwrap());
    let calls = Arc::new(AtomicUsize::new(0));
    let n = 8;
    let barrier = Arc::new(Barrier::new(n));
    let handles: Vec<_> = (0..n)
        .map(|_| {
            let (store, calls, barrier) = (store.clone(), calls.clone(), barrier.clone());
            thread::spawn(move || {
                barrier.wait();
                store
                    .fill(&EntryKey::Blob(digest(4)), |e| {
                        calls.fetch_add(1, Ordering::SeqCst);
                        thread::sleep(Duration::from_millis(50));
                        write_blob(e, b"once")
                    })
                    .unwrap()
            })
        })
        .collect();
    let got: Vec<PathBuf> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(got.iter().all(|p| p == &got[0]));
    assert_eq!(fs::read(&got[0]).unwrap(), b"once");
}

#[test]
fn open_removes_the_leftovers_of_a_dead_build() {
    let dir = tempfile::tempdir().unwrap();
    let dead = Store::open(&paths(dir.path())).unwrap();
    let id = dead.build_id().to_owned();
    let state = dead.state_dir().to_owned();
    fs::create_dir_all(dead.build_dir().join("stages/0/rootfs/deep")).unwrap();
    // Fills in flight when the build was killed.
    fs::create_dir_all(state.join("tmp").join(&id).join("0/usr/lib")).unwrap();
    fs::write(state.join("tmp").join(&id).join("1"), "half a blob").unwrap();
    drop(dead);

    let next = Store::open(&paths(dir.path())).unwrap();
    assert!(!state.join("builds").join(&id).exists());
    assert!(!state.join("tmp").join(&id).exists());
    assert!(!state.join("builds").join(format!("{id}.lock")).exists());
    assert_eq!(names(state.join("builds")), names_of_live(&next));
}

fn names_of_live(store: &Store) -> Vec<String> {
    let id = store.build_id();
    let mut v = vec![id.to_owned(), format!("{id}.lock")];
    v.sort();
    v
}

#[test]
fn open_keeps_a_running_build() {
    let dir = tempfile::tempdir().unwrap();
    let running = Store::open(&paths(dir.path())).unwrap();
    fs::write(running.build_dir().join("work"), "busy").unwrap();
    let other = Store::open(&paths(dir.path())).unwrap();
    assert_ne!(running.build_id(), other.build_id());
    assert_eq!(fs::read(running.build_dir().join("work")).unwrap(), b"busy");
}

#[test]
fn close_removes_the_build_directory() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&paths(dir.path())).unwrap();
    let build = store.build_dir().to_owned();
    fs::create_dir_all(build.join("stages/0/diff-1/a")).unwrap();
    let state = store.state_dir().to_owned();
    store.close().unwrap();
    assert!(!build.exists());
    assert!(names(state.join("builds")).is_empty());
    assert!(names(state.join("tmp")).is_empty());
}

#[test]
fn cache_dirs_are_named_by_the_hash_of_their_id() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&paths(dir.path())).unwrap();
    let a = store.cache_dir("/root/.npm").unwrap();
    assert!(a.is_dir());
    assert_eq!(a, store.cache_dir("/root/.npm").unwrap());
    assert_ne!(a, store.cache_dir("/root/.cache").unwrap());
    // sha256("/root/.npm")
    assert_eq!(a.parent().unwrap(), store.state_dir().join("caches"));
    assert_eq!(a.file_name().unwrap().len(), 64);
    let b = store.cache_dir("../../etc").unwrap();
    assert_eq!(b.parent().unwrap(), store.state_dir().join("caches"));
}

fn never() -> bool {
    false
}

#[test]
fn shared_cache_leases_use_one_directory() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&paths(dir.path())).unwrap();
    let a = store
        .cache_lease("npm", CacheSharing::Shared, None, &never)
        .unwrap();
    assert!(a.created());
    let b = store
        .cache_lease("npm", CacheSharing::Shared, None, &never)
        .unwrap();
    assert!(!b.created());
    assert_eq!(a.path(), b.path());
    assert_eq!(a.path(), store.cache_dir("npm").unwrap());
    drop((a, b));
    let c = store
        .cache_lease("npm", CacheSharing::Shared, None, &never)
        .unwrap();
    assert!(!c.created(), "kept between builds");
}

#[test]
fn a_private_cache_lease_gets_a_new_directory_while_one_is_held() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&paths(dir.path())).unwrap();
    let a = store
        .cache_lease("k", CacheSharing::Private, None, &never)
        .unwrap();
    fs::write(a.path().join("x"), b"x").unwrap();
    let b = store
        .cache_lease("k", CacheSharing::Private, None, &never)
        .unwrap();
    assert!(b.created());
    assert_ne!(a.path(), b.path());
    assert!(names(b.path()).is_empty(), "a new cache, not a copy");
    // A shared user beside a private holder gets another directory too.
    let s = store
        .cache_lease("k", CacheSharing::Shared, None, &never)
        .unwrap();
    assert_ne!(s.path(), a.path());
    assert_ne!(s.path(), b.path());
    let a_path = a.path().to_owned();
    drop((a, b, s));
    let again = store
        .cache_lease("k", CacheSharing::Private, None, &never)
        .unwrap();
    assert_eq!(again.path(), a_path, "a free directory is reused");
    assert!(!again.created());
}

#[test]
fn a_locked_cache_lease_waits_for_the_holder() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&paths(dir.path())).unwrap();
    let held = store
        .cache_lease("k", CacheSharing::Locked, None, &never)
        .unwrap();
    let path = held.path().to_owned();
    let start = std::time::Instant::now();
    thread::scope(|s| {
        s.spawn(|| {
            thread::sleep(Duration::from_millis(300));
            drop(held);
        });
        let next = store
            .cache_lease("k", CacheSharing::Locked, None, &never)
            .unwrap();
        assert_eq!(next.path(), path, "the same cache, after the holder");
        assert!(!next.created());
    });
    assert!(start.elapsed() >= Duration::from_millis(250));
    // A cancel ends the wait.
    let _held = store
        .cache_lease("k", CacheSharing::Locked, None, &never)
        .unwrap();
    let r = store.cache_lease("k", CacheSharing::Locked, None, &|| true);
    assert!(matches!(r, Err(Error::Cancelled)), "{r:?}");
}

#[test]
fn a_cache_lease_refuses_a_planted_entry() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&paths(dir.path())).unwrap();
    let path = store.cache_dir("k").unwrap();
    fs::remove_dir(&path).unwrap();
    std::os::unix::fs::symlink("/", &path).unwrap();
    let r = store.cache_lease("k", CacheSharing::Shared, None, &never);
    assert!(matches!(r, Err(Error::BadEntry { .. })), "{r:?}");
}

#[test]
fn entry_keys_need_their_algorithm_directory() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&paths(dir.path())).unwrap();
    let key = EntryKey::Blob(Digest::from_sha512_bytes(&[7; 64]));
    let p = store.fill(&key, |e| write_blob(e, b"512")).unwrap();
    assert_eq!(p.parent().unwrap(), store.state_dir().join("blobs/sha512"));
}

#[test]
fn a_new_state_directory_is_private() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&paths(dir.path())).unwrap();
    let mode = fs::metadata(store.state_dir())
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o077, 0, "{mode:o}");
}

#[test]
fn a_symlink_planted_as_an_entry_is_refused_not_followed() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&paths(dir.path())).unwrap();
    let state = store.state_dir().to_owned();
    let outside = dir.path().join("outside");
    fs::write(&outside, "host").unwrap();
    for key in [EntryKey::Blob(digest(8)), EntryKey::Layer(digest(8))] {
        let kind = if matches!(key, EntryKey::Blob(_)) {
            "blobs"
        } else {
            "layers/v3"
        };
        let p = state.join(kind).join("sha256").join(digest(8).encoded());
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&outside, &p).unwrap();
        let err = store.get(&key).unwrap_err();
        assert!(matches!(err, Error::BadEntry { .. }), "{err}");
        let err = store
            .fill(&key, |_| -> io::Result<u64> {
                panic!("filled over a symlink")
            })
            .unwrap_err();
        assert!(matches!(err, Error::BadEntry { .. }), "{err}");
        assert!(fs::symlink_metadata(&p).unwrap().is_symlink());
    }
    assert_eq!(fs::read(&outside).unwrap(), b"host");
}

/// Layers extracted under an earlier layout are never reused: version 2
/// may hold `trusted.*` xattrs from the image, which extraction no longer
/// sets (`ADR-0075`); version 1 named layers by diff ID.
#[test]
fn a_layer_of_an_earlier_layout_is_not_reused() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&paths(dir.path())).unwrap();
    for old in ["layers/sha256", "layers/v2/sha256"] {
        let p = store.state_dir().join(old).join(digest(10).encoded());
        fs::create_dir_all(p.join("etc")).unwrap();
    }
    assert_eq!(store.get(&EntryKey::Layer(digest(10))).unwrap(), None);
    let p = store
        .fill(&EntryKey::Layer(digest(10)), |_| -> io::Result<u64> {
            Ok(0)
        })
        .unwrap();
    assert!(
        p.starts_with(store.state_dir().join("layers/v3/sha256")),
        "{p:?}"
    );
}

#[test]
fn a_layer_of_the_wrong_type_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&paths(dir.path())).unwrap();
    let p = store
        .state_dir()
        .join("layers/v3/sha256")
        .join(digest(9).encoded());
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(&p, "not a directory").unwrap();
    let err = store.get(&EntryKey::Layer(digest(9))).unwrap_err();
    assert!(matches!(err, Error::BadEntry { .. }), "{err}");
}

/// Makes a FIFO at `p`, in this process: a spawned `mkfifo` would hold a
/// copy of every lock until it execs (see `tests/processes.rs`).
fn mkfifo(p: &Path) {
    use rustix::fs::{CWD, FileType, Mode, mknodat};
    mknodat(CWD, p, FileType::Fifo, Mode::from_raw_mode(0o600), 0).unwrap();
}

#[test]
fn a_fifo_planted_as_a_lock_file_is_refused_without_blocking() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(&paths(dir.path())).unwrap());
    let lock = store
        .state_dir()
        .join("locks/blobs/sha256")
        .join(digest(10).encoded());
    fs::create_dir_all(lock.parent().unwrap()).unwrap();
    mkfifo(&lock);
    let (tx, rx) = std::sync::mpsc::channel();
    let s = store.clone();
    thread::spawn(move || {
        let r = s.fill(&EntryKey::Blob(digest(10)), |e| write_blob(e, b"x"));
        tx.send(r.map_err(|e| e.to_string())).unwrap();
    });
    let r = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("fill blocked on a FIFO");
    let err = r.unwrap_err();
    assert!(err.contains("regular file"), "{err}");
}

#[test]
fn a_bad_lock_file_of_a_dead_build_does_not_stop_others() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join(".ranikust");
    Store::open(&paths(dir.path())).unwrap().close().unwrap();
    // A directory and a FIFO where the lock files of two builds go.
    fs::create_dir_all(state.join("builds/x/stages")).unwrap();
    fs::create_dir_all(state.join("builds/x.lock")).unwrap();
    fs::create_dir_all(state.join("tmp/y")).unwrap();
    mkfifo(&state.join("builds/y.lock"));
    let store = Store::open(&paths(dir.path())).unwrap();
    // Left for the maintainer, with a warning.
    assert!(state.join("builds/x/stages").is_dir());
    assert!(state.join("tmp/y").is_dir());
    store
        .fill(&EntryKey::Blob(digest(11)), |e| write_blob(e, b"ok"))
        .unwrap();
}

#[test]
fn a_symlink_left_as_a_build_directory_is_unlinked_not_followed() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join(".ranikust");
    let first = Store::open(&paths(dir.path())).unwrap();
    first
        .fill(&EntryKey::Blob(digest(12)), |e| write_blob(e, b"kept"))
        .unwrap();
    first.close().unwrap();
    // A dead build whose tmp/ and builds/ entries lead to the state
    // directory itself and to the host's root.
    std::os::unix::fs::symlink("/", state.join("tmp/z")).unwrap();
    std::os::unix::fs::symlink("..", state.join("builds/z")).unwrap();
    let store = Store::open(&paths(dir.path())).unwrap();
    assert!(!state.join("tmp/z").exists() && fs::symlink_metadata(state.join("tmp/z")).is_err());
    assert!(fs::symlink_metadata(state.join("builds/z")).is_err());
    assert_eq!(
        store.get(&EntryKey::Blob(digest(12))).unwrap(),
        Some(
            store
                .state_dir()
                .join("blobs/sha256")
                .join(digest(12).encoded())
        )
    );
    assert!(state.join(".ranikust-state").is_file());
}

#[test]
fn a_new_cache_gets_its_owner_and_mode_once() {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&paths(dir.path())).unwrap();
    let owner = rnk_store::CacheOwner {
        uid: rustix::process::geteuid().as_raw(),
        gid: rustix::process::getegid().as_raw(),
        mode: 0o4710,
    };
    let a = store
        .cache_lease("k", CacheSharing::Shared, Some(owner), &never)
        .unwrap();
    assert!(a.created());
    let meta = fs::metadata(a.path()).unwrap();
    assert_eq!(meta.permissions().mode() & 0o7777, 0o710, "no set-id bit");
    assert_eq!((meta.uid(), meta.gid()), (owner.uid, owner.gid));
    let path = a.path().to_owned();
    drop(a);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o750)).unwrap();
    let b = store
        .cache_lease("k", CacheSharing::Shared, Some(owner), &never)
        .unwrap();
    assert!(!b.created());
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
        0o750
    );
    // No temporary directory is left in caches/.
    assert!(
        names(store.state_dir().join("caches"))
            .iter()
            .all(|n| !n.starts_with('.'))
    );
}

/// An owner id of `u32::MAX`, which `chown` reads as "unchanged", leaves
/// that id as a new directory gets it, with the mode asked for.
#[test]
fn a_new_cache_owned_by_u32_max_keeps_the_processs_id() {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    let (uid, gid) = (
        rustix::process::geteuid().as_raw(),
        rustix::process::getegid().as_raw(),
    );
    for (n, (owner_uid, owner_gid)) in [(u32::MAX, gid), (uid, u32::MAX), (u32::MAX, u32::MAX)]
        .into_iter()
        .enumerate()
    {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&paths(dir.path())).unwrap();
        let owner = rnk_store::CacheOwner {
            uid: owner_uid,
            gid: owner_gid,
            mode: 0o750,
        };
        // A new directory's group: the egid, or its parent's where that is
        // setgid or the mount is `grpid`.
        let made_gid = fs::metadata(store.state_dir().join("caches"))
            .unwrap()
            .gid();
        let a = store
            .cache_lease("k", CacheSharing::Shared, Some(owner), &never)
            .unwrap_or_else(|e| panic!("{n}: {owner:?}: {e}"));
        assert!(a.created());
        let meta = fs::metadata(a.path()).unwrap();
        let want_gid = if owner_gid == u32::MAX {
            made_gid
        } else {
            owner_gid
        };
        assert_eq!((meta.uid(), meta.gid()), (uid, want_gid), "{owner:?}");
        assert_eq!(meta.permissions().mode() & 0o7777, 0o750, "{owner:?}");
    }
}

/// A cache that cannot be made leaves no temporary directory and no cache:
/// without the right to give files away, giving it to root fails.
#[test]
fn a_failed_new_cache_leaves_nothing() {
    let dir = tempfile::tempdir().unwrap();
    // A process that can give a file to root (root, or `CAP_CHOWN`) makes
    // the cache: nothing fails there to check.
    let probe = dir.path().join("probe");
    fs::write(&probe, "").unwrap();
    let gives_away = std::os::unix::fs::chown(&probe, Some(0), Some(0)).is_ok();
    fs::remove_file(&probe).unwrap();
    if gives_away {
        eprintln!("skipped: this process can give a directory to root");
        return;
    }
    let store = Store::open(&paths(dir.path())).unwrap();
    let owner = rnk_store::CacheOwner {
        uid: 0,
        gid: 0,
        mode: 0o755,
    };
    let r = store.cache_lease("k", CacheSharing::Shared, Some(owner), &never);
    assert!(r.is_err(), "{r:?}");
    assert!(names(store.state_dir().join("caches")).is_empty());
    // The next attempt is not blocked by a leftover.
    let ok = store.cache_lease("k", CacheSharing::Shared, None, &never);
    assert!(ok.unwrap().created());
}

#[test]
fn an_anonymous_store_keeps_nothing_in_the_state_directory() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    fs::create_dir(&state).unwrap();
    let store = Store::open_anonymous(&state).unwrap();
    assert!(store.is_anonymous());
    let key = EntryKey::Blob(digest(1));
    assert_eq!(store.get(&key).unwrap(), None);
    let path = store.fill(&key, |e| write_blob(e, b"blob")).unwrap();
    assert!(path.starts_with("/proc/self/fd"), "{}", path.display());
    assert_eq!(fs::read(&path).unwrap(), b"blob");
    // A second fill reuses the entry, without running the filler.
    let again = store
        .fill(&key, |_| -> io::Result<u64> { panic!("filled twice") })
        .unwrap();
    assert_eq!(again, path);
    assert_eq!(store.get(&key).unwrap(), Some(path.clone()));
    let (mut f, other) = store.anonymous_file().unwrap();
    f.write_all(b"layer").unwrap();
    assert_eq!(fs::read(&other).unwrap(), b"layer");
    // Only the marker of a build in progress, which holds nothing.
    assert_eq!(names(&state), [".ranikust-building"]);
    let layer = store.fill(&EntryKey::Layer(digest(2)), |_| -> io::Result<u64> {
        Ok(0)
    });
    assert!(matches!(layer, Err(Error::Anonymous { .. })), "{layer:?}");
    let never = || false;
    let cache = store.cache_lease("k", CacheSharing::Shared, None, &never);
    assert!(matches!(cache, Err(Error::Anonymous { .. })));
    store.close().unwrap();
    assert!(names(&state).is_empty());
}

#[test]
fn an_anonymous_store_says_when_its_files_are_in_memory() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    fs::create_dir(&state).unwrap();
    let store = Store::open_anonymous(&state).unwrap();
    drop(store.scratch_file().unwrap());
    assert!(
        !store.files_in_memory(),
        "on disk while the directory takes files"
    );
    // A state directory that takes no file any more: the next file is a
    // memfd, and the store says so.
    fs::set_permissions(&state, fs::Permissions::from_mode(0o500)).unwrap();
    let probe = state.join("probe");
    let takes_files = fs::File::create(&probe).is_ok();
    if takes_files {
        // Root or `CAP_DAC_OVERRIDE`: the permission stops nothing here.
        fs::remove_file(&probe).unwrap();
    }
    drop(store.scratch_file().unwrap());
    assert_eq!(store.files_in_memory(), !takes_files);
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
    store.close().unwrap();
}

#[test]
fn a_build_cut_short_keeps_the_next_builds_state_in_memory() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    fs::create_dir(&state).unwrap();
    // A clean build removes its marker; files are on disk (O_TMPFILE here).
    let store = Store::open_anonymous(&state).unwrap();
    let (_f, path) = store.anonymous_file().unwrap();
    let target = fs::read_link(&path).unwrap();
    assert!(
        !target.to_string_lossy().contains("memfd:"),
        "{}",
        target.display()
    );
    store.close().unwrap();
    assert!(names(&state).is_empty());
    // A RUN that ended with an error leaves the marker behind ...
    let store = Store::open_anonymous(&state).unwrap();
    let run = store.enter_run().unwrap();
    store.leave_run(run, false).unwrap();
    assert!(matches!(store.named_window(), Err(Error::Anonymous { .. })));
    store.close().unwrap();
    assert_eq!(names(&state), [".ranikust-building"]);
    // ... so the next build makes no name (here O_TMPFILE needs none, so
    // its files stay on disk; the named way falls back to memory, see the
    // anon module's tests), and keeps the marker.
    let store = Store::open_anonymous(&state).unwrap();
    let (_f, path) = store.anonymous_file().unwrap();
    assert!(fs::read_link(&path).is_ok());
    // Nor is anything unpacked by name (a `COPY --from` on `/`).
    let e = store.named_window().unwrap_err();
    assert!(matches!(e, Error::Anonymous { .. }), "{e}");
    assert!(e.to_string().contains("PID 1"), "{e}");
    store.close().unwrap();
    assert_eq!(names(&state), [".ranikust-building"]);
}

#[test]
fn one_anonymous_store_at_a_time_per_state_directory() {
    // Two containers sharing a volume: the second never touches the first's
    // names or marker.
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    fs::create_dir(&state).unwrap();
    let first = Store::open_anonymous(&state).unwrap();
    let window = first.named_window().unwrap();
    let name = first.scratch_name().unwrap();
    fs::create_dir(state.join(&name)).unwrap();
    let e = Store::open_anonymous(&state).unwrap_err();
    assert!(matches!(e, Error::Busy { .. }), "{e}");
    assert!(e.to_string().contains("its own volume"), "{e}");
    assert!(state.join(&name).is_dir());
    fs::remove_dir(state.join(&name)).unwrap();
    drop(window);
    first.close().unwrap();
    // Once it is closed, the next one opens, on a clean directory.
    let second = Store::open_anonymous(&state).unwrap();
    drop(second.named_window().unwrap());
    second.close().unwrap();
    assert!(names(&state).is_empty());
}

#[test]
fn a_marker_that_is_not_a_regular_file_never_blocks_the_start() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    fs::create_dir(&state).unwrap();
    let marker = state.join(".ranikust-building");
    // A FIFO: opened without blocking, counted as this container's.
    rustix::fs::mknodat(
        rustix::fs::CWD,
        &marker,
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::from_raw_mode(0o600),
        0,
    )
    .unwrap();
    let store = Store::open_anonymous(&state).unwrap();
    assert!(matches!(store.named_window(), Err(Error::Anonymous { .. })));
    store.close().unwrap();
    fs::remove_file(&marker).unwrap();
    // A link: never followed, counted likewise, and replaced by a regular
    // file holding this container's mark, which stays (ADR-0077).
    std::os::unix::fs::symlink("/etc/hostname", &marker).unwrap();
    let hostname = fs::read("/etc/hostname").ok();
    let store = Store::open_anonymous(&state).unwrap();
    assert!(matches!(store.named_window(), Err(Error::Anonymous { .. })));
    store.close().unwrap();
    assert!(fs::symlink_metadata(&marker).unwrap().is_file());
    assert!(fs::read_to_string(&marker).unwrap().starts_with("pid:["));
    assert_eq!(fs::read("/etc/hostname").ok(), hostname);
}

/// A state directory as kaniko mode on `/` uses it: new and empty.
fn anon_state(dir: &Path) -> (PathBuf, PathBuf) {
    let state = dir.join("state");
    fs::create_dir(&state).unwrap();
    let marker = state.join(".ranikust-building");
    (state, marker)
}

#[test]
fn a_marker_a_run_removed_is_written_again_and_stays() {
    let dir = tempfile::tempdir().unwrap();
    let (state, marker) = anon_state(dir.path());
    let store = Store::open_anonymous(&state).unwrap();
    let mine = fs::read(&marker).unwrap();
    let run = store.enter_run().unwrap();
    fs::remove_file(&marker).unwrap();
    store.leave_run(run, true).unwrap();
    assert_eq!(fs::read(&marker).unwrap(), mine);
    // The build makes no more names, and the marker stays at the end.
    assert!(store.named_window().is_err());
    store.close().unwrap();
    assert_eq!(names(&state), [".ranikust-building"]);
    let e = Store::open(&BuildPaths::new(dir.path(), &state)).unwrap_err();
    assert!(matches!(e, Error::UsedOnSlash { .. }), "{e}");
}

#[test]
fn a_marker_a_run_rewrote_for_another_container_is_replaced_and_stays() {
    // Another PID namespace's text would let the next build take the
    // marker over and make names while this build's processes live.
    let dir = tempfile::tempdir().unwrap();
    let (state, marker) = anon_state(dir.path());
    let store = Store::open_anonymous(&state).unwrap();
    let mine = fs::read(&marker).unwrap();
    let run = store.enter_run().unwrap();
    fs::write(&marker, "pid:[1] 00000000-0000-0000-0000-000000000000\n").unwrap();
    store.leave_run(run, true).unwrap();
    assert_eq!(fs::read(&marker).unwrap(), mine);
    store.close().unwrap();
    assert_eq!(fs::read(&marker).unwrap(), mine);
}

#[test]
fn a_marker_that_is_a_full_directory_is_replaced_by_a_fresh_one() {
    let dir = tempfile::tempdir().unwrap();
    let (state, marker) = anon_state(dir.path());
    // Left by an earlier build: counted as this container's, cut short.
    fs::create_dir_all(marker.join("a/b")).unwrap();
    fs::write(marker.join("a/b/c"), "x").unwrap();
    let store = Store::open_anonymous(&state).unwrap();
    assert!(fs::symlink_metadata(&marker).unwrap().is_file());
    let mine = fs::read(&marker).unwrap();
    // A `RUN` does the same during this build.
    let run = store.enter_run().unwrap();
    fs::remove_file(&marker).unwrap();
    fs::create_dir_all(marker.join("d")).unwrap();
    fs::write(marker.join("d/e"), "y").unwrap();
    store.leave_run(run, true).unwrap();
    assert_eq!(fs::read(&marker).unwrap(), mine);
    store.close().unwrap();
    assert_eq!(names(&state), [".ranikust-building"]);
    assert_eq!(fs::read(&marker).unwrap(), mine);
}

#[test]
fn a_failed_close_after_a_run_removed_the_marker_keeps_one_and_fails() {
    if rustix::process::geteuid().is_root() {
        // Root removes from a directory it may not write: no failure to see.
        eprintln!("skipped as root: the failure needs a permission check");
        return;
    }
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    let (state, marker) = anon_state(dir.path());
    let store = Store::open_anonymous(&state).unwrap();
    let run = store.enter_run().unwrap();
    // A directory whose entries cannot be removed, then the marker gone,
    // after the last check (a process the sweep missed).
    fs::create_dir(state.join("stuck")).unwrap();
    fs::write(state.join("stuck/file"), "x").unwrap();
    fs::set_permissions(state.join("stuck"), fs::Permissions::from_mode(0o500)).unwrap();
    store.leave_run(run, true).unwrap();
    fs::remove_file(&marker).unwrap();
    let e = store.close().unwrap_err();
    assert!(matches!(e, Error::NotEmptied { .. }), "{e}");
    assert!(e.to_string().contains("\"stuck\""), "{e}");
    assert!(e.to_string().contains("another build"), "{e}");
    assert!(fs::symlink_metadata(&marker).unwrap().is_file());
    let e = Store::open(&BuildPaths::new(dir.path(), &state)).unwrap_err();
    assert!(matches!(e, Error::UsedOnSlash { .. }), "{e}");
    fs::set_permissions(state.join("stuck"), fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn the_marker_keeper_writes_a_removed_marker_where_the_store_is_never_closed() {
    // A second signal ends the process before `close`: the keeper puts
    // the marker back first.
    let dir = tempfile::tempdir().unwrap();
    let (state, marker) = anon_state(dir.path());
    let store = Store::open_anonymous(&state).unwrap();
    let keeper = store.marker_keeper().unwrap().expect("an anonymous store");
    let run = store.enter_run().unwrap();
    fs::remove_file(&marker).unwrap();
    let outside = dir.path().join("outside");
    fs::write(&outside, "kept").unwrap();
    plant_as_a_run(&state, &outside);
    keeper.keep().unwrap();
    drop(run);
    drop(store);
    assert!(fs::symlink_metadata(&marker).unwrap().is_file());
    let e = Store::open(&BuildPaths::new(dir.path(), &state)).unwrap_err();
    assert!(matches!(e, Error::UsedOnSlash { .. }), "{e}");
    // Anything already under the name stays as it is.
    fs::remove_file(&marker).unwrap();
    fs::create_dir(&marker).unwrap();
    keeper.keep().unwrap();
    assert!(marker.is_dir());
    // A persistent store has none.
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&paths(dir.path())).unwrap();
    assert!(store.marker_keeper().unwrap().is_none());
}

#[test]
fn a_tree_deeper_than_path_max_is_emptied_at_the_end() {
    use rustix::fs::{CWD, Mode, OFlags, mkdirat, openat};
    // Any `RLIMIT_NOFILE` will do: the deletion holds a fixed number of
    // descriptors whatever the depth.
    const DEPTH: u64 = 2100; // "d/" each: 4200 bytes, past PATH_MAX
    /// Empties a temp dir on drop, a failed test included: the temp dir's
    /// own removal recurses with a descriptor per level.
    struct Emptied(PathBuf);
    impl Drop for Emptied {
        fn drop(&mut self) {
            let r = fs::canonicalize(&self.0)
                .map_err(|e| e.to_string())
                .and_then(|p| rnk_rootfs::RootPath::open(&p).map_err(|e| e.to_string()))
                .and_then(|root| {
                    rnk_rootfs::delete_contents(&root, &rnk_rootfs::IgnoreList::new())
                        .map_err(|e| e.to_string())
                });
            // A drop cannot fail the test; a tree left behind is at least shown.
            if let Err(e) = r {
                eprintln!("left behind in {}: {e}", self.0.display());
            }
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let _emptied = Emptied(dir.path().to_owned());
    let (state, _marker) = anon_state(dir.path());
    let store = Store::open_anonymous(&state).unwrap();
    let run = store.enter_run().unwrap();
    let mut at = openat(CWD, &state, OFlags::PATH | OFlags::DIRECTORY, Mode::empty()).unwrap();
    for _ in 0..DEPTH {
        mkdirat(&at, "d", Mode::from_raw_mode(0o700)).unwrap();
        at = openat(&at, "d", OFlags::PATH | OFlags::DIRECTORY, Mode::empty()).unwrap();
    }
    drop(at);
    store.leave_run(run, true).unwrap();
    store.close().unwrap();
    assert!(names(&state).is_empty(), "{:?}", names(&state));
}

#[test]
fn a_refusal_names_what_the_state_directory_holds() {
    let dir = tempfile::tempdir().unwrap();
    let (state, _marker) = anon_state(dir.path());
    // A name NFS gives a file removed while open: not a persistent store.
    fs::write(state.join(".nfs000000000001"), "").unwrap();
    let e = Store::open_anonymous(&state).unwrap_err();
    assert!(matches!(e, Error::StateNotEmpty { .. }), "{e}");
    let shown = e.to_string();
    assert!(shown.contains("\".nfs000000000001\""), "{shown}");
    assert!(shown.contains("NFS or FUSE"), "{shown}");
    assert!(!shown.contains("persistent store ("), "{shown}");
    // A persistent store is named as one.
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    store_with_data(dir.path(), &state);
    let e = Store::open_anonymous(&state).unwrap_err();
    assert!(
        e.to_string()
            .contains("a persistent store (.ranikust-state"),
        "{e}"
    );
}

#[test]
fn a_marker_from_another_container_or_boot_is_taken_over() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    fs::create_dir(&state).unwrap();
    // A state directory on a volume, left by a build cut short in a
    // container that is gone: its processes went with it.
    let marker = state.join(".ranikust-building");
    fs::write(&marker, "pid:[1] 00000000-0000-0000-0000-000000000000\n").unwrap();
    let store = Store::open_anonymous(&state).unwrap();
    drop(store.named_window().unwrap());
    let ours = fs::read_to_string(&marker).unwrap();
    assert!(ours.starts_with("pid:["), "{ours}");
    store.close().unwrap();
    assert!(names(&state).is_empty());
    // A name left with no marker (a RUN deleted it): cut short here.
    fs::create_dir(state.join(format!("{}00ff", rnk_store::ANON_PREFIX))).unwrap();
    let store = Store::open_anonymous(&state).unwrap();
    assert!(matches!(store.named_window(), Err(Error::Anonymous { .. })));
    store.close().unwrap();
    assert_eq!(names(&state), [".ranikust-building"]);
}

#[test]
fn a_state_directory_moved_during_a_run_fails_the_run() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    fs::create_dir(&state).unwrap();
    let store = Store::open_anonymous(&state).unwrap();
    let run = store.enter_run().unwrap();
    store.leave_run(run, true).unwrap();
    let run = store.enter_run().unwrap();
    // A RUN moves the directory and puts a link in its place.
    fs::rename(&state, dir.path().join("moved")).unwrap();
    std::os::unix::fs::symlink(dir.path().join("moved"), &state).unwrap();
    assert!(matches!(
        store.leave_run(run, true),
        Err(Error::BadEntry { .. })
    ));
    assert!(matches!(store.named_window(), Err(Error::Anonymous { .. })));
}

#[test]
fn a_persistent_store_gives_anonymous_files_too() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&paths(dir.path())).unwrap();
    let (mut f, path) = store.anonymous_file().unwrap();
    f.write_all(b"kept tree").unwrap();
    assert!(path.starts_with("/proc/self/fd"), "{}", path.display());
    assert_eq!(fs::read(&path).unwrap(), b"kept tree");
    assert!(names(store.build_dir()).is_empty());
}

#[test]
fn a_failed_anonymous_fill_publishes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_anonymous(dir.path()).unwrap();
    let key = EntryKey::Blob(digest(3));
    let r = store.fill(&key, |_| -> io::Result<u64> {
        Err(io::Error::other("digest mismatch"))
    });
    assert!(matches!(r, Err(Error::Fill { .. })), "{r:?}");
    assert_eq!(store.get(&key).unwrap(), None);
}

/// What a `Direct` `RUN` in kaniko mode on `/` can write in the state
/// directory, as root on its filesystem: a forged store marker, a layer
/// under a chain id for a later build to reuse, links and a FIFO.
fn plant_as_a_run(state: &Path, outside: &Path) {
    fs::write(state.join(".ranikust-state"), "").unwrap();
    let layer = state.join(format!("layers/v2/sha256/{}", "07".repeat(32)));
    fs::create_dir_all(&layer).unwrap();
    fs::write(layer.join("planted"), "poison").unwrap();
    std::os::unix::fs::symlink(outside, state.join("link")).unwrap();
    fs::hard_link(outside, state.join("hardlink")).unwrap();
    rustix::fs::mknodat(
        rustix::fs::CWD,
        state.join("fifo"),
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::from_raw_mode(0o600),
        0,
    )
    .unwrap();
}

/// A persistent store in `state` (its build directories in `dir`) with a
/// blob in it.
fn store_with_data(dir: &Path, state: &Path) {
    let store = Store::open(&BuildPaths::new(dir, state)).unwrap();
    store
        .fill(&EntryKey::Blob(digest(1)), |e| write_blob(e, b"blob"))
        .unwrap();
    store.close().unwrap();
}

#[test]
fn an_anonymous_store_refuses_a_persistent_store() {
    // Kaniko mode on `/` builds with a temporary store only: its `RUN` could
    // change what the builds of a persistent store reuse (ADR-0077).
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    store_with_data(dir.path(), &state);
    let before = names(&state);
    let e = Store::open_anonymous(&state).unwrap_err();
    assert!(matches!(e, Error::StateNotEmpty { .. }), "{e}");
    assert!(e.to_string().contains("temporary store"), "{e}");
    assert_eq!(names(&state), before);
}

/// A persistent store with no data, as a build killed in a set-up that
/// falls back to kaniko mode on `/` leaves it (ADR-0136), is emptied by
/// an anonymous store, which then opens; the names an earlier anonymous
/// store left among it still count as a build cut short. One that
/// another build holds, or that a kaniko-mode build on `/` used too, stays
/// refused and as it was.
#[test]
fn an_anonymous_store_empties_a_store_with_no_data_first() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    Store::open(&BuildPaths::new(dir.path(), &state))
        .unwrap()
        .close()
        .unwrap();
    let dead = Store::open(&BuildPaths::new(dir.path(), &state)).unwrap();
    fs::write(dead.build_dir().join("left"), b"x").unwrap();
    drop(dead);
    fs::create_dir_all(state.join(rnk_store::SANDBOX_DIR).join("runs")).unwrap();
    let store = Store::open_anonymous(&state).unwrap();
    assert_eq!(names(&state), [".ranikust-building"]);
    drop(store.named_window().unwrap());
    store.close().unwrap();
    assert!(names(&state).is_empty(), "{:?}", names(&state));

    Store::open(&BuildPaths::new(dir.path(), &state))
        .unwrap()
        .close()
        .unwrap();
    fs::create_dir(state.join(format!("{}00ff", rnk_store::ANON_PREFIX))).unwrap();
    let store = Store::open_anonymous(&state).unwrap();
    assert!(matches!(store.named_window(), Err(Error::Anonymous { .. })));
    store.close().unwrap();

    let state = dir.path().join("held");
    let held = Store::open(&BuildPaths::new(dir.path(), &state)).unwrap();
    let before = names(&state);
    let e = Store::open_anonymous(&state).unwrap_err();
    assert!(matches!(e, Error::Busy { .. }), "{e}");
    assert_eq!(names(&state), before);
    held.close().unwrap();

    fs::write(state.join(".ranikust-building"), b"").unwrap();
    let before = names(&state);
    let e = Store::open_anonymous(&state).unwrap_err();
    assert!(matches!(e, Error::StateNotEmpty { .. }), "{e}");
    assert_eq!(names(&state), before);

    // With a name no store makes, or a prune's marker: refused, kept.
    for name in ["notes", ".ranikust-pruning"] {
        let state = dir.path().join(format!("with{name}"));
        Store::open(&BuildPaths::new(dir.path(), &state))
            .unwrap()
            .close()
            .unwrap();
        fs::write(state.join(name), b"").unwrap();
        let before = names(&state);
        let e = Store::open_anonymous(&state).unwrap_err();
        assert!(matches!(e, Error::StateNotEmpty { .. }), "{name}: {e}");
        assert_eq!(names(&state), before, "{name}");
    }
}

/// The user namespace probe's directories share the anonymous prefix but
/// are made before any store opens: one a killed set-up left, alone or
/// beside a store with no data, is removed and tells no build cut short;
/// beside a name an anonymous store made, that name still does.
#[test]
fn a_probe_directory_left_tells_no_build_cut_short() {
    let dir = tempfile::tempdir().unwrap();
    let probe = format!("{}1-2", rnk_store::USERNS_PROBE_PREFIX);
    for with_store in [false, true] {
        let state = dir.path().join(format!("state-{with_store}"));
        if with_store {
            Store::open(&BuildPaths::new(dir.path(), &state))
                .unwrap()
                .close()
                .unwrap();
        } else {
            fs::create_dir(&state).unwrap();
        }
        fs::create_dir_all(state.join(&probe).join("x")).unwrap();
        let store = Store::open_anonymous(&state).unwrap();
        drop(store.named_window().unwrap());
        store.close().unwrap();
        assert!(
            names(&state).is_empty(),
            "{with_store}: {:?}",
            names(&state)
        );
    }
    // Beside a name an anonymous store made: a build cut short still.
    let state = dir.path().join("both");
    fs::create_dir(&state).unwrap();
    fs::create_dir(state.join(format!("{}00ff", rnk_store::ANON_PREFIX))).unwrap();
    fs::create_dir(state.join(&probe)).unwrap();
    let store = Store::open_anonymous(&state).unwrap();
    assert!(matches!(store.named_window(), Err(Error::Anonymous { .. })));
    store.close().unwrap();
}

/// A probe directory a probe still holds is never removed, whatever
/// empties the state directory: an anonymous store opens beside it and
/// leaves it at its end, a store with no data beside it is kept (and an
/// anonymous store refused), and `prune --all` empties all but it. Once
/// let go, each removes it.
#[test]
fn a_probe_directory_a_probe_holds_is_kept() {
    let dir = tempfile::tempdir().unwrap();
    let probe = format!("{}1-2", rnk_store::USERNS_PROBE_PREFIX);
    let hold = |state: &Path| {
        fs::create_dir_all(state.join(&probe).join("x")).unwrap();
        let held = fs::File::open(state.join(&probe)).unwrap();
        rustix::fs::flock(&held, rustix::fs::FlockOperation::NonBlockingLockExclusive).unwrap();
        held
    };
    let state = dir.path().join("alone");
    fs::create_dir(&state).unwrap();
    let held = hold(&state);
    let store = Store::open_anonymous(&state).unwrap();
    drop(store.named_window().unwrap());
    store.close().unwrap();
    assert_eq!(names(&state), [probe.as_str()]);
    assert!(state.join(&probe).join("x").is_dir());
    drop(held);
    Store::open_anonymous(&state).unwrap().close().unwrap();
    assert!(names(&state).is_empty(), "{:?}", names(&state));

    let state = dir.path().join("store");
    Store::open(&BuildPaths::new(dir.path(), &state))
        .unwrap()
        .close()
        .unwrap();
    let held = hold(&state);
    let before = names(&state);
    let e = Store::open_anonymous(&state).unwrap_err();
    assert!(matches!(e, Error::StateNotEmpty { .. }), "{e}");
    assert_eq!(names(&state), before);
    assert!(state.join(&probe).join("x").is_dir());
    drop(held);
    Store::open_anonymous(&state).unwrap().close().unwrap();
    assert!(names(&state).is_empty(), "{:?}", names(&state));

    let state = dir.path().join("pruned");
    store_with_data(dir.path(), &state);
    let held = hold(&state);
    assert!(Store::prune_all(&state).unwrap());
    assert_eq!(names(&state), [probe.as_str()]);
    assert!(state.join(&probe).join("x").is_dir());
    drop(held);
    Store::open(&BuildPaths::new(dir.path(), &state))
        .unwrap()
        .close()
        .unwrap();
    assert!(!state.join(&probe).exists());
}

#[test]
fn an_anonymous_store_refuses_files_it_did_not_leave() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    fs::create_dir(&state).unwrap();
    fs::write(state.join("notes"), "x").unwrap();
    let e = Store::open_anonymous(&state).unwrap_err();
    assert!(matches!(e, Error::StateNotEmpty { .. }), "{e}");
    assert_eq!(names(&state), ["notes"]);
}

#[test]
fn a_kaniko_build_on_slash_and_other_builds_never_share_a_state_directory() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    fs::create_dir(&state).unwrap();
    let paths = BuildPaths::new(dir.path(), &state);
    // An anonymous store holds it: a persistent one is refused and writes
    // nothing there.
    let anon = Store::open_anonymous(&state).unwrap();
    let e = Store::open(&paths).unwrap_err();
    assert!(matches!(e, Error::Busy { .. }), "{e}");
    assert_eq!(names(&state), [".ranikust-building"]);
    anon.close().unwrap();
    // A persistent store holds it: an anonymous one is refused.
    let first = Store::open(&paths).unwrap();
    let e = Store::open_anonymous(&state).unwrap_err();
    assert!(matches!(e, Error::Busy { .. }), "{e}");
    // Persistent builds still share it.
    let second = Store::open(&paths).unwrap();
    second.close().unwrap();
    first.close().unwrap();
}

#[test]
fn a_kaniko_build_on_slash_leaves_nothing_its_run_wrote() {
    // Published entries are reused without hashing them again, which holds
    // only while no `RUN` can write the state directory. A `Direct` `RUN`
    // can, so the anonymous store empties it at its end (ADR-0077).
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    fs::create_dir(&state).unwrap();
    let outside = dir.path().join("outside");
    fs::write(&outside, "kept").unwrap();
    let store = Store::open_anonymous(&state).unwrap();
    let run = store.enter_run().unwrap();
    plant_as_a_run(&state, &outside);
    store.leave_run(run, true).unwrap();
    store.close().unwrap();
    assert!(names(&state).is_empty(), "{:?}", names(&state));
    assert_eq!(fs::read_to_string(&outside).unwrap(), "kept");
    // A persistent store opened there next starts empty: nothing planted is
    // taken for a published entry.
    let store = Store::open(&BuildPaths::new(dir.path(), &state)).unwrap();
    assert_eq!(store.get(&EntryKey::Layer(digest(7))).unwrap(), None);
    store.close().unwrap();
}

#[test]
fn a_kaniko_build_on_slash_cut_short_keeps_persistent_stores_off_its_directory() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    fs::create_dir(&state).unwrap();
    let outside = dir.path().join("outside");
    fs::write(&outside, "kept").unwrap();
    let paths = BuildPaths::new(dir.path(), &state);
    // A `RUN` that ended with an error may have left processes: the marker
    // stays, the rest goes.
    let store = Store::open_anonymous(&state).unwrap();
    let run = store.enter_run().unwrap();
    plant_as_a_run(&state, &outside);
    store.leave_run(run, false).unwrap();
    store.close().unwrap();
    assert_eq!(names(&state), [".ranikust-building"]);
    assert_eq!(fs::read_to_string(&outside).unwrap(), "kept");
    let e = Store::open(&paths).unwrap_err();
    assert!(matches!(e, Error::UsedOnSlash { .. }), "{e}");
    // A forged store marker beside it changes nothing.
    fs::write(state.join(".ranikust-state"), "").unwrap();
    let e = Store::open(&paths).unwrap_err();
    assert!(matches!(e, Error::UsedOnSlash { .. }), "{e}");
    fs::remove_file(state.join(".ranikust-state")).unwrap();
    // A build killed before its end leaves what its `RUN` wrote, and its
    // marker: no persistent store opens on it either.
    let store = Store::open_anonymous(&state).unwrap();
    let run = store.enter_run().unwrap();
    plant_as_a_run(&state, &outside);
    store.leave_run(run, true).unwrap();
    drop(store);
    let e = Store::open(&paths).unwrap_err();
    assert!(matches!(e, Error::UsedOnSlash { .. }), "{e}");
}

/// What a test sees of the tree at `dir`: each entry's type and mode,
/// owner, ctime and size, without following symlinks.
fn tree(dir: &Path) -> Vec<(PathBuf, u32, u32, u32, i64, i64, u64)> {
    use std::os::unix::fs::MetadataExt as _;
    let mut out = Vec::new();
    let mut todo = vec![dir.to_owned()];
    while let Some(p) = todo.pop() {
        let m = fs::symlink_metadata(&p).unwrap();
        if m.is_dir() {
            todo.extend(fs::read_dir(&p).unwrap().map(|e| e.unwrap().path()));
        }
        out.push((
            p.strip_prefix(dir).unwrap().to_owned(),
            m.mode(),
            m.uid(),
            m.gid(),
            m.ctime(),
            m.ctime_nsec(),
            m.len(),
        ));
    }
    out.sort();
    out
}

/// Red team: a store directory planted as a symlink out never has its target changed (T13):
/// the open refuses it.
#[test]
fn red_team_a_store_directory_planted_as_a_symlink_out_of_the_state_dir_is_never_changed() {
    use std::os::unix::fs::PermissionsExt as _;
    let egid = rustix::process::getegid().as_raw();
    let group = rustix::process::getgroups()
        .unwrap()
        .into_iter()
        .map(rustix::process::Gid::as_raw)
        .find(|&g| g != egid)
        .unwrap_or(egid);
    let mut opened = 0;
    for d in ["blobs", "layers", "caches", "locks", "tmp", "builds"] {
        for absolute in [false, true] {
            for twin in [false, true] {
                // Not under /tmp: the twin of an absolute target would be in
                // the store's own tmp/.
                let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
                let base = fs::canonicalize(dir.path()).unwrap();
                let state = base.join(".ranikust");
                Store::open(&paths(&base)).unwrap().close().unwrap();
                let outside = base.join("outside");
                fs::create_dir_all(outside.join("sub")).unwrap();
                fs::write(outside.join("sub/file"), b"kept").unwrap();
                for p in [outside.join("sub"), outside.clone()] {
                    std::os::unix::fs::chown(&p, None, Some(group)).unwrap();
                    // The bit and gid `own_group` takes off a store directory.
                    fs::set_permissions(&p, fs::Permissions::from_mode(0o2750)).unwrap();
                }
                let (target, inside) = if absolute {
                    (
                        outside.clone(),
                        state.join(outside.strip_prefix("/").unwrap()),
                    )
                } else {
                    (PathBuf::from("../outside"), state.join("outside"))
                };
                if twin {
                    fs::create_dir_all(&inside).unwrap();
                }
                fs::remove_dir_all(state.join(d)).unwrap();
                std::os::unix::fs::symlink(&target, state.join(d)).unwrap();
                let before = tree(&outside);

                let case = format!("{d} -> {} (twin {twin})", target.display());
                let mut seen = Vec::new();
                let open = Store::open(&paths(&base));
                if let Err(e) = &open {
                    assert!(matches!(e, Error::NotAStateDir { .. }), "{case}: {e}");
                }
                if let Ok(store) = open {
                    opened += 1;
                    let layer = store.fill(
                        &EntryKey::Layer(digest(1)),
                        |e| -> rnk_rootfs::Result<u64> {
                            e.root().unwrap().create_dir_all("etc", 0o755)?;
                            Ok(0)
                        },
                    );
                    let blob = store.fill(&EntryKey::Blob(digest(2)), |e| write_blob(e, b"x"));
                    let cache = store
                        .cache_lease("c", CacheSharing::Shared, None, &never)
                        .map(|l| l.path().to_owned());
                    seen.extend([layer, blob, cache].into_iter().filter_map(Result::ok));
                    let _ = store.close();
                }
                assert_eq!(tree(&outside), before, "{case}");
                // What the store hands out never resolves out of it.
                for p in &seen {
                    if let Ok(real) = fs::canonicalize(p) {
                        assert!(real.starts_with(&state), "{case}: {}", real.display());
                    }
                }
                // The twin, in the root, is what the symlink led to.
                if twin && d == "layers" && !seen.is_empty() {
                    assert!(
                        inside.join("v3/sha256").join(digest(1).encoded()).is_dir(),
                        "{case}"
                    );
                }
            }
        }
    }
    assert_eq!(opened, 0, "the store opened on {opened} plants");
}
