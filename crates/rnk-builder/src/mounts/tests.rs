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
    a.finish();
    b.finish();
    m.sharing = None;
    let s1 = run_mounts(&[m.clone()], Path::new("/"), &store, &secrets, &never).unwrap();
    let s2 = run_mounts(&[m.clone()], Path::new("/"), &store, &secrets, &never).unwrap();
    assert_eq!(s1.binds[0].source, first, "shared: the first cache");
    assert_eq!(s2.binds[0].source, first);
    s1.finish();
    s2.finish();
    // A key with one cache: a locked user waits while it is held.
    m.target = "/locked".into();
    m.sharing = Some(Sharing::Locked);
    let held = run_mounts(&[m.clone()], Path::new("/"), &store, &secrets, &never).unwrap();
    let cancelled = run_mounts(&[m], Path::new("/"), &store, &secrets, &|| true);
    assert!(
        matches!(cancelled, Err(Error::Store(rnk_store::Error::Cancelled))),
        "{cancelled:?}"
    );
    held.finish();
}

/// One RUN mounting a locked key twice shares one lease, as BuildKit
/// shares one ref per key within an exec, instead of waiting for itself.
#[test]
fn a_key_mounted_twice_by_one_run_uses_one_lease() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let mut a = mount(MountType::Cache);
    a.target = "/a".into();
    a.cache_id = "k".into();
    a.sharing = Some(Sharing::Locked);
    let mut b = a.clone();
    b.target = "/b".into();
    let r = run_mounts(&[a, b], Path::new("/"), &store, &BTreeMap::new(), &|| true).unwrap();
    assert_eq!(r.binds.len(), 2);
    assert_eq!(r.binds[0].source, r.binds[1].source);
    assert_eq!(r.binds[0].target, PathBuf::from("/a"));
    assert_eq!(r.binds[1].target, PathBuf::from("/b"));
    r.finish();
}

/// Two RUNs with the same locked caches in opposite orders: both take
/// them in key order, so neither holds one the other waits for.
#[test]
fn locked_caches_in_opposite_orders_do_not_deadlock() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let locked = |id: &str, target: &str| {
        let mut m = mount(MountType::Cache);
        m.cache_id = id.into();
        m.target = target.into();
        m.sharing = Some(Sharing::Locked);
        m
    };
    let ab = [locked("a", "/a"), locked("b", "/b")];
    let ba = [locked("b", "/b"), locked("a", "/a")];
    std::thread::scope(|s| {
        for mounts in [&ab, &ba, &ab, &ba] {
            let store = &store;
            s.spawn(move || {
                for _ in 0..20 {
                    let r = run_mounts(mounts, Path::new("/"), store, &BTreeMap::new(), &never)
                        .unwrap();
                    std::thread::sleep(std::time::Duration::from_millis(1));
                    r.finish();
                }
            });
        }
    });
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

    // The id from the target's last name; 0400 and root's by default; no
    // copy anywhere: the sandbox writes it on the RUN's tmpfs.
    let mut m = mount(MountType::Secret);
    m.target = "/etc/token".into();
    let r = one(&m).unwrap();
    assert!(r.binds.is_empty());
    assert_eq!(
        r.secrets,
        vec![SecretFile {
            data: b"s3cret".as_slice().into(),
            target: "/etc/token".into(),
            uid: 0,
            gid: 0,
            mode: 0o400,
        }]
    );
    assert!(
        !format!("{r:?}").contains("s3cret"),
        "Debug hides the bytes"
    );
    r.finish();

    // The target from the id; source= names the id; mode and owner are
    // honoured, permission bits only.
    let mut m = mount(MountType::Secret);
    m.source = "token".into();
    m.mode = Some(0o4755);
    m.uid = Some(1000);
    m.gid = Some(1001);
    let f = &one(&m).unwrap().secrets[0];
    assert_eq!(f.target, PathBuf::from("/run/secrets/token"));
    assert_eq!((f.uid, f.gid, f.mode), (1000, 1001, 0o755));

    // env= alone: a variable, no file; with a target: both.
    let mut m = mount(MountType::Secret);
    m.cache_id = "token".into();
    m.env = Some("TOKEN".into());
    let r = one(&m).unwrap();
    assert!(r.secrets.is_empty());
    assert_eq!(r.env, vec![("TOKEN".into(), "s3cret".into())]);
    assert!(!format!("{r:?}").contains("s3cret"), "Debug hides values");
    m.target = "t".into();
    let r = one(&m).unwrap();
    assert_eq!(r.secrets[0].target, PathBuf::from("/app/t"));
    assert_eq!(r.env.len(), 1);

    // An unset variable reads as empty, as BuildKit's os.Getenv.
    let mut m = mount(MountType::Secret);
    m.cache_id = "env".into();
    m.env = Some("E".into());
    assert_eq!(one(&m).unwrap().env, vec![("E".into(), "".into())]);

    // Missing: left out unless required; no id at all is an error.
    let mut m = mount(MountType::Secret);
    m.cache_id = "other".into();
    let r = one(&m).unwrap();
    assert!(r.secrets.is_empty() && r.env.is_empty());
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
        "nothing written"
    );
}

