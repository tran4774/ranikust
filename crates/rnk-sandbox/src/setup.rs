//! Process set-up, once per build, while the process is single-threaded.

use rustix::io::Errno;
use rustix::mount::MountPropagationFlags;
use rustix::thread::{CapabilitySet, CapabilitySets, UnshareFlags};

use crate::caps;
use crate::error::{Error, Result, sys_err};

/// Moves the process into a private mount namespace whose `/` is a slave of
/// the original: mounts propagate in, none made by the build propagate out,
/// and all of them disappear with the process.
///
/// Call before any thread exists (before tokio or rayon start): in a
/// multi-threaded process `unshare(CLONE_NEWNS)` silently moves only the
/// calling thread.
///
/// # Errors
///
/// [`Error::NotSingleThreaded`] when other threads exist,
/// [`Error::InPlainChroot`] when `/` is not the root of a mount,
/// [`Error::Privilege`] without `CAP_SYS_ADMIN`, [`Error::Sys`] otherwise.
pub fn enter_build_namespace() -> Result<()> {
    let threads = thread_count()?;
    if threads != 1 {
        return Err(Error::NotSingleThreaded { threads });
    }
    // In a plain `chroot`, making `/` a slave mount fails with `EINVAL`
    // after the `unshare`: say why before changing anything. Only a definite
    // "no"; where the kernel does not tell, the `mount_change` below decides.
    if crate::probe::slash_is_mount_root()? == Some(false) {
        return Err(Error::InPlainChroot);
    }
    // SAFETY: `NEWNS` does not unshare the descriptor table, the only flag
    // `unshare_unsafe` is unsafe for; the process is single-threaded.
    match unsafe { rustix::thread::unshare_unsafe(UnshareFlags::NEWNS) } {
        Ok(()) => {}
        Err(Errno::PERM) => return Err(Error::Privilege("a private mount namespace")),
        Err(e) => return Err(sys_err("unshare(CLONE_NEWNS)", e)),
    }
    // `DOWNSTREAM` is `MS_SLAVE`.
    rustix::mount::mount_change(
        c"/",
        MountPropagationFlags::DOWNSTREAM | MountPropagationFlags::REC,
    )
    .map_err(|e| sys_err("make / a recursive slave mount", e))
}

/// Moves the process into a user namespace of the build's own, then into a
/// mount namespace that namespace owns, whose `/` is a recursive slave as
/// [`enter_build_namespace`]'s: the whole build runs as root of it
/// (`design/modes-and-detection.md`). Root's build without one keeps
/// [`enter_build_namespace`]: in a new user namespace the kernel refuses a
/// new `proc` where `/proc` is masked.
///
/// The map is the first of [`crate::probe_user_namespace`]'s order whose
/// steps pass here, each tried in a forked child first, since a map that
/// failed would leave this process in the namespace for good. Root maps
/// every id it has, each to itself ([`crate::IdMap::Ranges`], written by a
/// helper forked before the `unshare`); another user maps its own ids as 0
/// and `subordinate` from 1 through `newuidmap` and `newgidmap`
/// ([`crate::IdMap::Newuidmap`]); else one id, its own as 0, with
/// `setgroups` denied ([`crate::IdMap::OneId`], ADR-0098). The process is
/// dumpable only while its map is written (ADR-0051). The map and
/// `setgroups` returned are read back from `/proc/self` once inside: a map
/// that gives one id is [`crate::IdMap::OneId`], whichever was written.
/// Whether the process's uid in the initial user namespace is 0 is
/// recorded first, from the map it starts under: a `RUN`'s cgroups trust
/// `user.delegate` only where it is not (ADR-0135).
///
/// Call before any thread exists, and before any credential is read: each
/// trial's child is a copy of this process. Afterwards the effective uid is
/// 0 and the host's ids show as the map gives them: read anything that
/// depends on the ids outside first.
///
/// # Errors
///
/// [`Error::NotSingleThreaded`], [`Error::InPlainChroot`] and
/// [`Error::NoUserNamespace`] before any change; a host-side error (a pipe,
/// a fork) or a refused `unshare` that leave the process as it was;
/// [`Error::UserNamespaceBroken`] when a step failed once the process was
/// in the new user namespace: it must end.
pub fn enter_build_user_namespace(
    subordinate: Option<crate::SubordinateIds>,
) -> Result<crate::BuildUserNamespace> {
    require_single_thread()?;
    // As for the mount namespace alone: say why before changing anything.
    if crate::probe::slash_is_mount_root()? == Some(false) {
        return Err(Error::InPlainChroot);
    }
    // Which delegation marks a `RUN`'s cgroups trust depends on the uid
    // ranikust has here, which the new namespace's map hides (ADR-0135).
    crate::cgroup::record_kernel_root();
    crate::userns::enter_build(subordinate)
}

