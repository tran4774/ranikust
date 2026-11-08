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
    // Under a name the store makes, so that the marker alone decides.
    let precious = |state: &Path| {
        fs::create_dir_all(state.join("blobs")).unwrap();
        fs::write(state.join("blobs/precious"), b"x").unwrap();
    };
    precious(&state);
    let before = tree(&state);
    let e = Store::prune_all(&state).unwrap_err();
    assert!(matches!(e, Error::NotAStateDir { .. }), "{e}");
    assert!(
        e.to_string()
            .contains("has no regular file .ranikust-state"),
        "{e}"
    );
    assert_eq!(tree(&state), before);
    // A marker that is not a regular file marks nothing.
    for (i, kind) in ["link", "dir"].into_iter().enumerate() {
        let state = dir.path().join(format!("state-{i}"));
        precious(&state);
        let marker = state.join(rnk_store::STATE_MARKER);
        if kind == "link" {
            std::os::unix::fs::symlink(state.join("blobs/precious"), &marker).unwrap();
        } else {
            fs::create_dir(&marker).unwrap();
        }
        let before = tree(&state);
        let e = Store::prune_all(&state).unwrap_err();
        assert!(matches!(e, Error::NotAStateDir { .. }), "{kind}: {e}");
        assert_eq!(tree(&state), before, "{kind}");
    }
}

/// A marked directory holding, at its top, one name neither the store nor
/// a build makes there is refused whole, the marker kept in place, also
/// when an earlier prune renamed it.
#[test]
fn prune_all_refuses_a_name_the_store_does_not_make() {
    let dir = tempfile::tempdir().unwrap();
    for (i, (kind, journal)) in [
        ("file", false),
        ("dir", false),
        ("file", true),
        ("dir", true),
    ]
    .into_iter()
    .enumerate()
    {
        let state = dir.path().join(format!("state-{i}"));
        used_store(&state);
        if journal {
            fs::rename(
                state.join(rnk_store::STATE_MARKER),
                state.join(rnk_store::PRUNING_MARKER),
            )
            .unwrap();
        }
        let foreign = state.join("notes");
        if kind == "file" {
            fs::write(&foreign, b"x").unwrap();
        } else {
            fs::create_dir(&foreign).unwrap();
            fs::write(foreign.join("f"), b"x").unwrap();
        }
        let before = tree(&state);
        let e = Store::prune_all(&state).unwrap_err();
        assert!(matches!(e, Error::NotAStateDir { .. }), "{kind}: {e}");
        assert!(
            e.to_string()
                .contains("holds \"notes\", which ranikust does not make there"),
            "{kind}: {e}"
        );
        assert_eq!(tree(&state), before, "{kind}, journal {journal}");
    }
}

/// `check_prune`'s refusal to open a path with a symbolic link on it,
/// last or not, is told apart from every other refusal.
#[test]
fn check_prune_tells_a_symlink_on_the_path_apart() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    // Without the symlinks a `TMPDIR` may be reached through.
    let tmp = dir.path().canonicalize().unwrap();
    let real = tmp.join("real");
    let state = real.join("state");
    used_store(&state);
    symlink(&real, tmp.join("link")).unwrap();
    symlink(&state, tmp.join("last")).unwrap();
    for typed in [tmp.join("link/state"), tmp.join("last")] {
        let e = Store::check_prune(&typed).unwrap_err();
        assert!(e.is_symlink_on(&typed), "{e}");
        assert!(!e.is_symlink_on(&state), "{e}");
    }
    let unmarked = tmp.join("unmarked");
    fs::create_dir(&unmarked).unwrap();
    fs::write(unmarked.join("keep"), b"x").unwrap();
    let odd = tmp.join("odd");
    used_store(&odd);
    fs::remove_file(odd.join(rnk_store::STATE_MARKER)).unwrap();
    symlink(
        state.join(rnk_store::STATE_MARKER),
        odd.join(rnk_store::STATE_MARKER),
    )
    .unwrap();
    for dir in [tmp.join("missing"), unmarked, odd] {
        let e = Store::check_prune(&dir).unwrap_err();
        assert!(!e.is_symlink_on(&dir), "{e}");
    }
}

/// Every name the store and a build make at the top is taken: the store's
/// directories, the sandbox's and an anonymous store's names, the user
/// namespace probe's among them; one on a symlink is removed, not
/// followed.
#[test]
fn prune_all_takes_every_name_the_store_makes() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let outside = dir.path().join("outside");
    fs::create_dir_all(outside.join("sub")).unwrap();
    fs::write(outside.join("sub/f"), b"keep").unwrap();
    used_store(&state);
    fs::create_dir(state.join(rnk_store::SANDBOX_DIR)).unwrap();
    fs::write(state.join(rnk_store::SANDBOX_DIR).join("runs"), b"").unwrap();
    fs::create_dir(state.join(format!("{}00ff", rnk_store::ANON_PREFIX))).unwrap();
    let probe = state.join(format!("{}1-1", rnk_store::USERNS_PROBE_PREFIX));
    fs::create_dir(&probe).unwrap();
    std::os::unix::fs::symlink(outside.join("sub"), probe.join("out")).unwrap();
    let link = state.join(format!("{}1-2", rnk_store::USERNS_PROBE_PREFIX));
    std::os::unix::fs::symlink(outside.join("sub"), link).unwrap();
    let before = tree(&outside);
    assert!(Store::prune_all(&state).unwrap());
    assert_eq!(tree(&state), Vec::<PathBuf>::new());
    assert_eq!(tree(&outside), before);
}

