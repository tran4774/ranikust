//! Deletion races and mounts in the state directory, which need the
//! store's own `RootPath`.

use std::fs;
use std::path::Path;

use super::*;

fn store(dir: &Path) -> Store {
    Store::open(&BuildPaths::new(dir, dir.join("state"))).unwrap()
}

thread_local! {
    /// How many more marker creations on this thread fail with `ENOSPC`,
    /// as on a filesystem a `RUN` filled.
    static MARKER_CREATE_FAILS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// What the state directory held at each marker creation on this
    /// thread: a `.ranikust-state`, and the forged `layers/`.
    static MARKER_CREATE_SAW: std::cell::RefCell<Vec<(bool, bool)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// The hook in `create_building_marker`, given the state directory:
/// records what it holds, and whether this creation fails.
pub(super) fn marker_create_fails(dir: &OwnedFd) -> bool {
    let holds =
        |name: &str| rustix::fs::statat(dir, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW).is_ok();
    MARKER_CREATE_SAW.with(|s| s.borrow_mut().push((holds(MARKER), holds("layers"))));
    MARKER_CREATE_FAILS.with(|n| {
        let left = n.get();
        n.set(left.saturating_sub(1));
        left > 0
    })
}

/// A state directory as a `RUN` leaves it after removing the marker:
/// a forged persistent store in its place.
fn forge_after_marker_removed(state: &Path) {
    fs::remove_file(state.join(BUILDING)).unwrap();
    fs::write(state.join(MARKER), b"").unwrap();
    fs::create_dir_all(state.join("layers/v3/sha256/0707")).unwrap();
    fs::write(state.join("layers/v3/sha256/0707/planted"), b"poison").unwrap();
}

#[test]
fn a_marker_the_close_cannot_make_at_first_is_made_once_the_directory_is_emptied() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    fs::create_dir(&state).unwrap();
    let store = Store::open_anonymous(&state).unwrap();
    forge_after_marker_removed(&state);
    // The first creation fails, as on a filesystem a RUN filled.
    MARKER_CREATE_FAILS.with(|n| n.set(1));
    store.close().unwrap();
    let left: Vec<_> = fs::read_dir(&state)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(left, [BUILDING], "only the marker stays");
    assert!(
        fs::symlink_metadata(state.join(BUILDING))
            .unwrap()
            .is_file()
    );
}

#[test]
fn a_forged_state_marker_goes_before_the_marker_is_tried_again() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    fs::create_dir(&state).unwrap();
    let store = Store::open_anonymous(&state).unwrap();
    forge_after_marker_removed(&state);
    MARKER_CREATE_SAW.with(|s| s.borrow_mut().clear());
    MARKER_CREATE_FAILS.with(|n| n.set(1));
    store.close().unwrap();
    let saw = MARKER_CREATE_SAW.with(|s| s.borrow().clone());
    // The first try saw the forged store; the second came once
    // `.ranikust-state` was gone, before the rest was emptied, so a kill
    // during a long emptying leaves no store a persistent build would open.
    assert_eq!(saw, [(true, true), (false, true)]);
}

#[test]
fn a_marker_name_held_by_what_a_run_made_is_named_in_the_error() {
    // Root removes the directory below whatever its mode.
    if rustix::process::geteuid().is_root() {
        return;
    }
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    fs::create_dir(&state).unwrap();
    let store = Store::open_anonymous(&state).unwrap();
    // A RUN replaces the marker with a directory this user cannot empty.
    let marker = state.join(BUILDING);
    fs::remove_file(&marker).unwrap();
    fs::create_dir_all(marker.join("locked")).unwrap();
    fs::write(marker.join("locked/kept"), b"").unwrap();
    fs::set_permissions(marker.join("locked"), fs::Permissions::from_mode(0o500)).unwrap();
    let e = store.close().unwrap_err();
    fs::set_permissions(marker.join("locked"), fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        matches!(
            e,
            Error::Unmarked {
                emptied: true,
                held: true,
                ..
            }
        ),
        "{e:?}"
    );
    let shown = e.to_string();
    assert!(
        shown.contains("emptied except the name .ranikust-building"),
        "{shown}"
    );
}

#[test]
fn a_marker_a_run_kept_from_being_made_is_made_after_its_run() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    fs::create_dir(&state).unwrap();
    let store = Store::open_anonymous(&state).unwrap();
    let run = store.enter_run().unwrap();
    forge_after_marker_removed(&state);
    // The RUN filled the filesystem: the first try fails.
    MARKER_CREATE_FAILS.with(|n| n.set(1));
    store.leave_run(run, true).unwrap();
    assert!(!state.join(MARKER).exists(), "the forged store marker goes");
    assert!(
        fs::symlink_metadata(state.join(BUILDING))
            .unwrap()
            .is_file()
    );
    drop(store);
}

