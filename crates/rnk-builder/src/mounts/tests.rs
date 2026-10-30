use std::fs;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

use rnk_store::BuildPaths;

use super::*;

fn mount(kind: MountType) -> Mount {
    Mount {
        kind,
        ..Mount::default()
    }
}

fn never() -> bool {
    false
}

/// The caller's own ids on `m`: a regular user cannot give a file to
/// root, so T0 tests ask for the owner they already are.
fn owned(mut m: Mount) -> Mount {
    m.uid = Some(rustix::process::geteuid().as_raw());
    m.gid = Some(rustix::process::getegid().as_raw());
    m
}

fn store(dir: &Path) -> Store {
    Store::open(&BuildPaths::new(dir, dir.join(".ranikust"))).unwrap()
}

#[test]
fn mount_targets_are_absolute_and_clean() {
    let wd = Path::new("/app");
    assert_eq!(mount_target("/cache", wd), PathBuf::from("/cache"));
    assert_eq!(
        mount_target("node_modules/.cache", wd),
        PathBuf::from("/app/node_modules/.cache")
    );
    assert_eq!(mount_target("../../../x", wd), PathBuf::from("/x"));
}

#[test]
fn a_cache_id_defaults_to_the_cleaned_target() {
    let mut m = mount(MountType::Cache);
    m.target = "/root//.npm/".into();
    assert_eq!(cache_id(&m), "/root/.npm");
    m.target = "a/./b/../c".into();
    assert_eq!(cache_id(&m), "a/c");
    m.cache_id = "npm".into();
    assert_eq!(cache_id(&m), "npm");
}

#[test]
fn go_clean_matches_path_clean() {
    for (input, want) in [
        ("", "."),
        ("/", "/"),
        ("/..", "/"),
        ("a/../..", ".."),
        ("../a", "../a"),
        ("/a/b/../c/", "/a/c"),
        ("a//b", "a/b"),
    ] {
        assert_eq!(go_clean(input), want, "{input:?}");
    }
}

#[test]
fn owners_and_modes_key_other_caches() {
    let mut m = mount(MountType::Cache);
    m.target = "/c".into();
    let plain = cache_key(&m);
    assert_eq!(plain, "/c");
    m.uid = Some(1000);
    let owned = cache_key(&m);
    assert_ne!(owned, plain);
    m.mode = Some(0o755);
    assert_eq!(cache_key(&m), owned, "0755 is the default mode");
    m.mode = Some(0o700);
    assert_ne!(cache_key(&m), owned);
}

#[test]
fn cache_mounts_follow_sharing() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let secrets = BTreeMap::new();
    let mut m = mount(MountType::Cache);
    m.target = "/cache".into();
    m.sharing = Some(Sharing::Private);
    let a = run_mounts(&[m.clone()], Path::new("/"), &store, &secrets, &never).unwrap();
    let b = run_mounts(&[m.clone()], Path::new("/"), &store, &secrets, &never).unwrap();
    assert_ne!(
        a.binds[0].source, b.binds[0].source,
        "private: another cache"
    );
    assert_eq!(a.binds[0].target, PathBuf::from("/cache"));
    let first = a.binds[0].source.clone();
    a.finish().unwrap();
    b.finish().unwrap();
    m.sharing = None;
    let s1 = run_mounts(&[m.clone()], Path::new("/"), &store, &secrets, &never).unwrap();
    let s2 = run_mounts(&[m.clone()], Path::new("/"), &store, &secrets, &never).unwrap();
    assert_eq!(s1.binds[0].source, first, "shared: the first cache");
    assert_eq!(s2.binds[0].source, first);
    s1.finish().unwrap();
    s2.finish().unwrap();
    // A key with one cache: a locked user waits while it is held.
    m.target = "/locked".into();
    m.sharing = Some(Sharing::Locked);
    let held = run_mounts(&[m.clone()], Path::new("/"), &store, &secrets, &never).unwrap();
    let cancelled = run_mounts(&[m], Path::new("/"), &store, &secrets, &|| true);
    assert!(
        matches!(cancelled, Err(Error::Store(rnk_store::Error::Cancelled))),
        "{cancelled:?}"
    );
    held.finish().unwrap();
}

