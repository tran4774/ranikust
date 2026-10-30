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
use std::os::unix::fs::DirBuilderExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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

/// Guidance where the state directory is not safe to use as root.
const UNSAFE_STATE_DIR: &str = "the state directory and the workdir above it must be real \
     directories (no symlink), owned by the user running ranikust and not writable by group or \
     others; pass another --workdir or --state-dir";

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

/// Sets the process up for a build whose state directory is `state_dir`, in
/// `workdir` if given, once [`enter_namespace`] succeeded: the state
/// directory's check, the store (marker check, start-up clean-up), the probes of
/// `rnk-sandbox` and `rnk-snapshot`, the outcome, then the spawner thread.
/// Before the spawner, the only other thread is the overlay probe's
/// `mount(2)` attempt, which it joins: the process is single-threaded
/// again when the spawner starts. `userns_network` is `--allow
/// network.none-userns` (ADR-0033).
///
/// # Errors
///
/// A [`Refusal`] naming what failed and what to do instead.
pub(crate) fn set_up(
    state_dir: &Path,
    workdir: Option<&Path>,
    userns_network: bool,
) -> Result<Setup, Refusal> {
    let euid = rustix::process::geteuid().as_raw();
    prepare_state_dir(state_dir, workdir, euid)?;
    // The store first, then the probes, which write in the state directory
    // only once its marker is checked (Q-111, (a)).
    let store =
        Store::open(&BuildPaths::new(workdir.unwrap_or(state_dir), state_dir)).map_err(|e| {
            Refusal {
                reason: format!("cannot open the store in {}: {e}", state_dir.display()),
                guidance: "",
            }
        })?;
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

/// What kaniko mode's set-up hands to the build.
pub(crate) struct KanikoSetup {
    /// The store in the state directory, open for this build.
    pub(crate) store: Arc<Store>,
    /// The spawner thread every `RUN` is cloned from.
    pub(crate) sandbox: Sandbox,
    /// How `RUN` is isolated: the full sandbox on a host workdir where
    /// mounting works, else `Direct` (ADR-0043).
    pub(crate) isolation: rnk_sandbox::Isolation,
}

/// Guidance where kaniko mode runs without root.
const KANIKO_NEEDS_ROOT: &str = "kaniko mode needs root (chown, chroot, switching users), as \
     kaniko does: run ranikust as root in the container or VM that builds";

/// Sets kaniko mode up, once the root is claimed (a host workdir, locked
/// and marked, or `/` in a container): the state directory's check, the
/// store, and the spawner. `can_mount` is whether the build's mount
/// namespace could be entered: then, on a host workdir, `RUN` gets the
/// full sandbox, probed here; anywhere else it runs `Direct`, which on a
/// workdir enters it with `chroot` (ADR-0043). `nofile` is the
/// `RLIMIT_NOFILE` ranikust started with, given back to every `RUN`.
///
/// # Errors
///
/// A [`Refusal`]: not root, an unsafe state directory, the store, a probe.
pub(crate) fn set_up_kaniko(
    state_dir: &Path,
    claimed: Option<&crate::workdir::Workdir>,
    can_mount: bool,
    nofile: rnk_sandbox::NofileLimit,
) -> Result<KanikoSetup, Refusal> {
    let euid = rustix::process::geteuid().as_raw();
    let workdir = claimed.map(crate::workdir::Workdir::path);
    if euid != 0 {
        return Err(Refusal {
            reason: format!("kaniko mode cannot run as uid {euid}"),
            guidance: KANIKO_NEEDS_ROOT,
        });
    }
    prepare_state_dir(state_dir, workdir, euid)?;
    let store =
        Store::open(&BuildPaths::new(workdir.unwrap_or(state_dir), state_dir)).map_err(|e| {
            Refusal {
                reason: format!("cannot open the store in {}: {e}", state_dir.display()),
                guidance: "",
            }
        })?;
    let sandbox_dir = state_dir.join("sandbox");
    let caps = match workdir {
        Some(_) if can_mount => {
            let span = tracing::debug_span!("ranikust.probe", steps = 0_u64);
            let _entered = span.enter();
            let caps = rnk_sandbox::probe(&sandbox_dir).map_err(|e| Refusal {
                reason: format!("the RUN sandbox cannot run here: {e}"),
                guidance: "",
            })?;
            span.record("steps", 1_u64);
            Some(caps)
        }
        Some(_) | None => None,
    };
    let isolation = if caps.is_some() {
        rnk_sandbox::Isolation::Full
    } else {
        rnk_sandbox::Isolation::Direct
    };
    let started = match claimed {
        Some(w) => Sandbox::start_with_root(caps, &sandbox_dir, w.path(), w.root()),
        None => Sandbox::start(caps, &sandbox_dir),
    };
    let mut sandbox = started.map_err(|e| Refusal {
        reason: e.to_string(),
        guidance: "",
    })?;
    sandbox.set_nofile_limit(nofile);
    tracing::info!(
        "ranikust: kaniko mode on {}, RUN {}",
        workdir.map_or_else(|| "/".to_owned(), |w| w.display().to_string()),
        if caps.is_some() {
            "in the full sandbox"
        } else {
            "directly, as kaniko"
        }
    );
    Ok(KanikoSetup {
        store: Arc::new(store),
        sandbox,
        isolation,
    })
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

/// Creates the state directory if missing (mode 0700) and checks it, and
/// every directory above it up to and including `workdir`: each must be a
/// real directory, not a symlink, owned by `euid` and not writable by group
/// or others. The probes run as root in it, and the sandbox replays what its
/// journals there name. Checked before anything is created, and again
/// after. A stand-in for the workdir checks (Q-011); the store's marker
/// check follows it (`Store::open`, Q-111).
fn prepare_state_dir(state_dir: &Path, workdir: Option<&Path>, euid: u32) -> Result<(), Refusal> {
    check_state_dir(state_dir, workdir, euid, true)?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(state_dir)
        .map_err(|e| Refusal {
            reason: format!(
                "cannot create the state directory {}: {e}",
                state_dir.display()
            ),
            guidance: "",
        })?;
    check_state_dir(state_dir, workdir, euid, false)
}

/// The check of [`prepare_state_dir`]; with `missing_ok`, entries that do
/// not exist yet pass.
fn check_state_dir(
    state_dir: &Path,
    workdir: Option<&Path>,
    euid: u32,
    missing_ok: bool,
) -> Result<(), Refusal> {
    use std::os::unix::fs::MetadataExt as _;
    let chain = state_dir
        .ancestors()
        .take_while(|p| *p == state_dir || workdir.is_some_and(|w| p.starts_with(w)));
    for dir in chain {
        let refuse = |why: &str| Refusal {
            reason: format!("unsafe state directory: {} {why}", dir.display()),
            guidance: UNSAFE_STATE_DIR,
        };
        let st = match std::fs::symlink_metadata(dir) {
            Ok(st) => st,
            Err(e) if missing_ok && e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(refuse(&format!("cannot be read: {e}"))),
        };
        if st.file_type().is_symlink() {
            return Err(refuse("is a symlink"));
        }
        if !st.is_dir() {
            return Err(refuse("is not a directory"));
        }
        if st.uid() != euid {
            return Err(refuse(&format!("is owned by uid {}, not {euid}", st.uid())));
        }
        if st.mode() & 0o022 != 0 {
            return Err(refuse("is writable by group or others"));
        }
    }
    Ok(())
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
