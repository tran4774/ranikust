use std::fs;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _, symlink};

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
fn a_user_s_default_below_home_passes_the_same_checks() {
    // The default of a user other than root, below a home directory of
    // theirs; checked as whoever runs the test.
    let euid = rustix::process::geteuid().as_raw();
    let tmp = private_tempdir();
    let home = tmp.path().join("home");
    fs::create_dir(&home).unwrap();
    fs::set_permissions(&home, fs::Permissions::from_mode(0o755)).unwrap();
    let state = crate::config::default_state_dir(1000, Some(home.as_os_str())).unwrap();
    assert_eq!(state, home.join(".local/share/ranikust"));
    prepare(&state).unwrap();
    for d in [".local", ".local/share", ".local/share/ranikust"] {
        let st = fs::metadata(home.join(d)).unwrap();
        assert_eq!((st.uid(), st.mode() & 0o7777), (euid, 0o700), "{d}");
    }
    // A home its group may write: refused, saying what to do instead.
    let shared = tmp.path().join("shared");
    fs::create_dir(&shared).unwrap();
    fs::set_permissions(&shared, fs::Permissions::from_mode(0o775)).unwrap();
    let state = crate::config::default_state_dir(1000, Some(shared.as_os_str())).unwrap();
    let why = prepare(&state).unwrap_err().0;
    assert!(why.contains("writable by group or others"), "{why}");
    assert!(why.ends_with(private_state(euid)), "{why}");
    assert!(!shared.join(".local").exists());
}

#[test]
fn the_advice_names_the_default_of_the_effective_user() {
    assert!(private_state(0).contains("/var/lib/ranikust"));
    assert!(!private_state(0).contains("you"));
    for euid in [1, 1000, u32::MAX] {
        assert!(private_state(euid).contains("~/.local/share/ranikust"));
        assert!(private_state(euid).contains("only you or root"));
    }
    // The default may be the directory refused: never offered as the fix.
    for euid in [0, 1000] {
        assert!(!private_state(euid).contains("use the default"));
    }
}

/// What the commands a refusal prints require of a state directory: the
/// one checked, the build's rule with its owner as the user, and no system
/// directory, marked or not, even by another name; its mode is returned.
#[test]
fn the_commands_take_only_a_directory_a_build_accepts() {
    let tmp = private_tempdir();
    let state = tmp.path().join("state");
    fs::create_dir(&state).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o750)).unwrap();
    fs::write(state.join(rnk_store::STATE_MARKER), b"").unwrap();
    let check = rnk_store::Store::check_prune(&state).unwrap();
    let other = tmp.path().join("other");
    fs::create_dir(&other).unwrap();
    let mode = check_for_commands_beside(&state, &check, &[other.as_path()]).unwrap();
    assert_eq!(mode, 0o750);
    assert_eq!(check_for_commands(&state, &check).unwrap(), 0o750);
    let link = tmp.path().join("link");
    symlink(&state, &link).unwrap();
    let why = check_for_commands_beside(&state, &check, &[other.as_path(), link.as_path()])
        .unwrap_err()
        .0;
    assert!(
        why.contains(&format!("is the system directory {}", link.display())),
        "{why}"
    );
    for changed in [
        rnk_store::PruneCheck {
            ino: check.ino + 1,
            ..check
        },
        rnk_store::PruneCheck {
            uid: check.uid + 1,
            ..check
        },
    ] {
        let why = check_for_commands(&state, &changed).unwrap_err().0;
        assert!(why.contains("changed while ranikust checked it"), "{why}");
    }
    for mode in [0o1777, 0o770, 0o702] {
        fs::set_permissions(&state, fs::Permissions::from_mode(mode)).unwrap();
        let why = check_for_commands(&state, &check).unwrap_err().0;
        assert!(why.contains("is writable by group or others"), "{why}");
    }
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

