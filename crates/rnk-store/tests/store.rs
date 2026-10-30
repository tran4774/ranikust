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
use rnk_store::{BuildPaths, EntryKey, Error, Store, TmpEntry};

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
    assert!(state.join(".ranikust-root").is_file());
    for d in ["blobs", "layers", "caches", "tmp", "builds", "locks"] {
        assert!(state.join(d).is_dir(), "{d}");
    }
    assert!(store.build_dir().is_dir());
    assert_eq!(store.build_dir().parent().unwrap(), state.join("builds"));
}

#[test]
fn open_refuses_a_non_empty_directory_without_the_marker() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("precious"), "x").unwrap();
    let err = Store::open(&BuildPaths::new(dir.path(), dir.path())).unwrap_err();
    assert!(matches!(err, Error::NotAStateDir { .. }), "{err}");
    assert_eq!(names(dir.path()), ["precious"]);
}

#[test]
fn open_refuses_a_marker_that_is_not_a_regular_file() {
    let dir = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink("/etc/hostname", dir.path().join(".ranikust-root")).unwrap();
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
            .join("layers/sha256")
            .join(digest(2).encoded())
    );
    assert!(p.join("etc").is_dir());
    assert_eq!(
        fs::read_link(p.join("etc/link")).unwrap(),
        Path::new("../outside")
    );
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
            "layers"
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

#[test]
fn a_layer_of_the_wrong_type_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&paths(dir.path())).unwrap();
    let p = store
        .state_dir()
        .join("layers/sha256")
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
    assert!(state.join(".ranikust-root").is_file());
}
