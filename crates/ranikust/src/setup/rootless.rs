//! Rootless overlay mode (ADR-0104, ADR-0107): the user namespace probe
//! routes before the build's user namespace is entered ([`plan`]), the
//! process unchanged; the build is then set up in that namespace
//! ([`set_up`]), from which no other mode may follow in that process: root's
//! `--mode=auto` sets up in a child, to fall back to kaniko mode in the
//! parent (ADR-0136). Root's `--userns` takes the same path with its
//! `ranikust` range ([`rnk_sandbox::BuildIds::Shifted`]), and never falls
//! back.

use std::borrow::Cow;
use std::path::Path;
use std::sync::Arc;

use rnk_sandbox::{
    BuildIds, Fallbacks, KernelFs, MountResult, Sandbox, StepResult, UsernsCapabilities,
    UsernsReport,
};
use rnk_store::{BuildPaths, Store};

use super::no_userns::Stop;
use super::{Kind, Outcome, Refusal, Setup};
use crate::config::Seccomp;

/// What the probe before the build's user namespace found: how to enter
/// it, and what the sandbox may bind there.
pub(crate) struct Plan {
    /// The ids the probe's maps used.
    ids: BuildIds,
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

/// Guidance where a step of `--userns`'s user namespace fails.
const USERNS_UNUSABLE: &str = "--userns needs a user namespace in which root's ranikust range \
     may mount: run ranikust as root on a host or in a VM, or in a container started with \
     --privileged";

/// Guidance where another step of the user namespace fails.
const UNUSABLE: &str = "run ranikust as root (with sudo on a host or in a VM), or in a \
     container: kaniko mode, or overlay mode when it is started with --privileged";

/// The test hook's `no-userns` (`env`, the hook's variable): rootless
/// overlay mode refused before its probe, the process unchanged, with the
/// guidance for the effective uid `euid` where no user namespace is
/// usable, under `--userns` with `userns`, `binary` as for [`plan`].
pub(crate) fn refused_by_hook(
    env: &str,
    euid: u32,
    userns: bool,
    binary: Option<&Path>,
) -> NotHere {
    let what = if userns {
        "--userns"
    } else {
        "rootless overlay mode"
    };
    NotHere {
        refusal: Refusal {
            kind: Kind::Other,
            reason: format!("{what} cannot run here: refused by {env}=no-userns"),
            // As a user namespace without capabilities stops.
            guidance: guidance("hook", Stop::InNamespace, false, euid, userns, binary),
        },
        probed: false,
    }
}

/// The user namespace probe for rootless overlay mode, before the build's
/// user namespace: for a user other than root, whether it gets one with
/// capabilities, with nothing written ([`capabilities`]); the ids to map
/// (this user's subordinate ids, [`crate::subids::lookup`]; with `userns`,
/// root's `ranikust` range, [`crate::subids::root_range`], refused where
/// missing), the state directory's check for the namespace's root
/// ([`state_dir_for_probe`]), then the steps and the overlay mount in a
/// user namespace, in a directory made for them in the state directory,
/// named as the leftovers a store removes: removed after, or, with
/// `userns`, left to the build's store, which removes it from inside the
/// namespace, as it does an earlier build's; this process removes only an
/// earlier build's it had not yet given away
/// ([`super::remove_own_probe_leftovers`]). Logs the probe's line, whose
/// verdict counts an rbind of `/proc` only with `proc_rbind` (`--allow
/// proc.rbind`, ADR-0126). `binary` is the path ranikust started from,
/// which the advice where no user namespace is usable weighs
/// ([`super::no_userns`]): a user other than root's, never `--userns`'s.
/// Call while single-threaded.
///
/// # Errors
///
/// [`NotHere`]: the step that failed and what to do instead, with the
/// process unchanged; with `userns`, nothing written where the range is
/// missing or the state directory refused.
pub(crate) fn plan(
    state_dir: &Path,
    proc_rbind: bool,
    userns: bool,
    binary: Option<&Path>,
) -> Result<Plan, NotHere> {
    let euid = rustix::process::geteuid().as_raw();
    let what = if userns {
        "--userns"
    } else {
        "rootless overlay mode"
    };
    // With capabilities, AppArmor's switch is not what stops any step;
    // root's `--userns` is not a user's case.
    let capable = !userns && euid != 0 && capabilities(euid, binary)?;
    let unprobed = |refusal| NotHere {
        refusal,
        probed: false,
    };
    // Read first: a `--userns` state directory is checked for, and made
    // for, the range's first ids.
    let (ids, owner) = if userns {
        let sub = crate::subids::root_range().map_err(|reason| {
            unprobed(Refusal {
                kind: Kind::Other,
                reason: format!("--userns cannot run here: {reason}"),
                guidance: crate::subids::ROOT_RANGE_GUIDANCE.into(),
            })
        })?;
        let owner = crate::statedir::Owner::Mapped {
            uid: sub.uids.start,
            gid: sub.gids.start,
        };
        (BuildIds::Shifted(sub), owner)
    } else {
        let subordinate = crate::subids::lookup(euid).map_err(|e| {
            unprobed(Refusal {
                kind: Kind::Other,
                reason: format!("cannot read this user's subordinate ids: {e}"),
                guidance: "".into(),
            })
        })?;
        (BuildIds::Own(subordinate), crate::statedir::Owner::me())
    };
    let held = state_dir_for_probe(state_dir, euid, owner, what).map_err(unprobed)?;
    if userns {
        // The store, in the namespace, removes those it was given.
        super::remove_own_probe_leftovers(state_dir);
    }
    let span = tracing::debug_span!("ranikust.probe_userns", passes = tracing::field::Empty);
    let _entered = span.enter();
    let name = format!("{}{}", rnk_store::USERNS_PROBE_PREFIX, unique());
    let probed = super::probe_in_dir(state_dir, &name, ids).map_err(|e| {
        tracing::warn!("ranikust: user namespace probe could not run: {e}");
        NotHere {
            refusal: Refusal {
                kind: Kind::Other,
                reason: format!(
                    "{what} cannot run here: the user namespace probe could not run: {e}"
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
            reason: format!("{what} cannot run here: {reason}"),
            guidance,
        },
        probed: true,
    };
    if let Some(step) = report.stop(allowed) {
        let rbind = matches!(report.proc, MountResult::Rbind { .. });
        let stop = stop_for(step, capable, &Steps::probed(report));
        return Err(not_here(
            format!("the user namespace probe stops at {step}"),
            guidance(step, stop, rbind, euid, userns, binary),
        ));
    }
    if let Some(e) = probed.overlay_error() {
        return Err(not_here(
            format!("an overlay mount in the user namespace: {e}"),
            guidance("overlay", Stop::Unknown, false, euid, userns, binary),
        ));
    }
    let bind_proc = proc_rbind && matches!(report.proc, MountResult::Rbind { .. });
    let fallbacks = if bind_proc {
        report.fallbacks().allow_proc_rbind()
    } else {
        report.fallbacks()
    };
    Ok(Plan {
        ids,
        fallbacks,
        proc_rbind: bind_proc,
        held,
    })
}

/// Whether a user other than root (`euid`) gets a user namespace with
/// capabilities, checked before the state directory is made or anything
/// else written (#81): `Ok(true)` where it does, `Ok(false)` where the
/// check cannot tell, and the probe decides; `binary` as for [`plan`].
///
/// # Errors
///
/// The refusal where it gets none, logged as the probe's line.
fn capabilities(euid: u32, binary: Option<&Path>) -> Result<bool, NotHere> {
    let found = match rnk_sandbox::probe_user_namespace_capabilities() {
        Ok(found) => found,
        Err(e) => {
            tracing::debug!("ranikust: user namespace capability check: {e}");
            return Ok(false);
        }
    };
    let Some(step) = found.stop() else {
        return Ok(shown_capable(&found));
    };
    tracing::info!(
        "ranikust: user namespace probe: stops at {step}, nothing written; {found}; \
         kernel.apparmor_restrict_unprivileged_userns: {}",
        super::apparmor_switch()
    );
    Err(NotHere {
        refusal: Refusal {
            kind: Kind::Other,
            reason: format!(
                "rootless overlay mode cannot run here: the user namespace probe stops at {step}"
            ),
            guidance: guidance(
                step,
                stop_for(step, false, &Steps::checked(&found)),
                false,
                euid,
                false,
                binary,
            ),
        },
        probed: true,
    })
}

/// Whether the capability check shows a user namespace with capabilities:
/// both its steps ran and worked.
fn shown_capable(found: &UsernsCapabilities) -> bool {
    found.unshare == StepResult::Ok && found.mount_ns == StepResult::Ok
}

/// The results of the steps the advice weighs, from the probe or the
/// capability check; `None`: the step is not reported.
struct Steps<'a> {
    unshare: &'a StepResult,
    /// The last map tried.
    map: Option<&'a StepResult>,
    chown: Option<&'a StepResult>,
    mount_ns: &'a StepResult,
}

impl<'a> Steps<'a> {
    fn probed(report: &'a UsernsReport) -> Self {
        Self {
            unshare: &report.unshare,
            map: report.maps.last().map(|(_, result)| result),
            chown: Some(&report.chown),
            mount_ns: &report.mount_ns,
        }
    }

    fn checked(found: &'a UsernsCapabilities) -> Self {
        Self {
            unshare: &found.unshare,
            map: None,
            chown: None,
            mount_ns: &found.mount_ns,
        }
    }
}

/// What stopped the probe at `step`, as the advice weighs it: [`Stop::at`]
/// on that step's own result; `Unknown` for a step not reported, and for
/// any step where the capability check found the namespace's capabilities
/// (`capable`), as AppArmor's switch is then not what refused.
fn stop_for(step: &str, capable: bool, steps: &Steps<'_>) -> Stop {
    if capable {
        return Stop::Unknown;
    }
    let result = match step {
        "unshare" => Some(steps.unshare),
        "map" => steps.map,
        "chown" => steps.chown,
        "mount namespace" => Some(steps.mount_ns),
        _ => None,
    };
    result.map_or(Stop::Unknown, |result| {
        Stop::at(step, steps.unshare, result)
    })
}

/// The state directory the probe may write in, made if missing: private
/// (ADR-0050) and `owner`'s ([`crate::statedir::prepare_for`]), not on
/// overlayfs, as for overlay mode as root, and one the store would take,
/// checked without writing (Q-111, (a)); its shared lock held from then
/// on, `euid` naming what to do with a prune cut short. Each refusal comes
/// before anything is made or changed: overlayfs is told from the nearest
/// directory that exists, and the store's check runs before the mode is
/// set. `what` names the mode in messages.
fn state_dir_for_probe(
    state_dir: &Path,
    euid: u32,
    owner: crate::statedir::Owner,
    what: &str,
) -> Result<rnk_store::StateDirHold, Refusal> {
    let refusal = |refused: crate::statedir::Refused| Refusal {
        kind: Kind::Other,
        reason: refused.0,
        guidance: "".into(),
    };
    // A missing state directory would be made on its nearest parent's
    // filesystem.
    if crate::statedir::on_overlayfs(crate::statedir::nearest_existing(state_dir)) {
        return Err(Refusal {
            kind: Kind::Other,
            reason: format!(
                "{what} cannot run here: the state directory {} is on overlayfs, a container's \
                 own root, where it cannot keep its upper directories",
                state_dir.display()
            ),
            guidance: ON_OVERLAYFS.into(),
        });
    }
    let staged = crate::statedir::stage(state_dir, owner).map_err(refusal)?;
    let held = rnk_store::Store::check_state_dir(state_dir)
        .map_err(|e| super::store_refusal(&e, state_dir, euid))?;
    staged.finish().map_err(refusal)?;
    Ok(held)
}

/// What to do where the user namespace probe stops at `step` (`overlay`
/// for its overlay mount, `hook` for the test hook's refusal before it, as
/// where no user namespace is usable) for the effective uid `euid`, under
/// `--userns` with `userns`; `stop` is what stopped it there, as the advice
/// where no user namespace is usable weighs it ([`Stop::at`]); `rbind` says
/// an rbind of `/proc` worked there; `binary` as for [`plan`]. A user
/// namespace without capabilities (Ubuntu's AppArmor switch) stops a user
/// other than root at the map, the `chown` or the mount namespace.
fn guidance(
    step: &str,
    stop: Stop,
    rbind: bool,
    euid: u32,
    userns: bool,
    binary: Option<&Path>,
) -> Cow<'static, str> {
    match step {
        "proc" if rbind => PROC_RBIND.into(),
        // Root's `--userns` has no other mode to say what to do.
        _ if userns => USERNS_UNUSABLE.into(),
        // Root gets here only where it may not mount: its own refusal, or
        // kaniko mode, says what to do.
        _ if euid == 0 => "".into(),
        "unshare" | "map" | "chown" | "mount namespace" | "hook" => {
            super::no_userns::guidance(binary, stop).into()
        }
        _ => UNUSABLE.into(),
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
    /// The child that falls back (ADR-0136): a failure once the store's
    /// open took the directory abandons the store
    /// ([`rnk_store::Store::open_or_abandon`], [`rnk_store::Store::abandon`]),
    /// a forwarded stop ignored meanwhile
    /// ([`rnk_sandbox::Supervised::undoing`]).
    pub(crate) child: Option<&'a rnk_sandbox::Supervised>,
    /// The test hook's variable where its `rootless-broken` fails the
    /// set-up at its last step, once the sandbox started.
    pub(crate) broken_by: Option<&'static str>,
    /// The path ranikust started from, as for [`plan`].
    pub(crate) binary: Option<&'a Path>,
}

/// Enters the build's user namespace as `plan` says
/// ([`rnk_sandbox::enter_build_user_namespace`], which also enters the
/// build's mount namespace) and sets rootless overlay mode up in it: the
/// store, opened as root of the namespace ([`super::open_store`]); the
/// sandbox's probe with the fallbacks `plan` allows
/// ([`rnk_sandbox::probe_with_fallbacks`]); the overlay probe in the
/// build's directory; the spawner thread; then the outcome's line.
/// `scanned` says the peer scan passed (ADR-0129): a sandbox that binds
/// `/proc` without it is refused. `seccomp` as for [`super::set_up`],
/// `then` as it says. Call while single-threaded.
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
        ids,
        fallbacks,
        held,
        ..
    } = plan;
    let shifted = match ids {
        BuildIds::Own(_) => false,
        BuildIds::Shifted(_) => true,
    };
    let userns = rnk_sandbox::enter_build_user_namespace(ids)
        .map_err(|e| entry_refusal(e, euid, shifted, then.binary))?;
    tracing::info!("ranikust: the build's user namespace: {userns}");
    let mut held = Some(held);
    let opened = match then.child {
        // An open that fails once it took the directory empties a store with
        // no data, its own lock the only one left: kaniko mode follows.
        Some(child) => Store::open_or_abandon(&BuildPaths::new(state_dir, state_dir), || {
            drop(held.take());
            undoing(child);
        })
        .map_err(|e| super::store_refusal(&e, state_dir, euid)),
        None => super::open_store(state_dir, state_dir, euid),
    };
    let store = opened.map_err(super::inside_userns)?;
    // The store holds the directory now.
    drop(held);
    match in_store(
        state_dir, &store, fallbacks, scanned, seccomp, then, shifted,
    ) {
        Ok((outcome, sandbox)) => Ok(Setup {
            outcome,
            state_dir: state_dir.to_path_buf(),
            store: Arc::new(store),
            sandbox,
        }),
        Err(refusal) => {
            if let Some(child) = then.child {
                undoing(child);
                abandon(store);
            }
            Err(refusal)
        }
    }
}

/// [`set_up`]'s steps once the store is open: the outcome and the
/// sandbox, under `--userns` with `userns`.
fn in_store(
    state_dir: &Path,
    store: &rnk_store::Store,
    fallbacks: Fallbacks,
    scanned: bool,
    seccomp: &Seccomp,
    then: &Then<'_>,
    userns: bool,
) -> Result<(Outcome, Sandbox), Refusal> {
    let what = if userns {
        "--userns"
    } else {
        "rootless overlay mode"
    };
    let fail = |reason: String| broken(what, &reason);
    let sandbox_dir = state_dir.join(rnk_store::SANDBOX_DIR);
    let (caps, overlay) = {
        let span = tracing::debug_span!("ranikust.probe", steps = 0_u64);
        let _entered = span.enter();
        let caps = rnk_sandbox::probe_with_fallbacks(&sandbox_dir, fallbacks)
            .map_err(|e| fail(format!("the RUN sandbox cannot run here: {e}")))?;
        span.record("steps", 1_u64);
        if caps.proc() == KernelFs::Rbind && !scanned {
            return Err(fail(
                "the RUN sandbox would bind /proc, yet no peer scan ran".to_owned(),
            ));
        }
        let overlay = super::probe_overlay(store.build_dir(), 0).map_err(|r| fail(r.reason))?;
        span.record("steps", 2_u64);
        (caps, overlay)
    };
    let outcome = Outcome::OverlayRootless {
        sandbox: caps,
        overlay,
        userns,
    };
    let mut sandbox = Sandbox::start(caps, &sandbox_dir).map_err(|e| fail(e.to_string()))?;
    if then.userns_network {
        sandbox.allow_userns_network();
    }
    seccomp.apply(&mut sandbox);
    if let Some(env) = then.broken_by {
        return Err(fail(format!("refused by {env}=rootless-broken")));
    }
    // Once nothing is left to fail, so that a fallback logs one mode.
    tracing::info!("ranikust: {outcome}");
    Ok((outcome, sandbox))
}

/// What the fallback's child does before it undoes a failed set-up
/// ([`rnk_sandbox::Supervised::undoing`]).
fn undoing(child: &rnk_sandbox::Supervised) {
    if let Err(e) = child.undoing() {
        tracing::debug!("a stop may cut the store's removal short: {e}");
    }
}

/// Closes the store of a set-up that failed, and empties the state
/// directory where the store holds no data, whoever made it, so that
/// kaniko mode on `/` takes it (ADR-0136); else kaniko mode refuses it,
/// as it would have.
fn abandon(store: rnk_store::Store) {
    let state = store.state_dir().to_path_buf();
    match store.abandon() {
        Ok(true) => tracing::debug!(
            "the store in {}, which holds no data, is removed",
            state.display()
        ),
        Ok(false) => {}
        Err(e) => tracing::warn!("the store in {} is not removed: {e}", state.display()),
    }
}

/// Why the build's user namespace was not entered, for the effective uid
/// `euid`: `NoUserNamespace` gets, with `--userns` (`shifted`),
/// [`USERNS_UNUSABLE`], as the probe's stop does, else the advice where no
/// user namespace is usable, `binary` as for [`plan`]: [`plan`]'s probe
/// passed, so what refuses now is not known.
fn entry_refusal(
    e: rnk_sandbox::Error,
    euid: u32,
    shifted: bool,
    binary: Option<&Path>,
) -> Refusal {
    if shifted {
        let no_map = matches!(e, rnk_sandbox::Error::NoUserNamespace { .. });
        let mut refusal = super::userns_refusal(e, euid, "".into());
        if no_map {
            refusal.guidance = USERNS_UNUSABLE.into();
        }
        return refusal;
    }
    let no_userns = super::no_userns::guidance(binary, Stop::Unknown);
    super::userns_refusal(e, euid, no_userns.into())
}

/// A failure in the build's user namespace, `what` naming the mode: no
/// other mode may follow in this process.
fn broken(what: &str, reason: &str) -> Refusal {
    Refusal {
        kind: Kind::Broken,
        reason: format!("{what}: {reason}"),
        guidance: "".into(),
    }
}

#[cfg(test)]
mod tests {
    use rnk_sandbox::StepFailure;

