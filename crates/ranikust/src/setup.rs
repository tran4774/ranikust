//! The process set-up of a build, while the process is single-threaded:
//! the private mount namespace, the probes in order, the outcome, then the
//! spawner thread (`design/sandbox.md#process-set-up-once-per-build`,
//! `design/modes-and-detection.md#probe-order---modeauto`).
//!
//! Overlay mode as root is the one outcome built so far. `--mode=auto`
//! enters the namespace first ([`enter_namespace`]): where root may not
//! mount, the binary picks kaniko mode instead (ADR-0040); every other
//! environment is refused with the probe's diagnosis.

use std::fmt;
use std::os::fd::OwnedFd;
use std::os::unix::fs::DirBuilderExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rnk_rootfs::RootPath;
use rustix::fs::FlockOperation;

use rnk_sandbox::{Caps, Error as SandboxError, Sandbox};
use rnk_snapshot::{Error as SnapshotError, MountApi, OverlayCaps};
use rnk_store::{BuildPaths, Store};

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
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Outcome::OverlayRoot { sandbox, overlay } => {
                write!(f, "overlay mode as root, RUN enters its root with ")?;
                f.write_str(match sandbox.root_entry {
                    rnk_sandbox::RootEntry::PivotRoot => "pivot_root",
                    rnk_sandbox::RootEntry::MoveMount => "MS_MOVE",
                })?;
                f.write_str(match overlay.api {
                    MountApi::LowerdirPlus => ", overlay mounts with fsconfig(lowerdir+)",
                    MountApi::Mount2 => ", overlay mounts with mount(2)",
                })
            }
        }
    }
}

/// Why this environment cannot build, and what to do instead
/// (`design/modes-and-detection.md#refusal-and-guidance`).
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Refusal {
    /// What failed, with the step and errno when a probe found it.
    pub(crate) reason: String,
    /// What to do instead; empty when the reason already says it.
    pub(crate) guidance: &'static str,
}

