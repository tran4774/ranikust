//! The process set-up of a build, while the process is single-threaded:
//! the private mount namespace, the probes in order, the outcome, then the
//! spawner thread (`design/sandbox.md#process-set-up-once-per-build`,
//! `design/modes-and-detection.md#probe-order---modeauto`).
//!
//! The outcomes: overlay mode as root, overlay mode in the build's user
//! namespace ([`rootless`], ADR-0104, ADR-0107), and kaniko mode, as root,
//! on a container's `/` (ADR-0050) or with a directory per stage
//! (ADR-0059). `--mode=auto` enters the namespace first
//! ([`enter_namespace`]): where root may not mount, the binary tries
//! rootless overlay mode, then picks kaniko mode (ADR-0040); where overlayfs
//! refuses the state directory outside a container, kaniko mode with a
//! directory per stage; a user other than root builds in rootless overlay
//! mode; every other environment is refused with the probe's diagnosis.

use std::borrow::Cow;
use std::fmt;
use std::os::fd::OwnedFd;
use std::os::unix::fs::DirBuilderExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rnk_rootfs::RootPath;
use rustix::fs::FlockOperation;
use rustix::io::Errno;

use rnk_sandbox::{Caps, Error as SandboxError, Sandbox};
use rnk_snapshot::{Error as SnapshotError, MountApi, OverlayCaps};
use rnk_store::{BuildPaths, Store};

use crate::config::Seccomp;

pub(crate) mod rootless;

/// The outcome the probes picked (ADR-0002, ADR-0004). Each library gets
/// only its own part (Q-017, interim (b)).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Overlay mode as root: stage roots are overlay mounts, `RUN` runs in
    /// the full sandbox.
    OverlayRoot {
        /// For `rnk_sandbox::Sandbox::start`.
        sandbox: Caps,
        /// For `rnk-snapshot` (task 9).
        overlay: OverlayCaps,
    },
    /// Overlay mode in the build's user namespace, which the whole build
    /// runs in as its root (ADR-0104): stage roots are overlay mounts that
    /// namespace owns, `RUN` runs in the full sandbox there, with `proc`
    /// and `sysfs` as [`rnk_sandbox::probe_with_fallbacks`] chose them.
    OverlayRootless {
        /// For `rnk_sandbox::Sandbox::start`.
        sandbox: Caps,
        /// For `rnk-snapshot`.
        overlay: OverlayCaps,
        /// Root's `--userns`, in the `ranikust` range's namespace (ADR-0003,
        /// ADR-0139), rather than a user's rootless build.
        userns: bool,
    },
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (sandbox, overlay) = match self {
            Outcome::OverlayRoot { sandbox, overlay } => {
                f.write_str("overlay mode as root")?;
                (sandbox, overlay)
            }
            Outcome::OverlayRootless {
                sandbox,
                overlay,
                userns,
            } => {
                f.write_str(if *userns {
                    "overlay mode with --userns, in the build's user namespace of the ranikust \
                     range"
                } else {
                    "overlay mode, rootless, in the build's user namespace"
                })?;
                (sandbox, overlay)
            }
        };
        f.write_str(", RUN enters its root with ")?;
        f.write_str(match sandbox.root_entry {
            rnk_sandbox::RootEntry::PivotRoot => "pivot_root",
            rnk_sandbox::RootEntry::MoveMount => "MS_MOVE",
        })?;
        if let Outcome::OverlayRootless { .. } = self {
            write!(f, ", proc: {}, sysfs: {}", sandbox.proc(), sandbox.sysfs())?;
            if sandbox.sysfs() == rnk_sandbox::KernelFs::Rbind {
                // What the spawner does for such a RUN (`rnk_sandbox`).
                f.write_str(
                    " (for a RUN with --network=none, a new instance where it mounts, else the \
                     rbind with no network device)",
                )?;
            }
        }
        write!(f, ", overlay mounts with {}", mount_api_name(overlay.api))
    }
}

/// Why this environment cannot build, and what to do instead
/// (`design/modes-and-detection.md#refusal-and-guidance`).
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Refusal {
    /// What failed, with the step and errno when a probe found it.
    pub(crate) reason: String,
    /// What to do instead; empty when the reason already says it.
    /// ranikust's own text, though it may name the state directory.
    pub(crate) guidance: Cow<'static, str>,
    /// What kind of refusal it is: what `--mode=auto` may make of it
    /// ([`route`]).
    pub(crate) kind: Kind,
}

/// What kind of refusal a [`Refusal`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Anything else: a host-side failure, a missing tool, a lock.
    Other,
    /// Root lacks the privilege to mount: kaniko mode's place.
    NoMountPrivilege,
    /// Root may mount, but overlayfs refuses the state directory: the
    /// probe's mount was refused by the kernel, or the state directory is
    /// on overlayfs.
    OverlayUnusable,
    /// A step of the `RUN` sandbox was refused although root may mount (an
    /// LSM blocking mounts).
    SandboxBlocked,
    /// The process may be in the build's user namespace (it is not known to
    /// be unchanged), and its set-up or a later step failed: it must end,
    /// so no mode may follow in this process (ADR-0136: in a child, kaniko
    /// mode may in the parent).
    Broken,
}

/// What `--mode=auto` does after overlay mode's set-up refused, once root
/// could enter the build's mount namespace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Route {
    /// Build in kaniko mode with a directory per stage and the full
    /// sandbox (ADR-0059).
    KanikoStages,
    /// The refusal stands.
    Refuse,
    /// The refusal stands, with what makes overlay mode work: kaniko mode
    /// on a container's `/` where root may mount is refused (ADR-0054).
    RefuseInContainer,
}

/// Where `--mode=auto` goes after overlay mode's set-up refused with
/// `refusal`, root having entered the build's mount namespace. Only an
/// unusable overlayfs routes to kaniko mode, and only outside a container:
/// a blocked sandbox step would block kaniko mode off `/` too, and a
/// host-side failure is no reason to change modes.
pub(crate) fn route(refusal: &Refusal, in_container: bool) -> Route {
    match (refusal.kind, in_container) {
        (Kind::OverlayUnusable, false) => Route::KanikoStages,
        (Kind::OverlayUnusable, true) => Route::RefuseInContainer,
        (Kind::Other | Kind::NoMountPrivilege | Kind::SandboxBlocked | Kind::Broken, _) => {
            Route::Refuse
        }
    }
}

impl Refusal {
    /// Whether this environment needs kaniko mode: root that may not mount
    /// or cannot use overlay, what `--mode=auto` turns into kaniko mode.
    pub(crate) fn needs_kaniko_mode(&self) -> bool {
        self.kind == Kind::NoMountPrivilege
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.reason)?;
        if !self.guidance.is_empty() {
            write!(f, "\n  {}", self.guidance)?;
        }
        Ok(())
    }
}