    use super::*;

    /// A map refused at entry: `--userns` gets its own guidance, a user
    /// other than root the advice where no user namespace is usable, root
    /// nothing more; failures once inside name the mode they stop.
    #[test]
    fn userns_failures_name_userns_and_guide_as_its_probe() {
        let none = || rnk_sandbox::Error::NoUserNamespace { tried: Vec::new() };
        let binary = Some(Path::new("/home/u/bin/ranikust"));
        let r = entry_refusal(none(), 0, true, binary);
        assert_eq!(
            (r.kind, r.guidance.as_ref()),
            (Kind::Other, USERNS_UNUSABLE)
        );
        let r = entry_refusal(none(), 0, false, binary);
        assert_eq!(r.guidance, "");
        let r = entry_refusal(none(), 1000, false, binary);
        assert_eq!(
            r.guidance,
            super::super::no_userns::guidance(binary, Stop::Unknown)
        );
        let r = broken("--userns", "the RUN sandbox cannot run here: x");
        assert_eq!(r.kind, Kind::Broken);
        assert_eq!(r.reason, "--userns: the RUN sandbox cannot run here: x");
        let r = broken("rootless overlay mode", "x");
        assert_eq!(r.reason, "rootless overlay mode: x");
    }

    /// A user other than root whose user namespace has no capabilities
    /// (Ubuntu 24.04 stops it at the mount namespace), or who has none (the
    /// test hook), gets the advice for no user namespace, for what stopped
    /// it, and a step past the namespace the advice to run as root; root,
    /// whose own refusal or kaniko mode says what to do, nothing more; a
    /// refused new `proc` with a working rbind, `--allow proc.rbind`.
    #[test]
    fn each_stop_of_the_probe_gets_its_guidance() {
        let binary = Some(Path::new("/home/u/bin/ranikust"));
        let stops = [
            Stop::InNamespace,
            Stop::Unshare(libc::EPERM),
            Stop::Unshare(libc::EACCES),
            Stop::Unknown,
        ];
        for step in ["unshare", "map", "chown", "mount namespace", "hook"] {
            for stop in stops {
                assert_eq!(
                    guidance(step, stop, false, 1000, false, binary),
                    super::super::no_userns::guidance(binary, stop),
                    "{step}, {stop:?}"
                );
            }
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
            for stop in stops {
                assert_eq!(guidance(step, stop, false, 0, false, binary), "", "{step}");
                // `--userns` has no other mode to say what to do.
                assert_eq!(
                    guidance(step, stop, false, 0, true, binary),
                    USERNS_UNUSABLE,
                    "{step}"
                );
            }
        }
        assert_eq!(
            guidance("proc", Stop::Unknown, true, 1000, false, binary),
            PROC_RBIND
        );
        for userns in [false, true] {
            assert_eq!(
                guidance("proc", Stop::Unknown, true, 0, userns, binary),
                PROC_RBIND
            );
        }
        for step in ["proc", "sysfs", "overlay"] {
            for stop in stops {
                assert_eq!(
                    guidance(step, stop, false, 1000, false, binary),
                    UNUSABLE,
                    "{step}"
                );
            }
        }
        assert!(PROC_RBIND.contains("--allow proc.rbind"));
        assert!(PROC_RBIND.contains("not under ptrace's check"));
    }

