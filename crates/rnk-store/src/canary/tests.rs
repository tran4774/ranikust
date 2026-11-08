//! The canary's entries, name and check, on a directory of this process's
//! ids: no test here can give a file another owner.

use std::fs;
use std::os::unix::fs::symlink;

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

/// The names of `entries`, in order.
fn names(entries: &[Entry]) -> Vec<&str> {
    entries.iter().map(|e| e.name.as_str()).collect()
}

/// Puts something at a canary's path.
type Plant<'a> = &'a dyn Fn(&Path);

/// A state directory and its root.
fn state() -> (tempfile::TempDir, RootPath) {
    let dir = tempfile::tempdir().unwrap();
    let root = RootPath::open(dir.path()).unwrap();
    (dir, root)
}

#[test]
fn the_canary_holds_the_first_and_the_last_id_of_each_extent() {
    // As rootless Podman maps its root: the user as 0, then a range.
    let maps = IdMaps::parse(
        "0 1000 1\n1 100000 65536\n",
        "1 200000 10\n0 1000 1\n5000 300000 1\n",
    )
    .unwrap();
    let made = entries(&maps);
    assert_eq!(
        names(&made),
        [
            "uid-0",
            "uid-1",
            "uid-65536",
            "gid-0",
            "gid-1",
            "gid-10",
            "gid-5000"
        ]
    );
    // Each besides owned by the lowest id of the other map.
    for e in &made {
        let (id, other) = if e.name.starts_with("uid-") {
            (e.uid, e.gid)
        } else {
            (e.gid, e.uid)
        };
        assert_eq!(e.name.split_once('-').unwrap().1, id.to_string());
        assert_eq!(other, 0, "{e:?}");
    }
    // The widest map gives its first and its last id, never `u32::MAX`.
    let identity = IdMaps::parse("0 0 4294967295\n", "0 0 4294967295\n").unwrap();
    assert_eq!(
        names(&entries(&identity)),
        ["uid-0", "uid-4294967294", "gid-0", "gid-4294967294"]
    );
}

#[test]
fn the_canary_is_named_by_both_maps_beside_the_layers() {
    let a = IdMaps::parse("0 1000 1\n1 100000 65536\n", "0 1000 1\n").unwrap();
    let uid = IdMaps::parse("0 1000 1\n1 200000 65536\n", "0 1000 1\n").unwrap();
    let gid = IdMaps::parse("0 1000 1\n1 100000 65536\n", "0 1001 1\n").unwrap();
    let p = path(&a);
    assert_eq!(p.parent(), Some(Path::new("layers/v3/canary")));
    let hex = p.file_name().unwrap().to_str().unwrap();
    assert_eq!(hex.len(), 64);
    assert!(
        hex.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    );
    assert_eq!(path(&a), path(&a.clone()));
    assert_ne!(path(&a), path(&uid));
    assert_ne!(path(&a), path(&gid));
    assert_ne!(path(&uid), path(&gid));
}

#[test]
fn a_canary_made_by_this_process_matches_its_own_ids() {
    let (dir, root) = state();
    let maps = own_maps();
    assert_eq!(check(&root, dir.path(), &maps).unwrap(), Found::Absent);
    fs::create_dir(dir.path().join("tmp")).unwrap();
    create(&root, &maps, Path::new("tmp/canary"), 0o700).unwrap();
    assert!(!dir.path().join("tmp/canary").exists());
    assert_eq!(check(&root, dir.path(), &maps).unwrap(), Found::Matches);
    let (u, g) = own();
    let mut made: Vec<_> = fs::read_dir(dir.path().join(path(&maps)))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    made.sort();
    assert_eq!(made, [format!("gid-{g}"), format!("uid-{u}")]);
    // Never made over one that is there.
    fs::create_dir(dir.path().join("tmp/again")).unwrap();
    assert!(create(&root, &maps, Path::new("tmp/again/c"), 0o700).is_err());
}