/// A NUL byte cannot be in a variable; the error names the id, never the
/// value.
#[test]
fn a_nul_in_an_env_secret_is_refused_without_its_value() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let host = dir.path().join("token");
    fs::write(&host, b"top\0secret").unwrap();
    let secrets: BTreeMap<String, Secret> = [("token".to_owned(), Secret::File(host))].into();
    let mut m = mount(MountType::Secret);
    m.cache_id = "token".into();
    m.env = Some("TOKEN".into());
    let e = run_mounts(&[m], Path::new("/"), &store, &secrets, &never)
        .unwrap_err()
        .to_string();
    assert!(e.contains("\"token\""), "{e}");
    assert!(!e.contains("top"), "the value never: {e}");
}

#[test]
fn a_secret_over_the_limit_or_not_a_file_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("big");
    fs::write(
        &path,
        vec![b'x'; usize::try_from(MAX_SECRET_SIZE).unwrap() + 1],
    )
    .unwrap();
    assert!(Secret::File(path.clone()).read("big").is_err());
    assert!(Secret::File(path.clone()).check("big").is_err());
    fs::write(&path, vec![b'x'; usize::try_from(MAX_SECRET_SIZE).unwrap()]).unwrap();
    assert!(Secret::File(path.clone()).read("big").is_ok());
    assert!(Secret::File(path).check("big").is_ok());
    // A FIFO is refused at once, never opened for a blocking read.
    let fifo = dir.path().join("fifo");
    rustix::fs::mknodat(
        rustix::fs::CWD,
        &fifo,
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::from_raw_mode(0o600),
        0,
    )
    .unwrap();
    assert!(Secret::File(fifo.clone()).check("f").is_err());
    assert!(Secret::File(fifo).read("f").is_err());
    assert!(Secret::File(dir.path().to_owned()).check("d").is_err());
}

/// A new cache's owner and mode are the store's to set, once.
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
    r.finish();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o750)).unwrap();
    let r = run_mounts(&[m], Path::new("/"), &store, &BTreeMap::new(), &never).unwrap();
    assert_eq!(r.binds[0].source, path);
    let mode = fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o7777, 0o750, "not applied again");
    r.finish();
}

/// One secret mounted many times is read once and shared, so the sandbox
/// writes one file for all its targets.
#[test]
fn a_secret_mounted_twice_is_read_once() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let host = dir.path().join("token");
    fs::write(&host, b"s").unwrap();
    let secrets: BTreeMap<String, Secret> = [("token".to_owned(), Secret::File(host))].into();
    let mut a = mount(MountType::Secret);
    a.cache_id = "token".into();
    a.target = "/a".into();
    let mut b = a.clone();
    b.target = "/b".into();
    let r = run_mounts(&[a, b], Path::new("/"), &store, &secrets, &never).unwrap();
    assert_eq!(r.secrets.len(), 2);
    assert!(Arc::ptr_eq(&r.secrets[0].data, &r.secrets[1].data));
}

/// A `Direct` `RUN` keeps a secret as a variable only and runs without its
/// cache mounts; a secret file is refused (ADR-0057, Q-380, Q-381).
#[test]
fn direct_runs_take_secrets_as_variables_and_drop_caches() {
    let mut env = mount(MountType::Secret);
    env.cache_id = "tok".into();
    env.env = Some("TOK".into());
    let mut cache = mount(MountType::Cache);
    cache.target = "/cache".into();
    cache.sharing = Some(Sharing::Locked);
    let kept = direct_mounts(&[cache.clone(), env.clone()]).unwrap();
    assert_eq!(kept, vec![env.clone()]);
    let mut file = env.clone();
    file.target = "/run/secrets/tok".into();
    let e = direct_mounts(&[file]).unwrap_err().to_string();
    assert!(e.contains("env=<NAME>"), "{e}");
    let mut plain = env;
    plain.env = None;
    assert!(direct_mounts(&[plain]).is_err());
    // A cache with `from=` is refused, not dropped.
    cache.from = "stage".into();
    assert!(direct_mounts(&[cache]).is_err());
}

/// An unsupported mount after a secret is refused before the secret is
/// read, in every mode: here reading it would fail with another error.
#[test]
fn unsupported_mounts_are_refused_before_any_secret_is_read() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let secrets: BTreeMap<String, Secret> =
        [("tok".to_owned(), Secret::File(dir.path().join("missing")))].into();
    let mut env = mount(MountType::Secret);
    env.cache_id = "tok".into();
    env.env = Some("TOK".into());
    let mut bind = mount(MountType::Bind);
    bind.target = "/src".into();
    let e = run_mounts(
        &[env.clone(), bind.clone()],
        Path::new("/"),
        &store,
        &secrets,
        &never,
    )
    .unwrap_err()
    .to_string();
    assert!(e.contains("type=bind"), "{e}");
    let e = direct_mounts(&[env, bind]).unwrap_err().to_string();
    assert!(e.contains("type=bind"), "{e}");
}