    /// The advice weighs the result of the step that stopped the probe
    /// alone, and no step where the capability check found the namespace's
    /// capabilities.
    #[test]
    fn a_stop_is_weighed_on_its_own_step() {
        let failed = |op, errno| {
            StepResult::Failed(StepFailure {
                op,
                errno: Some(errno),
                detail: None,
            })
        };
        let ok = StepResult::Ok;
        let not_run = StepResult::NotRun("the probe's child stopped before it");
        let refused = failed("write uid_map", libc::EPERM);
        let eacces = failed("unshare(CLONE_NEWUSER)", libc::EACCES);
        let all = |r| Steps {
            unshare: &ok,
            map: Some(r),
            chown: Some(r),
            mount_ns: r,
        };
        for step in ["map", "chown", "mount namespace"] {
            assert_eq!(stop_for(step, false, &all(&refused)), Stop::InNamespace);
            assert_eq!(stop_for(step, true, &all(&refused)), Stop::Unknown);
            assert_eq!(stop_for(step, false, &all(&not_run)), Stop::Unknown);
        }
        let map_only = Steps {
            map: Some(&refused),
            ..all(&not_run)
        };
        assert_eq!(stop_for("map", false, &map_only), Stop::InNamespace);
        assert_eq!(stop_for("chown", false, &map_only), Stop::Unknown);
        assert_eq!(stop_for("mount namespace", false, &map_only), Stop::Unknown);
        let unreported = Steps {
            map: None,
            chown: None,
            ..all(&refused)
        };
        assert_eq!(stop_for("map", false, &unreported), Stop::Unknown);
        assert_eq!(stop_for("chown", false, &unreported), Stop::Unknown);
        let at_unshare = Steps {
            unshare: &eacces,
            ..all(&not_run)
        };
        assert_eq!(
            stop_for("unshare", false, &at_unshare),
            Stop::Unshare(libc::EACCES)
        );
        assert_eq!(stop_for("unshare", true, &at_unshare), Stop::Unknown);
        for step in ["proc", "sysfs", "overlay", "hook"] {
            assert_eq!(stop_for(step, false, &all(&refused)), Stop::Unknown);
        }
    }