#[test]
fn an_empty_marker_is_this_containers() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    fs::create_dir(&state).unwrap();
    // A marker made but left empty, its text unwritten (a full filesystem).
    fs::write(state.join(BUILDING), b"").unwrap();
    let root = RootPath::open(&state).unwrap();
    assert_eq!(
        read_building_marker(&root, &state, Some("pid:[1] boot:x")).unwrap(),
        Marker::Ours
    );
}

#[test]
fn a_marker_the_close_can_never_make_fails_it_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    fs::create_dir(&state).unwrap();
    let store = Store::open_anonymous(&state).unwrap();
    forge_after_marker_removed(&state);
    MARKER_CREATE_FAILS.with(|n| n.set(u32::MAX));
    let e = store.close().unwrap_err();
    MARKER_CREATE_FAILS.with(|n| n.set(0));
    assert!(
        matches!(
            e,
            Error::Unmarked {
                emptied: true,
                held: false,
                ..
            }
        ),
        "{e:?}"
    );
    let shown = e.to_string();
    assert!(shown.contains("nothing marks it"), "{shown}");
    assert!(shown.contains("delete this directory"), "{shown}");
    assert!(!shown.contains("keeps .ranikust-building"), "{shown}");
    // What the RUN forged is gone all the same.
    assert_eq!(fs::read_dir(&state).unwrap().count(), 0);
}

#[test]
fn a_directory_swapped_for_a_symlink_is_not_emptied() {
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path());
    let state = s.state_dir().to_owned();
    fs::create_dir_all(state.join("tmp/dead/sub")).unwrap();
    let st = s.state.statx("tmp/dead").unwrap();
    // The swap between the `statx` and the open: the name now leads to
    // the state directory itself.
    fs::rename(state.join("tmp/dead"), state.join("tmp/elsewhere")).unwrap();
    std::os::unix::fs::symlink("/", state.join("tmp/dead")).unwrap();

    let err = remove_dir_tree(&s.state, Path::new("tmp/dead"), &st).unwrap_err();
    assert!(matches!(err, Error::BadEntry { .. }), "{err}");
    for d in DIRS {
        assert!(state.join(d).is_dir(), "{d}");
    }
    assert!(state.join(MARKER).is_file());
    assert!(state.join("tmp/elsewhere/sub").is_dir());
}

#[test]
fn a_directory_swapped_for_another_is_not_emptied() {
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path());
    let state = s.state_dir().to_owned();
    fs::create_dir_all(state.join("tmp/dead")).unwrap();
    let st = s.state.statx("tmp/dead").unwrap();
    // The replacement exists before the original goes, so it cannot reuse
    // its inode number (some filesystems hand a freed one straight back).
    fs::create_dir_all(state.join("tmp/other/precious")).unwrap();
    fs::remove_dir(state.join("tmp/dead")).unwrap();
    fs::rename(state.join("tmp/other"), state.join("tmp/dead")).unwrap();

    let err = remove_dir_tree(&s.state, Path::new("tmp/dead"), &st).unwrap_err();
    assert!(matches!(err, Error::BadEntry { .. }), "{err}");
    assert!(state.join("tmp/dead/precious").is_dir());
}

