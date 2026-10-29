//! The crate's error type, and the steps a `RUN` child reports failures from.

use std::fmt;
use std::io;
use std::path::PathBuf;

/// One step of the per-`RUN` sequence (`design/sandbox.md`), as reported by
/// a child that failed before `execve`.
///
/// The child writes the step's code and the errno to a pipe; the codes are
/// private to this crate and only need to agree between parent and child of
/// the same binary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Step {
    /// `prctl(PR_SET_PDEATHSIG)`.
    DeathSignal,
    /// The parent died before the death signal was armed.
    ParentGone,
    /// Making every mount of the child private.
    PrivateMounts,
    /// Binding the stage root onto itself.
    BindRoot,
    /// Entering the stage root before `pivot_root`.
    EnterRoot,
    /// Resolving a mount target inside the root.
    ResolveTarget,
    /// Mounting `proc`.
    MountProc,
    /// Mounting a read-only `sysfs`.
    MountSys,
    /// Mounting the tmpfs on `/dev`.
    MountDev,
    /// Creating and binding a device node in `/dev`.
    DevNode,
    /// Mounting `devpts` or `/dev/shm`.
    DevMounts,
    /// Creating the `/dev` symlinks.
    DevSymlink,
    /// Binding a file copy (`resolv.conf`, `hosts`, `hostname`).
    BindFile,
    /// Binding a `RUN --mount`.
    BindMount,
    /// Remounting a bind read-only.
    Remount,
    /// `pivot_root(".", ".")`.
    PivotRoot,
    /// Detaching the old root after `pivot_root`.
    DetachOldRoot,
    /// `mount(root, "/", MS_MOVE)`, the fallback.
    MoveRoot,
    /// `chroot(".")` after `MS_MOVE`.
    ChrootAfterMove,
    /// Bringing up the loopback interface of a `--network=none` namespace.
    Loopback,
    /// Dropping capabilities from the bounding set.
    BoundingSet,
    /// Clearing the ambient capabilities.
    AmbientSet,
    /// Setting the effective, permitted and inheritable capabilities.
    CapabilitySets,
    /// `setgroups`.
    Groups,
    /// `setresgid`.
    Gid,
    /// `setresuid`.
    Uid,
    /// `chdir` to `WORKDIR`.
    Workdir,
    /// Wiring stdin, stdout and stderr.
    Stdio,
    /// `execve` of the command.
    Exec,
    /// `setpgid` of an [`Isolation::Direct`](crate::Isolation::Direct)
    /// command, so its leftovers can be killed.
    ProcessGroup,
}

impl Step {
    const ALL: [Step; 30] = [
        Step::DeathSignal,
        Step::ParentGone,
        Step::PrivateMounts,
        Step::BindRoot,
        Step::EnterRoot,
        Step::ResolveTarget,
        Step::MountProc,
        Step::MountSys,
        Step::MountDev,
        Step::DevNode,
        Step::DevMounts,
        Step::DevSymlink,
        Step::BindFile,
        Step::BindMount,
        Step::Remount,
        Step::PivotRoot,
        Step::DetachOldRoot,
        Step::MoveRoot,
        Step::ChrootAfterMove,
        Step::Loopback,
        Step::BoundingSet,
        Step::AmbientSet,
        Step::CapabilitySets,
        Step::Groups,
        Step::Gid,
        Step::Uid,
        Step::Workdir,
        Step::Stdio,
        Step::Exec,
        Step::ProcessGroup,
    ];

    /// The wire code of the step: its position in [`Step::ALL`].
    pub(crate) fn code(self) -> u32 {
        let index = Self::ALL
            .iter()
            .position(|s| *s == self)
            .unwrap_or(Self::ALL.len());
        u32::try_from(index).unwrap_or(u32::MAX)
    }

    /// The step a wire code names, if any.
    pub(crate) fn from_code(code: u32) -> Option<Step> {
        usize::try_from(code)
            .ok()
            .and_then(|i| Self::ALL.get(i).copied())
    }

