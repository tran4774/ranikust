use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

use rnk_sandbox::Step;

use super::*;

std::thread_local! {
    /// What [`probes_ran`] runs once, set by a test.
    static PROBES_RAN: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
}

/// Sets what [`probe_in_dir`] runs once its probes ended, on this thread.
pub(super) fn on_probes_ran(f: Box<dyn FnOnce()>) {
    PROBES_RAN.with(|h| *h.borrow_mut() = Some(f));
}

/// [`probe_in_dir`]'s hook, after the probes, before their directory is
/// removed or left.
pub(super) fn probes_ran() {
    if let Some(f) = PROBES_RAN.with(|h| h.borrow_mut().take()) {
        f();
    }
}

/// What another build does to a new probe directory before it is held.
enum Taken {
    /// Removes it as a leftover.
    Removed,
    /// Holds its lock, as a store about to remove it does.
    Locked,
}

std::thread_local! {
    /// What [`before_hold`] does, in turn, set by a test.
    static TAKEN_BEFORE_HOLD: std::cell::RefCell<std::collections::VecDeque<Taken>> =
        const { std::cell::RefCell::new(std::collections::VecDeque::new()) };
    /// The locks [`Taken::Locked`] took, until the test clears them.
    static LOCKED_BEFORE_HOLD: std::cell::RefCell<Vec<LockedElsewhere>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// [`make_probe_dir`]'s hook, between the directory `dir` made and held:
/// takes it as another build would, as a test asked on this thread.
pub(super) fn before_hold(dir: &Path) {
    match TAKEN_BEFORE_HOLD.with(|t| t.borrow_mut().pop_front()) {
        Some(Taken::Removed) => remove_own_probe_leftovers(dir.parent().unwrap()),
        Some(Taken::Locked) => {
            LOCKED_BEFORE_HOLD.with(|l| l.borrow_mut().push(LockedElsewhere::new(dir)));
        }
        None => {}
    }
}

/// A probe directory another build's start removes, or a store locks,
/// before it is held is made again under a name of its own, once.
#[test]
fn a_probe_directory_taken_before_it_is_held_is_made_again() {
    let tmp = private_tempdir();
    let root = RootPath::open(tmp.path()).unwrap();
    let name = format!("{}1-2", rnk_store::USERNS_PROBE_PREFIX);
    let again = format!("{name}-again");
    let names = || {
        let mut n: Vec<String> = fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        n.sort();
        n
    };
    let take = |t: Vec<Taken>| TAKEN_BEFORE_HOLD.with(|q| *q.borrow_mut() = t.into());
    for (taken, left) in [
        (Taken::Removed, vec![again.clone()]),
        (Taken::Locked, vec![name.clone(), again.clone()]),
    ] {
        take(vec![taken]);
        let (made, _, held) = make_probe_dir_again(&root, tmp.path(), &name).unwrap();
        assert_eq!(made, again);
        assert_eq!(names(), left);
        assert!(held.is_some());
        drop(held);
        LOCKED_BEFORE_HOLD.with(|l| l.borrow_mut().clear());
        for n in names() {
            fs::remove_dir(tmp.path().join(n)).unwrap();
        }
    }
    take(vec![Taken::Removed, Taken::Removed]);
    let Err(e) = make_probe_dir_again(&root, tmp.path(), &name) else {
        panic!("a probe directory removed twice before it was held");
    };
    assert!(e.contains("taken by another build"), "{e}");
    assert!(names().is_empty());
}

/// A probe directory removed between its `mkdirat` and its listing was taken
/// by another build's start: its name is made again, as when it is taken
/// before it is held. Another failure, or the `mkdirat`'s own, is not.
#[test]
fn a_probe_directory_gone_once_made_is_taken_not_an_error() {
    let io = |op, errno: Errno| rnk_rootfs::Error::Io {
        op,
        path: "p".into(),
        source: std::io::Error::from_raw_os_error(errno.raw_os_error()),
    };
    assert!(removed_once_made(&io("open", Errno::NOENT)));
    assert!(removed_once_made(&io("readdir", Errno::NOENT)));
    assert!(removed_once_made(&rnk_rootfs::Error::Changed {
        path: "p".into()
    }));
    assert!(!removed_once_made(&io("open", Errno::ACCESS)));
    assert!(!removed_once_made(&io("mkdir", Errno::NOENT)));
    assert!(!removed_once_made(&rnk_rootfs::Error::InvalidPath {
        path: "p".into(),
        reason: "the directory just made is not empty",
    }));
}

/// Linux's `EACCES`, without a dependency on `libc` for one constant.
const EACCES: i32 = 13;

/// Not root: the uid of a test user. Only the comparison with 0 matters.
const USER: u32 = 1000;

/// A caller's guidance for a user with no user namespace.
const NO_USER_NAMESPACE: &str = "the caller's advice";

const CAPS: OverlayCaps = OverlayCaps {
    api: MountApi::LowerdirPlus,
};

fn overlay_ok() -> Result<OverlayCaps, Refusal> {
    Ok(CAPS)
}

#[test]
fn both_probes_succeeding_pick_overlay_as_root() {
    let picked = pick(Ok("caps"), overlay_ok(), 0).unwrap();
    assert_eq!(picked, ("caps", CAPS));
}

#[test]
fn a_refused_overlay_mount_routes_root_to_kaniko_stages_but_a_host_error_does_not() {
    let mount = SnapshotError::Probe {
        step: "mount",
        source: Box::new(SnapshotError::Mount {
            target: "/s/merged".into(),
            lowers: 1,
            how: "mount(2)",
            step: "mount".into(),
            errno: rustix::io::Errno::INVAL,
            options: Some(120),
        }),
    };
    let r = overlay_refusal(&mount, 0);
    assert_eq!(r.guidance, OVERLAY_UNUSABLE);
    assert_eq!(r.kind, Kind::OverlayUnusable);
    assert!(!r.needs_kaniko_mode());
    assert!(
        r.reason.contains("EINVAL") || r.reason.contains("Invalid"),
        "{r}"
    );
    assert_eq!(overlay_refusal(&mount, USER).guidance, NEEDS_ROOT);

    let host = SnapshotError::Io {
        op: "create",
        path: "/s".into(),
        source: io::Error::from_raw_os_error(EACCES),
    };
    assert_eq!(overlay_refusal(&host, 0).guidance, "");
    assert_eq!(overlay_refusal(&host, 0).kind, Kind::Other);

    // The test hook's refusal routes as the kernel's.
    let r = overlay_refusal(&FAULT_OVERLAY_REFUSED, 0);
    assert_eq!(r.kind, Kind::OverlayUnusable);
    assert_eq!(route(&r, false), Route::KanikoStages);
}

/// The user-namespace probe's directory stays when a child was left by
/// the steps or by the overlay probe in the namespace, and only then.
#[test]
fn only_a_probe_child_left_behind_keeps_the_probes_directory() {
    let left = rnk_sandbox::Error::ProbeChildLeft { pid: 42 };
    assert!(keeps_dir(Some(&left), None));
    assert!(keeps_dir(None, Some(&left)));
    assert!(!keeps_dir(None, None));
    let others = [
        rnk_sandbox::Error::ProbeClosure("the probe's child was killed after 60 s".to_owned()),
        rnk_sandbox::Error::NotSingleThreaded { threads: 2 },
    ];
    for e in &others {
        assert!(!keeps_dir(Some(e), None), "{e}");
        assert!(!keeps_dir(None, Some(e)), "{e}");
    }
}

/// A probe mount that never reached the kernel (its thread could not start,
/// `EAGAIN`) or ran out of memory or descriptors says nothing about
/// overlayfs: no routing to kaniko mode (host-safety round 1, item 5).
#[test]
fn a_mount_thread_that_could_not_start_is_not_an_unusable_overlay() {
    let probe = |step: &str, errno| SnapshotError::Probe {
        step: "mount",
        source: Box::new(SnapshotError::Mount {
            target: "/s/merged".into(),
            lowers: 1,
            how: "mount(2)",
            step: step.into(),
            errno,
            options: Some(120),
        }),
    };
    use rustix::io::Errno;
    for (step, errno) in [
        (
            "start the mount thread: Resource temporarily unavailable",
            Errno::AGAIN,
        ),
        ("mount", Errno::AGAIN),
        ("mount", Errno::NOMEM),
        ("mount", Errno::MFILE),
        ("mount", Errno::NOSPC),
        ("mount", Errno::DQUOT),
        ("mount", Errno::IO),
    ] {
        let r = overlay_refusal(&probe(step, errno), 0);
        assert_eq!(r.kind, Kind::Other, "{step} {errno:?}");
        assert_eq!(route(&r, false), Route::Refuse, "{step} {errno:?}");
    }
    for errno in [Errno::INVAL, Errno::PERM, Errno::ACCESS, Errno::NODEV] {
        let r = overlay_refusal(&probe("mount", errno), 0);
        assert_eq!(route(&r, false), Route::KanikoStages, "{errno:?}");
    }
}

/// `--mode=auto`'s routing after overlay mode's set-up refused, as a pure
/// function (host-safety round 1, item 5).
#[test]
fn route_sends_only_an_unusable_overlay_outside_a_container_to_kaniko_stages() {
    let of = |kind| Refusal {
        kind,
        reason: "r".to_owned(),
        guidance: "".into(),
    };
    // The probe's mount refused, or a state directory on overlayfs.
    assert_eq!(
        route(&of(Kind::OverlayUnusable), false),
        Route::KanikoStages
    );
    // In a container where root may mount, kaniko mode on `/` is refused
    // (ADR-0054): the refusal stands.
    assert_eq!(
        route(&of(Kind::OverlayUnusable), true),
        Route::RefuseInContainer
    );
    for kind in [
        Kind::SandboxBlocked,
        Kind::NoMountPrivilege,
        Kind::Other,
        Kind::Broken,
    ] {
        for in_container in [false, true] {
            assert_eq!(route(&of(kind), in_container), Route::Refuse, "{kind:?}");
        }
    }
    // A state directory on overlayfs (`set_up`'s first refusal) is of the
    // routed kind; the VM's kaniko quick suite builds there
    // (`auto_routes_to_stages`).
}

#[test]
fn a_failed_sandbox_step_never_routes_to_kaniko_stages() {
    // Kaniko mode off `/` needs the same sandbox: `--mode=auto` must not
    // turn a blocked sandbox step into a kaniko-mode build.
    let r = pick::<()>(
        Err(SandboxError::Setup {
            step: Step::MountProc,
            source: io::Error::from_raw_os_error(EACCES),
        }),
        overlay_ok(),
        0,
    )
    .unwrap_err();
    assert_eq!(r.guidance, SANDBOX_BLOCKED);
    assert_eq!(r.kind, Kind::SandboxBlocked);
    assert_eq!(route(&r, false), Route::Refuse);
    assert!(!r.needs_kaniko_mode());
}

#[test]
fn no_guidance_sends_a_user_back_to_the_mode_they_used() {
    // The refusals `--mode=auto` can show never tell its user to use
    // `--mode=auto` (Phase 2 criterion 8, review finding #2).
    for text in [OVERLAY_UNUSABLE, ON_OVERLAYFS, SANDBOX_BLOCKED] {
        assert!(!text.contains("use --mode=auto"), "{text}");
    }
    assert!(NEEDS_KANIKO_MODE.contains("drop --mode=overlay"));
}

#[test]
fn a_sandbox_without_privileges_needs_root_unless_already_root() {
    let refusal = pick::<()>(
        Err(SandboxError::Privilege("the RUN sandbox")),
        overlay_ok(),
        USER,
    )
    .unwrap_err();
    assert_eq!(refusal.guidance, NEEDS_ROOT);
    assert!(refusal.reason.contains("RUN sandbox"), "{refusal}");
    // Root that entered the build's mount namespace but whose sandbox
    // lacks a privilege: kaniko mode off `/` would need it too, so the
    // guidance is its own and never sends an auto user back to auto
    // (host-safety re-check of round 1, item 4).
    let refusal = pick::<()>(
        Err(SandboxError::Privilege("the RUN sandbox")),
        overlay_ok(),
        0,
    )
    .unwrap_err();
    assert_eq!(refusal.guidance, SANDBOX_NO_PRIVILEGE);
    assert_eq!(refusal.kind, Kind::SandboxBlocked);
    assert_eq!(route(&refusal, false), Route::Refuse);
    assert!(!refusal.to_string().contains("--mode=auto"), "{refusal}");
}

/// A blocked sandbox step wins over an unusable overlay: `pick` reports
/// the sandbox, and `--mode=auto` does not route to kaniko mode off `/`,
/// which needs that sandbox (host-safety re-check of round 1, item 2).
#[test]
fn a_blocked_sandbox_wins_over_an_unusable_overlay() {
    let overlay = Refusal {
        kind: Kind::OverlayUnusable,
        reason: "overlay: EINVAL".to_owned(),
        guidance: OVERLAY_UNUSABLE.into(),
    };
    let r = pick::<()>(
        Err(SandboxError::Setup {
            step: Step::MountProc,
            source: io::Error::from_raw_os_error(EACCES),
        }),
        Err(overlay),
        0,
    )
    .unwrap_err();
    assert_eq!(r.kind, Kind::SandboxBlocked);
    assert_eq!(route(&r, false), Route::Refuse);
}

#[test]
fn a_failed_sandbox_step_is_reported_verbatim_with_kaniko_mode_as_the_way() {
    // Docker's `--cap-add SYS_ADMIN` with AppArmor's `docker-default`: the
    // mounts fail with `EACCES`.
    let r = pick::<()>(
        Err(SandboxError::Setup {
            step: Step::MountProc,
            source: io::Error::from_raw_os_error(EACCES),
        }),
        overlay_ok(),
        0,
    );
    let text = r.unwrap_err().to_string();
    assert!(text.contains("proc"), "{text}");
    assert!(text.contains("denied"), "{text}");
    assert!(text.contains("kaniko mode"), "{text}");
}

#[test]
fn a_host_side_failure_does_not_suggest_kaniko_mode() {
    // `<state>/sandbox/runs` on a read-only or full filesystem.
    let refusal = pick::<()>(
        Err(SandboxError::Io {
            op: "create",
            path: "/w/.ranikust/sandbox/runs".into(),
            source: io::Error::from_raw_os_error(EACCES),
        }),
        overlay_ok(),
        0,
    )
    .unwrap_err();
    assert_eq!(refusal.guidance, "");
    assert!(refusal.reason.contains("sandbox/runs"), "{refusal}");
}

#[test]
fn a_failed_overlay_probe_refuses_even_with_a_sandbox() {
    let failed = Refusal {
        kind: Kind::OverlayUnusable,
        reason: "overlay: EINVAL".to_owned(),
        guidance: OVERLAY_UNUSABLE.into(),
    };
    let r = pick(Ok(()), Err(failed), 0);
    assert_eq!(r.unwrap_err().reason, "overlay: EINVAL");
}

#[test]
fn the_plain_chroot_refusal_says_why() {
    let refusal = namespace_refusal(SandboxError::InPlainChroot, 0);
    let text = refusal.to_string();
    assert!(text.contains("plain chroot"), "{text}");
    assert!(!text.contains("Invalid argument"), "{text}");
}

#[test]
fn no_privileges_for_the_namespace_needs_root() {
    let refusal = namespace_refusal(SandboxError::Privilege("a private mount namespace"), USER);
    assert_eq!(refusal.guidance, NEEDS_ROOT);
    assert!(refusal.to_string().contains("sudo"));
    let refusal = namespace_refusal(SandboxError::Privilege("a private mount namespace"), 0);
    assert_eq!(refusal.guidance, NEEDS_KANIKO_MODE);
}

/// A process left inside a broken user namespace ends: no mode follows.
#[test]
fn a_broken_user_namespace_never_falls_back() {
    let broken = || {
        SandboxError::UserNamespaceBroken(rnk_sandbox::StepFailure {
            op: "write uid_map",
            errno: Some(EACCES),
            detail: None,
        })
    };
    for euid in [0, USER] {
        let refusal = userns_refusal(broken(), euid, NO_USER_NAMESPACE.into());
        assert_eq!(refusal.kind, Kind::Broken);
        assert!(!refusal.needs_kaniko_mode());
        for in_container in [false, true] {
            assert_eq!(route(&refusal, in_container), Route::Refuse);
        }
    }
    // An error not known to leave the process unchanged counts as broken.
    let refusal = userns_refusal(
        SandboxError::Privilege("a user namespace"),
        USER,
        NO_USER_NAMESPACE.into(),
    );
    assert_eq!(refusal.kind, Kind::Broken);
    // Not entered: the process is unchanged, the refusal of the usual kind.
    let refusal = userns_refusal(
        SandboxError::NoUserNamespace { tried: Vec::new() },
        USER,
        NO_USER_NAMESPACE.into(),
    );
    assert_eq!(refusal.kind, Kind::Other);
    assert_eq!(refusal.guidance, NO_USER_NAMESPACE);
    // The caller's guidance (`prune`'s), and root's: none.
    for (euid, want) in [(USER, "prune's"), (0, "")] {
        let no_userns = SandboxError::NoUserNamespace { tried: Vec::new() };
        assert_eq!(
            userns_refusal(no_userns, euid, "prune's".into()).guidance,
            want
        );
    }
    for unchanged in [
        SandboxError::NotSingleThreaded { threads: 2 },
        SandboxError::InPlainChroot,
        SandboxError::Sys {
            op: "unshare(CLONE_NEWUSER)",
            source: io::Error::from_raw_os_error(EACCES),
        },
    ] {
        assert_eq!(
            userns_refusal(unchanged, USER, NO_USER_NAMESPACE.into()).kind,
            Kind::Other
        );
    }
    // The store refusing once inside: broken whatever its kind, its words
    // kept.
    for kind in [
        Kind::Other,
        Kind::NoMountPrivilege,
        Kind::OverlayUnusable,
        Kind::SandboxBlocked,
        Kind::Broken,
    ] {
        let refusal = inside_userns(Refusal {
            kind,
            reason: "r".to_owned(),
            guidance: "g".into(),
        });
        assert_eq!(refusal.kind, Kind::Broken, "{kind:?}");
        assert_eq!(
            (refusal.reason.as_str(), refusal.guidance.as_ref()),
            ("r", "g")
        );
        assert_eq!(route(&refusal, false), Route::Refuse);
    }
}

/// A fresh temp dir no one else may write, whatever the umask.
fn private_tempdir() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o700)).unwrap();
    tmp
}