    /// Only a check whose two steps both worked shows capabilities.
    #[test]
    fn only_both_steps_working_show_capabilities() {
        let caps = |unshare, mount_ns, cut_short: Option<&str>| UsernsCapabilities {
            unshare,
            mount_ns,
            cut_short: cut_short.map(String::from),
        };
        let not_run = || StepResult::NotRun("the probe's child stopped before it");
        assert!(shown_capable(&caps(StepResult::Ok, StepResult::Ok, None)));
        assert!(!shown_capable(&caps(
            StepResult::Ok,
            not_run(),
            Some("killed by signal 9")
        )));
        assert!(!shown_capable(&caps(not_run(), not_run(), Some("killed"))));
    }

    /// The capability check's stop is weighed on its own step's result, as
    /// the probe's is.
    #[test]
    fn the_check_s_stop_is_weighed_on_its_own_step() {
        let failed = |op, errno| {
            StepResult::Failed(StepFailure {
                op,
                errno: Some(errno),
                detail: None,
            })
        };
        let weighed = |found: &UsernsCapabilities| {
            stop_for(found.stop().unwrap(), false, &Steps::checked(found))
        };
        let in_namespace = UsernsCapabilities {
            unshare: StepResult::Ok,
            mount_ns: failed("unshare(CLONE_NEWNS)", libc::EPERM),
            cut_short: None,
        };
        assert_eq!(weighed(&in_namespace), Stop::InNamespace);
        let at_unshare = UsernsCapabilities {
            unshare: failed("unshare(CLONE_NEWUSER)", libc::EACCES),
            mount_ns: StepResult::NotRun("no user namespace"),
            cut_short: None,
        };
        assert_eq!(weighed(&at_unshare), Stop::Unshare(libc::EACCES));
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
        let me = crate::statedir::Owner::me();
        let refusal = state_dir_for_probe(&foreign, 1000, me, "rootless").unwrap_err();
        assert!(
            refusal
                .reason
                .contains("has no regular file .ranikust-state"),
            "{}",
            refusal.reason
        );
        assert_eq!(names(&foreign), ["precious"]);
        let new = tmp.path().join("new");
        let held = state_dir_for_probe(&new, 1000, me, "rootless").unwrap();
        assert!(names(&new).is_empty());
        // Held until the store opens: no prune empties it meanwhile.
        let pruned = rnk_store::Store::prune_all(&new);
        assert!(
            matches!(pruned, Err(rnk_store::Error::InUse { .. })),
            "{pruned:?}"
        );
        drop(held);
    }