/// This process's own `/` is refused before anything is locked or looked
/// at; nothing would be removed there anyway, holding no marker.
#[test]
fn prune_all_refuses_the_machines_own_root() {
    let e = Store::prune_all(Path::new("/")).unwrap_err();
    assert!(matches!(e, Error::Rootfs(_)), "{e}");
    assert!(e.to_string().contains("own root"), "{e}");
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
        assert!(e.to_string().ends_with("so no build uses it"), "{e}");
    }
    // So does the check a set-up runs before it changes the directory.
    let e = Store::check_state_dir(&state).unwrap_err();
    assert!(matches!(e, Error::PruneCutShort { .. }), "{e}");
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
    assert!(matches!(e, Error::NotAStateDir { .. }), "{e}");
    let e = e.to_string();
    assert!(e.contains("not a regular file"), "{e}");
    assert!(e.contains("remove it first"), "{e}");
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

/// A store with no data leaves the state directory empty when abandoned,
/// whoever made it, the sandbox's directory and what dead builds left
/// included, so that kaniko mode on `/` takes it (ADR-0136); one with an
/// entry in `blobs/`, `layers/` or `caches/`, one another build holds, or
/// one with a name the store does not make, is kept whole.
#[test]
fn abandon_empties_a_store_with_no_data_that_no_build_holds() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let made = open(&state);
    fs::create_dir(state.join(rnk_store::SANDBOX_DIR)).unwrap();
    fs::write(made.build_dir().join("probe"), b"x").unwrap();
    assert!(made.abandon().unwrap(), "emptied");
    assert_eq!(tree(&state), Vec::<PathBuf>::new());
    let anon = Store::open_anonymous(&state).unwrap();
    anon.close().unwrap();

    // Found with no data, as a build killed before it used it leaves it.
    let state = dir.path().join("unused");
    open(&state).close().unwrap();
    let dead = open(&state);
    fs::write(dead.build_dir().join("left"), b"x").unwrap();
    drop(dead);
    fs::create_dir_all(state.join(rnk_store::SANDBOX_DIR).join("runs")).unwrap();
    fs::write(state.join("locks").join("l"), b"").unwrap();
    assert!(open(&state).abandon().unwrap(), "emptied");
    assert_eq!(tree(&state), Vec::<PathBuf>::new());

    // Found marked, with data: closed, the rest kept.
    let state = dir.path().join("found");
    used_store(&state);
    // The dead build's directories go at the next open.
    open(&state).close().unwrap();
    let before = tree(&state);
    assert!(!open(&state).abandon().unwrap(), "kept");
    assert_eq!(tree(&state), before);
    for data in ["blobs", "layers", "caches"] {
        let state = dir.path().join(data);
        open(&state).close().unwrap();
        fs::write(state.join(data).join("e"), b"").unwrap();
        let before = tree(&state);
        assert!(!open(&state).abandon().unwrap(), "{data}: kept");
        assert_eq!(tree(&state), before, "{data}");
    }

    // A name the store does not make: kept.
    let state = dir.path().join("foreign");
    open(&state).close().unwrap();
    fs::write(state.join("mine"), b"keep").unwrap();
    let before = tree(&state);
    assert!(!open(&state).abandon().unwrap(), "kept");
    assert_eq!(tree(&state), before);

    // Made, but another build holds it: closed, the rest kept.
    let state = dir.path().join("held");
    let made = open(&state);
    let other = open(&state);
    assert!(!made.abandon().unwrap(), "kept while held");
    assert!(state.join(rnk_store::STATE_MARKER).is_file());
    other.close().unwrap();
    assert!(Store::prune_all(&state).unwrap());
}

/// What an emptying of a store with no data cannot remove (a directory
/// the test user may not empty) is named, and the marker stays: the
/// directory is still a store, with no data, which builds open.
#[test]
fn abandon_keeps_the_marker_while_anything_stays() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    open(&state).close().unwrap();
    let shut = state.join("tmp").join("shut");
    fs::create_dir(&shut).unwrap();
    fs::write(shut.join("f"), b"x").unwrap();
    fs::set_permissions(&shut, fs::Permissions::from_mode(0o555)).unwrap();
    let probe = shut.join("probe");
    if fs::File::create(&probe).is_ok() {
        // Root or `CAP_DAC_OVERRIDE`: the mode stops nothing here.
        fs::remove_file(&probe).unwrap();
        fs::set_permissions(&shut, fs::Permissions::from_mode(0o755)).unwrap();
        return;
    }
    // The mode given back before any `unwrap`: the temp dir must go even
    // if one fails.
    let abandoned = Store::open(&BuildPaths::new(&state, &state)).map(Store::abandon);
    let left = tree(&state);
    let opened = Store::open(&BuildPaths::new(&state, &state));
    fs::set_permissions(&shut, fs::Permissions::from_mode(0o755)).unwrap();
    let abandoned = abandoned.unwrap();
    let e = abandoned.unwrap_err();
    assert!(matches!(e, Error::NotAbandoned { .. }), "{e}");
    assert!(e.to_string().contains("\"tmp\""), "{e}");
    assert!(
        left.contains(&PathBuf::from(rnk_store::STATE_MARKER)),
        "{left:?}"
    );
    assert!(left.contains(&PathBuf::from("tmp/shut/f")), "{left:?}");
    opened.unwrap().close().unwrap();
    assert!(open(&state).abandon().unwrap());
    assert_eq!(tree(&state), Vec::<PathBuf>::new());
}