/// Guidance where a user other than root reaches a step of overlay mode as
/// root: that user builds in rootless overlay mode instead.
const NEEDS_ROOT: &str = "overlay mode as root needs root: a user other than root builds in \
     rootless overlay mode (--mode=auto or --mode=overlay), or run ranikust with sudo, or as \
     root in a container";

/// Guidance where root may not mount: the outcome that would run here is
/// kaniko mode. Shown only with a forced `--mode=overlay`: `--mode=auto`
/// turns this refusal into kaniko mode.
const NEEDS_KANIKO_MODE: &str = "this environment needs kaniko mode: drop --mode=overlay (the \
     default --mode=auto picks kaniko mode here) or use --mode=kaniko; a container started \
     with --privileged can build in overlay mode";

/// Guidance where root may mount but overlayfs refuses the state directory.
const OVERLAY_UNUSABLE: &str = "mount another filesystem (a volume) at the state directory; \
     outside a container, the default --mode=auto (or --mode=kaniko) builds in kaniko mode with \
     a directory per stage instead";

/// Guidance where root may mount yet the `RUN` sandbox lacks a privilege
/// for one of its steps: neither overlay mode nor kaniko mode off `/` can run.
const SANDBOX_NO_PRIVILEGE: &str = "the RUN sandbox lacks a privilege here although root may \
     mount: run ranikust in a VM, or in a container started with --privileged for overlay \
     mode, or without CAP_SYS_ADMIN for kaniko mode on its /";

/// Guidance where a step of the `RUN` sandbox is refused although root may
/// mount (an LSM such as AppArmor's `docker-default` blocking mounts).
const SANDBOX_BLOCKED: &str = "the RUN sandbox cannot mount here: start the container with \
     --privileged for overlay mode, or without CAP_SYS_ADMIN for kaniko mode on its /";

/// A build environment, set up: what the build hands to the libraries.
pub(crate) struct Setup {
    /// The picked outcome.
    pub(crate) outcome: Outcome,
    /// The state directory the probes used.
    pub(crate) state_dir: PathBuf,
    /// The store in the state directory, open for this build; shared with
    /// `rnk-snapshot` (`rnk_snapshot::Overlay::new`) and the registry
    /// client. [`Store::close`] it at the end.
    pub(crate) store: Arc<Store>,
    /// The spawner thread every `RUN` is cloned from.
    pub(crate) sandbox: Sandbox,
}

/// Enters the build's private mount namespace (`rnk_sandbox`), the first
/// step of every set-up and the one that tells whether root may mount. Call
/// while single-threaded.
///
/// # Errors
///
/// A [`Refusal`]; [`Refusal::needs_kaniko_mode`] where root lacks the
/// privilege to mount.
pub(crate) fn enter_namespace() -> Result<(), Refusal> {
    let euid = rustix::process::geteuid().as_raw();
    rnk_sandbox::enter_build_namespace().map_err(|e| namespace_refusal(e, euid))
}

/// Guidance where no user namespace with a map can be made for a user
/// other than root.
const NO_USER_NAMESPACE: &str = "a build by a user other than root needs a user namespace of \
     its own with an id map: where kernel.apparmor_restrict_unprivileged_userns is 1 (Ubuntu \
     23.10 and later), only a program an AppArmor profile allows gets one; otherwise run \
     ranikust as root";

/// The build's user namespace, entered, and the store opened in it.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the tests of the build's user namespace read it")
)]
pub(crate) struct UsernsSetup {
    /// Its map, and the maps passed over on the way.
    pub(crate) userns: rnk_sandbox::BuildUserNamespace,
    /// The store in the state directory, opened as root of the namespace.
    pub(crate) store: Store,
}

/// Enters the build's user namespace and opens the store in it (M2): the
/// state directory's check first, outside, where its owners read as the
/// host's ([`crate::statedir::prepare`]); then [`enter_build_userns`]; then
/// [`Store::open`], whose start-up clean-up removes a dead build's files as
/// root of the namespace, those of subordinate ids too, which the user
/// outside could not. Call while single-threaded, in place of
/// [`enter_namespace`].
///
/// # Errors
///
/// A [`Refusal`] from the state directory's check, or as
/// [`enter_build_userns`]; a store refused is of [`Kind::Broken`].
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the tests of the build's user namespace call it; rootless overlay mode \
                  enters through `rootless::set_up`"
    )
)]
pub(crate) fn enter_user_namespace(state_dir: &Path) -> Result<UsernsSetup, Refusal> {
    prepare_state_dir(state_dir)?;
    // The user's, for the guidance: root of the namespace once in it.
    let euid = rustix::process::geteuid().as_raw();
    let userns = enter_build_userns(NO_USER_NAMESPACE.into())?;
    let store = open_store(state_dir, state_dir, euid).map_err(inside_userns)?;
    Ok(UsernsSetup { userns, store })
}

/// Enters the build's user namespace, checking and opening nothing of the
/// state directory, which the caller checked outside first: for a user
/// other than root, its subordinate ids ([`crate::subids`]); then
/// [`rnk_sandbox::enter_build_user_namespace`], which also enters the
/// build's mount namespace. Logs the map, one uid as such (ADR-0098). Call
/// while single-threaded.
///
/// # Errors
///
/// A [`Refusal`], with `no_userns` as its guidance where a user other than
/// root gets no user namespace. Where the process may be in the namespace
/// already (an error of [`rnk_sandbox::enter_build_user_namespace`] not
/// known to leave the process unchanged), it is of [`Kind::Broken`]: the
/// process must end, and no caller, in any mode, may ignore it. A caller's
/// own failure once in the namespace is made one with [`inside_userns`].
pub(crate) fn enter_build_userns(
    no_userns: Cow<'static, str>,
) -> Result<rnk_sandbox::BuildUserNamespace, Refusal> {
    let euid = rustix::process::geteuid().as_raw();
    let subordinate = crate::subids::lookup(euid).map_err(|e| Refusal {
        kind: Kind::Other,
        guidance: "".into(),
        reason: format!("cannot read this user's subordinate ids: {e}"),
    })?;
    let userns = rnk_sandbox::enter_build_user_namespace(rnk_sandbox::BuildIds::Own(subordinate))
        .map_err(|e| userns_refusal(e, euid, no_userns))?;
    tracing::info!("ranikust: the build's user namespace: {userns}");
    Ok(userns)
}

/// `refusal`, made once the process is in the build's user namespace: no
/// other mode may follow it.
pub(crate) fn inside_userns(refusal: Refusal) -> Refusal {
    Refusal {
        kind: Kind::Broken,
        ..refusal
    }
}

