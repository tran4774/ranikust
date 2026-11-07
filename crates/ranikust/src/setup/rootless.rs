//! Rootless overlay mode (ADR-0104, ADR-0107): the user namespace probe
//! routes before the build's user namespace is entered ([`plan`]), the
//! process unchanged; the build is then set up in that namespace
//! ([`set_up`]), from which no other mode may follow in that process: root's
//! `--mode=auto` sets up in a child, to fall back to kaniko mode in the
//! parent (ADR-0136).

use std::path::Path;
use std::sync::Arc;

use rnk_sandbox::{Fallbacks, KernelFs, MountResult, Sandbox, SubordinateIds};

use super::{Kind, Outcome, Refusal, Setup};
use crate::config::Seccomp;

/// What the probe before the build's user namespace found: how to enter
/// it, and what the sandbox may bind there.
pub(crate) struct Plan {
    /// The subordinate ids the probe's maps used.
    subordinate: Option<SubordinateIds>,
    /// The operator's request and what the probe needed.
    fallbacks: Fallbacks,
    /// Whether the build binds `/proc`.
    proc_rbind: bool,
    /// The state directory's shared lock, from its check until the store
    /// has taken its own ([`set_up`]).
    held: rnk_store::StateDirHold,
}

impl Plan {
    /// Whether the build gets an rbind of `/proc`: `--allow proc.rbind`
    /// where the probe found a new `proc` refused. The caller runs the
    /// peer scan before [`set_up`] then (ADR-0129, ADR-0130).
    pub(crate) fn proc_rbind(&self) -> bool {
        self.proc_rbind
    }
}

/// Why [`plan`] found no rootless overlay mode here; the process is
/// unchanged.
pub(crate) struct NotHere {
    /// What failed, and what to do instead.
    pub(crate) refusal: Refusal,
    /// Whether the user namespace probe ran and logged its line.
    pub(crate) probed: bool,
}

/// Guidance where the state directory is on overlayfs.
const ON_OVERLAYFS: &str = "mount a volume at the state directory, e.g. \
     `-v ranikust-state:/var/lib/ranikust`, or pass --state-dir on one";

/// Guidance where a new `proc` is refused and an rbind of `/proc` would
/// work, which the operator did not ask for (ADR-0126): the three ways on,
/// the flag with what it exposes (threat model, section 6).
const PROC_RBIND: &str = "a RUN needs a new proc, which this user namespace may not mount (a \
     container whose /proc has masked paths): build as a regular user on a host that allows \
     user namespaces, or in a container where ranikust may mount, or pass --allow proc.rbind, \
     through which every RUN sees each process of this PID namespace and reads the files of \
     /proc/<pid> not under ptrace's check, such as cmdline, status, stat, mountinfo, cgroup \
     and net/*, ranikust's included, and RUNs run one at a time; keep no secrets in other \
     processes' command lines and give the build container a PID namespace of its own: the \
     host's, one whose PID 1 is in another mount namespace, and one shared with a process of \
     another mount namespace that ranikust can open as it starts are refused (ADR-0126, \
     ADR-0129, ADR-0130, ADR-0134)";

/// Guidance where another step of the user namespace fails.
const UNUSABLE: &str = "run ranikust as root (with sudo on a host or in a VM), or in a \
     container: kaniko mode, or overlay mode when it is started with --privileged";

/// The test hook's `no-userns` (`env`, the hook's variable): rootless
/// overlay mode refused before its probe, the process unchanged, with the
/// guidance for the effective uid `euid`.
pub(crate) fn refused_by_hook(env: &str, euid: u32) -> NotHere {
    NotHere {
        refusal: Refusal {
            kind: Kind::Other,
            reason: format!("rootless overlay mode cannot run here: refused by {env}=no-userns"),
            guidance: guidance("hook", false, euid).into(),
        },
        probed: false,
    }
}