/// Kaniko mode on `/` (ADR-0078): gives ranikust's own process the
/// capability sets a `Direct` `RUN` keeps, so that nothing ranikust does on
/// `/` after a `RUN` (the outputs, the metrics report, the clean-up) uses an
/// authority the `RUN` was denied, such as `CAP_SYS_PTRACE` on a path the
/// `RUN` planted.
///
/// The bounding set goes to Docker's defaults where `CAP_SETPCAP` allows it
/// (else the build is refused, as ADR-0051 refuses the `RUN`), the ambient
/// and inheritable sets are cleared, and the permitted and effective sets
/// keep the permitted capabilities among the defaults. Then the process is
/// made not dumpable again, as ADR-0051 asks after any credential change,
/// and the sets are read back.
///
/// Call it once, from the binary, while the process is single-threaded and
/// before the first `RUN`: capabilities are per thread, and every thread
/// and child made later inherits these sets. A process that is not root
/// keeps its sets, as the `RUN` child's drop does.
///
/// # Errors
///
/// [`Error::NotSingleThreaded`] when other threads exist (checked first,
/// before any change); [`Error::RunReachesState`] when the bounding set
/// holds capabilities outside the defaults and `CAP_SETPCAP` is missing;
/// [`Error::Sys`] when a system call fails or a capability outside the
/// defaults is left.
pub fn trim_to_run_capabilities() -> Result<()> {
    let threads = thread_count()?;
    if threads != 1 {
        return Err(Error::NotSingleThreaded { threads });
    }
    if !rustix::process::geteuid().is_root() {
        return Ok(());
    }
    let kernel_last = caps::kernel_last_cap();
    let last = caps::last_cap_of(kernel_last);
    let defaults = caps::docker_default_mask();
    let sets =
        rustix::thread::capabilities(None).map_err(|e| sys_err("read the capability sets", e))?;
    let setpcap = sets.effective.contains(CapabilitySet::SETPCAP);
    match caps::direct_caps(caps::bounding_set(last), setpcap) {
        caps::DirectCaps::Keep => {}
        caps::DirectCaps::Drop => caps::BoundingDrops::new(kernel_last)
            .drop_all()
            .map_err(|e| sys_err("drop a capability from the bounding set", e))?,
        caps::DirectCaps::Refuse(names) => {
            return Err(Error::RunReachesState {
                caps: names.join(", "),
            });
        }
    }
    match rustix::thread::clear_ambient_capability_set() {
        // Kernels before 4.3 have no ambient set.
        Ok(()) | Err(Errno::INVAL) => {}
        Err(e) => return Err(sys_err("clear the ambient capability set", e)),
    }
    let keep = sets.permitted & CapabilitySet::from_bits_retain(defaults);
    rustix::thread::set_capabilities(
        None,
        CapabilitySets {
            effective: keep,
            permitted: keep,
            inheritable: CapabilitySet::empty(),
        },
    )
    .map_err(|e| sys_err("set the capability sets", e))?;
    // Lowering the sets does not reset the flag (only a gain does), but
    // ADR-0051 sets it again after any credential change.
    rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)
        .map_err(|e| sys_err("prctl(PR_SET_DUMPABLE, 0)", e))?;
    let after =
        rustix::thread::capabilities(None).map_err(|e| sys_err("read the capability sets", e))?;
    let left = trimmed_too_little(
        (after.permitted | after.effective).bits(),
        after.inheritable.bits(),
        caps::bounding_set(last),
    );
    if left {
        return Err(sys_err(
            "check the capability sets",
            std::io::Error::other(
                "a capability outside Docker's defaults is still held after the trim",
            ),
        ));
    }
    Ok(())
}