/// The refusal for [`rnk_sandbox::enter_build_user_namespace`]'s error
/// `e`, for the effective uid `euid`: [`Kind::Broken`] unless `e` is one
/// that function documents as leaving the process unchanged; `no_userns`
/// guides a user other than root who gets no user namespace.
fn userns_refusal(e: SandboxError, euid: u32, no_userns: Cow<'static, str>) -> Refusal {
    Refusal {
        kind: match e {
            SandboxError::NotSingleThreaded { .. }
            | SandboxError::InPlainChroot
            | SandboxError::NoUserNamespace { .. }
            | SandboxError::Sys { .. } => Kind::Other,
            _ => Kind::Broken,
        },
        guidance: if euid != 0 && matches!(e, SandboxError::NoUserNamespace { .. }) {
            no_userns
        } else {
            "".into()
        },
        reason: e.to_string(),
    }
}

/// Sets overlay mode up for a build whose state directory is `state_dir`,
/// once [`enter_namespace`] succeeded: the state directory's check
/// ([`crate::statedir::prepare`]; refused on overlayfs, a container's own
/// root, ADR-0050), the store (marker check, start-up clean-up), the probes of
/// `rnk-sandbox` and `rnk-snapshot`, the outcome, then the spawner thread.
/// Before the spawner, the only other thread is the overlay probe's
/// `mount(2)` attempt, which it joins: the process is single-threaded
/// again when the spawner starts. `userns_network` is `--allow
/// network.none-userns` (ADR-0033); `seccomp` the profile of every `RUN`
/// (ADR-0109); `userns_probe` (`--mode=auto`) adds the user namespace
/// probe, reported only, its verdict weighing `proc_rbind` (`--allow
/// proc.rbind`), and is cleared once the set-up reaches the probe,
/// whatever follows; `refuse_overlay`, the test hook's `overlay-refused`,
/// takes the overlay probe, once it passed, as a failed probe check, which
/// routes as a mount the kernel refused does.
///
/// # Errors
///
/// A [`Refusal`] naming what failed and what to do instead.
pub(crate) fn set_up(
    state_dir: &Path,
    userns_network: bool,
    seccomp: &Seccomp,
    userns_probe: &mut bool,
    proc_rbind: bool,
    refuse_overlay: bool,
) -> Result<Setup, Refusal> {
    let euid = rustix::process::geteuid().as_raw();
    prepare_state_dir(state_dir)?;
    if crate::statedir::on_overlayfs(state_dir) {
        return Err(Refusal {
            kind: Kind::OverlayUnusable,
            reason: format!(
                "the state directory {} is on overlayfs, a container's own root, where overlay \
                 mode cannot keep its upper directories",
                state_dir.display()
            ),
            guidance: ON_OVERLAYFS.into(),
        });
    }
    // The store first, then the probes, which write in the state directory
    // only once its marker is checked (Q-111, (a)).
    let store = open_store(state_dir, state_dir, euid)?;
    // Before the overlay probe: a thread it joined may still be counted.
    if std::mem::take(userns_probe) {
        report_user_namespace(&store, allowed(proc_rbind));
    }
    let sandbox_dir = state_dir.join(rnk_store::SANDBOX_DIR);
    let (caps, overlay) = {
        let span = tracing::debug_span!("ranikust.probe", steps = 0_u64);
        let _entered = span.enter();
        let sandbox = rnk_sandbox::probe(&sandbox_dir);
        span.record("steps", 1_u64);
        let overlay = probe_overlay(store.build_dir(), euid).and_then(|caps| {
            if refuse_overlay {
                Err(overlay_refusal(&FAULT_OVERLAY_REFUSED, euid))
            } else {
                Ok(caps)
            }
        });
        span.record("steps", 2_u64);
        pick(sandbox, overlay, euid)?
    };
    let outcome = Outcome::OverlayRoot {
        sandbox: caps,
        overlay,
    };
    tracing::info!("ranikust: {outcome}");
    let mut sandbox = Sandbox::start(caps, &sandbox_dir).map_err(|e| Refusal {
        kind: Kind::Other,
        reason: e.to_string(),
        guidance: "".into(),
    })?;
    if userns_network {
        sandbox.allow_userns_network();
    }
    seccomp.apply(&mut sandbox);
    Ok(Setup {
        outcome,
        state_dir: state_dir.to_path_buf(),
        store: Arc::new(store),
        sandbox,
    })
}

/// Guidance where overlay mode's state directory is on overlayfs.
const ON_OVERLAYFS: &str = "mount a volume at the state directory, e.g. \
     `-v ranikust-state:/var/lib/ranikust`, or pass --state-dir on one; outside a container \
     the default --mode=auto builds in kaniko mode there instead";

/// Opens the store in `state_dir` for a build whose root is `root`, by the
/// effective uid `euid` outside any user namespace of the build.
fn open_store(root: &Path, state_dir: &Path, euid: u32) -> Result<Store, Refusal> {
    Store::open(&BuildPaths::new(root, state_dir)).map_err(|e| store_refusal(&e, state_dir, euid))
}

/// A store that cannot be opened in `state_dir` by the effective uid
/// `euid` (outside any user namespace of the build), as a [`Refusal`]. An
/// error that names the state directory itself says so alone; a directory
/// a `prune --all` did not finish emptying gets what `euid` does with it
/// ([`crate::prune::cut_short`]).
fn store_refusal(e: &rnk_store::Error, state_dir: &Path, euid: u32) -> Refusal {
    use rnk_store::Error;
    let reason = match e {
        Error::Io { path, .. } if path == state_dir => e.to_string(),
        Error::NotAStateDir { .. }
        | Error::NoFlock { .. }
        | Error::Busy { .. }
        | Error::InUse { .. }
        | Error::NotPruned { .. }
        | Error::NotAbandoned { .. }
        | Error::PruneCutShort { .. }
        | Error::StateNotEmpty { .. }
        | Error::NotEmptied { .. }
        | Error::Unmarked { .. }
        | Error::UsedOnSlash { .. }
        | Error::AnonymousFile { .. }
        | Error::NameOutlivesUnlink { .. } => e.to_string(),
        Error::Io { .. }
        | Error::BadEntry { .. }
        | Error::Cancelled
        | Error::Anonymous { .. }
        | Error::Random(_)
        | Error::Rootfs(_)
        | Error::Fill { .. } => format!("cannot open the store in {}: {e}", state_dir.display()),
    };
    Refusal {
        kind: Kind::Other,
        reason,
        guidance: match crate::prune::unfinished_prune(e) {
            Some((crate::prune::Unfinished::CutShort, dir)) => {
                crate::prune::cut_short(dir, euid).into()
            }
            Some((crate::prune::Unfinished::NotPruned, _)) | None => "".into(),
        },
    }
}

