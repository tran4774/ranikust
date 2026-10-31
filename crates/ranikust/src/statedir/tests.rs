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
fn a_directory_that_is_not_a_state_directory_keeps_its_mode() {
    let tmp = private_tempdir();
    // A mistyped `--state-dir`: not empty, no marker, mode 0755.
    let precious = tmp.path().join("precious");
    fs::create_dir(&precious).unwrap();
    fs::write(precious.join("data"), b"x").unwrap();
    fs::set_permissions(&precious, fs::Permissions::from_mode(0o755)).unwrap();
    let why = prepare(&precious).unwrap_err().0;
    assert!(why.contains("not a state directory"), "{why}");
    assert_eq!(
        fs::metadata(&precious).unwrap().permissions().mode() & 0o7777,
        0o755
    );
    // An earlier ranikust's workdir or store: refused, named.
    fs::write(precious.join(".ranikust-root"), b"").unwrap();
    let why = prepare(&precious).unwrap_err().0;
    assert!(why.contains(".ranikust-root"), "{why}");
    // With the store's marker it is a state directory: made private.
    fs::write(precious.join(rnk_store::STATE_MARKER), b"").unwrap();
    prepare(&precious).unwrap();
    assert_eq!(
        fs::metadata(&precious).unwrap().permissions().mode() & 0o7777,
        0o700
    );
    // A marker that is not a regular file does not count.
    let linked = tmp.path().join("linked");
    fs::create_dir(&linked).unwrap();
    symlink("/etc/hostname", linked.join(rnk_store::STATE_MARKER)).unwrap();
    assert!(prepare(&linked).is_err());
}

#[test]
fn a_system_directory_is_never_taken_unmarked() {
    // Whichever of them exist here, empty or not, as any user: refused
    // without a change of mode.
    for sys in SYSTEM_DIRS {
        let dir = Path::new(sys);
        let Ok(before) = fs::metadata(dir) else {
            continue;
        };
        assert!(prepare(dir).is_err(), "{sys}");
        let after = fs::metadata(dir).unwrap();
        assert_eq!(
            before.permissions().mode(),
            after.permissions().mode(),
            "{sys}"
        );
    }
}

#[test]
fn the_system_directory_check_itself_refuses_them() {
    // The check `prepare` runs past the owner rule, reading only: every
    // system directory present is refused, an empty one by its identity.
    for sys in SYSTEM_DIRS {
        let Ok(fd) = open_dir(Path::new(sys), OFlags::RDONLY) else {
            continue;
        };
        let Err(why) = accept_existing(&fd, Path::new(sys)) else {
            panic!("{sys} accepted");
        };
        assert!(
            why.0.contains("system directory") || why.0.contains("not a state directory"),
            "{sys}: {}",
            why.0
        );
    }
}

#[test]
fn an_empty_directory_elsewhere_is_taken() {
    // A fresh volume mounted at any other path: accepted, made private.
    let tmp = private_tempdir();
    let vol = tmp.path().join("vol");
    fs::create_dir(&vol).unwrap();
    fs::set_permissions(&vol, fs::Permissions::from_mode(0o755)).unwrap();
    prepare(&vol).unwrap();
    assert_eq!(
        fs::metadata(&vol).unwrap().permissions().mode() & 0o7777,
        0o700
    );
}

#[test]
fn the_root_directory_is_never_a_state_directory() {
    for root in ["/", "//", "/usr/.."] {
        let why = prepare(Path::new(root)).unwrap_err().0;
        assert!(why.contains("is /"), "{root}: {why}");
    }
}

#[test]
fn builds_is_made_private_too() {
    let tmp = private_tempdir();
    let state = tmp.path().join("state");
    fs::create_dir(&state).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(state.join(rnk_store::STATE_MARKER), b"").unwrap();
    fs::create_dir(state.join("builds")).unwrap();
    fs::set_permissions(state.join("builds"), fs::Permissions::from_mode(0o755)).unwrap();
    prepare(&state).unwrap();
    assert_eq!(
        fs::metadata(state.join("builds"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o700
    );
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

#[test]
fn the_traces_of_a_kaniko_build_cut_short_on_slash_are_taken() {
    // Its marker and an unpacked name: the store heeds and removes them
    // (ADR-0056, Q-360); anything else beside them is still refused.
    let tmp = private_tempdir();
    let state = tmp.path().join("state");
    fs::create_dir(&state).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(state.join(rnk_store::BUILDING_MARKER), b"").unwrap();
    fs::create_dir(state.join(format!("{}00ff", rnk_store::ANON_PREFIX))).unwrap();
    prepare(&state).unwrap();
    fs::write(state.join("data"), b"x").unwrap();
    let why = prepare(&state).unwrap_err().0;
    assert!(why.contains("not a state directory"), "{why}");
}