    /// A missing state directory whose nearest parent is on overlayfs (a
    /// container's own root) is refused before anything is made; checked
    /// where the temporary directory is on overlayfs.
    #[test]
    fn a_missing_state_directory_on_overlayfs_is_refused_unmade() {
        let tmp = tempfile::tempdir().unwrap();
        if !crate::statedir::on_overlayfs(tmp.path()) {
            eprintln!("skipped: the temporary directory is not on overlayfs");
            return;
        }
        let me = crate::statedir::Owner::me();
        let state = tmp.path().join("a/state");
        let refusal = state_dir_for_probe(&state, 1000, me, "rootless").unwrap_err();
        assert!(
            refusal.reason.contains("is on overlayfs"),
            "{}",
            refusal.reason
        );
        assert!(!tmp.path().join("a").exists());
    }

    /// The store's refusals of the state directory come before its mode
    /// is changed: one another process holds alone, and one a kaniko-mode
    /// build on `/` used, keep the mode they had.
    #[test]
    fn a_state_directory_the_store_refuses_keeps_its_mode() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        if crate::statedir::on_overlayfs(tmp.path()) {
            eprintln!("skipped: the temporary directory is on overlayfs");
            return;
        }
        let set = |p: &Path, mode| {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
        };
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o7777;
        set(tmp.path(), 0o700);
        let me = crate::statedir::Owner::me();
        let open = tmp.path().join("open");
        std::fs::create_dir(&open).unwrap();
        set(&open, 0o755);
        let euid = rustix::process::geteuid().as_raw();
        let alone = crate::setup::tests::LockedElsewhere::new(&open);
        let refusal = state_dir_for_probe(&open, euid, me, "rootless").unwrap_err();
        assert!(
            refusal.reason.contains("another build is using"),
            "{}",
            refusal.reason
        );
        assert_eq!(mode(&open), 0o755);
        drop(alone);
        std::fs::write(open.join(rnk_store::BUILDING_MARKER), "").unwrap();
        let refusal = state_dir_for_probe(&open, euid, me, "rootless").unwrap_err();
        assert!(
            refusal.reason.contains("kaniko-mode build on /"),
            "{}",
            refusal.reason
        );
        assert_eq!(mode(&open), 0o755);
        // Taken, it is made private.
        std::fs::remove_file(open.join(rnk_store::BUILDING_MARKER)).unwrap();
        drop(state_dir_for_probe(&open, euid, me, "rootless").unwrap());
        assert_eq!(mode(&open), 0o700);
    }
}