/// What kaniko mode's set-up hands to the build.
pub(crate) struct KanikoSetup {
    /// The store in the state directory, open for this build.
    pub(crate) store: Arc<Store>,
    /// The spawner thread every `RUN` is cloned from.
    pub(crate) sandbox: Sandbox,
    /// How `RUN` is isolated: the full sandbox on a root in the state
    /// directory, `Direct` on `/` in a container (ADR-0043, ADR-0050).
    pub(crate) isolation: rnk_sandbox::Isolation,
    /// On `/`: the shared root, opened once, with its path. `None` off
    /// `/`, where each stage gets a directory of its own in the state
    /// directory (ADR-0059).
    pub(crate) root: Option<(RootPath, PathBuf)>,
    /// The canonical state directory.
    pub(crate) state_dir: PathBuf,
    /// On `/`: the `flock` on `/` that keeps a second build off it, held
    /// to the end.
    pub(crate) root_lock: Option<OwnedFd>,
}

/// Guidance where kaniko mode runs without root.
const KANIKO_NEEDS_ROOT: &str = "kaniko mode needs root (chown, chroot, switching users), as \
     kaniko does: run ranikust as root in the container or VM that builds";

/// Sets kaniko mode up (ADR-0050): the state directory's check, the store,
/// the build's root, and the spawner. With `on_slash` (ranikust runs in a
/// container) the root is `/`, locked for this build by `flock` on `/`
/// itself, and `RUN` runs `Direct`, as kaniko. Otherwise every stage gets
/// a directory of its own, `<state>/builds/<id>/stages/<n>/rootfs`, made by
/// the snapshotter and removed with the build (ADR-0059), and `RUN` gets
/// the full sandbox, probed here; without mount privileges (`can_mount`
/// false) that case is refused (Q-330, ADR-0050), never a bare `chroot` on
/// the host. `nofile` is the `RLIMIT_NOFILE` ranikust
/// started with, given back to every `RUN`; `seccomp` the profile of a
/// full-sandbox `RUN` (ADR-0109), which the caller refused on `/`;
/// `userns_probe` (`--mode=auto`) adds the user namespace probe, reported
/// only, its verdict weighing `proc_rbind` (`--allow proc.rbind`), with
/// ADR-0107's hint on `/` where only the state directory on overlayfs
/// keeps rootless overlay mode away.
///
/// # Errors
///
/// A [`Refusal`]: not root, an unsafe state directory, the store, the
/// lock, the root, a probe.
pub(crate) fn set_up_kaniko(
    state_dir: &Path,
    on_slash: bool,
    can_mount: bool,
    nofile: rnk_sandbox::NofileLimit,
    seccomp: &Seccomp,
    userns_probe: bool,
    proc_rbind: bool,
) -> Result<KanikoSetup, Refusal> {
    let euid = rustix::process::geteuid().as_raw();
    if euid != 0 {
        return Err(Refusal {
            kind: Kind::Other,
            reason: format!("kaniko mode cannot run as uid {euid}"),
            guidance: KANIKO_NEEDS_ROOT.into(),
        });
    }
    if let Some(refusal) = host_without_mounts(on_slash, can_mount) {
        return Err(refusal);
    }
    // On `/`, before anything else and while no other thread exists,
    // ranikust keeps only what a `RUN` keeps: whatever it later opens on
    // `/` after a `RUN`, the `RUN` could open itself (ADR-0078). The
    // anonymous store relies on it too: it deletes names a `RUN` can reach
    // (`Store::open_anonymous`), so the trim must stay before it.
    if on_slash {
        rnk_sandbox::trim_to_run_capabilities().map_err(|e| Refusal {
            kind: Kind::Other,
            reason: format!("kaniko mode on /: {e}"),
            guidance: "".into(),
        })?;
    }
    // On `/`, the lock first, on `/` itself: two builds in one container
    // never share it, whatever their state directories.
    let root_lock = if on_slash { Some(lock_slash()?) } else { None };
    prepare_state_dir(state_dir)?;
    let slash = Path::new("/");
    // On `/` a `RUN` reads any file with a path: the store keeps none, its
    // blobs are anonymous files (ADR-0051). A `RUN` also writes the state
    // directory there, so it is never a persistent store, and nothing a
    // `RUN` left outlives the build (ADR-0077).
    let store = if on_slash {
        Store::open_anonymous(state_dir).map_err(|e| store_refusal(&e, state_dir, euid))?
    } else {
        open_store(state_dir, state_dir, euid)?
    };
    let root = if on_slash {
        let root = RootPath::open(slash).map_err(|e| failed("opening /", &e))?;
        Some((root, slash.to_path_buf()))
    } else {
        None
    };
    let sandbox_dir = store.state_dir().join(rnk_store::SANDBOX_DIR);
    // Off `/` root may mount here (`host_without_mounts`): the full sandbox.
    let caps = if on_slash {
        None
    } else {
        let span = tracing::debug_span!("ranikust.probe", steps = 0_u64);
        let _entered = span.enter();
        let caps = rnk_sandbox::probe(&sandbox_dir).map_err(|e| Refusal {
            kind: Kind::Other,
            reason: format!("the RUN sandbox cannot run here: {e}"),
            guidance: "".into(),
        })?;
        span.record("steps", 1_u64);
        Some(caps)
    };
    if userns_probe
        && let Some(probed) = report_user_namespace(&store, allowed(proc_rbind))
        && on_slash
        && crate::statedir::on_overlayfs(store.state_dir())
        && let Some(hint) = volume_hint(&probed, proc_rbind)
    {
        tracing::info!("ranikust: {hint}");
    }
    let isolation = if caps.is_some() {
        rnk_sandbox::Isolation::Full
    } else {
        rnk_sandbox::Isolation::Direct
    };
    let started = match caps {
        // Stage directories in the state directory, as overlay mode's
        // mount points: the full sandbox enters each (ADR-0059).
        Some(caps) => Sandbox::start(caps, &sandbox_dir),
        // On `/` (no caps): `Direct` places nothing, no journal, no
        // directory; the process becomes the subreaper of what a `RUN`
        // leaves (ADR-0051).
        None => Sandbox::start_direct(),
    };
    let mut sandbox = started.map_err(|e| Refusal {
        kind: Kind::Other,
        reason: e.to_string(),
        guidance: "".into(),
    })?;
    sandbox.set_nofile_limit(nofile);
    // A `Direct` `RUN` never gets the profile, whatever it is (ADR-0103).
    seccomp.apply(&mut sandbox);
    match &root {
        Some((_, path)) => tracing::info!(
            "ranikust: kaniko mode on {}, RUN directly, as kaniko",
            path.display()
        ),
        None => tracing::info!(
            "ranikust: kaniko mode with a directory per stage in {}, RUN in the full sandbox",
            store.build_dir().join("stages").display()
        ),
    }
    Ok(KanikoSetup {
        state_dir: store.state_dir().to_path_buf(),
        store: Arc::new(store),
        sandbox,
        isolation,
        root,
        root_lock,
    })
}

