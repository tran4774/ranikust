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

/// Maps none of whose ids is this process's.
fn other_maps() -> IdMaps {
    let (u, g) = own();
    let other = |id: u32| if id == 4242 { 4243 } else { 4242 };
    IdMaps::parse(
        &format!("0 1000 1\n{} 100000 10\n", other(u)),
        &format!("0 1000 1\n{} 100000 10\n", other(g)),
    )
    .unwrap()
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
    let rel = canary::path(&other_maps());
    fs::create_dir_all(paths.state.join(&rel)).unwrap();
    for e in canary::entries(&other_maps()) {
        fs::write(paths.state.join(&rel).join(&e.name), b"").unwrap();
    }
    let before = tree(&paths.state);
    let e = Store::open(&paths).unwrap_err();
    let text = e.to_string();
    assert!(matches!(e, Error::IdMapChanged { .. }), "{text}");
    let (u, g) = own();
    assert!(
        text.contains(&format!("/uid-0 is owned by {u}:{g}, not 0:0")),
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
