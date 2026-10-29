//! The sandbox's probe: performs the real per-`RUN` sequence once, in an
//! empty root, and reports how a `RUN` can enter its root here.
//!
//! The probe's command does not exist, so a child that fails at
//! [`Step::Exec`] with `ENOENT` went through every step before it: the
//! namespaces, the mounts, entering the root, the capability drop and the
//! `USER` switch. Everything the child mounted lives in its own mount
//! namespace and disappears with it.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;

use tracing::field::Empty;

use crate::child::RootEntry;
use crate::error::{Error, Result, Step, io_err};
use crate::journal;
use crate::spawner::{ParentPid, Runner, clone_child};
use crate::spec::RunSpec;

/// What the probe found the sandbox can do in this environment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Caps {
    /// How a `RUN` enters its root: `pivot_root`, or `MS_MOVE` where seccomp
    /// blocks it.
    pub root_entry: RootEntry,
}

/// The command the probe tries to execute. It does not exist in the probe's
/// empty root.
const PROBE_PROGRAM: &str = "/.ranikust-probe";

/// Prefix of the probe's root directories in the sandbox directory.
const PROBE_ROOT_PREFIX: &str = "probe-";

/// Probes the sandbox: the full `RUN` sequence with `pivot_root`, then with
/// `MS_MOVE` if `pivot_root` alone was refused with `EPERM` (Docker's
/// seccomp profile) and `/` is a mount root. `dir` is the sandbox's
/// directory, as for [`crate::Sandbox::start`]; journals of dead builds are
/// replayed first.
///
/// Call in the single-threaded start-up window, before
/// [`crate::Sandbox::start`]. Each attempt is one `rnk_sandbox.probe` span.
///
/// # Errors
///
/// [`Error::Privilege`] when the namespaces cannot be created,
/// [`Error::Setup`] with the step and errno that failed, or a host-side
/// error.
pub fn probe(dir: &Path) -> Result<Caps> {
    let runner = Runner::new(dir, None)?;
    journal::replay(runner.runs_dir())?;
    remove_stale_roots(dir);
    let root_dir = create_root(dir)?;
    let parent = ParentPid::new()?;
    let result = attempts(dir, &root_dir, parent);
    // Empty again: the journal removed the stubs.
    if let Err(e) = fs::remove_dir(&root_dir) {
        tracing::warn!("rnk-sandbox: cannot remove {}: {e}", root_dir.display());
    }
    result
}

fn attempts(dir: &Path, root_dir: &Path, parent: ParentPid) -> Result<Caps> {
    let mut last = None;
    for root_entry in [RootEntry::PivotRoot, RootEntry::MoveMount] {
        if root_entry == RootEntry::MoveMount && !slash_is_mount_root()? {
            // The moved root would sit on a plain directory, which `..`
            // climbs out of: no fallback.
            break;
        }
        let span = tracing::debug_span!("rnk_sandbox.probe", ok = Empty);
        let runner = Runner::new(dir, Some(root_entry))?;
        let spec = RunSpec::new(root_dir, vec![PROBE_PROGRAM.into()]);
        let outcome =
            span.in_scope(|| reached_exec(runner.run(spec, false, |job| clone_child(job, parent))));
        span.record("ok", outcome.is_ok());
        match outcome {
            Ok(()) => return Ok(Caps { root_entry }),
            // Docker's seccomp profile: try `MS_MOVE`. Any other errno
            // (`EINVAL` when `/` is not a mount point) is final.
            Err(e) if is_seccomp_refusal(&e) => last = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or(Error::Privilege("the RUN sandbox")))
}

/// `pivot_root` refused with `EPERM`, as a seccomp filter does.
fn is_seccomp_refusal(e: &Error) -> bool {
    matches!(
        e,
        Error::Setup {
            step: Step::PivotRoot,
            source,
        } if source.raw_os_error() == Some(libc::EPERM)
    )
}

/// Whether this process's `/` is the root of a mount. `MS_MOVE` puts the
/// stage root on top of `/`; `..` stops there only if `/` is a mount root
/// (`follow_dotdot` in `fs/namei.c`), otherwise it climbs into the
/// directory above [inferred].
fn slash_is_mount_root() -> Result<bool> {
    let st = rustix::fs::statx(
        rustix::fs::CWD,
        c"/",
        rustix::fs::AtFlags::empty(),
        rustix::fs::StatxFlags::BASIC_STATS,
    )
    .map_err(|e| io_err("statx", "/", e.into()))?;
    let attr = rustix::fs::StatxAttributes::MOUNT_ROOT;
    Ok(st.stx_attributes_mask.contains(attr) && st.stx_attributes.contains(attr))
}

/// Success when the child got as far as `execve`.
fn reached_exec(r: Result<ExitStatus>) -> Result<()> {
    match r {
        Ok(_) => Ok(()),
        Err(Error::Exec { source, .. }) if source.raw_os_error() == Some(libc::ENOENT) => Ok(()),
        Err(e) => Err(e),
    }
}

/// A new, empty root directory for this probe.
fn create_root(dir: &Path) -> Result<PathBuf> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let p = dir.join(format!(
        "{PROBE_ROOT_PREFIX}{}-{:x}",
        rustix::process::getpid().as_raw_nonzero(),
        now.as_nanos()
    ));
    fs::create_dir(&p).map_err(|e| io_err("create", &p, e))?;
    // The child binds the root by path: no symlink on the way.
    p.canonicalize().map_err(|e| io_err("resolve", &p, e))
}