/// The user namespace probe for rootless overlay mode, before the build's
/// user namespace: the state directory's check ([`state_dir_for_probe`]),
/// the subordinate ids of this user ([`crate::subids`]), then the steps and
/// the overlay mount in a user namespace, in a directory made for them in
/// the state directory, named as the leftovers an anonymous store removes,
/// and removed after. Logs the probe's line, whose verdict counts an rbind
/// of `/proc` only with `proc_rbind` (`--allow proc.rbind`, ADR-0126). Call
/// while single-threaded.
///
/// # Errors
///
/// [`NotHere`]: the step that failed and what to do instead, with the
/// process unchanged.
pub(crate) fn plan(state_dir: &Path, proc_rbind: bool) -> Result<Plan, NotHere> {
    let euid = rustix::process::geteuid().as_raw();
    let unprobed = |refusal| NotHere {
        refusal,
        probed: false,
    };
    let held = state_dir_for_probe(state_dir, euid).map_err(unprobed)?;
    let subordinate = crate::subids::lookup(euid).map_err(|e| {
        unprobed(Refusal {
            kind: Kind::Other,
            reason: format!("cannot read this user's subordinate ids: {e}"),
            guidance: "".into(),
        })
    })?;
    let span = tracing::debug_span!("ranikust.probe_userns", passes = tracing::field::Empty);
    let _entered = span.enter();
    let name = format!("{}{}", rnk_store::USERNS_PROBE_PREFIX, unique());
    let probed = super::probe_in_dir(state_dir, &name, subordinate).map_err(|e| {
        tracing::warn!("ranikust: user namespace probe could not run: {e}");
        NotHere {
            refusal: Refusal {
                kind: Kind::Other,
                reason: format!(
                    "rootless overlay mode cannot run here: the user namespace probe could not \
                     run: {e}"
                ),
                guidance: "".into(),
            },
            probed: true,
        }
    })?;
    let allowed = super::allowed(proc_rbind);
    let (passes, line) = probed.summary(allowed);
    span.record("passes", passes);
    tracing::info!("ranikust: user namespace probe: {line}");
    let report = &probed.report;
    let not_here = |reason: String, guidance| NotHere {
        refusal: Refusal {
            kind: Kind::Other,
            reason: format!("rootless overlay mode cannot run here: {reason}"),
            guidance,
        },
        probed: true,
    };
    if let Some(step) = report.stop(allowed) {
        let rbind = matches!(report.proc, MountResult::Rbind { .. });
        return Err(not_here(
            format!("the user namespace probe stops at {step}"),
            guidance(step, rbind, euid).into(),
        ));
    }
    if let Some(e) = probed.overlay_error() {
        return Err(not_here(
            format!("an overlay mount in the user namespace: {e}"),
            guidance("overlay", false, euid).into(),
        ));
    }
    let bind_proc = proc_rbind && matches!(report.proc, MountResult::Rbind { .. });
    let fallbacks = if bind_proc {
        report.fallbacks().allow_proc_rbind()
    } else {
        report.fallbacks()
    };
    Ok(Plan {
        subordinate,
        fallbacks,
        proc_rbind: bind_proc,
        held,
    })
}

/// The state directory the probe may write in, made if missing: private
/// (ADR-0050), not on overlayfs, as for overlay mode as root, and one the
/// store would take, checked without writing (Q-111, (a)); its shared lock
/// held from then on, `euid` naming what to do with a prune cut short.
fn state_dir_for_probe(state_dir: &Path, euid: u32) -> Result<rnk_store::StateDirHold, Refusal> {
    super::prepare_state_dir(state_dir)?;
    if crate::statedir::on_overlayfs(state_dir) {
        return Err(Refusal {
            kind: Kind::Other,
            reason: format!(
                "rootless overlay mode cannot run here: the state directory {} is on overlayfs, \
                 a container's own root, where it cannot keep its upper directories",
                state_dir.display()
            ),
            guidance: ON_OVERLAYFS.into(),
        });
    }
    rnk_store::Store::check_state_dir(state_dir)
        .map_err(|e| super::store_refusal(&e, state_dir, euid))
}

/// What to do where the user namespace probe stops at `step` (`overlay`
/// for its overlay mount, `hook` for the test hook's refusal before it)
/// for the effective uid `euid`; `rbind` says an rbind of `/proc` worked
/// there. A user namespace without capabilities (Ubuntu's AppArmor switch)
/// stops a user other than root at the map, the `chown` or the mount
/// namespace.
fn guidance(step: &str, rbind: bool, euid: u32) -> &'static str {
    match step {
        "proc" if rbind => PROC_RBIND,
        // Root gets here only where it may not mount: its own refusal, or
        // kaniko mode, says what to do.
        _ if euid == 0 => "",
        "unshare" | "map" | "chown" | "mount namespace" => super::NO_USER_NAMESPACE,
        _ => UNUSABLE,
    }
}