/// The probe's directory in a setgid state directory takes the process's
/// gid and loses the bit, so that its entries carry a gid the map holds.
#[test]
fn the_probe_directory_drops_a_setgid_parents_bit() {
    let tmp = private_tempdir();
    fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o2700)).unwrap();
    let root = RootPath::open(tmp.path()).unwrap();
    let probe = root.create_sub_root("probe", 0o700).unwrap();
    let before = fs::metadata(tmp.path().join("probe")).unwrap();
    // A filesystem that ignores the bit leaves nothing to check.
    if before.mode() & 0o2000 == 0 {
        eprintln!("skipped: no setgid inheritance here");
        return;
    }
    assert!(own_group(&probe).unwrap());
    let after = fs::metadata(tmp.path().join("probe")).unwrap();
    assert_eq!(after.mode() & 0o7777, 0o700);
    assert_eq!(after.gid(), rustix::process::getegid().as_raw());
    // Once owned, nothing more changes.
    assert!(!own_group(&probe).unwrap());
}

/// The probe directory's group is set through its verified descriptor:
/// whatever has its name by then is left as it is.
#[test]
fn the_probe_directory_s_group_is_set_through_its_descriptor() {
    let tmp = private_tempdir();
    let root = RootPath::open(tmp.path()).unwrap();
    let probe = root.create_sub_root("probe", 0o700).unwrap();
    fs::rename(tmp.path().join("probe"), tmp.path().join("made")).unwrap();
    let other = tmp.path().join("probe");
    fs::write(&other, "").unwrap();
    // A setgid bit the change would clear, on an entry of this user's group.
    fs::set_permissions(&other, fs::Permissions::from_mode(0o2600)).unwrap();
    let before = fs::metadata(&other).unwrap();
    // A filesystem that drops the bit leaves nothing to check.
    if before.mode() & 0o2000 == 0 {
        eprintln!("skipped: no setgid bit kept here");
        return;
    }
    fs::set_permissions(tmp.path().join("made"), fs::Permissions::from_mode(0o2700)).unwrap();
    assert!(own_group(&probe).unwrap());
    let after = fs::metadata(&other).unwrap();
    assert_eq!(
        (after.mode(), after.gid(), after.ino()),
        (before.mode(), before.gid(), before.ino())
    );
    let made = fs::metadata(tmp.path().join("made")).unwrap();
    assert_eq!(made.mode() & 0o7777, 0o700);
}