/// A path through `..` names, in a refusal, the parent that breaks the
/// rule, not the one its text has at that depth.
#[test]
fn a_refusal_through_dotdot_names_the_parent_that_breaks_the_rule() {
    let tmp = private_tempdir();
    let open = tmp.path().join("open");
    let state = open.join("s/state");
    fs::create_dir_all(&state).unwrap();
    fs::set_permissions(open.join("s"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&open, fs::Permissions::from_mode(0o777)).unwrap();
    let given = state.join("../state");
    let want = format!("unsafe state directory: {} is writable", open.display());
    let why = check(&given).unwrap_err().0;
    assert!(why.contains(&want), "{why}");
    let why = prepare(&given).unwrap_err().0;
    assert!(why.contains(&want), "{why}");
}

/// What another local user can do in a sticky `/tmp` after `prepare` found
/// the nearest existing directory and before it creates the rest: plant a
/// symlink, or a directory of their own, at the first missing name.
#[test]
fn missing_directories_are_made_below_the_checked_one_never_through_a_symlink() {
    let tmp = private_tempdir();
    let euid = rustix::process::geteuid().as_raw();
    let victim = private_tempdir();
    let at = || open_dir(tmp.path(), OFlags::PATH).unwrap();
    symlink(victim.path(), tmp.path().join("a")).unwrap();
    let state = tmp.path().join("a/b/state");
    let why = create_missing(at(), tmp.path(), &state, euid)
        .unwrap_err()
        .0;
    assert!(why.contains("without following a symlink"), "{why}");
    assert_eq!(fs::read_dir(victim.path()).unwrap().count(), 0);
    // A directory others may write, made in the meantime, is refused before
    // anything is created in it.
    let open = tmp.path().join("open");
    fs::create_dir(&open).unwrap();
    fs::set_permissions(&open, fs::Permissions::from_mode(0o777)).unwrap();
    let why = create_missing(at(), tmp.path(), &open.join("b/state"), euid)
        .unwrap_err()
        .0;
    assert!(why.contains("writable by group or others"), "{why}");
    assert_eq!(fs::read_dir(&open).unwrap().count(), 0);
    // Otherwise every missing directory is made, mode 0700.
    let state = tmp.path().join("c/d/state");
    let fd = create_missing(at(), tmp.path(), &state, euid).unwrap();
    let st = rustix::fs::fstat(&fd).unwrap();
    let new = fs::metadata(&state).unwrap();
    assert_eq!((st.st_dev, st.st_ino), (new.dev(), new.ino()));
    for dir in [tmp.path().join("c"), tmp.path().join("c/d"), state] {
        let mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode & 0o077, 0, "{}", dir.display());
    }
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
    // The marker of a prune cut short counts: the store refuses a build
    // there itself, and prune --all finishes it.
    let pruning = tmp.path().join("pruning");
    fs::create_dir(&pruning).unwrap();
    fs::set_permissions(&pruning, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(pruning.join("data"), b"x").unwrap();
    let marker = pruning.join(rnk_store::PRUNING_MARKER);
    symlink("/etc/hostname", &marker).unwrap();
    let why = prepare(&pruning).unwrap_err().0;
    assert!(why.contains("not a state directory"), "{why}");
    let mode = |dir: &Path| fs::metadata(dir).unwrap().permissions().mode() & 0o7777;
    assert_eq!(mode(&pruning), 0o755);
    fs::remove_file(&marker).unwrap();
    fs::write(&marker, b"").unwrap();
    prepare(&pruning).unwrap();
    assert_eq!(mode(&pruning), 0o700);
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
        let Err(why) = accept_existing(&fd, Path::new(sys), 0) else {
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

/// A `$HOME` that reaches its directory through a `/proc` link (T4): the
/// default below it is refused, and nothing is made where the link leads.
#[test]
fn red_team_a_home_through_a_proc_link_is_refused_and_nothing_is_made() {
    use std::os::fd::AsRawFd as _;
    let tmp = private_tempdir();
    let home = tmp.path().join("home");
    fs::create_dir(&home).unwrap();
    fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
    let fd = fs::File::open(&home).unwrap();
    let n = fd.as_raw_fd();
    for via in [
        format!("/proc/self/fd/{n}"),
        format!("/proc/{}/fd/{n}", std::process::id()),
        format!("/proc/thread-self/fd/{n}"),
        format!("/proc/self/root{}", home.display()),
        format!("/dev/fd/{n}"),
    ] {
        let state =
            crate::config::default_state_dir(1000, Some(std::ffi::OsStr::new(&via))).unwrap();
        let why = prepare(&state).unwrap_err().0;
        assert!(why.contains("without following a symlink"), "{via}: {why}");
        assert_eq!(fs::read_dir(&home).unwrap().count(), 0, "{via}");
    }
}
