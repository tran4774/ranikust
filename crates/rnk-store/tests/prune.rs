//! `Store::prune_all` at tier T0 (ADR-0105), on the test user's own files
//! and without a user namespace. Files of other ids, removed as root of the
//! build's user namespace, are T1 and T2: `ranikust`'s
//! `setup/tests/userns.rs`.

use std::fs;
use std::io::{self, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use rnk_oci::Digest;
use rnk_store::{BuildPaths, CacheSharing, EntryKey, Error, Store};

fn digest(n: u8) -> Digest {
    Digest::from_sha256_bytes(&[n; 32])
}

fn open(state: &Path) -> Store {
    Store::open(&BuildPaths::new(state, state)).unwrap()
}

/// Every path below `dir`, never following a symlink, sorted.
fn tree(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut todo = vec![dir.to_owned()];
    while let Some(d) = todo.pop() {
        for e in fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            if fs::symlink_metadata(&p).unwrap().is_dir() {
                todo.push(p.clone());
            }
            out.push(p.strip_prefix(dir).unwrap().to_owned());
        }
    }
    out.sort();
    out
}

/// A state directory as builds leave it: a published blob and layer, a
/// cache mount with a file in it, their lock files, a build that closed,
/// and one that died with a file in each of its directories.
fn used_store(state: &Path) {
    let store = open(state);
    store
        .fill(&EntryKey::Blob(digest(1)), |e| {
            e.file().expect("a blob").write_all(b"blob")?;
            Ok::<_, io::Error>(4)
        })
        .unwrap();
    store
        .fill(&EntryKey::Layer(digest(2)), |e| {
            fs::create_dir(e.path().join("usr"))?;
            fs::write(e.path().join("usr/f"), b"x")?;
            std::os::unix::fs::symlink("/etc/hostname", e.path().join("usr/link"))?;
            Ok::<_, io::Error>(0)
        })
        .unwrap();
    let lease = store
        .cache_lease("go-build", CacheSharing::Shared, None, &|| false)
        .unwrap();
    fs::write(lease.path().join("c"), b"c").unwrap();
    drop(lease);
    store.close().unwrap();
    let dead = open(state);
    fs::write(dead.build_dir().join("left"), b"x").unwrap();
    let tmp = dead.state_dir().join("tmp").join(dead.build_id());
    fs::write(tmp.join("left"), b"x").unwrap();
    // Dropped unclosed: its lock goes with it, as with a killed process.
    drop(dead);
}

#[test]
fn prune_all_empties_what_builds_left_and_the_marker() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    used_store(&state);
    let before = tree(&state);
    for p in ["blobs", "layers", "caches", "locks", "tmp", "builds"] {
        assert!(before.contains(&PathBuf::from(p)), "{p}: {before:?}");
    }
    assert!(Store::prune_all(&state).unwrap());
    assert_eq!(tree(&state), Vec::<PathBuf>::new());
    // Then the directory goes with a plain rmdir, or serves the next build,
    // which marks it again.
    open(&state).close().unwrap();
    assert!(state.join(rnk_store::STATE_MARKER).is_file());
    assert!(Store::prune_all(&state).unwrap());
    fs::remove_dir(&state).unwrap();
}

#[test]
fn prune_all_refuses_while_a_build_holds_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    used_store(&state);
    let running = open(&state);
    let before = tree(&state);
    let e = Store::prune_all(&state).unwrap_err();
    assert!(matches!(e, Error::InUse { .. }), "{e}");
    assert!(e.to_string().contains("run it again"), "{e}");
    assert_eq!(tree(&state), before, "nothing removed");
    running.close().unwrap();
    assert!(Store::prune_all(&state).unwrap());
    assert_eq!(tree(&state), Vec::<PathBuf>::new());
}

#[test]
fn prune_all_refuses_a_directory_without_the_marker() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    fs::create_dir(&state).unwrap();
    fs::write(state.join("precious"), b"x").unwrap();
    let e = Store::prune_all(&state).unwrap_err();
    assert!(matches!(e, Error::NotAStateDir { .. }), "{e}");
    assert_eq!(tree(&state), [PathBuf::from("precious")]);
    // A marker that is not a regular file marks nothing.
    for (i, kind) in ["link", "dir"].into_iter().enumerate() {
        let state = dir.path().join(format!("state-{i}"));
        fs::create_dir(&state).unwrap();
        fs::write(state.join("precious"), b"x").unwrap();
        let marker = state.join(rnk_store::STATE_MARKER);
        if kind == "link" {
            std::os::unix::fs::symlink(state.join("precious"), &marker).unwrap();
        } else {
            fs::create_dir(&marker).unwrap();
        }
        let before = tree(&state);
        let e = Store::prune_all(&state).unwrap_err();
        assert!(matches!(e, Error::NotAStateDir { .. }), "{kind}: {e}");
        assert_eq!(tree(&state), before, "{kind}");
    }
}

