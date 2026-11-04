//! Process set-up, once per build, while the process is single-threaded.

use std::ffi::{CStr, CString, OsStr};
use std::io;
use std::os::fd::{AsFd, AsRawFd as _, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use rustix::fs::{AtFlags, FileType, Mode, OFlags, ResolveFlags, StatVfsMountFlags, StatxFlags};
use rustix::io::Errno;
use rustix::mount::{MountFlags, MountPropagationFlags, MoveMountFlags, OpenTreeFlags};
use rustix::thread::{CapabilitySet, CapabilitySets, UnshareFlags};

use crate::caps;
use crate::error::{Error, Result, sys_err};
use crate::plan;

/// Set once [`enter_build_namespace`] has moved the process into the
/// build's private mount namespace, the only one where
/// [`crate::Sandbox::start`] binds the binary over itself (ADR-0082): never
/// in a namespace the process shares with the host. It holds what
/// `readlink /proc/self/exe` gave before the `unshare`: once no process is
/// left in the first namespace, a binary on a file a container runtime
/// bound in may read as `/`. A static rather than a value passed down: it
/// records a fact of the process, which no caller of `Sandbox::start` can
/// get wrong.
static BUILD_NAMESPACE: OnceLock<rustix::io::Result<CString>> = OnceLock::new();

/// Once [`enter_build_namespace`] succeeded in this process, what
/// `readlink /proc/self/exe` gave before: the binary's path, or the errno.
pub(crate) fn build_namespace() -> Option<rustix::io::Result<&'static CStr>> {
    BUILD_NAMESPACE
        .get()
        .map(|link| link.as_deref().map_err(|e| *e))
}

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
    // Its failure refuses the full sandbox later: here it would read as
    // "root may not mount".
    let binary = rustix::fs::readlink(SELF_EXE, Vec::new());
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
    .map_err(|e| sys_err("make / a recursive slave mount", e))?;
    // Set once per process: a second successful call nests a namespace,
    // where the first path is still the binary's.
    let _ = BUILD_NAMESPACE.set(binary);
    Ok(())
}

/// The link the kernel keeps to the running binary.
const SELF_EXE: &CStr = c"/proc/self/exe";

/// Binds the running binary read-only over itself in the calling thread's
/// mount namespace, the build's (ADR-0082), so that its path there names a
/// read-only mount of its own (design/sandbox.md, process set-up). `link`
/// is what `readlink /proc/self/exe` gave before the process left its first
/// mount namespace; it must still name the running file (device and
/// inode). Where it names a read-only mount already (an earlier call, a
/// binary installed read-only) nothing is made. `mount_api` picks
/// `open_tree` over `mount(2)`. What `/proc/<pid>/exe` names does not
/// change: the file on the mount the process was executed from.
///
/// # Errors
///
/// [`Error::OwnBinary`] when `link` is an errno, the path does not name the
/// running file, or a step of the bind fails; [`Error::Sys`] when
/// `/proc/self/exe` cannot be opened or `statx` gives no mount id.
pub(crate) fn bind_own_binary(link: rustix::io::Result<&CStr>, mount_api: bool) -> Result<()> {
    let link = link.map_err(|e| step_err(Path::new("/proc/self/exe"), "readlink", e))?;
    let exe = rustix::fs::open(SELF_EXE, OFlags::PATH | OFlags::CLOEXEC, Mode::empty())
        .map_err(|e| sys_err("open /proc/self/exe", e))?;
    let running = statx(&exe).map_err(|e| sys_err("statx /proc/self/exe", e))?;
    let path = PathBuf::from(OsStr::from_bytes(link.to_bytes()));
    if !path.is_absolute() {
        return Err(not_bound(&path, "the kernel gave no path for it"));
    }
    let (file, st) = binary_at(link, &path, &running)?;
    let flags = rustix::fs::fstatvfs(&file)
        .map_err(|e| step_err(&path, "statvfs", e))?
        .f_flag;
    if flags.contains(StatVfsMountFlags::RDONLY) {
        tracing::debug!(binary = %path.display(), "the binary's mount is read-only already");
        return Ok(());
    }
    // The source mount's locked flags stay, or the remount fails in a user
    // namespace.
    let remount = plan::remount_flags(flags, MountFlags::RDONLY);
    if mount_api {
        let tree = rustix::mount::open_tree(
            &file,
            c"",
            OpenTreeFlags::OPEN_TREE_CLONE
                | OpenTreeFlags::OPEN_TREE_CLOEXEC
                | OpenTreeFlags::AT_EMPTY_PATH,
        )
        .map_err(|e| step_err(&path, "open_tree", e))?;
        rustix::mount::move_mount(
            &tree,
            c"",
            &file,
            c"",
            MoveMountFlags::MOVE_MOUNT_F_EMPTY_PATH | MoveMountFlags::MOVE_MOUNT_T_EMPTY_PATH,
        )
        .map_err(|e| step_err(&path, "move_mount", e))?;
        rustix::mount::mount_remount(fd_path(&tree).as_c_str(), remount, c"")
            .map_err(|e| step_err(&path, "remount read-only", e))?;
    } else {
        let at = fd_path(&file);
        rustix::mount::mount_bind(at.as_c_str(), at.as_c_str())
            .map_err(|e| step_err(&path, "bind", e))?;
        let (bind, again) = binary_at(link, &path, &running)?;
        if again.stx_mnt_id == st.stx_mnt_id {
            return Err(not_bound(&path, "the bind is not where its path leads"));
        }
        rustix::mount::mount_remount(fd_path(&bind).as_c_str(), remount, c"")
            .map_err(|e| step_err(&path, "remount read-only", e))?;
    }
    let (after, now) = binary_at(link, &path, &running)?;
    let read_only = rustix::fs::fstatvfs(&after)
        .map_err(|e| step_err(&path, "statvfs", e))?
        .f_flag
        .contains(StatVfsMountFlags::RDONLY);
    if now.stx_mnt_id == st.stx_mnt_id || !read_only {
        return Err(not_bound(&path, "its path does not name a read-only bind"));
    }
    tracing::debug!(binary = %path.display(), "bound the binary read-only over itself");
    Ok(())
}

