use std::fs;
use std::os::unix::fs::symlink;

use super::*;

fn refused(dir: &Path) -> String {
    claim(dir).unwrap_err().0
}

#[test]
fn system_directories_and_their_ancestors_are_refused_by_name() {
    for dir in ["/usr", "/usr/local", "/var", "/var/lib", "/tmp", "/home"] {
        assert!(system_by_name(Path::new(dir)).is_some(), "{dir}");
    }
    for dir in ["/home/user/w", "/tmp/w", "/srv/build", "/usr/share"] {
        assert_eq!(system_by_name(Path::new(dir)), None, "{dir}");
    }
}

#[test]
fn a_system_directory_is_refused_by_identity() {
    let usr = dir_id(Path::new("/usr")).unwrap();
    assert_eq!(same_as_system(usr).as_deref(), Some("/usr"));
    let root = dir_id(Path::new("/")).unwrap();
    assert!(same_as_system(root).unwrap().starts_with("/ (an ancestor"));
    let tmp = tempfile::tempdir().unwrap();
    assert_eq!(same_as_system(dir_id(tmp.path()).unwrap()), None);
}

#[test]
fn the_root_and_system_directories_are_refused_without_a_write() {
    for dir in [
        "/", "//", "/usr/../", "/usr", "/usr/", "/etc", "/var", "/var/lib", "/tmp",
    ] {
        let dir = Path::new(dir);
        if !dir.exists() {
            continue;
        }
        let why = refused(dir);
        assert!(
            why.contains("host's /") || why.contains("system directory"),
            "{}: {why}",
            dir.display()
        );
        assert!(!dir.join(MARKER).exists(), "{}", dir.display());
    }
}

#[test]
fn a_symlink_to_a_system_directory_is_refused_by_its_target() {
    let tmp = tempfile::tempdir().unwrap();
    let link = tmp.path().join("usr");
    symlink("/usr", &link).unwrap();
    let why = refused(&link);
    assert!(why.contains("system directory /usr"), "{why}");
}

#[test]
fn an_empty_workdir_gets_the_marker_and_the_lock() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("w");
    fs::create_dir(&dir).unwrap();
    let mut claimed = claim(&dir).unwrap();
    assert_eq!(claimed.path(), fs::canonicalize(&dir).unwrap());
    assert!(!dir.join(MARKER).exists(), "claim writes nothing");
    claimed.mark().unwrap();
    let marker = fs::symlink_metadata(dir.join(MARKER)).unwrap();
    assert!(marker.is_file());
    assert_eq!(claimed.root().read_names("/").unwrap(), [MARKER]);

    let why = refused(&dir);
    assert!(why.contains("another build"), "{why}");
    drop(claimed);
    // The marker is there now: the directory is claimed again.
    claim(&dir).unwrap();
}

#[test]
fn a_non_empty_workdir_needs_the_marker() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("w");
    fs::create_dir(&dir).unwrap();
    fs::write(dir.join("precious"), b"x").unwrap();
    let why = refused(&dir);
    assert!(why.contains("not empty") && why.contains(MARKER), "{why}");
    assert!(!dir.join(MARKER).exists(), "nothing is written on refusal");

    fs::write(dir.join(MARKER), b"").unwrap();
    claim(&dir).unwrap();
}

#[test]
fn a_marker_that_is_not_a_regular_file_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    for (name, make) in [
        (
            "link",
            (|m: &Path| symlink("/etc/passwd", m)) as fn(&Path) -> std::io::Result<()>,
        ),
        ("dir", |m: &Path| fs::create_dir(m)),
    ] {
        let dir = tmp.path().join(name);
        fs::create_dir(&dir).unwrap();
        make(&dir.join(MARKER)).unwrap();
        let why = refused(&dir);
        assert!(why.contains("not a regular file"), "{name}: {why}");
    }
}

#[test]
fn a_symlinked_workdir_is_resolved_first() {
    let tmp = tempfile::tempdir().unwrap();
    let real = tmp.path().join("real");
    fs::create_dir(&real).unwrap();
    let link = tmp.path().join("link");
    symlink(&real, &link).unwrap();
    let mut claimed = claim(&link).unwrap();
    assert_eq!(claimed.path(), fs::canonicalize(&real).unwrap());
    claimed.mark().unwrap();
    assert!(real.join(MARKER).is_file());
}