#[test]
fn prune_all_leaves_an_empty_directory_as_it_is() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    fs::create_dir(&state).unwrap();
    assert!(!Store::prune_all(&state).unwrap());
    assert_eq!(tree(&state), Vec::<PathBuf>::new(), "no marker made");
}

#[test]
fn prune_all_refuses_a_directory_a_kaniko_build_on_slash_used() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    used_store(&state);
    fs::write(state.join(rnk_store::BUILDING_MARKER), b"").unwrap();
    let before = tree(&state);
    let e = Store::prune_all(&state).unwrap_err();
    assert!(matches!(e, Error::UsedOnSlash { .. }), "{e}");
    assert_eq!(tree(&state), before);
}

#[test]
fn prune_all_keeps_the_marker_while_anything_stays() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    used_store(&state);
    // A directory the test user may not empty, as a layer of other ids is
    // outside the build's user namespace.
    let shut = state.join("builds").join("shut");
    fs::create_dir(&shut).unwrap();
    fs::write(shut.join("f"), b"x").unwrap();
    fs::set_permissions(&shut, fs::Permissions::from_mode(0o555)).unwrap();
    let probe = shut.join("probe");
    let stops = fs::File::create(&probe).is_err();
    if stops {
        let pruned = Store::prune_all(&state);
        let left = tree(&state);
        let opened = Store::open(&BuildPaths::new(&state, &state));
        // Before any assertion: the temp dir must go even if one fails.
        fs::set_permissions(&shut, fs::Permissions::from_mode(0o755)).unwrap();
        let e = pruned.unwrap_err();
        assert!(matches!(e, Error::NotPruned { .. }), "{e}");
        assert!(e.to_string().contains("\"builds\""), "{e}");
        let marker = PathBuf::from(rnk_store::PRUNING_MARKER);
        assert!(left.contains(&marker), "{left:?}");
        let old = PathBuf::from(rnk_store::STATE_MARKER);
        assert!(!left.contains(&old), "renamed, not copied: {left:?}");
        assert!(left.contains(&PathBuf::from("builds/shut/f")), "{left:?}");
        // What the prune left is never a build's store.
        let Err(e) = opened else {
            panic!("a store opened after a failed prune")
        };
        assert!(matches!(e, Error::PruneCutShort { .. }), "{e}");
    } else {
        // Root or `CAP_DAC_OVERRIDE`: the mode stops nothing here.
        fs::remove_file(&probe).unwrap();
    }
    assert!(Store::prune_all(&state).unwrap());
    assert_eq!(tree(&state), Vec::<PathBuf>::new());
}

/// A prune killed while it empties a layer in place leaves the layer
/// published and part-empty: no build opens the directory until a prune
/// finishes, which then empties it, its marker last.
#[test]
fn a_prune_cut_short_keeps_every_build_out_until_one_finishes() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    used_store(&state);
    let layer = open(&state).get(&EntryKey::Layer(digest(2))).unwrap();
    let layer = layer.expect("the layer is published");
    // Where a killed prune stops: its marker renamed first, then one file
    // of the layer gone.
    let marker = state.join(rnk_store::PRUNING_MARKER);
    fs::rename(state.join(rnk_store::STATE_MARKER), &marker).unwrap();
    fs::remove_file(layer.join("usr/f")).unwrap();
    let before = tree(&state);
    for _ in 0..2 {
        let Err(e) = Store::open(&BuildPaths::new(&state, &state)) else {
            panic!("a store opened on a prune cut short")
        };
        assert!(matches!(e, Error::PruneCutShort { .. }), "{e}");
        assert!(
            e.to_string().contains("run ranikust prune --all again"),
            "{e}"
        );
    }
    assert_eq!(tree(&state), before, "the refusal changes nothing");
    assert!(Store::prune_all(&state).unwrap());
    assert_eq!(tree(&state), Vec::<PathBuf>::new());
    // Killed after the last entry, before its marker: the next one ends.
    fs::write(&marker, b"").unwrap();
    let Err(e) = Store::open(&BuildPaths::new(&state, &state)) else {
        panic!("a store opened on a prune cut short")
    };
    assert!(matches!(e, Error::PruneCutShort { .. }), "{e}");
    assert!(Store::prune_all(&state).unwrap());
    assert_eq!(tree(&state), Vec::<PathBuf>::new());
    open(&state).close().unwrap();
}