/// Whether sets read back after [`trim_to_run_capabilities`] still hold
/// anything a `Direct` `RUN` lacks: a permitted or effective capability, or
/// one in the bounding set, outside Docker's defaults, or any inheritable
/// one.
fn trimmed_too_little(held: u64, inheritable: u64, bounding: u64) -> bool {
    let outside = !caps::docker_default_mask();
    held & outside != 0 || bounding & outside != 0 || inheritable != 0
}

/// [`Error::NotSingleThreaded`] unless this process has one thread.
pub(crate) fn require_single_thread() -> Result<()> {
    match thread_count()? {
        1 => Ok(()),
        threads => Err(Error::NotSingleThreaded { threads }),
    }
}

/// Threads of this process, from `/proc/self/status`.
fn thread_count() -> Result<u64> {
    let status = std::fs::read_to_string("/proc/self/status")
        .map_err(|e| crate::error::io_err("read", "/proc/self/status", e))?;
    parse_threads(&status).ok_or_else(|| {
        sys_err(
            "read the thread count",
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "no Threads: line in /proc/self/status",
            ),
        )
    })
}

fn parse_threads(status: &str) -> Option<u64> {
    status
        .lines()
        .find_map(|l| l.strip_prefix("Threads:"))
        .and_then(|v| v.trim().parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threads_line_is_parsed() {
        let status = "Name:\tranikust\nPid:\t1\nThreads:\t3\nVmRSS:\t1 kB\n";
        assert_eq!(parse_threads(status), Some(3));
        assert_eq!(parse_threads("Name:\tx\n"), None);
    }

    #[test]
    fn the_mount_root_check_works_here() {
        // What it answers depends on where the tests run; the chroot
        // refusal itself is checked in the VM smoke suite.
        crate::probe::slash_is_mount_root().unwrap();
    }

    #[test]
    fn a_test_process_is_multi_threaded_and_refused() {
        // The test harness runs tests on worker threads: refused before any
        // system call that could change the process.
        // A thread that lives until the check is done, whatever the harness
        // does with threads.
        let (done, wait) = std::sync::mpsc::channel::<()>();
        let spare = std::thread::spawn(move || {
            let _ = wait.recv();
        });
        let r = enter_build_namespace();
        drop(done);
        spare.join().unwrap();
        assert!(
            matches!(r, Err(Error::NotSingleThreaded { threads }) if threads > 1),
            "{r:?}"
        );
    }

    /// Capabilities are per thread: a trim on one thread of several would
    /// leave the others their sets, so it is refused before any change,
    /// whoever runs the test.
    #[test]
    fn a_multi_threaded_process_is_refused_the_trim() {
        let before = rustix::thread::capabilities(None).unwrap();
        let (done, wait) = std::sync::mpsc::channel::<()>();
        let spare = std::thread::spawn(move || {
            let _ = wait.recv();
        });
        let r = trim_to_run_capabilities();
        drop(done);
        spare.join().unwrap();
        assert!(
            matches!(r, Err(Error::NotSingleThreaded { threads }) if threads > 1),
            "{r:?}"
        );
        let after = rustix::thread::capabilities(None).unwrap();
        assert_eq!(
            (before.effective, before.permitted, before.inheritable),
            (after.effective, after.permitted, after.inheritable)
        );
    }

    #[test]
    fn the_read_back_check_sees_anything_beyond_the_defaults() {
        let defaults = caps::docker_default_mask();
        assert!(!trimmed_too_little(defaults, 0, defaults));
        // Fewer than the defaults (a container that dropped some) is fine.
        assert!(!trimmed_too_little(
            defaults & !(1 << 13),
            0,
            defaults & !(1 << 13)
        ));
        // CAP_SYS_PTRACE (19) held, or left in the bounding set.
        assert!(trimmed_too_little(defaults | (1 << 19), 0, defaults));
        assert!(trimmed_too_little(defaults, 0, defaults | (1 << 19)));
        // Anything inheritable.
        assert!(trimmed_too_little(defaults, 1 << 0, defaults));
    }
}
