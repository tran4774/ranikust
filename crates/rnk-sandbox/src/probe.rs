//! The sandbox's probe: performs the real per-`RUN` sequence once, in an
//! empty root, and reports how a `RUN` can enter its root and get its
//! `proc` and `sysfs` here.
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

/// What the probe found the sandbox can do in this environment. How a
/// `RUN` gets its kernel filesystems only the probe sets:
///
/// ```compile_fail,E0616
/// fn ask_for_an_rbind(mut caps: rnk_sandbox::Caps) {
///     caps.proc = rnk_sandbox::KernelFs::Rbind;
/// }
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Caps {
    /// How a `RUN` enters its root: `pivot_root`, or `MS_MOVE` where seccomp
    /// blocks it.
    pub root_entry: RootEntry,
    // Read-only outside the crate: an rbind only as the probe chose it,
    // `/proc`'s only on request.
    pub(crate) proc: KernelFs,
    pub(crate) sysfs: KernelFs,
}

impl Caps {
    /// How a `RUN` gets its `/proc`.
    #[must_use]
    pub fn proc(&self) -> KernelFs {
        self.proc
    }

    /// How a `RUN` gets its read-only `/sys`.
    #[must_use]
    pub fn sysfs(&self) -> KernelFs {
        self.sysfs
    }
}

/// How a `RUN` gets `proc` or `sysfs`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum KernelFs {
    /// A new instance; for `proc`, that of the `RUN`'s own PID namespace.
    Fresh,
    /// A recursive bind of the one ranikust sees, each of its mounts made
    /// `nosuid`, `nodev` and `noexec` (and read-only for `sysfs`), where a
    /// user namespace may not mount a new one. For `proc`, only with
    /// [`Fallbacks::allow_proc_rbind`].
    Rbind,
}

/// The kernel filesystems [`probe_with_fallbacks`] may bind where a new
/// instance is refused. [`crate::UsernsReport::fallbacks`] allows the
/// read-only rbind of `/sys` the user-namespace probe needed; an rbind of
/// `/proc` only [`Fallbacks::allow_proc_rbind`] allows (ADR-0126). The
/// default allows none.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Fallbacks {
    /// An rbind of `/proc`.
    pub(crate) proc: bool,
    /// A read-only rbind of `/sys`.
    pub(crate) sysfs: bool,
}

impl Fallbacks {
    /// Also allows an rbind of `/proc` where a new `proc` is refused, at the
    /// operator's request only (ADR-0126). Through it a `RUN` sees every
    /// process of the container, reads the command line and mount table of
    /// each, ranikust's included (its build arguments, its secret paths),
    /// and reaches the environment and files of those of its uid, such as a
    /// shell that started ranikust; ranikust itself, not dumpable
    /// (ADR-0051), stays closed to it. [`crate::Sandbox`] then runs its
    /// `RUN`s one at a time, so that none reaches another (ADR-0127). The
    /// rest is an accepted residual: start ranikust as the container's PID 1
    /// with its secrets in its environment, and ask for this only where the
    /// base images are trusted.
    #[must_use]
    pub fn allow_proc_rbind(self) -> Self {
        Self { proc: true, ..self }
    }
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
    probe_in(dir, Fallbacks::default())
}

/// [`probe`] in the build's user namespace: where mounting a new `proc` or
/// `sysfs` is refused with `EPERM` and `allowed` names it, the sequence is
/// tried again with an rbind of it ([`KernelFs::Rbind`]). A new instance
/// is always tried first, so a `RUN` never gets an rbind where a new one
/// mounts; the choice holds for every `RUN` of the sandbox started with
/// these [`Caps`]. An rbind of `/proc` is logged as a warning, once; a new
/// `proc` refused where `allowed` has no rbind of it is final, with a
/// warning that says how to get one.
///
/// # Errors
///
/// As [`probe`]: a refused new `proc` that `allowed` does not let be bound
/// is [`Error::Setup`] at [`Step::MountProc`].
pub fn probe_with_fallbacks(dir: &Path, allowed: Fallbacks) -> Result<Caps> {
    let result = probe_in(dir, allowed);
    if let Err(e) = &result
        && let Some(guidance) = proc_guidance(e, allowed)
    {
        tracing::warn!("{guidance}");
    }
    result
}

