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
    /// Remounting a bind `nodev` or read-only.
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
    /// `setsid`: no controlling terminal, and a process group of its own.
    Session,
    /// Making a `/proc` path read-only.
    ReadonlyPath,
    /// Masking a `/proc` or `/sys` path.
    MaskPath,
    /// Joining a new session keyring (`Direct`).
    SessionKeyring,
    /// The same join refused while ranikust's session keyring can still be
    /// looked up and the `RUN`'s profile lets `keyctl`, `add_key` or
    /// `request_key` run (ADR-0109).
    KeyringRefused,
    /// Writing and binding a `RUN --mount=type=secret` file on its tmpfs.
    Secret,
    /// Creating the network namespace of `--network=none` in the helper
    /// that holds it, with its own user namespace where needed.
    NetNamespace,
    /// Joining that network namespace.
    JoinNetwork,
    /// Installing the `RUN`'s seccomp profile.
    Seccomp,
    /// Installing the seccomp filter of a `RUN` in such a namespace
    /// (ADR-0033).
    NetworkFilter,
    /// `chroot` into a root other than `/` (`Isolation::Direct`).
    Chroot,
    /// Giving the command the `RLIMIT_NOFILE` ranikust started with.
    FileLimit,
    /// Giving the command the `RLIMIT_CORE` ranikust started with.
    CoreLimit,
    /// Starting the `RUN`'s init, PID 1 of its namespace, which forks the
    /// command and gives up its own capabilities (ADR-0080).
    Init,
    /// Moving the `RUN`'s init or command into its cgroup (ADR-0110).
    JoinCgroup,
    /// Making the command's cgroup namespace, rooted at its cgroup
    /// (ADR-0110).
    CgroupNamespace,
}