/// The marker becomes the journal by a rename, before anything is removed:
/// a prune killed at any point after leaves a directory no build opens.
/// No file is created on the way.
#[test]
fn the_marker_is_renamed_before_anything_is_removed() {
    use rustix::fs::inotify::{self, CreateFlags, ReadFlags, WatchFlags};
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    used_store(&state);
    let layer = open(&state).get(&EntryKey::Layer(digest(2))).unwrap();
    let layer = layer.expect("the layer is published");
    let fd = inotify::init(CreateFlags::NONBLOCK | CreateFlags::CLOEXEC).unwrap();
    let watched =
        WatchFlags::CREATE | WatchFlags::DELETE | WatchFlags::MOVED_FROM | WatchFlags::MOVED_TO;
    // One queue: the events of both directories in the order they happened.
    let top = inotify::add_watch(&fd, &state, watched).unwrap();
    inotify::add_watch(&fd, layer.join("usr"), watched).unwrap();
    assert!(Store::prune_all(&state).unwrap());
    let mut buf = vec![std::mem::MaybeUninit::uninit(); 1 << 16];
    let mut reader = inotify::Reader::new(&fd, &mut buf);
    let mut events = Vec::new();
    loop {
        match reader.next() {
            Ok(e) => {
                let name = e.file_name().map(|n| n.to_string_lossy().into_owned());
                events.push((e.wd() == top, e.events(), name));
            }
            Err(rustix::io::Errno::AGAIN) => break,
            Err(e) => panic!("{e}"),
        }
    }
    let renamed = events.iter().position(|(at_top, flags, name)| {
        *at_top
            && flags.contains(ReadFlags::MOVED_TO)
            && name.as_deref() == Some(rnk_store::PRUNING_MARKER)
    });
    let renamed = renamed.unwrap_or_else(|| panic!("no rename: {events:?}"));
    let removed = events
        .iter()
        .position(|(_, flags, _)| flags.contains(ReadFlags::DELETE));
    let removed = removed.unwrap_or_else(|| panic!("nothing removed: {events:?}"));
    assert!(renamed < removed, "{events:?}");
    assert!(
        !events.iter().any(|(_, f, _)| f.contains(ReadFlags::CREATE)),
        "{events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|(_, f, _)| f.contains(ReadFlags::QUEUE_OVERFLOW)),
        "{events:?}"
    );
}

/// A journal that is not a regular file was not left by a prune: the
/// prune removes nothing and says to remove it, and no build opens the
/// directory either.
#[test]
fn a_journal_that_is_not_a_file_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    used_store(&state);
    let journal = state.join(rnk_store::PRUNING_MARKER);
    fs::create_dir(&journal).unwrap();
    let before = tree(&state);
    let e = Store::prune_all(&state).unwrap_err();
    assert!(matches!(e, Error::NotPruned { .. }), "{e}");
    let e = e.to_string();
    assert!(e.contains("not a regular file"), "{e}");
    assert!(
        e.contains("remove it, then run ranikust prune --all"),
        "{e}"
    );
    assert_eq!(tree(&state), before);
    let Err(e) = Store::open(&BuildPaths::new(&state, &state)) else {
        panic!("a store opened")
    };
    assert!(matches!(e, Error::PruneCutShort { .. }), "{e}");
    fs::remove_dir(&journal).unwrap();
    assert!(Store::prune_all(&state).unwrap());
}

#[test]
fn links_out_of_the_state_directory_are_removed_not_followed() {
    use std::os::unix::fs::{MetadataExt as _, symlink};
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let outside = dir.path().join("outside");
    fs::create_dir_all(outside.join("sub")).unwrap();
    fs::write(outside.join("own"), b"keep").unwrap();
    fs::write(outside.join("sub/f"), b"keep").unwrap();
    used_store(&state);
    let cache = fs::read_dir(state.join("caches"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.is_dir())
        .expect("a cache");
    // What a `RUN` can leave in a cache mount, and what a layer can hold.
    symlink(&outside, cache.join("abs")).unwrap();
    symlink("../../../outside", cache.join("rel")).unwrap();
    fs::hard_link(outside.join("own"), cache.join("hard")).unwrap();
    rustix::fs::mknodat(
        rustix::fs::CWD,
        cache.join("fifo"),
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::from_raw_mode(0o600),
        0,
    )
    .unwrap();
    symlink(&outside, state.join("builds").join("ghost")).unwrap();
    let before = tree(&outside);
    assert!(Store::prune_all(&state).unwrap());
    assert_eq!(tree(&state), Vec::<PathBuf>::new());
    assert_eq!(tree(&outside), before);
    assert_eq!(fs::read(outside.join("own")).unwrap(), b"keep");
    assert_eq!(fs::metadata(outside.join("own")).unwrap().nlink(), 1);
    assert_eq!(fs::read(outside.join("sub/f")).unwrap(), b"keep");
}