/// A probe directory a build killed before it gave it away is removed,
/// only where it is this user's own, empty, mode 0700 (setgid too, as a
/// setgid state directory makes it), a real directory no probe holds;
/// anything else of a probe's name, and any other name, is left as found.
#[test]
fn only_an_empty_private_probe_directory_of_one_s_own_is_removed() {
    let tmp = private_tempdir();
    let dir = |name: &str, mode| {
        let p = tmp.path().join(name);
        fs::create_dir(&p).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(mode)).unwrap();
        p
    };
    let probe = |s: &str| format!("{}{s}", rnk_store::USERNS_PROBE_PREFIX);
    dir(&probe("empty"), 0o700);
    dir(&probe("setgid"), 0o2700);
    dir(&probe("open"), 0o755);
    dir(&probe("setuid"), 0o4700);
    let full = dir(&probe("full"), 0o700);
    fs::write(full.join("f"), "x").unwrap();
    let held = dir(&probe("held"), 0o700);
    let lock = LockedElsewhere::new(&held);
    let target = dir("target", 0o700);
    std::os::unix::fs::symlink(&target, tmp.path().join(probe("link"))).unwrap();
    fs::write(tmp.path().join(probe("file")), "").unwrap();
    dir("other", 0o700);
    let names = || {
        let mut n: Vec<String> = fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        n.sort();
        n
    };
    let mut kept = names();
    kept.retain(|n| *n != probe("empty") && *n != probe("setgid"));
    remove_own_probe_leftovers(tmp.path());
    assert_eq!(names(), kept);
    assert!(full.join("f").exists());
    drop(lock);
    remove_own_probe_leftovers(tmp.path());
    kept.retain(|n| *n != probe("held"));
    assert_eq!(names(), kept);
}

