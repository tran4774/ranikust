use std::fs;
use std::os::unix::fs::symlink;

use super::*;

fn refused(dir: &Path) -> String {
    claim(dir).unwrap_err().0
}

#[test]
fn the_root_and_system_directories_are_refused() {
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
    let claimed = claim(&dir).unwrap();
    assert_eq!(claimed.path(), fs::canonicalize(&dir).unwrap());
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
    let claimed = claim(&link).unwrap();
    assert_eq!(claimed.path(), fs::canonicalize(&real).unwrap());
    assert!(real.join(MARKER).is_file());
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