#[test]
fn secret_mounts_follow_buildkit_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let host = dir.path().join("token");
    fs::write(&host, b"s3cret").unwrap();
    let secrets: BTreeMap<String, Secret> = [
        ("token".to_owned(), Secret::File(host)),
        (
            "env".to_owned(),
            Secret::Env("RNK_TEST_SECRET_NEVER_SET".into()),
        ),
    ]
    .into();
    let wd = Path::new("/app");
    let one = |m: &Mount| run_mounts(std::slice::from_ref(m), wd, &store, &secrets, &never);

    // The id from the target's last name; a private copy, 0400.
    let mut m = owned(mount(MountType::Secret));
    m.target = "/etc/token".into();
    let r = one(&m).unwrap();
    let b = &r.binds[0];
    assert_eq!(b.target, PathBuf::from("/etc/token"));
    assert!(b.read_only);
    assert!(
        b.source.starts_with(store.build_dir()),
        "a copy in the build directory"
    );
    assert_eq!(fs::read(&b.source).unwrap(), b"s3cret");
    let meta = fs::metadata(&b.source).unwrap();
    assert_eq!(meta.permissions().mode() & 0o7777, 0o400);
    let copy = b.source.clone();
    r.finish().unwrap();
    assert!(!copy.exists(), "removed after the RUN");

    // The target from the id; source= names the id; mode is honoured.
    let mut m = owned(mount(MountType::Secret));
    m.source = "token".into();
    m.mode = Some(0o4755);
    let r = one(&m).unwrap();
    assert_eq!(r.binds[0].target, PathBuf::from("/run/secrets/token"));
    let mode = fs::metadata(&r.binds[0].source)
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o7777, 0o755, "permission bits only");
    r.finish().unwrap();

    // env= alone: a variable, no file; with a target: both.
    let mut m = mount(MountType::Secret);
    m.cache_id = "token".into();
    m.env = Some("TOKEN".into());
    let r = one(&m).unwrap();
    assert!(r.binds.is_empty());
    assert_eq!(r.env, vec![("TOKEN".into(), "s3cret".into())]);
    r.finish().unwrap();
    let mut m = owned(m);
    m.target = "t".into();
    let r = one(&m).unwrap();
    assert_eq!(r.binds[0].target, PathBuf::from("/app/t"));
    assert_eq!(r.env.len(), 1);
    r.finish().unwrap();

    // An unset variable reads as empty, as BuildKit's os.Getenv.
    let mut m = mount(MountType::Secret);
    m.cache_id = "env".into();
    m.env = Some("E".into());
    assert_eq!(one(&m).unwrap().env, vec![("E".into(), "".into())]);

    // Missing: left out unless required; no id at all is an error.
    let mut m = mount(MountType::Secret);
    m.cache_id = "other".into();
    let r = one(&m).unwrap();
    assert!(r.binds.is_empty() && r.env.is_empty());
    m.required = true;
    assert!(one(&m).is_err());
    assert!(one(&mount(MountType::Secret)).is_err());
    let mut m = mount(MountType::Secret);
    m.cache_id = "token".into();
    m.env = Some("A=B".into());
    assert!(one(&m).is_err());
    assert_eq!(
        fs::read_dir(store.build_dir()).unwrap().count(),
        0,
        "nothing left"
    );
}

#[test]
fn a_secret_over_the_limit_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("big");
    fs::write(
        &path,
        vec![b'x'; usize::try_from(MAX_SECRET_SIZE).unwrap() + 1],
    )
    .unwrap();
    assert!(Secret::File(path.clone()).read("big").is_err());
    fs::write(&path, vec![b'x'; usize::try_from(MAX_SECRET_SIZE).unwrap()]).unwrap();
    assert!(Secret::File(path).read("big").is_ok());
}

/// The owner a secret or a new cache gets needs root to `chown`; as a
/// regular user only the user's own ids work.
#[test]
fn owners_apply_to_new_caches_only() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let uid = rustix::process::geteuid().as_raw();
    let gid = rustix::process::getegid().as_raw();
    let mut m = mount(MountType::Cache);
    m.target = "/c".into();
    m.uid = Some(uid);
    m.gid = Some(gid);
    m.mode = Some(0o710);
    let r = run_mounts(
        &[m.clone()],
        Path::new("/"),
        &store,
        &BTreeMap::new(),
        &never,
    )
    .unwrap();
    let path = r.binds[0].source.clone();
    let meta = fs::metadata(&path).unwrap();
    assert_eq!(meta.permissions().mode() & 0o7777, 0o710);
    assert_eq!((meta.uid(), meta.gid()), (uid, gid));
    r.finish().unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o750)).unwrap();
    let r = run_mounts(&[m], Path::new("/"), &store, &BTreeMap::new(), &never).unwrap();
    assert_eq!(r.binds[0].source, path);
    let mode = fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o7777, 0o750, "not applied again");
    r.finish().unwrap();
}

/// BuildKit's default owner of a secret file is root: `chown` needs root,
/// so this runs in the T2 VM.
#[test]
#[ignore = "T2: gives a file to root; run as root in the test VM only"]
fn secret_files_default_to_root_and_0400() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let host = dir.path().join("token");
    fs::write(&host, b"s").unwrap();
    fs::set_permissions(&host, fs::Permissions::from_mode(0o644)).unwrap();
    std::os::unix::fs::chown(&host, Some(1000), Some(1000)).unwrap();
    let secrets: BTreeMap<String, Secret> = [("token".to_owned(), Secret::File(host))].into();
    let mut m = mount(MountType::Secret);
    m.cache_id = "token".into();
    let r = run_mounts(&[m.clone()], Path::new("/"), &store, &secrets, &never).unwrap();
    let meta = fs::metadata(&r.binds[0].source).unwrap();
    assert_eq!((meta.uid(), meta.gid()), (0, 0));
    assert_eq!(meta.permissions().mode() & 0o7777, 0o400);
    r.finish().unwrap();
    m.uid = Some(1000);
    m.gid = Some(1001);
    let r = run_mounts(&[m], Path::new("/"), &store, &secrets, &never).unwrap();
    let meta = fs::metadata(&r.binds[0].source).unwrap();
    assert_eq!((meta.uid(), meta.gid()), (1000, 1001));
    r.finish().unwrap();
}