/// An exclusive `flock` of the directory `path`, held by a child process
/// of its own until dropped: one this process took would outlive its drop
/// in the children other tests fork meanwhile.
pub(crate) struct LockedElsewhere(libc::pid_t);

impl LockedElsewhere {
    pub(crate) fn new(path: &Path) -> Self {
        let path_c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        let (r, w) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC).unwrap();
        let keep = u32::try_from(std::os::fd::AsRawFd::as_raw_fd(&w)).unwrap();
        // The standard streams and `w` stay open in the child.
        assert!(keep >= 3, "the pipe took a standard stream's number");
        let (below, above) = (keep - 1, keep + 1);
        // SAFETY: the child makes only async-signal-safe system calls, on
        // memory made before the fork, and never returns.
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            // SAFETY: plain system calls on a path, bounds and descriptors
            // made before the fork; the child waits to be killed. It
            // closes first every descriptor above the standard streams
            // but `w`, so that it holds no other test's lock, and says it
            // failed where it cannot.
            unsafe {
                let closed = (keep == 3
                    || libc::syscall(libc::SYS_close_range, 3_u32, below, 0_u32) == 0)
                    && libc::syscall(libc::SYS_close_range, above, u32::MAX, 0_u32) == 0;
                let fd = libc::open(
                    path_c.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
                );
                let locked = u8::from(
                    closed && fd >= 0 && libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) == 0,
                );
                libc::write(
                    std::os::fd::AsRawFd::as_raw_fd(&w),
                    (&raw const locked).cast(),
                    1,
                );
                loop {
                    libc::pause();
                }
            }
        }
        assert!(pid > 0, "fork: {}", io::Error::last_os_error());
        let held = Self(pid);
        drop(w);
        let mut locked = [0];
        assert_eq!(rustix::io::read(&r, &mut locked).unwrap(), 1);
        assert_eq!(
            locked,
            [1],
            "the child cannot close its other descriptors or lock {}",
            path.display()
        );
        held
    }
}

impl Drop for LockedElsewhere {
    fn drop(&mut self) {
        // SAFETY: plain system calls on this test's own child.
        unsafe {
            libc::kill(self.0, libc::SIGKILL);
            libc::waitpid(self.0, std::ptr::null_mut(), 0);
        }
    }
}

/// A probe directory given to a range is left to the store, so it must
/// bear a name the store removes.
#[test]
fn a_probe_directory_for_a_range_bears_a_name_the_store_removes() {
    let tmp = private_tempdir();
    let range = rnk_sandbox::IdRange {
        start: 300_000,
        count: 65_536,
    };
    let ids = rnk_sandbox::BuildIds::Shifted(rnk_sandbox::SubordinateIds {
        uids: range,
        gids: range,
    });
    let Err(e) = probe_in_dir(tmp.path(), "probe", ids) else {
        panic!("a range's probe in a directory the store never removes");
    };
    assert!(e.contains("not a name the store removes"), "{e}");
    assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 0);
}

/// A directory under a fresh temp dir, mode 0755, for a state directory.
fn base() -> (tempfile::TempDir, PathBuf) {
    let tmp = private_tempdir();
    let base = tmp.path().join("base");
    fs::create_dir(&base).unwrap();
    fs::set_permissions(&base, fs::Permissions::from_mode(0o755)).unwrap();
    (tmp, base)
}

#[test]
fn a_missing_state_dir_is_created_private() {
    let (_tmp, base) = base();
    let state = base.join("a/state");
    prepare_state_dir(&state).unwrap();
    assert_eq!(fs::metadata(&state).unwrap().mode() & 0o777, 0o700);
    // Again: an existing, safe one passes.
    prepare_state_dir(&state).unwrap();
}

#[test]
fn a_symlinked_state_dir_is_refused_and_nothing_is_created_through_it() {
    let (tmp, base) = base();
    let elsewhere = tmp.path().join("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    let state = base.join("state");
    std::os::unix::fs::symlink(&elsewhere, &state).unwrap();
    let refusal = prepare_state_dir(&state).unwrap_err();
    assert!(refusal.reason.contains("symlink"), "{refusal}");
    assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
}

#[test]
fn a_parent_others_may_write_is_refused_before_creating_the_state_dir() {
    let (_tmp, base) = base();
    for mode in [0o775, 0o757, 0o777] {
        fs::set_permissions(&base, fs::Permissions::from_mode(mode)).unwrap();
        let refusal = prepare_state_dir(&base.join("state")).unwrap_err();
        assert!(refusal.reason.contains("writable"), "{mode:o}: {refusal}");
    }
    assert!(!base.join("state").exists());
}

#[test]
fn a_state_dir_others_may_write_is_refused() {
    let (_tmp, base) = base();
    let state = base.join("state");
    fs::create_dir(&state).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o777)).unwrap();
    let refusal = prepare_state_dir(&state).unwrap_err();
    assert!(refusal.reason.contains("writable"), "{refusal}");
}

/// Whether the test runs as root, where a [`set_up`] that took its state
/// directory would mount outside any namespace of the build: skipped.
fn as_root() -> bool {
    let root = rustix::process::geteuid().is_root();
    if root {
        eprintln!("skipped: as root, a set-up that took the state directory would mount");
    }
    root
}

/// [`set_up`] for `state`, which a test expects refused before any probe.
fn set_up_refusal(state: &Path) -> Refusal {
    match set_up(state, false, &Seccomp::Unset, &mut false, false, false) {
        Ok(_) => panic!("overlay mode took {}", state.display()),
        Err(refusal) => refusal,
    }
}

