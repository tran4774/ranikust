//! The canary of a user namespace's id maps (ADR-0128) as a store opens,
//! reuses and publishes layers, the maps given to it: a test here enters
//! no user namespace, and gives no file another owner.

use std::os::unix::fs::MetadataExt as _;

use super::*;

/// This process's effective uid and gid.
fn own() -> (u32, u32) {
    (
        rustix::process::geteuid().as_raw(),
        rustix::process::getegid().as_raw(),
    )
}

/// Maps whose canary this process can make: one id each way, its own.
fn own_maps() -> IdMaps {
    let (u, g) = own();
    IdMaps::parse(&format!("{u} {u} 1\n"), &format!("{g} {g} 1\n")).unwrap()
}

/// An id other than `id`.
fn other(id: u32) -> u32 {
    if id == 4242 { 4243 } else { 4242 }
}

/// Maps none of whose ids is this process's, root's included.
fn other_maps() -> IdMaps {
    let (u, g) = own();
    IdMaps::parse(
        &format!("{} 100000 1\n", other(u)),
        &format!("{} 100000 1\n", other(g)),
    )
    .unwrap()
}

/// Plants the canary of `maps`, each entry this process's.
fn plant(state: &Path, maps: &IdMaps) {
    let rel = canary::path(maps);
    fs::create_dir_all(state.join(&rel)).unwrap();
    for e in canary::entries(maps) {
        fs::write(state.join(&rel).join(&e.name), b"").unwrap();
    }
}

/// Stores opened on this thread read `maps` as their namespace's.
fn give(maps: Option<IdMaps>) {
    ID_MAPS.with(|m| *m.borrow_mut() = Some(maps));
}

/// The layer `n`'s key.
fn key(n: u8) -> EntryKey {
    EntryKey::Layer(Digest::from_sha256_bytes(&[n; 32]))
}

/// Publishes the layer `n`, a file `f` in it, unless it is there.
fn layer(store: &Store, n: u8) -> Result<PathBuf> {
    store.fill(&key(n), |tmp| -> std::io::Result<u64> {
        fs::write(tmp.path().join("f"), b"x")?;
        Ok(1)
    })
}

/// Every path below `dir`, sorted, each with its type, owner and size:
/// what a refusal must leave as it was.
fn tree(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        for e in fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            let m = fs::symlink_metadata(&p).unwrap();
            if m.is_dir() {
                todo.push(p.clone());
            }
            out.push(format!(
                "{} {:o} {}:{} {}",
                p.strip_prefix(dir).unwrap().display(),
                m.mode(),
                m.uid(),
                m.gid(),
                m.len()
            ));
        }
    }
    out.sort();
    out
}

#[test]
fn the_first_layer_under_maps_makes_their_canary_and_the_next_stores_check_it() {
    let dir = tempfile::tempdir().unwrap();
    let paths = BuildPaths::new(dir.path(), dir.path().join("state"));
    give(Some(own_maps()));
    let store = Store::open(&paths).unwrap();
    assert_eq!(store.id_maps(), Some(&own_maps()));
    let canary = paths.state.join(canary::path(&own_maps()));
    assert!(!canary.exists());
    // A blob is no layer: no canary.
    store
        .fill(&EntryKey::Blob(Digest::from_sha256_bytes(&[9; 32])), |_| {
            Ok::<_, std::io::Error>(0)
        })
        .unwrap();
    assert!(!canary.exists());
    let first = layer(&store, 1).unwrap();
    let (u, g) = own();
    for e in canary::entries(&own_maps()) {
        let m = fs::symlink_metadata(canary.join(&e.name)).unwrap();
        assert!(m.is_file(), "{}", e.name);
        assert_eq!((m.uid(), m.gid()), (u, g), "{}", e.name);
    }
    assert_eq!(layer(&store, 1).unwrap(), first);
    layer(&store, 2).unwrap();
    store.close().unwrap();
    let store = Store::open(&paths).unwrap();
    assert_eq!(store.get(&key(1)).unwrap(), Some(first.clone()));
    assert_eq!(layer(&store, 1).unwrap(), first);
    store.close().unwrap();
    // The check of a state directory before its store opens agrees.
    drop(Store::check_state_dir(&paths.state).unwrap());
}