/// Guidance where kaniko mode would run on a host whose root may not mount.
const HOST_WITHOUT_MOUNTS: &str = "run ranikust in a container (kaniko mode then builds on its \
     /), or give it the privilege to mount (root on the host, a --privileged container) so \
     RUN gets the full sandbox";

/// Kaniko mode outside a container where root may not mount: `RUN` would
/// run in a bare `chroot` on the host, which root can leave
/// (`CAP_SYS_CHROOT`) and where it may make device nodes (`CAP_MKNOD`).
/// Refused (Q-330, ADR-0050).
pub(crate) fn host_without_mounts(on_slash: bool, can_mount: bool) -> Option<Refusal> {
    (!on_slash && !can_mount).then(|| Refusal {
        kind: Kind::Other,
        reason: "kaniko mode outside a container, where root may not mount: RUN would run in \
                 a bare chroot on this machine, which root can leave"
            .to_owned(),
        guidance: HOST_WITHOUT_MOUNTS.into(),
    })
}

/// Locks `/` itself (`flock` on a descriptor of it) for a kaniko-mode
/// build on it: one per container, whatever the state directory.
fn lock_slash() -> Result<OwnedFd, Refusal> {
    let fd = rustix::fs::open(
        "/",
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(|e| failed("opening / to lock it", &e))?;
    match rustix::fs::flock(&fd, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(fd),
        Err(rustix::io::Errno::WOULDBLOCK) => Err(Refusal {
            kind: Kind::Other,
            reason: "another ranikust build is using / in this container".to_owned(),
            guidance: "run one kaniko-mode build at a time per container".into(),
        }),
        Err(e) => Err(failed("cannot lock the root /", &e)),
    }
}

/// Step 2 of the probe order, reported only where it routes nothing (root's
/// overlay and kaniko modes): whether a `RUN` could run in a user namespace
/// here (`rnk_sandbox::probe_user_namespace`), and an overlay mount in it
/// (`rnk_snapshot::probe`), tried in a directory of this build on the state
/// directory's filesystem (where it mounts is \[open: #4], interim (a)).
/// Logs one line, whose verdict counts an rbind of `/proc` only where
/// `allowed` has it; nothing here refuses or changes the outcome. Root
/// only: the effective uid is checked. Call while single-threaded, before
/// the spawner starts and before `probe_overlay`, whose joined thread may
/// still be counted. What it found, if it ran.
fn report_user_namespace(store: &Store, allowed: rnk_sandbox::Fallbacks) -> Option<Probed> {
    let span = tracing::debug_span!("ranikust.probe_userns", passes = tracing::field::Empty);
    let _entered = span.enter();
    if !rustix::process::geteuid().is_root() {
        tracing::debug!("user namespace probe not run: not root");
        return None;
    }
    match probe_user_namespace(store) {
        Ok(probed) => {
            let (passes, line) = probed.summary(allowed);
            span.record("passes", passes);
            tracing::info!("ranikust: user namespace probe (reported only): {line}");
            Some(probed)
        }
        Err(e) => {
            tracing::warn!("ranikust: user namespace probe (reported only) could not run: {e}");
            None
        }
    }
}

/// The operator's request as [`rnk_sandbox::Fallbacks`]: an rbind of
/// `/proc` with `proc_rbind` (`--allow proc.rbind`, ADR-0126).
pub(crate) fn allowed(proc_rbind: bool) -> rnk_sandbox::Fallbacks {
    if proc_rbind {
        rnk_sandbox::Fallbacks::default().allow_proc_rbind()
    } else {
        rnk_sandbox::Fallbacks::default()
    }
}

/// What the user namespace probe and the overlay probe in it found.
pub(crate) struct Probed {
    /// The steps.
    pub(crate) report: rnk_sandbox::UsernsReport,
    /// The overlay probe in the namespace.
    overlay: OverlayInNamespace,
    /// `kernel.apparmor_restrict_unprivileged_userns`, for the line.
    apparmor: String,
}

impl Probed {
    /// Whether the path passes for a build that `allowed` lets bind, and
    /// the line to log.
    pub(crate) fn summary(&self, allowed: rnk_sandbox::Fallbacks) -> (bool, String) {
        userns_summary(
            self.report.stop(allowed),
            &self.report.to_string(),
            self.overlay.clone(),
            &self.apparmor,
        )
    }

    /// The overlay probe in the namespace's error, `None` where it passed.
    pub(crate) fn overlay_error(&self) -> Option<String> {
        match &self.overlay {
            Some(Ok(Ok(_))) => None,
            Some(Ok(Err(e))) => Some(format!("refused: {e}")),
            Some(Err(e)) => Some(format!("could not run: {e}")),
            None => Some("not run (no id map)".to_owned()),
        }
    }
}

/// ADR-0107's hint where kaniko mode on `/` builds because the state
/// directory is on overlayfs: what lets rootless overlay mode run here,
/// once the steps pass (an rbind of `/proc` only with `proc_rbind`, or
/// with the flag the hint then names).
fn volume_hint(probed: &Probed, proc_rbind: bool) -> Option<&'static str> {
    let report = &probed.report;
    if report.stop(allowed(proc_rbind)).is_none() {
        Some(
            "rootless overlay mode would build here with a volume at the state directory \
             (e.g. -v ranikust-state:/var/lib/ranikust): the state directory is on overlayfs, \
             where it cannot keep its upper directories (ADR-0107)",
        )
    } else if report.stop(allowed(true)).is_none() {
        Some(
            "rootless overlay mode would build here with a volume at the state directory \
             (e.g. -v ranikust-state:/var/lib/ranikust) and --allow proc.rbind, through which \
             every RUN reads the command lines of this PID namespace's processes, ranikust's \
             included (see --help) (ADR-0107, ADR-0126)",
        )
    } else {
        None
    }
}

/// [`report_user_namespace`]'s probes, in a directory of the store made for
/// them ([`probe_in_dir`]).
fn probe_user_namespace(store: &Store) -> Result<Probed, String> {
    // An anonymous store has no build directory: a name its next start
    // removes if this build leaves it, made while no `RUN` runs.
    let (_window, parent, name) = if store.is_anonymous() {
        let window = store.named_window().map_err(|e| e.to_string())?;
        let name = store.scratch_name().map_err(|e| e.to_string())?;
        (Some(window), store.state_dir(), name)
    } else {
        (None, store.build_dir(), "userns-probe".to_owned())
    };
    probe_in_dir(parent, &name, rnk_sandbox::BuildIds::Own(None))
}