impl Refusal {
    /// Whether this environment needs kaniko mode: root that may not mount
    /// or cannot use overlay, what `--mode=auto` turns into kaniko mode.
    pub(crate) fn needs_kaniko_mode(&self) -> bool {
        self.guidance == NEEDS_KANIKO_MODE
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

/// Guidance where the build needs root: only overlay mode as root exists in
/// Phase 1.
const NEEDS_ROOT: &str = "ranikust builds as root only for now (rootless overlay mode comes in \
     Phase 3, kaniko mode in Phase 2): run it with sudo, or in a container started with \
     --privileged";

/// Guidance where root cannot use the sandbox or overlay: the outcome that
/// would run here is kaniko mode.
const NEEDS_KANIKO_MODE: &str = "this environment needs kaniko mode: use --mode=kaniko or \
     --mode=auto; a container started with --privileged can build in overlay mode";

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

/// Sets overlay mode up for a build whose state directory is `state_dir`,
/// once [`enter_namespace`] succeeded: the state directory's check
/// ([`crate::statedir::prepare`]; refused on overlayfs, a container's own
/// root, ADR-0050), the store (marker check, start-up clean-up), the probes of
/// `rnk-sandbox` and `rnk-snapshot`, the outcome, then the spawner thread.
/// Before the spawner, the only other thread is the overlay probe's
/// `mount(2)` attempt, which it joins: the process is single-threaded
/// again when the spawner starts. `userns_network` is `--allow
/// network.none-userns` (ADR-0033).
///
/// # Errors
///
/// A [`Refusal`] naming what failed and what to do instead.
pub(crate) fn set_up(state_dir: &Path, userns_network: bool) -> Result<Setup, Refusal> {
    let euid = rustix::process::geteuid().as_raw();
    prepare_state_dir(state_dir)?;
    if crate::statedir::on_overlayfs(state_dir) {
        return Err(Refusal {
            reason: format!(
                "the state directory {} is on overlayfs, a container's own root, where overlay \
                 mode cannot keep its upper directories",
                state_dir.display()
            ),
            guidance: ON_OVERLAYFS,
        });
    }
    // The store first, then the probes, which write in the state directory
    // only once its marker is checked (Q-111, (a)).
    let store = open_store(state_dir, state_dir)?;
    let sandbox_dir = state_dir.join("sandbox");
    let outcome = {
        let span = tracing::debug_span!("ranikust.probe", steps = 0_u64);
        let _entered = span.enter();
        let sandbox = rnk_sandbox::probe(&sandbox_dir);
        span.record("steps", 1_u64);
        let overlay = probe_overlay(store.build_dir(), euid);
        span.record("steps", 2_u64);
        let (sandbox, overlay) = pick(sandbox, overlay, euid)?;
        Outcome::OverlayRoot { sandbox, overlay }
    };
    tracing::info!("ranikust: {outcome}");
    let Outcome::OverlayRoot { sandbox: caps, .. } = outcome;
    let mut sandbox = Sandbox::start(Some(caps), &sandbox_dir).map_err(|e| Refusal {
        reason: e.to_string(),
        guidance: "",
    })?;
    if userns_network {
        sandbox.allow_userns_network();
    }
    Ok(Setup {
        outcome,
        state_dir: state_dir.to_path_buf(),
        store: Arc::new(store),
        sandbox,
    })
}

/// Guidance where overlay mode's state directory is on overlayfs.
const ON_OVERLAYFS: &str = "mount a volume at the state directory, e.g. \
     `-v ranikust-state:/var/lib/ranikust`, or pass --state-dir on one; or use --mode=kaniko";

/// Opens the store in `state_dir` for a build whose root is `root`.
fn open_store(root: &Path, state_dir: &Path) -> Result<Store, Refusal> {
    Store::open(&BuildPaths::new(root, state_dir)).map_err(|e| Refusal {
        reason: format!("cannot open the store in {}: {e}", state_dir.display()),
        guidance: "",
    })
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
    /// The build's root, opened once: `/`, or the directory made for it.
    pub(crate) root: RootPath,
    /// Its path, for the ignore list, messages and the sandbox.
    pub(crate) root_path: PathBuf,
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
/// itself, and `RUN` runs `Direct`, as kaniko. Otherwise the root is
/// `<state>/builds/<id>/rootfs`, made here and removed with the build, and
/// `RUN` gets the full sandbox, probed here; without mount privileges
/// (`can_mount` false) that case is refused (Q-330, ADR-0050), never a bare
/// `chroot` on the host. `nofile` is the `RLIMIT_NOFILE` ranikust
/// started with, given back to every `RUN`.
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
) -> Result<KanikoSetup, Refusal> {
    let euid = rustix::process::geteuid().as_raw();
    if euid != 0 {
        return Err(Refusal {
            reason: format!("kaniko mode cannot run as uid {euid}"),
            guidance: KANIKO_NEEDS_ROOT,
        });
    }
    if let Some(refusal) = host_without_mounts(on_slash, can_mount) {
        return Err(refusal);
    }
    // On `/`, the lock first, on `/` itself: two builds in one container
    // never share it, whatever their state directories.
    let root_lock = if on_slash { Some(lock_slash()?) } else { None };
    prepare_state_dir(state_dir)?;
    let slash = Path::new("/");
    // On `/` a `RUN` reads any file with a path: the store keeps none, its
    // blobs are anonymous files (ADR-0051).
    let store = if on_slash {
        Store::open_anonymous(state_dir).map_err(|e| Refusal {
            reason: format!("cannot open the store in {}: {e}", state_dir.display()),
            guidance: "",
        })?
    } else {
        open_store(state_dir, state_dir)?
    };
    let state = RootPath::open(store.state_dir())
        .map_err(|e| failed(&format!("opening {}", store.state_dir().display()), &e))?;
    let (root, root_path) = if on_slash {
        let root = RootPath::open(slash).map_err(|e| failed("opening /", &e))?;
        (root, slash.to_path_buf())
    } else {
        let path = store.build_dir().join("rootfs");
        let rel = path
            .strip_prefix(store.state_dir())
            .map_err(|_| Refusal {
                reason: format!("the build root {} is outside the store", path.display()),
                guidance: "",
            })?
            .to_path_buf();
        state
            .create_dir(&rel, 0o755)
            .map_err(|e| failed(&format!("creating {}", path.display()), &e))?;
        let root = state
            .sub_root(&rel)
            .map_err(|e| failed(&format!("opening {}", path.display()), &e))?;
        (root, path)
    };
    let sandbox_dir = store.state_dir().join("sandbox");
    // Off `/` root may mount here (`host_without_mounts`): the full sandbox.
    let caps = if on_slash {
        None
    } else {
        let span = tracing::debug_span!("ranikust.probe", steps = 0_u64);
        let _entered = span.enter();
        let caps = rnk_sandbox::probe(&sandbox_dir).map_err(|e| Refusal {
            reason: format!("the RUN sandbox cannot run here: {e}"),
            guidance: "",
        })?;
        span.record("steps", 1_u64);
        Some(caps)
    };
    let isolation = if caps.is_some() {
        rnk_sandbox::Isolation::Full
    } else {
        rnk_sandbox::Isolation::Direct
    };
    let started = if on_slash {
        // `Direct` on `/` places nothing: no journal, no directory; the
        // process becomes the subreaper of what a `RUN` leaves (ADR-0051).
        Sandbox::start_direct()
    } else {
        Sandbox::start_with_root(caps, &sandbox_dir, &root_path, &root)
    };
    let mut sandbox = started.map_err(|e| Refusal {
        reason: e.to_string(),
        guidance: "",
    })?;
    sandbox.set_nofile_limit(nofile);
    tracing::info!(
        "ranikust: kaniko mode on {}, RUN {}",
        root_path.display(),
        if caps.is_some() {
            "in the full sandbox"
        } else {
            "directly, as kaniko"
        }
    );
    Ok(KanikoSetup {
        state_dir: store.state_dir().to_path_buf(),
        store: Arc::new(store),
        sandbox,
        isolation,
        root,
        root_path,
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
        reason: "kaniko mode outside a container, where root may not mount: RUN would run in \
                 a bare chroot on this machine, which root can leave"
            .to_owned(),
        guidance: HOST_WITHOUT_MOUNTS,
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
            reason: "another ranikust build is using / in this container".to_owned(),
            guidance: "run one kaniko-mode build at a time per container",
        }),
        Err(e) => Err(failed("cannot lock the root /", &e)),
    }
}

/// A set-up step that failed with `e`.
fn failed(what: &str, e: &dyn fmt::Display) -> Refusal {
    Refusal {
        reason: format!("{what}: {e}"),
        guidance: "",
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
            reason: format!("cannot create {}: {e}", dir.display()),
            guidance: "",
        })?;
    rnk_snapshot::probe(&dir).map_err(|e| overlay_refusal(&e, euid))
}

