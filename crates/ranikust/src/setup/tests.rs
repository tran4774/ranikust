use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

use rnk_sandbox::Step;

use super::*;

/// Linux's `EACCES`, without a dependency on `libc` for one constant.
const EACCES: i32 = 13;

/// Not root: the uid of a test user. Only the comparison with 0 matters.
const USER: u32 = 1000;

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
        guidance: "",
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
    for kind in [Kind::SandboxBlocked, Kind::NoMountPrivilege, Kind::Other] {
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
        guidance: OVERLAY_UNUSABLE,
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
        guidance: OVERLAY_UNUSABLE,
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

/// A fresh temp dir no one else may write, whatever the umask.
fn private_tempdir() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o700)).unwrap();
    tmp
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