/// The probes, for the ids `ids` maps, in `parent/name` (or the name
/// [`make_probe_dir_again`] takes instead), made for them and removed
/// after, unless a child of the probe was left there. With a
/// shifted range (`--userns`), the namespace's root owns what its children
/// write in: the directory and the overlay probe's; this process, which
/// gave it away, deletes nothing in it, and leaves it, its lock released,
/// to the store, which removes it from inside the build's user namespace
/// with the range's rights alone (its name must then be one the store
/// removes). A directory with the name a store removes
/// ([`rnk_store::USERNS_PROBE_PREFIX`]) is held by its exclusive `flock`
/// while the probes use it, and for this process's life when a child was
/// left: the store removes one only where it takes that lock.
pub(crate) fn probe_in_dir(
    parent: &Path,
    name: &str,
    ids: rnk_sandbox::BuildIds,
) -> Result<Probed, String> {
    let given = ids.shifted_root().is_some();
    if given && !name.starts_with(rnk_store::USERNS_PROBE_PREFIX) {
        return Err(format!(
            "{name} is not a name the store removes, for a directory given away"
        ));
    }
    let root = RootPath::open(parent).map_err(|e| e.to_string())?;
    let (name, scratch, held) = make_probe_dir_again(&root, parent, name)?;
    let name = name.as_str();
    let (r, left) = match own_group(&scratch) {
        Ok(_) => probe_in_scratch(&scratch, &parent.join(name), ids),
        Err(e) => (Err(e.to_string()), false),
    };
    #[cfg(test)]
    tests::probes_ran();
    if left {
        // The child may still write there; the error in the line says so.
        // Its lock stays held as long as this process, and that child, live.
        std::mem::forget(held);
        return r;
    }
    if given {
        // The range's processes may move anything of theirs in meanwhile.
        drop(held);
        return r;
    }
    let removed = rnk_rootfs::delete_contents(&scratch, &rnk_rootfs::IgnoreList::new())
        .and_then(|()| root.remove_dir(name));
    if let Err(e) = removed {
        tracing::warn!(
            "ranikust: cannot remove the user namespace probe's {}: {e}",
            parent.join(name).display()
        );
    }
    r
}

/// [`make_probe_dir`] with `name`, then once more with a name of its own
/// where another build took the first for a leftover before it was held
/// (removed it, [`remove_own_probe_leftovers`], or locked it to remove
/// it, as a store does): the name made.
fn make_probe_dir_again(
    root: &RootPath,
    parent: &Path,
    name: &str,
) -> Result<(String, RootPath, Option<OwnedFd>), String> {
    let again = format!("{name}-again");
    for name in [name, &again] {
        if let Some((scratch, held)) = make_probe_dir(root, parent, name)? {
            return Ok((name.to_owned(), scratch, held));
        }
    }
    Err(format!(
        "{} was taken by another build (removed, or locked to be removed) before it could \
         be held",
        parent.join(again).display()
    ))
}

/// Makes the directory `name` in `root` (at `parent`) for the probe, and
/// holds it where the store would take it for a leftover: one named
/// [`rnk_store::USERNS_PROBE_PREFIX`]. Others (in a build's directory) are
/// never held. `None` where another build took it before it was held
/// (removed it, or locked it to remove it).
fn make_probe_dir(
    root: &RootPath,
    parent: &Path,
    name: &str,
) -> Result<Option<(RootPath, Option<OwnedFd>)>, String> {
    let scratch = root
        .create_sub_root(name, 0o700)
        .map_err(|e| e.to_string())?;
    if !name.starts_with(rnk_store::USERNS_PROBE_PREFIX) {
        return Ok(Some((scratch, None)));
    }
    #[cfg(test)]
    tests::before_hold(&parent.join(name));
    match hold_dir(&scratch) {
        Ok(Some(held)) => Ok(Some((scratch, Some(held)))),
        Ok(None) => Ok(None),
        Err(e) => {
            // This error is the one to report; an empty directory left is a
            // leftover the next start removes.
            let _ = root.remove_dir(name);
            Err(format!("cannot hold {}: {e}", parent.join(name).display()))
        }
    }
}

/// Removes each directory of the user namespace probe in `parent`
/// ([`rnk_store::USERNS_PROBE_PREFIX`]) that this process's user made and
/// had not yet given away when its build was killed: the store, in the
/// build's user namespace under `--userns`, cannot open it. Only an empty
/// directory of this user's, mode `0700`, on `parent`'s mount and no
/// mount point, that no probe holds (its `flock`), told through its
/// descriptor; then its name alone is removed, which fails where it is no
/// longer empty. Nothing inside a directory is ever deleted. A failure is
/// logged: it must not stop the build.
pub(crate) fn remove_own_probe_leftovers(parent: &Path) {
    let root = match RootPath::open(parent) {
        Ok(root) => root,
        Err(e) => {
            tracing::warn!(
                "ranikust: cannot look for the user namespace probe's leftovers in {}: {e}",
                parent.display()
            );
            return;
        }
    };
    let names = match root.read_names(".") {
        Ok(names) => names,
        Err(e) => {
            tracing::warn!(
                "ranikust: cannot look for the user namespace probe's leftovers in {}: {e}",
                parent.display()
            );
            return;
        }
    };
    let prefix = rnk_store::USERNS_PROBE_PREFIX.as_bytes();
    for name in names
        .iter()
        .filter(|n| n.as_encoded_bytes().starts_with(prefix))
    {
        match remove_if_own_and_empty(&root, name) {
            Ok(true) => tracing::info!(
                "ranikust: removed {} left by an earlier build",
                parent.join(name).display()
            ),
            Ok(false) => {}
            Err(e) => tracing::warn!(
                "ranikust: {} left by an earlier build not removed: {e}",
                parent.join(name).display()
            ),
        }
    }
}