/// The warning for a new `proc` refused without the request (`e`, with no
/// rbind of `/proc` in `allowed`): how to get one, or the request and what
/// it exposes.
fn proc_guidance(e: &Error, allowed: Fallbacks) -> Option<&'static str> {
    (is_proc_refusal(e) && !allowed.proc).then_some(
        "rnk-sandbox: a RUN needs a new proc, which this user namespace may not mount \
         (a container whose /proc has masked paths): build as a regular user on a host that \
         allows user namespaces, or in a container where ranikust may mount, or, only with \
         trusted base images, allow an rbind of /proc (--allow proc.rbind), through which \
         each RUN sees this container's processes, reads their command lines, ranikust's \
         included, and reaches the environment and files of those of its uid",
    )
}

fn probe_in(dir: &Path, allowed: Fallbacks) -> Result<Caps> {
    let runner = Runner::new(dir, None)?;
    journal::replay(runner.runs_dir(), runner.state_dir(), None)?;
    remove_stale_roots(dir);
    let root_dir = create_root(dir)?;
    let parent = ParentPid::new()?;
    let result = attempts(dir, &root_dir, parent, allowed);
    // Empty again: the journal removed the stubs.
    if let Err(e) = fs::remove_dir(&root_dir) {
        tracing::warn!("rnk-sandbox: cannot remove {}: {e}", root_dir.display());
    }
    result
}

fn attempts(dir: &Path, root_dir: &Path, parent: ParentPid, allowed: Fallbacks) -> Result<Caps> {
    let mut caps = Caps {
        root_entry: RootEntry::PivotRoot,
        proc: KernelFs::Fresh,
        sysfs: KernelFs::Fresh,
    };
    loop {
        let span = tracing::debug_span!("rnk_sandbox.probe", ok = Empty);
        let runner = Runner::new(dir, Some(caps.root_entry))?
            .with_kernel_fs(caps.proc, caps.sysfs)
            .for_probe();
        let spec = RunSpec::new(root_dir, vec![PROBE_PROGRAM.into()]);
        let outcome =
            span.in_scope(|| reached_exec(runner.run(spec, None, |job| clone_child(job, parent))));
        span.record("ok", outcome.is_ok());
        let e = match outcome {
            Ok(()) => {
                log_rbinds(caps);
                return Ok(caps);
            }
            Err(e) => e,
        };
        let Some(next) = next_attempt(caps, allowed, &e) else {
            return Err(e);
        };
        if next.root_entry == RootEntry::MoveMount && slash_is_mount_root()? != Some(true) {
            // The moved root would sit on a plain directory, which `..`
            // climbs out of, or nobody knows: no fallback.
            return Err(e);
        }
        tracing::debug!("rnk-sandbox: probe: {e}; trying {next:?}");
        caps = next;
    }
}

/// The attempt after `caps` failed with `e`, if any: a new `proc` or
/// `sysfs` refused with `EPERM` that `allowed` lets be bound gets an
/// rbind; `pivot_root` refused with `EPERM`, as Docker's seccomp profile
/// does, `MS_MOVE`. Anything else is final, `pivot_root`'s `EINVAL`
/// where `/` is not a mount point included.
fn next_attempt(caps: Caps, allowed: Fallbacks, e: &Error) -> Option<Caps> {
    let Error::Setup { step, source } = e else {
        return None;
    };
    let eperm = source.raw_os_error() == Some(libc::EPERM);
    // Comparisons, not a match: every other step, and any added later, is
    // final.
    if *step == Step::MountProc && eperm && allowed.proc && caps.proc == KernelFs::Fresh {
        return Some(Caps {
            proc: KernelFs::Rbind,
            ..caps
        });
    }
    if *step == Step::MountSys && eperm && allowed.sysfs && caps.sysfs == KernelFs::Fresh {
        return Some(Caps {
            sysfs: KernelFs::Rbind,
            ..caps
        });
    }
    (is_seccomp_refusal(e) && caps.root_entry == RootEntry::PivotRoot).then_some(Caps {
        root_entry: RootEntry::MoveMount,
        ..caps
    })
}