/// A name no other build's probe takes at once: this process's id and the
/// time.
fn unique() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("{}-{nanos}", std::process::id())
}

/// What [`set_up`] does besides its steps.
pub(crate) struct Then<'a> {
    /// A `RUN` with `--network=none` may get a network namespace of its own
    /// (`--allow network.none-userns`), as for [`super::set_up`].
    pub(crate) userns_network: bool,
    /// The child that falls back (ADR-0136): a failure once the store is
    /// open abandons it ([`rnk_store::Store::abandon`]), a forwarded stop
    /// ignored meanwhile ([`rnk_sandbox::Supervised::undoing`]).
    pub(crate) abandon: Option<&'a rnk_sandbox::Supervised>,
    /// The test hook's variable where its `rootless-broken` fails the
    /// set-up once the store is open.
    pub(crate) broken_by: Option<&'static str>,
}

/// Enters the build's user namespace as `plan` says
/// ([`rnk_sandbox::enter_build_user_namespace`], which also enters the
/// build's mount namespace) and sets rootless overlay mode up in it: the
/// store, opened as root of the namespace ([`super::open_store`]); the
/// sandbox's probe with the fallbacks `plan` allows
/// ([`rnk_sandbox::probe_with_fallbacks`]); the overlay probe in the
/// build's directory; the outcome; then the spawner thread. `scanned` says
/// the peer scan passed (ADR-0129): a sandbox that binds `/proc` without it
/// is refused. `seccomp` as for [`super::set_up`], `then` as it says. Call
/// while single-threaded.
///
/// # Errors
///
/// A [`Refusal`]: of [`Kind::Other`] only where
/// [`rnk_sandbox::enter_build_user_namespace`] left the process unchanged,
/// else of [`Kind::Broken`]: the process must end.
pub(crate) fn set_up(
    state_dir: &Path,
    plan: Plan,
    scanned: bool,
    seccomp: &Seccomp,
    then: &Then<'_>,
) -> Result<Setup, Refusal> {
    let euid = rustix::process::geteuid().as_raw();
    let Plan {
        subordinate,
        fallbacks,
        held,
        ..
    } = plan;
    let userns = rnk_sandbox::enter_build_user_namespace(subordinate)
        .map_err(|e| super::userns_refusal(e, euid, super::NO_USER_NAMESPACE.into()))?;
    tracing::info!("ranikust: the build's user namespace: {userns}");
    let store = super::open_store(state_dir, state_dir, euid).map_err(super::inside_userns)?;
    // The store holds the directory now.
    drop(held);
    match in_store(state_dir, &store, fallbacks, scanned, seccomp, then) {
        Ok((outcome, sandbox)) => Ok(Setup {
            outcome,
            state_dir: state_dir.to_path_buf(),
            store: Arc::new(store),
            sandbox,
        }),
        Err(refusal) => {
            if let Some(child) = then.abandon {
                if let Err(e) = child.undoing() {
                    tracing::debug!("a stop may cut the store's removal short: {e}");
                }
                abandon(store);
            }
            Err(refusal)
        }
    }
}

/// [`set_up`]'s steps once the store is open: the outcome and the
/// sandbox.
fn in_store(
    state_dir: &Path,
    store: &rnk_store::Store,
    fallbacks: Fallbacks,
    scanned: bool,
    seccomp: &Seccomp,
    then: &Then<'_>,
) -> Result<(Outcome, Sandbox), Refusal> {
    if let Some(env) = then.broken_by {
        return Err(broken(format!("refused by {env}=rootless-broken")));
    }
    let sandbox_dir = state_dir.join(rnk_store::SANDBOX_DIR);
    let (caps, overlay) = {
        let span = tracing::debug_span!("ranikust.probe", steps = 0_u64);
        let _entered = span.enter();
        let caps = rnk_sandbox::probe_with_fallbacks(&sandbox_dir, fallbacks)
            .map_err(|e| broken(format!("the RUN sandbox cannot run here: {e}")))?;
        span.record("steps", 1_u64);
        if caps.proc() == KernelFs::Rbind && !scanned {
            return Err(broken(
                "the RUN sandbox would bind /proc, yet no peer scan ran".to_owned(),
            ));
        }
        let overlay = super::probe_overlay(store.build_dir(), 0).map_err(|r| broken(r.reason))?;
        span.record("steps", 2_u64);
        (caps, overlay)
    };
    let outcome = Outcome::OverlayRootless {
        sandbox: caps,
        overlay,
    };
    let mut sandbox = Sandbox::start(caps, &sandbox_dir).map_err(|e| broken(e.to_string()))?;
    if then.userns_network {
        sandbox.allow_userns_network();
    }
    seccomp.apply(&mut sandbox);
    // Once nothing is left to fail, so that a fallback logs one mode.
    tracing::info!("ranikust: {outcome}");
    Ok((outcome, sandbox))
}