/// The binary at `path` (`link`, as bytes), looked up without symlinks: it
/// must be the regular file `running` is.
fn binary_at(
    link: &CStr,
    path: &Path,
    running: &rustix::fs::Statx,
) -> Result<(OwnedFd, rustix::fs::Statx)> {
    let fd = rustix::fs::openat2(
        rustix::fs::CWD,
        link,
        OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS,
    )
    .map_err(|e| step_err(path, "open", e))?;
    let st = statx(&fd).map_err(|e| step_err(path, "statx", e))?;
    let same = (st.stx_dev_major, st.stx_dev_minor, st.stx_ino)
        == (
            running.stx_dev_major,
            running.stx_dev_minor,
            running.stx_ino,
        );
    if !same || FileType::from_raw_mode(st.stx_mode.into()) != FileType::RegularFile {
        return Err(not_bound(
            path,
            "not the file ranikust runs from: replaced or removed since it started, or out of this \
             mount namespace's reach",
        ));
    }
    Ok((fd, st))
}

/// `statx` of `fd` with its mount id, which must be there.
fn statx(fd: impl AsFd) -> rustix::io::Result<rustix::fs::Statx> {
    let st = rustix::fs::statx(
        fd,
        c"",
        AtFlags::EMPTY_PATH,
        StatxFlags::BASIC_STATS | StatxFlags::MNT_ID,
    )?;
    if StatxFlags::from_bits_retain(st.stx_mask).contains(StatxFlags::MNT_ID) {
        Ok(st)
    } else {
        Err(Errno::NOSYS)
    }
}

/// `/proc/self/fd/<n>` for `fd`.
fn fd_path(fd: impl AsFd) -> CString {
    let p = format!("/proc/self/fd/{}", fd.as_fd().as_raw_fd());
    // Digits and slashes hold no NUL.
    CString::new(p).unwrap_or_default()
}

fn not_bound(path: &Path, why: &str) -> Error {
    Error::OwnBinary {
        path: path.to_owned(),
        source: io::Error::other(why.to_owned()),
    }
}

/// A step of [`bind_own_binary`] on the binary at `path` failed with `e`.
fn step_err(path: &Path, step: &str, e: Errno) -> Error {
    let e = io::Error::from(e);
    Error::OwnBinary {
        path: path.to_owned(),
        source: io::Error::new(e.kind(), format!("{step}: {e}")),
    }
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
    let last = caps::last_cap();
    let defaults = caps::docker_default_mask();
    let sets =
        rustix::thread::capabilities(None).map_err(|e| sys_err("read the capability sets", e))?;
    let setpcap = sets.effective.contains(CapabilitySet::SETPCAP);
    match caps::direct_caps(caps::bounding_set(last), setpcap) {
        caps::DirectCaps::Keep => {}
        caps::DirectCaps::Drop => {
            let (drops, len) = caps::bounding_drops(last);
            for &cap in drops.iter().take(len) {
                let set = CapabilitySet::from_bits_retain(1u64.checked_shl(cap).unwrap_or(0));
                match rustix::thread::remove_capability_from_bounding_set(set) {
                    Ok(()) => {}
                    // Past the kernel's last capability.
                    Err(Errno::INVAL) => break,
                    Err(e) => return Err(sys_err("drop a capability from the bounding set", e)),
                }
            }
        }
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
        // So `Sandbox::start` binds nothing over the binary in a namespace
        // the process may share with the host (ADR-0082).
        assert!(build_namespace().is_none());
    }

    /// ADR-0082: a path that is not the running binary, or none, refuses
    /// the bind before any mount is tried, so this runs anywhere.
    #[test]
    fn the_bind_refuses_a_path_that_is_not_the_running_binary() {
        let cases = [
            // The errno of a failed `readlink` is kept.
            (Err(Errno::NAMETOOLONG), "/proc/self/exe", "readlink: "),
            (Ok(c"x"), "x", "the kernel gave no path"),
            // Another file: a binary replaced, or out of the namespace's reach.
            (Ok(c"/"), "/", "not the file ranikust runs from"),
            // Looked up without symlinks.
            (Ok(c"/proc/self/exe"), "/proc/self/exe", "open: "),
        ];
        for (link, at, why) in cases {
            match bind_own_binary(link, false) {
                Err(Error::OwnBinary { path, source }) => {
                    assert_eq!(path, Path::new(at), "{link:?}");
                    assert!(source.to_string().starts_with(why), "{link:?}: {source}");
                }
                r => panic!("{link:?}: {r:?}"),
            }
        }
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