/// The canary of the maps there, but with owners other than they give: as
/// a namespace sees it where the maps above its parent moved, or the
/// state directory got other owners on disk. Refused by the store's open
/// and by the check before it, each before anything is written.
#[test]
fn a_canary_of_other_owners_refuses_the_state_directory_before_anything_is_written() {
    let dir = tempfile::tempdir().unwrap();
    let paths = BuildPaths::new(dir.path(), dir.path().join("state"));
    give(Some(other_maps()));
    Store::open(&paths).unwrap().close().unwrap();
    plant(&paths.state, &other_maps());
    let before = tree(&paths.state);
    let e = Store::open(&paths).unwrap_err();
    let text = e.to_string();
    assert!(matches!(e, Error::IdMapChanged { .. }), "{text}");
    let (u, g) = own();
    let (ou, og) = (other(u), other(g));
    assert!(
        text.contains(&format!("/uid-{ou} is owned by {u}:{g}, not {ou}:{og}")),
        "{text}"
    );
    assert!(text.contains("ranikust prune --all"), "{text}");
    assert_eq!(tree(&paths.state), before);
    let e = Store::check_state_dir(&paths.state).unwrap_err();
    assert!(matches!(e, Error::IdMapChanged { .. }), "{e}");
    assert_eq!(tree(&paths.state), before);
    // The control: other maps, or none, have no canary there.
    give(Some(own_maps()));
    Store::open(&paths).unwrap().close().unwrap();
    give(None);
    Store::open(&paths).unwrap().close().unwrap();
}

/// Before a store opens in a user namespace this process makes, the canary
/// of each map it may get is checked from here: owned by the ids the map
/// gives its entries' own. One of other owners refuses the state directory
/// before anything is written; one of the ids the map gives, or none, does
/// not.
#[test]
fn a_child_namespace_s_canary_is_checked_from_here_before_anything_is_written() {
    let dir = tempfile::tempdir().unwrap();
    let paths = BuildPaths::new(dir.path(), dir.path().join("state"));
    give(None);
    Store::open(&paths).unwrap().close().unwrap();
    let (u, g) = own();
    let child = |uid: u32| IdMaps::parse(&format!("0 {uid} 1\n"), &format!("0 {g} 1\n")).unwrap();
    // This process's ids are the child's 0: its canary, as made there.
    let same = child(u);
    let moved = child(other(u));
    let unplanted = IdMaps::parse("0 100000 10\n", "0 100000 10\n").unwrap();
    plant(&paths.state, &same);
    plant(&paths.state, &moved);
    let before = tree(&paths.state);
    drop(Store::check_state_dir_for(&paths.state, &[same.clone(), unplanted]).unwrap());
    let e = Store::check_state_dir_for(&paths.state, &[same, moved]).unwrap_err();
    let text = e.to_string();
    assert!(matches!(e, Error::IdMapChanged { .. }), "{text}");
    assert!(
        text.contains(&format!(
            "/uid-0 is owned by {u}:{g}, not {}:{g} (the build's user namespace's 0:0)",
            other(u)
        )),
        "{text}"
    );
    assert!(text.contains("ranikust prune --all"), "{text}");
    assert_eq!(tree(&paths.state), before);
}

/// A canary that cannot be made publishes no layer, and leaves in `tmp/`
/// only what it did not make; the next layer makes it.
#[test]
fn a_canary_that_cannot_be_made_publishes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let paths = BuildPaths::new(dir.path(), dir.path().join("state"));
    give(Some(own_maps()));
    let store = Store::open(&paths).unwrap();
    // The canary's temporary name, after the layer's (`tmp/<id>/0`), taken.
    let tmp = paths.state.join("tmp").join(store.build_id());
    fs::create_dir_all(&tmp).unwrap();
    fs::write(tmp.join("1"), b"").unwrap();
    let e = layer(&store, 1).unwrap_err();
    assert!(!matches!(e, Error::IdMapChanged { .. }), "{e}");
    assert!(!paths.state.join(key(1).path()).exists(), "{e}");
    assert!(!paths.state.join(canary::path(&own_maps())).exists(), "{e}");
    let left: Vec<_> = fs::read_dir(&tmp)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(left, ["1"], "{e}");
    layer(&store, 1).unwrap();
    assert!(paths.state.join(canary::path(&own_maps())).is_dir());
    store.close().unwrap();
}