/// [`remove_own_probe_leftovers`] for `name` of `root`: whether it was
/// removed.
fn remove_if_own_and_empty(root: &RootPath, name: &std::ffi::OsStr) -> Result<bool, String> {
    let euid = rustix::process::geteuid().as_raw();
    let st = match root.statx(name) {
        Ok(st) => st,
        Err(e) if e.raw_os_error() == Some(Errno::NOENT.raw_os_error()) => return Ok(false),
        Err(e) => return Err(e.to_string()),
    };
    let own = st.file_type() == rnk_rootfs::FileType::Directory
        && !st.mount_root
        && st.mnt_id == root.mnt_id()
        && st.uid == euid
        && st.mode & 0o7777 == 0o700;
    if !own {
        return Ok(false);
    }
    let fd = match root.open_at(name, &rnk_rootfs::OpenHow::read().directory().no_follow()) {
        Ok(fd) => fd,
        Err(e) if e.raw_os_error() == Some(Errno::NOENT.raw_os_error()) => return Ok(false),
        Err(e) => return Err(e.to_string()),
    };
    let held = rustix::fs::fstat(&fd).map_err(|e| e.to_string())?;
    let listed = rustix::fs::makedev(st.dev_major, st.dev_minor);
    if (held.st_dev, held.st_ino, held.st_uid) != (listed, st.ino, euid)
        || held.st_mode & 0o7777 != 0o700
    {
        return Ok(false);
    }
    match rustix::fs::flock(&fd, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {}
        // A probe uses it.
        Err(Errno::WOULDBLOCK) => return Ok(false),
        Err(e) => return Err(e.to_string()),
    }
    for entry in rustix::fs::Dir::read_from(&fd).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        if !matches!(entry.file_name().to_bytes(), b"." | b"..") {
            return Ok(false);
        }
    }
    // The name alone: `rmdir` takes only an empty directory, whatever is
    // there now.
    match root.remove_dir(name) {
        Ok(()) => Ok(true),
        Err(e)
            if [Errno::NOENT, Errno::NOTEMPTY]
                .map(Errno::raw_os_error)
                .map(Some)
                .contains(&e.raw_os_error()) =>
        {
            Ok(false)
        }
        Err(e) => Err(e.to_string()),
    }
}

/// A descriptor of the directory `dir` holding its exclusive `flock`, taken
/// without waiting. It was just made, empty and this user's, so a store or
/// a build's start may have taken it for a leftover meanwhile: `None` where
/// one holds it or removed it. Errors name their step.
fn hold_dir(dir: &RootPath) -> Result<Option<OwnedFd>, String> {
    let fd = dir
        .open_at(".", &rnk_rootfs::OpenHow::read().directory().no_follow())
        .map_err(|e| format!("open: {e}"))?;
    match rustix::fs::flock(&fd, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {}
        Err(Errno::WOULDBLOCK) => return Ok(None),
        Err(e) => return Err(format!("flock: {e}")),
    }
    let st = rustix::fs::fstat(&fd).map_err(|e| format!("fstat: {e}"))?;
    Ok((st.st_nlink > 0).then_some(fd))
}

/// Gives the directory `name` in `root`, which this process just made, to
/// `owner`'s uid and gid, checked on the pinned entry: in a directory
/// another process of that uid may write, a name swapped meanwhile for
/// anything but a directory of this process's is left as it is, and
/// refused.
fn give_dir(root: &RootPath, name: &str, owner: (u32, u32)) -> Result<(), String> {
    let euid = rustix::process::geteuid().as_raw();
    let given = root
        .set_meta_if(name, |st| {
            (st.file_type() == rnk_rootfs::FileType::Directory && st.uid == euid).then(|| {
                rnk_rootfs::Meta {
                    uid: Some(owner.0),
                    gid: Some(owner.1),
                    ..rnk_rootfs::Meta::default()
                }
            })
        })
        .map_err(|e| e.to_string())?;
    if given {
        Ok(())
    } else {
        Err(format!(
            "{name} is no longer the directory made for the probe"
        ))
    }
}

/// Gives the directory `dir` the process's gid and no setgid bit, through
/// its own descriptor, as the store's own directories get (ADR-0099): a
/// setgid parent's gid, which no map holds, would stop the probe's `chown`
/// in the user namespace.
fn own_group(dir: &RootPath) -> rnk_rootfs::Result<bool> {
    const SETGID: u32 = 0o2000;
    let egid = rustix::process::getegid().as_raw();
    dir.set_meta_if(".", |st| {
        (st.mode & SETGID != 0 || st.gid != egid).then(|| rnk_rootfs::Meta {
            gid: Some(egid),
            mode: Some(st.permissions() & !SETGID),
            ..rnk_rootfs::Meta::default()
        })
    })
}

/// The probes in `scratch`, at `path`; and whether their directory stays
/// ([`keeps_dir`]; never when `overlay` cannot be made in it, before any
/// child of the overlay probe).
fn probe_in_scratch(
    scratch: &RootPath,
    path: &Path,
    ids: rnk_sandbox::BuildIds,
) -> (Result<Probed, String>, bool) {
    let report = match rnk_sandbox::probe_user_namespace(scratch, ids) {
        Ok(report) => report,
        Err(e) => return (Err(e.to_string()), keeps_dir(Some(&e), None)),
    };
    let overlay = match report.map() {
        Some(_) => {
            let made = scratch
                .create_dir("overlay", 0o700)
                .map_err(|e| e.to_string())
                .and_then(|()| {
                    ids.shifted_root()
                        .map_or(Ok(()), |owner| give_dir(scratch, "overlay", owner))
                });
            if let Err(e) = made {
                return (Err(e), false);
            }
            report.in_namespace(overlay_in_namespace, &path.join("overlay"))
        }
        None => None,
    };
    let keep = keeps_dir(None, overlay.as_ref().and_then(|r| r.as_ref().err()));
    let overlay = overlay.map(|r| r.map_err(|e| e.to_string()));
    let apparmor = match std::fs::read_to_string(APPARMOR_USERNS) {
        Ok(v) => v.trim().to_owned(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => "absent".to_owned(),
        Err(e) => format!("unreadable: {e}"),
    };
    let probed = Probed {
        report,
        overlay,
        apparmor,
    };
    (Ok(probed), keep)
}

/// `rnk-snapshot`'s overlay probe in `dir`, run in the user namespace: the
/// mount API's name.
fn overlay_in_namespace(dir: &Path) -> Result<String, String> {
    rnk_snapshot::probe(dir)
        .map(|caps| mount_api_name(caps.api).to_owned())
        .map_err(|e| e.to_string())
}

/// Whether the probe's directory stays: the steps' error or the overlay
/// probe's in the namespace says a child of the probe was left there,
/// which may still work in it.
fn keeps_dir(steps: Option<&rnk_sandbox::Error>, overlay: Option<&rnk_sandbox::Error>) -> bool {
    let left = |e: Option<&rnk_sandbox::Error>| {
        matches!(e, Some(rnk_sandbox::Error::ProbeChildLeft { .. }))
    };
    left(steps) || left(overlay)
}

/// Ubuntu's switch that gives an unprivileged user namespace no
/// capabilities (24.04 sets it).
const APPARMOR_USERNS: &str = "/proc/sys/kernel/apparmor_restrict_unprivileged_userns";

/// The overlay probe in the user namespace: `None` without a map, `Err`
/// when it could not run, else its own result (the mount API's name).
type OverlayInNamespace = Option<Result<Result<String, String>, String>>;

/// The user namespace probe's line: whether the path passes (every step a
/// `RUN` needs, then overlay), where it stops, each step, the overlay and
/// the AppArmor switch.
fn userns_summary(
    stop: Option<&str>,
    steps: &str,
    overlay: OverlayInNamespace,
    apparmor: &str,
) -> (bool, String) {
    let (overlay_ok, overlay) = match overlay {
        Some(Ok(Ok(api))) => (Some(true), format!("ok, {api}")),
        Some(Ok(Err(e))) => (Some(false), format!("refused: {e}")),
        Some(Err(e)) => (None, format!("could not run: {e}")),
        None => (None, "not run (no id map)".to_owned()),
    };
    let verdict = match (stop, overlay_ok) {
        (Some(step), _) => format!("stops at {step}"),
        (None, Some(true)) => "passes".to_owned(),
        (None, Some(false)) => "stops at overlay".to_owned(),
        (None, None) => "overlay not probed".to_owned(),
    };
    let passes = stop.is_none() && overlay_ok == Some(true);
    (
        passes,
        format!(
            "{verdict}; {steps}; overlay: {overlay}; kernel.apparmor_restrict_unprivileged_userns: \
             {apparmor}"
        ),
    )
}

/// How overlay mounts are made, for the log.
fn mount_api_name(api: MountApi) -> &'static str {
    match api {
        MountApi::LowerdirPlus => "fsconfig(lowerdir+)",
        MountApi::Mount2 => "mount(2)",
    }
}