impl Step {
    const ALL: [Step; 45] = [
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
        Step::Session,
        Step::ReadonlyPath,
        Step::MaskPath,
        Step::SessionKeyring,
        Step::KeyringRefused,
        Step::Secret,
        Step::NetNamespace,
        Step::JoinNetwork,
        Step::Seccomp,
        Step::NetworkFilter,
        Step::Chroot,
        Step::FileLimit,
        Step::CoreLimit,
        Step::Init,
        Step::JoinCgroup,
        Step::CgroupNamespace,
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
            Step::Remount => "remount a bind nodev or read-only",
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
            Step::Session => "start a new session",
            Step::ReadonlyPath => "make a /proc path read-only",
            Step::MaskPath => "mask a /proc or /sys path",
            Step::SessionKeyring => "join a new session keyring",
            Step::KeyringRefused => {
                "join a new session keyring, refused while ranikust's could still be looked up \
                 (a RUN goes on without one only under a seccomp profile that refuses keyctl, \
                 add_key and request_key, as --seccomp=default does)"
            }
            Step::Secret => "write and bind a secret file",
            Step::NetNamespace => "create the network namespace",
            Step::JoinNetwork => "join the network namespace",
            Step::Seccomp => {
                "install the RUN's seccomp profile (--seccomp=unconfined runs RUN without one)"
            }
            Step::NetworkFilter => {
                "install the seccomp filter of RUN --network=none in a user namespace \
                 (--allow network.none-userns, ADR-0033)"
            }
            Step::Chroot => "chroot into the root",
            Step::FileLimit => "set RLIMIT_NOFILE",
            Step::CoreLimit => "set RLIMIT_CORE",
            Step::Init => "start the RUN's init",
            Step::JoinCgroup => "join the RUN's cgroup",
            Step::CgroupNamespace => "make the RUN's cgroup namespace",
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
    /// The parent could not make the `RUN`'s cgroups in the delegated tree,
    /// before the clone (ADR-0110).
    #[error("RUN sandbox: cannot {op} {path:?}: {source}")]
    Cgroup {
        /// The step that failed.
        op: &'static str,
        /// The cgroup it failed in.
        path: PathBuf,
        /// The errno.
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
    /// [`crate::enter_build_namespace`] or
    /// [`crate::trim_to_run_capabilities`] was called with other threads
    /// alive.
    #[error("the process must be single-threaded here, but has {threads} threads")]
    NotSingleThreaded {
        /// Threads counted in `/proc/self/status`.
        threads: u64,
    },
    /// `/` is not the root of a mount: the process runs inside a plain
    /// `chroot`, where the build namespace cannot be set up and `MS_MOVE`
    /// would let a `RUN` climb out of its root.
    #[error(
        "/ is not a mount point: ranikust runs inside a plain chroot, where it \
         cannot isolate RUN; run it on the host or in a container, or \
         bind-mount the chroot directory onto itself before entering it"
    )]
    InPlainChroot,
    /// `RUN --network=none` cannot bring up loopback: the process lacks
    /// `CAP_NET_ADMIN` over a new network namespace, and the user-namespace
    /// path is not allowed (ADR-0033).
    #[error(
        "RUN --network=none: cannot bring up the loopback interface without \
         CAP_NET_ADMIN ({source}); give ranikust CAP_NET_ADMIN, or pass \
         --allow network.none-userns to use a user namespace under a seccomp \
         filter (ADR-0033)"
    )]
    NoLoopback {
        /// Why the network namespace failed without a user namespace.
        source: io::Error,
    },
    /// `RUN --network=none` cannot bring up loopback, with
    /// `--allow network.none-userns` either: user namespaces, or joining one
    /// by `setns`, are denied (ADR-0033).
    #[error(
        "RUN --network=none: cannot bring up the loopback interface: without \
         CAP_NET_ADMIN ({direct}), and through a user namespace ({userns}); \
         give ranikust CAP_NET_ADMIN, or allow user namespaces"
    )]
    NoLoopbackUserns {
        /// Why the network namespace failed without a user namespace.
        direct: io::Error,
        /// Why the user-namespace path failed.
        userns: io::Error,
    },
    /// A `Direct` `RUN` on `/` would keep a capability outside Docker's
    /// defaults (any may reach ranikust's state: ptrace, a mount over
    /// `/proc`, BPF, modules, raw I/O), and ranikust lacks `CAP_SETPCAP` to
    /// drop it (ADR-0051).
    #[error(
        "RUN directly on /: the container grants {caps}, beyond Docker's defaults, which \
         a RUN could use to reach ranikust's state and credentials, and ranikust lacks \
         CAP_SETPCAP to take them away from the RUN; start the container without them, \
         or with CAP_SETPCAP"
    )]
    RunReachesState {
        /// The capabilities, comma-separated.
        caps: String,
    },
    /// A `Direct` `RUN` on `/` (kaniko mode where nothing can be mounted)
    /// was asked for what needs a mount or a namespace: a `RUN --mount`, a
    /// secret file, `--network=none`, or a `#!` heredoc where no anonymous
    /// script can run. Refused rather than run without it (ADR-0057).
    #[error("RUN directly on / (kaniko mode, where nothing can be mounted): {0}")]
    Direct(String),
    /// The `RUN` left processes that kept appearing after every kill: a
    /// fork loop faster than the sweep (ADR-0051).
    #[error("RUN left processes that could not all be killed ({left} still alive)")]
    Survivors {
        /// How many were alive after the last round.
        left: usize,
    },
    /// A seccomp profile is malformed or asks for what ranikust does not
    /// support (ADR-0109); nothing ran.
    #[error("seccomp profile: {0}")]
    SeccompProfile(String),
    /// The full sandbox's init, which sets the `RUN` up and then waits for
    /// its command, was killed by a signal before it wrote the command's
    /// status: before or after the go byte, so the command may have run,
    /// and how it ended is unknown. Under a seccomp profile, `SIGSYS` is
    /// most likely the profile's kill or trap of a call the init makes.
    #[error(
        "RUN sandbox: the RUN's init was killed by {} before it reported the command's end, \
         so the command may have started{}",
        signal_text(*signal),
        if *seccomp {
            "; the RUN's seccomp profile is the likely cause: it kills or traps a call \
             the init makes"
        } else {
            ""
        }
    )]
    SetupKilled {
        /// The signal.
        signal: i32,
        /// The signal is `SIGSYS` and the `RUN` had a seccomp profile.
        seccomp: bool,
    },
    /// The full sandbox's init exited with a code other than 0 before it
    /// wrote the command's status, and left no failure record: a step that
    /// could not go on (its go byte, its wait), or a record its filters
    /// refused. The code is the init's own; the command may have run.
    #[error(
        "RUN sandbox: the RUN's init exited with code {code} before it reported the \
         command's end, so the command may have started; the code is the init's own, \
         not the command's"
    )]
    InitExited {
        /// The init's exit code.
        code: i32,
    },
    /// The spawner thread is gone.
    #[error("the sandbox spawner thread has stopped")]
    SpawnerGone,
    /// [`crate::Sandbox::cancel`] was called: the `RUN` was killed, or was
    /// not started.
    #[error("RUN cancelled")]
    Cancelled,
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

impl Error {
    /// Whether the error is certain to have come before the `RUN`'s
    /// command ran: no process of the `RUN` can outlive it. False for
    /// [`Error::Cancelled`] (it may have killed a running `RUN`) and any
    /// error after the start, whose sweep may not have finished.
    ///
    /// This rests on the spawner's order (`spawner.rs`, `execute`): the
    /// spec and privileges are checked before the fork; the child reports
    /// [`Error::Setup`] or [`Error::Exec`] only from before `execve`, the
    /// full sandbox's init included, whose command starts only on the go
    /// byte the init writes after its last step (`child.rs`, `run_init`),
    /// and the parent returns them only after the process tree is swept
    /// (`Survivors` otherwise), and [`Error::Cgroup`] before the clone. A
    /// variant returned after the command starts must not be added here.
    #[must_use]
    pub fn ran_no_command(&self) -> bool {
        matches!(
            self,
            Self::Setup { .. }
                | Self::Cgroup { .. }
                | Self::Exec { .. }
                | Self::InvalidSpec(_)
                | Self::Privilege(_)
                | Self::NotSingleThreaded { .. }
                | Self::InPlainChroot
                | Self::NoLoopback { .. }
                | Self::NoLoopbackUserns { .. }
                | Self::RunReachesState { .. }
                | Self::Direct(_)
        )
    }
}