/// Root's overlay mode refuses its state directory before it changes
/// anything there, as rootless overlay mode does: one another process
/// holds alone, one a kaniko-mode build on `/` used, one a `prune --all`
/// cut short, and one whose store directory is a symlink keep their modes
/// and names.
#[test]
fn overlay_mode_refuses_its_state_directory_unchanged() {
    if as_root() {
        return;
    }
    let tmp = private_tempdir();
    // Refused on overlayfs before the store's checks this test is about.
    if crate::statedir::on_overlayfs(tmp.path()) {
        eprintln!("skipped: the temporary directory is on overlayfs");
        return;
    }
    let state = tmp.path().join("state");
    Store::open(&BuildPaths::new(&state, &state))
        .unwrap()
        .close()
        .unwrap();
    for p in [&state, &state.join("builds")] {
        fs::set_permissions(p, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let seen = || {
        let mut names: Vec<_> = fs::read_dir(&state)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        names.sort();
        let mode = |p: &Path| fs::metadata(p).unwrap().mode() & 0o7777;
        (names, mode(&state), mode(&state.join("builds")))
    };
    let check = |what: &str, says: &str| {
        let before = seen();
        let refusal = set_up_refusal(&state);
        assert!(refusal.reason.contains(says), "{what}: {refusal}");
        assert_eq!(seen(), before, "{what}: {refusal}");
        refusal
    };

    let alone = LockedElsewhere::new(&state);
    check("held alone", "another build is using");
    drop(alone);

    let building = state.join(rnk_store::BUILDING_MARKER);
    fs::write(&building, "").unwrap();
    check("used on /", "kaniko-mode build on /");
    fs::remove_file(&building).unwrap();

    let (marker, pruning) = (
        state.join(rnk_store::STATE_MARKER),
        state.join(rnk_store::PRUNING_MARKER),
    );
    fs::rename(&marker, &pruning).unwrap();
    let refusal = check("a prune cut short", "did not finish emptying");
    assert!(refusal.guidance.contains("prune --all"), "{refusal}");
    fs::rename(&pruning, &marker).unwrap();

    let blobs = state.join("blobs");
    fs::remove_dir(&blobs).unwrap();
    std::os::unix::fs::symlink("layers", &blobs).unwrap();
    check("a symlink as blobs", "holds blobs, which is a symlink");
    fs::remove_file(&blobs).unwrap();

    // The control: taken, it is made private.
    let euid = rustix::process::geteuid().as_raw();
    let me = crate::statedir::Owner::me();
    drop(hold_state_dir(&state, euid, me, || unreachable!("on overlayfs")).unwrap());
    assert_eq!(seen().1, 0o700);
}

/// A missing state directory whose nearest parent is on overlayfs (a
/// container's own root) is refused by root's overlay mode before
/// anything is made, routed as overlayfs unusable; checked where the
/// temporary directory is on overlayfs.
#[test]
fn overlay_mode_refuses_a_missing_state_directory_on_overlayfs_unmade() {
    if as_root() {
        return;
    }
    let tmp = private_tempdir();
    if !crate::statedir::on_overlayfs(tmp.path()) {
        eprintln!("skipped: the temporary directory is not on overlayfs");
        return;
    }
    let refusal = set_up_refusal(&tmp.path().join("a/state"));
    assert_eq!(refusal.kind, Kind::OverlayUnusable, "{refusal}");
    assert!(refusal.reason.contains("is on overlayfs"), "{refusal}");
    assert!(!tmp.path().join("a").exists());
}

/// Every entry below `dir`, `dir` included, never following a symlink or
/// opening a special file: its path, mode, size, mtime, ctime, and a
/// file's bytes or a symlink's target.
fn snapshot(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(p) = todo.pop() {
        let m = fs::symlink_metadata(&p).unwrap();
        let content = if m.is_file() {
            String::from_utf8_lossy(&fs::read(&p).unwrap()).into_owned()
        } else if m.file_type().is_symlink() {
            fs::read_link(&p).unwrap().display().to_string()
        } else {
            String::new()
        };
        if m.is_dir() {
            todo.extend(fs::read_dir(&p).unwrap().map(|e| e.unwrap().path()));
        }
        out.push(format!(
            "{} {:o} {} {}.{} {}.{} {content}",
            p.display(),
            m.mode(),
            m.len(),
            m.mtime(),
            m.mtime_nsec(),
            m.ctime(),
            m.ctime_nsec()
        ));
    }
    out.sort();
    out
}

/// Red team: root's overlay mode, and the check rootless overlay mode and
/// `--userns` share, refuse a state directory whose store directory is a
/// special file or a link, one that is not a store, one whose marker is
/// not a file, and one at or below a special file, without blocking and
/// with nothing changed, what a link leads to included (T4).
#[test]
fn red_team_a_planted_state_directory_is_refused_unchanged_by_each_set_up() {
    use rustix::fs::{CWD, FileType, Mode, mknodat};
    use std::os::unix::fs::symlink;
    use std::sync::mpsc;
    use std::time::Duration;
    if as_root() {
        return;
    }
    let tmp = private_tempdir();
    if crate::statedir::on_overlayfs(tmp.path()) {
        eprintln!("skipped: the temporary directory is on overlayfs");
        return;
    }
    let t = tmp.path();
    let node = |p: &Path, kind: FileType| mknodat(CWD, p, kind, Mode::from_raw_mode(0o600), 0);
    let outside = t.join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("f"), "keep").unwrap();
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o755)).unwrap();
    let fifo_out = t.join("fifo-out");
    node(&fifo_out, FileType::Fifo).unwrap();
    fs::write(t.join("file"), "keep").unwrap();
    symlink("missing", t.join("dead")).unwrap();
    // 0:0, a whiteout: the only device node a user may make.
    let chardev = node(&t.join("chardev"), FileType::CharacterDevice).is_ok();
    drop(std::os::unix::net::UnixListener::bind(t.join("sock")).unwrap());
    // A FIFO opened by mistake blocks: each set-up runs on a thread of its own.
    let refused = |state: &Path, case: &str| -> Vec<String> {
        let mut reasons = Vec::new();
        for shared in [false, true] {
            let (tx, rx) = mpsc::channel();
            let s = state.to_owned();
            std::thread::spawn(move || {
                let r = if shared {
                    let euid = rustix::process::geteuid().as_raw();
                    let on_overlayfs = || Refusal {
                        kind: Kind::OverlayUnusable,
                        reason: "on overlayfs".into(),
                        guidance: "".into(),
                    };
                    let me = crate::statedir::Owner::me();
                    hold_state_dir(&s, euid, me, on_overlayfs).map(drop)
                } else {
                    set_up(&s, false, &Seccomp::Unset, &mut false, false, false).map(drop)
                };
                tx.send(r.map_err(|r| r.reason)).unwrap();
            });
            let r = rx
                .recv_timeout(Duration::from_secs(30))
                .unwrap_or_else(|_| panic!("{case} (shared {shared}): blocked"));
            reasons.push(
                r.err()
                    .unwrap_or_else(|| panic!("{case} (shared {shared}): taken")),
            );
        }
        reasons
    };
    let mut cases = 0;
    let mut check = |state: &Path, case: &str, says: &str| {
        let before = snapshot(t);
        for reason in refused(state, case) {
            assert!(reason.contains(says), "{case}: {reason}");
        }
        assert_eq!(snapshot(t), before, "{case}");
        cases += 1;
    };
    // A store as builds leave it, open to others as no set-up leaves it.
    let store = |name: &str| {
        let state = t.join(name);
        Store::open(&BuildPaths::new(&state, &state))
            .unwrap()
            .close()
            .unwrap();
        for p in [&state, &state.join("builds")] {
            fs::set_permissions(p, fs::Permissions::from_mode(0o755)).unwrap();
        }
        state
    };

    for d in ["blobs", "builds", "tmp"] {
        for plant in [
            "a FIFO",
            "a socket",
            "a character device",
            "a link out",
            "a dead link",
            "a link to a FIFO",
        ] {
            if plant == "a character device" && !chardev {
                continue;
            }
            let state = store(&format!("{d}-{}", plant.replace(' ', "-")));
            let at = state.join(d);
            fs::remove_dir_all(&at).unwrap();
            match plant {
                "a FIFO" => node(&at, FileType::Fifo).unwrap(),
                "a socket" => drop(std::os::unix::net::UnixListener::bind(&at).unwrap()),
                "a character device" => node(&at, FileType::CharacterDevice).unwrap(),
                "a link out" => symlink(&outside, &at).unwrap(),
                "a dead link" => symlink("missing", &at).unwrap(),
                _ => symlink(&fifo_out, &at).unwrap(),
            }
            let says = format!("holds {d}, which is");
            check(&state, &format!("{d} as {plant}"), &says);
        }
    }

    // Not a store: a directory as a system one is, and markers that are not files.
    let sys = t.join("sys");
    for d in ["bin", "etc", "usr/lib"] {
        fs::create_dir_all(sys.join(d)).unwrap();
    }
    fs::write(sys.join("etc/passwd"), "root:x:0:0::/root:/bin/sh\n").unwrap();
    fs::set_permissions(&sys, fs::Permissions::from_mode(0o755)).unwrap();
    let unmarked = "has no regular file .ranikust-state";
    check(&sys, "a system-like directory", unmarked);
    for plant in [
        "a directory",
        "a FIFO",
        "a link to a file out",
        "a dead link",
    ] {
        let state = t.join(format!("marker-{}", plant.replace(' ', "-")));
        fs::create_dir(&state).unwrap();
        fs::create_dir(state.join("blobs")).unwrap();
        let marker = state.join(rnk_store::STATE_MARKER);
        match plant {
            "a directory" => fs::create_dir(&marker).unwrap(),
            "a FIFO" => node(&marker, FileType::Fifo).unwrap(),
            "a link to a file out" => symlink(outside.join("f"), &marker).unwrap(),
            _ => symlink("missing", &marker).unwrap(),
        }
        fs::set_permissions(&state, fs::Permissions::from_mode(0o755)).unwrap();
        check(&state, &format!("the marker as {plant}"), unmarked);
    }

    // At or below what is not a directory.
    let mut at_or_below = vec!["fifo-out", "file", "dead", "sock"];
    if chardev {
        at_or_below.push("chardev");
    }
    for name in at_or_below {
        let unopened = "as a directory without following a symlink";
        check(&t.join(name), &format!("at {name}"), unopened);
        check(
            &t.join(name).join("state"),
            &format!("below {name}"),
            unopened,
        );
    }
    assert!(cases >= 3 * 5 + 5 + 8, "{cases} cases");

    // The control: a store as builds leave it is taken, and made private.
    let state = store("taken");
    let euid = rustix::process::geteuid().as_raw();
    let me = crate::statedir::Owner::me();
    drop(hold_state_dir(&state, euid, me, || unreachable!("on overlayfs")).unwrap());
    assert_eq!(fs::metadata(&state).unwrap().mode() & 0o7777, 0o700);
}

