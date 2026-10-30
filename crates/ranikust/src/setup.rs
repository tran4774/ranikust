//! The process set-up of a build, while the process is single-threaded:
//! the private mount namespace, the probes in order, the outcome, then the
//! spawner thread (`design/sandbox.md#process-set-up-once-per-build`,
//! `design/modes-and-detection.md#probe-order---modeauto`).
//!
//! Phase 1 builds one outcome, overlay mode as root; every other
//! environment is refused with the probe's diagnosis.

use std::fmt;
use std::os::unix::fs::DirBuilderExt as _;
use std::path::{Path, PathBuf};

use rnk_sandbox::{Caps, Error as SandboxError, Sandbox};

/// What `rnk-snapshot`'s overlay probe found: the overlay mount with the
/// ADR-0016 options and `fsconfig(lowerdir+)` (probe steps 1 and 4).
///
/// A placeholder until task 9: `rnk-snapshot` defines its own caps type and
/// [`probe_overlay`] returns it, as `rnk_sandbox::probe` returns
/// [`Caps`] (Q-017, interim (b)).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OverlayCaps {
    /// Whether the probe really ran; `false` until task 9.
    pub(crate) probed: bool,
}

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
                if !overlay.probed {
                    f.write_str(" (overlay not probed yet)")?;
                }
                Ok(())
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
const NEEDS_KANIKO_MODE: &str = "this environment needs kaniko mode, which comes in Phase 2; \
     a container started with --privileged can build in overlay mode";

/// A build environment, set up: what the build hands to the libraries.
pub(crate) struct Setup {
    /// The picked outcome.
    pub(crate) outcome: Outcome,
    /// The state directory the probes used.
    pub(crate) state_dir: PathBuf,
    /// The spawner thread every `RUN` is cloned from.
    pub(crate) sandbox: Sandbox,
}

/// Guidance where the state directory is not safe to use as root.
const UNSAFE_STATE_DIR: &str = "the state directory and the workdir above it must be real \
     directories (no symlink), owned by the user running ranikust and not writable by group or \
     others; pass another --workdir or --state-dir";

/// Sets the process up for a build whose state directory is `state_dir`, in
/// `workdir` if given: a private mount namespace, the state directory's
/// check, the probes of `rnk-sandbox` and `rnk-snapshot`, the outcome, then
/// the spawner thread. Starts no thread before the spawner.
///
/// # Errors
///
/// A [`Refusal`] naming what failed and what to do instead.
pub(crate) fn set_up(state_dir: &Path, workdir: Option<&Path>) -> Result<Setup, Refusal> {
    let euid = rustix::process::geteuid().as_raw();
    // Workdir checks (Q-011) come first, once designed.
    rnk_sandbox::enter_build_namespace().map_err(|e| namespace_refusal(e, euid))?;
    prepare_state_dir(state_dir, workdir, euid)?;
    let sandbox_dir = state_dir.join("sandbox");
    let outcome = {
        // `steps`: the probes that really ran; the overlay seam does not
        // count until task 9 fills it.
        let span = tracing::debug_span!("ranikust.probe", steps = 0_u64);
        let _entered = span.enter();
        let sandbox = rnk_sandbox::probe(&sandbox_dir);
        span.record("steps", 1_u64);
        let overlay = probe_overlay(state_dir);
        let (sandbox, overlay) = pick(sandbox, overlay, euid)?;
        Outcome::OverlayRoot { sandbox, overlay }
    };
    tracing::info!("ranikust: {outcome}");
    // Task 7: `rnk_store::Store::open` (marker check, start-up clean-up)
    // goes here, before any thread.
    let Outcome::OverlayRoot { sandbox: caps, .. } = outcome;
    let sandbox = Sandbox::start(Some(caps), &sandbox_dir).map_err(|e| Refusal {
        reason: e.to_string(),
        guidance: "",
    })?;
    Ok(Setup {
        outcome,
        state_dir: state_dir.to_path_buf(),
        sandbox,
    })
}

/// The seam for task 9: `rnk_snapshot::probe(state_dir)` replaces this.
/// Until then, overlay is assumed to work wherever the sandbox does.
fn probe_overlay(_state_dir: &Path) -> Result<OverlayCaps, Refusal> {
    Ok(OverlayCaps { probed: false })
}

/// Creates the state directory if missing (mode 0700) and checks it, and
/// every directory above it up to and including `workdir`: each must be a
/// real directory, not a symlink, owned by `euid` and not writable by group
/// or others. The probes run as root in it, and the sandbox replays what its
/// journals there name. Checked before anything is created, and again
/// after. A stand-in for the workdir checks (Q-011) and the store's marker
/// (Q-111).
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