/// The crate's result type.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// `signal 31 (SIGSYS)`: the number, and the name of a signal that ends a
/// process by default.
fn signal_text(signal: i32) -> String {
    let name = match signal {
        libc::SIGHUP => "SIGHUP",
        libc::SIGINT => "SIGINT",
        libc::SIGQUIT => "SIGQUIT",
        libc::SIGILL => "SIGILL",
        libc::SIGTRAP => "SIGTRAP",
        libc::SIGABRT => "SIGABRT",
        libc::SIGBUS => "SIGBUS",
        libc::SIGFPE => "SIGFPE",
        libc::SIGKILL => "SIGKILL",
        libc::SIGUSR1 => "SIGUSR1",
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGUSR2 => "SIGUSR2",
        libc::SIGPIPE => "SIGPIPE",
        libc::SIGALRM => "SIGALRM",
        libc::SIGTERM => "SIGTERM",
        libc::SIGXCPU => "SIGXCPU",
        libc::SIGXFSZ => "SIGXFSZ",
        libc::SIGSYS => "SIGSYS",
        _ => return format!("signal {signal}"),
    };
    format!("signal {signal} ({name})")
}

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

    /// A filter that fails to install is named, the profile's failure says
    /// how to run without it, and a keyring join an LSM refused which
    /// profile lets the `RUN` go on; any other failed join names no flag.
    #[test]
    fn a_failed_filter_install_names_the_filter() {
        let text = |step| {
            Error::Setup {
                step,
                source: io::Error::from_raw_os_error(libc::EINVAL),
            }
            .to_string()
        };
        let profile = text(Step::Seccomp);
        assert!(profile.contains("seccomp profile"), "{profile}");
        assert!(profile.contains("--seccomp=unconfined"), "{profile}");
        let network = text(Step::NetworkFilter);
        assert!(network.contains("network.none-userns"), "{network}");
        assert!(!network.contains("--seccomp=unconfined"), "{network}");
        let refused = text(Step::KeyringRefused);
        assert!(refused.contains("--seccomp=default"), "{refused}");
        let keyring = text(Step::SessionKeyring);
        assert!(!keyring.contains("--seccomp"), "{keyring}");
    }

    /// An init a signal killed is named with the signal, by number and
    /// name; the profile is named as the likely cause only for `SIGSYS`
    /// under one. Nothing reads as the command's exit code, nor says
    /// whether the command ran.
    #[test]
    fn a_killed_init_names_its_signal_and_the_likely_cause() {
        let text = |signal, seccomp| Error::SetupKilled { signal, seccomp }.to_string();
        let by_profile = text(libc::SIGSYS, true);
        assert!(
            by_profile.contains(&format!(
                "init was killed by signal {} (SIGSYS) before it reported the command's end",
                libc::SIGSYS
            )),
            "{by_profile}"
        );
        assert!(
            by_profile.contains("seccomp profile is the likely cause"),
            "{by_profile}"
        );
        let other = text(libc::SIGKILL, false);
        assert!(
            other.contains(&format!("killed by signal {} (SIGKILL)", libc::SIGKILL)),
            "{other}"
        );
        assert!(!other.contains("seccomp"), "{other}");
        let unnamed = text(libc::SIGRTMIN() + 1, false);
        assert!(
            unnamed.contains(&format!("killed by signal {} before", libc::SIGRTMIN() + 1)),
            "{unnamed}"
        );
        for t in [by_profile, other, unnamed] {
            assert!(!t.contains("code"), "{t}");
            assert!(t.contains("the command may have started"), "{t}");
        }
        let e = Error::SetupKilled {
            signal: libc::SIGSYS,
            seccomp: true,
        };
        assert!(!e.ran_no_command(), "the command may have run");
    }

    /// An init's own exit code is named as the init's, never as the
    /// command's, and says nothing of whether the command ran.
    #[test]
    fn an_inits_own_code_is_not_the_commands() {
        let e = Error::InitExited { code: 125 };
        let t = e.to_string();
        assert!(t.contains("init exited with code 125"), "{t}");
        assert!(t.contains("the command may have started"), "{t}");
        assert!(t.contains("the code is the init's own"), "{t}");
        assert!(!t.contains("non-zero code"), "{t}");
        assert!(!e.ran_no_command(), "the command may have run");
    }
}