/// Closes the store of a set-up that failed, and empties the state
/// directory where its open made it a store, so that kaniko mode on `/`
/// takes it (ADR-0136); else kaniko mode refuses it, as it would have.
fn abandon(store: rnk_store::Store) {
    let state = store.state_dir().to_path_buf();
    match store.abandon() {
        Ok(true) => tracing::debug!(
            "the store this set-up made in {} is removed",
            state.display()
        ),
        Ok(false) => {}
        Err(e) => tracing::warn!(
            "the store this set-up made in {} is not removed: {e}",
            state.display()
        ),
    }
}

/// A failure in the build's user namespace: no other mode may follow in
/// this process.
fn broken(reason: String) -> Refusal {
    Refusal {
        kind: Kind::Broken,
        reason: format!("rootless overlay mode: {reason}"),
        guidance: "".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A user other than root whose user namespace has no capabilities
    /// (Ubuntu 24.04 stops it at the `chown`) is pointed at the AppArmor
    /// switch, and a step past the namespace at running as root; root, whose
    /// own refusal or kaniko mode says what to do, at nothing more; a
    /// refused new `proc` with a working rbind, at `--allow proc.rbind`.
    #[test]
    fn each_stop_of_the_probe_gets_its_guidance() {
        for step in ["unshare", "map", "chown", "mount namespace"] {
            assert_eq!(
                guidance(step, false, 1000),
                super::super::NO_USER_NAMESPACE,
                "{step}"
            );
        }
        for step in [
            "unshare",
            "map",
            "chown",
            "mount namespace",
            "proc",
            "sysfs",
            "overlay",
            "hook",
        ] {
            assert_eq!(guidance(step, false, 0), "", "{step}");
        }
        assert_eq!(guidance("proc", true, 1000), PROC_RBIND);
        assert_eq!(guidance("proc", true, 0), PROC_RBIND);
        for step in ["proc", "sysfs", "overlay", "hook"] {
            assert_eq!(guidance(step, false, 1000), UNUSABLE, "{step}");
        }
        assert!(super::super::NO_USER_NAMESPACE.contains("apparmor_restrict_unprivileged_userns"));
        assert!(PROC_RBIND.contains("--allow proc.rbind"));
        assert!(PROC_RBIND.contains("not under ptrace's check"));
    }

    /// The probe writes nothing in a directory the store would refuse, and
    /// leaves a new one unmarked, held until the store opens.
    #[test]
    fn the_probe_writes_only_in_a_state_directory() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        // The probe refuses overlayfs before the store check this test is about.
        if crate::statedir::on_overlayfs(tmp.path()) {
            eprintln!("skipped: the temporary directory is on overlayfs");
            return;
        }
        let private = |p: &Path| {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700)).unwrap();
        };
        private(tmp.path());
        let names = |p: &Path| -> Vec<_> {
            std::fs::read_dir(p)
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect()
        };
        let foreign = tmp.path().join("foreign");
        std::fs::create_dir(&foreign).unwrap();
        private(&foreign);
        std::fs::write(foreign.join("precious"), "x").unwrap();
        let refusal = state_dir_for_probe(&foreign, 1000).unwrap_err();
        assert!(
            refusal
                .reason
                .contains("has no regular file .ranikust-state"),
            "{}",
            refusal.reason
        );
        assert_eq!(names(&foreign), ["precious"]);
        let new = tmp.path().join("new");
        let held = state_dir_for_probe(&new, 1000).unwrap();
        assert!(names(&new).is_empty());
        // Held until the store opens: no prune empties it meanwhile.
        let pruned = rnk_store::Store::prune_all(&new);
        assert!(
            matches!(pruned, Err(rnk_store::Error::InUse { .. })),
            "{pruned:?}"
        );
        drop(held);
    }
}