/// A set-up step that failed with `e`.
fn failed(what: &str, e: &dyn fmt::Display) -> Refusal {
    Refusal {
        kind: Kind::Other,
        reason: format!("{what}: {e}"),
        guidance: "".into(),
    }
}

/// `rnk-snapshot`'s overlay probe (steps 1 and 4), in a directory of this
/// build: on the state directory's filesystem, where the upper directories
/// will be, and removed with the build.
fn probe_overlay(build_dir: &Path, euid: u32) -> Result<OverlayCaps, Refusal> {
    let dir = build_dir.join("overlay-probe");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .map_err(|e| Refusal {
            kind: Kind::Other,
            reason: format!("cannot create {}: {e}", dir.display()),
            guidance: "".into(),
        })?;
    rnk_snapshot::probe(&dir).map_err(|e| overlay_refusal(&e, euid))
}

/// What the test hook's `overlay-refused` makes of a passed overlay probe:
/// a failed probe check, which routes as a mount the kernel refused (on
/// NFS) does.
const FAULT_OVERLAY_REFUSED: SnapshotError =
    SnapshotError::ProbeCheck("refused by RANIKUST_DEBUG_FAULT=overlay-refused");

/// The refusal for a failed overlay probe. Where the kernel refused the
/// mount (an upper directory on overlayfs or NFS, overlay nested too deep,
/// no privilege), root needs kaniko mode; a host-side failure (the state
/// directory unwritable, a full disk) gets no such guidance.
fn overlay_refusal(e: &SnapshotError, euid: u32) -> Refusal {
    let refused_mount = match e {
        SnapshotError::Probe { source, .. } => kernel_refused_mount(source),
        SnapshotError::ProbeCheck(_) => true,
        _ => false,
    };
    let (kind, guidance) = match (refused_mount, euid) {
        (true, 0) => (Kind::OverlayUnusable, OVERLAY_UNUSABLE),
        (true, _) => (Kind::Other, NEEDS_ROOT),
        (false, _) => (Kind::Other, ""),
    };
    Refusal {
        kind,
        reason: format!("overlay mode cannot run here: {e}"),
        guidance: guidance.into(),
    }
}

/// Whether the kernel refused the probe's overlay mount: a `Mount` error
/// from the mount itself, not from starting its thread or running out of
/// memory, descriptors, processes or disk, or an I/O error, which say
/// nothing about overlayfs.
fn kernel_refused_mount(e: &SnapshotError) -> bool {
    match e {
        SnapshotError::Mount { step, errno, .. } => {
            !step.starts_with("start the mount thread")
                && ![
                    Errno::AGAIN,
                    Errno::NOMEM,
                    Errno::MFILE,
                    Errno::NFILE,
                    Errno::NOSPC,
                    Errno::DQUOT,
                    Errno::IO,
                ]
                .contains(errno)
        }
        _ => false,
    }
}

/// [`crate::statedir::prepare`] as a [`Refusal`].
fn prepare_state_dir(state_dir: &Path) -> Result<(), Refusal> {
    crate::statedir::prepare(state_dir).map_err(|refused| Refusal {
        kind: Kind::Other,
        reason: refused.0,
        guidance: "".into(),
    })
}

/// The refusal for a failed [`rnk_sandbox::enter_build_namespace`].
fn namespace_refusal(e: SandboxError, euid: u32) -> Refusal {
    let (kind, guidance) = match (&e, euid) {
        (SandboxError::Privilege(_), 0) => (Kind::NoMountPrivilege, NEEDS_KANIKO_MODE),
        (SandboxError::Privilege(_), _) => (Kind::Other, NEEDS_ROOT),
        _ => (Kind::Other, ""),
    };
    Refusal {
        kind,
        reason: e.to_string(),
        guidance: guidance.into(),
    }
}

/// Picks the outcome from the probes' results: both must succeed for
/// [`Outcome::OverlayRoot`], the only outcome of Phase 1; otherwise a
/// refusal with the failing probe's diagnosis. Kaniko mode is suggested only
/// where the environment refused a step of the sandbox, not for a host-side
/// failure (the state directory unwritable, a full disk). Generic over the
/// sandbox's caps, which only `rnk-sandbox` can make.
fn pick<C>(
    sandbox: Result<C, SandboxError>,
    overlay: Result<OverlayCaps, Refusal>,
    euid: u32,
) -> Result<(C, OverlayCaps), Refusal> {
    let sandbox = sandbox.map_err(|e| {
        let (kind, guidance) = match (&e, euid) {
            // Root entered the build's mount namespace, yet a step of the
            // sandbox lacks a privilege: kaniko mode off `/` would need it
            // too, so neither mode is the way here.
            (SandboxError::Privilege(_), 0) => (Kind::SandboxBlocked, SANDBOX_NO_PRIVILEGE),
            (SandboxError::Privilege(_), _) => (Kind::Other, NEEDS_ROOT),
            (SandboxError::Setup { .. }, _) => (Kind::SandboxBlocked, SANDBOX_BLOCKED),
            _ => (Kind::Other, ""),
        };
        Refusal {
            kind,
            guidance: guidance.into(),
            reason: format!("the RUN sandbox cannot run here: {e}"),
        }
    })?;
    Ok((sandbox, overlay?))
}

#[cfg(test)]
mod tests;