    fn name(self) -> &'static str {
        match self {
            Step::DeathSignal => "set the parent-death signal",
            Step::ParentGone => "parent died during set-up",
            Step::PrivateMounts => "make mounts private",
            Step::BindRoot => "bind the root onto itself",
            Step::EnterRoot => "enter the root",
            Step::ResolveTarget => "resolve a mount target in the root",
            Step::MountProc => "mount proc",
            Step::MountSys => "mount sysfs",
            Step::MountDev => "mount /dev",
            Step::DevNode => "bind a device node",
            Step::DevMounts => "mount devpts or /dev/shm",
            Step::DevSymlink => "create a /dev symlink",
            Step::BindFile => "bind resolv.conf, hosts or hostname",
            Step::BindMount => "bind a RUN --mount",
            Step::Remount => "remount a bind read-only",
            Step::PivotRoot => "pivot_root",
            Step::DetachOldRoot => "detach the old root",
            Step::MoveRoot => "move the root (MS_MOVE)",
            Step::ChrootAfterMove => "chroot after MS_MOVE",
            Step::Loopback => "bring up the loopback interface",
            Step::BoundingSet => "drop the capability bounding set",
            Step::AmbientSet => "clear ambient capabilities",
            Step::CapabilitySets => "set capabilities",
            Step::Groups => "set supplementary groups",
            Step::Gid => "set the group id",
            Step::Uid => "set the user id",
            Step::Workdir => "change to the working directory",
            Step::Stdio => "set up stdin, stdout and stderr",
            Step::Exec => "execute the command",
            Step::ProcessGroup => "create a process group",
        }
    }
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Errors of the sandbox.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The `RUN` child failed at a step before `execve`.
    #[error("RUN sandbox: {step}: {source}")]
    Setup {
        /// The failing step.
        step: Step,
        /// The errno it failed with.
        #[source]
        source: io::Error,
    },
    /// The command could not be executed: not found on `PATH`, not
    /// executable, or not a valid binary.
    #[error("RUN: cannot execute {program:?}: {source}")]
    Exec {
        /// `argv[0]`.
        program: String,
        /// The errno of the last `execve` attempt that mattered.
        #[source]
        source: io::Error,
    },
    /// The `RUN` specification is invalid.
    #[error("invalid RUN specification: {0}")]
    InvalidSpec(String),
    /// A step needs privileges the process does not have.
    #[error("{0} needs privileges this process does not have")]
    Privilege(&'static str),
    /// [`crate::enter_build_namespace`] was called with other threads alive.
    #[error("the process must be single-threaded here, but has {threads} threads")]
    NotSingleThreaded {
        /// Threads counted in `/proc/self/status`.
        threads: u64,
    },
    /// The spawner thread is gone.
    #[error("the sandbox spawner thread has stopped")]
    SpawnerGone,
    /// A filesystem operation inside the root failed.
    #[error(transparent)]
    Rootfs(#[from] rnk_rootfs::Error),
    /// A host-side I/O operation failed.
    #[error("{op} {path:?}: {source}")]
    Io {
        /// What was being done.
        op: &'static str,
        /// The host path involved.
        path: PathBuf,
        /// The cause.
        #[source]
        source: io::Error,
    },
    /// A system call of the parent failed.
    #[error("{op}: {source}")]
    Sys {
        /// The system call.
        op: &'static str,
        /// The cause.
        #[source]
        source: io::Error,
    },
}

/// The crate's result type.
pub type Result<T, E = Error> = std::result::Result<T, E>;

pub(crate) fn io_err(op: &'static str, path: impl Into<PathBuf>, source: io::Error) -> Error {
    Error::Io {
        op,
        path: path.into(),
        source,
    }
}

pub(crate) fn sys_err(op: &'static str, source: impl Into<io::Error>) -> Error {
    Error::Sys {
        op,
        source: source.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_codes_round_trip() {
        for step in Step::ALL {
            assert_eq!(Step::from_code(step.code()), Some(step));
        }
        assert_eq!(Step::from_code(u32::MAX), None);
        let len = u32::try_from(Step::ALL.len()).unwrap();
        assert_eq!(Step::from_code(len), None);
    }

    #[test]
    fn setup_error_names_step_and_errno() {
        let e = Error::Setup {
            step: Step::PivotRoot,
            source: io::Error::from_raw_os_error(libc::EPERM),
        };
        let text = e.to_string();
        assert!(text.contains("pivot_root"), "{text}");
        assert!(text.contains("not permitted"), "{text}");
    }
}