#[test]
fn a_workdir_filled_before_marking_loses_the_marker() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("w");
    fs::create_dir(&dir).unwrap();
    let mut claimed = claim(&dir).unwrap();
    fs::write(dir.join("late"), b"x").unwrap();
    let why = claimed.mark().unwrap_err().0;
    assert!(
        why.contains("was filled") && why.contains("removed again"),
        "{why}"
    );
    assert!(!dir.join(MARKER).exists());
}

#[test]
fn a_missing_workdir_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let why = refused(&tmp.path().join("missing"));
    assert!(why.contains("cannot resolve"), "{why}");
}

#[test]
fn the_nofile_limit_ends_at_the_hard_limit() {
    let hard = rustix::process::getrlimit(rustix::process::Resource::Nofile).maximum;
    assert_eq!(raise_nofile_limit().unwrap(), hard);
    let now = rustix::process::getrlimit(rustix::process::Resource::Nofile);
    assert_eq!(now.current, hard);
}

fn dir(path: &str, uid: u32, mode: u32) -> ChainDir {
    ChainDir {
        path: PathBuf::from(path),
        uid,
        mode,
    }
}

/// `/home/u/rnk/w` with `rnk` root's `0700`: the chain ADR-0047 accepts;
/// `/home/u` may be the user's.
fn home_chain() -> Vec<ChainDir> {
    vec![
        dir("/home/u/rnk/w", 0, 0o755),
        dir("/home/u/rnk", 0, 0o700),
        dir("/home/u", 1000, 0o750),
        dir("/home", 0, 0o755),
        dir("/", 0, 0o755),
    ]
}

fn chain_refusal(chain: &[ChainDir]) -> String {
    check_chain(chain).unwrap_err().0
}

#[test]
fn a_root_private_ancestor_passes_whoever_owns_what_is_above() {
    check_chain(&home_chain()).unwrap();
    check_chain(&[
        dir("/tmp/b/w", 0, 0o700),
        dir("/tmp/b", 0, 0o700),
        dir("/tmp", 0, 0o1777),
        dir("/", 0, 0o755),
    ])
    .unwrap();
}

#[test]
fn a_workdir_not_owned_by_root_or_writable_by_others_is_refused() {
    let mut c = home_chain();
    c[0].uid = 1000;
    assert!(chain_refusal(&c).contains("not root"));
    let mut c = home_chain();
    c[0].mode = 0o770;
    assert!(chain_refusal(&c).contains("writable by group or others"));
}

#[test]
fn a_private_ancestor_of_another_user_is_refused() {
    // `/home/u/w` with only `/home/u` private: u reaches the setuid files.
    let c = vec![
        dir("/home/u/w", 0, 0o700),
        dir("/home/u", 1000, 0o700),
        dir("/home", 0, 0o755),
        dir("/", 0, 0o755),
    ];
    let why = chain_refusal(&c);
    assert!(why.contains("/home/u is owned by uid 1000"), "{why}");
    assert!(why.contains("install -d -m 0700"), "{why}");
}

#[test]
fn a_writable_directory_below_the_private_one_is_refused() {
    let mut c = home_chain();
    c.insert(1, dir("/home/u/rnk/mid", 0, 0o777));
    c[0].path = PathBuf::from("/home/u/rnk/mid/w");
    assert!(chain_refusal(&c).contains("mid is writable"));
}

#[test]
fn a_chain_others_can_search_is_refused() {
    let mut c = home_chain();
    c[1].mode = 0o750;
    let why = chain_refusal(&c);
    assert!(
        why.contains("not root") || why.contains("keeps group"),
        "{why}"
    );
    let all_open = vec![
        dir("/srv/w", 0, 0o755),
        dir("/srv", 0, 0o755),
        dir("/", 0, 0o755),
    ];
    assert!(chain_refusal(&all_open).contains("keeps group and others out"));
}

#[test]
fn the_real_parents_are_walked_from_the_descriptor() {
    // As a test user nothing is root's: the walk reaches the temp dir's
    // owner and stops there; what matters is that it read the chain.
    let tmp = tempfile::tempdir().unwrap();
    let wd = tmp.path().join("w");
    fs::create_dir(&wd).unwrap();
    let claimed = claim(&wd).unwrap();
    if rustix::process::geteuid().is_root() {
        return;
    }
    let why = check_private(&claimed).unwrap_err().0;
    assert!(
        why.contains(&format!("{} is owned by uid", wd.display())),
        "{why}"
    );
}