/// A canary whose entries cannot be given their owners (maps of ids not
/// this process's, as a user without `CAP_CHOWN`) publishes no layer, and
/// what it made in `tmp/` is removed.
#[test]
fn a_canary_half_made_is_removed() {
    if rustix::process::geteuid().is_root() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let paths = BuildPaths::new(dir.path(), dir.path().join("state"));
    give(Some(other_maps()));
    let store = Store::open(&paths).unwrap();
    let e = layer(&store, 1).unwrap_err();
    assert!(!matches!(e, Error::IdMapChanged { .. }), "{e}");
    assert!(!paths.state.join(key(1).path()).exists(), "{e}");
    assert!(
        !paths.state.join(canary::path(&other_maps())).exists(),
        "{e}"
    );
    let tmp = paths.state.join("tmp").join(store.build_id());
    assert_eq!(fs::read_dir(&tmp).unwrap().count(), 0, "{e}");
    store.close().unwrap();
}

/// Two stores that opened with no canary: the first layer makes it, and
/// the other store finds it under its lock and makes none.
#[test]
fn stores_that_opened_with_no_canary_make_one_between_them() {
    let dir = tempfile::tempdir().unwrap();
    let paths = BuildPaths::new(dir.path(), dir.path().join("state"));
    give(Some(own_maps()));
    let a = Store::open(&paths).unwrap();
    let b = Store::open(&paths).unwrap();
    layer(&a, 1).unwrap();
    let canary = paths.state.join(canary::path(&own_maps()));
    let made = fs::metadata(&canary).unwrap().ino();
    layer(&b, 2).unwrap();
    assert_eq!(
        b.get(&key(1)).unwrap(),
        Some(paths.state.join(key(1).path()))
    );
    assert_eq!(fs::metadata(&canary).unwrap().ino(), made);
    a.close().unwrap();
    b.close().unwrap();
}

/// A layer under the maps with no canary there (one gone, or published
/// before canaries) is never reused, by `get` or by `fill`.
#[test]
fn a_layer_under_maps_with_no_canary_is_never_reused() {
    let dir = tempfile::tempdir().unwrap();
    let paths = BuildPaths::new(dir.path(), dir.path().join("state"));
    give(Some(own_maps()));
    let store = Store::open(&paths).unwrap();
    layer(&store, 1).unwrap();
    store.close().unwrap();
    fs::remove_dir_all(paths.state.join(canary::path(&own_maps()))).unwrap();
    let store = Store::open(&paths).unwrap();
    for e in [
        store.get(&key(1)).unwrap_err(),
        layer(&store, 1).unwrap_err(),
    ] {
        let text = e.to_string();
        assert!(matches!(e, Error::IdMapChanged { .. }), "{text}");
        assert!(text.contains("is missing, yet layer sha256:"), "{text}");
    }
    store.close().unwrap();
}

/// Outside a user namespace, layers are published and reused with no
/// canary, and none is checked.
#[test]
fn outside_a_user_namespace_there_is_no_canary() {
    let dir = tempfile::tempdir().unwrap();
    let paths = BuildPaths::new(dir.path(), dir.path().join("state"));
    give(None);
    let store = Store::open(&paths).unwrap();
    assert_eq!(store.id_maps(), None);
    let first = layer(&store, 1).unwrap();
    assert!(!paths.state.join("layers/v3/canary").exists());
    store.close().unwrap();
    let store = Store::open(&paths).unwrap();
    assert_eq!(store.get(&key(1)).unwrap(), Some(first));
    store.close().unwrap();
}