#[test]
#[ignore = "T2c: sysadmin: bind-mounts in a private mount namespace"]
fn a_mount_where_clean_up_deletes_is_not_emptied() {
    // On a thread of its own in a new mount namespace, every mount private,
    // so nothing it mounts reaches another namespace.
    std::thread::spawn(|| {
        use rustix::mount::{MountPropagationFlags, mount_bind};
        use rustix::thread::UnshareFlags;
        // SAFETY: this thread shares nothing that the unshared filesystem
        // attributes or mount namespace would break; it ends with the test.
        unsafe { rustix::thread::unshare_unsafe(UnshareFlags::FS | UnshareFlags::NEWNS) }
            .expect("unshare(CLONE_FS | CLONE_NEWNS)");
        rustix::mount::mount_change(
            "/",
            MountPropagationFlags::PRIVATE | MountPropagationFlags::REC,
        )
        .expect("make every mount private");

        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("host-file"), "keep").unwrap();
        let state = dir.path().join("state");
        let dead = store(dir.path());
        let id = dead.build_id().to_owned();
        drop(dead);
        let target = state.join("tmp").join(&id);
        mount_bind(outside.path(), &target).unwrap();
        // Declared after both directories: dropped first, on a panic too.
        let bind = Detach(target);

        // Clean-up meets the mount on a dead build's directory: it warns
        // and goes on, and the mounted directory keeps its contents.
        let next = Store::open(&BuildPaths::new(dir.path(), &state));
        let kept = fs::read_to_string(outside.path().join("host-file"));
        drop(bind);
        let next = next.unwrap();
        assert_eq!(kept.unwrap(), "keep");

        // The same through a fill's discard path and `close`: a mount on
        // the build's own directory is refused, not emptied.
        mount_bind(outside.path(), next.build_dir()).unwrap();
        let bind = Detach(next.build_dir().to_owned());
        let st = next
            .state
            .statx(Path::new("builds").join(&next.id))
            .unwrap();
        let r = remove_dir_tree(&next.state, &Path::new("builds").join(&next.id), &st);
        let kept = fs::read_to_string(outside.path().join("host-file"));
        drop(bind);
        assert!(matches!(r, Err(Error::BadEntry { .. })), "{r:?}");
        assert_eq!(kept.unwrap(), "keep");
    })
    .join()
    .unwrap();
}

/// A mount a T2 test made below its temporary directories, detached on drop
/// (panics included) before they are removed. If it cannot be detached the
/// process aborts, so nothing is removed through it.
struct Detach(std::path::PathBuf);

impl Drop for Detach {
    fn drop(&mut self) {
        if let Err(e) = rustix::mount::unmount(&self.0, rustix::mount::UnmountFlags::DETACH) {
            eprintln!(
                "cannot detach {}: {e}; aborting to remove nothing",
                self.0.display()
            );
            std::process::abort();
        }
    }
}

/// The advice given when an anonymous store opened on the traces of a build
/// cut short, word for word, in its warning and in `named_window`'s refusal.
#[test]
fn the_cut_short_warning_and_refusal_give_the_same_advice() {
    assert_eq!(
        CUT_SHORT,
        "an earlier build in this container did not end cleanly and may have left RUN \
         processes; run ranikust as the container's PID 1, or restart the container, to keep \
         state on disk"
    );
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    fs::create_dir(&state).unwrap();
    // A marker of this container left behind: the next build is tainted.
    let first = Store::open_anonymous(&state).unwrap();
    let run = first.enter_run().unwrap();
    first.leave_run(run, false).unwrap();
    first.close().unwrap();
    let s = Store::open_anonymous(&state).unwrap();
    let e = s.named_window().unwrap_err();
    assert_eq!(
        e.to_string(),
        "kaniko mode on / refuses COPY --from a kept stage or an image, which unpacks it by \
         name: an earlier build in this container did not end cleanly and may have left RUN \
         processes; run ranikust as the container's PID 1, or restart the container (ADR-0066)"
    );
    s.close().unwrap();
}

/// Each refusal of an anonymous store says what it refuses and why, with
/// its decision: a failed `RUN`'s fence (ADR-0056) is not a store that keeps
/// nothing (ADR-0051).
#[test]
fn anonymous_refusals_say_why() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open_anonymous(dir.path()).unwrap();
    let never = || false;
    let e = s
        .cache_lease("k", crate::CacheSharing::Shared, None, &never)
        .unwrap_err();
    assert_eq!(
        e.to_string(),
        "kaniko mode on / refuses RUN --mount=type=cache: the store keeps nothing in the state \
         directory a RUN could reach (ADR-0051)"
    );
    let run = s.enter_run().unwrap();
    s.leave_run(run, false).unwrap();
    assert_eq!(
        s.named_window().unwrap_err().to_string(),
        "kaniko mode on / refuses named state for the rest of the build: a RUN may have left \
         processes behind, or the state directory changed under it (ADR-0056)"
    );
    s.close().unwrap();
}