/// The refusal for a failed overlay probe. Where the kernel refused the
/// mount (an upper directory on overlayfs or NFS, overlay nested too deep,
/// no privilege), root needs kaniko mode; a host-side failure (the state
/// directory unwritable, a full disk) gets no such guidance.
fn overlay_refusal(e: &SnapshotError, euid: u32) -> Refusal {
    let refused_mount = matches!(
        e,
        SnapshotError::Probe { source, .. }
            if matches!(**source, SnapshotError::Mount { .. })
    ) || matches!(e, SnapshotError::ProbeCheck(_));
    Refusal {
        reason: format!("overlay mode cannot run here: {e}"),
        guidance: if refused_mount {
            privilege_guidance(euid)
        } else {
            ""
        },
    }
}

/// [`crate::statedir::prepare`] as a [`Refusal`].
fn prepare_state_dir(state_dir: &Path) -> Result<(), Refusal> {
    crate::statedir::prepare(state_dir).map_err(|refused| Refusal {
        reason: refused.0,
        guidance: "",
    })
}

/// The guidance for missing privileges: root that still lacks them (Docker's
/// defaults) needs kaniko mode; anyone else needs root.
fn privilege_guidance(euid: u32) -> &'static str {
    if euid == 0 {
        NEEDS_KANIKO_MODE
    } else {
        NEEDS_ROOT
    }
}

/// The refusal for a failed [`rnk_sandbox::enter_build_namespace`].
fn namespace_refusal(e: SandboxError, euid: u32) -> Refusal {
    let guidance = match e {
        SandboxError::Privilege(_) => privilege_guidance(euid),
        _ => "",
    };
    Refusal {
        reason: e.to_string(),
        guidance,
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
    let sandbox = sandbox.map_err(|e| Refusal {
        guidance: match e {
            SandboxError::Privilege(_) => privilege_guidance(euid),
            SandboxError::Setup { .. } => NEEDS_KANIKO_MODE,
            _ => "",
        },
        reason: format!("the RUN sandbox cannot run here: {e}"),
    })?;
    Ok((sandbox, overlay?))
}

#[cfg(test)]
mod tests;
