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

/// `/home/u/w` under a `0700` home: the chain ADR-0047 accepts.
fn home_chain() -> Vec<ChainDir> {
    vec![
        dir("/home/u/w", 0, 0o700),
        dir("/home/u", 1000, 0o700),
        dir("/home", 0, 0o755),
        dir("/", 0, 0o755),
    ]
}

fn chain_refusal(chain: &[ChainDir]) -> String {
    check_chain(chain).unwrap_err().0
}

#[test]
fn a_private_chain_passes_also_through_a_root_sticky_tmp() {
    check_chain(&home_chain()).unwrap();
    check_chain(&[
        dir("/tmp/b/w", 0, 0o755),
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
fn an_ancestor_writable_by_others_is_refused_unless_root_sticky() {
    let mut c = home_chain();
    c[2].mode = 0o777;
    assert!(chain_refusal(&c).contains("/home is writable"));
    let mut c = home_chain();
    c[1].mode = 0o1777;
    let why = chain_refusal(&c);
    assert!(
        why.contains("/home/u is writable"),
        "sticky but not root's: {why}"
    );
}

#[test]
fn two_other_owners_in_the_chain_are_refused() {
    let mut c = home_chain();
    c[2].uid = 1001;
    assert!(chain_refusal(&c).contains("uid 1001"));
}

#[test]
fn a_chain_others_can_search_is_refused() {
    let mut c = home_chain();
    c[1].mode = 0o750;
    assert!(
        chain_refusal(&c).contains("0700"),
        "group may search /home/u"
    );
    let mut c = home_chain();
    c[1].mode = 0o701;
    assert!(chain_refusal(&c).contains("0700"));
}
