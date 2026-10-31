use std::fs;
use std::os::unix::fs::{PermissionsExt as _, symlink};

use super::*;

fn dir(path: &str, uid: u32, mode: u32) -> ChainDir {
    ChainDir {
        path: PathBuf::from(path),
        uid,
        mode,
    }
}

/// `/var/lib/ranikust` as root: the default ADR-0050 accepts.
fn default_chain() -> Vec<ChainDir> {
    vec![
        dir("/var/lib/ranikust", 0, 0o700),
        dir("/var/lib", 0, 0o755),
        dir("/var", 0, 0o755),
        dir("/", 0, 0o755),
    ]
}

fn refusal(chain: &[ChainDir], has_state: bool) -> String {
    check_chain(chain, 0, has_state).unwrap_err().0
}

#[test]
fn the_default_and_roots_sticky_directory_pass() {
    check_chain(&default_chain(), 0, true).unwrap();
    check_chain(
        &[
            dir("/tmp/s", 0, 0o700),
            dir("/tmp", 0, 0o1777),
            dir("/", 0, 0o755),
        ],
        0,
        true,
    )
    .unwrap();
}

#[test]
fn a_state_directory_of_another_user_or_writable_by_others_is_refused() {
    let mut c = default_chain();
    c[0].uid = 1000;
    assert!(refusal(&c, true).contains("owned by uid 1000, not 0"));
    let mut c = default_chain();
    c[0].mode = 0o770;
    assert!(refusal(&c, true).contains("writable by group or others"));
}

#[test]
fn a_parent_another_user_owns_or_may_write_is_refused() {
    // A home directory: its owner could rename the state directory away.
    let home = vec![
        dir("/home/u/state", 0, 0o700),
        dir("/home/u", 1000, 0o700),
        dir("/home", 0, 0o755),
        dir("/", 0, 0o755),
    ];
    let why = refusal(&home, true);
    assert!(why.contains("/home/u is owned by uid 1000"), "{why}");
    assert!(why.contains("/var/lib/ranikust"), "{why}");
    let mut c = default_chain();
    c[1].mode = 0o777;
    assert!(refusal(&c, true).contains("/var/lib is writable"));
    // A sticky directory someone else owns is no protection.
    let foreign_sticky = vec![
        dir("/scratch/s", 0, 0o700),
        dir("/scratch", 1000, 0o1777),
        dir("/", 0, 0o755),
    ];
    assert!(refusal(&foreign_sticky, true).contains("/scratch is owned by uid 1000"));
}

#[test]
fn before_creation_every_entry_is_a_parent() {
    // `/var/lib` exists, the state directory not yet: `/var/lib` is judged
    // as a parent, not as the state directory.
    check_chain(&default_chain()[1..], 0, false).unwrap();
    let mut c = default_chain()[1..].to_vec();
    c[0].mode = 0o777;
    assert!(refusal(&c, false).contains("/var/lib is writable"));
}

/// A fresh temp dir no one else may write, whatever the umask.
fn private_tempdir() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o700)).unwrap();
    tmp
}

#[test]
fn prepare_creates_a_private_state_directory() {
    let tmp = private_tempdir();
    let state = tmp.path().join("a/b/state");
    prepare(&state).unwrap();
    let mode = fs::metadata(&state).unwrap().permissions().mode() & 0o7777;
    assert_eq!(mode, 0o700);
    // An existing one others may search is made private.
    let open = tmp.path().join("open");
    fs::create_dir(&open).unwrap();
    fs::set_permissions(&open, fs::Permissions::from_mode(0o755)).unwrap();
    prepare(&open).unwrap();
    assert_eq!(
        fs::metadata(&open).unwrap().permissions().mode() & 0o7777,
        0o700
    );
}

#[test]
fn prepare_refuses_without_creating_anything() {
    let tmp = private_tempdir();
    // A parent others may write: nothing is created below it.
    let shared = tmp.path().join("shared");
    fs::create_dir(&shared).unwrap();
    fs::set_permissions(&shared, fs::Permissions::from_mode(0o777)).unwrap();
    let why = prepare(&shared.join("state")).unwrap_err().0;
    assert!(why.contains("writable by group or others"), "{why}");
    assert!(!shared.join("state").exists());
    // A symlink on the way is never followed.
    let real = tmp.path().join("real");
    fs::create_dir(&real).unwrap();
    let link = tmp.path().join("link");
    symlink(&real, &link).unwrap();
    let why = prepare(&link.join("state")).unwrap_err().0;
    assert!(why.contains("without following a symlink"), "{why}");
    assert!(!real.join("state").exists());
}

#[test]
fn a_plain_directory_is_not_overlayfs() {
    let tmp = tempfile::tempdir().unwrap();
    // The test tree may itself sit on overlayfs in a container; only check
    // that the call answers for a real directory and refuses a missing one.
    let _ = on_overlayfs(tmp.path());
    assert!(!on_overlayfs(&tmp.path().join("missing")));
}

#[test]
fn the_nofile_limit_ends_at_the_hard_limit() {
    let hard = rustix::process::getrlimit(rustix::process::Resource::Nofile).maximum;
    assert_eq!(raise_nofile_limit().unwrap(), hard);
    let now = rustix::process::getrlimit(rustix::process::Resource::Nofile);
    assert_eq!(now.current, hard);
}
