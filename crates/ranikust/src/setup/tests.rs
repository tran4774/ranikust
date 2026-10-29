use std::io;

use rnk_sandbox::Step;

use super::*;

/// Linux's `EACCES`, without a dependency on `libc` for one constant.
const EACCES: i32 = 13;

fn overlay_ok() -> Result<OverlayCaps, Refusal> {
    Ok(OverlayCaps { probed: true })
}

#[test]
fn both_probes_succeeding_pick_overlay_as_root() {
    let picked = pick(Ok("caps"), overlay_ok()).unwrap();
    assert_eq!(picked, ("caps", OverlayCaps { probed: true }));
}

#[test]
fn a_sandbox_without_privileges_needs_root() {
    let r = pick::<()>(
        Err(SandboxError::Privilege("the RUN sandbox")),
        overlay_ok(),
    );
    let refusal = r.unwrap_err();
    assert_eq!(refusal.guidance, NEEDS_ROOT);
    assert!(refusal.reason.contains("RUN sandbox"), "{refusal}");
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
    );
    let text = r.unwrap_err().to_string();
    assert!(text.contains("proc"), "{text}");
    assert!(text.contains("denied"), "{text}");
    assert!(text.contains("kaniko mode"), "{text}");
}

#[test]
fn a_failed_overlay_probe_refuses_even_with_a_sandbox() {
    let failed = Refusal {
        reason: "overlay: EINVAL".to_owned(),
        guidance: NEEDS_KANIKO_MODE,
    };
    let r = pick(Ok(()), Err(failed));
    assert_eq!(r.unwrap_err().reason, "overlay: EINVAL");
}

#[test]
fn the_plain_chroot_refusal_says_why() {
    let refusal = namespace_refusal(SandboxError::InPlainChroot);
    let text = refusal.to_string();
    assert!(text.contains("plain chroot"), "{text}");
    assert!(!text.contains("Invalid argument"), "{text}");
}

#[test]
fn no_privileges_for_the_namespace_needs_root() {
    let refusal = namespace_refusal(SandboxError::Privilege("a private mount namespace"));
    assert_eq!(refusal.guidance, NEEDS_ROOT);
    assert!(refusal.to_string().contains("sudo"));
}