/// A directory below `/var/lib`, root's and `0755` so that a `--userns`
/// range may search it, removed when dropped.
struct VarLib(PathBuf);

impl VarLib {
    fn new(what: &str) -> Self {
        let p = PathBuf::from(format!(
            "/var/lib/rnk-red-team-{what}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir(&p).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        Self(p)
    }
}

impl Drop for VarLib {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// T2, as real root: root's overlay mode refuses `/` and each system
/// directory there is, its owner and mode kept and no name of the store
/// made in it; root's overlay mode, and the check `--userns` runs for its
/// range, refuse a state directory whose store directory is a real device
/// node, a FIFO, a socket or a link out, unopened and with nothing changed
/// (T4).
#[test]
#[ignore = "T2: real root's overlay set-up on system directories and device nodes; one taken would change the machine"]
fn red_team_root_refuses_system_directories_and_device_nodes_unchanged() {
    use rustix::fs::{CWD, FileType, Mode, makedev, mknodat};
    use std::os::unix::fs::{lchown, symlink};
    assert!(rustix::process::geteuid().is_root(), "run as root");
    // A FIFO or a device opened by mistake may block: the process ends instead.
    // SAFETY: `alarm` only arms this process's timer.
    unsafe { libc::alarm(300) };
    let store_names = [
        rnk_store::STATE_MARKER,
        "blobs",
        "layers",
        "caches",
        "locks",
        "tmp",
        "builds",
    ];
    let present = |dir: &Path| -> Vec<bool> {
        store_names
            .iter()
            .map(|n| fs::symlink_metadata(dir.join(n)).is_ok())
            .collect()
    };
    let mut system: Vec<&str> = crate::statedir::SYSTEM_DIRS.to_vec();
    system.push("/");
    let mut tried = 0;
    for sys in system {
        let dir = Path::new(sys);
        let Ok(before) = fs::symlink_metadata(dir) else {
            continue;
        };
        let had = present(dir);
        let refused = set_up(dir, false, &Seccomp::Unset, &mut false, false, false).map(drop);
        assert!(refused.is_err(), "{sys} taken");
        let after = fs::symlink_metadata(dir).unwrap();
        let ids = |m: &fs::Metadata| (m.mode(), m.uid(), m.gid());
        assert_eq!(ids(&after), ids(&before), "{sys}");
        assert_eq!(present(dir), had, "{sys}");
        tried += 1;
    }
    assert!(tried >= 10, "{tried} system directories");

    let base = VarLib::new("set-up");
    let b = base.0.as_path();
    let node = |p: &Path, kind: FileType, dev| {
        mknodat(CWD, p, kind, Mode::from_raw_mode(0o600), dev).unwrap();
    };
    let plant = |at: &Path, what: &str| match what {
        "a FIFO" => node(at, FileType::Fifo, 0),
        "a socket" => drop(std::os::unix::net::UnixListener::bind(at).unwrap()),
        "a character device" => node(at, FileType::CharacterDevice, makedev(1, 3)),
        "a block device" => node(at, FileType::BlockDevice, makedev(7, 0)),
        "a link to /etc" => symlink("/etc", at).unwrap(),
        _ => symlink("/dev/zero", at).unwrap(),
    };
    let plants = [
        "a FIFO",
        "a socket",
        "a character device",
        "a block device",
        "a link to /etc",
        "a link to a device",
    ];
    let etc = fs::symlink_metadata("/etc").unwrap().mode();
    for d in ["blobs", "builds", "tmp"] {
        for what in plants {
            let case = format!("{d} as {what}");
            let state = b.join(case.replace(' ', "-").replace('/', ""));
            Store::open(&BuildPaths::new(&state, &state))
                .unwrap()
                .close()
                .unwrap();
            for p in [&state, &state.join("builds")] {
                fs::set_permissions(p, fs::Permissions::from_mode(0o755)).unwrap();
            }
            fs::remove_dir_all(state.join(d)).unwrap();
            plant(&state.join(d), what);
            let before = snapshot(b);
            let refusal = set_up_refusal(&state);
            let says = format!("holds {d}, which is");
            assert!(refusal.reason.contains(&says), "{case}: {refusal}");
            assert_eq!(snapshot(b), before, "{case}");
        }
    }
    assert_eq!(fs::symlink_metadata("/etc").unwrap().mode(), etc);

    // `--userns`: the state directory and what is planted in it are the range's.
    let (uid, gid) = (310_000, 410_000);
    let range = crate::statedir::Owner::Mapped { uid, gid };
    let give = |p: &Path| lchown(p, Some(uid), Some(gid)).unwrap();
    let ranged = |name: &str| {
        let state = b.join(name);
        fs::create_dir(&state).unwrap();
        fs::write(state.join(rnk_store::STATE_MARKER), "").unwrap();
        give(&state.join(rnk_store::STATE_MARKER));
        fs::set_permissions(&state, fs::Permissions::from_mode(0o755)).unwrap();
        give(&state);
        state
    };
    for d in ["blobs", "builds"] {
        for what in plants {
            let case = format!("the range's {d} as {what}");
            let state = ranged(&case.replace(' ', "-").replace(['/', '\''], ""));
            plant(&state.join(d), what);
            give(&state.join(d));
            let before = snapshot(b);
            let Err(refusal) = hold_state_dir(&state, 0, range, || unreachable!("on overlayfs"))
            else {
                panic!("{case}: taken");
            };
            let says = format!("holds {d}, which is");
            assert!(refusal.reason.contains(&says), "{case}: {refusal}");
            assert_eq!(snapshot(b), before, "{case}");
        }
    }

    // The controls: root's store and the range's are taken, made private.
    let state = b.join("root-s");
    Store::open(&BuildPaths::new(&state, &state))
        .unwrap()
        .close()
        .unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o755)).unwrap();
    let me = crate::statedir::Owner::me();
    drop(hold_state_dir(&state, 0, me, || unreachable!("on overlayfs")).unwrap());
    assert_eq!(fs::metadata(&state).unwrap().mode() & 0o7777, 0o700);
    let state = ranged("the-range-s");
    drop(hold_state_dir(&state, 0, range, || unreachable!("on overlayfs")).unwrap());
    assert_eq!(fs::metadata(&state).unwrap().mode() & 0o7777, 0o700);
    // SAFETY: as above, cancelled.
    unsafe { libc::alarm(0) };
}

#[test]
fn kaniko_mode_on_a_host_without_mounts_is_refused() {
    // Q-330: never a bare chroot on the host; in a container, or with mount
    // privileges, it goes on.
    let refusal = host_without_mounts(false, false).expect("refused");
    assert!(refusal.reason.contains("bare chroot"), "{refusal}");
    assert_eq!(refusal.guidance, HOST_WITHOUT_MOUNTS);
    assert!(host_without_mounts(true, false).is_none());
    assert!(host_without_mounts(false, true).is_none());
}

/// Red team (criterion 8): only a refused overlay mount routes `--mode=auto`
/// to kaniko mode. A probe that mounted but could not unmount, failed
/// writing through the overlay, a state directory path overlay's options
/// cannot carry, an option string too long, a `Mount` error not wrapped
/// as the probe's, a probe error wrapping a probe error, and `ENFILE`
/// (the one excluded errno the earlier test leaves out) all refuse, in a
/// container or not.
#[test]
fn red_team_probe_failures_other_than_a_refused_mount_never_route() {
    use rustix::io::Errno;
    let mount = |errno| SnapshotError::Mount {
        target: "/s/merged".into(),
        lowers: 1,
        how: "mount(2)",
        step: "mount".into(),
        errno,
        options: Some(120),
    };
    let probe = |step, source| SnapshotError::Probe {
        step,
        source: Box::new(source),
    };
    let not_routed = [
        probe(
            "unmount",
            SnapshotError::Unmount {
                target: "/s/merged".into(),
                errno: Errno::BUSY,
            },
        ),
        probe(
            "write through the overlay",
            SnapshotError::Rootfs(rnk_rootfs::Error::InvalidPath {
                path: "new".into(),
                reason: "test",
            }),
        ),
        SnapshotError::UnsafePath {
            path: "/s,lowerdir=/".into(),
            reason: "a comma",
        },
        probe(
            "mount",
            SnapshotError::OptionsTooLong {
                target: "/s/merged".into(),
                lowers: 1,
                len: 5000,
                max: 4095,
            },
        ),
        // Not the probe's: never a refused probe mount.
        mount(Errno::INVAL),
        probe("mount", probe("mount", mount(Errno::INVAL))),
        probe("mount", mount(Errno::NFILE)),
    ];
    for e in &not_routed {
        let r = overlay_refusal(e, 0);
        assert_eq!(r.kind, Kind::Other, "{e}");
        for in_container in [false, true] {
            assert_eq!(route(&r, in_container), Route::Refuse, "{e}");
        }
        assert!(!r.needs_kaniko_mode(), "{e}");
    }
    // Not root: whatever the kernel said, never kaniko mode.
    let r = overlay_refusal(&probe("mount", mount(Errno::INVAL)), USER);
    assert_eq!(route(&r, false), Route::Refuse);
}

/// Phase 2 acceptance criterion 5: a second kaniko-mode build on `/` in one
/// container is refused by the `flock` on `/` itself, whatever its state
/// directory. `lock_slash` is the lock `set_up_kaniko` takes on `/` before
/// it writes anything (the state directory comes after it). Each call opens
/// `/` anew, so two calls are two open file descriptions, as two builds
/// are: the second is refused while the first holds the lock, and takes it
/// once the first lets go. The descriptor is `/`'s own inode, not a path a
/// `RUN` could replace (it cannot replace `/`). `T2c` (and the VM, a
/// disposable root): an advisory lock on the machine's `/`, held for the
/// test only; end to end, with two builds, in the VM's quick suite
/// (`slash_lock_refuses_a_second_build`).
#[test]
#[ignore = "T2c: default: flock on the container's / as root, as two kaniko-mode builds would"]
fn a_second_build_on_slash_is_refused_by_the_lock() {
    let first =
        lock_slash().unwrap_or_else(|r| panic!("the first build cannot lock /: {}", r.reason));
    let st = rustix::fs::fstat(&first).unwrap();
    let slash = rustix::fs::stat("/").unwrap();
    assert_eq!(
        (st.st_dev, st.st_ino),
        (slash.st_dev, slash.st_ino),
        "the lock is on / itself"
    );
    let Err(refusal) = lock_slash() else {
        panic!("a second build on / took the lock the first holds");
    };
    assert_eq!(
        refusal.reason,
        "another ranikust build is using / in this container"
    );
    assert_eq!(
        refusal.guidance,
        "run one kaniko-mode build at a time per container"
    );
    assert_eq!(
        refusal.kind,
        Kind::Other,
        "never a reason for --mode=auto to route elsewhere"
    );
    // Still refused: the refusal took nothing from the first build.
    assert!(
        lock_slash().is_err(),
        "the first build lost its lock to a refused one"
    );
    drop(first);
    let again =
        lock_slash().unwrap_or_else(|r| panic!("/ stays locked after the build: {}", r.reason));
    drop(again);
}

#[test]
fn a_store_refusal_names_its_state_directory_once() {
    use rnk_store::Error;
    let dir = Path::new("/home/u/state");
    let path = || dir.to_owned();
    let source = || io::Error::from(rustix::io::Errno::OPNOTSUPP);
    let (left, found, why) = (String::from("x"), String::from("x"), String::from("x"));
    for e in [
        Error::NotAStateDir {
            path: path(),
            why: why.clone(),
        },
        Error::NoFlock {
            path: path(),
            source: source(),
        },
        Error::Busy { path: path() },
        Error::InUse { path: path() },
        Error::NotPruned {
            path: path(),
            left: left.clone(),
        },
        Error::PruneCutShort { path: path() },
        Error::StateNotEmpty {
            path: path(),
            found,
        },
        Error::NotEmptied { path: path(), left },
        Error::Unmarked {
            path: path(),
            emptied: false,
            held: false,
            why: why.clone(),
        },
        Error::UsedOnSlash { path: path() },
        Error::AnonymousFile {
            path: path(),
            source: source(),
        },
        Error::NameOutlivesUnlink { path: path(), why },
        Error::Io {
            op: "create",
            path: path(),
            source: source(),
        },
    ] {
        let refusal = store_refusal(&e, dir, 1000);
        assert_eq!(refusal.reason.matches("/home/u/state").count(), 1, "{e}");
        assert_eq!(refusal.kind, Kind::Other, "{e}");
    }
    // What failed below it says where the store is too.
    let e = Error::Cancelled;
    let refusal = store_refusal(&e, dir, 1000);
    assert_eq!(
        refusal.reason,
        format!("cannot open the store in /home/u/state: {e}")
    );
}

/// A build refused by a `prune --all` that did not finish is shown the
/// next step, for the directory the store names: a user runs `prune --all`
/// again, root, which it refuses, empties the directory itself where
/// ranikust can check it; other refusals get no guidance.
#[test]
fn a_build_refused_by_an_unfinished_prune_is_shown_the_next_step() {
    use rnk_store::Error;
    // Paths that do not exist, below no host directory a symlink may be.
    let tmp = private_tempdir();
    let given = &tmp.path().join("link");
    let real = &tmp.path().join("gone/my state");
    let cut_short = || Error::PruneCutShort {
        path: real.to_owned(),
    };
    let refusal = store_refusal(&cut_short(), given, 1000);
    assert_eq!(
        refusal.guidance,
        format!(
            "ranikust prune --all finishes emptying it or says what stops it: run ranikust prune \
             --all --state-dir '{}'",
            real.display()
        )
    );
    assert!(
        refusal
            .to_string()
            .ends_with(&format!("\n  {}", refusal.guidance)),
        "{refusal}"
    );
    // Root gets commands for a directory ranikust checks, none where it
    // cannot.
    let root = store_refusal(&cut_short(), given, 0);
    assert!(
        root.guidance.starts_with(&format!(
            "ranikust gives no commands for '{}': ",
            real.display()
        )),
        "{root}"
    );
    assert!(!root.guidance.contains("cd -P"), "{root}");
    let left = tmp.path().join("my state");
    std::fs::create_dir_all(left.join("blobs")).unwrap();
    // Private whatever the umask: root gets commands only for a directory
    // a build accepts.
    std::fs::set_permissions(&left, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(left.join(rnk_store::PRUNING_MARKER), b"").unwrap();
    let e = Error::PruneCutShort { path: left.clone() };
    let root = store_refusal(&e, &left, 0);
    let (dev, ino, uid) = {
        use std::os::unix::fs::MetadataExt as _;
        let m = std::fs::metadata(&left).unwrap();
        (m.dev(), m.ino(), m.uid())
    };
    let commands = format!(
        "( {} && CDPATH= cd -P -- '{}' && stat -c %d:%i:%u:%a . | grep -qxF {dev}:{ino}:{uid}:700 && exec flock -xn . sh -c '",
        crate::prune::ENV_CHECK,
        left.display()
    );
    assert!(root.guidance.contains(&commands), "{root}");
    // Only the reason's account of the prune: nothing sends root to it.
    assert_eq!(root.to_string().matches("prune --all").count(), 1, "{root}");
    for e in [
        Error::Busy {
            path: real.to_owned(),
        },
        Error::NotPruned {
            path: real.to_owned(),
            left: "\"x\"".to_owned(),
        },
    ] {
        assert_eq!(store_refusal(&e, given, 1000).guidance, "", "{e}");
    }
}

#[test]
fn the_user_namespace_line_says_whether_the_path_passes_and_where_it_stops() {
    let steps = "unshare(CLONE_NEWUSER): ok; map: multi-range; chown: ok";
    let ok = Some(Ok(Ok(mount_api_name(MountApi::LowerdirPlus).to_owned())));
    assert_eq!(
        userns_summary(None, steps, ok.clone(), "0"),
        (
            true,
            "passes; unshare(CLONE_NEWUSER): ok; map: multi-range; chown: ok; overlay: ok, \
             fsconfig(lowerdir+); kernel.apparmor_restrict_unprivileged_userns: 0"
                .to_owned()
        )
    );
    let (passes, line) = userns_summary(Some("unshare"), steps, None, "absent");
    assert!(!passes);
    assert!(line.starts_with("stops at unshare; "), "{line}");
    assert!(
        line.ends_with(
            "; overlay: not run (no id map); kernel.apparmor_restrict_unprivileged_userns: absent"
        ),
        "{line}"
    );
    let refused = Some(Ok(Err("mount: EINVAL".to_owned())));
    let (passes, line) = userns_summary(None, steps, refused, "1");
    assert!(!passes);
    assert!(line.starts_with("stops at overlay; "), "{line}");
    assert!(
        line.contains("; overlay: refused: mount: EINVAL; "),
        "{line}"
    );
    // A probe that could not run is not a refusal.
    let host = Some(Err("fork the probe's child: EAGAIN".to_owned()));
    let (passes, line) = userns_summary(None, steps, host, "1");
    assert!(!passes);
    assert!(line.starts_with("overlay not probed; "), "{line}");
    assert!(
        line.contains("; overlay: could not run: fork the probe's child: EAGAIN; "),
        "{line}"
    );
    // The steps decide first.
    let (passes, line) = userns_summary(Some("chown"), steps, ok, "1");
    assert!(!passes);
    assert!(line.starts_with("stops at chown; "), "{line}");
}

/// Red team (#74): the user namespace line never reads as a mode line.
#[test]
fn red_team_the_user_namespace_line_never_reads_as_a_mode_line() {
    let steps = [
        "unshare(CLONE_NEWUSER): ok; map: multi-range; chown: ok; unshare(CLONE_NEWNS): ok; \
         proc: new instance; sysfs: rbind of /sys (mount sysfs: EPERM); mknod: EPERM",
        "unshare(CLONE_NEWUSER): ok; map: one uid: write uid_map: EPERM",
        "unshare(CLONE_NEWUSER): EPERM",
    ];
    let api = |a| Some(Ok(Ok(mount_api_name(a).to_owned())));
    let overlays: [OverlayInNamespace; 5] = [
        api(MountApi::LowerdirPlus),
        api(MountApi::Mount2),
        Some(Ok(Err("mount: EINVAL".to_owned()))),
        Some(Err("fork the probe's child: EAGAIN".to_owned())),
        None,
    ];
    let marks = [
        "overlay mode",
        "kaniko mode",
        "set up for",
        "RUN enters its root",
        "RUN directly",
        "RUN in the full sandbox",
        "directory per stage",
        "root may not mount",
        "cannot run here",
    ];
    for stop in [
        None,
        Some("unshare"),
        Some("map"),
        Some("chown"),
        Some("mknod"),
    ] {
        for step in steps {
            for overlay in &overlays {
                for apparmor in ["0", "1", "absent", "unreadable: EACCES"] {
                    let (_, line) = userns_summary(stop, step, overlay.clone(), apparmor);
                    assert!(!line.contains('\n'), "{line}");
                    assert!(
                        ["passes; ", "stops at ", "overlay not probed; "]
                            .iter()
                            .any(|v| line.starts_with(v)),
                        "{line}"
                    );
                    for mark in marks {
                        assert!(!line.contains(mark), "{mark:?} in {line}");
                    }
                }
            }
        }
    }
}

pub(crate) mod userns;

/// Only a probe directory named as the store's leftovers is held while the
/// probe runs: a store removes those it can lock. One in a build's
/// directory (kaniko mode's report) is never a leftover, and never held.
#[test]
fn only_a_probe_directory_a_store_would_remove_is_held() {
    let dir = tempfile::tempdir().unwrap();
    let root = RootPath::open(dir.path()).unwrap();
    let lock = |name: &str| {
        let fd = fs::File::open(dir.path().join(name)).unwrap();
        rustix::fs::flock(&fd, FlockOperation::NonBlockingLockExclusive).is_ok()
    };
    let name = format!("{}1-2", rnk_store::USERNS_PROBE_PREFIX);
    let (_, held) = make_probe_dir(&root, dir.path(), &name).unwrap().unwrap();
    assert!(held.is_some());
    assert!(!lock(&name), "held");
    drop(held);
    // A test alongside that forks meanwhile gives its child a copy of the
    // descriptor, which holds the lock until that child execs or exits.
    let released = (0..500).any(|_| {
        lock(&name) || {
            std::thread::sleep(std::time::Duration::from_millis(10));
            false
        }
    });
    assert!(released, "released");
    let (_, held) = make_probe_dir(&root, dir.path(), "userns-probe")
        .unwrap()
        .unwrap();
    assert!(held.is_none());
    assert!(lock("userns-probe"));
}