/// Removes the empty roots of probes that died; replaying their journals
/// emptied them. A non-empty one is left alone.
fn remove_stale_roots(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry
            .file_name()
            .to_str()
            .is_some_and(|n| n.starts_with(PROBE_ROOT_PREFIX))
        {
            match fs::remove_dir(entry.path()) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => {
                    tracing::debug!("rnk-sandbox: keeping {}: {e}", entry.path().display());
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reaching_exec_is_success() {
        let enoent = Error::Exec {
            program: PROBE_PROGRAM.into(),
            source: io::Error::from_raw_os_error(libc::ENOENT),
        };
        assert!(reached_exec(Err(enoent)).is_ok());
        let eacces = Error::Exec {
            program: PROBE_PROGRAM.into(),
            source: io::Error::from_raw_os_error(libc::EACCES),
        };
        assert!(reached_exec(Err(eacces)).is_err());
        let setup = Error::Setup {
            step: Step::MountProc,
            source: io::Error::from_raw_os_error(libc::EPERM),
        };
        assert!(reached_exec(Err(setup)).is_err());
    }

    #[test]
    fn only_eperm_from_pivot_root_falls_back() {
        let setup = |step, errno| Error::Setup {
            step,
            source: io::Error::from_raw_os_error(errno),
        };
        assert!(is_seccomp_refusal(&setup(Step::PivotRoot, libc::EPERM)));
        assert!(!is_seccomp_refusal(&setup(Step::PivotRoot, libc::EINVAL)));
        assert!(!is_seccomp_refusal(&setup(Step::MountProc, libc::EPERM)));
    }

    #[test]
    fn the_test_process_root_is_a_mount_root() {
        // Linux >= 5.8 reports the attribute; outside a plain `chroot`, `/`
        // is a mount root.
        assert!(slash_is_mount_root().unwrap());
    }

    #[test]
    fn stale_roots_are_removed_only_when_empty() {
        let dir = tempfile::tempdir().unwrap();
        let empty = dir.path().join("probe-1-a");
        let full = dir.path().join("probe-2-b");
        let other = dir.path().join("runs");
        for p in [&empty, &full, &other] {
            fs::create_dir(p).unwrap();
        }
        fs::write(full.join("x"), b"").unwrap();
        remove_stale_roots(dir.path());
        assert!(!empty.exists());
        assert!(full.exists());
        assert!(other.exists());
        let root = create_root(dir.path()).unwrap();
        assert!(root.is_dir());
        assert!(root.is_absolute());
    }

    /// The real sequence: needs `CAP_SYS_ADMIN`, so it runs in the T2 VM.
    #[test]
    #[ignore = "T2: creates namespaces and mounts; run as root in the test VM only"]
    fn probe_reaches_exec_as_root() {
        let dir = tempfile::tempdir().unwrap();
        let caps = probe(dir.path()).unwrap();
        assert_eq!(caps.root_entry, RootEntry::PivotRoot);
        assert_eq!(fs::read_dir(dir.path().join("runs")).unwrap().count(), 0);
    }
}