/// The lines, once per probe, of the rbinds it chose.
fn log_rbinds(caps: Caps) {
    match caps.proc {
        KernelFs::Fresh => {}
        KernelFs::Rbind => tracing::warn!(
            "rnk-sandbox: a new proc is refused here and an rbind of /proc is allowed, so \
             each RUN sees this container's processes, reads their command lines and mount \
             tables, ranikust's included, and reaches the environment and files of those of \
             its uid; RUNs run one at a time. Start ranikust as the container's PID 1, with \
             secrets in its environment, not on its command line"
        ),
    }
    match caps.sysfs {
        KernelFs::Fresh => {}
        KernelFs::Rbind => tracing::info!(
            "rnk-sandbox: a new sysfs is refused here, so each RUN gets a read-only rbind of /sys"
        ),
    }
}

/// A new `proc` refused with `EPERM`, as in the build's user namespace in a
/// container whose `/proc` has masked paths.
fn is_proc_refusal(e: &Error) -> bool {
    matches!(
        e,
        Error::Setup {
            step: Step::MountProc,
            source,
        } if source.raw_os_error() == Some(libc::EPERM)
    )
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
/// directory above (measured by the VM smoke suite). In a plain `chroot`, `/`
/// is not a mount root.
///
/// `None` when the kernel does not report the attribute (it is missing from
/// `stx_attributes_mask`, as in some gVisor versions): unknown, so neither
/// a refusal nor a reason to trust `MS_MOVE`.
pub(crate) fn slash_is_mount_root() -> Result<Option<bool>> {
    let st = rustix::fs::statx(
        rustix::fs::CWD,
        c"/",
        rustix::fs::AtFlags::empty(),
        rustix::fs::StatxFlags::BASIC_STATS,
    )
    .map_err(|e| io_err("statx", "/", e.into()))?;
    Ok(mount_root_attr(st.stx_attributes_mask, st.stx_attributes))
}

/// [`slash_is_mount_root`] from the raw `statx` fields.
fn mount_root_attr(
    mask: rustix::fs::StatxAttributes,
    attributes: rustix::fs::StatxAttributes,
) -> Option<bool> {
    let attr = rustix::fs::StatxAttributes::MOUNT_ROOT;
    mask.contains(attr).then(|| attributes.contains(attr))
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
    fn the_mount_root_attribute_is_unknown_unless_in_the_mask() {
        use rustix::fs::StatxAttributes as A;
        assert_eq!(mount_root_attr(A::empty(), A::MOUNT_ROOT), None);
        assert_eq!(mount_root_attr(A::MOUNT_ROOT, A::MOUNT_ROOT), Some(true));
        assert_eq!(mount_root_attr(A::MOUNT_ROOT, A::empty()), Some(false));
    }

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
    fn a_kernel_fs_is_bound_only_when_allowed_and_a_new_one_is_refused() {
        let setup = |step, errno| Error::Setup {
            step,
            source: io::Error::from_raw_os_error(errno),
        };
        let fresh = Caps {
            root_entry: RootEntry::PivotRoot,
            proc: KernelFs::Fresh,
            sysfs: KernelFs::Fresh,
        };
        let both = Fallbacks {
            proc: true,
            sysfs: true,
        };
        let proc_eperm = setup(Step::MountProc, libc::EPERM);
        // `probe`: nothing allowed, a refused mount is final.
        assert_eq!(next_attempt(fresh, Fallbacks::default(), &proc_eperm), None);
        let rbind = next_attempt(fresh, both, &proc_eperm).unwrap();
        assert_eq!(rbind.proc, KernelFs::Rbind);
        assert_eq!(rbind.sysfs, KernelFs::Fresh);
        // Tried once: the rbind refused too is final.
        assert_eq!(next_attempt(rbind, both, &proc_eperm), None);
        // Another errno is not the user namespace's refusal.
        let proc_einval = setup(Step::MountProc, libc::EINVAL);
        assert_eq!(next_attempt(fresh, both, &proc_einval), None);
        let only_proc = Fallbacks {
            proc: true,
            sysfs: false,
        };
        let sys_eperm = setup(Step::MountSys, libc::EPERM);
        assert_eq!(next_attempt(rbind, only_proc, &sys_eperm), None);
        let rbinds = next_attempt(rbind, both, &sys_eperm).unwrap();
        assert_eq!(
            (rbinds.proc, rbinds.sysfs),
            (KernelFs::Rbind, KernelFs::Rbind)
        );
        // `MS_MOVE` keeps the kernel filesystems chosen, and is tried once.
        let pivot = setup(Step::PivotRoot, libc::EPERM);
        let moved = next_attempt(rbinds, both, &pivot).unwrap();
        assert_eq!(
            moved,
            Caps {
                root_entry: RootEntry::MoveMount,
                ..rbinds
            }
        );
        assert_eq!(next_attempt(moved, both, &pivot), None);
        assert_eq!(next_attempt(fresh, both, &Error::Privilege("x")), None);
    }

    #[test]
    fn proc_is_bound_only_on_request_else_refused_with_guidance() {
        let setup = |step, errno| Error::Setup {
            step,
            source: io::Error::from_raw_os_error(errno),
        };
        let fresh = Caps {
            root_entry: RootEntry::PivotRoot,
            proc: KernelFs::Fresh,
            sysfs: KernelFs::Fresh,
        };
        // What `UsernsReport::fallbacks` gives where both were refused.
        let report = Fallbacks {
            proc: false,
            sysfs: true,
        };
        let proc_eperm = setup(Step::MountProc, libc::EPERM);
        assert_eq!(next_attempt(fresh, report, &proc_eperm), None);
        assert!(is_proc_refusal(&proc_eperm));
        assert!(!is_proc_refusal(&setup(Step::MountProc, libc::EINVAL)));
        assert!(!is_proc_refusal(&setup(Step::MountSys, libc::EPERM)));
        let sysfs = next_attempt(fresh, report, &setup(Step::MountSys, libc::EPERM)).unwrap();
        assert_eq!(
            (sysfs.proc, sysfs.sysfs),
            (KernelFs::Fresh, KernelFs::Rbind)
        );
        let asked = report.allow_proc_rbind();
        assert_eq!(
            asked,
            Fallbacks {
                proc: true,
                sysfs: true
            }
        );
        assert_eq!(
            next_attempt(fresh, asked, &proc_eperm).unwrap().proc,
            KernelFs::Rbind
        );
        assert!(Fallbacks::default().allow_proc_rbind().proc);
        assert!(!Fallbacks::default().allow_proc_rbind().sysfs);
        // The way out, only where the refusal was final for want of a request.
        let guidance = proc_guidance(&proc_eperm, report).unwrap();
        assert!(guidance.contains("allows user namespaces"), "{guidance}");
        assert!(guidance.contains("--allow proc.rbind"), "{guidance}");
        assert!(guidance.contains("processes"), "{guidance}");
        assert_eq!(proc_guidance(&proc_eperm, asked), None);
        assert_eq!(
            proc_guidance(&setup(Step::MountSys, libc::EPERM), report),
            None
        );
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

    /// `--network=none` through a held network namespace, the path of an
    /// environment without `CAP_NET_ADMIN` (ADR-0033): the child joins it
    /// and mounts `sysfs` for it, then reaches `execve`.
    #[test]
    #[ignore = "T2: creates namespaces and mounts; run as root in the test VM only"]
    fn a_run_joins_a_held_network_namespace() {
        let dir = tempfile::tempdir().unwrap();
        let root_dir = create_root(dir.path()).unwrap();
        let runner = Runner::new(dir.path(), Some(RootEntry::PivotRoot)).unwrap();
        runner.hold_networks();
        let mut spec = RunSpec::new(&root_dir, vec![PROBE_PROGRAM.into()]);
        spec.network = crate::spec::Network::None;
        let parent = ParentPid::new().unwrap();
        reached_exec(runner.run(spec, None, |job| clone_child(job, parent))).unwrap();
        assert_eq!(fs::read_dir(dir.path().join("runs")).unwrap().count(), 0);
    }
}