/// Each entry missing in turn refuses the canary, naming it.
#[test]
fn a_canary_missing_an_entry_is_refused() {
    let maps = own_maps();
    for gone in entries(&maps) {
        let (dir, root) = state();
        fs::create_dir(dir.path().join("tmp")).unwrap();
        create(&root, &maps, Path::new("tmp/c"), 0o700).unwrap();
        fs::remove_file(dir.path().join(path(&maps)).join(&gone.name)).unwrap();
        let e = check(&root, dir.path(), &maps).unwrap_err();
        assert!(matches!(e, Error::IdMapChanged { .. }), "{e}");
        assert!(
            e.to_string().contains(&format!(
                "{}/{} is missing",
                path(&maps).display(),
                gone.name
            )),
            "{e}"
        );
    }
}

/// The canary of maps whose ids are not this process's, made by it: what
/// a namespace sees where the maps above it moved, or the state directory
/// was given other owners on disk.
#[test]
fn a_canary_of_other_owners_is_refused_with_the_way_out() {
    let (dir, root) = state();
    let (u, g) = own();
    let other = |id: u32| if id == 4242 { 4243 } else { 4242 };
    let maps = IdMaps::parse(
        &format!("{} 100000 10\n", other(u)),
        &format!("{} 100000 10\n", other(g)),
    )
    .unwrap();
    let rel = path(&maps);
    fs::create_dir_all(dir.path().join(&rel)).unwrap();
    for e in entries(&maps) {
        fs::write(dir.path().join(&rel).join(&e.name), b"").unwrap();
    }
    let e = check(&root, dir.path(), &maps).unwrap_err();
    let text = e.to_string();
    assert!(matches!(e, Error::IdMapChanged { .. }), "{text}");
    assert!(
        text.contains(&format!(
            "{}/uid-{} is owned by {u}:{g}, not {}:{}",
            rel.display(),
            other(u),
            other(u),
            other(g)
        )),
        "{text}"
    );
    assert!(text.contains("ranikust prune --all"), "{text}");
    assert!(text.contains(&dir.path().display().to_string()), "{text}");
}

/// A canary that is a file or a symlink, or holds an entry that is not a
/// regular file, is refused, never followed.
#[test]
fn a_canary_of_the_wrong_type_is_refused() {
    let maps = own_maps();
    let rel = path(&maps);
    let (u, _) = own();
    let plant: [(&str, Plant<'_>); 4] = [
        ("a file", &|p| fs::write(p, b"").unwrap()),
        ("a symlink to a good one", &|p| {
            let good = p.with_file_name("good");
            fs::create_dir(&good).unwrap();
            for e in entries(&maps) {
                fs::write(good.join(&e.name), b"").unwrap();
            }
            symlink("good", p).unwrap();
        }),
        ("an entry that is a directory", &|p| {
            fs::create_dir(p).unwrap();
            for e in entries(&maps) {
                fs::create_dir(p.join(&e.name)).unwrap();
            }
        }),
        ("an entry that is a symlink", &|p| {
            fs::create_dir(p).unwrap();
            for e in entries(&maps) {
                fs::write(p.join(format!("{}.real", e.name)), b"").unwrap();
                symlink(format!("{}.real", e.name), p.join(&e.name)).unwrap();
            }
        }),
    ];
    for (what, plant) in plant {
        let (dir, root) = state();
        fs::create_dir_all(dir.path().join(&rel).parent().unwrap()).unwrap();
        plant(&dir.path().join(&rel));
        let e = check(&root, dir.path(), &maps).unwrap_err();
        assert!(matches!(e, Error::IdMapChanged { .. }), "{what}: {e}");
        let wrong = e.to_string();
        assert!(
            wrong.contains("is not a directory") || wrong.contains("is not a regular file"),
            "{what}: {wrong}"
        );
        assert!(
            !wrong.contains(&format!("uid-{u} is owned")),
            "{what}: {wrong}"
        );
    }
}