/// `scratch_file` and `anonymous_file` make their files in the build
/// directory of a persistent store and in the state directory of an
/// anonymous one (`O_TMPFILE` here, so on disk), and name nothing there.
#[test]
fn scratch_and_anonymous_files_are_made_where_each_store_keeps_them() {
    use std::io::{Read as _, Seek as _, Write as _};
    use std::os::fd::AsRawFd as _;
    let names = |d: &Path| -> Vec<std::ffi::OsString> {
        fs::read_dir(d)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect()
    };
    let dir = tempfile::tempdir().unwrap();
    let persistent = store(dir.path());
    let anon_dir = dir.path().join("anon");
    fs::create_dir(&anon_dir).unwrap();
    let anonymous = Store::open_anonymous(&anon_dir).unwrap();
    for (s, shown) in [
        (&persistent, persistent.build_dir().to_owned()),
        (&anonymous, fs::canonicalize(&anon_dir).unwrap()),
    ] {
        let before = names(&shown);
        let mut f = s.scratch_file().unwrap();
        f.write_all(b"scratch").unwrap();
        f.rewind().unwrap();
        let mut back = String::new();
        f.read_to_string(&mut back).unwrap();
        assert_eq!(back, "scratch");
        let target = fs::read_link(format!("/proc/self/fd/{}", f.as_raw_fd())).unwrap();
        assert!(target.starts_with(&shown), "{}", target.display());
        let (mut kept, path) = s.anonymous_file().unwrap();
        kept.write_all(b"kept").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"kept");
        let target = fs::read_link(&path).unwrap();
        assert!(target.starts_with(&shown), "{}", target.display());
        assert_eq!(before, names(&shown), "{}", shown.display());
    }
    anonymous.close().unwrap();
}

/// A store directory whose group cannot be made the process's: a warning
/// outside a user namespace, where no map needs the gid; an error in one,
/// or where the namespace cannot be told. Any other failure is an error
/// everywhere.
#[test]
fn a_refused_group_change_is_a_warning_only_outside_a_user_namespace() {
    let refused = || rnk_rootfs::Error::Io {
        op: "chown",
        path: PathBuf::from("tmp"),
        source: io::Error::from_raw_os_error(Errno::PERM.raw_os_error()),
    };
    assert!(refused_group(refused(), "tmp", || Ok(false)).is_ok());
    let e = refused_group(refused(), "tmp", || Ok(true)).unwrap_err();
    assert!(matches!(e, Error::Rootfs(_)), "{e}");
    let unknown = || Err(Error::Random(io::Error::other("no procfs")));
    let e = refused_group(refused(), "tmp", unknown).unwrap_err();
    assert!(matches!(e, Error::Rootfs(_)), "{e}");
    let missing = rnk_rootfs::Error::Io {
        op: "open",
        path: PathBuf::from("tmp"),
        source: io::Error::from_raw_os_error(Errno::NOENT.raw_os_error()),
    };
    assert!(refused_group(missing, "tmp", || Ok(false)).is_err());
    let chmod = rnk_rootfs::Error::Io {
        op: "chmod",
        path: PathBuf::from("tmp"),
        source: io::Error::from_raw_os_error(Errno::PERM.raw_os_error()),
    };
    assert!(refused_group(chmod, "tmp", || Ok(false)).is_ok());
    // EPERM from a step before the change is not a refused change.
    let open = rnk_rootfs::Error::Io {
        op: "open",
        path: PathBuf::from("tmp"),
        source: io::Error::from_raw_os_error(Errno::PERM.raw_os_error()),
    };
    assert!(refused_group(open, "tmp", || Ok(false)).is_err());
}

/// A symlink of another group planted at a store directory's name keeps
/// its owner, group and ctime when the store opens, which then refuses it:
/// only a directory takes the process's group.
#[test]
fn a_symlink_at_a_store_directory_s_name_keeps_its_group() {
    use std::os::unix::fs::MetadataExt as _;
    let egid = rustix::process::getegid().as_raw();
    // Another group the user is in; root may take any.
    let Some(group) = rustix::process::getgroups()
        .unwrap()
        .into_iter()
        .map(rustix::process::Gid::as_raw)
        .find(|&g| g != egid)
        .or_else(|| rustix::process::geteuid().is_root().then_some(egid ^ 1))
    else {
        eprintln!("skipped: the user has one group");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    drop(store(dir.path()));
    let state = dir.path().join("state");
    fs::remove_dir_all(state.join("tmp")).unwrap();
    std::os::unix::fs::symlink("blobs", state.join("tmp")).unwrap();
    std::os::unix::fs::lchown(state.join("tmp"), None, Some(group)).unwrap();
    let seen = || {
        let m = fs::symlink_metadata(state.join("tmp")).unwrap();
        (m.uid(), m.gid(), m.ctime(), m.ctime_nsec())
    };
    let before = seen();
    let opened = Store::open(&BuildPaths::new(dir.path(), &state));
    assert_eq!(seen(), before, "{:?}", opened.as_ref().err());
    let e = opened.expect_err("a symlink as tmp/ is refused");
    assert!(
        matches!(&e, Error::Rootfs(r) if r.raw_os_error() == Some(Errno::NOTDIR.raw_os_error())),
        "{e}"
    );
}
