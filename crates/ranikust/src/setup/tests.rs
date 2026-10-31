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
fn a_refused_overlay_mount_needs_kaniko_mode_as_root_but_a_host_error_does_not() {
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
    assert_eq!(r.guidance, NEEDS_KANIKO_MODE);
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
    // Root without the privileges: Docker's defaults, kaniko mode's place.
    let refusal = pick::<()>(
        Err(SandboxError::Privilege("the RUN sandbox")),
        overlay_ok(),
        0,
    )
    .unwrap_err();
    assert_eq!(refusal.guidance, NEEDS_KANIKO_MODE);
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
        reason: "overlay: EINVAL".to_owned(),
        guidance: NEEDS_KANIKO_MODE,
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
